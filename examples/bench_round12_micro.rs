//! Round-12 CPU/alloc micro-pack (no Postgres).
//!
//! Five sections, each BEFORE (verbatim replica of the shipped shape) vs
//! AFTER (proposed shape), with byte-identity / equivalence gates:
//!
//!   [1] Listing JSON serialization — axum `Json`'s 128-byte `BytesMut`
//!       seed + doubling-realloc chain vs a pre-sized `Vec` +
//!       `serde_json::to_writer` (the `sized_json` helper).
//!   [2] Dynamic-compression predicate — the ~28-node `And` chain (each
//!       `NotForContentType` re-reading + re-validating the Content-Type
//!       header) vs a single-pass policy node.
//!   [3] Security-header stack — 4 `SetResponseHeaderLayer`s wrapping the
//!       CSP middleware (5 tower layers) vs the headers folded into the
//!       CSP pass (1 layer).
//!   [4] Media capture-metadata extraction — the 2-3 opens per image /
//!       2 per video (kamadak full read + nom-exif path re-reads) vs the
//!       single-read shape (nom-exif fed from the in-RAM bytes,
//!       one `MediaParser`, kind-dispatched videos).
//!   [5] Chunked-upload session map — 2 (prepare) + 3 (commit) DashMap
//!       lookups per chunk plus 2 `Uuid::to_string` allocs vs fused
//!       owner-check lookups + stack-encoded uuid compare.
//!
//! Run:
//!   cargo run --release --features bench --example bench_round12_micro
//! Tunables (env): BENCH_ITERS (100000), BENCH_ROWS (500),
//!   BENCH_MEDIA_ITERS (300), BENCH_COLD_ITERS (20; 0 disables the
//!   drop_caches cold arms, which need root)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bytes::{BufMut, Bytes, BytesMut};
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use dashmap::DashMap;
use uuid::Uuid;

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
        "| {:<38} | {:>12.1} | {:>10.2} |",
        label, m.wall_ns_per_op, m.allocs_per_op
    );
}

// ────────────────────────────────────────────────────────────────────────────
// [1] Listing JSON — axum Json 128-byte seed vs pre-sized writer
// ────────────────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct RowDto {
    id: String,
    name: String,
    path: String,
    size: u64,
    mime_type: Arc<str>,
    folder_id: Option<String>,
    created_at: u64,
    modified_at: u64,
    icon_class: Arc<str>,
    icon_special_class: Arc<str>,
    category: Arc<str>,
    size_formatted: String,
}

fn make_rows(n: usize) -> Vec<RowDto> {
    let mime: Arc<str> = Arc::from("image/jpeg");
    let icon: Arc<str> = Arc::from("fas fa-file-image");
    let special: Arc<str> = Arc::from("image-icon");
    let category: Arc<str> = Arc::from("Image");
    (0..n)
        .map(|i| RowDto {
            id: Uuid::new_v4().to_string(),
            name: format!("IMG_2024_{i:05}.jpg"),
            path: format!("/Photos/2024/Summer trip/IMG_2024_{i:05}.jpg"),
            size: 3_274_291 + i as u64,
            mime_type: mime.clone(),
            folder_id: Some(Uuid::new_v4().to_string()),
            created_at: 1_719_830_000 + i as u64,
            modified_at: 1_719_830_100 + i as u64,
            icon_class: icon.clone(),
            icon_special_class: special.clone(),
            category: category.clone(),
            size_formatted: "3.27 MB".to_string(),
        })
        .collect()
}

/// BEFORE, verbatim axum `Json::into_response` buffer flow.
fn json_before(rows: &[RowDto]) -> Bytes {
    let mut buf = BytesMut::with_capacity(128).writer();
    serde_json::to_writer(&mut buf, rows).expect("serialize");
    buf.into_inner().freeze()
}

/// AFTER: the `sized_json` shape — one pre-sized allocation.
fn json_after(rows: &[RowDto], per_row_estimate: usize) -> Bytes {
    let mut buf = Vec::with_capacity(64 + rows.len() * per_row_estimate);
    serde_json::to_writer(&mut buf, rows).expect("serialize");
    Bytes::from(buf)
}

fn section_sized_json() {
    let n: usize = env_or("BENCH_ROWS", 500);
    let iters: usize = env_or("BENCH_ITERS", 100_000) / 100;
    let rows = make_rows(n);

    let b = json_before(&rows);
    let a = json_after(&rows, 384);
    assert_eq!(b, a, "serialized bytes differ");
    let actual = b.len() / n;
    println!(
        "# [1] gate: bytes identical — OK ({} rows, {} B total, ~{} B/row, estimate 384)",
        n,
        b.len(),
        actual
    );

    let before = measure(iters, || {
        black_box(json_before(black_box(&rows)));
    });
    let after = measure(iters, || {
        black_box(json_after(black_box(&rows), 384));
    });

    println!("\n## [1] Listing JSON serialization ({n} rows)");
    println!("| arm | ns/op | allocs/op |");
    print_row("BEFORE axum Json (128 B seed)", &before);
    print_row("AFTER  sized_json (pre-sized)", &after);
    println!(
        "# {:.2}x wall, {:.1} fewer allocs/response",
        before.wall_ns_per_op / after.wall_ns_per_op,
        before.allocs_per_op - after.allocs_per_op
    );
    if after.wall_ns_per_op >= before.wall_ns_per_op {
        eprintln!("GATE FAIL [1]: pre-sized arm not faster — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [2] Compression predicate — 28-node And chain vs single pass
// ────────────────────────────────────────────────────────────────────────────

mod predicate_bench {
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
    use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};

    /// BEFORE, verbatim `main.rs` predicate (SizeAbove + 27 content-type
    /// exclusions + Content-Disposition guard, left-nested `And`).
    #[derive(Clone, Copy)]
    pub struct NotForDownloads;
    impl Predicate for NotForDownloads {
        fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
        where
            B: http_body::Body,
        {
            !response.headers().contains_key(CONTENT_DISPOSITION)
        }
    }

    pub fn before_predicate() -> impl Predicate {
        SizeAbove::new(256)
            .and(NotForContentType::GRPC)
            .and(NotForContentType::SSE)
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
            .and(NotForContentType::const_new("video/"))
            .and(NotForContentType::const_new("audio/"))
            .and(NotForContentType::const_new("font/woff"))
            .and(NotForContentType::const_new("application/font-woff"))
            .and(NotForContentType::const_new("application/zip"))
            .and(NotForContentType::const_new("application/gzip"))
            .and(NotForContentType::const_new("application/x-gzip"))
            .and(NotForContentType::const_new("application/x-tar"))
            .and(NotForContentType::const_new("application/x-7z-compressed"))
            .and(NotForContentType::const_new("application/x-rar-compressed"))
            .and(NotForContentType::const_new("application/x-bzip2"))
            .and(NotForContentType::const_new("application/zstd"))
            .and(NotForContentType::const_new("application/x-xz"))
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
            .and(NotForContentType::const_new("application/pdf"))
            .and(NotForContentType::const_new("application/octet-stream"))
            .and(NotForDownloads)
    }

    /// AFTER: the single-pass content-policy node (chained after the same
    /// `SizeAbove`, which keeps tower-http's size heuristics verbatim).
    /// One Content-Type read + one prefix scan + one disposition probe.
    #[derive(Clone, Copy)]
    pub struct SinglePassContentPolicy;

    /// Exact prefix set of the BEFORE chain, in chain order.
    const EXCLUDED_CT_PREFIXES: &[&str] = &[
        "application/grpc",
        "text/event-stream",
        "image/jpeg",
        "image/png",
        "image/gif",
        "image/webp",
        "image/avif",
        "image/heic",
        "image/heif",
        "image/jp2",
        "image/x-icon",
        "image/vnd.microsoft.icon",
        "video/",
        "audio/",
        "font/woff",
        "application/font-woff",
        "application/zip",
        "application/gzip",
        "application/x-gzip",
        "application/x-tar",
        "application/x-7z-compressed",
        "application/x-rar-compressed",
        "application/x-bzip2",
        "application/zstd",
        "application/x-xz",
        "application/vnd.openxmlformats-officedocument",
        "application/vnd.oasis.opendocument",
        "application/epub+zip",
        "application/java-archive",
        "application/vnd.android.package-archive",
        "application/pdf",
        "application/octet-stream",
    ];

    impl Predicate for SinglePassContentPolicy {
        fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
        where
            B: http_body::Body,
        {
            let headers = response.headers();
            // Mirror `NotForContentType`: a missing / non-UTF8 Content-Type
            // is compressible as far as the type exclusions are concerned.
            let ct = headers
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !ct.is_empty()
                && EXCLUDED_CT_PREFIXES
                    .iter()
                    .any(|prefix| ct.starts_with(prefix))
            {
                return false;
            }
            !headers.contains_key(CONTENT_DISPOSITION)
        }
    }

    pub fn after_predicate() -> impl Predicate {
        SizeAbove::new(256).and(SinglePassContentPolicy)
    }
}

fn section_predicate() {
    use axum::body::Body;
    use axum::http::Response;
    use tower_http::compression::predicate::Predicate;

    let iters: usize = env_or("BENCH_ITERS", 100_000);
    let before = predicate_bench::before_predicate();
    let after = predicate_bench::after_predicate();

    // Corpus: (content-type, content-length, disposition, label). Covers the
    // compressible hot cases, every exclusion family, edge cases.
    let mut corpus: Vec<(Response<Body>, &'static str)> = Vec::new();
    let mk = |ct: Option<&str>, len: usize, disp: bool| -> Response<Body> {
        let mut b = Response::builder().status(200);
        if let Some(ct) = ct {
            b = b.header("content-type", ct);
        }
        b = b.header("content-length", len.to_string());
        if disp {
            b = b.header("content-disposition", "attachment; filename=\"x\"");
        }
        b.body(Body::empty()).unwrap()
    };
    corpus.push((mk(Some("application/json"), 50_000, false), "json 50K"));
    corpus.push((
        mk(Some("text/html; charset=utf-8"), 8_000, false),
        "html 8K",
    ));
    corpus.push((mk(Some("image/jpeg"), 500_000, false), "jpeg"));
    corpus.push((mk(Some("image/svg+xml"), 12_000, false), "svg"));
    corpus.push((mk(Some("video/mp4"), 10_000_000, false), "mp4"));
    corpus.push((mk(Some("application/pdf"), 900_000, false), "pdf"));
    corpus.push((mk(Some("application/zip"), 70_000, false), "zip"));
    corpus.push((
        mk(
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
            90_000,
            false,
        ),
        "docx",
    ));
    corpus.push((mk(Some("application/json"), 100, false), "tiny json"));
    corpus.push((mk(Some("text/event-stream"), 50_000, false), "sse"));
    corpus.push((mk(Some("application/grpc"), 50_000, false), "grpc"));
    corpus.push((mk(Some("font/woff2"), 30_000, false), "woff2"));
    corpus.push((mk(Some("font/woff"), 30_000, false), "woff"));
    corpus.push((mk(Some("application/xml"), 20_000, true), "download xml"));
    corpus.push((mk(None, 20_000, false), "no content-type"));
    corpus.push((mk(Some("application/octet-stream"), 5_000, false), "octet"));
    corpus.push((mk(Some("audio/flac"), 5_000_000, false), "flac"));
    corpus.push((mk(Some("image/x-icon"), 5_000, false), "ico"));

    // Verdict-identity gate across the whole corpus.
    for (resp, label) in &corpus {
        let b = before.should_compress(resp);
        let a = after.should_compress(resp);
        assert_eq!(b, a, "verdict differs for {label}");
    }
    println!(
        "# [2] gate: predicate verdicts identical across {} response shapes — OK",
        corpus.len()
    );

    // Hot case: the compressible JSON response (worst case for the chain —
    // every node runs).
    let hot = mk(Some("application/json"), 50_000, false);
    let m_before = measure(iters, || {
        black_box(before.should_compress(black_box(&hot)));
    });
    let m_after = measure(iters, || {
        black_box(after.should_compress(black_box(&hot)));
    });
    // Excluded case (early-exit for the chain on node 3): jpeg.
    let jpeg = mk(Some("image/jpeg"), 500_000, false);
    let m_before_x = measure(iters, || {
        black_box(before.should_compress(black_box(&jpeg)));
    });
    let m_after_x = measure(iters, || {
        black_box(after.should_compress(black_box(&jpeg)));
    });

    println!("\n## [2] Compression predicate — VERDICT: REJECTED (kept as evidence)");
    println!("| arm | ns/op | allocs/op |");
    print_row("BEFORE chain, compressible JSON", &m_before);
    print_row("AFTER  single-pass, same", &m_after);
    print_row("BEFORE chain, excluded jpeg", &m_before_x);
    print_row("AFTER  single-pass, same", &m_after_x);
    println!(
        "# compressible {:.2}x, excluded {:.2}x",
        m_before.wall_ns_per_op / m_after.wall_ns_per_op,
        m_before_x.wall_ns_per_op / m_after_x.wall_ns_per_op
    );
    // REJECTED (round 12): the monomorphized `And` chain compiles to
    // straight-line inlined header probes — ~4.6 ns TOTAL for the whole
    // 28-node walk, zero allocs. The "28 redundant Content-Type reads"
    // hypothesis was wrong at the machine level; a hand-fused single-pass
    // node measures within noise (±10%) and is sometimes slower on the
    // compressible case. Production keeps the declarative chain — it costs
    // nothing and reads better. This section stays as the reproducible
    // evidence for that rejection (the bench_favorites_authz pattern).
    println!("# not shipped: chain is already ~free; fused node within noise");
}

// ────────────────────────────────────────────────────────────────────────────
// [3] Security-header stack — 5 layers vs 1 fused middleware
// ────────────────────────────────────────────────────────────────────────────

mod headers_bench {
    use axum::Router;
    use axum::http::HeaderValue;
    use axum::http::header::HeaderName;
    use axum::routing::get;
    use tower_http::set_header::SetResponseHeaderLayer;

    const CSP: &str = "default-src 'self'; \
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
                     form-action 'self' https:";

    async fn csp_only(
        req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        let mut res = next.run(req).await;
        if res.status() == axum::http::StatusCode::NOT_MODIFIED {
            return res;
        }
        let is_html = res
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html"));
        if is_html {
            res.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
        } else {
            res.headers_mut().insert(
                axum::http::header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(CSP),
            );
        }
        res
    }

    /// AFTER: the four static headers folded into the same response pass.
    /// NOTE: applied BEFORE the 304 early-return — the standalone
    /// `SetResponseHeaderLayer`s stamp 304s too, and byte-identity with
    /// the BEFORE stack (including on 304s) is gated below.
    async fn fused(
        req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        let mut res = next.run(req).await;
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
        if res.status() == axum::http::StatusCode::NOT_MODIFIED {
            return res;
        }
        let is_html = res
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html"));
        if is_html {
            res.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
        } else {
            res.headers_mut().insert(
                axum::http::header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(CSP),
            );
        }
        res
    }

    async fn json_handler() -> ([(HeaderName, &'static str); 1], &'static str) {
        (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            "{\"ok\":true}",
        )
    }
    async fn html_handler() -> ([(HeaderName, &'static str); 1], &'static str) {
        (
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<html></html>",
        )
    }
    async fn not_modified() -> axum::http::StatusCode {
        axum::http::StatusCode::NOT_MODIFIED
    }

    fn routes() -> Router {
        Router::new()
            .route("/json", get(json_handler))
            .route("/html", get(html_handler))
            .route("/304", get(not_modified))
    }

    /// BEFORE, verbatim `main.rs` stack: CSP middleware + 4 header layers.
    pub fn before_app() -> Router {
        routes()
            .layer(axum::middleware::from_fn(csp_only))
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
            ))
    }

    pub fn after_app() -> Router {
        routes().layer(axum::middleware::from_fn(fused))
    }
}

fn section_headers() {
    use tower::ServiceExt;

    let iters: usize = env_or("BENCH_ITERS", 100_000) / 10;
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("rt");

    let call =
        |app: &axum::Router, path: &str| -> (axum::http::StatusCode, Vec<(String, String)>) {
            let app = app.clone();
            rt.block_on(async move {
                let res = app
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(path)
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = res.status();
                let mut headers: Vec<(String, String)> = res
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_string(),
                            String::from_utf8_lossy(v.as_bytes()).to_string(),
                        )
                    })
                    .collect();
                headers.sort();
                (status, headers)
            })
        };

    let before_app = headers_bench::before_app();
    let after_app = headers_bench::after_app();

    // Byte-identity gate on all three response classes (incl. the 304).
    for path in ["/json", "/html", "/304"] {
        let b = call(&before_app, path);
        let a = call(&after_app, path);
        assert_eq!(b, a, "headers differ for {path}");
    }
    println!("# [3] gate: status + full sorted header set identical (json/html/304) — OK");

    let m_before = measure(iters, || {
        black_box(call(&before_app, "/json"));
    });
    let m_after = measure(iters, || {
        black_box(call(&after_app, "/json"));
    });

    println!("\n## [3] Security-header stack (per request, incl. router overhead)");
    println!("| arm | ns/op | allocs/op |");
    print_row("BEFORE 5 layers (CSP + 4 set-header)", &m_before);
    print_row("AFTER  1 fused middleware", &m_after);
    println!(
        "# {:.2}x wall, {:.1} fewer allocs/request",
        m_before.wall_ns_per_op / m_after.wall_ns_per_op,
        m_before.allocs_per_op - m_after.allocs_per_op
    );
    if m_after.wall_ns_per_op >= m_before.wall_ns_per_op {
        eprintln!("GATE FAIL [3]: fused middleware not faster — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [4] Media capture-metadata single-read
// ────────────────────────────────────────────────────────────────────────────

mod media_bench {
    use super::*;
    use nom_exif::{EntryValue, ExifTag, MediaParser, MediaSource, TrackInfoTag};
    use oxicloud::infrastructure::services::exif_service::{ExifMetadata, ExifService};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static OPENS: AtomicU64 = AtomicU64::new(0);

    /// Minimal EXIF APP1 with IFD0 { Orientation, ExifIFD ptr } and
    /// ExifIFD { DateTimeOriginal } spliced after the JPEG SOI —
    /// the `bench_support::inject_exif_orientation` technique extended
    /// with a capture date.
    pub fn inject_exif_with_date(jpeg: &[u8], orientation: u16, date: Option<&str>) -> Vec<u8> {
        assert!(
            jpeg.len() >= 2 && jpeg[0] == 0xFF && jpeg[1] == 0xD8,
            "not a JPEG"
        );

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&0x2Au16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset

        match date {
            None => {
                // Orientation only (the date-less arm).
                tiff.extend_from_slice(&1u16.to_le_bytes());
                tiff.extend_from_slice(&0x0112u16.to_le_bytes());
                tiff.extend_from_slice(&3u16.to_le_bytes());
                tiff.extend_from_slice(&1u32.to_le_bytes());
                tiff.extend_from_slice(&(orientation as u32).to_le_bytes());
                tiff.extend_from_slice(&0u32.to_le_bytes());
            }
            Some(dt) => {
                assert_eq!(dt.len(), 19, "EXIF datetime must be 19 chars");
                // IFD0: 2 entries (Orientation, ExifIFD pointer).
                // IFD0 @8, size = 2 + 2*12 + 4 = 30 → ExifIFD @38.
                // ExifIFD: 1 entry (DateTimeOriginal), size = 2+12+4 = 18
                // → date bytes @56, 20 bytes (19 + NUL).
                tiff.extend_from_slice(&2u16.to_le_bytes());
                tiff.extend_from_slice(&0x0112u16.to_le_bytes());
                tiff.extend_from_slice(&3u16.to_le_bytes());
                tiff.extend_from_slice(&1u32.to_le_bytes());
                tiff.extend_from_slice(&(orientation as u32).to_le_bytes());
                tiff.extend_from_slice(&0x8769u16.to_le_bytes()); // ExifIFD ptr
                tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
                tiff.extend_from_slice(&1u32.to_le_bytes());
                tiff.extend_from_slice(&38u32.to_le_bytes());
                tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD

                tiff.extend_from_slice(&1u16.to_le_bytes()); // ExifIFD entries
                tiff.extend_from_slice(&0x9003u16.to_le_bytes()); // DateTimeOriginal
                tiff.extend_from_slice(&2u16.to_le_bytes()); // ASCII
                tiff.extend_from_slice(&20u32.to_le_bytes());
                tiff.extend_from_slice(&56u32.to_le_bytes());
                tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD

                tiff.extend_from_slice(dt.as_bytes());
                tiff.push(0);
            }
        }

        let mut payload = Vec::with_capacity(6 + tiff.len());
        payload.extend_from_slice(b"Exif\0\0");
        payload.extend_from_slice(&tiff);
        let seg_len = u16::try_from(2 + payload.len()).expect("segment size");

        let mut out = Vec::with_capacity(jpeg.len() + 4 + payload.len());
        out.extend_from_slice(&jpeg[0..2]);
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&seg_len.to_be_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// Minimal ISO-BMFF: ftyp(isom) + moov(mvhd v0 with a creation time).
    pub fn craft_minimal_mp4(creation: DateTime<Utc>) -> Vec<u8> {
        let epoch_1904 = Utc.with_ymd_and_hms(1904, 1, 1, 0, 0, 0).unwrap();
        let secs = (creation - epoch_1904).num_seconds() as u32;

        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&[0, 0, 0, 0]); // version 0 + flags
        mvhd.extend_from_slice(&secs.to_be_bytes()); // creation_time
        mvhd.extend_from_slice(&secs.to_be_bytes()); // modification_time
        mvhd.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        mvhd.extend_from_slice(&60_000u32.to_be_bytes()); // duration
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        mvhd.extend_from_slice(&[0u8; 10]); // reserved
        // identity matrix
        for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            mvhd.extend_from_slice(&v.to_be_bytes());
        }
        mvhd.extend_from_slice(&[0u8; 24]); // pre_defined
        mvhd.extend_from_slice(&2u32.to_be_bytes()); // next_track_ID

        let boxed = |name: &[u8; 4], body: &[u8]| -> Vec<u8> {
            let mut b = Vec::with_capacity(8 + body.len());
            b.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
            b.extend_from_slice(name);
            b.extend_from_slice(body);
            b
        };

        let mvhd_box = boxed(b"mvhd", &mvhd);
        let moov = boxed(b"moov", &mvhd_box);
        let ftyp = boxed(b"ftyp", b"isom\x00\x00\x02\x00isomiso2mp41");
        let mdat = boxed(b"mdat", &[0u8; 1024]);

        let mut out = Vec::new();
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&moov);
        out.extend_from_slice(&mdat);
        out
    }

    /// Mirror of `media_metadata_service`'s private `NomExif` accumulator.
    #[derive(Debug, Default, PartialEq)]
    pub struct NomLite {
        pub captured_at: Option<DateTime<Utc>>,
        pub latitude: Option<f64>,
        pub longitude: Option<f64>,
    }

    fn to_utc(ev: &EntryValue) -> Option<DateTime<Utc>> {
        let edt = ev.as_datetime()?;
        let utc0 = FixedOffset::east_opt(0)?;
        Some(edt.or_offset(utc0).with_timezone(&Utc))
    }

    fn nom_from_exif(exif: &nom_exif::Exif, out: &mut NomLite) {
        out.captured_at = exif
            .get(ExifTag::DateTimeOriginal)
            .and_then(to_utc)
            .or_else(|| exif.get(ExifTag::CreateDate).and_then(to_utc));
        if let Some(gps) = exif.gps_info() {
            out.latitude = gps.latitude_decimal();
            out.longitude = gps.longitude_decimal();
        }
    }

    /// BEFORE, verbatim `read_nom_exif`: `read_exif(path)` (open #1) then
    /// the `read_track(path)` fallback (open #2). The two top-level nom-exif
    /// fns are replicated inline (open → seekable → fresh parser) so the
    /// bench can count opens; this is exactly their lib.rs body.
    pub fn before_read_nom(path: &Path) -> NomLite {
        let mut out = NomLite::default();

        OPENS.fetch_add(1, Ordering::Relaxed);
        if let Ok(file) = std::fs::File::open(path)
            && let Ok(ms) = MediaSource::seekable(file)
            && let Ok(iter) = MediaParser::new().parse_exif(ms)
        {
            let exif: nom_exif::Exif = iter.into();
            nom_from_exif(&exif, &mut out);
        }

        if out.captured_at.is_none() {
            OPENS.fetch_add(1, Ordering::Relaxed);
            if let Ok(file) = std::fs::File::open(path)
                && let Ok(ms) = MediaSource::seekable(file)
                && let Ok(track) = MediaParser::new().parse_track(ms)
                && let Some(dt) = track.get(TrackInfoTag::CreateDate).and_then(to_utc)
            {
                out.captured_at = Some(dt);
            }
        }
        out
    }

    /// BEFORE, verbatim `extract_blocking` image arm: whole-file read for
    /// kamadak (open #0) + `read_nom_exif` (opens #1/#2).
    pub fn before_image(path: &Path) -> (Option<ExifMetadata>, NomLite) {
        OPENS.fetch_add(1, Ordering::Relaxed);
        let kamadak = std::fs::read(path)
            .ok()
            .and_then(|b| ExifService::extract(&b));
        let nom = before_read_nom(path);
        (kamadak, nom)
    }

    pub fn before_video(path: &Path) -> NomLite {
        before_read_nom(path)
    }

    /// AFTER: nom-exif fed from the already-read bytes (zero-copy), one
    /// reused parser, memory-mode track fallback (covers MIME-mislabel).
    pub fn after_nom_from_bytes(parser: &mut MediaParser, bytes: &Bytes) -> NomLite {
        let mut out = NomLite::default();
        if let Ok(ms) = MediaSource::from_memory(bytes.clone())
            && let Ok(iter) = parser.parse_exif(ms)
        {
            let exif: nom_exif::Exif = iter.into();
            nom_from_exif(&exif, &mut out);
        }
        if out.captured_at.is_none()
            && let Ok(ms) = MediaSource::from_memory(bytes.clone())
            && let Ok(track) = parser.parse_track(ms)
            && let Some(dt) = track.get(TrackInfoTag::CreateDate).and_then(to_utc)
        {
            out.captured_at = Some(dt);
        }
        out
    }

    pub fn after_image(path: &Path) -> (Option<ExifMetadata>, NomLite) {
        OPENS.fetch_add(1, Ordering::Relaxed);
        let Ok(buf) = std::fs::read(path) else {
            return (None, NomLite::default());
        };
        let kamadak = ExifService::extract(&buf);
        let bytes = Bytes::from(buf);
        let mut parser = MediaParser::new();
        let nom = after_nom_from_bytes(&mut parser, &bytes);
        (kamadak, nom)
    }

    /// AFTER video arm: ONE open, kind-dispatched.
    pub fn after_video(path: &Path) -> NomLite {
        let mut out = NomLite::default();
        OPENS.fetch_add(1, Ordering::Relaxed);
        let Ok(file) = std::fs::File::open(path) else {
            return out;
        };
        let Ok(ms) = MediaSource::seekable(file) else {
            return out;
        };
        let mut parser = MediaParser::new();
        match ms.kind() {
            nom_exif::MediaKind::Image => {
                if let Ok(iter) = parser.parse_exif(ms) {
                    let exif: nom_exif::Exif = iter.into();
                    nom_from_exif(&exif, &mut out);
                }
            }
            nom_exif::MediaKind::Track => {
                if let Ok(track) = parser.parse_track(ms)
                    && let Some(dt) = track.get(TrackInfoTag::CreateDate).and_then(to_utc)
                {
                    out.captured_at = Some(dt);
                }
            }
        }
        out
    }
}

fn section_media() {
    use media_bench::*;

    let iters: usize = env_or("BENCH_MEDIA_ITERS", 300);
    let cold_iters: usize = env_or("BENCH_COLD_ITERS", 20);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/bench-media");
    std::fs::create_dir_all(&dir).expect("mkdir");

    // Corpus: a ~1.5 MB JPEG with an EXIF date, the same without a date
    // (exercises the track-fallback re-open), a PNG (no EXIF at all), and
    // a minimal MP4.
    let img = image::RgbImage::from_fn(2000, 1500, |x, y| {
        image::Rgb([
            ((x * 7 + y * 3) % 251) as u8,
            ((x * 13 + y * 5) % 241) as u8,
            ((x * 3 + y * 11) % 239) as u8,
        ])
    });
    let mut jpeg_plain = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_plain, 90)
        .encode_image(&image::DynamicImage::ImageRgb8(img.clone()))
        .expect("jpeg");
    let jpeg_dated = inject_exif_with_date(&jpeg_plain, 6, Some("2024:06:01 12:00:00"));
    let jpeg_undated = inject_exif_with_date(&jpeg_plain, 6, None);
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(800, 600, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    }))
    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    .expect("png");
    let mp4 = craft_minimal_mp4(Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap());

    let cases: Vec<(&str, PathBuf, bool)> = vec![
        ("jpeg_dated", dir.join("dated.jpg"), true),
        ("jpeg_undated", dir.join("undated.jpg"), true),
        ("png_noexif", dir.join("plain.png"), true),
        ("mp4_video", dir.join("clip.mp4"), false),
    ];
    std::fs::write(&cases[0].1, &jpeg_dated).unwrap();
    std::fs::write(&cases[1].1, &jpeg_undated).unwrap();
    std::fs::write(&cases[2].1, &png).unwrap();
    std::fs::write(&cases[3].1, &mp4).unwrap();

    // Equivalence gates: identical extraction output per corpus file, and
    // the dated JPEG / MP4 must actually yield the crafted timestamp (so
    // the corpus is known-good, not vacuously equal).
    for (name, path, is_image) in &cases {
        if *is_image {
            let (bk, bn) = before_image(path);
            let (ak, an) = after_image(path);
            assert_eq!(
                format!("{bk:?}"),
                format!("{ak:?}"),
                "kamadak differs for {name}"
            );
            assert_eq!(bn, an, "nom-exif differs for {name}");
        } else {
            let b = before_video(path);
            let a = after_video(path);
            assert_eq!(b, a, "video extraction differs for {name}");
            assert!(
                b.captured_at.is_some(),
                "crafted MP4 must yield a creation date"
            );
        }
    }
    let (_, dated_nom) = before_image(&cases[0].1);
    assert!(
        dated_nom.captured_at.is_some(),
        "dated JPEG must yield a date"
    );
    println!("# [4] gate: BEFORE/AFTER extraction identical across 4 corpus files — OK");

    println!("\n## [4] Media capture-metadata extraction (warm page cache)");
    println!("| case / arm | ns/op | allocs/op |  opens/op |");
    let mut total_speedup = 1.0f64;
    for (name, path, is_image) in &cases {
        let o0 = OPENS.load(Ordering::Relaxed);
        let m_before = measure(iters, || {
            if *is_image {
                black_box(before_image(path));
            } else {
                black_box(before_video(path));
            }
        });
        let before_opens = (OPENS.load(Ordering::Relaxed) - o0) as f64 / iters as f64;
        let o1 = OPENS.load(Ordering::Relaxed);
        let m_after = measure(iters, || {
            if *is_image {
                black_box(after_image(path));
            } else {
                black_box(after_video(path));
            }
        });
        let after_opens = (OPENS.load(Ordering::Relaxed) - o1) as f64 / iters as f64;
        println!(
            "| BEFORE {name:<30} | {:>12.1} | {:>10.2} | {:>9.2} |",
            m_before.wall_ns_per_op, m_before.allocs_per_op, before_opens
        );
        println!(
            "| AFTER  {name:<30} | {:>12.1} | {:>10.2} | {:>9.2} |",
            m_after.wall_ns_per_op, m_after.allocs_per_op, after_opens
        );
        total_speedup *= m_before.wall_ns_per_op / m_after.wall_ns_per_op;
        if m_after.wall_ns_per_op >= m_before.wall_ns_per_op * 1.02 {
            eprintln!("GATE FAIL [4]: AFTER slower for {name} — rollback");
            std::process::exit(1);
        }
    }
    println!(
        "# geomean speedup {:.2}x across the corpus",
        total_speedup.powf(0.25)
    );

    // Cold-cache arms (root only): true disk-I/O shape of the extra opens.
    if cold_iters > 0 && std::fs::write("/proc/sys/vm/drop_caches", "3").is_ok() {
        println!("\n## [4b] Cold page cache (drop_caches between passes)");
        println!("| case | BEFORE ms/op | AFTER ms/op |");
        for (name, path, is_image) in &cases {
            let mut b_ms = 0.0;
            let mut a_ms = 0.0;
            for _ in 0..cold_iters {
                std::fs::write("/proc/sys/vm/drop_caches", "3").ok();
                let t = Instant::now();
                if *is_image {
                    black_box(before_image(path));
                } else {
                    black_box(before_video(path));
                }
                b_ms += t.elapsed().as_secs_f64() * 1e3;
                std::fs::write("/proc/sys/vm/drop_caches", "3").ok();
                let t = Instant::now();
                if *is_image {
                    black_box(after_image(path));
                } else {
                    black_box(after_video(path));
                }
                a_ms += t.elapsed().as_secs_f64() * 1e3;
            }
            println!(
                "| {name:<14} | {:>12.3} | {:>11.3} |",
                b_ms / cold_iters as f64,
                a_ms / cold_iters as f64
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [5] Chunked-upload session map — fused owner-check lookups
// ────────────────────────────────────────────────────────────────────────────

mod session_bench {
    use super::*;

    pub struct FakeSession {
        pub user_id: String,
        pub chunk_sizes: Vec<usize>,
        pub temp_dir: PathBuf,
        pub bytes_received: u64,
    }

    pub type Sessions = DashMap<String, FakeSession>;

    /// BEFORE, verbatim shapes: `verify_session_owner` (get #1 +
    /// `user_id.to_string()`) then the operation's own get / get_mut.
    fn verify_owner(sessions: &Sessions, upload_id: &str, user_id: &str) -> Result<(), ()> {
        let session = sessions.get(upload_id).ok_or(())?;
        if session.user_id != user_id {
            return Err(());
        }
        Ok(())
    }

    pub fn before_prepare(
        sessions: &Sessions,
        upload_id: &str,
        user_id: Uuid,
        chunk_index: usize,
    ) -> Result<(PathBuf, usize), ()> {
        verify_owner(sessions, upload_id, &user_id.to_string())?;
        let session = sessions.get(upload_id).ok_or(())?;
        if chunk_index >= session.chunk_sizes.len() {
            return Err(());
        }
        Ok((
            session.temp_dir.join(format!("chunk_{:06}", chunk_index)),
            session.chunk_sizes[chunk_index],
        ))
    }

    pub fn before_commit(
        sessions: &Sessions,
        upload_id: &str,
        user_id: Uuid,
        chunk_index: usize,
        actual_size: u64,
    ) -> Result<u64, ()> {
        verify_owner(sessions, upload_id, &user_id.to_string())?;
        let (_chunk_path, _expected) = {
            let session = sessions.get(upload_id).ok_or(())?;
            if chunk_index >= session.chunk_sizes.len() {
                return Err(());
            }
            (
                session.temp_dir.join(format!("chunk_{:06}", chunk_index)),
                session.chunk_sizes[chunk_index],
            )
        };
        let bytes = {
            let mut session = sessions.get_mut(upload_id).ok_or(())?;
            session.bytes_received += actual_size;
            session.bytes_received
        };
        Ok(bytes)
    }

    /// AFTER: the owner check folded into the operation's own lookup, uuid
    /// compared via a stack-encoded hyphenated form (no `to_string`).
    #[inline]
    fn owner_matches(session_user: &str, user_id: Uuid) -> bool {
        let mut buf = [0u8; 36];
        session_user == user_id.hyphenated().encode_lower(&mut buf) as &str
    }

    pub fn after_prepare(
        sessions: &Sessions,
        upload_id: &str,
        user_id: Uuid,
        chunk_index: usize,
    ) -> Result<(PathBuf, usize), ()> {
        let session = sessions.get(upload_id).ok_or(())?;
        if !owner_matches(&session.user_id, user_id) {
            return Err(());
        }
        if chunk_index >= session.chunk_sizes.len() {
            return Err(());
        }
        Ok((
            session.temp_dir.join(format!("chunk_{:06}", chunk_index)),
            session.chunk_sizes[chunk_index],
        ))
    }

    pub fn after_commit(
        sessions: &Sessions,
        upload_id: &str,
        user_id: Uuid,
        chunk_index: usize,
        actual_size: u64,
    ) -> Result<u64, ()> {
        let (_chunk_path, _expected) = {
            let session = sessions.get(upload_id).ok_or(())?;
            if !owner_matches(&session.user_id, user_id) {
                return Err(());
            }
            if chunk_index >= session.chunk_sizes.len() {
                return Err(());
            }
            (
                session.temp_dir.join(format!("chunk_{:06}", chunk_index)),
                session.chunk_sizes[chunk_index],
            )
        };
        let bytes = {
            let mut session = sessions.get_mut(upload_id).ok_or(())?;
            session.bytes_received += actual_size;
            session.bytes_received
        };
        Ok(bytes)
    }
}

fn section_sessions() {
    use session_bench::*;

    let iters: usize = env_or("BENCH_ITERS", 100_000);
    let sessions: Sessions = DashMap::new();
    let owner = Uuid::new_v4();
    let intruder = Uuid::new_v4();
    let upload_id = Uuid::new_v4().to_string();
    sessions.insert(
        upload_id.clone(),
        FakeSession {
            user_id: owner.to_string(),
            chunk_sizes: vec![5 * 1024 * 1024; 200],
            temp_dir: PathBuf::from("/tmp/oxi-chunk-bench"),
            bytes_received: 0,
        },
    );

    // Equivalence gates: same accept/reject on owner, intruder, unknown
    // session, out-of-range index; same returned values.
    let b_ok = before_prepare(&sessions, &upload_id, owner, 3);
    let a_ok = after_prepare(&sessions, &upload_id, owner, 3);
    assert_eq!(b_ok, a_ok);
    assert!(b_ok.is_ok());
    assert_eq!(
        before_prepare(&sessions, &upload_id, intruder, 3),
        after_prepare(&sessions, &upload_id, intruder, 3)
    );
    assert!(after_prepare(&sessions, &upload_id, intruder, 3).is_err());
    assert_eq!(
        before_prepare(&sessions, "nope", owner, 0),
        after_prepare(&sessions, "nope", owner, 0)
    );
    assert_eq!(
        before_prepare(&sessions, &upload_id, owner, 9999),
        after_prepare(&sessions, &upload_id, owner, 9999)
    );
    {
        let b = before_commit(&sessions, &upload_id, owner, 3, 100);
        let a = after_commit(&sessions, &upload_id, owner, 3, 100);
        assert!(b.is_ok() && a.is_ok());
        assert_eq!(a.unwrap(), b.unwrap() + 100, "cumulative counter advances");
        sessions.get_mut(&upload_id).unwrap().bytes_received = 0;
    }
    println!(
        "# [5] gate: identical accept/reject + values across owner/intruder/unknown/range — OK"
    );

    let m_before = measure(iters, || {
        black_box(before_prepare(&sessions, &upload_id, owner, 3).ok());
        black_box(before_commit(&sessions, &upload_id, owner, 3, 5 * 1024 * 1024).ok());
    });
    sessions.get_mut(&upload_id).unwrap().bytes_received = 0;
    let m_after = measure(iters, || {
        black_box(after_prepare(&sessions, &upload_id, owner, 3).ok());
        black_box(after_commit(&sessions, &upload_id, owner, 3, 5 * 1024 * 1024).ok());
    });

    println!("\n## [5] Chunked-upload session ops (prepare + commit per chunk)");
    println!("| arm | ns/op | allocs/op |");
    print_row("BEFORE 5 lookups + 2 to_string", &m_before);
    print_row("AFTER  3 lookups + stack encode", &m_after);
    println!(
        "# {:.2}x wall, {:.1} fewer allocs/chunk",
        m_before.wall_ns_per_op / m_after.wall_ns_per_op,
        m_before.allocs_per_op - m_after.allocs_per_op
    );
    if m_after.wall_ns_per_op >= m_before.wall_ns_per_op {
        eprintln!("GATE FAIL [5]: fused lookups not faster — rollback");
        std::process::exit(1);
    }
}

fn main() {
    println!("#################################################################");
    println!("# Round-12 CPU/alloc micro-pack");
    println!("#################################################################\n");

    section_sized_json();
    section_predicate();
    section_headers();
    section_media();
    section_sessions();

    println!("\nGATE PASS (all sections)");
}
