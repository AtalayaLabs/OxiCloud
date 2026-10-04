use crate::application::ports::file_ports::CacheOutcome;
use crate::common::errors::DomainError;
use bytes::Bytes;
use moka::future::Cache;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tracing::{debug, info};

/// Configuration for the file content cache
#[derive(Debug, Clone)]
pub struct FileContentCacheConfig {
    /// Maximum size of individual files to cache (bytes)
    pub max_file_size: usize,
    /// Maximum total cache size (bytes)
    pub max_total_size: usize,
    /// Maximum number of entries
    pub max_entries: usize,
}

impl Default for FileContentCacheConfig {
    fn default() -> Self {
        Self {
            max_file_size: 10 * 1024 * 1024,   // 10MB max per file
            max_total_size: 512 * 1024 * 1024, // 512MB total cache
            max_entries: 10000,                // Max 10k files
        }
    }
}

impl FileContentCacheConfig {
    /// Create a new configuration with custom values
    pub fn new(max_file_mb: usize, max_total_mb: usize, max_entries: usize) -> Self {
        Self {
            max_file_size: max_file_mb * 1024 * 1024,
            max_total_size: max_total_mb * 1024 * 1024,
            max_entries,
        }
    }
}

/// Cache entry with metadata
#[derive(Clone)]
struct CacheEntry {
    content: Bytes,
    etag: Arc<str>,
    content_type: Arc<str>,
}

/// Lock-free concurrent file content cache backed by `moka`.
///
/// Unlike the previous `lru::LruCache` + `RwLock` design, `moka` uses
/// lock-free reads. Concurrent downloads no longer serialize on a write lock
/// just to update LRU order.
pub struct FileContentCache {
    cache: Cache<String, CacheEntry>,
    config: FileContentCacheConfig,
    hits: AtomicUsize,
    misses: AtomicUsize,
}

impl FileContentCache {
    /// Create a new file content cache with the given configuration
    pub fn new(config: FileContentCacheConfig) -> Self {
        info!(
            "Initializing FileContentCache (moka): max_file={}MB, max_total={}MB, max_entries={}",
            config.max_file_size / (1024 * 1024),
            config.max_total_size / (1024 * 1024),
            config.max_entries
        );

        let cache = Cache::builder()
            .max_capacity(config.max_total_size as u64)
            .weigher(|_key: &String, value: &CacheEntry| -> u32 {
                // Weight = content size.  moka evicts entries when the sum
                // of weights exceeds max_capacity.
                value.content.len().min(u32::MAX as usize) as u32
            })
            .time_to_live(Duration::from_secs(3600))
            .time_to_idle(Duration::from_secs(300))
            .build();

        Self {
            cache,
            config,
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
        }
    }
}

impl Default for FileContentCache {
    fn default() -> Self {
        Self::new(FileContentCacheConfig::default())
    }
}

impl FileContentCache {
    /// Check if a file should be cached based on its size
    pub fn should_cache(&self, size: usize) -> bool {
        size <= self.config.max_file_size
    }

    /// Get file content from cache (lock-free read)
    ///
    /// Returns `(content, etag, content_type)` if found.
    /// All three clones are O(1): `Bytes` and `Arc<str>` only bump a ref count.
    pub async fn get(&self, file_id: &str) -> Option<(Bytes, Arc<str>, Arc<str>)> {
        if let Some(entry) = self.cache.get(file_id).await {
            self.hits.fetch_add(1, Ordering::Relaxed);
            // Mirror the in-process atomic into the Prometheus
            // counter. The `metrics` crate is a no-op when the
            // recorder isn't installed (OXICLOUD_METRICS_LISTEN
            // unset), so this is free on deployments that don't
            // scrape. Counter name follows the convention
            // documented in `interfaces/metrics.rs`:
            // `oxicloud_<subsystem>_<verb>_total`.
            metrics::counter!("oxicloud_content_cache_hits_total").increment(1);
            debug!("Cache HIT for file: {}", file_id);
            Some((
                entry.content.clone(),
                entry.etag.clone(),
                entry.content_type.clone(),
            ))
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("oxicloud_content_cache_misses_total").increment(1);
            debug!("Cache MISS for file: {}", file_id);
            None
        }
    }

    /// Check if file exists in cache without updating LRU order
    pub async fn contains(&self, file_id: &str) -> bool {
        self.cache.contains_key(file_id)
    }

    /// Put file content into cache
    ///
    /// Moka handles eviction automatically based on weight (content size).
    pub async fn put(
        &self,
        file_id: String,
        content: Bytes,
        etag: Arc<str>,
        content_type: Arc<str>,
    ) {
        let size = content.len();

        // Don't cache if too large
        if size > self.config.max_file_size {
            debug!("File {} too large to cache: {} bytes", file_id, size);
            return;
        }

        let entry = CacheEntry {
            content,
            etag,
            content_type,
        };

        self.cache.insert(file_id.clone(), entry).await;
        debug!("Cached file {} ({} bytes)", file_id, size);
    }

    /// Get from cache, or load-and-cache with **single-flight coalescing**.
    ///
    /// On a miss, concurrent callers for the same `cache_key` share ONE `load`
    /// future (moka `try_get_with`) instead of every caller hitting disk — the
    /// classic thundering-herd / cache-stampede fix. With `N` simultaneous
    /// requests for the same uncached blob this turns `N` disk reads into `1`
    /// read plus `N-1` cheap waits, collapsing tail latency under load.
    ///
    /// Safe because the cache is content-addressed (key = immutable blob hash):
    /// the coalesced value is identical for every caller and never goes stale,
    /// so there is nothing to invalidate.
    ///
    /// `etag` / `content_type` describe the loaded content and are only used
    /// when this call is the one that populates the entry.
    pub async fn get_or_load<F>(
        &self,
        cache_key: String,
        etag: Arc<str>,
        content_type: Arc<str>,
        load: F,
    ) -> Result<(Bytes, Arc<str>, Arc<str>, CacheOutcome), DomainError>
    where
        F: Future<Output = Result<Bytes, DomainError>>,
    {
        // Fast path: lock-free hit (also keeps hit/miss stats meaningful).
        if let Some((bytes, etag_hit, mime_hit)) = self.get(&cache_key).await {
            return Ok((bytes, etag_hit, mime_hit, CacheOutcome::Hit));
        }
        self.load_and_cache(cache_key, etag, content_type, load)
            .await
    }

    /// The populate-on-miss half of [`Self::get_or_load`], with single-flight
    /// coalescing but WITHOUT the leading `get` probe.
    ///
    /// Hot read paths that have *already* probed the cache with [`Self::get`]
    /// (a borrow) call this directly on the miss branch — they then build the
    /// owned `cache_key` / `etag` / `content_type` (each a heap allocation)
    /// only when they are actually needed to populate, so a cache HIT allocates
    /// none of them (benches/ROUND29.md §B). Because the caller's own `get`
    /// already counted the hit/miss, this method does not re-probe — keeping the
    /// hit/miss stat counts identical to a single `get_or_load` call.
    pub async fn load_and_cache<F>(
        &self,
        cache_key: String,
        etag: Arc<str>,
        content_type: Arc<str>,
        load: F,
    ) -> Result<(Bytes, Arc<str>, Arc<str>, CacheOutcome), DomainError>
    where
        F: Future<Output = Result<Bytes, DomainError>>,
    {
        // Slow path: coalesce concurrent misses into a single `load`.
        //
        // Timing instrumentation. The loader duration is the single most
        // actionable number for "why is my image slow?" questions —
        // it isolates the backend fetch (local disk / S3 round-trip)
        // from the surrounding handler and streaming overhead. Only
        // the thundering-herd WINNER sees `loaded = true`; followers
        // wait on the same future and record `loaded = false` with
        // the wait duration. Keep this at `info!` so it surfaces by
        // default without a RUST_LOG tweak; the key is on its own
        // target so it can be silenced per-deployment if it ever
        // becomes noisy.
        let key_for_log = cache_key.clone();
        let started = std::time::Instant::now();
        let loaded_in_this_call = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loaded_flag = loaded_in_this_call.clone();
        let entry = self
            .cache
            .try_get_with(cache_key, async move {
                loaded_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                let content = load.await?;
                Ok::<CacheEntry, DomainError>(CacheEntry {
                    content,
                    etag,
                    content_type,
                })
            })
            .await
            // try_get_with hands back `Arc<DomainError>` shared by all waiters;
            // DomainError isn't Clone (it carries a boxed source), so rebuild a
            // fresh one preserving the kind / entity / message.
            .map_err(|shared: Arc<DomainError>| {
                DomainError::new(shared.kind, shared.entity_type, shared.message.clone())
            })?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let did_load = loaded_in_this_call.load(std::sync::atomic::Ordering::Relaxed);
        info!(
            target: "oxicloud::content_cache",
            cache_key = %key_for_log,
            loaded = did_load,
            size_bytes = entry.content.len() as u64,
            duration_ms = elapsed_ms,
            "content served on cache miss"
        );

        // CacheOutcome mapping: either we LOADED the entry (loader ran,
        // bytes came from the backend) → MISS, or we WAITED on a
        // concurrent caller's in-flight load (single-flight follower) →
        // still MISS, because the follower paid the backend's latency
        // by proxy. Labelling a follower as HIT would mislead operators
        // into believing moka was warm when the response time will show
        // backend-fetch cost.
        //
        // `did_load` is kept for the structured log line above but is
        // not currently used to split the outcome. Future refinement
        // (phase 2): a task-local recorder written by
        // `CachedBlobBackend` on each chunk read lets us downgrade this
        // to `HitBackend` when every chunk came off the local
        // `.blob-cache` disk tier instead of the remote. Deferred.
        let _ = did_load;
        let outcome = CacheOutcome::Miss;

        Ok((entry.content, entry.etag, entry.content_type, outcome))
    }

    /// Remove a file from cache (e.g., when file is deleted or modified)
    pub async fn invalidate(&self, file_id: &str) {
        self.cache.remove(file_id).await;
        debug!("Invalidated cache for file: {}", file_id);
    }

    /// Clear the entire cache
    pub async fn clear(&self) {
        self.cache.invalidate_all();
        info!("Cache cleared");
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;
        let hit_rate = if total > 0 {
            (hits as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        CacheStats {
            current_size_bytes: self.cache.weighted_size() as usize,
            max_size_bytes: self.config.max_total_size,
            entries: self.cache.entry_count(),
            hits,
            misses,
            hit_rate_percent: hit_rate,
        }
    }

    /// Spawn a background sampler that publishes two Prometheus
    /// gauges on a 30-second cadence:
    ///
    /// - `oxicloud_content_cache_size_bytes` — moka's weighted-size
    ///   total (sum of entry sizes). The denominator for "how full
    ///   is my cache?".
    /// - `oxicloud_content_cache_entries` — moka's entry count. Useful
    ///   on top of size when watching for pathologies (lots of tiny
    ///   entries vs few huge ones).
    ///
    /// Also mirrors a current-value gauge for the max budget so a
    /// scrape can show capacity-fill ratio without a config lookup:
    ///
    /// - `oxicloud_content_cache_max_size_bytes`
    ///
    /// Gauges follow the convention from `interfaces/metrics.rs`;
    /// the `metrics` crate is a no-op when the recorder isn't
    /// installed, so this task is still cheap on deployments without
    /// `OXICLOUD_METRICS_LISTEN`. Detached — tracks the runtime.
    pub fn spawn_metrics_sampler(self: &Arc<Self>) {
        let this = Arc::clone(self);
        metrics::describe_gauge!(
            "oxicloud_content_cache_size_bytes",
            "Current total weighted size (bytes) of entries in the moka file-content cache."
        );
        metrics::describe_gauge!(
            "oxicloud_content_cache_entries",
            "Current entry count in the moka file-content cache."
        );
        metrics::describe_gauge!(
            "oxicloud_content_cache_max_size_bytes",
            "Configured maximum weighted size (bytes) of the moka file-content cache. \
             Ratio to `oxicloud_content_cache_size_bytes` gives capacity fill."
        );
        metrics::describe_counter!(
            "oxicloud_content_cache_hits_total",
            "File-content cache hits — bytes served from the moka in-memory tier."
        );
        metrics::describe_counter!(
            "oxicloud_content_cache_misses_total",
            "File-content cache misses — the loader ran. Includes single-flight followers."
        );
        tokio::spawn(async move {
            // Immediate first sample so the first scrape after boot
            // doesn't publish zero-initialised placeholders. Matches
            // the pattern in `session_liveness_gauges::spawn`.
            this.publish_gauges();
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // consume the immediate-first tick
            loop {
                ticker.tick().await;
                this.publish_gauges();
            }
        });
        info!("📊 FileContentCache metrics sampler spawned (30s cadence)");
    }

    /// Push the three gauge values for one scrape cycle. Separated
    /// from the task body for testability and so a caller can force
    /// a snapshot ahead of a known-timing test.
    fn publish_gauges(&self) {
        metrics::gauge!("oxicloud_content_cache_size_bytes").set(self.cache.weighted_size() as f64);
        metrics::gauge!("oxicloud_content_cache_entries").set(self.cache.entry_count() as f64);
        metrics::gauge!("oxicloud_content_cache_max_size_bytes")
            .set(self.config.max_total_size as f64);
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub current_size_bytes: usize,
    pub max_size_bytes: usize,
    /// Number of cached entries. Each moka entry is ONE small file
    /// (<10 MB threshold) stored as its assembled bytes keyed by the
    /// file's content hash — not individual chunks. For a 5 MB file
    /// physically split into ~5 chunks on-backend, moka still holds
    /// a single assembled entry here. So the right word on the UI
    /// is "files cached", not "chunks" — mirror of the on-disk
    /// `.blob-cache` tier, which IS per-chunk.
    pub entries: u64,
    pub hits: usize,
    pub misses: usize,
    pub hit_rate_percent: f64,
}

/// Thread-safe wrapper for sharing across handlers
pub type SharedFileContentCache = Arc<FileContentCache>;

// ─── ContentCachePort implementation ─────────────────────────

use crate::application::ports::cache_ports::ContentCachePort;

impl ContentCachePort for FileContentCache {
    fn should_cache(&self, size: usize) -> bool {
        FileContentCache::should_cache(self, size)
    }

    async fn get(&self, file_id: &str) -> Option<(Bytes, Arc<str>, Arc<str>)> {
        FileContentCache::get(self, file_id).await
    }

    async fn put(&self, file_id: String, content: Bytes, etag: Arc<str>, content_type: Arc<str>) {
        FileContentCache::put(self, file_id, content, etag, content_type).await
    }

    async fn invalidate(&self, file_id: &str) {
        FileContentCache::invalidate(self, file_id).await
    }

    async fn clear(&self) {
        FileContentCache::clear(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cache_put_get() {
        let cache = FileContentCache::new(FileContentCacheConfig {
            max_file_size: 1024,
            max_total_size: 4096,
            max_entries: 100,
        });

        let content = Bytes::from("Hello, World!");
        cache
            .put(
                "file1".to_string(),
                content.clone(),
                "etag1".into(),
                "text/plain".into(),
            )
            .await;

        let result = cache.get("file1").await;
        assert!(result.is_some());
        let (cached_content, etag, content_type) = result.unwrap();
        assert_eq!(cached_content, content);
        assert_eq!(&*etag, "etag1");
        assert_eq!(&*content_type, "text/plain");
    }

    #[tokio::test]
    async fn test_cache_eviction() {
        let cache = FileContentCache::new(FileContentCacheConfig {
            max_file_size: 50, // only files ≤ 50 bytes are cacheable
            max_total_size: 1024,
            max_entries: 100,
        });

        // A file within the limit should be cached
        let small = Bytes::from(vec![0u8; 50]);
        cache
            .put("small".to_string(), small, "e1".into(), "app/bin".into())
            .await;
        assert!(cache.get("small").await.is_some());

        // A file exceeding max_file_size is rejected by our own logic
        let big = Bytes::from(vec![1u8; 51]);
        cache
            .put("big".to_string(), big, "e2".into(), "app/bin".into())
            .await;
        assert!(
            cache.get("big").await.is_none(),
            "File exceeding max_file_size must not be cached"
        );

        // Explicit invalidation removes entries immediately
        cache.invalidate("small").await;
        assert!(
            cache.get("small").await.is_none(),
            "Invalidated entry must be gone"
        );
    }

    #[tokio::test]
    async fn test_cache_invalidate() {
        let cache = FileContentCache::new(FileContentCacheConfig::default());

        let content = Bytes::from("test");
        cache
            .put("file1".to_string(), content, "e".into(), "t".into())
            .await;

        assert!(cache.get("file1").await.is_some());

        cache.invalidate("file1").await;

        assert!(cache.get("file1").await.is_none());
    }

    /// Correctness of the stampede fix: N concurrent misses for the same key
    /// must coalesce into exactly ONE load (moka single-flight).
    #[tokio::test]
    async fn get_or_load_coalesces_concurrent_misses() {
        use std::sync::atomic::AtomicUsize;

        let cache = Arc::new(FileContentCache::new(FileContentCacheConfig::default()));
        let loads = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..64 {
            let cache = Arc::clone(&cache);
            let loads = Arc::clone(&loads);
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_load(
                        "blob-hash".to_string(),
                        "\"blob-hash\"".into(),
                        "image/png".into(),
                        async move {
                            loads.fetch_add(1, Ordering::SeqCst);
                            // Slow load so all 64 tasks pile onto the same miss.
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            Ok(Bytes::from_static(b"the-blob-bytes"))
                        },
                    )
                    .await
            }));
        }

        for h in handles {
            let (bytes, _etag, _ct, _outcome) = h.await.unwrap().unwrap();
            assert_eq!(&bytes[..], b"the-blob-bytes");
        }

        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "64 concurrent misses must trigger exactly ONE load (single-flight)"
        );
    }

    /// Before/after benchmark for the cache-stampede fix.
    ///
    /// Run with:
    ///   cargo test --release -p oxicloud bench_stampede -- --ignored --nocapture
    ///
    /// Models a viral hot blob: `K` clients request the same uncached key at
    /// once, and each load contends on a bounded resource (the rayon transcode
    /// pool / DB pool) with `POOL` permits. Reports work amplification and tail
    /// latency for the NAIVE get()+put() pattern vs the COALESCED get_or_load().
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "benchmark — run with --ignored --nocapture"]
    async fn bench_stampede() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Instant;
        use tokio::sync::Semaphore;

        const K: usize = 128; // concurrent clients, all requesting the SAME hot key
        const LOAD_MS: u64 = 30; // cost of one expensive load (disk + decode/encode)
        const POOL: usize = 4; // bounded resource the loads contend on

        // One expensive load: take a permit from the bounded pool, then work.
        async fn expensive_load(
            sem: Arc<Semaphore>,
            loads: Arc<AtomicUsize>,
            load_ms: u64,
        ) -> Bytes {
            let _permit = sem.acquire().await.unwrap();
            loads.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(load_ms)).await;
            Bytes::from_static(b"blob")
        }

        fn pct(sorted: &[u128], p: f64) -> u128 {
            if sorted.is_empty() {
                return 0;
            }
            let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
            sorted[idx]
        }

        // ── Scenario A: NAIVE get() + put() (today's pattern) ──
        let (naive_ms, naive_lats, naive_loads) = {
            let cache = Arc::new(FileContentCache::new(FileContentCacheConfig::default()));
            let sem = Arc::new(Semaphore::new(POOL));
            let loads = Arc::new(AtomicUsize::new(0));
            let t0 = Instant::now();
            let mut handles = Vec::new();
            for _ in 0..K {
                let cache = Arc::clone(&cache);
                let sem = Arc::clone(&sem);
                let loads = Arc::clone(&loads);
                handles.push(tokio::spawn(async move {
                    let r0 = Instant::now();
                    if cache.get("hot").await.is_some() {
                        return r0.elapsed().as_millis();
                    }
                    let bytes = expensive_load(sem, loads, LOAD_MS).await;
                    cache
                        .put("hot".to_string(), bytes, "e".into(), "t".into())
                        .await;
                    r0.elapsed().as_millis()
                }));
            }
            let mut lats = Vec::new();
            for h in handles {
                lats.push(h.await.unwrap());
            }
            lats.sort_unstable();
            (t0.elapsed().as_millis(), lats, loads.load(Ordering::SeqCst))
        };

        // ── Scenario B: COALESCED get_or_load() (the fix) ──
        let (coal_ms, coal_lats, coal_loads) = {
            let cache = Arc::new(FileContentCache::new(FileContentCacheConfig::default()));
            let sem = Arc::new(Semaphore::new(POOL));
            let loads = Arc::new(AtomicUsize::new(0));
            let t0 = Instant::now();
            let mut handles = Vec::new();
            for _ in 0..K {
                let cache = Arc::clone(&cache);
                let sem = Arc::clone(&sem);
                let loads = Arc::clone(&loads);
                handles.push(tokio::spawn(async move {
                    let r0 = Instant::now();
                    cache
                        .get_or_load("hot".to_string(), "e".into(), "t".into(), async move {
                            Ok(expensive_load(sem, loads, LOAD_MS).await)
                        })
                        .await
                        .unwrap();
                    r0.elapsed().as_millis()
                }));
            }
            let mut lats = Vec::new();
            for h in handles {
                lats.push(h.await.unwrap());
            }
            lats.sort_unstable();
            (t0.elapsed().as_millis(), lats, loads.load(Ordering::SeqCst))
        };

        println!(
            "\n╔══ Cache stampede: K={K} clients on the same hot key, pool={POOL}, load={LOAD_MS}ms ══"
        );
        println!("║ pattern                │ loads │ p50(ms) │ p99(ms) │ max(ms) │ wall(ms)");
        println!(
            "║ NAIVE get()+put()      │ {naive_loads:>5} │ {:>7} │ {:>7} │ {:>7} │ {naive_ms:>7}",
            pct(&naive_lats, 0.50),
            pct(&naive_lats, 0.99),
            naive_lats.last().copied().unwrap_or(0)
        );
        println!(
            "║ COALESCED get_or_load  │ {coal_loads:>5} │ {:>7} │ {:>7} │ {:>7} │ {coal_ms:>7}",
            pct(&coal_lats, 0.50),
            pct(&coal_lats, 0.99),
            coal_lats.last().copied().unwrap_or(0)
        );
        let amp = naive_loads as f64 / coal_loads.max(1) as f64;
        let p99x = pct(&naive_lats, 0.99) as f64 / pct(&coal_lats, 0.99).max(1) as f64;
        println!("╚══ {amp:.0}× fewer loads · {p99x:.0}× lower p99 tail latency\n");

        // Guard rails so the benchmark also asserts the win.
        assert_eq!(coal_loads, 1, "coalesced path must load exactly once");
        assert!(
            naive_loads > coal_loads * 10,
            "naive path should stampede the loader"
        );
    }
}
