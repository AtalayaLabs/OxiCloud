//! `TimeoutBlobBackend` — bounds every backend call in wall-clock time.
//!
//! ## Why this exists
//!
//! A backend call that *fails* is handled: it is classified, retried if
//! transient, and pauses the job at its cursor if it stays transient. A
//! call that never returns is handled by nothing at all.
//!
//! That is not hypothetical. Pull the network on an established TCP
//! connection and there is no RST and no ICMP — the peer simply stops
//! answering, and a socket read blocks until the OS gives up on
//! retransmission, on the order of fifteen minutes. For that whole
//! window the job is neither running nor failed: no error, so no retry,
//! no log line, no pause, nothing on the admin page. It looks exactly
//! like a very slow migration.
//!
//! Refusing a connection is instant and *does* surface (that is what
//! makes a `127.0.0.1` test look reassuring); losing a network mid-flight
//! is the silent case, and it is also the realistic one.
//!
//! ## Why a decorator rather than per-SDK configuration
//!
//! The S3 SDK can express this natively, and does — see the
//! `TimeoutConfig` in `s3_blob_backend.rs`, which is throughput-aware and
//! therefore strictly better for streams. But it only covers S3, and
//! Azure's 0.21 client has no equivalent knob short of supplying a custom
//! transport. That asymmetry is the reason this lives in the chain
//! instead of being configured twice.
//!
//! The local backend is a different story and deliberately not the
//! justification for this decorator: a local path is reached through the
//! kernel, and the kernel already owns that timeout. iSCSI gives up after
//! `replacement_timeout` (120s by default) and returns an I/O error;
//! NVMe-oF and soft-mounted NFS behave the same way. Those surface as an
//! `io::Error` and are classified by `local_io_error`, which is where
//! they belong. Local passes through this decorator only because a
//! uniform chain is simpler than a conditional one, and a bound that
//! never fires costs nothing.
//!
//! ## Operation classes
//!
//! A single timeout cannot fit both a `HEAD` and a multi-gigabyte upload,
//! so calls are bounded by what they do:
//!
//! * **Metadata** — `blob_exists`, `blob_size`, `delete_blob`,
//!   `initialize`, `health_check`, `list_blob_hashes`. Bounded tightly.
//!   These are the calls a migration makes per blob, and `blob_exists` in
//!   particular is its very first probe of the source.
//! * **Open** — `get_blob_stream`, `get_blob_range_stream`. The future
//!   resolves once the response *starts*; the body streams afterwards. So
//!   this bounds time-to-first-byte, not transfer duration, and a slow
//!   large read is never punished for being large.
//! * **Write** — the `put_*` family and `sync_blobs`. The entire transfer
//!   happens inside the future, so any wall-clock bound here is also a
//!   maximum upload duration. Unbounded by default for that reason:
//!   getting it wrong truncates legitimate uploads, which is a worse
//!   failure than the hang it would prevent. S3 covers this properly
//!   through stalled-stream protection, which measures throughput instead
//!   of elapsed time.
//!
//! A timeout is reported as [`DomainError::transient_backend`], because
//! that is what it is: no information was obtained about the blob. The
//! job engine pauses at its cursor and the work resumes when the network
//! does.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::application::ports::blob_storage_ports::{
    BlobListPage, BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use crate::domain::errors::DomainError;
use bytes::Bytes;

// ── Timeout policy ─────────────────────────────────────────────────

/// Per-operation-class wall-clock bounds.
#[derive(Debug, Clone)]
pub struct TimeoutPolicy {
    /// Bound for metadata calls (exists, size, delete, init, health, list).
    pub metadata: Option<Duration>,
    /// Bound for time-to-first-byte on reads.
    pub open: Option<Duration>,
    /// Bound for the whole of a write. `None` (the default) leaves large
    /// uploads unbounded — see the module docs.
    pub write: Option<Duration>,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            metadata: Some(Duration::from_secs(30)),
            open: Some(Duration::from_secs(60)),
            write: None,
        }
    }
}

impl TimeoutPolicy {
    /// A policy that bounds nothing — the pre-decorator behaviour.
    pub fn disabled() -> Self {
        Self {
            metadata: None,
            open: None,
            write: None,
        }
    }

    /// True when at least one class is bounded, i.e. wrapping is worth it.
    pub fn is_enabled(&self) -> bool {
        self.metadata.is_some() || self.open.is_some() || self.write.is_some()
    }
}

// ── TimeoutBlobBackend ─────────────────────────────────────────────

/// Decorator that fails a call the backend never answers.
pub struct TimeoutBlobBackend {
    inner: Arc<dyn BlobStorageBackend>,
    policy: TimeoutPolicy,
}

impl TimeoutBlobBackend {
    pub fn new(inner: Arc<dyn BlobStorageBackend>, policy: TimeoutPolicy) -> Self {
        Self { inner, policy }
    }
}

/// Await `fut`, giving up after `limit`.
///
/// `name` is lazy for the same reason as the retry decorator's: the
/// success path must not pay for a `format!` it will never print.
async fn with_timeout<T, L>(
    limit: Option<Duration>,
    backend: &'static str,
    name: L,
    fut: impl std::future::Future<Output = Result<T, DomainError>>,
) -> Result<T, DomainError>
where
    L: Fn() -> String,
{
    let Some(limit) = limit else {
        return fut.await;
    };
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result,
        Err(_) => {
            let op = name();
            // The one log line that distinguishes "hung" from "slow".
            // Without it a stalled backend is invisible until the job
            // pauses, and the pause reason alone does not say which
            // layer noticed.
            tracing::warn!(
                target: "oxicloud::storage",
                wrapper = "timeout",
                backend = backend,
                operation = %op,
                timeout_ms = limit.as_millis() as u64,
                "⏱️ Backend call timed out — treating as transient"
            );
            Err(DomainError::transient_backend(
                "Blob",
                format!(
                    "{op} on {backend} backend timed out after {:?} (no response)",
                    limit
                ),
            ))
        }
    }
}

impl BlobStorageBackend for TimeoutBlobBackend {
    fn initialize(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let limit = self.policy.metadata;
        Box::pin(async move {
            let backend = inner.backend_type();
            with_timeout(
                limit,
                backend,
                || "initialize".to_string(),
                inner.initialize(),
            )
            .await
        })
    }

    fn put_blob(
        &self,
        hash: &str,
        source_path: &Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let limit = self.policy.write;
        let hash = hash.to_string();
        let path = source_path.to_path_buf();
        Box::pin(async move {
            let backend = inner.backend_type();
            let label = hash.clone();
            with_timeout(
                limit,
                backend,
                || format!("put_blob({label})"),
                inner.put_blob(&hash, &path),
            )
            .await
        })
    }

    fn put_blob_from_bytes(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let limit = self.policy.write;
        let hash = hash.to_string();
        Box::pin(async move {
            let backend = inner.backend_type();
            let label = hash.clone();
            with_timeout(
                limit,
                backend,
                || format!("put_blob_from_bytes({label})"),
                inner.put_blob_from_bytes(&hash, data),
            )
            .await
        })
    }

    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let limit = self.policy.write;
        let hash = hash.to_string();
        Box::pin(async move {
            let backend = inner.backend_type();
            let label = hash.clone();
            with_timeout(
                limit,
                backend,
                || format!("put_blob_from_bytes_unsynced({label})"),
                inner.put_blob_from_bytes_unsynced(&hash, data),
            )
            .await
        })
    }

    fn put_blob_from_bytes_replace(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let limit = self.policy.write;
        let hash = hash.to_string();
        Box::pin(async move {
            let backend = inner.backend_type();
            let label = hash.clone();
            with_timeout(
                limit,
                backend,
                || format!("put_blob_from_bytes_replace({label})"),
                inner.put_blob_from_bytes_replace(&hash, data),
            )
            .await
        })
    }

    fn sync_blobs(
        &self,
        hashes: &[String],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let limit = self.policy.write;
        let hashes = hashes.to_vec();
        Box::pin(async move {
            let backend = inner.backend_type();
            let count = hashes.len();
            with_timeout(
                limit,
                backend,
                || format!("sync_blobs({count} hashes)"),
                inner.sync_blobs(&hashes),
            )
            .await
        })
    }

    fn get_blob_stream(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let inner = self.inner.clone();
        let limit = self.policy.open;
        let hash = hash.to_string();
        Box::pin(async move {
            let backend = inner.backend_type();
            let label = hash.clone();
            with_timeout(
                limit,
                backend,
                || format!("get_blob_stream({label})"),
                inner.get_blob_stream(&hash),
            )
            .await
        })
    }

    fn get_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let inner = self.inner.clone();
        let limit = self.policy.open;
        let hash = hash.to_string();
        Box::pin(async move {
            let backend = inner.backend_type();
            let label = hash.clone();
            with_timeout(
                limit,
                backend,
                || format!("get_blob_range_stream({label}, {start}..{end:?})"),
                inner.get_blob_range_stream(&hash, start, end),
            )
            .await
        })
    }

    fn delete_blob(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let limit = self.policy.metadata;
        let hash = hash.to_string();
        Box::pin(async move {
            let backend = inner.backend_type();
            let label = hash.clone();
            with_timeout(
                limit,
                backend,
                || format!("delete_blob({label})"),
                inner.delete_blob(&hash),
            )
            .await
        })
    }

    fn blob_exists(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let limit = self.policy.metadata;
        let hash = hash.to_string();
        Box::pin(async move {
            let backend = inner.backend_type();
            let label = hash.clone();
            with_timeout(
                limit,
                backend,
                || format!("blob_exists({label})"),
                inner.blob_exists(&hash),
            )
            .await
        })
    }

    fn blob_size(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let limit = self.policy.metadata;
        let hash = hash.to_string();
        Box::pin(async move {
            let backend = inner.backend_type();
            let label = hash.clone();
            with_timeout(
                limit,
                backend,
                || format!("blob_size({label})"),
                inner.blob_size(&hash),
            )
            .await
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        let inner = self.inner.clone();
        let limit = self.policy.metadata;
        Box::pin(async move {
            let backend = inner.backend_type();
            with_timeout(
                limit,
                backend,
                || "health_check".to_string(),
                inner.health_check(),
            )
            .await
        })
    }

    fn list_blob_hashes(
        &self,
        cursor: Option<String>,
        limit_n: usize,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobListPage, DomainError>> + Send + '_>>
    {
        let inner = self.inner.clone();
        let limit = self.policy.metadata;
        Box::pin(async move {
            let backend = inner.backend_type();
            with_timeout(
                limit,
                backend,
                || format!("list_blob_hashes(limit {limit_n})"),
                inner.list_blob_hashes(cursor, limit_n),
            )
            .await
        })
    }

    fn backend_type(&self) -> &'static str {
        self.inner.backend_type()
    }

    fn local_blob_path(&self, hash: &str) -> Option<PathBuf> {
        self.inner.local_blob_path(hash)
    }

    /// Forwarded, and then re-wrapped.
    ///
    /// `uncached()` exists so a verification pass can read past the
    /// cache; an inner backend reached that way is no less able to hang
    /// than the cached one, so it keeps the same bound.
    fn uncached(&self) -> Option<Arc<dyn BlobStorageBackend>> {
        self.inner.uncached().map(|inner| {
            Arc::new(TimeoutBlobBackend::new(inner, self.policy.clone()))
                as Arc<dyn BlobStorageBackend>
        })
    }

    fn read_prefetch(&self) -> usize {
        self.inner.read_prefetch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::errors::ErrorKind;

    /// A backend whose every call parks forever — the network-pulled case.
    struct HangingBackend;

    impl BlobStorageBackend for HangingBackend {
        fn initialize(
            &self,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>>
        {
            Box::pin(async { std::future::pending().await })
        }
        fn put_blob(
            &self,
            _hash: &str,
            _source_path: &Path,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>>
        {
            Box::pin(async { std::future::pending().await })
        }
        fn put_blob_from_bytes(
            &self,
            _hash: &str,
            _data: Bytes,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>>
        {
            Box::pin(async { std::future::pending().await })
        }
        fn get_blob_stream(
            &self,
            _hash: &str,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
        {
            Box::pin(async { std::future::pending().await })
        }
        fn get_blob_range_stream(
            &self,
            _hash: &str,
            _start: u64,
            _end: Option<u64>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
        {
            Box::pin(async { std::future::pending().await })
        }
        fn delete_blob(
            &self,
            _hash: &str,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>>
        {
            Box::pin(async { std::future::pending().await })
        }
        fn blob_exists(
            &self,
            _hash: &str,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>>
        {
            Box::pin(async { std::future::pending().await })
        }
        fn blob_size(
            &self,
            _hash: &str,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>>
        {
            Box::pin(async { std::future::pending().await })
        }
        fn health_check(
            &self,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { std::future::pending().await })
        }
        fn backend_type(&self) -> &'static str {
            "hanging"
        }
        fn local_blob_path(&self, _hash: &str) -> Option<PathBuf> {
            None
        }
    }

    fn wrapped() -> TimeoutBlobBackend {
        TimeoutBlobBackend::new(
            Arc::new(HangingBackend),
            TimeoutPolicy {
                metadata: Some(Duration::from_millis(50)),
                open: Some(Duration::from_millis(50)),
                write: Some(Duration::from_millis(50)),
            },
        )
    }

    /// The whole point: a hang must become a *transient* error, not a
    /// hang and not a permanent one. `NotFound` here would tell a
    /// migration the blob is absent; a permanent error would fail the
    /// run instead of pausing it.
    #[tokio::test]
    async fn a_hanging_backend_yields_a_transient_error_not_a_hang() {
        let backend = wrapped();

        let err = backend.blob_exists("abc").await.unwrap_err();
        assert!(
            err.is_transient(),
            "a stalled probe must be transient so the job pauses and resumes: {err}"
        );
        assert_ne!(
            err.kind,
            ErrorKind::NotFound,
            "a hang says nothing about whether the blob exists"
        );

        assert!(backend.blob_size("abc").await.unwrap_err().is_transient());
        assert!(backend.delete_blob("abc").await.unwrap_err().is_transient());
        assert!(backend.initialize().await.unwrap_err().is_transient());
        // `BlobStream` is not `Debug`, so go through `.err()` rather than
        // `unwrap_err()`.
        assert!(
            backend
                .get_blob_stream("abc")
                .await
                .err()
                .expect("a hanging read must not succeed")
                .is_transient()
        );
    }

    /// `write: None` is the default, and it must genuinely mean
    /// "unbounded" — a large upload cannot be truncated by this
    /// decorator.
    #[tokio::test]
    async fn an_unbounded_class_is_not_bounded() {
        let backend = TimeoutBlobBackend::new(
            Arc::new(HangingBackend),
            TimeoutPolicy {
                metadata: Some(Duration::from_millis(50)),
                open: None,
                write: None,
            },
        );

        // Bounded class still fires...
        assert!(backend.blob_exists("abc").await.unwrap_err().is_transient());

        // ...while an unbounded one is still pending long after the
        // bounded one would have given up. `write: None` is the default,
        // and a truncated multi-gigabyte upload is a worse outcome than
        // the hang the bound would have caught.
        let open = backend.get_blob_stream("abc");
        assert!(
            tokio::time::timeout(Duration::from_millis(250), open)
                .await
                .is_err(),
            "an unbounded class must never be cut short by the decorator"
        );
    }

    #[test]
    fn disabled_is_disabled() {
        assert!(!TimeoutPolicy::disabled().is_enabled());
        assert!(TimeoutPolicy::default().is_enabled());
    }
}
