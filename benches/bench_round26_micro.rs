//! Round-26 CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–25: each section is BEFORE (verbatim replica of the
//! shipped-before shape) vs AFTER (replica of the shipped-after shape, which the
//! source is then made to match), with a value-equivalence gate and a
//! `GATE FAIL … rollback` `std::process::exit(1)` if the AFTER arm fails to beat
//! BEFORE — the round's roll-back rule encoded into the benchmark.
//!
//!   [P1] `drive_pg_repository`'s four policy reads decode `d.policies` into a
//!        throwaway `serde_json::Value` DOM and then call
//!        `DrivePolicies::from_value(&raw)` (`Self::deserialize(&Value)`) — the
//!        exact throwaway-DOM pattern ROUND23 §J1 removed for contacts, but left
//!        on the drive-policy path (§J2 removed only the `from_value` clone). The
//!        Value tree (a `Map` + boxed String key + `Value` node per policy field)
//!        is walked once and dropped. AFTER decodes straight into the struct via
//!        `serde_json::from_slice::<DrivePolicies>` (what `sqlx::types::Json<T>`
//!        runs on the raw JSONB bytes) — no intermediate DOM. The lenient
//!        `unwrap_or_default` fallback is preserved.
//!
//! Run:
//!   RUSTFLAGS="-C target-cpu=x86-64-v3" \
//!     cargo bench --features=bench --bench bench_round26_micro
//! Tunables (env): P1_ITERS (200000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use oxicloud::domain::entities::drive::DrivePolicies;
use serde::Deserialize as _;

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
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

#[derive(Clone, Copy)]
struct Measure {
    ns: f64,
    allocs: f64,
    bytes: f64,
}

fn measure<T>(iters: u64, mut f: impl FnMut() -> T) -> Measure {
    black_box(f());
    ALLOC_CALLS.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
    let start = Instant::now();
    for _ in 0..iters {
        black_box(f());
    }
    let ns = start.elapsed().as_nanos() as f64 / iters as f64;
    Measure {
        ns,
        allocs: ALLOC_CALLS.load(Ordering::Relaxed) as f64 / iters as f64,
        bytes: ALLOC_BYTES.load(Ordering::Relaxed) as f64 / iters as f64,
    }
}

fn report(tag: &str, before: Measure, after: Measure) {
    println!("## {tag}");
    println!("| arm    |        ns/op |   allocs/op |    bytes/op |");
    println!(
        "| BEFORE | {:>12.1} | {:>11.2} | {:>11.0} |",
        before.ns, before.allocs, before.bytes
    );
    println!(
        "| AFTER  | {:>12.1} | {:>11.2} | {:>11.0} |",
        after.ns, after.allocs, after.bytes
    );
    println!(
        "# {:.2}x wall · {:.2} fewer allocs/op · {:.0} fewer bytes/op\n",
        before.ns / after.ns.max(0.0001),
        before.allocs - after.allocs,
        before.bytes - after.bytes
    );
}

fn gate(tag: &str, metric: &str, before: f64, after: f64) {
    if after >= before {
        eprintln!("GATE FAIL [{tag}] {metric}: AFTER {after} !< BEFORE {before} — rollback");
        std::process::exit(1);
    }
}

// ── [P1] drive-policy JSONB decode: Value DOM + from_value vs from_slice<T> ───
fn section_p1() {
    let iters: u64 = env_or("P1_ITERS", 200_000);
    // A realistically-populated policies bag (several fields set); the column is
    // `jsonb NOT NULL DEFAULT '{}'`, and `#[serde(default)]` fills the rest.
    let json: &[u8] = br#"{"forbid_sharing":true,"forbid_public_links":true,"include_in_photo_index":true,"read_only":false,"forbid_cross_drive_move":true}"#;

    // Equivalence: both arms yield the identical DrivePolicies.
    let before_val: serde_json::Value = serde_json::from_slice(json).unwrap();
    let before = DrivePolicies::deserialize(&before_val).unwrap_or_default();
    let after = serde_json::from_slice::<DrivePolicies>(json).unwrap_or_default();
    assert_eq!(before, after, "P1 decoded policies differ");

    let b = measure(iters, || {
        // BEFORE: raw JSONB → full serde_json::Value DOM → deserialize(&Value).
        let v: serde_json::Value = serde_json::from_slice(black_box(json)).unwrap();
        DrivePolicies::deserialize(&v).unwrap_or_default()
    });
    let a = measure(iters, || {
        // AFTER: raw JSONB → from_slice::<DrivePolicies> (what sqlx Json<T> does).
        serde_json::from_slice::<DrivePolicies>(black_box(json)).unwrap_or_default()
    });
    report(
        "[P1] drive-policy JSONB decode (Value DOM vs from_slice)",
        b,
        a,
    );
    gate("P1", "allocs/op", b.allocs, a.allocs);
}

fn main() {
    println!("# Round-26 micro alloc pack\n");
    section_p1();
    println!("All Round-26 micro sections passed their gate.");
}
