//! Round-14 CPU/alloc micro-pack (no Postgres).
//!
//! Each section is BEFORE (verbatim replica of the shipped shape, or the
//! shipped function itself) vs AFTER (proposed shape), with a byte-identity /
//! equivalence gate and a `GATE FAIL … rollback` check that exits non-zero if
//! the AFTER arm fails to beat its BEFORE — the round's roll-back rule encoded
//! into the benchmark.
//!
//!   [A1] Cookie auth extract — `extract_cookie_value` (owned `String`, only
//!        reborrowed as `&str` into `validate_token`) vs the borrow-only
//!        `extract_cookie_str` that already backs the CSRF middleware.
//!   [A2] Search `compute_relevance` — `name.to_lowercase()` per result row
//!        vs an ASCII case-fold fast path (Unicode fallback preserved).
//!   [A3] Auth middleware `sub` → `Uuid` — re-parsed from the 36-char claim on
//!        every authenticated request vs a pre-parsed `Uuid` (Copy) carried on
//!        the cached claims.
//!   [A4] Auth middleware `HeaderMap` clone — the `headers: HeaderMap`
//!        extractor duplicates the whole map per request though every use is a
//!        read `request.headers()` already exposes.
//!   [A5] CalDAV getlastmodified — `updated_at.to_rfc2822()` (heap `String`
//!        per event) vs the stack `common::fmt::rfc2822_utc` the CardDAV
//!        emitter already uses.
//!   [A6] CalDAV per-event href + quoted etag — a fresh `format!` `String`
//!        pair per event vs a reused page buffer (`clear()` + `write!`), the
//!        shape the CardDAV report emitter already ships.
//!
//! Run:
//!   cargo bench --features=bench --bench bench_round14_micro
//! Tunables (env): BENCH_ITERS (200000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fmt::Write as _;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::http::{HeaderMap, HeaderValue, header};
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
        "| {:<40} | {:>12.1} | {:>10.2} |",
        label, m.wall_ns_per_op, m.allocs_per_op
    );
}

fn header_footer(name: &str, before: &Measured, after: &Measured) {
    println!("| arm | ns/op | allocs/op |");
    print_row(&format!("BEFORE {name}"), before);
    print_row(&format!("AFTER  {name}"), after);
    println!(
        "# {:.2}x wall, {:.2} fewer allocs/op",
        before.wall_ns_per_op / after.wall_ns_per_op,
        before.allocs_per_op - after.allocs_per_op
    );
}

// ────────────────────────────────────────────────────────────────────────────
// [A1] Cookie auth extract — owned String vs borrow-only &str
// ────────────────────────────────────────────────────────────────────────────

fn section_cookie() {
    use oxicloud::interfaces::api::cookie_auth::{extract_cookie_str, extract_cookie_value};

    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let name = "oxicloud_access";
    let jwt =
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIwMTIzNDU2Nzg5YWJjZGVmIn0.c2lnbmF0dXJlLXBsYWNlaG9sZGVy";
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!(
            "{name}={jwt}; oxicloud_csrf=3f2504e0-4f89-41d3-9a0c-0305e82c3301"
        ))
        .unwrap(),
    );

    // Gate: byte-identical value.
    let owned = extract_cookie_value(&headers, name);
    let borrowed = extract_cookie_str(&headers, name);
    assert_eq!(owned.as_deref(), borrowed, "cookie value differs");
    assert_eq!(borrowed, Some(jwt), "unexpected cookie value");
    println!("# [A1] gate: borrow-only extract byte-identical to owned — OK");

    let m_before = measure(iters, || {
        let v = extract_cookie_value(black_box(&headers), name);
        black_box(v);
    });
    let m_after = measure(iters, || {
        let v = extract_cookie_str(black_box(&headers), name);
        black_box(v);
    });

    println!("\n## [A1] Cookie access-token extract (per cookie-authed /api request)");
    header_footer("extract owned/borrow", &m_before, &m_after);
    if m_after.allocs_per_op >= m_before.allocs_per_op {
        eprintln!("GATE FAIL [A1]: borrow arm did not remove an allocation — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [A2] Search compute_relevance — Unicode lowercase vs ASCII fast path
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE — verbatim `search_service::compute_relevance`.
fn relevance_before(name: &str, query_lower: &str) -> u32 {
    let name_lower = name.to_lowercase();
    if name_lower == query_lower {
        100
    } else if name_lower.starts_with(query_lower) {
        80
    } else if name_lower.contains(query_lower) {
        let ratio = query_lower.len() as f64 / name_lower.len() as f64;
        50 + (ratio * 20.0) as u32
    } else {
        0
    }
}

#[inline]
fn ascii_ci_starts_with(h: &[u8], n: &[u8]) -> bool {
    h.len() >= n.len() && h[..n.len()].eq_ignore_ascii_case(n)
}
#[inline]
fn ascii_ci_contains(h: &[u8], n: &[u8]) -> bool {
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// AFTER — ASCII fast path (Unicode fallback preserves exact behavior).
fn relevance_after(name: &str, query_lower: &str) -> u32 {
    if name.is_ascii() {
        let nb = name.as_bytes();
        let qb = query_lower.as_bytes();
        if nb.eq_ignore_ascii_case(qb) {
            100
        } else if ascii_ci_starts_with(nb, qb) {
            80
        } else if ascii_ci_contains(nb, qb) {
            let ratio = query_lower.len() as f64 / name.len() as f64;
            50 + (ratio * 20.0) as u32
        } else {
            0
        }
    } else {
        relevance_before(name, query_lower)
    }
}

fn section_relevance() {
    let iters: usize = env_or("BENCH_ITERS", 200_000) / 4;

    // Mixed corpus: exact / prefix / substring / miss, ASCII and non-ASCII
    // names, ASCII and non-ASCII (already-lowercased) queries.
    let corpus: &[(&str, &str)] = &[
        ("Report.pdf", "report.pdf"),
        ("Report.pdf", "report"),
        ("Annual Report 2026.pdf", "report"),
        ("Vacation Photo.jpg", "xyz"),
        ("Hello World.txt", "world"),
        ("IMG_20260719_120000.HEIC", "img"),
        ("Résumé Final.pdf", "resume"),
        ("Café Menu.txt", "café"),
        ("STRASSE.txt", "straße"),
        ("naïve-approach.md", "naïve"),
        ("Notes.md", "note"),
        ("budget-Q3.xlsx", "q3"),
    ];

    // Gate: AFTER == BEFORE for every corpus entry.
    for (name, q) in corpus {
        assert_eq!(
            relevance_before(name, q),
            relevance_after(name, q),
            "relevance differs for ({name:?}, {q:?})"
        );
    }
    println!(
        "# [A2] gate: ASCII fast path matches Unicode lowercase across {} cases — OK",
        corpus.len()
    );

    let m_before = measure(iters, || {
        for (name, q) in corpus {
            black_box(relevance_before(black_box(name), black_box(q)));
        }
    });
    let m_after = measure(iters, || {
        for (name, q) in corpus {
            black_box(relevance_after(black_box(name), black_box(q)));
        }
    });

    println!(
        "\n## [A2] compute_relevance over a {}-row result page (per search / keystroke)",
        corpus.len()
    );
    header_footer("relevance whole corpus", &m_before, &m_after);
    if m_after.wall_ns_per_op >= m_before.wall_ns_per_op {
        eprintln!("GATE FAIL [A2]: ASCII fast path not faster — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [A3] Auth middleware sub → Uuid — re-parse per request vs pre-parsed Copy
// ────────────────────────────────────────────────────────────────────────────

fn section_sub_parse() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let sub = "0123abcd-4f89-41d3-9a0c-0305e82c3301".to_string();
    let pre_parsed = Uuid::parse_str(&sub).unwrap();

    // Gate: the pre-parsed uuid equals a fresh parse.
    assert_eq!(
        Uuid::parse_str(&sub).unwrap(),
        pre_parsed,
        "uuid parse differs"
    );
    println!("# [A3] gate: pre-parsed sub_id equals per-request parse — OK");

    let m_before = measure(iters, || {
        let u = Uuid::parse_str(black_box(&sub)).unwrap();
        black_box(u);
    });
    let m_after = measure(iters, || {
        let u = black_box(pre_parsed); // Copy of the pre-parsed Uuid
        black_box(u);
    });

    println!("\n## [A3] sub → Uuid on the authed request path (Bearer + cookie)");
    header_footer("sub parse/copy", &m_before, &m_after);
    if m_after.wall_ns_per_op >= m_before.wall_ns_per_op {
        eprintln!("GATE FAIL [A3]: pre-parsed copy not faster — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [A4] Auth middleware HeaderMap clone — clone-the-map vs borrow + get
// ────────────────────────────────────────────────────────────────────────────

fn build_request_headers() -> HeaderMap {
    // A representative authed browser request.
    let mut h = HeaderMap::new();
    h.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig"),
    );
    h.insert(
        header::COOKIE,
        HeaderValue::from_static(
            "oxicloud_access=eyJ.payload.sig; oxicloud_csrf=3f2504e0-4f89-41d3-9a0c-0305e82c3301",
        ),
    );
    h.insert(header::HOST, HeaderValue::from_static("cloud.example.com"));
    h.insert(
        header::USER_AGENT,
        HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36"),
    );
    h.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    h.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br"),
    );
    h.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9"),
    );
    h.insert(
        header::REFERER,
        HeaderValue::from_static("https://cloud.example.com/files"),
    );
    h.insert(
        "x-csrf-token",
        HeaderValue::from_static("3f2504e0-4f89-41d3-9a0c-0305e82c3301"),
    );
    h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    h
}

fn section_headermap_clone() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let headers = build_request_headers();

    // Gate: the token extracted from a cloned map equals that from the borrowed map.
    let from_clone = {
        let c = headers.clone();
        c.get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let from_borrow = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    assert_eq!(from_clone.as_deref(), from_borrow, "authorization differs");
    println!("# [A4] gate: token from cloned map == token from borrowed map — OK");

    let m_before = measure(iters, || {
        // BEFORE: the `headers: HeaderMap` extractor clones the whole map,
        // then the middleware only reads from it.
        let cloned = black_box(&headers).clone();
        let tok = cloned.get(header::AUTHORIZATION);
        black_box(tok);
        black_box(cloned);
    });
    let m_after = measure(iters, || {
        // AFTER: read straight from the borrowed request headers.
        let tok = black_box(&headers).get(header::AUTHORIZATION);
        black_box(tok);
    });

    println!("\n## [A4] Auth middleware HeaderMap (per authed /api + DAV + NC request)");
    header_footer("headers clone/borrow", &m_before, &m_after);
    if m_after.allocs_per_op >= m_before.allocs_per_op {
        eprintln!("GATE FAIL [A4]: borrow arm did not remove allocations — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [A5] CalDAV getlastmodified — chrono to_rfc2822 String vs stack rfc2822_utc
// ────────────────────────────────────────────────────────────────────────────

fn section_caldav_rfc2822() {
    use chrono::{DateTime, Utc};
    use oxicloud::common::fmt::rfc2822_utc;

    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // A spread of realistic event updated_at timestamps.
    let secs: &[i64] = &[
        1_752_752_834, // 2025-07-17 …
        0,             // Thu, 1 Jan 1970 (day not zero-padded — the parity edge)
        1_600_000_000,
        1_262_304_000,
        253_402_300_799, // 9999-12-31 23:59:59 (max 4-digit year)
    ];

    // Gate: rfc2822_utc byte-identical to chrono to_rfc2822 for every sample.
    for &s in secs {
        let dt = DateTime::<Utc>::from_timestamp(s, 0).unwrap();
        let chrono_s = dt.to_rfc2822();
        let mut buf = [0u8; 31];
        let stack_s = rfc2822_utc(&mut buf, s).expect("in range");
        assert_eq!(chrono_s, stack_s, "rfc2822 differs for secs={s}");
    }
    println!("# [A5] gate: stack rfc2822_utc byte-identical to chrono to_rfc2822 — OK");

    let dts: Vec<DateTime<Utc>> = secs
        .iter()
        .map(|&s| DateTime::<Utc>::from_timestamp(s, 0).unwrap())
        .collect();

    let m_before = measure(iters, || {
        for dt in &dts {
            black_box(black_box(dt).to_rfc2822());
        }
    });
    let m_after = measure(iters, || {
        for &s in secs {
            let mut buf = [0u8; 31];
            black_box(rfc2822_utc(&mut buf, black_box(s)));
        }
    });

    println!(
        "\n## [A5] CalDAV getlastmodified render ({} events, per REPORT/PROPFIND)",
        secs.len()
    );
    header_footer("rfc2822 chrono/stack", &m_before, &m_after);
    if m_after.allocs_per_op >= m_before.allocs_per_op {
        eprintln!("GATE FAIL [A5]: stack render did not remove allocations — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [A6] CalDAV per-event href + quoted etag — fresh format! vs reused buffer
// ────────────────────────────────────────────────────────────────────────────

fn section_caldav_href_etag() {
    let iters: usize = env_or("BENCH_ITERS", 200_000) / 20;
    let base_href = "/caldav/alice/personal/";
    // A page of events (uid, id) like write_report_page iterates.
    let events: Vec<(String, Uuid)> = (0..40)
        .map(|i| {
            (
                format!("event-uid-{i:04}-abcdef@oxicloud"),
                Uuid::from_u128(0x1000 + i as u128),
            )
        })
        .collect();

    // Gate: reused-buffer output identical to the per-event format! pair.
    for (uid, id) in &events {
        let href_fmt = format!("{base_href}{uid}.ics");
        let etag_fmt = format!("\"{id}\"");
        let mut href_buf = String::new();
        let mut etag_buf = String::new();
        write!(href_buf, "{base_href}{uid}.ics").unwrap();
        write!(etag_buf, "\"{id}\"").unwrap();
        assert_eq!(href_fmt, href_buf, "href differs");
        assert_eq!(etag_fmt, etag_buf, "etag differs");
    }
    println!("# [A6] gate: reused-buffer href/etag identical to per-event format! — OK");

    let m_before = measure(iters, || {
        // BEFORE: two fresh String allocations per event.
        for (uid, id) in &events {
            let href = format!("{base_href}{uid}.ics");
            let etag = format!("\"{id}\"");
            black_box((href, etag));
        }
    });
    let m_after = measure(iters, || {
        // AFTER: one reusable href buffer + one etag buffer for the whole page.
        let mut href = String::new();
        let mut etag = String::new();
        for (uid, id) in &events {
            href.clear();
            etag.clear();
            let _ = write!(href, "{base_href}{uid}.ics");
            let _ = write!(etag, "\"{id}\"");
            black_box((&href, &etag));
        }
    });

    println!(
        "\n## [A6] CalDAV per-event href + etag ({} events/page, per REPORT/PROPFIND)",
        events.len()
    );
    header_footer("href+etag per page", &m_before, &m_after);
    if m_after.allocs_per_op >= m_before.allocs_per_op {
        eprintln!("GATE FAIL [A6]: reused buffer did not reduce allocations — rollback");
        std::process::exit(1);
    }
}

fn main() {
    println!("#################################################################");
    println!("# Round-14 CPU/alloc micro-pack");
    println!("#################################################################\n");

    section_cookie();
    section_relevance();
    section_sub_parse();
    section_headermap_clone();
    section_caldav_rfc2822();
    section_caldav_href_etag();

    println!("\nGATE PASS (all sections)");
}
