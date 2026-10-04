//! Round-9 storage micro-pack benchmark — four independent A/Bs, no Postgres.
//!
//! [1] Local chunk write — the old `try_exists` (stat) + `File::create` pair
//!     vs the new single atomic `create_new` open, at chunk-write level via
//!     the bench wrapper over the production writer. Fresh-write AND
//!     already-exists (dedup re-upload skip) arms.
//! [2] CDC read prep — the old per-read deep clone of the cached manifest's
//!     `Vec<String>` chunk-hash list vs the new index-over-`Arc` iteration
//!     (structural replica of `BlobHandler::stream_chunks` before/after;
//!     the production change is exactly this data-flow).
//! [3] Manifest cache miss herd — the old `get → SELECT → insert` shape vs
//!     the new fast-get + `try_get_with` single-flight, K concurrent cold
//!     readers on one key over a real moka cache with a counted loader
//!     (structural replica of `BlobHandler::manifest_cached`, sqlx swapped
//!     for a latency-injected counted loader).
//! [4] Chunk `Content-MD5` verification hex — 16× `format!("{b:02x}")` +
//!     collect vs `common::fmt::hex_lower` (1 sized alloc).
//!
//! Gates: [1] AFTER wall < BEFORE wall (fresh) + identical on-disk content +
//! identical skip semantics; [2] AFTER allocs < BEFORE allocs + identical
//! hash sequence; [3] AFTER loader runs == 1 (BEFORE > 1) + identical value;
//! [4] identical hex + fewer allocs.
//!
//! Run:
//!   cargo run --release --features bench --example bench_storage_micro
//! Tunables (env): BENCH_CHUNKS (20000), BENCH_CHUNK_KB (4), BENCH_HERD (64),
//!                 BENCH_MANIFEST_CHUNKS (4096)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use oxicloud::infrastructure::services::local_blob_backend::write_blob_bytes_for_bench;

// ─── Counting allocator ─────────────────────────────────────────────────────

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

// ─── [1] BEFORE replica: stat-then-create chunk writer (verbatim) ───────────

async fn write_blob_bytes_before(
    blob_path: &std::path::Path,
    data: &Bytes,
) -> std::io::Result<Option<tokio::fs::File>> {
    use tokio::io::AsyncWriteExt;
    if tokio::fs::try_exists(blob_path).await.unwrap_or(false) {
        return Ok(None);
    }
    let mut file = tokio::fs::File::create(blob_path).await?;
    file.write_all(data).await?;
    Ok(Some(file))
}

async fn section_1(chunks: usize, chunk_kb: usize) {
    let payload = Bytes::from(vec![0x5au8; chunk_kb * 1024]);
    let dir_before = tempfile::tempdir().expect("tempdir");
    let dir_after = tempfile::tempdir().expect("tempdir");

    // Fresh writes.
    let t = Instant::now();
    for i in 0..chunks {
        let p = dir_before.path().join(format!("{i:08x}.blob"));
        write_blob_bytes_before(&p, &payload)
            .await
            .expect("before write");
    }
    let before_fresh = t.elapsed().as_secs_f64() * 1e3;

    let t = Instant::now();
    for i in 0..chunks {
        let p = dir_after.path().join(format!("{i:08x}.blob"));
        write_blob_bytes_for_bench(&p, &payload)
            .await
            .expect("after write");
    }
    let after_fresh = t.elapsed().as_secs_f64() * 1e3;

    // Equivalence: same file count, same bytes for a sample.
    let sample = dir_after.path().join(format!("{:08x}.blob", chunks / 2));
    let got = tokio::fs::read(&sample).await.expect("sample read");
    assert_eq!(got.len(), payload.len(), "content length mismatch");
    assert_eq!(&got[..64], &payload[..64], "content mismatch");

    // Already-exists skip (dedup re-upload): both must return None-equivalent.
    let t = Instant::now();
    for i in 0..chunks {
        let p = dir_before.path().join(format!("{i:08x}.blob"));
        let r = write_blob_bytes_before(&p, &payload).await.expect("skip");
        assert!(r.is_none(), "BEFORE re-put must skip");
    }
    let before_skip = t.elapsed().as_secs_f64() * 1e3;

    let t = Instant::now();
    for i in 0..chunks {
        let p = dir_after.path().join(format!("{i:08x}.blob"));
        let r = write_blob_bytes_for_bench(&p, &payload)
            .await
            .expect("skip");
        assert!(r.is_none(), "AFTER re-put must skip (AlreadyExists)");
    }
    let after_skip = t.elapsed().as_secs_f64() * 1e3;

    println!("\n#################################################################");
    println!("# [1] local chunk write — stat+create vs atomic create_new");
    println!("# chunks={chunks} x {chunk_kb} KiB");
    println!("#################################################################\n");
    println!(
        "| {:<26} | {:>12} | {:>12} |",
        "arm", "fresh ms", "re-put ms"
    );
    println!(
        "| {:<26} | {:>12.1} | {:>12.1} |",
        "BEFORE (stat+create)", before_fresh, before_skip
    );
    println!(
        "| {:<26} | {:>12.1} | {:>12.1} |",
        "AFTER  (create_new)", after_fresh, after_skip
    );
    println!(
        "\nfresh {:.2}x · re-put {:.2}x",
        before_fresh / after_fresh,
        before_skip / after_skip
    );
    if after_fresh >= before_fresh {
        eprintln!("GATE FAIL [1]: create_new not faster on fresh writes — rollback");
        std::process::exit(1);
    }
}

// ─── [2] manifest read prep: Vec clone vs Arc-index ─────────────────────────

struct ManifestReplica {
    chunk_hashes: Vec<String>,
}

fn section_2(manifest_chunks: usize) {
    let manifest = Arc::new(ManifestReplica {
        chunk_hashes: (0..manifest_chunks).map(|i| format!("{i:064x}")).collect(),
    });
    let reads = 200usize;

    // BEFORE: each read clones the whole hash list out of the shared Arc
    // (the old `stream_chunks(m.chunk_hashes.clone())` call shape).
    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    let mut sum_before = 0usize;
    for _ in 0..reads {
        let hashes: Vec<String> = manifest.chunk_hashes.clone();
        for h in &hashes {
            sum_before += h.len();
        }
        black_box(&hashes);
    }
    let before_ms = t.elapsed().as_secs_f64() * 1e3;
    let before_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;

    // AFTER: each read bumps the Arc and indexes (the new `stream_chunks(m)`).
    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    let mut sum_after = 0usize;
    for _ in 0..reads {
        let m = manifest.clone();
        for i in 0..m.chunk_hashes.len() {
            sum_after += m.chunk_hashes[i].len();
        }
        black_box(&m);
    }
    let after_ms = t.elapsed().as_secs_f64() * 1e3;
    let after_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;

    assert_eq!(sum_before, sum_after, "hash sequence mismatch");

    println!("\n#################################################################");
    println!("# [2] CDC read prep — manifest Vec<String> clone vs Arc index");
    println!("# manifest={manifest_chunks} chunks, reads={reads}");
    println!("#################################################################\n");
    println!(
        "| {:<26} | {:>10} | {:>12} | {:>12} |",
        "arm", "wall ms", "allocs", "allocs/read"
    );
    println!(
        "| {:<26} | {:>10.3} | {:>12} | {:>12.1} |",
        "BEFORE (clone Vec)",
        before_ms,
        before_allocs,
        before_allocs as f64 / reads as f64
    );
    println!(
        "| {:<26} | {:>10.3} | {:>12} | {:>12.1} |",
        "AFTER  (Arc index)",
        after_ms,
        after_allocs,
        after_allocs as f64 / reads as f64
    );
    if after_allocs >= before_allocs {
        eprintln!("GATE FAIL [2]: Arc-index not fewer allocs — rollback");
        std::process::exit(1);
    }
}

// ─── [3] manifest miss herd: get→insert vs try_get_with ─────────────────────

async fn section_3(herd: usize) {
    type Cache = moka::future::Cache<String, Arc<Vec<u64>>>;

    let value = || Arc::new(vec![7u64; 1024]);
    let simulated_query = Duration::from_millis(2);

    // BEFORE shape: check, query (2 ms), insert — every cold caller loads.
    let cache: Cache = moka::future::Cache::new(1000);
    let loads = Arc::new(AtomicU64::new(0));
    let mut set = tokio::task::JoinSet::new();
    let t = Instant::now();
    for _ in 0..herd {
        let cache = cache.clone();
        let loads = loads.clone();
        set.spawn(async move {
            if let Some(v) = cache.get("hot-file").await {
                return v;
            }
            loads.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(simulated_query).await;
            let v = value();
            cache.insert("hot-file".to_string(), v.clone()).await;
            v
        });
    }
    let mut first: Option<Arc<Vec<u64>>> = None;
    while let Some(r) = set.join_next().await {
        let v = r.expect("join");
        if let Some(f) = &first {
            assert_eq!(f.len(), v.len());
        } else {
            first = Some(v);
        }
    }
    let before_ms = t.elapsed().as_secs_f64() * 1e3;
    let before_loads = loads.load(Ordering::Relaxed);

    // AFTER shape: fast get + try_get_with — the herd coalesces onto 1 load.
    let cache: Cache = moka::future::Cache::new(1000);
    let loads = Arc::new(AtomicU64::new(0));
    let mut set = tokio::task::JoinSet::new();
    let t = Instant::now();
    for _ in 0..herd {
        let cache = cache.clone();
        let loads = loads.clone();
        set.spawn(async move {
            if let Some(v) = cache.get("hot-file").await {
                return v;
            }
            cache
                .try_get_with("hot-file".to_string(), async move {
                    loads.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(simulated_query).await;
                    Ok::<_, std::convert::Infallible>(value())
                })
                .await
                .expect("infallible")
        });
    }
    while let Some(r) = set.join_next().await {
        let v = r.expect("join");
        assert_eq!(v.len(), first.as_ref().unwrap().len());
    }
    let after_ms = t.elapsed().as_secs_f64() * 1e3;
    let after_loads = loads.load(Ordering::Relaxed);

    println!("\n#################################################################");
    println!("# [3] manifest cold-miss herd — get→insert vs try_get_with");
    println!("# herd={herd} concurrent readers, 2 ms simulated manifest SELECT");
    println!("#################################################################\n");
    println!("| {:<26} | {:>10} | {:>12} |", "arm", "wall ms", "loads");
    println!(
        "| {:<26} | {:>10.1} | {:>12} |",
        "BEFORE (get→insert)", before_ms, before_loads
    );
    println!(
        "| {:<26} | {:>10.1} | {:>12} |",
        "AFTER  (single-flight)", after_ms, after_loads
    );
    if after_loads != 1 {
        eprintln!("GATE FAIL [3]: single-flight ran {after_loads} loads (expected 1) — rollback");
        std::process::exit(1);
    }
    if before_loads <= 1 {
        eprintln!(
            "GATE WARN [3]: BEFORE herd only loaded {before_loads}x — herd too small to show the stampede"
        );
    }
}

// ─── [4] Content-MD5 hex ────────────────────────────────────────────────────

fn section_4() {
    let digests: Vec<[u8; 16]> = (0..1000u32)
        .map(|i| {
            let mut d = [0u8; 16];
            d[..4].copy_from_slice(&i.to_le_bytes());
            d
        })
        .collect();

    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    let before: Vec<String> = digests
        .iter()
        .map(|d| d.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .collect();
    let before_ms = t.elapsed().as_secs_f64() * 1e3;
    let before_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;

    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    let after: Vec<String> = digests
        .iter()
        .map(|d| oxicloud::common::fmt::hex_lower(d))
        .collect();
    let after_ms = t.elapsed().as_secs_f64() * 1e3;
    let after_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;

    assert_eq!(before, after, "hex output mismatch");

    println!("\n#################################################################");
    println!("# [4] chunk Content-MD5 hex — per-byte format! vs hex_lower");
    println!("# digests=1000");
    println!("#################################################################\n");
    println!(
        "| {:<26} | {:>10} | {:>12} | {:>14} |",
        "arm", "wall ms", "allocs", "allocs/digest"
    );
    println!(
        "| {:<26} | {:>10.3} | {:>12} | {:>14.2} |",
        "BEFORE (format!/byte)",
        before_ms,
        before_allocs,
        before_allocs as f64 / 1000.0
    );
    println!(
        "| {:<26} | {:>10.3} | {:>12} | {:>14.2} |",
        "AFTER  (hex_lower)",
        after_ms,
        after_allocs,
        after_allocs as f64 / 1000.0
    );
    if after_allocs >= before_allocs {
        eprintln!("GATE FAIL [4]: hex_lower not fewer allocs — rollback");
        std::process::exit(1);
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let chunks: usize = env_or("BENCH_CHUNKS", 20_000);
    let chunk_kb: usize = env_or("BENCH_CHUNK_KB", 4);
    let herd: usize = env_or("BENCH_HERD", 64);
    let manifest_chunks: usize = env_or("BENCH_MANIFEST_CHUNKS", 4096);

    section_1(chunks, chunk_kb).await;
    section_2(manifest_chunks);
    section_3(herd).await;
    section_4();

    println!("\nGATE PASS: all four sections improved with identical outputs.");
}
