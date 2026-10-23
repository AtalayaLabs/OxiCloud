//! OCS capabilities poll benchmark — rebuild-per-request vs memoized bytes.
//!
//! `/ocs/v{1,2}.php/cloud/capabilities` returns a payload that is
//! process-invariant (pure config: base URL + emulated NC version), yet
//! every NC desktop/mobile client polls it on connect and periodically.
//! The old handler re-built the ~40-node `json!` tree — including a
//! `std::env::var("OXICLOUD_BASE_URL")` lookup and three `format!`s —
//! and re-serialized it on EVERY poll. Round 9 serializes both versions
//! once into a `OnceLock<[Bytes; 2]>`; a poll is a `Bytes` refcount bump.
//!
//! The BEFORE arm is the production payload builder invoked per request
//! (via the bench wrapper) + `serde_json::to_vec`, exactly the old
//! handler flow (`Json(payload)` serializes with `to_vec`). The AFTER
//! arm is the memoized-bytes flow. The equivalence gate asserts the
//! served bytes are identical.
//!
//! Run (no Postgres needed):
//!   cargo run --release --features bench --example bench_capabilities_static
//! Tunables (env): BENCH_POLLS (50000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bytes::Bytes;
use oxicloud::interfaces::nextcloud::ocs_handler::capabilities_payload_for_bench;

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

const EMULATED: (u32, u32, u32) = (28, 0, 4);
const VERSION_STRING: &str = "28.0.4";
const MAX_CHUNK_SIZE: u64 = 100 * 1024 * 1024;

/// BEFORE flow, verbatim shape: env lookup + tree build + serialize per poll.
fn before_poll(ocs_version: u8) -> Vec<u8> {
    let base_url =
        env::var("OXICLOUD_BASE_URL").unwrap_or_else(|_| "http://localhost:8086".to_string());
    let payload = capabilities_payload_for_bench(
        &base_url,
        EMULATED,
        VERSION_STRING,
        MAX_CHUNK_SIZE,
        ocs_version,
    );
    serde_json::to_vec(&payload).expect("serialize")
}

/// AFTER flow: the production memoization shape (OnceLock + Bytes clone).
fn after_poll(cache: &OnceLock<[Bytes; 2]>, ocs_version: u8) -> Bytes {
    let bodies = cache.get_or_init(|| {
        let base_url =
            env::var("OXICLOUD_BASE_URL").unwrap_or_else(|_| "http://localhost:8086".to_string());
        [1u8, 2u8].map(|v| {
            Bytes::from(
                serde_json::to_vec(&capabilities_payload_for_bench(
                    &base_url,
                    EMULATED,
                    VERSION_STRING,
                    MAX_CHUNK_SIZE,
                    v,
                ))
                .expect("serialize"),
            )
        })
    });
    bodies[usize::from(ocs_version != 1)].clone()
}

fn main() {
    let polls: usize = env_or("BENCH_POLLS", 50_000);
    let cache: OnceLock<[Bytes; 2]> = OnceLock::new();

    // Equivalence gate: identical served bytes for both OCS versions.
    for v in [1u8, 2u8] {
        assert_eq!(
            before_poll(v),
            after_poll(&cache, v).as_ref(),
            "capabilities v{v} bytes differ"
        );
    }
    println!("# equivalence gate: v1 + v2 served bytes identical — OK");

    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for i in 0..polls {
        black_box(before_poll(if i % 2 == 0 { 1 } else { 2 }));
    }
    let before_ms = t.elapsed().as_secs_f64() * 1e3;
    let before_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;

    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for i in 0..polls {
        black_box(after_poll(&cache, if i % 2 == 0 { 1 } else { 2 }));
    }
    let after_ms = t.elapsed().as_secs_f64() * 1e3;
    let after_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;

    println!("\n#################################################################");
    println!("# OCS capabilities poll — rebuild+serialize vs memoized Bytes");
    println!("# polls={polls}");
    println!("#################################################################\n");
    println!(
        "| {:<26} | {:>10} | {:>12} | {:>12} |",
        "arm", "wall ms", "allocs", "allocs/poll"
    );
    println!(
        "| {:<26} | {:>10.1} | {:>12} | {:>12.2} |",
        "BEFORE (rebuild)",
        before_ms,
        before_allocs,
        before_allocs as f64 / polls as f64
    );
    println!(
        "| {:<26} | {:>10.1} | {:>12} | {:>12.2} |",
        "AFTER  (memoized)",
        after_ms,
        after_allocs,
        after_allocs as f64 / polls as f64
    );
    println!(
        "\n{:.1}x faster, {:.0}x fewer allocs",
        before_ms / after_ms,
        before_allocs as f64 / after_allocs.max(1) as f64
    );

    if after_ms >= before_ms || after_allocs >= before_allocs {
        eprintln!("GATE FAIL: memoized arm not strictly better — rollback");
        std::process::exit(1);
    }
    println!("GATE PASS");
}
