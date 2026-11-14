//! Round-22 CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–21: each section is BEFORE (verbatim replica of the
//! shipped-before shape) vs AFTER (verbatim replica of the shipped-after shape,
//! which the source is then made to match), with a byte/-value equivalence gate
//! and a `GATE FAIL … rollback` check that `std::process::exit(1)`s if the AFTER
//! arm fails to beat its BEFORE — the round's roll-back rule encoded into the
//! benchmark. An AFTER that doesn't win is never applied to the source.
//!
//!   [H1] Hot GET handlers (`get_thumbnail`, `download_file`, `list_files`,
//!        `list_photos`, NC `preview`, public-share download/access) took the
//!        axum `HeaderMap` extractor, which does `parts.headers.clone()` — an
//!        owned clone of the whole request header table (~2 allocs) — just to
//!        read 1–3 headers. AFTER takes `req: Request` and reads `req.headers()`
//!        by borrow (the ROUND14 §A4 middleware pattern applied to the handlers).
//!
//!   [W1] The native WebDAV `write_etag_quoted` (per file AND per folder of
//!        every `/webdav/` PROPFIND row, up to 500/page — the most-travelled
//!        DAV path) built a `"{etag}"` String then wrote it auto-escaped;
//!        `quick_xml` escapes the `"` → `&quot;`, re-allocating an owned `Cow`,
//!        so 2 allocs/row (buffer + escape). AFTER emits the quotes as borrowed
//!        pre-escaped `&quot;` events (the ROUND20 §C1 / ROUND21 §R4 pattern).
//!
//!   [C1] The CalDAV `getetag` emit (per event of every calendar REPORT/multiget,
//!        per calendar of the home-set PROPFIND) still escaped a `"…"` value
//!        (reused buffer → 1 alloc/event escape; `format!` calendar sites → 2).
//!        AFTER routes all five sites through a `write_quoted_etag` helper
//!        (borrowed pre-escaped quotes) — 0 allocs.
//!
//!   [D1] `FileDto::from` cloned `content_hash` via `file.content_hash()
//!        .to_string()` and then `into_parts()` MOVED the same `blob_hash` into
//!        `parts.blob_hash`, which was dropped unused — 1 wasted alloc on every
//!        listing row. AFTER reuses `parts.blob_hash` (0).
//!
//!   [E1] `CalendarEvent::update_time_range` / `update_all_day` stamped timed
//!        DTSTART/DTEND via `format!("{}", t.format("%Y%m%dT%H%M%SZ"))` — chrono's
//!        strftime interpreter (~3 allocs). AFTER stack-renders via the shipped
//!        `fmt::compact_ical_utc` and passes the `&str` straight to
//!        `update_ical_property` (0 allocs), chrono fallback out of range.
//!
//!   [S1] `ShareItemType::try_from` matched `s.to_lowercase().as_str()` — a
//!        throwaway Unicode-lowercased String — against two ASCII literals.
//!        AFTER uses `eq_ignore_ascii_case` (byte-identical acceptance, 0 allocs).
//!
//! Run:
//!   cargo bench --features=bench --bench bench_round22_micro
//! Tunables (env): BENCH_ITERS (200000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use chrono::{TimeZone, Utc};
use quick_xml::Writer;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};

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
    // Warm up (grow any reused buffers, prime caches) so the measured window
    // reflects steady state, not first-touch growth.
    for _ in 0..(iters / 20).max(1) {
        f();
    }
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
        "| {:<52} | {:>12.1} | {:>10.2} |",
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

fn gate_allocs(tag: &str, before: &Measured, after: &Measured) {
    if after.allocs_per_op >= before.allocs_per_op {
        eprintln!("GATE FAIL [{tag}]: AFTER did not reduce allocations — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [H1] Hot GET handler HeaderMap extractor — axum `HeaderMap` (clones the whole
//      request header table via `parts.headers.clone()`) vs `Request` + borrow.
// ────────────────────────────────────────────────────────────────────────────

/// A realistic browser GET request header set (what a thumbnail / photo-list /
/// download request actually carries). The `HeaderMap` extractor clones ALL of
/// it just so the handler can read 1–3 headers (IF_NONE_MATCH / ACCEPT / RANGE).
fn realistic_request_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::HOST, HeaderValue::from_static("cloud.example.com"));
    h.insert(
        header::USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/126.0 Safari/537.36",
        ),
    );
    h.insert(
        header::ACCEPT,
        HeaderValue::from_static("image/avif,image/webp,image/apng,image/*,*/*;q=0.8"),
    );
    h.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br, zstd"),
    );
    h.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9,es;q=0.8"),
    );
    h.insert(
        header::REFERER,
        HeaderValue::from_static("https://cloud.example.com/photos"),
    );
    h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    h.insert(
        HeaderName::from_static("sec-fetch-dest"),
        HeaderValue::from_static("image"),
    );
    h.insert(
        HeaderName::from_static("sec-fetch-mode"),
        HeaderValue::from_static("no-cors"),
    );
    h.insert(
        HeaderName::from_static("sec-fetch-site"),
        HeaderValue::from_static("same-origin"),
    );
    h.insert(
        header::COOKIE,
        HeaderValue::from_static(
            "oxicloud_session=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature; csrf=abc123",
        ),
    );
    h.insert(
        header::IF_NONE_MATCH,
        HeaderValue::from_static("\"thumb-6b1e9f00-preview-webp\""),
    );
    h
}

fn section_h1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let headers = realistic_request_headers();

    // Equivalence: both arms read the identical IF_NONE_MATCH value.
    let cloned = headers.clone();
    let borrowed = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    assert_eq!(
        cloned
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok()),
        borrowed,
        "H1 read value differs"
    );

    // BEFORE: axum's `HeaderMap` extractor materializes an owned clone of the
    // whole request header table (`parts.headers.clone()`), then the handler
    // reads one header out of it.
    let before = measure(iters, || {
        let owned = black_box(&headers).clone();
        black_box(
            owned
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok()),
        );
    });

    // AFTER: take `req: Request` and read `req.headers()` by borrow — the header
    // table is never cloned; the same value is read straight from the borrow.
    let after = measure(iters, || {
        let borrow: &HeaderMap = black_box(&headers);
        black_box(
            borrow
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok()),
        );
    });

    println!(
        "\n## [H1] Hot GET handler HeaderMap clone (per thumbnail/photo/download/preview/share req)"
    );
    header_footer("HeaderMap::clone() vs &HeaderMap borrow", &before, &after);
    gate_allocs("H1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [W1] Native WebDAV getetag — sized String + escape vs borrowed pre-escaped.
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE (verbatim `webdav_adapter::write_etag_quoted`): sized `"{etag}"`
/// String then auto-escaped write — `quick_xml` escapes the `"` → `&quot;`,
/// re-allocating an owned `Cow`. 2 allocs/row (buffer + escape).
fn w1_before(buf: &mut Vec<u8>, etag: &str) {
    let mut w = Writer::new(&mut *buf);
    w.write_event(Event::Start(BytesStart::new("D:getetag")))
        .unwrap();
    let mut quoted = String::with_capacity(etag.len() + 2);
    quoted.push('"');
    quoted.push_str(etag);
    quoted.push('"');
    w.write_event(Event::Text(BytesText::new(&quoted))).unwrap();
    w.write_event(Event::End(BytesEnd::new("D:getetag")))
        .unwrap();
}

/// AFTER: borrowed pre-escaped `&quot;` quotes around the escaped body.
fn w1_after(buf: &mut Vec<u8>, etag: &str) {
    let mut w = Writer::new(&mut *buf);
    w.write_event(Event::Start(BytesStart::new("D:getetag")))
        .unwrap();
    w.write_event(Event::Text(BytesText::from_escaped("&quot;")))
        .unwrap();
    w.write_event(Event::Text(BytesText::new(etag))).unwrap();
    w.write_event(Event::Text(BytesText::from_escaped("&quot;")))
        .unwrap();
    w.write_event(Event::End(BytesEnd::new("D:getetag")))
        .unwrap();
}

fn section_w1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let etag = "d41d8cd98f00b204e9800998ecf8427e-1719792000"; // realistic file etag

    // Equivalence: byte-identical output, incl. an etag with XML-special chars.
    let (mut b1, mut b2) = (Vec::new(), Vec::new());
    w1_before(&mut b1, etag);
    w1_after(&mut b2, etag);
    assert_eq!(b1, b2, "W1 emitted bytes differ (hex etag)");
    let (mut s1, mut s2) = (Vec::new(), Vec::new());
    w1_before(&mut s1, "a&b<c\"d-42");
    w1_after(&mut s2, "a&b<c\"d-42");
    assert_eq!(s1, s2, "W1 emitted bytes differ (special chars)");

    let mut buf = Vec::with_capacity(96);
    let before = measure(iters, || {
        buf.clear();
        w1_before(black_box(&mut buf), black_box(etag));
    });
    let after = measure(iters, || {
        buf.clear();
        w1_after(black_box(&mut buf), black_box(etag));
    });

    println!("\n## [W1] Native WebDAV getetag (per file+folder PROPFIND row, up to 500/page)");
    header_footer("sized String + escape vs borrowed events", &before, &after);
    gate_allocs("W1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [C1] CalDAV getetag — reused-buffer escape vs borrowed pre-escaped.
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE (verbatim `caldav_adapter` event sites): reused buffer holds `"{id}"`,
/// written auto-escaped — the buffer is amortized, so the only per-event alloc
/// is the escape of the two `"` → owned `Cow`. 1 alloc/event.
fn c1_before(buf: &mut Vec<u8>, etag_buf: &mut String, id: &str) {
    let mut w = Writer::new(&mut *buf);
    w.write_event(Event::Start(BytesStart::new("D:getetag")))
        .unwrap();
    etag_buf.clear();
    etag_buf.push('"');
    etag_buf.push_str(id);
    etag_buf.push('"');
    w.write_event(Event::Text(BytesText::new(etag_buf.as_str())))
        .unwrap();
    w.write_event(Event::End(BytesEnd::new("D:getetag")))
        .unwrap();
}

/// AFTER (`write_quoted_etag`): borrowed pre-escaped quotes; the UUID body is a
/// borrow (no XML-special chars). 0 allocs/event.
fn c1_after(buf: &mut Vec<u8>, id: &str) {
    let mut w = Writer::new(&mut *buf);
    w.write_event(Event::Start(BytesStart::new("D:getetag")))
        .unwrap();
    w.write_event(Event::Text(BytesText::from_escaped("&quot;")))
        .unwrap();
    w.write_event(Event::Text(BytesText::new(id))).unwrap();
    w.write_event(Event::Text(BytesText::from_escaped("&quot;")))
        .unwrap();
    w.write_event(Event::End(BytesEnd::new("D:getetag")))
        .unwrap();
}

fn section_c1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let id = "6b1e9f00-4c2a-4f1e-9b7a-2d5e8c1f0a3b"; // calendar / event UUID

    // Equivalence: byte-identical output for the UUID body.
    let (mut b1, mut eb, mut b2) = (Vec::new(), String::new(), Vec::new());
    c1_before(&mut b1, &mut eb, id);
    c1_after(&mut b2, id);
    assert_eq!(b1, b2, "C1 emitted bytes differ");

    let mut buf = Vec::with_capacity(96);
    let mut etag_buf = String::with_capacity(40);
    let before = measure(iters, || {
        buf.clear();
        c1_before(black_box(&mut buf), black_box(&mut etag_buf), black_box(id));
    });
    let after = measure(iters, || {
        buf.clear();
        c1_after(black_box(&mut buf), black_box(id));
    });

    println!("\n## [C1] CalDAV getetag (per event of every REPORT/multiget, per calendar)");
    header_footer("reused-buffer escape vs borrowed events", &before, &after);
    gate_allocs("C1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [D1] FileDto::from content_hash — getter clone (moved parts.blob_hash dropped)
//      vs reuse the moved String.
// ────────────────────────────────────────────────────────────────────────────

/// The 64-char BLAKE3-hex String a `File` owns in `blob_hash` (allocated in both
/// arms — the baseline; the delta is exactly the `content_hash` clone).
fn make_hash() -> String {
    "d41d8cd98f00b204e9800998ecf8427ed41d8cd98f00b204e9800998ecf8427e".to_string()
}

/// BEFORE: `content_hash = file.content_hash().to_string()` clones the hash,
/// then `into_parts()` moves the *same* `blob_hash` into `parts.blob_hash`,
/// which is dropped unused in the `Self { … }` ctor.
fn d1_before(owned_hash: String) -> String {
    let content_hash = owned_hash.clone(); // File::content_hash().to_string()
    let parts_blob_hash = owned_hash; // into_parts() moves blob_hash
    let _ = parts_blob_hash; // dropped unused in the Self{} ctor
    content_hash
}

/// AFTER: `content_hash: parts.blob_hash` — reuse the moved String, no clone.
fn d1_after(owned_hash: String) -> String {
    owned_hash
}

fn section_d1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);

    // Equivalence: identical content_hash string.
    assert_eq!(
        d1_before(make_hash()),
        d1_after(make_hash()),
        "D1 content_hash differs"
    );

    let before = measure(iters, || {
        black_box(d1_before(black_box(make_hash())));
    });
    let after = measure(iters, || {
        black_box(d1_after(black_box(make_hash())));
    });

    println!("\n## [D1] FileDto::from content_hash (per file row of every listing)");
    header_footer("getter clone + drop moved vs reuse moved", &before, &after);
    gate_allocs("D1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [E1] CalendarEvent timed DTSTART/DTEND — chrono strftime vs compact_ical_utc.
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE: `format!("{}", t.format("%Y%m%dT%H%M%SZ"))` — chrono's strftime
/// interpreter builds a `DelayedFormat` and formats six fields through
/// `core::fmt`, heap-allocating.
fn e1_before(dt: chrono::DateTime<Utc>) -> String {
    format!("{}", dt.format("%Y%m%dT%H%M%SZ"))
}

fn section_e1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let secs: i64 = 1_752_753_434; // 2025-07-17T11:57:14Z
    let dt = Utc.timestamp_opt(secs, 0).unwrap();

    // Equivalence: the stack render equals the chrono strftime output.
    let mut ebuf = [0u8; 16];
    let after_str = oxicloud::common::fmt::compact_ical_utc(&mut ebuf, secs).expect("in range");
    assert_eq!(e1_before(dt), after_str, "E1 stamp differs");

    let before = measure(iters, || {
        black_box(e1_before(black_box(dt)));
    });
    // AFTER: stack render via the shipped helper; the `&str` is passed straight
    // to `update_ical_property` in the source — 0 allocs.
    let after = measure(iters, || {
        let mut buf = [0u8; 16];
        black_box(oxicloud::common::fmt::compact_ical_utc(
            &mut buf,
            black_box(secs),
        ));
    });

    println!("\n## [E1] CalendarEvent timed DTSTART/DTEND (per event-edit PUT)");
    header_footer("chrono %Y%m%dT%H%M%SZ vs compact_ical_utc", &before, &after);
    gate_allocs("E1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [S1] ShareItemType::try_from — to_lowercase() String vs eq_ignore_ascii_case.
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE: `s.to_lowercase().as_str()` — a throwaway Unicode-lowercased String
/// (always allocates) — matched against two ASCII literals.
fn s1_before(s: &str) -> u8 {
    match s.to_lowercase().as_str() {
        "file" => 0,
        "folder" => 1,
        _ => 2,
    }
}

/// AFTER: `eq_ignore_ascii_case` — allocation-free, byte-identical acceptance
/// for the ASCII targets.
fn s1_after(s: &str) -> u8 {
    if s.eq_ignore_ascii_case("file") {
        0
    } else if s.eq_ignore_ascii_case("folder") {
        1
    } else {
        2
    }
}

fn section_s1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);

    // Equivalence across mixed case + invalid input.
    for s in ["file", "File", "FOLDER", "folder", "Folder", "bogus", ""] {
        assert_eq!(s1_before(s), s1_after(s), "S1 verdict differs for {s:?}");
    }

    let sample = "Folder"; // mixed-case → to_lowercase allocates
    let before = measure(iters, || {
        black_box(s1_before(black_box(sample)));
    });
    let after = measure(iters, || {
        black_box(s1_after(black_box(sample)));
    });

    println!("\n## [S1] ShareItemType::try_from (per share item-type parse)");
    header_footer(
        "to_lowercase() String vs eq_ignore_ascii_case",
        &before,
        &after,
    );
    gate_allocs("S1", &before, &after);
}

fn main() {
    println!("# Round-22 micro-pack — BEFORE/AFTER (counting allocator, release)");
    println!("# allocs/op is the deterministic gate; a non-winning AFTER exits 1 (rollback).");
    section_h1();
    section_w1();
    section_c1();
    section_d1();
    section_e1();
    section_s1();
    println!("\nAll Round-22 sections passed their allocation gate.");
}
