//! `MigrationBlobBackend` — decorator that enables zero-downtime migration
//! between blob storage backends.
//!
//! During a migration the decorator writes to the **target** backend and reads
//! from **target-first-then-source** (dual-read).  A background job
//! (see `migration_job.rs`) copies remaining blobs in the background.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::application::ports::blob_storage_ports::{
    BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use crate::common::errors::DomainError;

// ── Migration state ────────────────────────────────────────────────

/// Progress of an ongoing (or completed) backend migration.
#[derive(Debug, Clone, Serialize)]
pub struct MigrationState {
    pub status: MigrationStatus,
    pub total_blobs: u64,
    pub migrated_blobs: u64,
    pub migrated_bytes: u64,
    pub failed_blobs: Vec<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl Default for MigrationState {
    fn default() -> Self {
        Self {
            status: MigrationStatus::Idle,
            total_blobs: 0,
            migrated_blobs: 0,
            migrated_bytes: 0,
            failed_blobs: Vec::new(),
            started_at: None,
            completed_at: None,
        }
    }
}

/// Status of the migration job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStatus {
    Idle,
    Running,
    Paused,
    Completed,
    Failed,
}

// ── MigrationBlobBackend ───────────────────────────────────────────

/// A `BlobStorageBackend` decorator that proxies requests to a *source*
/// (old) and *target* (new) backend, enabling live migration.
pub struct MigrationBlobBackend {
    source: Arc<dyn BlobStorageBackend>,
    target: Arc<dyn BlobStorageBackend>,
    state: Arc<RwLock<MigrationState>>,
}

impl MigrationBlobBackend {
    pub fn new(
        source: Arc<dyn BlobStorageBackend>,
        target: Arc<dyn BlobStorageBackend>,
        state: Arc<RwLock<MigrationState>>,
    ) -> Self {
        Self {
            source,
            target,
            state,
        }
    }

    pub fn state(&self) -> &Arc<RwLock<MigrationState>> {
        &self.state
    }

    pub fn source(&self) -> &Arc<dyn BlobStorageBackend> {
        &self.source
    }

    pub fn target(&self) -> &Arc<dyn BlobStorageBackend> {
        &self.target
    }
}

/// Boxed future alias (same as in the trait module).
type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl BlobStorageBackend for MigrationBlobBackend {
    fn initialize(&self) -> BoxFut<'_, Result<(), DomainError>> {
        Box::pin(async move {
            self.target.initialize().await?;
            // Source is already initialised; call anyway for idempotency.
            self.source.initialize().await?;
            Ok(())
        })
    }

    /// Writes go to **target** only.
    fn put_blob(&self, hash: &str, source_path: &Path) -> BoxFut<'_, Result<u64, DomainError>> {
        let hash = hash.to_string();
        let path = source_path.to_path_buf();
        Box::pin(async move { self.target.put_blob(&hash, &path).await })
    }

    /// Writes bytes to **target** only.
    fn put_blob_from_bytes(&self, hash: &str, data: Bytes) -> BoxFut<'_, Result<u64, DomainError>> {
        let hash = hash.to_string();
        Box::pin(async move { self.target.put_blob_from_bytes(&hash, data).await })
    }

    /// Unsynced writes go to **target** only (same as the synced variant).
    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> BoxFut<'_, Result<u64, DomainError>> {
        let hash = hash.to_string();
        Box::pin(async move { self.target.put_blob_from_bytes_unsynced(&hash, data).await })
    }

    /// Durability sweep goes to **target**, where unsynced writes land.
    fn sync_blobs(&self, hashes: &[String]) -> BoxFut<'_, Result<(), DomainError>> {
        self.target.sync_blobs(hashes)
    }

    /// Read from target first; fall back to source.
    fn get_blob_stream(&self, hash: &str) -> BoxFut<'_, Result<BlobStream, DomainError>> {
        let hash = hash.to_string();
        Box::pin(async move {
            match self.target.get_blob_stream(&hash).await {
                Ok(stream) => Ok(stream),
                Err(_) => self.source.get_blob_stream(&hash).await,
            }
        })
    }

    fn get_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> BoxFut<'_, Result<BlobStream, DomainError>> {
        let hash = hash.to_string();
        Box::pin(async move {
            match self.target.get_blob_range_stream(&hash, start, end).await {
                Ok(stream) => Ok(stream),
                Err(_) => self.source.get_blob_range_stream(&hash, start, end).await,
            }
        })
    }

    /// Delete from **both** backends (best-effort on source).
    fn delete_blob(&self, hash: &str) -> BoxFut<'_, Result<(), DomainError>> {
        let hash = hash.to_string();
        Box::pin(async move {
            self.target.delete_blob(&hash).await?;
            // Best-effort on source — ignore errors (blob may already be gone).
            let _ = self.source.delete_blob(&hash).await;
            Ok(())
        })
    }

    /// Exists in either backend.
    fn blob_exists(&self, hash: &str) -> BoxFut<'_, Result<bool, DomainError>> {
        let hash = hash.to_string();
        Box::pin(async move {
            if self.target.blob_exists(&hash).await? {
                return Ok(true);
            }
            self.source.blob_exists(&hash).await
        })
    }

    fn blob_size(&self, hash: &str) -> BoxFut<'_, Result<u64, DomainError>> {
        let hash = hash.to_string();
        Box::pin(async move {
            match self.target.blob_size(&hash).await {
                Ok(sz) => Ok(sz),
                Err(_) => self.source.blob_size(&hash).await,
            }
        })
    }

    fn health_check(&self) -> BoxFut<'_, Result<StorageHealthStatus, DomainError>> {
        Box::pin(async move {
            let target_health = self.target.health_check().await?;
            let source_health = self.source.health_check().await?;
            Ok(StorageHealthStatus {
                connected: target_health.connected && source_health.connected,
                backend_type: format!(
                    "migration({} → {})",
                    source_health.backend_type, target_health.backend_type
                ),
                message: format!(
                    "Source: {} | Target: {}",
                    source_health.message, target_health.message
                ),
                available_bytes: target_health.available_bytes,
            })
        })
    }

    fn backend_type(&self) -> &'static str {
        "migration"
    }

    /// Reads are served target-first (see `get_blob_stream`), so adopt the
    /// target's read-ahead.
    fn read_prefetch(&self) -> usize {
        self.target.read_prefetch()
    }

    fn local_blob_path(&self, hash: &str) -> Option<PathBuf> {
        // Prefer target, fall back to source.
        self.target
            .local_blob_path(hash)
            .or_else(|| self.source.local_blob_path(hash))
    }
}
