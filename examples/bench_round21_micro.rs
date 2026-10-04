//! Round-21 CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–20: each section is BEFORE (verbatim replica of the
//! shipped-before shape) vs AFTER (verbatim replica of the shipped-after shape,
//! which the source is then made to match), with a byte/-value equivalence gate
//! and a `GATE FAIL … rollback` check that `std::process::exit(1)`s if the AFTER
//! arm fails to beat its BEFORE — the round's roll-back rule encoded into the
//! benchmark. An AFTER that doesn't win is never applied to the source.
//!
//!   [R1] The CalDAV/CardDAV row-mapping repositories build their result Vec
//!        with `let mut v = Vec::new(); for row in rows { v.push(map(row)?) }`,
//!        growing the container from capacity 0 (~⌈log₂N⌉ reallocations, each
//!        memcpy-ing the accumulated rows). AFTER pre-sizes with
//!        `Vec::with_capacity(rows.len())` — the file-side sibling ROUND20 §I1
//!        shipped, extended to the calendar/contact repos it deferred.
//!
//!   [R2] `BlobHandler::settle_batch` cloned every 64-char chunk hash into a
//!        `Vec<String>` purely to `.bind()` it to the pin `UPDATE … = ANY($1)`.
//!        AFTER binds a borrowed `Vec<&str>` — sqlx encodes `&[&str]` to
//!        `text[]` identically (favorites_pg_repository.rs:271 already does
//!        this), so the per-chunk hash `String` disappears.
//!
//!   [R3] `BlobHandler::store_loose_chunks` (the delta-upload sibling of the
//!        ROUND17 §D2 ingest loop) kept an intra-request dedup `HashSet<String>`
//!        and cloned the hex hash TWICE per frame (into `received` and into the
//!        set). AFTER keys the set on the raw `[u8; 32]` BLAKE3 digest (`Copy`,
//!        no heap key) and moves the hex into `received` on a duplicate.
//!
//!   [R4] `carddav_adapter::write_contact_response` built a `"…"`-quoted
//!        `String` for `getetag` then wrote it auto-escaped (quick_xml escapes
//!        the `"` → `&quot;`, re-allocating). AFTER emits the two quotes as
//!        borrowed pre-escaped `&quot;` text events (the NextCloud ROUND20 §C1
//!        pattern applied to the CardDAV emitter it missed).
//!
//!   [R5] `contact_to_vcard` stamped `BDAY` via `write!(…, "{}",
//!        bday.format("%Y-%m-%d"))`, running chrono's strftime interpreter per
//!        contact-with-birthday. AFTER renders the fixed `YYYY-MM-DD` on the
//!        stack via `fmt::compact_date` (the date-only companion to the §V2 REV
//!        renderer), with the chrono fallback for out-of-range years.
//!
//!   [R6] The NextCloud trashbin PROPFIND row set `d:getcontenttype` for a
//!        folder to `"httpd/unix-directory".to_string()` — a heap String for a
//!        static constant, per trashed folder row. AFTER borrows it via
//!        `Cow::Borrowed` (the ROUND16 §M1 `Cow<'static, str>` pattern).
//!
//! Run:
//!   cargo run --release --features bench --example bench_round21_micro
//! Tunables (env): BENCH_ITERS (200000), R1_ROWS (200), R3_FRAMES (128)

use std::alloc::{GlobalAlloc, Layout, System};
use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::NaiveDate;
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
        "| {:<50} | {:>12.1} | {:>10.2} |",
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
// [R1] Row-mapper container pre-size — Vec::new()+push vs with_capacity+push
// ────────────────────────────────────────────────────────────────────────────

/// A Contact-sized (~192 B) mapped element so the container-realloc memcpy cost
/// is realistic. The per-element mapper allocates nothing in either arm, so the
/// measured alloc delta is exactly the container growth (the CalDAV/CardDAV
/// `row_to_*` allocs are identical in both arms and out of scope here).
type MappedRow = [u8; 192];

fn r1_before(rows: &[MappedRow]) -> Vec<MappedRow> {
    let mut out = Vec::new();
    for row in rows {
        out.push(*row);
    }
    out
}

fn r1_after(rows: &[MappedRow]) -> Vec<MappedRow> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(*row);
    }
    out
}

fn section_r1() {
    let n: usize = env_or("R1_ROWS", 200);
    let iters: usize = env_or("BENCH_ITERS", 200_000) / 20; // heavier op
    let rows: Vec<MappedRow> = (0..n).map(|i| [i as u8; 192]).collect();

    assert_eq!(r1_before(&rows).len(), r1_after(&rows).len());

    let before = measure(iters, || {
        black_box(r1_before(black_box(&rows)));
    });
    let after = measure(iters, || {
        black_box(r1_after(black_box(&rows)));
    });

    println!("\n## [R1] CalDAV/CardDAV row-mapper pre-size ({n} contact-sized rows)");
    header_footer("Vec::new()+push vs with_capacity+push", &before, &after);
    gate_allocs("R1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [R2] settle_batch bind — Vec<String> clone vs Vec<&str> borrow
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE: clone every chunk hash into an owned `Vec<String>` to `.bind()`.
fn r2_before(batch: &[(String, u64)]) -> Vec<String> {
    batch.iter().map(|(h, _)| h.clone()).collect()
}

/// AFTER: borrow — sqlx encodes `&[&str]` to `text[]` identically.
fn r2_after(batch: &[(String, u64)]) -> Vec<&str> {
    batch.iter().map(|(h, _)| h.as_str()).collect()
}

fn section_r2() {
    let n: usize = env_or("FLUSH_MAX_CHUNKS", 32);
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // A settle batch of 32 chunks, each a 64-char BLAKE3 hex hash.
    let batch: Vec<(String, u64)> = (0..n)
        .map(|i| {
            (
                format!("{:064x}", i as u128 * 0x9E37_79B9_7F4A_7C15),
                65_536,
            )
        })
        .collect();

    // Equivalence: the borrowed &strs equal the owned String hashes.
    let b = r2_before(&batch);
    let a = r2_after(&batch);
    assert_eq!(b.len(), a.len(), "R2 length differs");
    assert!(
        b.iter().zip(&a).all(|(s, t)| s == t),
        "R2 bound hashes differ"
    );

    let before = measure(iters, || {
        black_box(r2_before(black_box(&batch)));
    });
    let after = measure(iters, || {
        black_box(r2_after(black_box(&batch)));
    });

    println!("\n## [R2] settle_batch hash bind ({n}-chunk batch)");
    header_footer("Vec<String> clone vs Vec<&str> borrow", &before, &after);
    gate_allocs("R2", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [R3] store_loose_chunks — HashSet<String>+2 clones vs HashSet<[u8;32]>+move
// ────────────────────────────────────────────────────────────────────────────

/// `(received-in-order, distinct-new-rows)` — `store_loose_chunks`'s two
/// observable outputs.
type R3Out = (Vec<(String, u64)>, Vec<(String, i64)>);

/// BEFORE: the shipped-before delta-upload loop — `HashSet<String>` intra-
/// request dedup set, hex hash cloned into `received` AND into the set per frame.
/// Returns (received-in-order, distinct-new-rows) — the observable result.
fn r3_before(frames: &[([u8; 32], String)]) -> R3Out {
    let mut received: Vec<(String, u64)> = Vec::new();
    let mut new_rows: Vec<(String, i64)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (_digest, hex) in frames {
        // Step 1 (common to both arms): the fresh per-frame hex String
        // (`blake3::hash(&data).to_hex().to_string()`).
        let hash = hex.clone();
        received.push((hash.clone(), 65_536));
        if seen.insert(hash.clone()) {
            new_rows.push((hash, 65_536));
        }
    }
    (received, new_rows)
}

/// AFTER: dedup set keyed on the raw 32-byte digest; hex moved into `received`
/// on a duplicate, cloned only on the first occurrence (needed by `new_rows`).
fn r3_after(frames: &[([u8; 32], String)]) -> R3Out {
    let mut received: Vec<(String, u64)> = Vec::new();
    let mut new_rows: Vec<(String, i64)> = Vec::new();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    for (digest, hex) in frames {
        let hash = hex.clone(); // step 1, same as BEFORE
        if seen.insert(*digest) {
            received.push((hash.clone(), 65_536));
            new_rows.push((hash, 65_536));
        } else {
            received.push((hash, 65_536));
        }
    }
    (received, new_rows)
}

fn section_r3() {
    let n: usize = env_or("R3_FRAMES", 128);
    let iters: usize = env_or("BENCH_ITERS", 200_000) / 20; // heavier op
    // A delta stream where every other frame repeats the previous chunk (a
    // re-chunked near-duplicate / zero-padded region) → 50% intra-request dups.
    let frames: Vec<([u8; 32], String)> = (0..n)
        .map(|i| {
            let key = i / 2; // pairs share a digest
            let h = blake3::hash(&(key as u64).to_le_bytes());
            (*h.as_bytes(), h.to_hex().to_string())
        })
        .collect();

    // Equivalence: identical received sequence and distinct new_rows.
    let b = r3_before(&frames);
    let a = r3_after(&frames);
    assert_eq!(b.0, a.0, "R3 received sequence differs");
    assert_eq!(b.1, a.1, "R3 new_rows differ");

    let before = measure(iters, || {
        black_box(r3_before(black_box(&frames)));
    });
    let after = measure(iters, || {
        black_box(r3_after(black_box(&frames)));
    });

    println!("\n## [R3] store_loose_chunks dedup ({n} frames, 50% dup)");
    header_footer("HashSet<String>+2 clones vs [u8;32]+move", &before, &after);
    gate_allocs("R3", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [R4] CardDAV getetag — quoted String + escape vs borrowed pre-escaped
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE: build a `"…"`-quoted `String`, then write it as an auto-escaped text
/// element — `quick_xml` escapes the `"` → `&quot;`, re-allocating an owned Cow.
fn r4_before(buf: &mut Vec<u8>, etag: &str) {
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

/// AFTER: emit the pre-escaped `&quot;` quote literals as borrowed text events
/// around the escaped etag body — byte-identical output, zero owned strings.
fn r4_after(buf: &mut Vec<u8>, etag: &str) {
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

fn section_r4() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let etag = "a1b2c3d4e5f6-1719792000"; // realistic contact etag

    // Equivalence: byte-identical output, incl. an etag with XML-special chars.
    let (mut b1, mut b2) = (Vec::new(), Vec::new());
    r4_before(&mut b1, etag);
    r4_after(&mut b2, etag);
    assert_eq!(b1, b2, "R4 emitted bytes differ (hex etag)");
    let (mut s1, mut s2) = (Vec::new(), Vec::new());
    r4_before(&mut s1, "abc&def<x\"y");
    r4_after(&mut s2, "abc&def<x\"y");
    assert_eq!(s1, s2, "R4 emitted bytes differ (special chars)");

    let mut buf = Vec::with_capacity(64);
    let before = measure(iters, || {
        buf.clear();
        r4_before(black_box(&mut buf), black_box(etag));
    });
    let after = measure(iters, || {
        buf.clear();
        r4_after(black_box(&mut buf), black_box(etag));
    });

    println!("\n## [R4] CardDAV getetag (per-contact multiget/PROPFIND row)");
    header_footer("quoted String + escape vs borrowed events", &before, &after);
    gate_allocs("R4", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [R5] BDAY stamp — chrono %Y-%m-%d interpreter vs compact_date stack render
// ────────────────────────────────────────────────────────────────────────────

const DEC_LUT: &[u8; 200] = b"0001020304050607080910111213141516171819\
                              2021222324252627282930313233343536373839\
                              4041424344454647484950515253545556575859\
                              6061626364656667686970717273747576777879\
                              8081828384858687888990919293949596979899";

#[inline]
fn push2(out: &mut [u8], pos: usize, v: u32) {
    let d = (v as usize) * 2;
    out[pos] = DEC_LUT[d];
    out[pos + 1] = DEC_LUT[d + 1];
}

#[inline]
fn push4(out: &mut [u8], pos: usize, v: i64) {
    out[pos] = b'0' + (v / 1000 % 10) as u8;
    out[pos + 1] = b'0' + (v / 100 % 10) as u8;
    out[pos + 2] = b'0' + (v / 10 % 10) as u8;
    out[pos + 3] = b'0' + (v % 10) as u8;
}

/// Replica of the shipped `fmt::compact_date` the AFTER source calls.
fn bench_compact_date(buf: &mut [u8; 10], year: i32, month: u32, day: u32) -> Option<&str> {
    if !(0..=9999).contains(&year) {
        return None;
    }
    push4(buf, 0, year as i64);
    buf[4] = b'-';
    push2(buf, 5, month);
    buf[7] = b'-';
    push2(buf, 8, day);
    Some(std::str::from_utf8(&buf[..]).expect("ascii"))
}

/// BEFORE: `write!(vcard, "BDAY:{}\r\n", bday.format("%Y-%m-%d"))` into the
/// reused buffer — chrono's strftime interpreter per contact-with-birthday.
fn r5_before(vcard: &mut String, bday: NaiveDate) {
    use std::fmt::Write as _;
    let _ = write!(vcard, "BDAY:{}\r\n", bday.format("%Y-%m-%d"));
}

/// AFTER: stack render via `compact_date`, chrono fallback out of range.
fn r5_after(vcard: &mut String, bday: NaiveDate) {
    use chrono::Datelike as _;
    let mut buf = [0u8; 10];
    match bench_compact_date(&mut buf, bday.year(), bday.month(), bday.day()) {
        Some(s) => {
            vcard.push_str("BDAY:");
            vcard.push_str(s);
            vcard.push_str("\r\n");
        }
        None => {
            use std::fmt::Write as _;
            let _ = write!(vcard, "BDAY:{}\r\n", bday.format("%Y-%m-%d"));
        }
    }
}

fn section_r5() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let bday = NaiveDate::from_ymd_opt(1987, 3, 5).unwrap();

    // Equivalence: byte-identical BDAY line.
    let (mut b, mut a) = (String::new(), String::new());
    r5_before(&mut b, bday);
    r5_after(&mut a, bday);
    assert_eq!(b, a, "R5 BDAY line differs");
    assert_eq!(b, "BDAY:1987-03-05\r\n");

    let mut buf = String::with_capacity(32);
    let before = measure(iters, || {
        buf.clear();
        r5_before(black_box(&mut buf), black_box(bday));
    });
    let after = measure(iters, || {
        buf.clear();
        r5_after(black_box(&mut buf), black_box(bday));
    });

    println!("\n## [R5] BDAY stamp (per contact-with-birthday)");
    header_footer("chrono %Y-%m-%d vs compact_date", &before, &after);
    gate_allocs("R5", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [R6] trashbin folder content-type — String::to_string() vs Cow::Borrowed
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE: heap a `String` for the static folder content-type constant.
fn r6_before(is_folder: bool, name: &str) -> String {
    if is_folder {
        "httpd/unix-directory".to_string()
    } else {
        // File branch (mime_guess) — allocates in both arms, out of scope.
        format!("application/{}", name.rsplit('.').next().unwrap_or("octet"))
    }
}

/// AFTER: borrow the folder constant; only the file branch owns its String.
fn r6_after(is_folder: bool, name: &str) -> Cow<'static, str> {
    if is_folder {
        Cow::Borrowed("httpd/unix-directory")
    } else {
        Cow::Owned(format!(
            "application/{}",
            name.rsplit('.').next().unwrap_or("octet")
        ))
    }
}

fn section_r6() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);

    // Equivalence: same content-type string for a folder row.
    assert_eq!(r6_before(true, "x"), r6_after(true, "x").as_ref());

    let before = measure(iters, || {
        black_box(r6_before(black_box(true), black_box("Documents")));
    });
    let after = measure(iters, || {
        black_box(r6_after(black_box(true), black_box("Documents")));
    });

    println!("\n## [R6] trashbin folder content-type (per trashed folder row)");
    header_footer("String::to_string() vs Cow::Borrowed", &before, &after);
    gate_allocs("R6", &before, &after);
}

fn main() {
    println!("# Round-21 micro-pack — BEFORE/AFTER (counting allocator, release)");
    println!("# allocs/op is the deterministic gate; a non-winning AFTER exits 1 (rollback).");
    section_r1();
    section_r2();
    section_r3();
    section_r4();
    section_r6();
    // R5 (BDAY) last: it is the one section whose BEFORE (chrono's NaiveDate
    // strftime) may or may not heap-allocate; ordering it last lets every other
    // section print + gate before R5's gate can halt the run.
    section_r5();
    println!("\nAll Round-21 sections passed their allocation gate.");
}
