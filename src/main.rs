#![allow(async_fn_in_trait)]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};

use axum::Router;
use axum::extract::DefaultBodyLimit;
use oxicloud::interfaces::middleware::trace_span::{
    ClientIpMakeSpan, LogBadRequest, UuidRequestId,
};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// OxiCloud - Cloud Storage Platform
///
/// OxiCloud is a NextCloud-like file storage system built in Rust with a focus on
/// performance, security, and clean architecture. The system provides:
///
/// - File and folder management with rich metadata
/// - User authentication and authorization
/// - File trash system with automatic cleanup
/// - Efficient handling of large files through parallel processing
/// - Compression capabilities for bandwidth optimization
/// - RESTful API and web interface
///
/// The architecture follows the Clean/Hexagonal Architecture pattern with:
///
/// - Domain Layer: Core business entities and repository interfaces (domain/*)
/// - Application Layer: Use cases and service orchestration (application/*)
/// - Infrastructure Layer: Technical implementations of repositories (infrastructure/*)
/// - Interface Layer: API endpoints and web controllers (interfaces/*)
///
/// Dependencies are managed through dependency inversion, with high-level modules
/// defining interfaces (ports) that low-level modules implement (adapters).
///
/// @author OxiCloud Development Team
use oxicloud::common;
use oxicloud::infrastructure;
use oxicloud::interfaces;

use common::di::AppServiceFactory;
use infrastructure::db::create_database_pools;
use interfaces::{
    create_api_routes, create_health_routes, create_public_api_routes, web::create_web_routes,
};

fn parse_addr(host: &str, port: u16) -> Result<SocketAddr, String> {
    // Strip surrounding brackets from IPv6: [::1] -> ::1
    let host = host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    // Try parsing as IPv6 first, then IPv4
    // and format the address string accordingly
    //   - IPv6: "[::1]:8080"
    //   - IPv4: "127.0.0.1:8080"
    let addr_str = if host.contains(':') {
        format!("[{host}]:{port}") // IPv6
    } else {
        format!("{host}:{port}") // IPv4
    };

    addr_str
        .parse::<SocketAddr>()
        .map_err(|e| format!("Invalid address '{}': {}", addr_str, e))
}

fn make_socket(addr: &SocketAddr, reuse_port: bool) -> std::io::Result<Socket> {
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    // SO_REUSEPORT: opt-in only — must be explicitly enabled via
    // OXICLOUD_REUSE_PORT=true.  Disabled by default so that accidentally
    // starting a second instance fails fast with "address already in use"
    // rather than silently sharing the port.
    #[cfg(not(windows))]
    if reuse_port {
        socket.set_reuse_port(true)?;
    }
    // Disable Nagle's algorithm — send small responses (JSON, PROPFIND)
    // immediately instead of waiting up to 40ms for coalescing.
    socket.set_tcp_nodelay(true)?;
    // Detect dead connections within 60s instead of hours
    socket.set_keepalive(true)?;
    socket.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(Duration::from_secs(60))
            .with_interval(Duration::from_secs(10)),
    )?;
    socket.set_nonblocking(true)?;

    // For IPv6: disable dual-stack to be explicit about what you're binding
    // (set true to restrict to IPv6-only, false to also accept IPv4-mapped)
    if addr.is_ipv6() {
        socket.set_only_v6(true)?; // explicit: one socket = one protocol
    }

    socket.bind(&(*addr).into())?;
    // High backlog for connection bursts (WebDAV clients open many parallel connections)
    socket.listen(2048)?;

    Ok(socket)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env file if present (for local development)
    dotenvy::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    oxicloud::interfaces::middleware::trusted_proxy::log_config();

    tracing::info!(
        "OxiCloud v{} | branch={} commit={}",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_BRANCH"),
        env!("GIT_HASH")
    );

    // Load configuration from environment variables
    let config = common::config::AppConfig::from_env();

    // Surface the upload-size limits at startup. Operators (and the
    // CI runner) need to see what's actually in effect — a silent
    // fallback to the 100 MB default when `OXICLOUD_CHUNK_MAX_BYTES`
    // is mistyped or missing is the exact failure mode that's
    // hardest to spot from chunked-upload tests.
    tracing::info!(
        max_upload_size_mb = config.storage.max_upload_size / (1024 * 1024),
        direct_put_max_bytes_mb = config.storage.direct_put_max_bytes / (1024 * 1024),
        chunk_max_bytes_mb = config.storage.chunk_max_bytes / (1024 * 1024),
        "Upload limits loaded from config"
    );

    // Ensure storage and locales directories exist
    let storage_path = config.storage_path.clone();
    if !storage_path.exists() {
        std::fs::create_dir_all(&storage_path).expect("Failed to create storage directory");
    }
    // Initialize database pools if auth is enabled
    let db_pools = if config.features.enable_auth {
        match create_database_pools(&config).await {
            Ok(pools) => {
                tracing::info!("PostgreSQL database pools initialized successfully");
                Some(pools)
            }
            Err(e) => {
                // SECURITY: fail-closed. If auth is required but the database
                // is unreachable, the server MUST NOT start in public mode.
                panic!(
                    "FATAL: enable_auth=true but database connection failed: {}. \
                     Refusing to start without authentication.",
                    e
                );
            }
        }
    } else {
        None
    };

    // Ensure locales directory exists for i18n
    let locales_path = PathBuf::from("./static/locales");
    if !locales_path.exists() {
        std::fs::create_dir_all(&locales_path).expect("Failed to create locales directory");
    }

    // Build all services via the factory
    let factory = AppServiceFactory::with_config(storage_path, locales_path, config.clone());

    let app_state = factory.build_app_state(db_pools).await
        .expect("Failed to build application state. If running in Docker, ensure the storage volume is writable by the oxicloud user (UID 1001)");

    // Wrap in Arc so that Axum clones a single refcount per request
    // instead of deep-copying ~42 Arc fields + 16 String/PathBuf allocations.
    let app_state = Arc::new(app_state);

    // Build application router
    let api_routes = create_api_routes(&app_state);
    let public_api_routes = create_public_api_routes(&app_state);
    let health_routes = create_health_routes(&app_state);
    let web_routes = create_web_routes();

    let mut app;

    // Build CalDAV / CardDAV / WebDAV protocol routers (merged at top-level, not under /api)
    use oxicloud::interfaces::api::handlers::caldav_handler;
    use oxicloud::interfaces::api::handlers::carddav_handler;
    use oxicloud::interfaces::api::handlers::webdav_handler;
    let caldav_router = caldav_handler::caldav_routes();
    let well_known_router = caldav_handler::well_known_routes();
    let carddav_router = carddav_handler::carddav_routes();
    let webdav_router = webdav_handler::webdav_routes();

    // CalDAV/CardDAV only carry XML payloads — cap at 1 MB at the transport
    // level so `body::to_bytes()` cannot be abused to OOM the server.
    // WebDAV is excluded: its streaming PUT handler enforces its own per-upload
    // limit from StorageConfig::max_upload_size.
    let caldav_router = caldav_router.layer(RequestBodyLimitLayer::new(1_048_576));
    let carddav_router = carddav_router.layer(RequestBodyLimitLayer::new(1_048_576));

    // Build WOPI routes if enabled
    use oxicloud::interfaces::api::handlers::wopi_handler;
    let wopi_routes = if config.wopi.enabled {
        if let (Some(token_svc), Some(lock_svc), Some(discovery_svc)) = (
            &app_state.wopi_token_service,
            &app_state.wopi_lock_service,
            &app_state.wopi_discovery_service,
        ) {
            // WOPI_BASE_URL: the URL OnlyOffice/Collabora uses to call back into OxiCloud
            // WOPI_PUBLIC_BASE_URL: the URL the browser uses to reach OxiCloud
            // Both must be set for Docker/multi-host deployments. WOPI_BASE_URL takes
            // precedence if both are set (supports the legacy single-URL pattern).
            let wopi_base_url = std::env::var("OXICLOUD_WOPI_BASE_URL")
                .or_else(|_| std::env::var("OXICLOUD_WOPI_PUBLIC_BASE_URL"))
                .map(|v| v.trim_end_matches('/').to_string())
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| config.base_url());

            let public_base_url = std::env::var("OXICLOUD_WOPI_PUBLIC_BASE_URL")
                .or_else(|_| std::env::var("OXICLOUD_WOPI_BASE_URL"))
                .map(|v| v.trim_end_matches('/').to_string())
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| config.base_url());

            let wopi_state = wopi_handler::WopiState {
                token_service: token_svc.clone(),
                lock_service: lock_svc.clone(),
                discovery_service: discovery_svc.clone(),
                app_state: app_state.clone(),
                public_base_url,
                wopi_base_url,
            };

            let (protocol, api) = wopi_handler::wopi_routes(wopi_state);
            Some((protocol, api))
        } else {
            None
        }
    } else {
        None
    };

    // Build Nextcloud routes if enabled
    let nextcloud_router = if config.nextcloud.enabled {
        use oxicloud::interfaces::nextcloud::routes::nextcloud_routes_with_state;
        Some(nextcloud_routes_with_state(app_state.clone()))
    } else {
        None
    };

    // Apply auth middleware to protected API routes when auth is enabled
    if config.features.enable_auth {
        // SECURITY: if auth is required, auth_service MUST be present at this
        // point.  The earlier guards in di.rs and main.rs guarantee this, but
        // add a defensive check so a future refactor cannot silently degrade.
        assert!(
            app_state.auth_service.is_some(),
            "FATAL: enable_auth=true but auth_service is None. \
             This should have been caught during initialization."
        );
    }
    if config.features.enable_auth {
        use interfaces::api::handlers::auth_handler::{
            auth_protected_routes, auth_public_routes, login_route, refresh_route, register_route,
            setup_route,
        };
        use oxicloud::interfaces::api::handlers::app_password_handler;
        use oxicloud::interfaces::api::handlers::device_auth_handler;
        use oxicloud::interfaces::middleware::auth::auth_middleware;
        use oxicloud::interfaces::middleware::csrf::csrf_middleware;
        use oxicloud::interfaces::middleware::rate_limit::{
            RateLimiter, rate_limit_login, rate_limit_refresh, rate_limit_register,
        };

        // ── Rate limiters (IP-based, in-memory via moka) ────────────────
        let rl = &config.auth.rate_limit;
        let login_limiter = Arc::new(RateLimiter::new(
            rl.login_max_requests,
            rl.login_window_secs,
            100_000,
        ));
        let register_limiter = Arc::new(RateLimiter::new(
            rl.register_max_requests,
            rl.register_window_secs,
            100_000,
        ));
        let refresh_limiter = Arc::new(RateLimiter::new(
            rl.refresh_max_requests,
            rl.refresh_window_secs,
            100_000,
        ));
        tracing::info!(
            "Rate limiting enabled — login: {}/{} s, register: {}/{} s, refresh: {}/{} s",
            rl.login_max_requests,
            rl.login_window_secs,
            rl.register_max_requests,
            rl.register_window_secs,
            rl.refresh_max_requests,
            rl.refresh_window_secs,
        );

        // Auth routes split by rate-limit policy
        let auth_login = login_route()
            .layer(axum::middleware::from_fn_with_state(
                login_limiter.clone(),
                rate_limit_login,
            ))
            .with_state(app_state.clone());
        let auth_register = register_route()
            .layer(axum::middleware::from_fn_with_state(
                register_limiter.clone(),
                rate_limit_register,
            ))
            .with_state(app_state.clone());
        let auth_refresh = refresh_route()
            .layer(axum::middleware::from_fn_with_state(
                refresh_limiter.clone(),
                rate_limit_refresh,
            ))
            .with_state(app_state.clone());
        // Public auth routes (status, OIDC)
        let auth_public = auth_public_routes().with_state(app_state.clone());
        // Protected auth routes (/me, /change-password, /logout) — require auth + CSRF
        let auth_protected = auth_protected_routes()
            .layer(axum::middleware::from_fn(csrf_middleware))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                auth_middleware,
            ))
            .with_state(app_state.clone());
        // App password management routes — require auth + CSRF
        let app_pw_protected = app_password_handler::app_password_routes()
            .layer(axum::middleware::from_fn(csrf_middleware))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                auth_middleware,
            ))
            .with_state(app_state.clone());
        // One-time setup route — public, rate-limited like register
        let setup_router = setup_route()
            .layer(axum::middleware::from_fn_with_state(
                register_limiter.clone(),
                rate_limit_register,
            ))
            .with_state(app_state.clone());

        // Device Authorization Grant (RFC 8628)
        // Public endpoints: /api/auth/device/authorize + /api/auth/device/token
        let device_public =
            device_auth_handler::device_auth_public_routes().with_state(app_state.clone());
        // Protected endpoints: /api/auth/device/verify, /api/auth/device/devices
        let device_protected = device_auth_handler::device_auth_protected_routes()
            .layer(axum::middleware::from_fn(csrf_middleware))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                auth_middleware,
            ))
            .with_state(app_state.clone());

        // Protected API routes — require valid JWT token
        let protected_api = api_routes
            .layer(axum::middleware::from_fn(csrf_middleware))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                auth_middleware,
            ));

        // CalDAV/CardDAV/WebDAV with auth + internal-only middleware
        // (merged, not nested). External users have no calendar, no
        // address book, and no home folder — locking them out of these
        // protocol subtrees in one place avoids leaking the protocol
        // surface to a principal kind that can do nothing with it. The
        // `require_internal_user_layer` runs AFTER auth (tower order:
        // later .layer() = outermost = runs first).
        use oxicloud::interfaces::middleware::user::require_internal_user_layer;
        let caldav_protected = caldav_router
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                require_internal_user_layer,
            ))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                auth_middleware,
            ));
        let carddav_protected = carddav_router
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                require_internal_user_layer,
            ))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                auth_middleware,
            ));
        let webdav_protected = webdav_router
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                require_internal_user_layer,
            ))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                auth_middleware,
            ));

        // Magic-link redemption — public, no CSRF, no rate limit (the token IS
        // the credential and `mark_used` is single-use). PR 12 will add a
        // per-IP limiter on top.
        let magic_link_router = interfaces::api::handlers::magic_link_handler::magic_link_routes()
            .with_state(app_state.clone());

        app = Router::new()
            // Health / readiness probes — no auth, mounted at root
            .merge(health_routes)
            // Magic-link redemption — top-level, no `/api/` prefix
            .merge(magic_link_router)
            // Rate-limited auth endpoints (login, register, refresh)
            .nest("/api/auth", auth_login)
            .nest("/api/auth", auth_register)
            .nest("/api/auth", auth_refresh)
            // Public auth endpoints (status, OIDC)
            .nest("/api/auth", auth_public)
            // Protected auth endpoints (/me, /change-password, /logout)
            .nest("/api/auth", auth_protected)
            // App password management (create, list, revoke)
            .nest("/api/auth", app_pw_protected)
            // One-time setup endpoint — public, rate-limited
            .nest("/api", setup_router)
            // Device Auth Grant public endpoints (authorize + token polling)
            .nest("/api/auth/device", device_public)
            // Device Auth Grant protected endpoints (verify + device management)
            .nest("/api/auth/device", device_protected)
            // Public API routes (share access, i18n) — no auth required
            .nest("/api", public_api_routes)
            // All other API routes are protected by auth middleware
            .nest("/api", protected_api)
            // RFC 6764 well-known discovery (public, no auth — just redirects)
            .merge(well_known_router.clone())
            // CalDAV/CardDAV/WebDAV protocols merged at top-level for client compatibility
            .merge(caldav_protected)
            .merge(carddav_protected)
            .merge(webdav_protected)
            .merge(web_routes);

        // Mount Nextcloud routes (uses its own Basic Auth middleware).
        // **Merged BEFORE the trace + request-id layers** so NC requests
        // get the same `request_id` / `user_id` / `client_ip` span
        // fields as every other surface — see
        // `interfaces/middleware/trace_span.rs::ClientIpMakeSpan`.
        if let Some(nc_router) = nextcloud_router {
            app = app.merge(nc_router.with_state(app_state.clone()));
        }

        // Mount WOPI routes (protocol routes use own token auth, API routes behind auth middleware).
        // Same reasoning as NC above: merge before the trace layer so
        // WOPI requests appear in the structured log channel.
        if let Some((wopi_protocol, wopi_api)) = wopi_routes {
            let wopi_api_protected = wopi_api
                .layer(axum::middleware::from_fn(csrf_middleware))
                .layer(axum::middleware::from_fn_with_state(
                    app_state.clone(),
                    auth_middleware,
                ));
            app = app
                .nest("/wopi", wopi_protocol)
                .nest("/api/wopi", wopi_api_protected);
        }

        // ── Trace + request-id layers applied LAST so every route
        //    merged above (including the conditional NC and WOPI
        //    surfaces) is wrapped. New protocol routers added later
        //    only have to be merged before this point to get tracing
        //    for free — no second site to remember to update.
        app = app
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(ClientIpMakeSpan)
                    .on_response(LogBadRequest),
            )
            .layer(PropagateRequestIdLayer::x_request_id())
            .layer(SetRequestIdLayer::x_request_id(UuidRequestId));
    } else {
        // Auth disabled — no middleware applied
        tracing::warn!("Authentication is DISABLED — all API routes are publicly accessible");
        app = Router::new()
            // Health / readiness probes — no auth, mounted at root
            .merge(health_routes)
            .nest("/api", public_api_routes)
            .nest("/api", api_routes)
            // RFC 6764 well-known discovery (just redirects)
            .merge(well_known_router)
            // CalDAV/CardDAV/WebDAV protocols merged at top-level
            .merge(caldav_router)
            .merge(carddav_router)
            .merge(webdav_router)
            .merge(web_routes);

        // Mount Nextcloud routes — merged BEFORE the trace + request-id
        // layers so NC requests get the same span fields as every
        // other surface (matches the auth-enabled branch above).
        if let Some(nc_router) = nextcloud_router {
            app = app.merge(nc_router.with_state(app_state.clone()));
        }

        // Mount WOPI routes (no auth middleware when auth is disabled).
        // Same reasoning: merge before the trace layer.
        if let Some((wopi_protocol, wopi_api)) = wopi_routes {
            app = app.nest("/wopi", wopi_protocol).nest("/api/wopi", wopi_api);
        }

        // ── Trace + request-id layers applied LAST. See the
        //    auth-enabled branch above for the rationale.
        app = app
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(ClientIpMakeSpan)
                    .on_response(LogBadRequest),
            )
            .layer(PropagateRequestIdLayer::x_request_id())
            .layer(SetRequestIdLayer::x_request_id(UuidRequestId));
    }

    // Increase the default body limit to allow large file uploads.
    // Uses architecture-appropriate limit: 10 GB on 64-bit, 1 GB on 32-bit.
    // Without this Axum caps Multipart bodies at 2 MB.
    #[cfg(target_pointer_width = "64")]
    const BODY_LIMIT: usize = 10 * 1024 * 1024 * 1024; // 10 GB
    #[cfg(target_pointer_width = "32")]
    const BODY_LIMIT: usize = 1024 * 1024 * 1024; // 1 GB
    app = app.layer(DefaultBodyLimit::max(BODY_LIMIT));

    // ── HTTP compression (gzip + Brotli) ─────────────────────────────────
    // Negotiates the best encoding via Accept-Encoding. Policy: compress
    // everything by default so no shrinkable response is ever missed (text,
    // JSON, JS/CSS, XML, SVG, fonts ttf/otf, WASM…), and skip ONLY content
    // that is already compressed — where a second pass burns CPU and adds
    // latency for ~0 bytes saved.
    //
    // We deliberately do NOT blanket-exclude `image/*`: `image/svg+xml` is
    // plain text and compresses ~70%, so the genuinely-compressed raster
    // formats are listed individually instead, leaving SVG compressible.
    //
    // This is the single, global compression layer (the `/api` router used to
    // add its own predicate-less one, which silently compressed media). It is
    // reverse-proxy friendly: a proxy that sees `Content-Encoding` passes
    // the response through untouched.
    {
        use tower_http::compression::CompressionLayer;
        use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};

        // Never compress file-body responses (downloads, inline previews, ZIP
        // exports). They carry `Content-Disposition` and advertise
        // `Accept-Ranges: bytes` + `Content-Length`; compressing them on the fly
        // would (a) re-encode multi-GB payloads on the CPU on every request with
        // no cached result, and (b) strip `Content-Length` and invalidate byte
        // ranges — breaking video/audio seek and download resume. API JSON and
        // static assets never set `Content-Disposition`, so they stay compressed.
        #[derive(Clone, Copy)]
        struct NotForDownloads;
        impl Predicate for NotForDownloads {
            fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
            where
                B: http_body::Body,
            {
                !response
                    .headers()
                    .contains_key(axum::http::header::CONTENT_DISPOSITION)
            }
        }

        let predicate = SizeAbove::new(256)
            .and(NotForContentType::GRPC)
            .and(NotForContentType::SSE)
            // ── already-compressed raster images (SVG intentionally absent) ──
            .and(NotForContentType::const_new("image/jpeg"))
            .and(NotForContentType::const_new("image/png"))
            .and(NotForContentType::const_new("image/gif"))
            .and(NotForContentType::const_new("image/webp"))
            .and(NotForContentType::const_new("image/avif"))
            .and(NotForContentType::const_new("image/heic"))
            .and(NotForContentType::const_new("image/heif"))
            .and(NotForContentType::const_new("image/jp2"))
            .and(NotForContentType::const_new("image/x-icon"))
            .and(NotForContentType::const_new("image/vnd.microsoft.icon"))
            // ── audio / video families (already compressed) ──
            .and(NotForContentType::const_new("video/"))
            .and(NotForContentType::const_new("audio/"))
            // ── already-compressed web fonts; ttf/otf left compressible ──
            .and(NotForContentType::const_new("font/woff"))
            .and(NotForContentType::const_new("application/font-woff"))
            // ── archives & compressed containers ──
            .and(NotForContentType::const_new("application/zip"))
            .and(NotForContentType::const_new("application/gzip"))
            .and(NotForContentType::const_new("application/x-gzip"))
            .and(NotForContentType::const_new("application/x-tar"))
            .and(NotForContentType::const_new("application/x-7z-compressed"))
            .and(NotForContentType::const_new("application/x-rar-compressed"))
            .and(NotForContentType::const_new("application/x-bzip2"))
            .and(NotForContentType::const_new("application/zstd"))
            .and(NotForContentType::const_new("application/x-xz"))
            // ── zip-based document / app bundles (docx/xlsx/pptx, odf, epub…) ──
            .and(NotForContentType::const_new(
                "application/vnd.openxmlformats-officedocument",
            ))
            .and(NotForContentType::const_new(
                "application/vnd.oasis.opendocument",
            ))
            .and(NotForContentType::const_new("application/epub+zip"))
            .and(NotForContentType::const_new("application/java-archive"))
            .and(NotForContentType::const_new(
                "application/vnd.android.package-archive",
            ))
            // ── PDF: streams are usually already deflated; often large ──
            .and(NotForContentType::const_new("application/pdf"))
            // ── opaque binary we couldn't identify ──
            .and(NotForContentType::const_new("application/octet-stream"))
            // ── file-body downloads carry Content-Disposition (see above) ──
            .and(NotForDownloads);

        app = app.layer(CompressionLayer::new().compress_when(predicate));
    }

    // ── Security headers ─────────────────────────────────────────────────
    // Applied globally so every response (API, static, DAV) carries them.
    use axum::http::HeaderValue;
    use axum::http::header::HeaderName;

    app = app
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("content-security-policy"),
            // Note: 'unsafe-inline' is required for style-src because the
            // frontend JavaScript dynamically sets inline styles (e.g.,
            // element.style.display = 'none'). This is a common pattern
            // for UI state management and cannot be easily migrated to
            // external CSS classes without significant refactoring.
            // frame-src: '*' only matches network schemes, so 'blob:' must be
            // listed explicitly for inline PDF/document viewers.
            // media-src: needed for blob: video/audio playback.
            HeaderValue::from_static(
                "default-src 'self'; \
                 script-src 'self'; \
                 worker-src 'self'; \
                 style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data: blob: https:; \
                 media-src 'self' blob:; \
                 connect-src 'self'; \
                 font-src 'self' data:; \
                 frame-src * blob:; \
                 frame-ancestors 'none'; \
                 base-uri 'self'; \
                 form-action 'self'",
            ),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
        ));

    // Warn once at startup if auth cookies are not Secure.
    // HttpOnly + SameSite protection is nullified over plain HTTP because tokens
    // travel in cleartext and can be intercepted by a network observer.
    if !crate::interfaces::api::cookie_auth::is_cookie_secure() {
        tracing::warn!(
            "⚠️  SECURITY: auth cookies are NOT marked Secure. \
             Tokens will be transmitted in plaintext over HTTP. \
             Set OXICLOUD_COOKIE_SECURE=true for any HTTPS deployment."
        );
    }

    // Start server — tuned socket for low-latency responses
    // TODO: suport multiple addresses ?
    let addr = parse_addr(&config.server_host, config.server_port)?;

    // SO_REUSEPORT: disabled by default — a second instance on the same port
    // fails loudly instead of silently sharing the socket.  Set
    // OXICLOUD_REUSE_PORT=true only when you deliberately run multiple
    // workers (e.g. behind a process supervisor or during a rolling restart).
    let reuse_port = std::env::var("OXICLOUD_REUSE_PORT")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);

    if reuse_port {
        tracing::warn!(
            "OXICLOUD_REUSE_PORT is enabled — multiple processes may bind to port {}",
            config.server_port
        );
    }

    tracing::info!("Starting OxiCloud server on http://{}", addr);

    let socket = make_socket(&addr, reuse_port)?;

    let listener = tokio::net::TcpListener::from_std(socket.into())?;

    // Provide the fully-built state to the router
    let app = app.with_state(app_state);

    // TCP_NODELAY is inherited from the listening socket on Linux,
    // so every accepted connection already has Nagle disabled.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    tracing::info!("Server shutdown completed");

    Ok(())
}
