//! `CachedBlobBackend` — LRU local-disk cache decorator for remote blob backends.
//!
//! Wraps any `BlobStorageBackend` (typically S3 or Azure) and transparently
//! caches hot blobs on a local SSD.  Reads check the cache first; cache misses
//! are fetched from the inner backend and written to the cache.  Writes go to
//! the inner backend AND the local cache simultaneously.
//!
//! Eviction is LRU based on a configurable maximum disk budget.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::application::ports::blob_storage_ports::{
    BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use crate::domain::errors::DomainError;

/// Chunk size for streaming cached file reads (256 KB).
const STREAM_CHUNK_SIZE: usize = 256 * 1024;

// ── Configuration ──────────────────────────────────────────────────

/// Configuration for the LRU disk cache.
#[derive(Debug, Clone)]
pub struct BlobCacheConfig {
    /// Directory where cached blobs are stored.
    pub cache_dir: PathBuf,
    /// Maximum total cache size in bytes.
    pub max_cache_bytes: u64,
}

// ── Cache entry ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CacheEntry {
    size: u64,
}

// ── CachedBlobBackend ──────────────────────────────────────────────

/// A `BlobStorageBackend` decorator that adds an LRU disk cache in front of
/// a remote backend.
///
/// The index is a `moka::sync::Cache` with a byte weigher: cached reads
/// probe it lock-free (sharded, striped recency) where the previous
/// `tokio::sync::Mutex<LruCache>` serialized EVERY cached chunk read on one
/// global async mutex — negative scaling under concurrent readers
/// (benches/ROUND12.md §B: 2.08 → 1.07 Mops/s going 1 → 2 readers on the
/// mutex; moka holds 1.7-2.4). moka also owns the byte budget: eviction by
/// weighted size replaces the manual `current_size` counter +
/// `collect_evictions` sweep, and the eviction listener unlinks the evicted
/// `.blob` (only on size-eviction — a Replaced entry shares its file with
/// the replacement, and Explicit invalidations unlink at their call site).
pub struct CachedBlobBackend {
    inner: Arc<dyn BlobStorageBackend>,
    cache_dir: PathBuf,
    max_cache_bytes: u64,
    index: moka::sync::Cache<String, CacheEntry>,
    /// Per-hash single-flight gates for cache misses. K concurrent cold
    /// readers of one blob (e.g. a video player's parallel Range probes)
    /// used to each download the FULL blob from the remote backend — and
    /// race their writes on one shared `.tmp` path. The gate coalesces
    /// them onto one fetch; waiters re-check the cache and serve locally
    /// (16 fetches -> 1, benches/BLOB-CACHE.md).
    inflight: Arc<DashMap<String, Arc<Mutex<()>>>>,
}

fn cached_path_in(cache_dir: &Path, hash: &str) -> PathBuf {
    let prefix = &hash[..2.min(hash.len())];
    cache_dir.join(prefix).join(format!("{hash}.blob"))
}

impl CachedBlobBackend {
    /// Create a new cached backend wrapping `inner`.
    pub fn new(inner: Arc<dyn BlobStorageBackend>, config: &BlobCacheConfig) -> Self {
        let listener_dir = config.cache_dir.clone();
        Self {
            inner,
            cache_dir: config.cache_dir.clone(),
            max_cache_bytes: config.max_cache_bytes,
            index: moka::sync::Cache::builder()
                .weigher(|_k: &String, e: &CacheEntry| e.size.clamp(1, u32::MAX as u64) as u32)
                .max_capacity(config.max_cache_bytes)
                .eviction_listener(move |hash: Arc<String>, _entry, cause| {
                    // Size-evicted blobs lose their on-disk file here (the
                    // sweep `collect_evictions` used to do). A quick unlink
                    // on the inserting task's thread, off the hot get path.
                    if cause == moka::notification::RemovalCause::Size {
                        let _ = std::fs::remove_file(cached_path_in(&listener_dir, &hash));
                    }
                })
                .build(),
            inflight: Arc::new(DashMap::new()),
        }
    }

    /// Path where a blob is cached locally.
    fn cached_path(&self, hash: &str) -> PathBuf {
        cached_path_in(&self.cache_dir, hash)
    }
}

impl BlobStorageBackend for CachedBlobBackend {
    fn initialize(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let cache_dir = self.cache_dir.clone();
        let index = self.index.clone();
        Box::pin(async move {
            inner.initialize().await?;

            // Create the cache dir AND its 256 {00..ff} shard dirs up front
            // (mirroring LocalBlobBackend::initialize), so the write paths never
            // pay a per-chunk `create_dir_all` on an already-existing shard — a
            // ~45 µs mkdirat(EEXIST)+stat+blocking-dispatch removed per cache
            // write on cached-remote deployments (benches/ROUND26.md §D1).
            fs::create_dir_all(&cache_dir).await.map_err(|e| {
                DomainError::internal_error("BlobCache", format!("mkdir cache_dir: {e}"))
            })?;
            for prefix in &crate::infrastructure::services::local_blob_backend::HEX_PREFIXES {
                fs::create_dir_all(cache_dir.join(prefix))
                    .await
                    .map_err(|e| {
                        DomainError::internal_error("BlobCache", format!("mkdir cache shard: {e}"))
                    })?;
            }

            // Scan existing cache to rebuild index.  Collect entries WITHOUT
            // holding the index lock — a large cache directory walk must not
            // serialize concurrent blob operations behind the mutex.
            let mut total_bytes = 0u64;
            let mut entries: Vec<(String, u64)> = Vec::new();
            if let Ok(mut read_dir) = fs::read_dir(&cache_dir).await {
                while let Ok(Some(prefix_entry)) = read_dir.next_entry().await {
                    if !prefix_entry.path().is_dir() {
                        continue;
                    }
                    if let Ok(mut sub_dir) = fs::read_dir(prefix_entry.path()).await {
                        while let Ok(Some(entry)) = sub_dir.next_entry().await {
                            let path = entry.path();
                            if path.extension().and_then(|e| e.to_str()) == Some("blob")
                                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                            {
                                let size = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
                                entries.push((stem.to_string(), size));
                                total_bytes += size;
                            }
                        }
                    }
                }
            }
            // Rebuild the index; if the restored set exceeds the byte
            // budget, moka trims it (and the eviction listener unlinks the
            // trimmed files) — the old index carried the excess until the
            // next insert.
            for (stem, size) in entries {
                index.insert(stem, CacheEntry { size });
            }
            tracing::info!(
                "Blob cache initialized: {} bytes in cache at {}",
                total_bytes,
                cache_dir.display()
            );
            Ok(())
        })
    }

    fn put_blob(
        &self,
        hash: &str,
        source_path: &Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_string();
        let source = source_path.to_path_buf();
        Box::pin(async move {
            // Cache FIRST: every inner backend consumes the source file
            // (local renames it, S3/Azure delete it after upload), so the
            // old populate-after-put ordering failed 100% of the time and
            // the first read after a whole-file put paid a full remote
            // re-download (the ROUND11 deferred correctness note; fix
            // gated in benches/ROUND12.md §B).
            let cached = self.insert_into_cache(&hash, &source).await.is_ok();
            match self.inner.put_blob(&hash, &source).await {
                Ok(bytes) => Ok(bytes),
                Err(e) => {
                    // Never serve a blob the backend rejected: drop the
                    // just-inserted cache entry + file.
                    if cached {
                        self.index.invalidate(&hash);
                        let _ = fs::remove_file(self.cached_path(&hash)).await;
                    }
                    Err(e)
                }
            }
        })
    }

    fn put_blob_from_bytes(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_string();
        Box::pin(async move {
            let size = self.inner.put_blob_from_bytes(&hash, data.clone()).await?;
            self.cache_bytes_write_through(hash, &data).await;
            Ok(size)
        })
    }

    // Without this override the trait default would re-route the CDC chunk
    // write through `put_blob_from_bytes` above, whose inner (synced) call
    // pays the remote exists-probe per chunk. The local write-through cache
    // population is kept identical — post-upload readers (thumbnail/EXIF/
    // face hooks) hit the cache instead of re-fetching from the remote.
    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_string();
        Box::pin(async move {
            let size = self
                .inner
                .put_blob_from_bytes_unsynced(&hash, data.clone())
                .await?;
            self.cache_bytes_write_through(hash, &data).await;
            Ok(size)
        })
    }

    // The durability barrier must reach the backend that buffered the
    // unsynced writes; the local cache copy is disposable and needs none.
    fn sync_blobs(
        &self,
        hashes: &[String],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        self.inner.sync_blobs(hashes)
    }

    fn get_blob_stream(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let hash = hash.to_string();
        Box::pin(async move {
            // Lock-free cache probe (bumps moka recency) — the old shape
            // took the one global async mutex here on EVERY cached chunk
            // read, and cloned `cache_dir` per hit for a miss-only struct.
            if self.index.get(&hash).is_some() {
                let cached = self.cached_path(&hash);
                if let Ok(file) = fs::File::open(&cached).await {
                    let stream: BlobStream =
                        Box::pin(ReaderStream::with_capacity(file, STREAM_CHUNK_SIZE));
                    return Ok(stream);
                }
                // Cache entry stale (file vanished) — drop it from the index.
                self.index.invalidate(&hash);
            }

            // Cache miss — fetch from inner (single-flight), spool to cache
            let cached = self.cached_path(&hash);
            let dest = self.fetch_and_cache_singleflight(&hash, &cached).await?;
            let file = fs::File::open(&dest).await.map_err(|e| {
                DomainError::internal_error("BlobCache", format!("re-open cached: {e}"))
            })?;
            let stream: BlobStream = Box::pin(ReaderStream::with_capacity(file, STREAM_CHUNK_SIZE));
            Ok(stream)
        })
    }

    fn get_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let hash = hash.to_string();
        Box::pin(async move {
            // Lock-free cache probe (bumps moka recency); the filesystem is
            // only touched after the probe, as before.
            if self.index.get(&hash).is_some() {
                let cached = self.cached_path(&hash);
                if let Ok(mut file) = fs::File::open(&cached).await {
                    file.seek(std::io::SeekFrom::Start(start))
                        .await
                        .map_err(|e| {
                            DomainError::internal_error("BlobCache", format!("seek: {e}"))
                        })?;
                    let take_len = end.map(|e| e.saturating_sub(start)).unwrap_or(u64::MAX);
                    let limited = file.take(take_len);
                    let stream: BlobStream =
                        Box::pin(ReaderStream::with_capacity(limited, STREAM_CHUNK_SIZE));
                    return Ok(stream);
                }
                self.index.invalidate(&hash);
            }

            // Cache miss — fetch full blob into cache (single-flight: a
            // player's parallel cold Range probes coalesce onto ONE remote
            // download), then serve the range locally.
            let cached = self.cached_path(&hash);
            let dest = self.fetch_and_cache_singleflight(&hash, &cached).await?;
            let mut file = fs::File::open(&dest)
                .await
                .map_err(|e| DomainError::internal_error("BlobCache", format!("re-open: {e}")))?;
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| DomainError::internal_error("BlobCache", format!("seek: {e}")))?;
            let take_len = end.map(|e| e.saturating_sub(start)).unwrap_or(u64::MAX);
            let limited = file.take(take_len);
            let stream: BlobStream =
                Box::pin(ReaderStream::with_capacity(limited, STREAM_CHUNK_SIZE));
            Ok(stream)
        })
    }

    fn delete_blob(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let hash = hash.to_string();
        Box::pin(async move {
            self.inner.delete_blob(&hash).await?;
            // Explicit invalidation unlinks here (the eviction listener
            // only unlinks size-evictions).
            self.index.invalidate(&hash);
            let _ = fs::remove_file(self.cached_path(&hash)).await;
            Ok(())
        })
    }

    fn blob_exists(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>> {
        let hash = hash.to_string();
        Box::pin(async move {
            // Check cache first (fast, lock-free)
            if self.index.get(&hash).is_some() {
                return Ok(true);
            }
            self.inner.blob_exists(&hash).await
        })
    }

    fn blob_size(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_string();
        Box::pin(async move {
            // Check cache (lock-free)
            if let Some(entry) = self.index.get(&hash) {
                return Ok(entry.size);
            }
            // Fallback to cached file on disk (in case index was lost)
            if let Ok(meta) = fs::metadata(self.cached_path(&hash)).await {
                return Ok(meta.len());
            }
            self.inner.blob_size(&hash).await
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        Box::pin(async move {
            let mut status = self.inner.health_check().await?;
            // Flush moka's pending maintenance so the reported byte count
            // is current (rare admin path — the cost is fine here).
            self.index.run_pending_tasks();
            let used = self.index.weighted_size();
            status.message = format!(
                "{} | Cache: {}/{} bytes used at {}",
                status.message,
                used,
                self.max_cache_bytes,
                self.cache_dir.display()
            );
            status.backend_type = format!("cached({})", status.backend_type);
            Ok(status)
        })
    }

    fn backend_type(&self) -> &'static str {
        "cached"
    }

    /// A cache miss fetches from the inner backend, so adopt its read-ahead
    /// (high for remote, where prefetch pays off; hits read local cache files).
    fn read_prefetch(&self) -> usize {
        self.inner.read_prefetch()
    }

    fn local_blob_path(&self, hash: &str) -> Option<PathBuf> {
        // If the blob is cached locally, return that path
        let path = self.cached_path(hash);
        if path.exists() { Some(path) } else { None }
    }

    /// Enumeration MUST delegate to the primary (inner) backend, not
    /// the local cache. The cache is by definition a subset (only
    /// recently-accessed blobs); walking the cache would look like
    /// "most of my blobs are orphans" from the tenant's perspective.
    /// The inner backend is the authoritative "what exists" source.
    fn list_blob_hashes(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        crate::application::ports::blob_storage_ports::BlobListPage,
                        DomainError,
                    >,
                > + Send
                + '_,
        >,
    > {
        self.inner.list_blob_hashes(cursor, limit)
    }
}

// ── Cache internals (miss path + population) ───────────────────────

impl CachedBlobBackend {
    /// Best-effort write-through cache population shared by both blob-bytes
    /// PUT paths. moka enforces the byte budget on every insert (the old
    /// index deliberately skipped the eviction sweep on this path, letting
    /// write bursts overshoot the budget until the next read-miss insert).
    async fn cache_bytes_write_through(&self, hash: String, data: &Bytes) {
        // The shard dir was created at initialize() — no per-write create_dir_all
        // (benches/ROUND26.md §D1).
        let dest = self.cached_path(&hash);
        let _ = fs::write(&dest, data).await;
        let data_len = data.len() as u64;
        self.index.insert(hash, CacheEntry { size: data_len });
    }

    /// Single-flight wrapper around [`Self::fetch_and_cache`]: the first
    /// caller for a hash becomes the leader and downloads; concurrent
    /// callers queue on the per-hash gate, then re-check the cache and serve
    /// the leader's file without touching the remote backend. Errors are not
    /// cached — the gate entry is dropped, so the next caller retries.
    async fn fetch_and_cache_singleflight(
        &self,
        hash: &str,
        cached: &Path,
    ) -> Result<PathBuf, DomainError> {
        let gate = self
            .inflight
            .entry(hash.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = gate.lock().await;

        // Re-check under the gate: if we queued behind the leader, the blob
        // is on disk now and this turns into a local open.
        if self.index.get(hash).is_some() && fs::metadata(cached).await.is_ok() {
            return Ok(cached.to_path_buf());
        }

        let result = self.fetch_and_cache(hash).await;
        // Drop the gate whether we succeeded or failed; a late-arriving
        // caller after an error creates a fresh gate and retries the fetch.
        self.inflight.remove(hash);
        result
    }

    async fn insert_into_cache(&self, hash: &str, source_path: &Path) -> Result<(), DomainError> {
        // Shard dir pre-created at initialize() (benches/ROUND26.md §D1).
        let dest = self.cached_path(hash);

        let size = fs::metadata(source_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        fs::copy(source_path, &dest).await.map_err(|e| {
            DomainError::internal_error("BlobCache", format!("cache copy failed: {e}"))
        })?;

        // moka enforces the byte budget; size-evicted victims are unlinked
        // by the eviction listener.
        self.index.insert(hash.to_string(), CacheEntry { size });
        Ok(())
    }

    async fn fetch_and_cache(&self, hash: &str) -> Result<PathBuf, DomainError> {
        let stream = self.inner.get_blob_stream(hash).await?;

        // Shard dir pre-created at initialize() (benches/ROUND26.md §D1).
        let dest = self.cached_path(hash);

        // Unique temp name: even if two fetches for one hash ever race
        // (e.g. across processes sharing a cache dir), each writes its own
        // inode and the rename is atomic — a torn/interleaved file can
        // never land at the final path.
        let tmp = dest.with_extension(format!("{}.tmp", Uuid::new_v4()));
        let write_result: Result<u64, DomainError> = async {
            let mut file = fs::File::create(&tmp).await.map_err(|e| {
                DomainError::internal_error("BlobCache", format!("create tmp: {e}"))
            })?;

            use futures::StreamExt;
            let mut stream = stream;
            let mut total = 0u64;
            while let Some(chunk) = stream.next().await {
                let bytes = chunk.map_err(|e| {
                    DomainError::internal_error("BlobCache", format!("stream read: {e}"))
                })?;
                total += bytes.len() as u64;
                file.write_all(&bytes)
                    .await
                    .map_err(|e| DomainError::internal_error("BlobCache", format!("write: {e}")))?;
            }
            file.flush()
                .await
                .map_err(|e| DomainError::internal_error("BlobCache", format!("flush: {e}")))?;
            Ok(total)
        }
        .await;
        let total = match write_result {
            Ok(total) => total,
            Err(e) => {
                // Unique tmp names never get overwritten by a later fetch —
                // reap the partial file instead of leaking it.
                let _ = fs::remove_file(&tmp).await;
                return Err(e);
            }
        };

        if let Err(e) = fs::rename(&tmp, &dest).await {
            let _ = fs::remove_file(&tmp).await;
            return Err(DomainError::internal_error(
                "BlobCache",
                format!("rename: {e}"),
            ));
        }

        // moka enforces the byte budget; size-evicted victims are unlinked
        // by the eviction listener.
        self.index
            .insert(hash.to_string(), CacheEntry { size: total });

        Ok(dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::services::local_blob_backend::LocalBlobBackend;
    use futures::StreamExt;

    async fn read_range(
        backend: &dyn BlobStorageBackend,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Vec<u8> {
        let mut stream = backend
            .get_blob_range_stream(hash, start, end)
            .await
            .expect("open range stream");
        let mut output = Vec::new();
        while let Some(chunk) = stream.next().await {
            output.extend_from_slice(&chunk.expect("read range chunk"));
        }
        output
    }

    #[tokio::test]
    async fn range_end_is_exclusive_on_cold_and_hot_cache_reads() {
        let data = Bytes::from_static(b"abcdef");
        let hash = blake3::hash(&data).to_hex().to_string();

        let inner_root = tempfile::tempdir().expect("inner tempdir");
        let inner = Arc::new(LocalBlobBackend::new(inner_root.path()));
        inner.initialize().await.expect("initialize inner");
        inner
            .put_blob_from_bytes(&hash, data)
            .await
            .expect("seed inner");

        let cache_root = tempfile::tempdir().expect("cache tempdir");
        let cached = CachedBlobBackend::new(
            inner.clone(),
            &BlobCacheConfig {
                cache_dir: cache_root.path().to_path_buf(),
                max_cache_bytes: 1024 * 1024,
            },
        );
        cached.initialize().await.expect("initialize cache");

        // Cold read fills the cache and must honor the exclusive end.
        assert_eq!(read_range(&cached, &hash, 0, Some(1)).await, b"a");
        assert!(cached.local_blob_path(&hash).is_some());

        // Remove the origin so every remaining assertion proves a hot-cache read.
        inner.delete_blob(&hash).await.expect("remove origin");
        assert_eq!(read_range(&cached, &hash, 1, Some(3)).await, b"bc");
        assert!(read_range(&cached, &hash, 3, Some(3)).await.is_empty());
        assert_eq!(read_range(&cached, &hash, 2, None).await, b"cdef");
    }
}
