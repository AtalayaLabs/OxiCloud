//! Round-17 dedup + CardDAV CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–16: each section is BEFORE (verbatim replica of the
//! shipped-before shape) vs AFTER (verbatim replica of the shipped-after shape,
//! or the shipped helper where reachable), with a byte/-value equivalence gate
//! and a `GATE FAIL … rollback` check that exits non-zero if the AFTER arm
//! fails to reduce allocations — the round's roll-back rule encoded into the
//! benchmark.
//!
//!   [D1] `BlobHandler::hash_chunk_sequence` (delta-commit verification) took
//!        `chunks: &[(String, u64)]` and fed the backend stream via
//!        `chunks.iter().cloned()` — re-allocating every chunk-hash String a
//!        second time, on top of the owned `Vec` the caller already built with
//!        `c.h.clone()`. Taking the `Vec` by value and `into_iter()`-ing it
//!        moves those Strings in: zero internal clones.
//!   [D2] The chunk-ingest loop (`store_from_stream`) allocated the 64-char hex
//!        hash String THREE times per chunk: `to_hex().to_string()`, then
//!        `chunk_hashes.push(hash.clone())`, then `session_seen.insert(hash
//!        .clone())` — the last dropped immediately on a duplicate. Keying the
//!        intra-upload dedup set on the raw 32-byte BLAKE3 digest (`[u8; 32]`,
//!        `Copy`, no heap) drops the set clone entirely, and moving the hex into
//!        `chunk_hashes` on the duplicate branch drops the manifest clone there:
//!        3 → 2 allocs (new chunk) / 3 → 1 (duplicate), on the hottest write
//!        path in the dedup system.
//!   [V1] `contact_to_vcard` emitted every EMAIL/TEL/ADR `TYPE=` token via
//!        `ty.to_uppercase()` — one throw-away String per token per vCard. The
//!        `push_upper` helper writes the upper-cased chars straight into the
//!        vCard buffer: zero temporaries.
//!
//! Run:
//!   cargo run --release --features bench --example bench_round17_micro
//! Tunables (env): BENCH_ITERS (100000), BENCH_CHUNKS (64), BENCH_DUP_RATIO (2)

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::env;
use std::fmt::Write as _;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bytes::Bytes;

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
        "| {:<46} | {:>12.1} | {:>10.2} |",
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
// [D1] hash_chunk_sequence — `&[..]` + iter().cloned() vs `Vec` by value
// ────────────────────────────────────────────────────────────────────────────
//
// The caller (`delta_upload_service::commit`) already owns a fresh
// `Vec<(String, u64)>` built with `c.h.clone()`; that `.collect()` is identical
// on both call shapes, so it is EXCLUDED from the comparison. The delta is what
// `hash_chunk_sequence` does INTERNALLY to feed `futures::stream::iter(..)`:
//   BEFORE — `chunks.iter().cloned()` re-clones every (String, u64) → N String
//            allocations (+ the collected Vec) inside the function.
//   AFTER  — the `Vec` is moved in and `into_iter()`- d → the Strings relocate
//            with zero heap traffic; the function iterates the owned pairs.
// The streamed (hash, size) pairs are byte-identical, so the recomputed BLAKE3
// and every size check are unchanged — only the ownership differs.

fn d1_before_internal(chunks: &[(String, u64)]) -> Vec<(String, u64)> {
    // Materialises `stream::iter(chunks.iter().cloned())`'s input — the same N
    // element clones + one Vec the old `&[..]` signature forced. `to_vec()` is
    // `iter().cloned().collect()` (identical allocations), spelled the way
    // clippy prefers.
    chunks.to_vec()
}

fn section_hash_chunk_sequence() {
    let iters: usize = env_or("BENCH_ITERS", 100_000);
    let n: usize = env_or("BENCH_CHUNKS", 64);

    // A realistic manifest: N distinct 64-hex chunk hashes + declared sizes.
    let base: Vec<(String, u64)> = (0..n)
        .map(|i| {
            let h = blake3::hash(format!("d1-chunk-{i}").as_bytes())
                .to_hex()
                .to_string();
            (h, 1024 + i as u64)
        })
        .collect();

    // Gate: the old internal clone is a pure copy — moving instead changes
    // nothing the function observes (same pairs, same order).
    assert_eq!(
        d1_before_internal(&base),
        base,
        "d1 clone is not a pure copy"
    );

    let before = measure(iters, || {
        // The internal re-clone the `&[..]` signature forced.
        black_box(d1_before_internal(black_box(&base)));
    });
    let after = measure(iters, || {
        // The by-value signature adds no internal copy — it consumes the moved
        // pairs (modelled here as an in-order read of the same owned pairs).
        for c in black_box(&base).iter() {
            black_box(c);
        }
    });

    println!("\n## [D1] hash_chunk_sequence internal clone ({n} chunks/op)");
    header_footer("hash_chunk_sequence by-value", &before, &after);
    gate_allocs("D1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [D2] chunk-ingest loop — 3 hash-String allocs/chunk vs 2 (new) / 1 (dup)
// ────────────────────────────────────────────────────────────────────────────

struct IngestOut {
    chunk_hashes: Vec<String>,
    chunk_sizes: Vec<u64>,
    /// The distinct hashes that would be written to the backend, in order.
    pending: Vec<String>,
}

/// BEFORE: verbatim replica of the shipped-before loop body.
fn d2_before(payloads: &[Bytes]) -> IngestOut {
    let mut chunk_hashes: Vec<String> = Vec::new();
    let mut chunk_sizes: Vec<u64> = Vec::new();
    let mut session_seen: HashSet<String> = HashSet::new();
    let mut pending: Vec<(String, Bytes)> = Vec::new();

    for data in payloads {
        let hash = blake3::hash(data).to_hex().to_string();
        chunk_sizes.push(data.len() as u64);
        chunk_hashes.push(hash.clone());
        if session_seen.insert(hash.clone()) {
            pending.push((hash, data.clone()));
        }
    }
    IngestOut {
        chunk_hashes,
        chunk_sizes,
        pending: pending.into_iter().map(|(h, _)| h).collect(),
    }
}

/// AFTER: verbatim replica of the shipped-after loop body — the dedup set keys
/// on the raw 32-byte digest, and the manifest push is split across the
/// new/duplicate branches so a duplicate moves (not clones) the hex in.
fn d2_after(payloads: &[Bytes]) -> IngestOut {
    let mut chunk_hashes: Vec<String> = Vec::new();
    let mut chunk_sizes: Vec<u64> = Vec::new();
    let mut session_seen: HashSet<[u8; 32]> = HashSet::new();
    let mut pending: Vec<(String, Bytes)> = Vec::new();

    for data in payloads {
        let digest = blake3::hash(data);
        let hash = digest.to_hex().to_string();
        chunk_sizes.push(data.len() as u64);
        if session_seen.insert(*digest.as_bytes()) {
            chunk_hashes.push(hash.clone());
            pending.push((hash, data.clone()));
        } else {
            chunk_hashes.push(hash);
        }
    }
    IngestOut {
        chunk_hashes,
        chunk_sizes,
        pending: pending.into_iter().map(|(h, _)| h).collect(),
    }
}

fn section_chunk_ingest() {
    let iters: usize = env_or("BENCH_ITERS", 100_000);
    let n: usize = env_or("BENCH_CHUNKS", 64);
    // 1-in-K chunks repeats an earlier one (models intra-file dedup: repeated
    // blocks, zero-padded regions, re-chunked near-duplicates). K=2 ⇒ ~half the
    // stream is duplicate, the case a dedup store exists to make cheap.
    let dup_ratio: usize = env_or("BENCH_DUP_RATIO", 2).max(1);

    let payloads: Vec<Bytes> = (0..n)
        .map(|i| {
            let key = if dup_ratio > 0 && i % dup_ratio == 0 && i >= dup_ratio {
                i - dup_ratio // repeat an earlier chunk's bytes
            } else {
                i
            };
            Bytes::from(format!("d2-chunk-payload-{key}-{}", "x".repeat(256)))
        })
        .collect();

    // Gate: identical observable output — the ordered manifest, the sizes, and
    // the distinct write set are byte-for-byte equal (only the private set's key
    // representation differs).
    let b = d2_before(&payloads);
    let a = d2_after(&payloads);
    assert_eq!(b.chunk_hashes, a.chunk_hashes, "d2 manifest differs");
    assert_eq!(b.chunk_sizes, a.chunk_sizes, "d2 sizes differ");
    assert_eq!(b.pending, a.pending, "d2 write-set differs");

    let before = measure(iters, || {
        black_box(d2_before(black_box(&payloads)));
    });
    let after = measure(iters, || {
        black_box(d2_after(black_box(&payloads)));
    });

    println!("\n## [D2] chunk-ingest hash allocs ({n} chunks/op, 1-in-{dup_ratio} dup)");
    header_footer("chunk-ingest session_seen [u8;32]", &before, &after);
    gate_allocs("D2", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [V1] contact_to_vcard TYPE tokens — per-token to_uppercase() String vs push
// ────────────────────────────────────────────────────────────────────────────

/// AFTER helper: write the upper-cased form of `s` straight into `buf`.
/// Uses `char::to_uppercase`, so the bytes are identical to `s.to_uppercase()`.
fn push_upper(buf: &mut String, s: &str) {
    for c in s.chars() {
        for u in c.to_uppercase() {
            buf.push(u);
        }
    }
}

/// BEFORE: verbatim replica — `write!` the `to_uppercase()` temporary.
fn v1_before(types: &[&str]) -> String {
    let mut vcard = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");
    for ty in types {
        let _ = write!(vcard, "EMAIL;TYPE={}:x@e.test\r\n", ty.to_uppercase());
    }
    vcard
}

/// AFTER: verbatim replica of the shipped-after emit — push pieces + upper.
fn v1_after(types: &[&str]) -> String {
    let mut vcard = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");
    for ty in types {
        vcard.push_str("EMAIL;TYPE=");
        push_upper(&mut vcard, ty);
        vcard.push_str(":x@e.test\r\n");
    }
    vcard
}

fn section_vcard_types() {
    let iters: usize = env_or("BENCH_ITERS", 100_000);
    // A contact's worth of EMAIL/TEL/ADR type tokens (already-upper, lower,
    // mixed, and an x- extension — the shapes real address books carry).
    let types = [
        "HOME", "work", "Cell", "voice", "fax", "x-custom", "WORK", "home",
    ];

    // Gate: byte-identical vCard, and push_upper == str::to_uppercase per token.
    for ty in types {
        let mut got = String::new();
        push_upper(&mut got, ty);
        assert_eq!(got, ty.to_uppercase(), "push_upper differs for {ty:?}");
    }
    assert_eq!(v1_before(&types), v1_after(&types), "v1 vcard differs");

    let before = measure(iters, || {
        black_box(v1_before(black_box(&types)));
    });
    let after = measure(iters, || {
        black_box(v1_after(black_box(&types)));
    });

    println!(
        "\n## [V1] contact_to_vcard TYPE tokens ({} tokens/op)",
        types.len()
    );
    header_footer("vcard TYPE push_upper", &before, &after);
    gate_allocs("V1", &before, &after);
}

fn main() {
    println!("#################################################################");
    println!("# Round-17 dedup + CardDAV CPU/alloc micro-pack");
    println!("#################################################################");

    section_hash_chunk_sequence();
    section_chunk_ingest();
    section_vcard_types();

    println!("\nGATE PASS (all sections)");
}
