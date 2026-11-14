//! Blob-cache index benchmark — `Mutex<LruCache>` vs moka byte-weigher
//! (the ROUND11 deferred lead; no Postgres).
//!
//! `CachedBlobBackend` keeps its cache index in a
//! `tokio::sync::Mutex<LruCache<String, CacheEntry>>`: EVERY cached chunk
//! read acquires the one global async mutex to probe + LRU-promote (the
//! promote needs `&mut`), so N-core read concurrency collapses onto a
//! single serialization domain — and an N-chunk CDC file read is N
//! acquisitions, with every other concurrent reader contending.
//!
//! AFTER: a `moka::sync::Cache` with a byte weigher — lock-free sharded
//! reads with striped recency, byte-budget eviction handled by moka
//! (replacing the manual `current_size` + `collect_evictions` machinery),
//! and an eviction listener that unlinks the evicted `.blob` file (only on
//! size-eviction — Replaced/Explicit must NOT unlink, gated below).
//!
//! Arms:
//!   [1] pure index ops, K tasks × M hit-probes (the scaling ceiling)
//!   [2] end-to-end warm-hit read (index probe + open + 64 KiB read),
//!       K = 1/2/4/8 readers over a shared corpus
//!   [3] safety gates: byte budget enforced + evicted files unlinked +
//!       replaced entries keep their file + single-flight still coalesces
//!       K concurrent misses onto 1 inner fetch
//!
//! Run:
//!   cargo bench --features=bench --bench bench_blob_cache_index
//! Tunables (env): BENCH_OPS (200000), BENCH_FILES (256), BENCH_READERS (8)

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use lru::LruCache;
use tokio::sync::Mutex;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Debug, Clone)]
struct CacheEntry {
    size: u64,
}

/// BEFORE, verbatim: the shipped index shape + the per-hit prologue
/// allocations of `get_blob_stream` (hash `to_string`, `cached_path`
/// build, unconditional `cache_dir.clone()`).
struct BeforeIndex {
    cache_dir: PathBuf,
    index: Arc<Mutex<LruCache<String, CacheEntry>>>,
}

impl BeforeIndex {
    fn cached_path(&self, hash: &str) -> PathBuf {
        let prefix = &hash[..2.min(hash.len())];
        self.cache_dir.join(prefix).join(format!("{hash}.blob"))
    }

    /// The exact hit-path prologue of `get_blob_stream`.
    async fn hit_probe(&self, hash: &str) -> Option<PathBuf> {
        let hash = hash.to_string();
        let cached = self.cached_path(&hash);
        let _cache_dir = self.cache_dir.clone(); // paid on hits, used on misses
        if self.index.lock().await.get(&hash).is_some() {
            return Some(cached);
        }
        None
    }
}

/// AFTER: moka byte-weigher index + borrow-only hit prologue.
struct AfterIndex {
    cache_dir: PathBuf,
    index: moka::sync::Cache<String, CacheEntry>,
}

impl AfterIndex {
    fn new(cache_dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            cache_dir,
            index: moka::sync::Cache::builder()
                .weigher(|_k: &String, e: &CacheEntry| e.size.clamp(1, u32::MAX as u64) as u32)
                .max_capacity(max_bytes)
                .build(),
        }
    }

    fn cached_path(&self, hash: &str) -> PathBuf {
        let prefix = &hash[..2.min(hash.len())];
        self.cache_dir.join(prefix).join(format!("{hash}.blob"))
    }

    fn hit_probe(&self, hash: &str) -> Option<PathBuf> {
        if self.index.get(hash).is_some() {
            return Some(self.cached_path(hash));
        }
        None
    }
}

// ────────────────────────────────────────────────────────────────────────────

async fn section_index_ops(hashes: Arc<Vec<String>>) {
    let ops: usize = env_or("BENCH_OPS", 200_000);
    let readers_max: usize = env_or("BENCH_READERS", 8);

    let before = Arc::new(BeforeIndex {
        cache_dir: PathBuf::from("/tmp/bench-blob-idx"),
        index: Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(1_000_000).unwrap(),
        ))),
    });
    let after = Arc::new(AfterIndex::new(
        PathBuf::from("/tmp/bench-blob-idx"),
        u64::MAX,
    ));
    for h in hashes.iter() {
        before
            .index
            .lock()
            .await
            .put(h.clone(), CacheEntry { size: 1024 });
        after.index.insert(h.clone(), CacheEntry { size: 1024 });
    }

    println!("\n## [1] Pure index hit-probes (ops total = {ops}, split across K tasks)");
    println!("| K | BEFORE Mutex<LruCache> Mops/s | AFTER moka Mops/s | speedup |");
    for k in [1usize, 2, 4, 8].into_iter().filter(|k| *k <= readers_max) {
        let per_task = ops / k;

        let t = Instant::now();
        let mut handles = Vec::new();
        for t_id in 0..k {
            let idx = before.clone();
            let hs = hashes.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..per_task {
                    let h = &hs[(i * 31 + t_id * 7) % hs.len()];
                    std::hint::black_box(idx.hit_probe(h).await);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let before_mops = (per_task * k) as f64 / t.elapsed().as_secs_f64() / 1e6;

        let t = Instant::now();
        let mut handles = Vec::new();
        for t_id in 0..k {
            let idx = after.clone();
            let hs = hashes.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..per_task {
                    let h = &hs[(i * 31 + t_id * 7) % hs.len()];
                    std::hint::black_box(idx.hit_probe(h));
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let after_mops = (per_task * k) as f64 / t.elapsed().as_secs_f64() / 1e6;

        println!(
            "| {k} | {before_mops:>10.2} | {after_mops:>10.2} | {:>6.2}x |",
            after_mops / before_mops
        );
    }
}

async fn section_warm_reads(hashes: Arc<Vec<String>>) {
    let readers_max: usize = env_or("BENCH_READERS", 8);
    let reads: usize = 20_000;

    // Real cached files on disk (64 KiB each).
    let dir = PathBuf::from("/tmp/bench-blob-idx");
    let _ = std::fs::remove_dir_all(&dir);
    let payload = vec![0xA5u8; 64 * 1024];
    let before = Arc::new(BeforeIndex {
        cache_dir: dir.clone(),
        index: Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(1_000_000).unwrap(),
        ))),
    });
    let after = Arc::new(AfterIndex::new(dir.clone(), u64::MAX));
    for h in hashes.iter() {
        let p = before.cached_path(h);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &payload).unwrap();
        before.index.lock().await.put(
            h.clone(),
            CacheEntry {
                size: payload.len() as u64,
            },
        );
        after.index.insert(
            h.clone(),
            CacheEntry {
                size: payload.len() as u64,
            },
        );
    }

    async fn read_file(path: &PathBuf) -> u64 {
        use tokio::io::AsyncReadExt;
        let mut f = tokio::fs::File::open(path).await.unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let n = f.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total
    }

    println!("\n## [2] Warm-hit read (probe + open + 64 KiB read), {reads} reads split across K");
    println!("| K | BEFORE Kops/s | AFTER Kops/s | speedup |");
    for k in [1usize, 2, 4, 8].into_iter().filter(|k| *k <= readers_max) {
        let per_task = reads / k;

        let t = Instant::now();
        let mut handles = Vec::new();
        for t_id in 0..k {
            let idx = before.clone();
            let hs = hashes.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..per_task {
                    let h = &hs[(i * 31 + t_id * 7) % hs.len()];
                    let p = idx.hit_probe(h).await.expect("hit");
                    std::hint::black_box(read_file(&p).await);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let before_kops = (per_task * k) as f64 / t.elapsed().as_secs_f64() / 1e3;

        let t = Instant::now();
        let mut handles = Vec::new();
        for t_id in 0..k {
            let idx = after.clone();
            let hs = hashes.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..per_task {
                    let h = &hs[(i * 31 + t_id * 7) % hs.len()];
                    let p = idx.hit_probe(h).expect("hit");
                    std::hint::black_box(read_file(&p).await);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let after_kops = (per_task * k) as f64 / t.elapsed().as_secs_f64() / 1e3;

        println!(
            "| {k} | {before_kops:>9.1} | {after_kops:>9.1} | {:>6.2}x |",
            after_kops / before_kops
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────

async fn section_safety_gates() {
    use dashmap::DashMap;

    println!("\n## [3] Safety gates");

    // (a) Byte budget + eviction-unlink + replaced-keeps-file, on the moka
    //     shape the production migration ships.
    let dir = PathBuf::from("/tmp/bench-blob-idx-gate");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let unlinked = Arc::new(AtomicU64::new(0));

    let cache_dir = dir.clone();
    let unlinked_l = unlinked.clone();
    let cache: moka::sync::Cache<String, CacheEntry> = moka::sync::Cache::builder()
        .weigher(|_k: &String, e: &CacheEntry| e.size.clamp(1, u32::MAX as u64) as u32)
        .max_capacity(10 * 1024 * 1024) // 10 MiB budget
        .eviction_listener(move |hash: Arc<String>, _entry, cause| {
            // Unlink ONLY blobs moka pushed out for size; a Replaced entry
            // refers to the same path as its replacement, and Explicit
            // removals (delete_blob) unlink at the call site.
            if cause == moka::notification::RemovalCause::Size {
                let prefix = &hash[..2.min(hash.len())];
                let p = cache_dir.join(prefix).join(format!("{hash}.blob"));
                let _ = std::fs::remove_file(&p);
                unlinked_l.fetch_add(1, Ordering::Relaxed);
            }
        })
        .build();

    let payload = vec![0x5Au8; 1024 * 1024]; // 1 MiB blobs
    for i in 0..100 {
        let hash = format!("{i:02x}gatehash{i:04}");
        let prefix = &hash[..2];
        let p = dir.join(prefix).join(format!("{hash}.blob"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &payload).unwrap();
        cache.insert(
            hash,
            CacheEntry {
                size: payload.len() as u64,
            },
        );
    }
    cache.run_pending_tasks();
    let weighted = cache.weighted_size();
    assert!(weighted <= 10 * 1024 * 1024, "budget exceeded: {weighted}");
    // Every surviving entry's file exists; evicted files unlinked.
    let mut on_disk = 0u64;
    for i in 0..100 {
        let hash = format!("{i:02x}gatehash{i:04}");
        let prefix = &hash[..2];
        let p = dir.join(prefix).join(format!("{hash}.blob"));
        let exists = p.exists();
        if cache.get(&hash).is_some() {
            assert!(exists, "surviving entry lost its file: {hash}");
        }
        if exists {
            on_disk += 1;
        }
    }
    assert!(
        on_disk <= 12,
        "disk not trimmed to budget: {on_disk} files remain"
    );
    assert!(unlinked.load(Ordering::Relaxed) >= 88);
    println!(
        "# gate (a) OK — weighted {:.1} MiB ≤ 10 MiB budget, {} files on disk, {} unlinked",
        weighted as f64 / (1024.0 * 1024.0),
        on_disk,
        unlinked.load(Ordering::Relaxed)
    );

    // (b) Replacing an entry must NOT unlink the shared path.
    let u0 = unlinked.load(Ordering::Relaxed);
    let some_hash = cache
        .iter()
        .next()
        .map(|(k, _)| (*k).clone())
        .expect("nonempty");
    let some_path = {
        let prefix = &some_hash[..2.min(some_hash.len())];
        dir.join(prefix).join(format!("{some_hash}.blob"))
    };
    cache.insert(some_hash.clone(), CacheEntry { size: 1024 * 1024 });
    cache.run_pending_tasks();
    assert!(some_path.exists(), "replace unlinked the live file");
    assert_eq!(
        unlinked.load(Ordering::Relaxed),
        u0,
        "replace must not count as size-eviction unlink"
    );
    println!("# gate (b) OK — replaced entry keeps its file");

    // (c) Single-flight (DashMap gate, unchanged by the migration) still
    //     coalesces K concurrent misses to one inner fetch.
    let fetches = Arc::new(AtomicU64::new(0));
    let inflight: Arc<DashMap<String, Arc<Mutex<()>>>> = Arc::new(DashMap::new());
    let done: Arc<moka::sync::Cache<String, CacheEntry>> =
        Arc::new(moka::sync::Cache::builder().max_capacity(1_000_000).build());
    let mut handles = Vec::new();
    for _ in 0..16 {
        let fetches = fetches.clone();
        let inflight = inflight.clone();
        let done = done.clone();
        handles.push(tokio::spawn(async move {
            let hash = "sf-hash".to_string();
            let gate = inflight
                .entry(hash.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone();
            let _guard = gate.lock().await;
            if done.get(&hash).is_some() {
                return;
            }
            // simulate the remote fetch
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            fetches.fetch_add(1, Ordering::Relaxed);
            done.insert(hash.clone(), CacheEntry { size: 1 });
            inflight.remove(&hash);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(fetches.load(Ordering::Relaxed), 1, "single-flight broken");
    println!("# gate (c) OK — 16 concurrent misses → 1 fetch");
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let n_files: usize = env_or("BENCH_FILES", 256);
    let hashes: Arc<Vec<String>> = Arc::new(
        (0..n_files)
            .map(|i| format!("{:02x}benchhash{i:06}", i % 256))
            .collect(),
    );

    println!("#################################################################");
    println!("# Blob-cache index — Mutex<LruCache> vs moka byte-weigher");
    println!("#################################################################");

    section_index_ops(hashes.clone()).await;
    section_warm_reads(hashes.clone()).await;
    section_safety_gates().await;

    println!("\nGATE PASS (safety gates all hold — adopt if [1]/[2] favour moka)");
}
