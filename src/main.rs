#![allow(async_fn_in_trait)]

// mimalloc returns freed pages to the OS only when `MIMALLOC_PURGE_DELAY` is
// low/zero. With the default it retains them, so process RSS clamps at the peak
// even after the in-memory caches (file content, thumbnails, transcode) expire
// by TTL. The Dockerfile and docker-compose set `MIMALLOC_PURGE_DELAY=0` so RSS
// tracks the live working set — benchmarked on musl/aarch64 at ~400 MB
// reclaimed after a 400 MB alloc→free spike, vs 0 MB by default, at no
// throughput cost. (jemalloc with `muzzy_decay_ms:0` is an equivalent
// alternative; mimalloc+env is preferred on the musl/Alpine target — its
// `background_thread` is unsupported there.)
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};

use axum::Router;
use axum::extract::DefaultBodyLimit;
use oxicloud::access_log;
use oxicloud::interfaces::middleware::trace_span::{ClientIpMakeSpan, UuidRequestId};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{PropagateRequestIdLayer, SetRequestIdLayer};
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
    create_api_routes, create_health_routes, create_public_api_routes,
    web::{create_web_routes, resolve_static_path},
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Minimal CLI:
    //   --version                   Print version + branch + commit hash and exit.
    //   --config <path>             Load env from this file. When given, the default
    //                               `./.env` probe is INTENTIONALLY skipped — tests
    //                               use this to isolate from a developer's repo-root
    //                               `.env`, and operators get a reproducible "this
    //                               file and nothing else" boot.
    //   --select-storage <name>     One-shot repair: verify the named entry exists
    //                               in the current .env, UPDATE
    //                               admin_settings.storage.active_backend_name in
    //                               the DB, and exit. Does NOT boot the server.
    //                               Use to recover from the "boot fails on missing
    //                               entry" case — see
    //                               `docs/plan/storage-multi-entry.md` §Fallback.
    let mut args = std::env::args().skip(1);
    let mut config_path: Option<String> = None;
    let mut select_storage: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!(
                    "OxiCloud v{} (branch={} commit={})",
                    env!("CARGO_PKG_VERSION"),
                    env!("GIT_BRANCH"),
                    env!("GIT_HASH"),
                );
                return Ok(());
            }
            "--config" => {
                let Some(p) = args.next() else {
                    eprintln!("--config requires a path argument");
                    std::process::exit(2);
                };
                config_path = Some(p);
            }
            "--select-storage" => {
                let Some(name) = args.next() else {
                    eprintln!("--select-storage requires an entry name");
                    std::process::exit(2);
                };
                select_storage = Some(name);
            }
            "--fingerprint" => {
                // One-shot helper: compute the SSH-style colon-hex
                // fingerprint of a base64-encoded AES-256 key and
                // print to stdout. Same truncation used by the v1
                // header's `<key_fp>` field + the `backend_rotate`
                // completion summary — so an admin can:
                //   1. Look at the `head_key_fp` reported by the
                //      last rotate run.
                //   2. Run `oxicloud --fingerprint <base64key>` for
                //      each candidate in `.env`.
                //   3. Match — the key that produces the reported
                //      fingerprint is the current head; any other
                //      key in `_ENCRYPTION_KEY` no longer decrypts
                //      any live blob and can be dropped.
                //
                // Also accepts `-` for stdin so keys never touch the
                // shell history:
                //     echo -n '<base64>' | oxicloud --fingerprint -
                let Some(key_b64) = args.next() else {
                    eprintln!("--fingerprint requires a base64 key argument (or `-` for stdin)");
                    std::process::exit(2);
                };
                let key_b64 = if key_b64 == "-" {
                    use std::io::Read;
                    let mut buf = String::new();
                    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
                        eprintln!("failed to read key from stdin: {e}");
                        std::process::exit(2);
                    }
                    buf.trim().to_string()
                } else {
                    key_b64
                };
                match oxicloud::common::config::fingerprint_from_base64_key(&key_b64) {
                    Ok(fp) => {
                        println!("{fp}");
                        return Ok(());
                    }
                    Err(e) => {
                        eprintln!("--fingerprint: {e}");
                        std::process::exit(2);
                    }
                }
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => {
                eprintln!("Unknown argument: {other}");
                eprintln!("Try `oxicloud --help`.");
                std::process::exit(2);
            }
        }
    }

    match config_path {
        Some(ref path) => {
            // Explicit file → hard error on a missing/unreadable path.
            // Silent fallback would defeat the purpose of pinning the
            // config source.
            //
            // `from_filename_override` (not `from_filename`) so the
            // config file wins over the shell's process env. Without
            // this, an operator's leftover `export OXICLOUD_*` from a
            // dev session leaks into a `--config` invocation and
            // silently corrupts test/CI runs — a rejected shell var
            // stays in effect despite the "explicit config" contract.
            // For the default (no `--config`) path we KEEP the
            // non-overriding `dotenvy::dotenv()` — that path is dev
            // convenience where a live shell export is the expected
            // ad-hoc override.
            if let Err(e) = dotenvy::from_filename_override(path) {
                eprintln!("failed to load --config {path}: {e}");
                std::process::exit(2);
            }
        }
        None => {
            // Default dev-convenience probe at CWD/.env.
            dotenvy::dotenv().ok();
        }
    }

    // Build the Tokio runtime — needed either way (the repair flag also
    // needs async DB access). Sized from cgroup CPU quota — see
    // `build_runtime`.
    let runtime = build_runtime()?;

    // Repair-flag short-circuit. `--select-storage` runs the small
    // "verify entry + UPDATE pointer + exit" path and NEVER falls
    // through to booting the server — the operator restarts normally
    // after this exits.
    if let Some(name) = select_storage {
        return runtime.block_on(run_select_storage(&name));
    }

    runtime.block_on(run())
}

/// Print the `--help` output. Kept as a fn (not an inline string) so
/// the layout is easy to eyeball + doesn't clutter the argv match arm.
///
/// One `println!` per output line — source layout matches what the
/// user sees. Longer than one big raw string but grep-friendly (a
/// specific flag description shows up as its own hit) and diffs stay
/// line-local.
///
/// Sections: USAGE (invocation shapes) → OPTIONS (per-flag
/// explanations) → ENVIRONMENT (the small set of env vars an operator
/// checks at first boot). Everything else lives in `example.env` —
/// listing all 80+ env knobs here would rot every release.
fn print_help() {
    println!(
        "OxiCloud v{} (branch={} commit={})",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_BRANCH"),
        env!("GIT_HASH"),
    );
    println!();
    println!("USAGE:");
    println!("  oxicloud [--config <path>]              Boot the server. This is the normal");
    println!("                                          invocation for a docker/systemd unit.");
    println!();
    println!("  oxicloud --select-storage <name>        One-shot repair — set the active");
    println!("                                          storage entry in the DB and exit.");
    println!();
    println!("  oxicloud --fingerprint <base64key|->    One-shot helper — print the SSH-style");
    println!("                                          fingerprint of a base64 AES-256 key.");
    println!("                                          Same shape used by the v1 blob header");
    println!("                                          + `backend_rotate` completion summary.");
    println!("                                          Read stdin with `-` to keep keys out");
    println!("                                          of shell history.");
    println!();
    println!("  oxicloud --version                      Print version + commit and exit.");
    println!();
    println!("  oxicloud --help                         Print this help and exit.");
    println!();
    println!();
    println!("OPTIONS:");
    println!("  --config <path>");
    println!("      Load environment variables from <path> instead of the default `./.env`.");
    println!("      When set, the file WINS over any pre-existing shell exports (the");
    println!("      overriding variant of dotenvy). Use for reproducible CI / systemd");
    println!("      unit boots where a leaked shell export must not silently corrupt");
    println!("      config. Without this flag, the default `./.env` probe is");
    println!("      non-overriding — shell exports win — matching dev convenience.");
    println!();
    println!("  --select-storage <name>");
    println!("      Verify <name> is declared in `OXICLOUD_STORAGE_ENTRIES`, then set");
    println!("      `admin_settings.storage.active_backend_name = <name>` in the DB and");
    println!("      exit. Does NOT boot the server. Use to unblock boot after renaming");
    println!("      or removing a storage entry in `.env` while the DB still points at");
    println!("      the old name (the server aborts boot with a pointer to this flag");
    println!("      when that happens). See `docs/plan/storage-multi-entry.md`");
    println!("      §Fallback for the full recovery flow.");
    println!();
    println!("  --fingerprint <base64key | ->");
    println!("      Compute the SSH-style colon-hex fingerprint (16-hex, 8-byte");
    println!("      truncation of sha256) of a base64-encoded AES-256 key. Matches the");
    println!("      `head_key_fp` field the `backend_rotate` job reports on completion,");
    println!("      and the raw <key_fp> field embedded in every v1 blob header. Used");
    println!("      to identify which key in `OXICLOUD_STORAGE_<N>_ENCRYPTION_KEY`");
    println!("      corresponds to the current on-disk head — safe to drop any key");
    println!("      whose fingerprint does NOT match the last-successful rotate's");
    println!("      `head_key_fp`. Pass `-` to read the key from stdin so it never");
    println!("      touches shell history:");
    println!();
    println!("          echo -n '<base64>' | oxicloud --fingerprint -");
    println!();
    println!("  --version, -V");
    println!("      Print the version, git branch, and commit hash. Exits 0.");
    println!();
    println!("  --help, -h");
    println!("      Print this help. Exits 0.");
    println!();
    println!();
    println!("ENVIRONMENT:");
    println!("  DATABASE_URL          PostgreSQL connection string (required for boot and");
    println!("                        for --select-storage).");
    println!();
    println!("  OXICLOUD_SERVER_HOST  Bind host (default: 127.0.0.1).");
    println!("  OXICLOUD_SERVER_PORT  Bind port (default: 8086).");
    println!();
    println!("  OXICLOUD_STORAGE_ENTRIES=<n1>,<n2>,...");
    println!("                        Comma-separated list of named storage entries. Each");
    println!("                        entry N declared here reads its config from");
    println!("                        `OXICLOUD_STORAGE_<N>_BACKEND`,");
    println!("                        `OXICLOUD_STORAGE_<N>_S3_BUCKET`, etc. See");
    println!("                        `docs/plan/storage-multi-entry.md` for the full");
    println!("                        contract. When unset (with legacy flat storage");
    println!("                        vars present) one entry named `default` is");
    println!("                        synthesised.");
    println!();
    println!("The full env-var surface is documented in `example.env` at the repo root.");
}

/// Repair-flag body. Loads env config, parses entries, verifies the
/// requested name is declared, connects to PG, upserts
/// `admin_settings.storage.active_backend_name`. Never touches the
/// server — the operator restarts after this exits.
///
/// Exit codes:
/// - `0` on success.
/// - Non-zero via `std::process::exit` on every failure path (name
///   not declared, DB unreachable, upsert failed). Printed to stderr.
async fn run_select_storage(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    use common::config::AppConfig;
    use infrastructure::services::entry_backend::persist_active_backend_name;

    // Parse entries + validate `name` is declared. Loading AppConfig
    // here re-runs the same env-parse the server does at boot, so a
    // successful --select-storage guarantees a subsequent normal
    // boot will find the entry (no drift between the two code paths).
    let config = AppConfig::from_env();
    if config.storage_entries.is_empty() {
        eprintln!(
            "OXICLOUD_STORAGE_ENTRIES is not set (or synthesised — legacy path). \
             `--select-storage` needs at least one named entry to switch to."
        );
        std::process::exit(2);
    }
    if !config.storage_entries.iter().any(|e| e.name == name) {
        let available = config
            .storage_entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "entry `{name}` is not declared in OXICLOUD_STORAGE_ENTRIES. Available: [{available}]"
        );
        std::process::exit(2);
    }

    // Connect to PG using the same DATABASE_URL the server uses.
    let db_url = std::env::var("DATABASE_URL").map_err(
        |_| "DATABASE_URL not set — `--select-storage` needs the same DB the server would boot on",
    )?;
    let pool = sqlx::PgPool::connect(&db_url)
        .await
        .map_err(|e| format!("failed to connect to DATABASE_URL: {e}"))?;

    persist_active_backend_name(&pool, name)
        .await
        .map_err(|e| {
            format!("failed to write admin_settings.storage.active_backend_name = `{name}`: {e}")
        })?;

    println!(
        "active_backend_name = `{name}` written to admin_settings. Restart the server to switch."
    );
    Ok(())
}

/// Construct the multi-threaded Tokio runtime with explicit, CFS-quota-aware
/// pool sizes.
///
/// `#[tokio::main]` hides two defaults that misbehave under container limits:
///   • worker threads default to `available_parallelism()`, which honours CPU
///     affinity but **ignores the CFS quota** (`--cpus` / `cpu.max`) — so on a
///     2-core-quota container on a 64-core host it spawns 64 workers that
///     time-slice across 2 cores.
///   • the blocking pool defaults to a flat **512** threads — a multi-GB RSS
///     blast radius for this heavy `spawn_blocking` user.
///
/// Both come from [`common::runtime::runtime_pool_sizes`] (env-overridable via
/// `OXICLOUD_WORKER_THREADS` / `OXICLOUD_MAX_BLOCKING_THREADS`). Unset env on an
/// uncontended host reproduces the previous behaviour.
fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    let (workers, max_blocking) = common::runtime::runtime_pool_sizes();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(max_blocking)
        .thread_name("oxicloud-worker")
        .enable_all()
        .build()
}

/// Async entrypoint, driven by the runtime built in [`main`].
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing.
    //
    // Default access-log policy — two independent directives are
    // injected unless the operator has already named them:
    //
    //   `http=warn`         (4xx + 5xx for every access-log target)
    //   `http::web=error`   (5xx only for static / ServeDir / catch-all)
    //
    // `http::web` is pulled down to ERROR because it's the noisiest
    // surface (every CSS/JS/img/favicon request hits it) and its
    // 4xx are almost always "browser asked for a file we don't ship",
    // not a real signal. Operators investigating a 404 storm can
    // promote it back: `RUST_LOG=info,http::web=warn`.
    //
    // The detection is substring-based:
    //   - `http=` in RUST_LOG  → operator owns the http baseline.
    //   - `http::web=` in RUST_LOG → operator owns the web subtarget.
    // The two are independent — supplying `http=info` still gets a
    // free `http::web=error` unless the operator named that too.
    //
    // Empty / unset / no http directives → both defaults applied.
    // Note that `http::web=…` does NOT contain `http=` as a substring
    // (different characters around the `:`), so the two checks don't
    // alias each other.
    let rust_log = match std::env::var("RUST_LOG").ok().filter(|s| !s.is_empty()) {
        None => "info,http=warn,http::web=error".to_string(),
        Some(mut s) => {
            if !s.contains("http=") {
                s.push_str(",http=warn");
            }
            if !s.contains("http::web=") {
                s.push_str(",http::web=error");
            }
            s
        }
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(rust_log))
        .with(tracing_subscriber::fmt::layer())
        .init();

    oxicloud::interfaces::middleware::trusted_proxy::log_config();

    tracing::info!(
        "OxiCloud v{} | branch={} commit={}",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_BRANCH"),
        env!("GIT_HASH")
    );

    // Surface the runtime pool sizing chosen in `build_runtime`. `available`
    // is what tokio's default would have used; `cgroup_cpu_quota` is the CFS
    // limit it ignores. When the two diverge, the worker count tracks the
    // smaller (effective) value — the whole point of the explicit builder.
    let (rt_workers, rt_max_blocking) = common::runtime::runtime_pool_sizes();
    tracing::info!(
        worker_threads = rt_workers,
        max_blocking_threads = rt_max_blocking,
        available_parallelism =
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        cgroup_cpu_quota = ?common::runtime::cgroup_cpu_quota(),
        "Tokio runtime pools sized"
    );

    // Load configuration from environment variables
    let config = common::config::AppConfig::from_env();

    // SECURITY: fail-closed on incoherent auth-method configuration. A
    // magic-link-only policy without a working SMTP sender locks every
    // user out — nothing can mint tokens, so nobody can log in. Refuse
    // to start rather than boot into a bricked auth surface.
    //
    // The SMTP-mock (`OXICLOUD_SMTP_MOCK=true` in `tests/common/server.env`)
    // sets `OXICLOUD_SMTP_HOST=localhost`, so `is_enabled()` returns
    // true and the Hurl test harness satisfies this gate without a real
    // mail server.
    if config
        .auth
        .allowed_auth_methods
        .contains(&common::config::AuthMethod::MagicLink)
        && !config
            .auth
            .allowed_auth_methods
            .contains(&common::config::AuthMethod::Password)
        && !config
            .auth
            .allowed_auth_methods
            .contains(&common::config::AuthMethod::Oidc)
        && !config.smtp.is_enabled()
    {
        panic!(
            "FATAL: OXICLOUD_AUTH_METHODS enables `magic_link` as the ONLY \
             self-service auth method, but no SMTP transport is configured. \
             Set OXICLOUD_SMTP_HOST (and matching OXICLOUD_SMTP_* settings), \
             add `password` or `oidc` to OXICLOUD_AUTH_METHODS, or drop \
             `magic_link` from the list. Refusing to start."
        );
    }

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

    // Locales directory for i18n. Resolved from wherever the SPA is actually
    // served (the Vite `static-dist/` build, or the configured static path in
    // the container) so deployments find their locale files correctly. A source
    // checkout without a built SPA still has the canonical locales under the
    // frontend static assets, so fall back to those for `just dev`.
    //
    // Read-only at runtime: locales ship as static assets (Vite copies
    // `frontend/static/locales` into the build, the Dockerfile copies that into
    // /app/static). Fail-fast if the path is missing rather than silently
    // creating an empty directory and limping along with a "translation missing"
    // error on every request later.
    let locales_path = {
        let served = resolve_static_path(&config).join("locales");
        if served.is_dir() {
            served
        } else {
            std::path::PathBuf::from("frontend/static/locales")
        }
    };
    if !locales_path.is_dir() {
        panic!(
            "FATAL: locales directory not found at {}. \
             Check OXICLOUD_STATIC_PATH (currently {}) and ensure the \
             static asset bundle includes a `locales/` subdirectory.",
            locales_path.display(),
            config.static_path.display()
        );
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
    // RFC 6764 discovery for both CalDAV and CardDAV (public redirects).
    let well_known_router =
        caldav_handler::well_known_routes().merge(carddav_handler::well_known_routes());
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

        // Access-log targets are declared per-mount via `access_log!(…)`
        // — see `interfaces/middleware/trace_span.rs` for the catalogue.
        app = Router::new()
            // Health / readiness probes — no auth, mounted at root
            .merge(health_routes.layer(access_log!("http::probe")))
            // Magic-link redemption — top-level, no `/api/` prefix
            .merge(magic_link_router.layer(access_log!("http::web")))
            // Rate-limited auth endpoints (login, register, refresh)
            .nest(
                "/api/auth",
                auth_login.layer(access_log!("http::api::auth")),
            )
            .nest(
                "/api/auth",
                auth_register.layer(access_log!("http::api::auth")),
            )
            .nest(
                "/api/auth",
                auth_refresh.layer(access_log!("http::api::auth")),
            )
            // Public auth endpoints (status, OIDC)
            .nest(
                "/api/auth",
                auth_public.layer(access_log!("http::api::auth")),
            )
            // Protected auth endpoints (/me, /change-password, /logout)
            .nest(
                "/api/auth",
                auth_protected.layer(access_log!("http::api::auth")),
            )
            // App password management (create, list, revoke)
            .nest(
                "/api/auth",
                app_pw_protected.layer(access_log!("http::api::auth")),
            )
            // One-time setup endpoint — public, rate-limited
            .nest("/api", setup_router.layer(access_log!("http::api")))
            // Device Auth Grant public endpoints (authorize + token polling)
            .nest(
                "/api/auth/device",
                device_public.layer(access_log!("http::api::auth")),
            )
            // Device Auth Grant protected endpoints (verify + device management)
            .nest(
                "/api/auth/device",
                device_protected.layer(access_log!("http::api::auth")),
            )
            // Public API routes (share access, i18n) — no auth required
            .nest("/api", public_api_routes.layer(access_log!("http::api")))
            // All other API routes are protected by auth middleware
            .nest("/api", protected_api.layer(access_log!("http::api")))
            // RFC 6764 well-known discovery (public, no auth — just redirects)
            .merge(well_known_router.clone().layer(access_log!("http::dav")))
            // CalDAV/CardDAV/WebDAV protocols merged at top-level for client compatibility
            .merge(caldav_protected.layer(access_log!("http::dav")))
            .merge(carddav_protected.layer(access_log!("http::dav")))
            .merge(webdav_protected.layer(access_log!("http::dav")))
            // Web (HTML pages) — also the ServeDir fallback root, so
            // static asset hits land here. We keep them on the `web`
            // target for simplicity; switch to `http::static` when
            // the static surface is split into its own router.
            .merge(web_routes.layer(access_log!("http::web")));

        // Mount Nextcloud routes (uses its own Basic Auth middleware).
        // **Merged BEFORE the trace + request-id layers** so NC requests
        // get the same `request_id` / `user_id` / `client_ip` span
        // fields as every other surface — see
        // `interfaces/middleware/trace_span.rs::ClientIpMakeSpan`.
        if let Some(nc_router) = nextcloud_router {
            app = app.merge(
                nc_router
                    .with_state(app_state.clone())
                    .layer(access_log!("http::nextcloud")),
            );
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
                .nest("/wopi", wopi_protocol.layer(access_log!("http::wopi")))
                .nest(
                    "/api/wopi",
                    wopi_api_protected.layer(access_log!("http::api")),
                );
        }

        // ── Trace + request-id layers applied LAST so every route
        //    merged above (including the conditional NC and WOPI
        //    surfaces) is wrapped. New protocol routers added later
        //    only have to be merged before this point to get tracing
        //    for free — no second site to remember to update.
        app = app
            .layer(TraceLayer::new_for_http().make_span_with(ClientIpMakeSpan))
            .layer(PropagateRequestIdLayer::x_request_id())
            .layer(SetRequestIdLayer::x_request_id(UuidRequestId));
    } else {
        // Auth disabled — no middleware applied
        tracing::warn!("Authentication is DISABLED — all API routes are publicly accessible");
        app = Router::new()
            // Health / readiness probes — no auth, mounted at root
            .merge(health_routes.layer(access_log!("http::probe")))
            .nest("/api", public_api_routes.layer(access_log!("http::api")))
            .nest("/api", api_routes.layer(access_log!("http::api")))
            // RFC 6764 well-known discovery (just redirects)
            .merge(well_known_router.layer(access_log!("http::dav")))
            // CalDAV/CardDAV/WebDAV protocols merged at top-level
            .merge(caldav_router.layer(access_log!("http::dav")))
            .merge(carddav_router.layer(access_log!("http::dav")))
            .merge(webdav_router.layer(access_log!("http::dav")))
            .merge(web_routes.layer(access_log!("http::web")));

        // Mount Nextcloud routes — merged BEFORE the trace + request-id
        // layers so NC requests get the same span fields as every
        // other surface (matches the auth-enabled branch above).
        if let Some(nc_router) = nextcloud_router {
            app = app.merge(
                nc_router
                    .with_state(app_state.clone())
                    .layer(access_log!("http::nextcloud")),
            );
        }

        // Mount WOPI routes (no auth middleware when auth is disabled).
        // Same reasoning: merge before the trace layer.
        if let Some((wopi_protocol, wopi_api)) = wopi_routes {
            app = app
                .nest("/wopi", wopi_protocol.layer(access_log!("http::wopi")))
                .nest("/api/wopi", wopi_api.layer(access_log!("http::api")));
        }

        // ── Trace + request-id layers applied LAST. See the
        //    auth-enabled branch above for the rationale.
        app = app
            .layer(TraceLayer::new_for_http().make_span_with(ClientIpMakeSpan))
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

        // Explicit quality: the layer's default maps to Brotli QUALITY 11
        // (async-compression Level::Default → BrotliEncoderParams::default(),
        // brotli-8.0.2 encode.rs:323) — a deploy-grade setting that cost
        // ~90 ms of CPU per 64 KiB JSON response. Level 4 emits ~15 % more
        // bytes at ~1 % of the CPU (0.9 ms) — measured in
        // benches/STATIC-PRECOMPRESSED.md. Applies to gzip too (level 4,
        // the classic dynamic-content setting).
        app = app.layer(
            CompressionLayer::new()
                .quality(tower_http::CompressionLevel::Precise(4))
                .compress_when(predicate),
        );
    }

    // ── Security headers ─────────────────────────────────────────────────
    // Applied globally so every response (API, static, DAV) carries them.
    use axum::http::HeaderValue;
    use axum::http::header::HeaderName;

    // Content-Security-Policy is content-type-aware.
    //
    // HTML documents are served by the SvelteKit SPA, which emits its OWN
    // strict, hash-based CSP via a <meta> tag (see `kit.csp` in
    // frontend/svelte.config.js). SvelteKit's inline bootstrap script is
    // hashed per build, so a static `script-src 'self'` header here would
    // block it and blank the app. We therefore do NOT send a CSP header on
    // text/html responses and let the SPA's meta policy govern them.
    //
    // Every other response (API JSON, DAV XML, static JS/CSS/img) gets the
    // strict header below. Notes:
    //   • style-src 'unsafe-inline': the frontend sets inline styles at
    //     runtime (e.g. element.style.display); hashes can't cover those.
    //   • frame-src '*': only matches network schemes, so 'blob:' is listed
    //     explicitly for inline PDF/document viewers.
    //   • media-src 'blob:': needed for blob: video/audio playback.
    //   • form-action 'https:': the WOPI office editor is launched by POSTing a
    //     token form to a cross-origin, admin-configured Collabora/OnlyOffice
    //     host. Mirrors the SPA meta policy in frontend/svelte.config.js.
    // The four static security headers ride in the same response pass —
    // they used to be four separate `SetResponseHeaderLayer`s stacked on
    // top of this middleware (5 tower layers per response). Folding them
    // here measured 1.43x per request / −26 allocs with a byte-identical
    // header set, including on 304s (benches/ROUND12.md §M3). They are
    // inserted BEFORE the 304 early-return below because the standalone
    // layers stamped 304s too.
    async fn content_security_policy(
        req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        let mut res = next.run(req).await;
        {
            let h = res.headers_mut();
            h.insert(
                HeaderName::from_static("x-content-type-options"),
                HeaderValue::from_static("nosniff"),
            );
            h.insert(
                HeaderName::from_static("x-frame-options"),
                HeaderValue::from_static("DENY"),
            );
            h.insert(
                HeaderName::from_static("referrer-policy"),
                HeaderValue::from_static("strict-origin-when-cross-origin"),
            );
            h.insert(
                HeaderName::from_static("permissions-policy"),
                HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
            );
        }
        // A 304 Not Modified carries no entity headers (no Content-Type) since
        // there's no body — `is_html` would read `None` and misclassify it as
        // "not html", attaching the strict headerless CSP below. Browsers merge
        // a 304's headers into the cached document's effective response, so
        // that stray header would then stack with (and defeat) the SPA's own
        // hash-based `<meta>` CSP on every revalidated repeat visit — this was
        // a real bug (see git blame): a browser tab reopened at `/login` after
        // the first, freshly-fetched visit got permanently stuck behind the
        // boot spinner because its now-conditionally-cached `200` picked up an
        // extra hash-less `script-src 'self'` header from the 304 that
        // revalidated it, blocking the app's own inline hydration script.
        // Nothing to add on a 304 regardless — its headers must only carry
        // caching metadata, never a fresh policy decision.
        if res.status() == axum::http::StatusCode::NOT_MODIFIED {
            return res;
        }
        let is_html = res
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html"));
        if is_html {
            // `no-store` (not just `no-cache`) on the SPA shell: Chrome/Firefox/
            // Safari all treat `no-store` as an explicit opt-out of the
            // back-forward cache (bfcache), which is a full in-memory snapshot
            // of the page that bypasses HTTP revalidation entirely — `no-cache`
            // alone does NOT prevent it. Without this, a shell instance loaded
            // before a deploy can be resurrected byte-for-byte (old inline
            // hydration script + old CSP hash) after navigating away and back —
            // e.g. the OIDC login round-trip's two full-page navigations — and
            // the resurrected page's old CSP `<meta>` no longer matches assets
            // referenced by the current build, leaving the app permanently
            // stuck behind the boot spinner until a hard reload.
            res.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
        } else {
            res.headers_mut().insert(
                axum::http::header::CONTENT_SECURITY_POLICY,
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
                     form-action 'self' https:",
                ),
            );
        }
        res
    }

    app = app.layer(axum::middleware::from_fn(content_security_policy));

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
