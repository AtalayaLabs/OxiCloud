//! Round-13 HTTP micro-pack (no Postgres).
//!
//! Two sections, each BEFORE (verbatim replica of the shipped shape) vs
//! AFTER (proposed shape), with byte-identity / equivalence gates:
//!
//!   [H1] Duplicate `TraceLayer` on `/api` — the inner
//!        `TraceLayer::new_for_http()` in `routes.rs` sat under the global
//!        `TraceLayer + ClientIpMakeSpan` stack in `main.rs`, so every
//!        `/api` request was wrapped in TWO span/response-future layers.
//!        Measured end-to-end through real axum routers, one stack vs two.
//!   [H2] Per-request `client_ip` `String` in the span factory —
//!        `ClientIpMakeSpan::make_span` allocated an owned `String` on every
//!        request purely to feed the span's `%client_ip` Display, vs a
//!        borrow-only `ClientIpDisplay` that renders into the span storage.
//!
//! Run:
//!   cargo bench --features=bench --bench bench_round13_micro
//! Tunables (env): BENCH_ITERS (200000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::http::HeaderMap;
use oxicloud::interfaces::middleware::trusted_proxy::{
    ClientIpDisplay, client_ip_display_from_parts, client_ip_from_parts,
};

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Measured {
    wall_ns_per_op: f64,
    allocs_per_op: f64,
}

fn measure<F: FnMut()>(iters: usize, mut f: F) -> Measured {
    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let wall = t.elapsed().as_nanos() as f64 / iters as f64;
    let allocs = (ALLOC_CALLS.load(Ordering::Relaxed) - a0) as f64 / iters as f64;
    Measured {
        wall_ns_per_op: wall,
        allocs_per_op: allocs,
    }
}

fn print_row(label: &str, m: &Measured) {
    println!(
        "| {:<40} | {:>12.1} | {:>10.2} |",
        label, m.wall_ns_per_op, m.allocs_per_op
    );
}

// ────────────────────────────────────────────────────────────────────────────
// [H1] Duplicate TraceLayer on /api — one stack vs two, end-to-end
// ────────────────────────────────────────────────────────────────────────────

fn section_trace_dedup() {
    use axum::Router;
    use axum::routing::get;
    use oxicloud::interfaces::middleware::trace_span::ClientIpMakeSpan;
    use tower::ServiceExt;
    use tower_http::trace::TraceLayer;

    let iters: usize = env_or("BENCH_ITERS", 200_000) / 20;
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("rt");

    async fn handler() -> &'static str {
        "{\"ok\":true}"
    }

    // AFTER: the global stack only (one TraceLayer + ClientIpMakeSpan).
    let after_app = Router::new()
        .route("/api/x", get(handler))
        .layer(TraceLayer::new_for_http().make_span_with(ClientIpMakeSpan));

    // BEFORE: the inner per-router TraceLayer, then the global stack on top.
    let before_app = Router::new()
        .route("/api/x", get(handler))
        .layer(TraceLayer::new_for_http())
        .layer(TraceLayer::new_for_http().make_span_with(ClientIpMakeSpan));

    let call = |app: &axum::Router| {
        let app = app.clone();
        rt.block_on(async move {
            let res = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/x")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            res.status()
        })
    };

    // Gate: identical status through both stacks.
    assert_eq!(call(&before_app), call(&after_app), "status differs");
    println!("# [H1] gate: /api response status identical with 1 vs 2 trace layers — OK");

    let m_before = measure(iters, || {
        black_box(call(&before_app));
    });
    let m_after = measure(iters, || {
        black_box(call(&after_app));
    });

    println!("\n## [H1] Duplicate TraceLayer on /api (per request, incl. router)");
    println!("| arm | ns/op | allocs/op |");
    print_row("BEFORE 2 trace layers", &m_before);
    print_row("AFTER  1 (global only)", &m_after);
    println!(
        "# {:.2}x wall, {:.1} fewer allocs/request",
        m_before.wall_ns_per_op / m_after.wall_ns_per_op,
        m_before.allocs_per_op - m_after.allocs_per_op
    );
    if m_after.wall_ns_per_op >= m_before.wall_ns_per_op {
        eprintln!("GATE FAIL [H1]: dedup not faster — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [H2] client_ip String vs borrow-only Display
// ────────────────────────────────────────────────────────────────────────────

fn section_client_ip() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);

    // Three realistic request shapes.
    let direct_peer: Option<SocketAddr> = Some("203.0.113.7:54321".parse().unwrap());
    let empty_headers = HeaderMap::new();

    let proxy_peer: Option<SocketAddr> = Some("10.0.0.1:443".parse().unwrap());
    let mut xff_headers = HeaderMap::new();
    xff_headers.insert(
        "x-forwarded-for",
        "198.51.100.23, 10.0.0.1".parse().unwrap(),
    );

    // Equivalence gate: Display output identical to the owned String for all
    // shapes (note: the trusted-proxy branch only forwards when the peer is
    // an actually-configured trusted CIDR; with none configured both peers
    // render as the direct address — so the gate compares the SAME resolver
    // logic on both sides, which is what matters for byte-identity).
    for (headers, peer) in [
        (&empty_headers, direct_peer),
        (&xff_headers, proxy_peer),
        (&empty_headers, None),
    ] {
        let owned = client_ip_from_parts(headers, peer, true);
        let borrowed = format!("{}", client_ip_display_from_parts(headers, peer, true));
        assert_eq!(owned, borrowed, "client_ip bytes differ");
    }
    // Directly exercise every ClientIpDisplay variant's Display.
    assert_eq!(
        format!("{}", ClientIpDisplay::Forwarded("1.2.3.4")),
        "1.2.3.4"
    );
    assert_eq!(
        format!(
            "{}",
            ClientIpDisplay::PeerWithPort("5.6.7.8:9".parse().unwrap())
        ),
        "5.6.7.8:9"
    );
    assert_eq!(
        format!("{}", ClientIpDisplay::PeerIp("5.6.7.8".parse().unwrap())),
        "5.6.7.8"
    );
    assert_eq!(format!("{}", ClientIpDisplay::Unknown), "unknown");
    println!("# [H2] gate: borrow-only Display renders byte-identical to owned String — OK");

    // The span records `client_ip = %ip`; emulate that terminal render into a
    // reusable String (the span's field storage) for BOTH arms so we isolate
    // the ONE allocation the owned resolver adds on top.
    use std::fmt::Write as _;

    let m_before = measure(iters, || {
        let ip = client_ip_from_parts(black_box(&empty_headers), black_box(direct_peer), true);
        let mut sink = String::new();
        let _ = write!(sink, "{ip}");
        black_box(sink);
    });
    let m_after = measure(iters, || {
        let ip =
            client_ip_display_from_parts(black_box(&empty_headers), black_box(direct_peer), true);
        let mut sink = String::new();
        let _ = write!(sink, "{ip}");
        black_box(sink);
    });

    println!("\n## [H2] client_ip resolution for the span factory (direct peer)");
    println!("| arm | ns/op | allocs/op |");
    print_row("BEFORE owned String + render", &m_before);
    print_row("AFTER  borrow Display + render", &m_after);
    println!(
        "# {:.2}x wall, {:.1} fewer allocs/request",
        m_before.wall_ns_per_op / m_after.wall_ns_per_op,
        m_before.allocs_per_op - m_after.allocs_per_op
    );
    if m_after.allocs_per_op >= m_before.allocs_per_op {
        eprintln!("GATE FAIL [H2]: borrow arm did not remove an allocation — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [L1] Locale supported-codes: per-request rebuild vs precomputed borrow
// ────────────────────────────────────────────────────────────────────────────

fn section_locale() {
    use oxicloud::common::locale::LocaleRegistry;
    use std::path::Path;

    let iters: usize = env_or("BENCH_ITERS", 200_000) / 2;

    // Real registry over the shipped locales (16 JSON files).
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("frontend/static/locales");
    let registry = match LocaleRegistry::discover(&dir, "en") {
        Ok(r) => r,
        Err(e) => {
            println!("# [L1] skipped — locale registry unavailable: {e}");
            return;
        }
    };
    let n = registry.supported_codes().len();

    // Equivalence gate: same code SET both ways (order differs — the
    // Accept-Language crate ranks by header q-values, not list order).
    let mut before_set: Vec<String> = registry.iter().map(|l| l.as_str().to_string()).collect();
    let mut after_set: Vec<String> = registry.supported_codes().to_vec();
    before_set.sort();
    after_set.sort();
    assert_eq!(before_set, after_set, "supported-code sets differ");
    println!("# [L1] gate: rebuilt and precomputed supported-code sets identical ({n} codes) — OK");

    // BEFORE, verbatim old extractor: N owned Strings + the &str view.
    let m_before = measure(iters, || {
        let owned: Vec<String> = registry.iter().map(|l| l.as_str().to_string()).collect();
        let view: Vec<&str> = owned.iter().map(String::as_str).collect();
        black_box(&view);
        black_box(owned);
    });
    // AFTER: borrow the precomputed list; build only the &str view.
    let m_after = measure(iters, || {
        let view: Vec<&str> = registry
            .supported_codes()
            .iter()
            .map(String::as_str)
            .collect();
        black_box(view);
    });

    println!("\n## [L1] Locale supported-codes for Accept-Language ({n} locales)");
    println!("| arm | ns/op | allocs/op |");
    print_row("BEFORE rebuild N Strings + view", &m_before);
    print_row("AFTER  borrow precomputed + view", &m_after);
    println!(
        "# {:.2}x wall, {:.1} fewer allocs per anonymous request",
        m_before.wall_ns_per_op / m_after.wall_ns_per_op,
        m_before.allocs_per_op - m_after.allocs_per_op
    );
    if m_after.wall_ns_per_op >= m_before.wall_ns_per_op {
        eprintln!("GATE FAIL [L1]: precomputed borrow not faster — rollback");
        std::process::exit(1);
    }
}

fn main() {
    println!("#################################################################");
    println!("# Round-13 HTTP micro-pack");
    println!("#################################################################\n");

    section_trace_dedup();
    section_client_ip();
    section_locale();

    println!("\nGATE PASS (all sections)");
}
