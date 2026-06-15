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
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use lru::LruCache;
use std::num::NonZeroUsize;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;

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
pub struct CachedBlobBackend {
    inner: Arc<dyn BlobStorageBackend>,
    cache_dir: PathBuf,
    max_cache_bytes: u64,
    index: Arc<Mutex<LruCache<String, CacheEntry>>>,
    current_size: Arc<AtomicU64>,
}

impl CachedBlobBackend {
    /// Create a new cached backend wrapping `inner`.
    pub fn new(inner: Arc<dyn BlobStorageBackend>, config: &BlobCacheConfig) -> Self {
        Self {
            inner,
            cache_dir: config.cache_dir.clone(),
            max_cache_bytes: config.max_cache_bytes,
            // Capacity is essentially unbounded — eviction is by byte budget, not count.
            index: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(1_000_000).unwrap(),
            ))),
            current_size: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Path where a blob is cached locally.
    fn cached_path(&self, hash: &str) -> PathBuf {
        let prefix = &hash[..2.min(hash.len())];
        self.cache_dir.join(prefix).join(format!("{hash}.blob"))
    }
}

impl BlobStorageBackend for CachedBlobBackend {
    fn initialize(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let cache_dir = self.cache_dir.clone();
        let index = self.index.clone();
        let current_size = self.current_size.clone();
        Box::pin(async move {
            inner.initialize().await?;

            // Create cache dir structure (256 prefix dirs)
            fs::create_dir_all(&cache_dir).await.map_err(|e| {
                DomainError::internal_error("BlobCache", format!("mkdir cache_dir: {e}"))
            })?;

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
            // Bulk-insert the rebuilt index under a single brief lock.
            {
                let mut idx = index.lock().await;
                for (stem, size) in entries {
                    idx.put(stem, CacheEntry { size });
                }
            }
            current_size.store(total_bytes, Ordering::Relaxed);
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
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let source = source_path.to_path_buf();
        let self_ref = CachedRef {
            cache_dir: self.cache_dir.clone(),
            max_cache_bytes: self.max_cache_bytes,
            index: self.index.clone(),
            current_size: self.current_size.clone(),
        };
        Box::pin(async move {
            // Write to inner backend
            let bytes = inner.put_blob(&hash, &source).await?;
            // Also cache locally (best-effort)
            let _ = self_ref.insert_into_cache_static(&hash, &source).await;
            Ok(bytes)
        })
    }

    fn put_blob_from_bytes(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let self_ref = CachedRef {
            cache_dir: self.cache_dir.clone(),
            max_cache_bytes: self.max_cache_bytes,
            index: self.index.clone(),
            current_size: self.current_size.clone(),
        };
        Box::pin(async move {
            let size = inner.put_blob_from_bytes(&hash, data.clone()).await?;
            // Also cache locally (best-effort): write bytes to cache path
            let dest = self_ref.cached_path(&hash);
            if let Some(parent) = dest.parent() {
                let _ = fs::create_dir_all(parent).await;
            }
            let _ = fs::write(&dest, &data).await;
            let data_len = data.len() as u64;
            let mut idx = self_ref.index.lock().await;
            if let Some(old) = idx.put(hash, CacheEntry { size: data_len }) {
                self_ref.current_size.fetch_sub(old.size, Ordering::Relaxed);
            }
            self_ref.current_size.fetch_add(data_len, Ordering::Relaxed);
            Ok(size)
        })
    }

    fn get_blob_stream(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let hash = hash.to_string();
        let cached = self.cached_path(&hash);
        let index = self.index.clone();
        let inner = self.inner.clone();
        let cache_dir = self.cache_dir.clone();
        let max_cache_bytes = self.max_cache_bytes;
        let current_size = self.current_size.clone();
        Box::pin(async move {
            // Check cache presence (and bump LRU recency) under a brief lock,
            // then release it BEFORE touching the filesystem so concurrent
            // readers don't serialize behind a single open() syscall.
            if index.lock().await.get(&hash).is_some() {
                if let Ok(file) = fs::File::open(&cached).await {
                    let stream: BlobStream =
                        Box::pin(ReaderStream::with_capacity(file, STREAM_CHUNK_SIZE));
                    return Ok(stream);
                }
                // Cache entry stale (file vanished) — drop it from the index.
                if let Some(entry) = index.lock().await.pop(&hash) {
                    current_size.fetch_sub(entry.size, Ordering::Relaxed);
                }
            }

            // Cache miss — fetch from inner, spool to cache
            let self_ref = CachedRef {
                cache_dir,
                max_cache_bytes,
                index: index.clone(),
                current_size: current_size.clone(),
            };
            let dest = self_ref.fetch_and_cache_static(&hash, &*inner).await?;
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
        let cached = self.cached_path(&hash);
        let index = self.index.clone();
        let inner = self.inner.clone();
        let cache_dir = self.cache_dir.clone();
        let max_cache_bytes = self.max_cache_bytes;
        let current_size = self.current_size.clone();
        Box::pin(async move {
            // Check cache presence (and bump LRU recency) under a brief lock,
            // then release it BEFORE the open()/seek() syscalls so concurrent
            // range readers don't serialize behind the index mutex.
            if index.lock().await.get(&hash).is_some() {
                if let Ok(mut file) = fs::File::open(&cached).await {
                    file.seek(std::io::SeekFrom::Start(start))
                        .await
                        .map_err(|e| {
                            DomainError::internal_error("BlobCache", format!("seek: {e}"))
                        })?;
                    let take_len = end.map(|e| e - start + 1).unwrap_or(u64::MAX);
                    let limited = file.take(take_len);
                    let stream: BlobStream =
                        Box::pin(ReaderStream::with_capacity(limited, STREAM_CHUNK_SIZE));
                    return Ok(stream);
                }
                if let Some(entry) = index.lock().await.pop(&hash) {
                    current_size.fetch_sub(entry.size, Ordering::Relaxed);
                }
            }

            // Cache miss — fetch full blob into cache, then serve range
            let self_ref = CachedRef {
                cache_dir,
                max_cache_bytes,
                index: index.clone(),
                current_size: current_size.clone(),
            };
            let dest = self_ref.fetch_and_cache_static(&hash, &*inner).await?;
            let mut file = fs::File::open(&dest)
                .await
                .map_err(|e| DomainError::internal_error("BlobCache", format!("re-open: {e}")))?;
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| DomainError::internal_error("BlobCache", format!("seek: {e}")))?;
            let take_len = end.map(|e| e - start + 1).unwrap_or(u64::MAX);
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
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let cached = self.cached_path(&hash);
        let index = self.index.clone();
        let current_size = self.current_size.clone();
        Box::pin(async move {
            inner.delete_blob(&hash).await?;
            // Remove from cache — drop the index lock before the unlink()
            // syscall so deletes don't serialize concurrent cache lookups.
            if let Some(entry) = index.lock().await.pop(&hash) {
                current_size.fetch_sub(entry.size, Ordering::Relaxed);
            }
            let _ = fs::remove_file(&cached).await;
            Ok(())
        })
    }

    fn blob_exists(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let index = self.index.clone();
        Box::pin(async move {
            // Check cache first (fast)
            {
                let mut idx = index.lock().await;
                if idx.get(&hash).is_some() {
                    return Ok(true);
                }
            }
            inner.blob_exists(&hash).await
        })
    }

    fn blob_size(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let index = self.index.clone();
        let cached = self.cached_path(&hash);
        Box::pin(async move {
            // Check cache
            {
                let mut idx = index.lock().await;
                if let Some(entry) = idx.get(&hash) {
                    return Ok(entry.size);
                }
            }
            // Fallback to cached file on disk (in case index was lost)
            if let Ok(meta) = fs::metadata(&cached).await {
                return Ok(meta.len());
            }
            inner.blob_size(&hash).await
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        let inner = self.inner.clone();
        let cache_dir = self.cache_dir.clone();
        let current_size = self.current_size.clone();
        let max_bytes = self.max_cache_bytes;
        Box::pin(async move {
            let mut status = inner.health_check().await?;
            let used = current_size.load(Ordering::Relaxed);
            status.message = format!(
                "{} | Cache: {}/{} bytes used at {}",
                status.message,
                used,
                max_bytes,
                cache_dir.display()
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
}

// ── Helper struct for owned references in async closures ───────────

/// Cloneable set of cache internals — avoids borrow issues in boxed futures.
struct CachedRef {
    cache_dir: PathBuf,
    max_cache_bytes: u64,
    index: Arc<Mutex<LruCache<String, CacheEntry>>>,
    current_size: Arc<AtomicU64>,
}

impl CachedRef {
    fn cached_path(&self, hash: &str) -> PathBuf {
        let prefix = &hash[..2.min(hash.len())];
        self.cache_dir.join(prefix).join(format!("{hash}.blob"))
    }

    /// Pop LRU entries until the cache is back within its byte budget,
    /// returning the on-disk paths of the evicted blobs.
    ///
    /// Only the in-memory index is touched here (atomic counter + LRU map);
    /// the caller MUST unlink the returned paths AFTER releasing the index
    /// lock so the `remove_file` syscalls never run while the mutex is held.
    fn collect_evictions(&self, idx: &mut LruCache<String, CacheEntry>) -> Vec<PathBuf> {
        let mut victims = Vec::new();
        while self.current_size.load(Ordering::Relaxed) > self.max_cache_bytes {
            if let Some((evicted_hash, evicted_entry)) = idx.pop_lru() {
                self.current_size
                    .fetch_sub(evicted_entry.size, Ordering::Relaxed);
                victims.push(self.cached_path(&evicted_hash));
            } else {
                break;
            }
        }
        victims
    }

    async fn insert_into_cache_static(
        &self,
        hash: &str,
        source_path: &Path,
    ) -> Result<(), DomainError> {
        let dest = self.cached_path(hash);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                DomainError::internal_error("BlobCache", format!("mkdir failed: {e}"))
            })?;
        }

        let size = fs::metadata(source_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        fs::copy(source_path, &dest).await.map_err(|e| {
            DomainError::internal_error("BlobCache", format!("cache copy failed: {e}"))
        })?;

        // Update the index and pick eviction victims under a single brief
        // lock, then unlink the evicted files AFTER releasing it — file
        // removal must not run while the index mutex is held.
        let to_evict = {
            let mut idx = self.index.lock().await;
            if let Some(old) = idx.put(hash.to_string(), CacheEntry { size }) {
                self.current_size.fetch_sub(old.size, Ordering::Relaxed);
            }
            self.current_size.fetch_add(size, Ordering::Relaxed);
            self.collect_evictions(&mut idx)
        };
        for path in to_evict {
            let _ = fs::remove_file(&path).await;
        }
        Ok(())
    }

    async fn fetch_and_cache_static(
        &self,
        hash: &str,
        inner: &dyn BlobStorageBackend,
    ) -> Result<PathBuf, DomainError> {
        let stream = inner.get_blob_stream(hash).await?;

        let dest = self.cached_path(hash);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                DomainError::internal_error("BlobCache", format!("mkdir failed: {e}"))
            })?;
        }

        let tmp = dest.with_extension("tmp");
        let mut file = fs::File::create(&tmp)
            .await
            .map_err(|e| DomainError::internal_error("BlobCache", format!("create tmp: {e}")))?;

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
        drop(file);

        fs::rename(&tmp, &dest)
            .await
            .map_err(|e| DomainError::internal_error("BlobCache", format!("rename: {e}")))?;

        let to_evict = {
            let mut idx = self.index.lock().await;
            if let Some(old) = idx.put(hash.to_string(), CacheEntry { size: total }) {
                self.current_size.fetch_sub(old.size, Ordering::Relaxed);
            }
            self.current_size.fetch_add(total, Ordering::Relaxed);
            self.collect_evictions(&mut idx)
        };
        for path in to_evict {
            let _ = fs::remove_file(&path).await;
        }

        Ok(dest)
    }
}
