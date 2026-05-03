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
use interfaces::{create_api_routes, create_public_api_routes, web::create_web_routes};

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

        // CalDAV/CardDAV/WebDAV with auth middleware (merged, not nested)
        let caldav_protected = caldav_router.layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            auth_middleware,
        ));
        let carddav_protected = carddav_router.layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            auth_middleware,
        ));
        let webdav_protected = webdav_router.layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            auth_middleware,
        ));

        app = Router::new()
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
            .merge(web_routes)
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(ClientIpMakeSpan)
                    .on_response(LogBadRequest),
            )
            .layer(PropagateRequestIdLayer::x_request_id())
            .layer(SetRequestIdLayer::x_request_id(UuidRequestId));

        // Mount Nextcloud routes (uses its own Basic Auth middleware)
        if let Some(nc_router) = nextcloud_router {
            app = app.merge(nc_router.with_state(app_state.clone()));
        }

        // Mount WOPI routes (protocol routes use own token auth, API routes behind auth middleware)
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
    } else {
        // Auth disabled — no middleware applied
        tracing::warn!("Authentication is DISABLED — all API routes are publicly accessible");
        app = Router::new()
            .nest("/api", public_api_routes)
            .nest("/api", api_routes)
            // RFC 6764 well-known discovery (just redirects)
            .merge(well_known_router)
            // CalDAV/CardDAV/WebDAV protocols merged at top-level
            .merge(caldav_router)
            .merge(carddav_router)
            .merge(webdav_router)
            .merge(web_routes)
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(ClientIpMakeSpan)
                    .on_response(LogBadRequest),
            )
            .layer(PropagateRequestIdLayer::x_request_id())
            .layer(SetRequestIdLayer::x_request_id(UuidRequestId));

        // Mount Nextcloud routes
        if let Some(nc_router) = nextcloud_router {
            app = app.merge(nc_router.with_state(app_state.clone()));
        }

        // Mount WOPI routes (no auth middleware when auth is disabled)
        if let Some((wopi_protocol, wopi_api)) = wopi_routes {
            app = app.nest("/wopi", wopi_protocol).nest("/api/wopi", wopi_api);
        }
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
                 style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data: blob:; \
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

    // Start server — tuned socket for low-latency responses
    let addr = SocketAddr::from(([0, 0, 0, 0], config.server_port));
    tracing::info!("Starting OxiCloud server on http://{}", addr);

    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    // Allow multiple workers on the same port (future-ready)
    #[cfg(not(windows))]
    socket.set_reuse_port(true)?;
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
    socket.bind(&addr.into())?;
    // High backlog for connection bursts (WebDAV clients open many parallel connections)
    socket.listen(2048)?;

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
