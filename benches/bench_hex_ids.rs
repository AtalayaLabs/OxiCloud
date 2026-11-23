//! Micro-alloc benchmark: digest-hex rendering and NC id-batch marshalling.
//!
//! Two round-6 changes, both equivalence-gated against their verbatim
//! BEFORE shapes and measured with a counting allocator:
//!
//! 1. `IncrementalHasher::finalize_hex` (upload_ingest.rs) rendered MD5 /
//!    SHA-256 digests with `.map(|b| format!("{b:02x}")).collect()` — one
//!    heap `String` per digest byte (16 / 32 allocs) per chunk finalize.
//!    AFTER: `common::fmt::hex_lower` writes into one preallocated String.
//!
//! 2. `batch_resolve_ids` (NC webdav_handler) cloned every child id into a
//!    `Vec<String>` and the id service keyed its result map by `String` —
//!    ~3 heap allocs per child per page. AFTER the whole chain is borrowed:
//!    `Vec<&str>` in, `HashMap<Uuid, i64>` out, `Uuid::parse_str` lookups.
//!
//! Run:
//!   cargo bench --features=bench --bench bench_hex_ids
//! Tunables (env): BENCH_ITERS (10000), BENCH_CHILDREN (500).

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use md5::Digest;
use oxicloud::common::fmt::hex_lower;
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

fn measure<R>(f: impl FnOnce() -> R) -> (R, u64, f64) {
    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    let r = f();
    let el = t.elapsed().as_secs_f64();
    let allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;
    (r, allocs, el)
}

// ── 1. digest hex ───────────────────────────────────────────────────────────

/// BEFORE, verbatim: one `format!` per digest byte.
fn hex_before(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn bench_hex(iters: usize) {
    // Deterministic digests of both production sizes (MD5=16, SHA-256=32).
    let md5s: Vec<[u8; 16]> = (0..64u64)
        .map(|i| md5::Md5::digest(i.to_le_bytes()).into())
        .collect();
    let sha256s: Vec<[u8; 32]> = (0..64u64)
        .map(|i| sha2::Sha256::digest(i.to_le_bytes()).into())
        .collect();

    // Equivalence gate: byte-identical output on every digest.
    for d in &md5s {
        assert_eq!(hex_lower(d), hex_before(d), "md5 hex mismatch");
    }
    for d in &sha256s {
        assert_eq!(hex_lower(d), hex_before(d), "sha256 hex mismatch");
    }

    println!("── finalize_hex: per-byte format! vs hex_lower ({iters} finalizes/arm) ──\n");
    println!(
        "| {:<8} | {:<8} | {:>12} | {:>10} | {:>12} |",
        "digest", "arm", "allocs", "wall ms", "allocs/call"
    );
    for (label, digests) in [("md5", md5s.len()), ("sha256", sha256s.len())] {
        for arm in ["BEFORE", "AFTER"] {
            let (sink, allocs, secs) = measure(|| {
                let mut sink = 0usize;
                for i in 0..iters {
                    let s = match (label, arm) {
                        ("md5", "BEFORE") => hex_before(&md5s[i % digests]),
                        ("md5", "AFTER") => hex_lower(&md5s[i % digests]),
                        ("sha256", "BEFORE") => hex_before(&sha256s[i % digests]),
                        _ => hex_lower(&sha256s[i % digests]),
                    };
                    sink += s.len();
                }
                sink
            });
            std::hint::black_box(sink);
            println!(
                "| {:<8} | {:<8} | {:>12} | {:>10.2} | {:>12.2} |",
                label,
                arm,
                allocs,
                secs * 1e3,
                allocs as f64 / iters as f64
            );
        }
    }
}

// ── 2. NC id-batch marshalling ──────────────────────────────────────────────

/// BEFORE, verbatim caller+service marshalling: clone ids into `Vec<String>`,
/// key the result map by cloned `String`, look children up by `&String`.
fn ids_before(child_ids: &[String], nc: &HashMap<Uuid, i64>) -> Vec<Option<i64>> {
    let file_uuids: Vec<String> = child_ids.to_vec();
    let mut map: HashMap<String, i64> = HashMap::with_capacity(file_uuids.len());
    for raw in &file_uuids {
        let Ok(uuid) = Uuid::parse_str(raw) else {
            continue;
        };
        if let Some(id) = nc.get(&uuid) {
            map.insert(raw.clone(), *id);
        }
    }
    child_ids.iter().map(|id| map.get(id).copied()).collect()
}

/// AFTER: borrowed slice in, `Uuid`-keyed map out, parse-and-get lookups —
/// the exact shapes now in `batch_resolve_ids` + `nc_id_of`.
fn ids_after(child_ids: &[String], nc: &HashMap<Uuid, i64>) -> Vec<Option<i64>> {
    let file_uuids: Vec<&str> = child_ids.iter().map(String::as_str).collect();
    let mut map: HashMap<Uuid, i64> = HashMap::with_capacity(file_uuids.len());
    for raw in &file_uuids {
        let Ok(uuid) = Uuid::parse_str(raw) else {
            continue;
        };
        if let Some(id) = nc.get(&uuid) {
            map.insert(uuid, *id);
        }
    }
    child_ids
        .iter()
        .map(|id| Uuid::parse_str(id).ok().and_then(|u| map.get(&u).copied()))
        .collect()
}

fn bench_ids(pages: usize, children: usize) {
    // A PROPFIND page of `children` DTO ids (36-byte uuid strings) resolved
    // against the id service's numeric mapping.
    let uuids: Vec<Uuid> = (0..children).map(|_| Uuid::new_v4()).collect();
    let child_ids: Vec<String> = uuids.iter().map(|u| u.to_string()).collect();
    let nc: HashMap<Uuid, i64> = uuids
        .iter()
        .enumerate()
        .map(|(i, u)| (*u, i as i64 + 1000))
        .collect();

    // Equivalence gate: identical per-child resolution, including an
    // unparseable id and an unmapped-but-valid id.
    let mut gate_ids = child_ids.clone();
    gate_ids.push("not-a-uuid".to_string());
    gate_ids.push(Uuid::new_v4().to_string());
    assert_eq!(
        ids_before(&gate_ids, &nc),
        ids_after(&gate_ids, &nc),
        "id resolution mismatch"
    );

    println!("\n── batch_resolve_ids marshalling: String-keyed vs borrowed+Uuid ──");
    println!("   ({pages} pages × {children} children/arm)\n");
    println!(
        "| {:<8} | {:>12} | {:>10} | {:>14} |",
        "arm", "allocs", "wall ms", "allocs/child"
    );
    for arm in ["BEFORE", "AFTER"] {
        let (sink, allocs, secs) = measure(|| {
            let mut sink = 0usize;
            for _ in 0..pages {
                let resolved = if arm == "BEFORE" {
                    ids_before(&child_ids, &nc)
                } else {
                    ids_after(&child_ids, &nc)
                };
                sink += resolved.iter().flatten().count();
            }
            sink
        });
        assert_eq!(sink, pages * children, "all children must resolve");
        println!(
            "| {:<8} | {:>12} | {:>10.2} | {:>14.3} |",
            arm,
            allocs,
            secs * 1e3,
            allocs as f64 / (pages * children) as f64
        );
    }
}

fn main() {
    let iters: usize = env_or("BENCH_ITERS", 10_000);
    let children: usize = env_or("BENCH_CHILDREN", 500);

    bench_hex(iters);
    bench_ids(iters / 10, children);

    println!("\n(BEFORE arms are verbatim replicas of the replaced shapes; equivalence");
    println!(" asserted before timing. Allocs counted via a wrapping GlobalAlloc.)");
}
