//! Round-25 CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–24: each section is BEFORE (verbatim replica of the
//! shipped-before shape) vs AFTER (verbatim replica of the shipped-after shape,
//! which the source is then made to match), with a byte/-value equivalence gate
//! and a `GATE FAIL … rollback` check that `std::process::exit(1)`s if the AFTER
//! arm fails to beat its BEFORE — the round's roll-back rule encoded into the
//! benchmark. An AFTER that doesn't win is never applied to the source.
//!
//!   [M1] `EncryptedBlobBackend::decrypt_bytes` decrypts "in place" per its own
//!        doc comment — but `let mut ciphertext = encrypted.split_off(NONCE_SIZE)`
//!        allocates a fresh `Vec` and memcpy's the ENTIRE ciphertext+tag (~1 MiB
//!        per CDC chunk, up to a whole legacy blob) on every decrypted read.
//!        ROUND11 §15 fixed only the encrypt side. AFTER copies the 12-byte nonce
//!        and 16-byte tag to the stack, decrypts the middle in place via
//!        `decrypt_in_place_detached`, and returns a zero-copy `Bytes::slice`
//!        past the nonce — 0 extra allocations, 0 full-payload memcpy. The RAM
//!        win is in BYTES: peak drops from ~2× to ~1× the payload.
//!
//!   [M2] Delta commit (`delta_upload_service::commit_with_perms`) materializes
//!        the per-occurrence chunk-hash list a THIRD time at the manifest bind
//!        (`request.chunks.iter().map(|c| c.h.clone()).collect()`), even though
//!        `request.chunks` is owned and dead after that line. AFTER move-unzips
//!        (`request.chunks.into_iter().map(|c| (c.h, c.s)).unzip()`) — N 64-byte
//!        hash-String clones → 0.
//!
//!   [M3] `folder_handler::download_folder_zip{,_impl}` binds a
//!        `Query<HashMap<String, String>>` as `_params` and discards it — pure
//!        dead work: axum parses the whole query string into a `HashMap` + one
//!        owned `String` key and value per param, all dropped unread. AFTER
//!        removes the extractor (byte-identical response; the handler only reads
//!        the path `id`).
//!
//! Run:
//!   RUSTFLAGS="-C target-cpu=x86-64-v3" \
//!     cargo bench --features=bench --bench bench_round25_micro
//! Tunables (env): BENCH_ITERS (200000), M1_ITERS (2000), CHUNKS (4000),
//!                 PAYLOAD (262144 bytes for the M1 decrypt payload).

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aes_gcm::aead::{AeadInPlace, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use bytes::Bytes;

// ── Counting allocator: tracks BOTH alloc count and total bytes requested ────
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
        // A realloc that grows requests `new_size` fresh bytes.
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

/// Run `f` `iters` times, returning per-op wall ns, alloc count and alloc bytes.
fn measure<T>(iters: u64, mut f: impl FnMut() -> T) -> Measure {
    // warm
    black_box(f());
    ALLOC_CALLS.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
    let start = Instant::now();
    for _ in 0..iters {
        black_box(f());
    }
    let ns = start.elapsed().as_nanos() as f64 / iters as f64;
    let allocs = ALLOC_CALLS.load(Ordering::Relaxed) as f64 / iters as f64;
    let bytes = ALLOC_BYTES.load(Ordering::Relaxed) as f64 / iters as f64;
    Measure { ns, allocs, bytes }
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

/// Roll-back gate: `exit(1)` unless AFTER strictly beats BEFORE on `metric`.
fn gate(tag: &str, metric: &str, before: f64, after: f64) {
    if after >= before {
        eprintln!("GATE FAIL [{tag}] {metric}: AFTER {after} !< BEFORE {before} — rollback");
        std::process::exit(1);
    }
}

const NONCE_SIZE: usize = 12;
const TAG_SIZE: usize = 16;

// ── [M1] EncryptedBlobBackend::decrypt_bytes ─────────────────────────────────
// Build one ciphertext template `[nonce][ciphertext+tag]` and, per iteration,
// clone it (1 alloc, common to both arms) then decrypt via each shape.

fn build_ciphertext(cipher: &Aes256Gcm, plaintext: &[u8]) -> Vec<u8> {
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let mut out = Vec::with_capacity(NONCE_SIZE + plaintext.len() + TAG_SIZE);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(plaintext);
    let tag = cipher
        .encrypt_in_place_detached(&nonce, b"", &mut out[NONCE_SIZE..])
        .expect("encrypt");
    out.extend_from_slice(&tag);
    out
}

/// BEFORE: the shipped `split_off` shape — one fresh Vec + full memcpy.
fn decrypt_before(cipher: &Aes256Gcm, mut encrypted: Vec<u8>) -> Bytes {
    let mut ciphertext = encrypted.split_off(NONCE_SIZE);
    let nonce = Nonce::from_slice(&encrypted);
    cipher
        .decrypt_in_place(nonce, b"", &mut ciphertext)
        .expect("decrypt");
    Bytes::from(ciphertext)
}

/// AFTER: decrypt the middle in place, return a zero-copy slice past the nonce.
fn decrypt_after(cipher: &Aes256Gcm, mut encrypted: Vec<u8>) -> Bytes {
    let len = encrypted.len();
    let mut nonce_buf = [0u8; NONCE_SIZE];
    nonce_buf.copy_from_slice(&encrypted[..NONCE_SIZE]);
    let nonce = Nonce::from_slice(&nonce_buf);
    let tag = aes_gcm::aead::Tag::<Aes256Gcm>::clone_from_slice(&encrypted[len - TAG_SIZE..]);
    cipher
        .decrypt_in_place_detached(nonce, b"", &mut encrypted[NONCE_SIZE..len - TAG_SIZE], &tag)
        .expect("decrypt");
    encrypted.truncate(len - TAG_SIZE);
    Bytes::from(encrypted).slice(NONCE_SIZE..)
}

fn section_m1() {
    let iters: u64 = env_or("M1_ITERS", 2000);
    let payload_len: usize = env_or("PAYLOAD", 262_144);
    let key = [7u8; 32];
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let plaintext: Vec<u8> = (0..payload_len).map(|i| (i * 31 + 7) as u8).collect();
    let template = build_ciphertext(&cipher, &plaintext);

    // Equivalence: both arms recover the exact plaintext.
    let a = decrypt_before(&cipher, template.clone());
    let b = decrypt_after(&cipher, template.clone());
    assert_eq!(
        a.as_ref(),
        plaintext.as_slice(),
        "M1 BEFORE plaintext mismatch"
    );
    assert_eq!(
        b.as_ref(),
        plaintext.as_slice(),
        "M1 AFTER plaintext mismatch"
    );
    assert_eq!(a, b, "M1 arms disagree");

    let before = measure(iters, || decrypt_before(&cipher, template.clone()));
    let after = measure(iters, || decrypt_after(&cipher, template.clone()));
    report(
        &format!("[M1] decrypt_bytes in place ({payload_len}-byte payload)"),
        before,
        after,
    );
    // The RAM win: AFTER must allocate strictly fewer bytes (no ciphertext copy).
    gate("M1", "bytes/op", before.bytes, after.bytes);
}

// ── [M2] Delta commit chunk-hash list: clone vs move-unzip ───────────────────
struct ChunkRefRep {
    h: String,
    s: u64,
}

fn hex64(i: usize) -> String {
    // 64-char hex, deterministic — mirrors a BLAKE3 chunk hash string.
    let mut s = String::with_capacity(64);
    for k in 0..32 {
        use std::fmt::Write;
        let _ = write!(
            s,
            "{:02x}",
            (i.wrapping_mul(2_654_435_761).wrapping_add(k)) as u8
        );
    }
    s
}

fn section_m2() {
    let n: usize = env_or("CHUNKS", 4000);
    let iters: u64 = env_or("M2_ITERS", 400);

    // Equivalence check on one build.
    let build = || -> Vec<ChunkRefRep> {
        (0..n)
            .map(|i| ChunkRefRep {
                h: hex64(i),
                s: (i as u64) * 7,
            })
            .collect()
    };
    let cb = build();
    let before_h: Vec<String> = cb.iter().map(|c| c.h.clone()).collect();
    let before_s: Vec<u64> = cb.iter().map(|c| c.s).collect();
    let (after_h, after_s): (Vec<String>, Vec<u64>) =
        build().into_iter().map(|c| (c.h, c.s)).unzip();
    assert_eq!(before_h, after_h, "M2 hash arms differ");
    assert_eq!(before_s, after_s, "M2 size arms differ");

    let before = measure(iters, || {
        let chunks = build();
        let hh: Vec<String> = chunks.iter().map(|c| c.h.clone()).collect();
        let ss: Vec<u64> = chunks.iter().map(|c| c.s).collect();
        (hh, ss)
    });
    let after = measure(iters, || {
        let chunks = build();
        let (hh, ss): (Vec<String>, Vec<u64>) = chunks.into_iter().map(|c| (c.h, c.s)).unzip();
        (hh, ss)
    });
    report(
        &format!("[M2] delta-commit chunk-hash list ({n} chunks)"),
        before,
        after,
    );
    gate("M2", "allocs/op", before.allocs, after.allocs);
}

// ── [M3] folder_handler dead Query<HashMap> ──────────────────────────────────
// Replicates axum's `Query<HashMap<String,String>>` extraction (build an owned
// key+value map from the query string) vs no extractor.
fn parse_query_map(q: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            m.insert(k.to_string(), v.to_string());
        }
    }
    m
}

fn section_m3() {
    let iters: u64 = env_or("BENCH_ITERS", 200_000);
    // A representative query string a client might append (cache-buster etc.).
    let q = "folder_id=8c1f0e2a-1234-4a5b-9c8d-abcdef012345&t=1720000000";

    // Equivalence: the handler only ever needs the path id, never these params.
    let before_map = parse_query_map(q);
    assert!(before_map.contains_key("folder_id"), "M3 setup");

    let before = measure(iters, || {
        // BEFORE: axum builds and drops the map on every request.
        let m = parse_query_map(black_box(q));
        black_box(m.len())
    });
    let after = measure(iters, || {
        // AFTER: no extractor — nothing parsed.
        black_box(())
    });
    report("[M3] folder download dead Query<HashMap>", before, after);
    gate("M3", "allocs/op", before.allocs, after.allocs);
}

fn main() {
    println!("# Round-25 micro alloc/RAM pack\n");
    section_m1();
    section_m2();
    section_m3();
    println!("All Round-25 micro sections passed their gate.");
}
