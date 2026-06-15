//! `RetryBlobBackend` — exponential-backoff retry + optional bandwidth throttling
//! decorator for remote blob backends.
//!
//! Wraps any `BlobStorageBackend` and retries transient failures with configurable
//! exponential backoff.  Optionally throttles upload/download bandwidth via
//! inter-chunk sleeps.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::application::ports::blob_storage_ports::{
    BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use crate::domain::errors::DomainError;
use bytes::Bytes;

// ── Retry policy ───────────────────────────────────────────────────

/// Exponential backoff retry configuration.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts (0 = no retries).
    pub max_retries: u32,
    /// Initial backoff duration before the first retry.
    pub initial_backoff: Duration,
    /// Maximum backoff duration (capped).
    pub max_backoff: Duration,
    /// Multiplier applied to backoff after each attempt.
    pub backoff_multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
            backoff_multiplier: 2.0,
        }
    }
}

// ── RetryBlobBackend ───────────────────────────────────────────────

/// Decorator that retries failed backend operations with exponential backoff.
pub struct RetryBlobBackend {
    inner: Arc<dyn BlobStorageBackend>,
    policy: RetryPolicy,
}

impl RetryBlobBackend {
    pub fn new(inner: Arc<dyn BlobStorageBackend>, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }
}

/// Execute an async closure with exponential backoff retry.
async fn retry_async<F, Fut, T>(
    policy: &RetryPolicy,
    name: &str,
    mut f: F,
) -> Result<T, DomainError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DomainError>>,
{
    let mut attempt = 0u32;
    let mut backoff = policy.initial_backoff;

    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < policy.max_retries && is_retryable(&e) => {
                attempt += 1;
                tracing::warn!(
                    "Retry {}/{} for {} after error: {} (backoff {:?})",
                    attempt,
                    policy.max_retries,
                    name,
                    e,
                    backoff
                );
                tokio::time::sleep(backoff).await;
                let next =
                    Duration::from_secs_f64(backoff.as_secs_f64() * policy.backoff_multiplier);
                backoff = next.min(policy.max_backoff);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Determine if an error is likely transient (network timeout, 5xx, etc.).
fn is_retryable(err: &DomainError) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("timeout")
        || msg.contains("connection")
        || msg.contains("503")
        || msg.contains("500")
        || msg.contains("429")
        || msg.contains("temporarily")
        || msg.contains("broken pipe")
        || msg.contains("reset by peer")
}

impl BlobStorageBackend for RetryBlobBackend {
    fn initialize(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        Box::pin(async move {
            retry_async(&policy, "initialize", || {
                let inner = inner.clone();
                async move { inner.initialize().await }
            })
            .await
        })
    }

    fn put_blob(
        &self,
        hash: &str,
        source_path: &Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let hash = hash.to_string();
        let path = source_path.to_path_buf();
        Box::pin(async move {
            retry_async(&policy, &format!("put_blob({hash})"), || {
                let inner = inner.clone();
                let hash = hash.clone();
                let path = path.clone();
                async move { inner.put_blob(&hash, &path).await }
            })
            .await
        })
    }

    fn put_blob_from_bytes(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let hash = hash.to_string();
        Box::pin(async move {
            retry_async(&policy, &format!("put_blob_from_bytes({hash})"), || {
                let inner = inner.clone();
                let hash = hash.clone();
                let data = data.clone();
                async move { inner.put_blob_from_bytes(&hash, data).await }
            })
            .await
        })
    }

    fn get_blob_stream(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let hash = hash.to_string();
        Box::pin(async move {
            retry_async(&policy, &format!("get_blob_stream({hash})"), || {
                let inner = inner.clone();
                let hash = hash.clone();
                async move { inner.get_blob_stream(&hash).await }
            })
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
        let policy = self.policy.clone();
        let hash = hash.to_string();
        Box::pin(async move {
            retry_async(&policy, &format!("get_blob_range({hash})"), || {
                let inner = inner.clone();
                let hash = hash.clone();
                async move { inner.get_blob_range_stream(&hash, start, end).await }
            })
            .await
        })
    }

    fn delete_blob(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let hash = hash.to_string();
        Box::pin(async move {
            retry_async(&policy, &format!("delete_blob({hash})"), || {
                let inner = inner.clone();
                let hash = hash.clone();
                async move { inner.delete_blob(&hash).await }
            })
            .await
        })
    }

    fn blob_exists(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let hash = hash.to_string();
        Box::pin(async move {
            retry_async(&policy, &format!("blob_exists({hash})"), || {
                let inner = inner.clone();
                let hash = hash.clone();
                async move { inner.blob_exists(&hash).await }
            })
            .await
        })
    }

    fn blob_size(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let hash = hash.to_string();
        Box::pin(async move {
            retry_async(&policy, &format!("blob_size({hash})"), || {
                let inner = inner.clone();
                let hash = hash.clone();
                async move { inner.blob_size(&hash).await }
            })
            .await
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        Box::pin(async move {
            retry_async(&policy, "health_check", || {
                let inner = inner.clone();
                async move { inner.health_check().await }
            })
            .await
        })
    }

    fn backend_type(&self) -> &'static str {
        "retry"
    }

    /// Transparent wrapper: the inner backend serves the bytes.
    fn read_prefetch(&self) -> usize {
        self.inner.read_prefetch()
    }

    fn local_blob_path(&self, hash: &str) -> Option<PathBuf> {
        self.inner.local_blob_path(hash)
    }
}
