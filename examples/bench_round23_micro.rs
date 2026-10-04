//! Round-23 CPU/alloc micro-pack (no Postgres) — the deterministic alloc gates
//! for the decode / clone candidates. The end-to-end PostgreSQL latency +
//! equivalence evidence lives in `bench_round23_queries.rs`.
//!
//! Same rule as ROUND2–22: each section is BEFORE (verbatim replica of the
//! shipped-before shape) vs AFTER (verbatim replica of the shipped-after shape,
//! which the source is then made to match), with a byte/-value equivalence gate
//! and a `GATE FAIL … rollback` check that `std::process::exit(1)`s if the AFTER
//! arm fails to beat its BEFORE.
//!
//!   [J1] `contact_pg_repository::row_to_contact` (+ the `contact_group`
//!        sibling) decoded each JSONB column with `row.get::<serde_json::Value>`
//!        + `serde_json::from_value::<Vec<Dto>>` — a throwaway `Value` DOM per
//!        column, walked a second time. AFTER decodes straight into the typed
//!        Vec via `sqlx::types::Json<T>` (one `from_slice` pass). Modeled here
//!        as `from_slice::<Value>` + `from_value` vs `from_slice::<Vec<Dto>>`.
//!
//!   [J2] `DrivePolicies::from_value` did `serde_json::from_value(value.clone())`
//!        — cloning the ENTIRE policies DOM per drive-policy read. AFTER
//!        deserializes from the borrow (`T::deserialize(&Value)`), no clone.
//!
//!   [U1] `blob_handler` (`store_loose_chunks` final registration + the ingest
//!        `run_rollback`) built `Vec<String>`/`Vec<i64>` by CLONING every hash
//!        out of an owned, dead-after `Vec<(String,i64)>` purely to reshape for
//!        `sync_blobs(&[String])` + the UNNEST bind. AFTER moves via
//!        `into_iter().unzip()`.
//!
//! Run:
//!   cargo run --release --features bench --example bench_round23_micro
//! Tunables (env): BENCH_ITERS (200000), J1_ROWS (3), U1_CHUNKS (256)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
// [J1] Contact JSONB decode — Value DOM + from_value vs Json<T> from_slice.
// Verbatim replicas of the persistence DTOs (contact_persistence_dto.rs).
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EmailDto {
    email: String,
    r#type: String,
    is_primary: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PhoneDto {
    number: String,
    r#type: String,
    is_primary: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AddressDto {
    street: Option<String>,
    city: Option<String>,
    state: Option<String>,
    postal_code: Option<String>,
    country: Option<String>,
    r#type: String,
    is_primary: bool,
}

/// BEFORE: `row.get::<Value>` (sqlx JSONB→Value DOM) then `from_value::<Vec<T>>`
/// (a second walk of the DOM). Modeled with `from_slice::<Value>` (what sqlx's
/// Value decoder does) + `from_value`.
fn j1_before(
    email: &[u8],
    phone: &[u8],
    addr: &[u8],
) -> (Vec<EmailDto>, Vec<PhoneDto>, Vec<AddressDto>) {
    let ev: Value = serde_json::from_slice(email).unwrap();
    let pv: Value = serde_json::from_slice(phone).unwrap();
    let av: Value = serde_json::from_slice(addr).unwrap();
    let emails = serde_json::from_value::<Vec<EmailDto>>(ev).unwrap_or_default();
    let phones = serde_json::from_value::<Vec<PhoneDto>>(pv).unwrap_or_default();
    let addrs = serde_json::from_value::<Vec<AddressDto>>(av).unwrap_or_default();
    (emails, phones, addrs)
}

/// AFTER: `sqlx::types::Json<Vec<T>>` decodes the JSONB bytes straight into the
/// typed Vec (one `from_slice::<Vec<T>>`), no intermediate DOM.
fn j1_after(
    email: &[u8],
    phone: &[u8],
    addr: &[u8],
) -> (Vec<EmailDto>, Vec<PhoneDto>, Vec<AddressDto>) {
    let emails = serde_json::from_slice::<Vec<EmailDto>>(email).unwrap_or_default();
    let phones = serde_json::from_slice::<Vec<PhoneDto>>(phone).unwrap_or_default();
    let addrs = serde_json::from_slice::<Vec<AddressDto>>(addr).unwrap_or_default();
    (emails, phones, addrs)
}

fn section_j1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let n: usize = env_or("J1_ROWS", 3); // entries per column, realistic contact

    let mk_emails = |n: usize| -> Vec<EmailDto> {
        (0..n)
            .map(|i| EmailDto {
                email: format!("user{i}@example.com"),
                r#type: if i == 0 { "home" } else { "work" }.to_string(),
                is_primary: i == 0,
            })
            .collect()
    };
    let mk_phones = |n: usize| -> Vec<PhoneDto> {
        (0..n)
            .map(|i| PhoneDto {
                number: format!("+1-555-010{i}"),
                r#type: "cell".to_string(),
                is_primary: i == 0,
            })
            .collect()
    };
    let mk_addrs = |n: usize| -> Vec<AddressDto> {
        (0..n)
            .map(|i| AddressDto {
                street: Some(format!("{} Main St", 100 + i)),
                city: Some("Springfield".to_string()),
                state: Some("IL".to_string()),
                postal_code: Some("62704".to_string()),
                country: Some("US".to_string()),
                r#type: "home".to_string(),
                is_primary: i == 0,
            })
            .collect()
    };

    let email_b = serde_json::to_vec(&mk_emails(n)).unwrap();
    let phone_b = serde_json::to_vec(&mk_phones(n)).unwrap();
    let addr_b = serde_json::to_vec(&mk_addrs(n)).unwrap();

    // Equivalence: identical decoded Vecs.
    assert_eq!(
        j1_before(&email_b, &phone_b, &addr_b),
        j1_after(&email_b, &phone_b, &addr_b),
        "J1 decoded contacts differ"
    );

    let before = measure(iters, || {
        black_box(j1_before(
            black_box(&email_b),
            black_box(&phone_b),
            black_box(&addr_b),
        ));
    });
    let after = measure(iters, || {
        black_box(j1_after(
            black_box(&email_b),
            black_box(&phone_b),
            black_box(&addr_b),
        ));
    });

    println!(
        "\n## [J1] Contact JSONB decode ({n} entries/col — per contact row of every list/multiget/sync)"
    );
    header_footer(
        "Value DOM + from_value vs Json<T> from_slice",
        &before,
        &after,
    );
    gate_allocs("J1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [J2] Drive policies decode — from_value(value.clone()) vs deserialize(&value).
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
struct Policies {
    forbid_public_links: bool,
    read_only: bool,
}

/// BEFORE: clone the whole `Value` DOM, then `from_value`.
fn j2_before(value: &Value) -> Policies {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// AFTER: deserialize straight from the borrow — no DOM clone.
fn j2_after(value: &Value) -> Policies {
    Policies::deserialize(value).unwrap_or_default()
}

fn section_j2() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // A realistic on-disk policies bag with an unknown key preserved on disk
    // (the lenient contract) so the DOM isn't trivially tiny.
    let value: Value = serde_json::from_str(
        r#"{"forbid_public_links":true,"read_only":false,"x_future_flag":"kept-on-disk"}"#,
    )
    .unwrap();

    assert_eq!(
        j2_before(&value),
        j2_after(&value),
        "J2 decoded policies differ"
    );
    assert!(j2_after(&value).forbid_public_links);

    let before = measure(iters, || {
        black_box(j2_before(black_box(&value)));
    });
    let after = measure(iters, || {
        black_box(j2_after(black_box(&value)));
    });

    println!("\n## [J2] Drive policies decode (per move/copy/share/grant drive-policy read)");
    header_footer(
        "from_value(value.clone()) vs deserialize(&value)",
        &before,
        &after,
    );
    gate_allocs("J2", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [U1] dedup hash reshape — clone-collect vs into_iter().unzip().
// ────────────────────────────────────────────────────────────────────────────

fn u1_build(n: usize) -> Vec<(String, i64)> {
    (0..n)
        .map(|i| {
            (
                format!("{:064x}", i as u128 * 0x9E37_79B9_7F4A_7C15),
                i as i64,
            )
        })
        .collect()
}

/// BEFORE: clone every hash out of the owned (dead-after) Vec to reshape.
fn u1_before(rows: Vec<(String, i64)>) -> (Vec<String>, Vec<i64>) {
    let hashes: Vec<String> = rows.iter().map(|(h, _)| h.clone()).collect();
    let sizes: Vec<i64> = rows.iter().map(|(_, s)| *s).collect();
    (hashes, sizes)
}

/// AFTER: move via unzip — no per-hash content copy.
fn u1_after(rows: Vec<(String, i64)>) -> (Vec<String>, Vec<i64>) {
    rows.into_iter().unzip()
}

fn section_u1() {
    let n: usize = env_or("U1_CHUNKS", 256);
    let iters: usize = env_or("BENCH_ITERS", 200_000) / 20; // heavier op

    // Equivalence: identical hashes + sizes.
    assert_eq!(
        u1_before(u1_build(n)),
        u1_after(u1_build(n)),
        "U1 reshape differs"
    );

    let before = measure(iters, || {
        black_box(u1_before(black_box(u1_build(n))));
    });
    let after = measure(iters, || {
        black_box(u1_after(black_box(u1_build(n))));
    });

    println!(
        "\n## [U1] dedup hash reshape ({n} distinct new chunks — per delta-upload registration)"
    );
    header_footer("clone-collect vs into_iter().unzip()", &before, &after);
    gate_allocs("U1", &before, &after);
}

fn main() {
    println!("# Round-23 micro-pack — BEFORE/AFTER (counting allocator, release)");
    println!("# allocs/op is the deterministic gate; a non-winning AFTER exits 1 (rollback).");
    section_j1();
    section_j2();
    section_u1();
    println!("\nAll Round-23 micro sections passed their allocation gate.");
}
