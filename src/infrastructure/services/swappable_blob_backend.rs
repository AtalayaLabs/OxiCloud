//! `SwappableBlobBackend` — atomic hot-swap wrapper for `BlobStorageBackend`.
//!
//! Enables in-process cutover after a migration completes, without
//! restarting the server. The runtime never holds a raw
//! `Arc<dyn BlobStorageBackend>` pointing at a specific concrete
//! backend; it holds `Arc<SwappableBlobBackend>`, and every method
//! call snapshots the CURRENT inner backend from an `ArcSwap` before
//! delegating.
//!
//! Design contract (see `docs/plan/storage-multi-entry.md`
//! §"Read-only mode" cross-ref — the hot-swap upgrade removes the
//! restart step):
//!
//! * **Reads are lock-free.** `ArcSwap::load_full` bumps the strong
//!   count on the current inner Arc and returns it — no lock, no
//!   contention with concurrent readers, no contention with a swap.
//! * **In-flight operations complete on the OLD backend.** Each
//!   method call clones the current inner Arc at the top; the
//!   spawned future holds that clone for its whole lifetime. A swap
//!   that happens mid-future doesn't affect that future — a `PUT`
//!   that started on local completes on local, even after cutover
//!   flipped the pointer to S3.
//! * **New operations after a swap see the NEW backend.** The
//!   `ArcSwap::store` on `swap()` is atomic relative to
//!   `ArcSwap::load_full` — no race window where a request sees a
//!   torn state.
//! * **The old backend is dropped when the last in-flight future
//!   holding it finishes.** Standard `Arc` refcount semantics.
//!
//! The wrapper delegates every method the `BlobStorageBackend` trait
//! declares. Two mildly interesting cases:
//! - `local_blob_path` — meaningful only for a local backend. If the
//!   current inner is S3, returns `None`, same as any remote backend.
//!   After a Local→S3 hot-swap, callers stop getting fast-paths;
//!   they get streaming (correct fallback in every caller).
//! - `list_blob_hashes` — takes an opaque cursor whose format is
//!   backend-specific. Swapping mid-enumeration would invalidate the
//!   cursor. Not currently an issue because enumeration only runs
//!   inside `backend_consistency` / `blobs_consistency`, which
//!   snapshot at run start (they clone the Arc into their own local
//!   `backend` for the whole loop, so a mid-run swap doesn't affect
//!   them — same in-flight guarantee as any other operation).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use bytes::Bytes;

use crate::application::ports::blob_storage_ports::{
    BlobListPage, BlobStorageBackend, BlobStream, BoxFut, StorageHealthStatus,
};
use crate::common::errors::DomainError;

/// Atomic hot-swap wrapper around a concrete `BlobStorageBackend`.
///
/// Reads are effectively lock-free — an uncontended `RwLock::read`
/// on a modern std impl is just a relaxed atomic bump; contention
/// only exists during the brief window of a swap. Swap is a bounded
/// `RwLock::write` that flips the inner `Arc` — no I/O, no waiting
/// for in-flight futures. In-flight futures already hold a clone of
/// the previous inner `Arc` (loaded when their method call started)
/// and complete against that; the old Arc drops naturally when the
/// last of them finishes.
///
/// `arc-swap`'s `ArcSwap<T>` was the first choice here — it's
/// lock-free — but it requires `T: Sized`, which `dyn ...` is not.
/// The `Arc<dyn Trait>` snapshot pattern via `RwLock` is the
/// standard workaround; performance is indistinguishable at the
/// per-request cadence of a blob backend.
pub struct SwappableBlobBackend {
    inner: RwLock<Arc<dyn BlobStorageBackend>>,
}

impl SwappableBlobBackend {
    /// Wrap an initial backend. Every call goes to this one until
    /// [`Self::swap`] replaces it.
    pub fn new(initial: Arc<dyn BlobStorageBackend>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    /// Atomically replace the inner backend. Subsequent method calls
    /// see `new`; futures already running against the previous inner
    /// keep running to completion (the previous Arc is only fully
    /// dropped when the last of them finishes).
    ///
    /// Callers responsible for `.initialize()` on `new` before the
    /// swap — this method assumes the new backend is ready.
    pub fn swap(&self, new: Arc<dyn BlobStorageBackend>) {
        // A poisoned lock here would mean a panic during a previous
        // swap — recover the guard to keep the swap surface robust
        // (nobody should panic during a normal `store`, but a
        // recovering-from-poison call still yields a working writer).
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = new;
    }

    /// Take a snapshot of the current inner backend. Cheap: one
    /// `read()` + one strong-count bump on the returned Arc. Callers
    /// can hold the resulting Arc across await points; the wrapper's
    /// swap won't affect it.
    pub fn current(&self) -> Arc<dyn BlobStorageBackend> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Consumers of `Arc<dyn BlobStorageBackend>` get exactly today's
/// contract — the wrapper is transparent from their point of view.
impl BlobStorageBackend for SwappableBlobBackend {
    fn initialize(&self) -> BoxFut<'_, Result<(), DomainError>> {
        let inner = self.current();
        Box::pin(async move { inner.initialize().await })
    }

    fn put_blob(&self, hash: &str, source_path: &Path) -> BoxFut<'_, Result<u64, DomainError>> {
        let inner = self.current();
        let hash = hash.to_owned();
        let source_path = source_path.to_owned();
        Box::pin(async move { inner.put_blob(&hash, &source_path).await })
    }

    fn put_blob_from_bytes(
        &self,
        hash: &str,
        data: Bytes,
    ) -> BoxFut<'_, Result<u64, DomainError>> {
        let inner = self.current();
        let hash = hash.to_owned();
        Box::pin(async move { inner.put_blob_from_bytes(&hash, data).await })
    }

    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> BoxFut<'_, Result<u64, DomainError>> {
        let inner = self.current();
        let hash = hash.to_owned();
        Box::pin(async move { inner.put_blob_from_bytes_unsynced(&hash, data).await })
    }

    fn sync_blobs(&self, hashes: &[String]) -> BoxFut<'_, Result<(), DomainError>> {
        let inner = self.current();
        let hashes = hashes.to_vec();
        Box::pin(async move { inner.sync_blobs(&hashes).await })
    }

    fn get_blob_stream(&self, hash: &str) -> BoxFut<'_, Result<BlobStream, DomainError>> {
        let inner = self.current();
        let hash = hash.to_owned();
        Box::pin(async move { inner.get_blob_stream(&hash).await })
    }

    fn get_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> BoxFut<'_, Result<BlobStream, DomainError>> {
        let inner = self.current();
        let hash = hash.to_owned();
        Box::pin(async move { inner.get_blob_range_stream(&hash, start, end).await })
    }

    fn delete_blob(&self, hash: &str) -> BoxFut<'_, Result<(), DomainError>> {
        let inner = self.current();
        let hash = hash.to_owned();
        Box::pin(async move { inner.delete_blob(&hash).await })
    }

    fn blob_exists(&self, hash: &str) -> BoxFut<'_, Result<bool, DomainError>> {
        let inner = self.current();
        let hash = hash.to_owned();
        Box::pin(async move { inner.blob_exists(&hash).await })
    }

    fn blob_size(&self, hash: &str) -> BoxFut<'_, Result<u64, DomainError>> {
        let inner = self.current();
        let hash = hash.to_owned();
        Box::pin(async move { inner.blob_size(&hash).await })
    }

    fn health_check(&self) -> BoxFut<'_, Result<StorageHealthStatus, DomainError>> {
        let inner = self.current();
        Box::pin(async move { inner.health_check().await })
    }

    fn backend_type(&self) -> &'static str {
        // We return the CURRENT backend's static kind. Delegation
        // means the returned string reflects the entry the swap
        // pointer currently names.
        //
        // NB: `&'static str` is deliberate — every backend's kind is
        // a compile-time constant ("local", "s3", "azure"), so this
        // is safe. The trait signature makes it appear to outlive
        // the `&self` borrow, but the value is a static so there's
        // no lifetime hazard.
        self.current().backend_type()
    }

    fn local_blob_path(&self, hash: &str) -> Option<PathBuf> {
        self.current().local_blob_path(hash)
    }

    fn read_prefetch(&self) -> usize {
        self.current().read_prefetch()
    }

    fn list_blob_hashes(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> BoxFut<'_, Result<BlobListPage, DomainError>> {
        let inner = self.current();
        Box::pin(async move { inner.list_blob_hashes(cursor, limit).await })
    }
}

