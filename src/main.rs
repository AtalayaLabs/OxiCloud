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

/// Parse a WOPI discovery URL string into an origin (scheme + host + optional port).
///
/// Returns `None` — after logging why — when the ariable is unset, empty, 
/// unparseable, uses a non-HTTP(S) scheme, or has no host. Callers then fall 
/// back to a self-only policy instead of panicking on operator misconfiguration.
fn wopi_origin_from_str(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let url = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(value = raw, error = %e,
                "ignoring OXICLOUD_WOPI_DISCOVERY_URL: not a valid URL");
            return None;
        }
    };

    if !matches!(url.scheme(), "http" | "https") {
        tracing::warn!(scheme = url.scheme(),
            "ignoring OXICLOUD_WOPI_DISCOVERY_URL: scheme must be http or https");
        return None;
    }

    let Some(host) = url.host_str() else {
        tracing::warn!(value = raw,
            "ignoring OXICLOUD_WOPI_DISCOVERY_URL: URL has no host");
        return None;
    };

    // The url crate normalizes default ports away at parse time, so an explicit
    // :80 / :443 already yields port() == None here — no special-casing needed.
    // host_str() returns the bracketed form for IPv6 (e.g. "[::1]"), which is
    // exactly what a CSP host-source expects.
    let port = url.port().map_or(String::new(), |p| format!(":{p}"));
    Some(format!("{}://{host}{port}", url.scheme()))
}

/// Read `OXICLOUD_WOPI_DISCOVERY_URL` and extract the origin.
///
/// Returns `None` when the env var is unset, empty, or malformed.
fn wopi_origin() -> Option<String> {
    let raw = std::env::var("OXICLOUD_WOPI_DISCOVERY_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    wopi_origin_from_str(&raw)
}

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
    // Negotiates the best encoding via Accept-Encoding.  Skips responses
    // that are already compressed or wouldn't benefit (images, video, etc.).
    // Compatible with a future reverse proxy — if the proxy sees
    // `Content-Encoding` it will pass the response through untouched.
    {
        use tower_http::compression::CompressionLayer;
        use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};

        let predicate = SizeAbove::new(256)
            .and(NotForContentType::GRPC)
            .and(NotForContentType::IMAGES)
            .and(NotForContentType::SSE)
            .and(NotForContentType::const_new("application/octet-stream"))
            .and(NotForContentType::const_new("application/zip"))
            .and(NotForContentType::const_new("application/gzip"))
            .and(NotForContentType::const_new("application/x-tar"))
            .and(NotForContentType::const_new("application/pdf"))
            .and(NotForContentType::const_new("video/"))
            .and(NotForContentType::const_new("audio/"));

        app = app.layer(CompressionLayer::new().compress_when(predicate));
    }

    // ── Security headers ─────────────────────────────────────────────────
    // Applied globally so every response (API, static, DAV) carries them.
    use axum::http::HeaderValue;
    use axum::http::header::HeaderName;

    // Build CSP dynamically — WOPI editor origins need form-action + frame-src.
    let wopi = wopi_origin();
    let form_action = match &wopi {
        Some(origin) => format!("form-action 'self' {origin}"),
        None => "form-action 'self'".to_string(),
    };

    let frame_src = match &wopi {
        Some(origin) => format!("frame-src 'self' {origin} blob:"),
        None => "frame-src 'self' blob:".to_string(),
    };

    let csp_value = format!(
        "default-src 'self'; \
         script-src 'self'; \
         worker-src 'self'; \
         style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob: https:; \
         media-src 'self' blob:; \
         connect-src 'self'; \
         font-src 'self' data:; \
         object-src 'none'; \
         {frame_src}; \
         frame-ancestors 'none'; \
         base-uri 'self'; \
         {form_action}",
    );

    app = app
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("content-security-policy"),
            // style-src 'unsafe-inline': covers inline `style="..."` attributes
            // and injected <style> blocks in the served markup.
            // media-src: needed for blob: video/audio playback.
            HeaderValue::from_str(&csp_value).expect("CSP must be valid"),
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

#[cfg(test)]
mod wopi_origin_tests {
    use super::*;

    #[test]
    fn empty_string_returns_none() {
        assert!(wopi_origin_from_str("").is_none());
    }

    #[test]
    fn whitespace_only_returns_none() {
        assert!(wopi_origin_from_str("   ").is_none());
    }

    #[test]
    fn invalid_url_returns_none() {
        assert!(wopi_origin_from_str("not a url").is_none());
    }

    #[test]
    fn ftp_scheme_returns_none() {
        assert!(wopi_origin_from_str("ftp://example.com").is_none());
    }

    #[test]
    fn https_with_path_strips_path() {
        let result = wopi_origin_from_str("https://collabora.example.com/hosting/discovery");
        assert_eq!(result, Some("https://collabora.example.com".to_string()));
    }

    #[test]
    fn deeply_nested_path_strips_all() {
        let result = wopi_origin_from_str(
            "https://onlyoffice.example.com:9980/hosting/discovery?param=value",
        );
        assert_eq!(
            result,
            Some("https://onlyoffice.example.com:9980".to_string())
        );
    }

    #[test]
    fn https_with_custom_port() {
        let result = wopi_origin_from_str("https://example.com:8443/path");
        assert_eq!(result, Some("https://example.com:8443".to_string()));
    }

    #[test]
    fn http_url_preserves_scheme() {
        let result = wopi_origin_from_str("http://localhost:9980/hosting/discovery");
        assert_eq!(result, Some("http://localhost:9980".to_string()));
    }

    #[test]
    fn default_https_port_omitted() {
        let result = wopi_origin_from_str("https://example.com:443/path");
        assert_eq!(result, Some("https://example.com".to_string()));
    }

    #[test]
    fn default_http_port_omitted() {
        let result = wopi_origin_from_str("http://example.com:80/path");
        assert_eq!(result, Some("http://example.com".to_string()));
    }

    #[test]
    fn trailing_slash_strips_slash() {
        let result = wopi_origin_from_str("https://example.com/");
        assert_eq!(result, Some("https://example.com".to_string()));
    }

    #[test]
    fn no_trailing_slash_no_path() {
        let result = wopi_origin_from_str("https://example.com");
        assert_eq!(result, Some("https://example.com".to_string()));
    }

    #[test]
    fn url_with_query_params_strips_query() {
        let result = wopi_origin_from_str("https://example.com/path?foo=bar&baz=qux");
        assert_eq!(result, Some("https://example.com".to_string()));
    }

    #[test]
    fn url_with_fragment_strips_fragment() {
        let result = wopi_origin_from_str("https://example.com/path#section");
        assert_eq!(result, Some("https://example.com".to_string()));
    }

    #[test]
    fn url_with_userinfo_strips_userinfo() {
        let result = wopi_origin_from_str("https://user:pass@example.com/path");
        assert_eq!(result, Some("https://example.com".to_string()));
    }

    #[test]
    fn ipv6_without_port() {
        let result = wopi_origin_from_str("http://[::1]:9980/path");
        assert_eq!(result, Some("http://[::1]:9980".to_string()));
    }

    #[test]
    fn ipv6_with_port() {
        let result = wopi_origin_from_str("https://[2001:db8::1]/hosting");
        assert_eq!(result, Some("https://[2001:db8::1]".to_string()));
    }
}
