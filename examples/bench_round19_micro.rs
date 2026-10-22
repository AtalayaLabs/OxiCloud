//! Round-19 CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–18: each section is BEFORE (verbatim replica of the
//! shipped-before shape) vs AFTER (the shipped function itself where reachable —
//! `common::fmt::compact_ical_utc` — else a verbatim replica of the shipped-after
//! shape), with a byte/-value equivalence gate and a `GATE FAIL … rollback`
//! check that exits non-zero if the AFTER arm fails to beat its BEFORE — the
//! round's roll-back rule encoded into the benchmark.
//!
//!   [M1] `AppPasswordService::verify_basic_auth` builds the moka cache key as
//!        `blake3::hash(format!("{username}:{password}").as_bytes())` on EVERY
//!        Basic-auth DAV/CalDAV/CardDAV/NextCloud request (before the cache
//!        lookup, so even cache hits pay it). The `format!` heap-allocates one
//!        throw-away `String` per request purely to feed bytes to blake3. The
//!        shipped-after form streams the same bytes into an incremental
//!        `blake3::Hasher` — byte-identical 32-byte key, zero allocation.
//!
//!   [M2] `WopiTokenService::validate_token` / `generate_token` rebuilt a
//!        `Validation` (allocates a `required_spec_claims` HashSet + an
//!        `algorithms` Vec) and a `DecodingKey`/`EncodingKey` (copies the secret
//!        into a fresh Vec) on EVERY WOPI protocol call — Office/Collabora hosts
//!        poll these continuously. The shipped-after form prebuilds all three as
//!        struct fields in `new()` (exactly what `JwtTokenService` already does).
//!
//!   [V1] `contact_to_vcard` / `generate_vcard` emit, per contact in every
//!        CardDAV REPORT / multiget / PROPFIND-with-address-data:
//!          - FN fallback `format!("{first} {last}").trim().to_string()` dropped
//!            the throwaway `.to_string()` copy (writes the borrowed trim slice);
//!          - NOTE `notes.replace('\n', "\\n")` allocated a full copy even when
//!            the note has no newline — now guarded (`contains('\n')`), the
//!            common no-newline note writes the borrowed slice directly;
//!          - REV `updated_at.format("%Y%m%dT%H%M%SZ")` ran chrono's strftime
//!            interpreter — now `common::fmt::compact_ical_utc` (stack LUT).
//!
//!   [V2] REV/DTSTAMP stamp isolated: chrono `.format("%Y%m%dT%H%M%SZ")` vs the
//!        new `common::fmt::compact_ical_utc` stack renderer (CPU / wall gate).
//!
//!   [M4] `trash_service::row_to_item_dto` `clone()`d `name` / `path` /
//!        `blob_hash` out of an OWNED `row` that is dropped at fn end — now
//!        moved (the favorites / recent / folder row mappers already move these
//!        same fields).
//!
//!   [M5] `SearchUseCase::search` built the cache-key user segment via
//!        `user_id.to_string()` (heap) to feed `create_cache_key`'s hasher — now
//!        stack-encoded via `Uuid::hyphenated().encode_lower(&mut [u8; 36])`,
//!        byte-identical string ⇒ identical u64 key, zero allocation.
//!
//!   [M6] WebDAV streaming PROPFIND built each child `href` with a fresh
//!        `format!` per row (up to 500 rows/page) — now a single buffer reused
//!        across the page (`clear` + `push_str` + `extend`).
//!
//!   [M7] `nextcloud::session::extract_url_user` forced `.into_owned()` on the
//!        `urlencoding::decode` `Cow` on EVERY path-scoped NC DAV request, even
//!        though a plain-ASCII username decodes to `Cow::Borrowed` — now returns
//!        the `Cow` and compares by `.as_ref()`, zero-alloc on the common path.
//!
//! Run:
//!   cargo run --release --features bench --example bench_round19_micro
//! Tunables (env): BENCH_ITERS (200000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::{DateTime, TimeZone, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
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
        "| {:<48} | {:>12.1} | {:>10.2} |",
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

/// Wall gate for the CPU-only sections (identical alloc count both arms).
/// Requires AFTER to be at least `min_ratio`× faster to guard against noise.
fn gate_wall(tag: &str, before: &Measured, after: &Measured, min_ratio: f64) {
    let ratio = before.wall_ns_per_op / after.wall_ns_per_op;
    if ratio < min_ratio {
        eprintln!(
            "GATE FAIL [{tag}]: AFTER wall {:.1}ns not ≥{min_ratio:.2}× faster than BEFORE {:.1}ns (ratio {ratio:.2}) — rollback",
            after.wall_ns_per_op, before.wall_ns_per_op
        );
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [M1] Basic-auth cache key — format! + hash vs incremental hasher
// ────────────────────────────────────────────────────────────────────────────

fn m1_before(username: &str, password: &str) -> [u8; 32] {
    blake3::hash(format!("{}:{}", username, password).as_bytes()).into()
}

fn m1_after(username: &str, password: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(username.as_bytes());
    h.update(b":");
    h.update(password.as_bytes());
    h.finalize().into()
}

fn section_m1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let username = "benchuser@example.com";
    let password = "Xk29-a83Q-p01M-77zL"; // NC app-password shape

    // Equivalence: the two forms feed blake3 the exact same byte stream.
    assert_eq!(
        m1_before(username, password),
        m1_after(username, password),
        "M1 cache key differs between BEFORE and AFTER"
    );

    let before = measure(iters, || {
        black_box(m1_before(black_box(username), black_box(password)));
    });
    let after = measure(iters, || {
        black_box(m1_after(black_box(username), black_box(password)));
    });

    println!("\n## [M1] Basic-auth cache key (blake3)");
    header_footer("verify_basic_auth cache-key hash", &before, &after);
    gate_allocs("M1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [M2] WOPI token validate — rebuilt Validation/DecodingKey vs prebuilt
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct BenchWopiClaims {
    sub: String,
    file_id: String,
    can_write: bool,
    scope: String,
    username: String,
    exp: i64,
    iat: i64,
}

fn m2_make_token(secret: &str) -> String {
    let claims = BenchWopiClaims {
        sub: "c410b103-7b86-4ac2-9eb4-3804351547be".into(),
        file_id: "0e72efc0-0d1c-45a1-b434-52336643b3f7".into(),
        can_write: true,
        scope: "wopi".into(),
        username: "bench_user".into(),
        exp: 4_102_444_799, // far future so validation passes
        iat: 1_700_000_000,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .expect("encode")
}

fn m2_before(secret: &str, token: &str) -> String {
    let validation = Validation::new(Algorithm::HS256);
    let data = decode::<BenchWopiClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .expect("decode");
    data.claims.file_id
}

fn m2_after(decoding_key: &DecodingKey, validation: &Validation, token: &str) -> String {
    let data = decode::<BenchWopiClaims>(token, decoding_key, validation).expect("decode");
    data.claims.file_id
}

fn section_m2() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let secret = "wopi_secret_at_least_32_bytes_long!!";
    let token = m2_make_token(secret);

    // Prebuilt (shipped-after) config, mirroring WopiTokenService::new.
    let decoding_key = DecodingKey::from_secret(secret.as_bytes());
    let validation = Validation::new(Algorithm::HS256);

    // Equivalence: same claim extracted.
    assert_eq!(
        m2_before(secret, &token),
        m2_after(&decoding_key, &validation, &token),
        "M2 decoded claim differs between BEFORE and AFTER"
    );

    let before = measure(iters, || {
        black_box(m2_before(black_box(secret), black_box(&token)));
    });
    let after = measure(iters, || {
        black_box(m2_after(
            black_box(&decoding_key),
            black_box(&validation),
            black_box(&token),
        ));
    });

    println!("\n## [M2] WOPI token validate (prebuilt Validation/DecodingKey)");
    header_footer("validate_token", &before, &after);
    gate_allocs("M2", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [V1] vCard per-contact emit — FN fallback / NOTE / REV
// ────────────────────────────────────────────────────────────────────────────

struct BenchContact {
    uid: String,
    first_name: Option<String>,
    last_name: Option<String>,
    full_name: Option<String>,
    email: Vec<(String, String)>,
    notes: Option<String>,
    updated_at: DateTime<Utc>,
}

fn v1_before(c: &BenchContact) -> String {
    let mut vcard = String::with_capacity(256);
    vcard.push_str("BEGIN:VCARD\r\nVERSION:3.0\r\n");
    let _ = write!(vcard, "UID:{}\r\n", c.uid);

    if let (Some(last), Some(first)) = (&c.last_name, &c.first_name) {
        let _ = write!(vcard, "N:{};{};;;\r\n", last, first);
    }

    if let Some(fn_name) = &c.full_name {
        let _ = write!(vcard, "FN:{}\r\n", fn_name);
    } else {
        let fn_name = format!(
            "{} {}",
            c.first_name.as_deref().unwrap_or(""),
            c.last_name.as_deref().unwrap_or(""),
        )
        .trim()
        .to_string();
        if !fn_name.is_empty() {
            let _ = write!(vcard, "FN:{}\r\n", fn_name);
        } else {
            vcard.push_str("FN:Unknown\r\n");
        }
    }

    for (ty, addr) in &c.email {
        vcard.push_str("EMAIL;TYPE=");
        oxicloud::common::fmt::push_upper(&mut vcard, ty);
        vcard.push(':');
        vcard.push_str(addr);
        vcard.push_str("\r\n");
    }

    if let Some(notes) = &c.notes {
        let _ = write!(vcard, "NOTE:{}\r\n", notes.replace('\n', "\\n"));
    }

    let _ = write!(vcard, "REV:{}\r\n", c.updated_at.format("%Y%m%dT%H%M%SZ"));
    vcard.push_str("END:VCARD\r\n");
    vcard
}

fn v1_after(c: &BenchContact) -> String {
    let mut vcard = String::with_capacity(256);
    vcard.push_str("BEGIN:VCARD\r\nVERSION:3.0\r\n");
    let _ = write!(vcard, "UID:{}\r\n", c.uid);

    if let (Some(last), Some(first)) = (&c.last_name, &c.first_name) {
        let _ = write!(vcard, "N:{};{};;;\r\n", last, first);
    }

    if let Some(fn_name) = &c.full_name {
        let _ = write!(vcard, "FN:{}\r\n", fn_name);
    } else {
        let fn_name = format!(
            "{} {}",
            c.first_name.as_deref().unwrap_or(""),
            c.last_name.as_deref().unwrap_or(""),
        );
        let trimmed = fn_name.trim();
        if !trimmed.is_empty() {
            let _ = write!(vcard, "FN:{}\r\n", trimmed);
        } else {
            vcard.push_str("FN:Unknown\r\n");
        }
    }

    for (ty, addr) in &c.email {
        vcard.push_str("EMAIL;TYPE=");
        oxicloud::common::fmt::push_upper(&mut vcard, ty);
        vcard.push(':');
        vcard.push_str(addr);
        vcard.push_str("\r\n");
    }

    if let Some(notes) = &c.notes {
        if notes.contains('\n') {
            let _ = write!(vcard, "NOTE:{}\r\n", notes.replace('\n', "\\n"));
        } else {
            vcard.push_str("NOTE:");
            vcard.push_str(notes);
            vcard.push_str("\r\n");
        }
    }

    let mut rev_buf = [0u8; 16];
    let secs = c.updated_at.timestamp();
    match oxicloud::common::fmt::compact_ical_utc(&mut rev_buf, secs) {
        Some(s) => {
            vcard.push_str("REV:");
            vcard.push_str(s);
            vcard.push_str("\r\n");
        }
        None => {
            let _ = write!(vcard, "REV:{}\r\n", c.updated_at.format("%Y%m%dT%H%M%SZ"));
        }
    }
    vcard.push_str("END:VCARD\r\n");
    vcard
}

fn section_v1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // A contact WITHOUT full_name (exercises the FN fallback), with a
    // multi-line-free NOTE (the common case) and a REV stamp.
    let c = BenchContact {
        uid: "c-round19@oxicloud.test".into(),
        first_name: Some("Ada".into()),
        last_name: Some("Lovelace".into()),
        full_name: None,
        email: vec![
            ("home".into(), "ada@oxicloud.test".into()),
            ("work".into(), "a.lovelace@work.test".into()),
        ],
        notes: Some("Met at the analytical-engine expo; follow up re: punch cards.".into()),
        updated_at: Utc.timestamp_opt(1_752_753_434, 0).unwrap(),
    };

    let b = v1_before(&c);
    let a = v1_after(&c);
    assert_eq!(b, a, "V1 emitted vCard differs between BEFORE and AFTER");

    let before = measure(iters, || {
        black_box(v1_before(black_box(&c)));
    });
    let after = measure(iters, || {
        black_box(v1_after(black_box(&c)));
    });

    println!("\n## [V1] vCard per-contact emit (FN fallback + NOTE + REV)");
    header_footer("contact_to_vcard", &before, &after);
    gate_allocs("V1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [V2] REV/DTSTAMP stamp — chrono strftime vs compact_ical_utc (wall)
// ────────────────────────────────────────────────────────────────────────────

fn section_v2() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let dt = Utc.timestamp_opt(1_752_753_434, 0).unwrap();
    let secs = dt.timestamp();

    // Equivalence: identical stamp bytes.
    let mut buf = [0u8; 16];
    assert_eq!(
        oxicloud::common::fmt::compact_ical_utc(&mut buf, secs).unwrap(),
        dt.format("%Y%m%dT%H%M%SZ").to_string(),
        "V2 stamp differs between chrono and compact_ical_utc"
    );

    // Both write into a reused buffer (isolating the formatter cost, not the
    // buffer alloc) — mirrors the REV emit into the per-contact vCard buffer.
    let mut sink = String::with_capacity(32);
    let before = measure(iters, || {
        sink.clear();
        let _ = write!(sink, "{}", black_box(dt).format("%Y%m%dT%H%M%SZ"));
        black_box(&sink);
    });
    let after = measure(iters, || {
        sink.clear();
        let mut b = [0u8; 16];
        if let Some(s) = oxicloud::common::fmt::compact_ical_utc(&mut b, black_box(secs)) {
            sink.push_str(s);
        }
        black_box(&sink);
    });

    println!("\n## [V2] REV stamp — chrono strftime vs compact_ical_utc");
    header_footer("compact_ical_utc", &before, &after);
    // CPU-only: chrono's DelayedFormat writes field-by-field (no heap), so the
    // win is wall, not allocs. Require a clear ≥2× to shrug off noise.
    gate_wall("V2", &before, &after, 2.0);
}

// ────────────────────────────────────────────────────────────────────────────
// [M4] trash row → DTO — clone vs move of owned String fields
// ────────────────────────────────────────────────────────────────────────────

struct BenchTrashRow {
    resource_id: Uuid,
    name: String,
    path: Option<String>,
    blob_hash: Option<String>,
}

struct BenchFileDto {
    id: String,
    name: String,
    path: String,
    content_hash: String,
    etag: String,
}

fn compute_etag(hash: &str, modified: u64) -> String {
    // Same shape as File::compute_etag (a small formatted String) — the point
    // is the surrounding clone/move, not this helper.
    format!("\"{hash}-{modified}\"")
}

fn m4_before(row: BenchTrashRow) -> BenchFileDto {
    let path = row.path.clone().unwrap_or_default();
    let content_hash = row.blob_hash.clone().unwrap_or_default();
    let etag = if content_hash.is_empty() {
        String::new()
    } else {
        compute_etag(&content_hash, 1_752_753_434)
    };
    let _classes = row.name.len(); // stands in for category_for(&row.name, …)
    BenchFileDto {
        id: row.resource_id.to_string(),
        name: row.name.clone(),
        path,
        content_hash,
        etag,
    }
}

fn m4_after(row: BenchTrashRow) -> BenchFileDto {
    let path = row.path.unwrap_or_default();
    let content_hash = row.blob_hash.unwrap_or_default();
    let etag = if content_hash.is_empty() {
        String::new()
    } else {
        compute_etag(&content_hash, 1_752_753_434)
    };
    let _classes = row.name.len();
    BenchFileDto {
        id: row.resource_id.to_string(),
        name: row.name,
        path,
        content_hash,
        etag,
    }
}

fn make_row() -> BenchTrashRow {
    BenchTrashRow {
        resource_id: Uuid::from_u128(0x0e72efc0_0d1c_45a1_b434_52336643b3f7),
        name: "Quarterly Financial Report 2026 Q3 (final).xlsx".into(),
        path: Some(
            "/Documents/Finance/2026/Q3/Quarterly Financial Report 2026 Q3 (final).xlsx".into(),
        ),
        blob_hash: Some("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262".into()),
    }
}

fn section_m4() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);

    // Equivalence: identical DTO fields.
    let b = m4_before(make_row());
    let a = m4_after(make_row());
    assert!(
        b.id == a.id
            && b.name == a.name
            && b.path == a.path
            && b.content_hash == a.content_hash
            && b.etag == a.etag,
        "M4 DTO differs between BEFORE and AFTER"
    );

    let before = measure(iters, || {
        black_box(m4_before(black_box(make_row())));
    });
    let after = measure(iters, || {
        black_box(m4_after(black_box(make_row())));
    });

    println!("\n## [M4] trash row → DTO (move vs clone)");
    // Both arms pay the identical `make_row()` construction + `id.to_string()`;
    // the delta is the removed name/path/blob_hash clones.
    header_footer("row_to_item_dto (file branch)", &before, &after);
    gate_allocs("M4", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [M5] search cache key — user_id.to_string() vs stack hyphenated encode
// ────────────────────────────────────────────────────────────────────────────

fn m5_key_from_str(user_id: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    // stand-in for `criteria.hash(&mut hasher)` — a constant, identical both arms
    "q=report&type=file".hash(&mut hasher);
    user_id.hash(&mut hasher);
    hasher.finish()
}

fn m5_before(user_id: Uuid) -> u64 {
    let user_id_str = user_id.to_string();
    m5_key_from_str(&user_id_str)
}

fn m5_after(user_id: Uuid) -> u64 {
    let mut buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
    let s = user_id.hyphenated().encode_lower(&mut buf);
    m5_key_from_str(s)
}

fn section_m5() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let user_id = Uuid::from_u128(0xc410b103_7b86_4ac2_9eb4_3804351547be);

    // Equivalence: the stack-encoded string is byte-identical to to_string(),
    // so the hasher sees identical bytes ⇒ identical key.
    assert_eq!(
        m5_before(user_id),
        m5_after(user_id),
        "M5 cache key differs between BEFORE and AFTER"
    );

    let before = measure(iters, || {
        black_box(m5_before(black_box(user_id)));
    });
    let after = measure(iters, || {
        black_box(m5_after(black_box(user_id)));
    });

    println!("\n## [M5] search cache key (Uuid stack-encode)");
    header_footer("create_cache_key user segment", &before, &after);
    gate_allocs("M5", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [M6] PROPFIND per-child href — fresh format! vs reused buffer
// ────────────────────────────────────────────────────────────────────────────

const BENCH_ENCODE_SET: &AsciiSet = NON_ALPHANUMERIC;

fn m6_before(base_href: &str, names: &[String]) -> usize {
    // Mirrors the shipped per-child loop: one fresh String per row.
    let mut total = 0usize;
    for name in names {
        let href = format!(
            "{}{}",
            base_href,
            utf8_percent_encode(name, BENCH_ENCODE_SET)
        );
        total += black_box(href).len();
    }
    total
}

fn m6_after(base_href: &str, names: &[String]) -> usize {
    // One buffer reused across the whole page.
    let mut total = 0usize;
    let mut href = String::new();
    for name in names {
        href.clear();
        href.push_str(base_href);
        href.extend(utf8_percent_encode(name, BENCH_ENCODE_SET));
        total += black_box(&href).len();
    }
    total
}

fn section_m6() {
    let iters: usize = env_or("BENCH_ITERS", 4_000); // per-op is a whole page
    let base_href = "/webdav/Documents/Projects/";
    let names: Vec<String> = (0..64)
        .map(|i| format!("Report {i} draft (v2) — final.pdf"))
        .collect();

    // Equivalence: byte-identical href set.
    let mut hb = Vec::new();
    for name in &names {
        hb.push(format!(
            "{}{}",
            base_href,
            utf8_percent_encode(name, BENCH_ENCODE_SET)
        ));
    }
    let mut ha = Vec::new();
    {
        let mut href = String::new();
        for name in &names {
            href.clear();
            href.push_str(base_href);
            href.extend(utf8_percent_encode(name, BENCH_ENCODE_SET));
            ha.push(href.clone());
        }
    }
    assert_eq!(hb, ha, "M6 href set differs between BEFORE and AFTER");

    let before = measure(iters, || {
        black_box(m6_before(black_box(base_href), black_box(&names)));
    });
    let after = measure(iters, || {
        black_box(m6_after(black_box(base_href), black_box(&names)));
    });

    println!(
        "\n## [M6] PROPFIND per-child href ({}-child page, reused buffer)",
        names.len()
    );
    header_footer("streaming PROPFIND href build", &before, &after);
    gate_allocs("M6", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [M7] NC extract_url_user — into_owned() vs Cow
// ────────────────────────────────────────────────────────────────────────────

fn m7_before(user_seg: &str, raw_username: &str) -> bool {
    // Shipped-before: force an owned String, then compare.
    match urlencoding::decode(user_seg).ok().map(|s| s.into_owned()) {
        Some(url_user) => url_user != raw_username,
        None => false,
    }
}

fn m7_after(user_seg: &str, raw_username: &str) -> bool {
    // Shipped-after: keep the Cow, compare by slice (zero-alloc on the common
    // no-escape path).
    match urlencoding::decode(user_seg).ok() {
        Some(url_user) => url_user.as_ref() != raw_username,
        None => false,
    }
}

fn section_m7() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let user_seg = "benchuser"; // plain ASCII, decodes to Cow::Borrowed
    let raw_username = "benchuser";

    // Equivalence: same mismatch verdict (here: equal ⇒ false).
    assert_eq!(
        m7_before(user_seg, raw_username),
        m7_after(user_seg, raw_username),
        "M7 verdict differs between BEFORE and AFTER"
    );
    // And on a genuine mismatch.
    assert_eq!(
        m7_before("someone", raw_username),
        m7_after("someone", raw_username),
        "M7 mismatch verdict differs between BEFORE and AFTER"
    );

    let before = measure(iters, || {
        black_box(m7_before(black_box(user_seg), black_box(raw_username)));
    });
    let after = measure(iters, || {
        black_box(m7_after(black_box(user_seg), black_box(raw_username)));
    });

    println!("\n## [M7] NC extract_url_user (Cow, no into_owned)");
    header_footer("path-scoped NC user cross-check", &before, &after);
    gate_allocs("M7", &before, &after);
}

fn main() {
    println!("#################################################################");
    println!("# Round-19 CPU/alloc micro-pack (no Postgres)");
    println!("#################################################################");

    section_m1();
    section_m2();
    section_v1();
    section_v2();
    section_m4();
    section_m5();
    section_m6();
    section_m7();

    println!("\nGATE PASS (all sections)");
}
