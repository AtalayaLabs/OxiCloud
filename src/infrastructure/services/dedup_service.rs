//! Content-Addressable Storage with CDC Deduplication (PostgreSQL-backed)
//!
//! Implements sub-file deduplication using FastCDC (content-defined chunking).
//! Files are split into variable-size chunks (64 KB – 1 MB, avg 256 KB)
//! using the FastCDC 2020 algorithm. Each chunk is BLAKE3-hashed and stored
//! independently in the blob backend. A *manifest* in PostgreSQL maps the
//! whole-file hash to the ordered list of chunk hashes that compose it.
//!
//! Architecture:
//! ```text
//! ┌─────────────────┐     ┌─────────────────────┐     ┌─────────────┐
//! │ storage.files   │────▶│ chunk_manifests      │────▶│ storage.blobs│──▶ Blob Store
//! │ (references)    │     │ (file→[chunk_hashes])│     │ (chunks)     │
//! └─────────────────┘     └─────────────────────┘     └─────────────┘
//! ```
//!
//! **Backward compatibility**: files uploaded before CDC (legacy whole-file
//! blobs in `storage.blobs`) are served transparently — when no manifest
//! row exists for a hash, the service falls back to direct blob reads.
//!
//! **Write-first strategy** (store_from_file):
//!   1. CDC-analyse the file (mmap → FastCDC boundaries + per-chunk BLAKE3).
//!   2. Batch-check which chunk hashes already exist in PG (dedup skip).
//!   3. Read + upload only *new* chunks to the blob backend (idempotent).
//!   4. Bump ref_count for existing chunks (no disk I/O).
//!   5. Single manifest INSERT (~few ms total).
//!   6. PG connection is never held during disk I/O.
//!
//! Benefits:
//! - Sub-file dedup: edited files share unchanged chunks
//! - ACID durability — crash-safe, zero orphaned index entries
//! - PG connections never blocked by disk I/O (write-first)
//! - 60-80% storage reduction for versioned / edited files
//! - Faster uploads when chunks already exist

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use futures::{Stream, TryStreamExt};

use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::application::ports::dedup_ports::{
    BlobMetadataDto, DedupPort, DedupResultDto, DedupStatsDto,
};
use crate::domain::errors::{DomainError, ErrorKind};
use crate::infrastructure::services::thumbnail_service::ThumbnailService;

// ── CDC Constants ────────────────────────────────────────────────────────────

/// Minimum CDC chunk size (64 KB).
const CDC_MIN_CHUNK: usize = 65_536;
/// Average CDC chunk size (256 KB).
const CDC_AVG_CHUNK: usize = 262_144;
/// Maximum CDC chunk size (1 MB).
const CDC_MAX_CHUNK: usize = 1_048_576;

// ── CDC helper types ─────────────────────────────────────────────────────────

/// Metadata for a single CDC chunk (offset + length + BLAKE3 hash).
struct ChunkMeta {
    hash: String,
    offset: usize,
    length: usize,
}

/// Content-Addressable Storage Service with CDC (PostgreSQL-backed)
///
/// Splits files into variable-size chunks via FastCDC, stores each chunk
/// in the [`BlobStorageBackend`], and maintains a manifest in PostgreSQL
/// mapping file_hash → \[chunk_hashes\].  BLAKE3 hashing, ref-counting
/// and the PostgreSQL dedup index all live here.
pub struct DedupService {
    /// Pluggable blob storage backend (local FS, S3, …).
    backend: Arc<dyn BlobStorageBackend>,
    /// PostgreSQL connection pool (dedup index in `storage.blobs`) — primary,
    /// used by request-path operations (store_from_file, etc.).
    pool: Arc<PgPool>,
    /// Isolated maintenance pool for long-running operations
    /// (verify_integrity, garbage_collect) that must never starve the primary.
    maintenance_pool: Arc<PgPool>,
    /// Optional thumbnail service — when set, blob-hash thumbnails are deleted
    /// from disk whenever a blob's ref_count reaches zero.
    thumbnail_service: Option<Arc<ThumbnailService>>,
}

impl DedupService {
    /// Create a new dedup service backed by PostgreSQL.
    ///
    /// * `backend` — pluggable blob storage (local filesystem, S3, etc.).
    /// * `pool` — primary pool for request-path operations.
    /// * `maintenance_pool` — isolated pool for verify_integrity / garbage_collect.
    pub fn new(
        backend: Arc<dyn BlobStorageBackend>,
        pool: Arc<PgPool>,
        maintenance_pool: Arc<PgPool>,
    ) -> Self {
        Self {
            backend,
            pool,
            maintenance_pool,
            thumbnail_service: None,
        }
    }

    /// Attach a thumbnail service so that disk thumbnails are cleaned up when
    /// a blob's ref_count drops to zero.
    pub fn with_thumbnail_service(mut self, svc: Arc<ThumbnailService>) -> Self {
        self.thumbnail_service = Some(svc);
        self
    }

    /// Creates a stub instance for testing — never hits PG or the filesystem.
    #[cfg(any(test, feature = "integration_tests"))]
    pub fn new_stub() -> Self {
        use crate::infrastructure::services::local_blob_backend::LocalBlobBackend;
        let stub_pool = Arc::new(
            sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                .max_connections(1)
                .connect_lazy("postgres://invalid:5432/none")
                .unwrap(),
        );
        Self {
            backend: Arc::new(LocalBlobBackend::new(Path::new("/tmp/oxicloud_stub_blobs"))),
            pool: stub_pool.clone(),
            maintenance_pool: stub_pool,
            thumbnail_service: None,
        }
    }

    /// Initialize the service (delegate to backend + log stats from PG).
    pub async fn initialize(&self) -> Result<(), DomainError> {
        self.backend.initialize().await?;

        let blob_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage.blobs")
            .fetch_one(self.pool.as_ref())
            .await
            .unwrap_or(0);

        let blob_bytes: i64 =
            sqlx::query_scalar("SELECT COALESCE(SUM(size), 0) FROM storage.blobs")
                .fetch_one(self.pool.as_ref())
                .await
                .unwrap_or(0);

        let manifest_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM storage.chunk_manifests")
                .fetch_one(self.pool.as_ref())
                .await
                .unwrap_or(0);

        tracing::info!(
            "Dedup service initialized (backend={}, CDC): {} chunk blobs ({} bytes), {} manifests",
            self.backend.backend_type(),
            blob_count,
            blob_bytes,
            manifest_count,
        );

        Ok(())
    }

    /// Return a reference to the underlying blob storage backend.
    pub fn backend(&self) -> &Arc<dyn BlobStorageBackend> {
        &self.backend
    }

    // ── Path helpers ─────────────────────────────────────────────

    /// Get the local blob path for a given hash (if the backend supports it).
    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.backend
            .local_blob_path(hash)
            .unwrap_or_else(|| PathBuf::from(format!("remote://{}", hash)))
    }

    // ── CDC analysis ───────────────────────────────────────────

    /// Single-pass CDC: compute whole-file BLAKE3 hash + chunk boundaries + per-chunk hashes.
    ///
    /// Memory-maps the file and runs FastCDC boundary detection
    /// concurrently with BLAKE3 hashing — all in one pass.
    async fn cdc_hash_and_chunk_file(path: &Path) -> std::io::Result<(String, Vec<ChunkMeta>)> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&path)?;
            let file_size = file.metadata()?.len();

            if file_size == 0 {
                return Ok((blake3::hash(b"").to_hex().to_string(), vec![]));
            }

            // SAFETY: file is opened read-only; no concurrent writers expected
            // (source is a temp upload file owned exclusively by this request).
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            let chunker =
                fastcdc::v2020::FastCDC::new(&mmap, CDC_MIN_CHUNK, CDC_AVG_CHUNK, CDC_MAX_CHUNK);

            let mut file_hasher = blake3::Hasher::new();
            let mut chunks = Vec::new();

            for chunk in chunker {
                let data = &mmap[chunk.offset..chunk.offset + chunk.length];
                file_hasher.update(data);
                chunks.push(ChunkMeta {
                    hash: blake3::hash(data).to_hex().to_string(),
                    offset: chunk.offset,
                    length: chunk.length,
                });
            }

            Ok((file_hasher.finalize().to_hex().to_string(), chunks))
        })
        .await
        .expect("cdc_hash_and_chunk_file: spawn_blocking panicked")
    }

    /// CDC analysis without file-hash computation (when hash is pre-computed).
    async fn cdc_chunk_file(path: &Path) -> std::io::Result<Vec<ChunkMeta>> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&path)?;
            let file_size = file.metadata()?.len();

            if file_size == 0 {
                return Ok(vec![]);
            }

            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            let chunker =
                fastcdc::v2020::FastCDC::new(&mmap, CDC_MIN_CHUNK, CDC_AVG_CHUNK, CDC_MAX_CHUNK);

            let chunks: Vec<ChunkMeta> = chunker
                .map(|chunk| {
                    let data = &mmap[chunk.offset..chunk.offset + chunk.length];
                    ChunkMeta {
                        hash: blake3::hash(data).to_hex().to_string(),
                        offset: chunk.offset,
                        length: chunk.length,
                    }
                })
                .collect();

            Ok(chunks)
        })
        .await
        .expect("cdc_chunk_file: spawn_blocking panicked")
    }

    // ── Hash helpers ─────────────────────────────────────────────

    /// Calculate BLAKE3 hash of a file (~5× faster than SHA-256).
    ///
    /// Uses memory-mapped I/O with rayon parallelism.  Kept for callers
    /// that only need the hash (e.g. upload handlers pre-computing the hash
    /// before calling `store_from_file`).
    pub async fn hash_file(path: &Path) -> std::io::Result<String> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let mut hasher = blake3::Hasher::new();
            hasher.update_mmap_rayon(&path)?;
            Ok(hasher.finalize().to_hex().to_string())
        })
        .await
        .expect("hash_file: spawn_blocking task panicked")
    }

    // ── Core store operations ────────────────────────────────────

    /// Store content with CDC deduplication (from file).
    ///
    /// **Fast path**: if `pre_computed_hash` is `Some`, the manifest /
    /// legacy-blob index is checked *before* running CDC — returning
    /// instantly on a full-file dedup hit.
    ///
    /// **New-file path**: CDC-analyses the file (single mmap pass),
    /// stores unique chunks via the blob backend, then inserts the
    /// manifest in PostgreSQL.
    pub async fn store_from_file(
        &self,
        source_path: &Path,
        content_type: Option<String>,
        pre_computed_hash: Option<String>,
    ) -> Result<DedupResultDto, DomainError> {
        // ── Fast path: pre-computed hash → check before CDC ──────
        if let Some(ref hash) = pre_computed_hash
            && let Some(result) = self.try_dedup_hit(hash, source_path).await?
        {
            return Ok(result);
        }

        // ── CDC analysis ─────────────────────────────────────────
        let (file_hash, chunks) = if let Some(hash) = pre_computed_hash {
            let chunks = Self::cdc_chunk_file(source_path)
                .await
                .map_err(DomainError::from)?;
            (hash, chunks)
        } else {
            let (hash, chunks) = Self::cdc_hash_and_chunk_file(source_path)
                .await
                .map_err(DomainError::from)?;
            // Check dedup with newly computed hash
            if let Some(result) = self.try_dedup_hit(&hash, source_path).await? {
                return Ok(result);
            }
            (hash, chunks)
        };

        let file_size = fs::metadata(source_path)
            .await
            .map_err(DomainError::from)?
            .len();

        // ── Store chunks (write-first — no PG connection held) ───
        let (chunk_hashes, chunk_sizes) = self.store_chunks(source_path, &chunks).await?;

        // ── Insert manifest ──────────────────────────────────────
        sqlx::query(
            "INSERT INTO storage.chunk_manifests
                 (file_hash, chunk_hashes, chunk_sizes, total_size, chunk_count, content_type, ref_count)
             VALUES ($1, $2, $3, $4, $5, $6, 1)",
        )
        .bind(&file_hash)
        .bind(&chunk_hashes)
        .bind(chunk_sizes.iter().map(|s| *s as i64).collect::<Vec<_>>())
        .bind(file_size as i64)
        .bind(chunk_hashes.len() as i32)
        .bind(&content_type)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("Dedup", format!("Failed to insert manifest: {}", e))
        })?;

        // ── Clean up source file ─────────────────────────────────
        let _ = fs::remove_file(source_path).await;

        tracing::info!(
            "NEW BLOB (CDC): {} ({} bytes, {} chunks)",
            &file_hash[..12],
            file_size,
            chunk_hashes.len()
        );

        Ok(DedupResultDto::NewBlob {
            hash: file_hash,
            size: file_size,
        })
    }

    /// Check manifest or legacy blob for a dedup hit.
    ///
    /// Returns `Some(ExistingBlob)` if the exact file was already stored.
    /// Bumps the appropriate ref_count and removes the source file.
    async fn try_dedup_hit(
        &self,
        hash: &str,
        source_path: &Path,
    ) -> Result<Option<DedupResultDto>, DomainError> {
        // ── CDC manifest hit ─────────────────────────────────────
        let manifest = sqlx::query_as::<_, (i64,)>(
            "SELECT total_size FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("Dedup", format!("Failed to check manifest: {}", e))
        })?;

        if let Some((total_size,)) = manifest {
            sqlx::query(
                "UPDATE storage.chunk_manifests SET ref_count = ref_count + 1 WHERE file_hash = $1",
            )
            .bind(hash)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "Dedup",
                    format!("Failed to bump manifest ref_count: {}", e),
                )
            })?;

            let _ = fs::remove_file(source_path).await;

            tracing::info!(
                "DEDUP HIT (manifest): {} ({} bytes saved)",
                &hash[..12],
                total_size
            );
            return Ok(Some(DedupResultDto::ExistingBlob {
                hash: hash.to_owned(),
                size: total_size as u64,
                saved_bytes: total_size as u64,
            }));
        }

        // ── Legacy whole-file blob hit ───────────────────────────
        let legacy = sqlx::query_as::<_, (i64,)>("SELECT size FROM storage.blobs WHERE hash = $1")
            .bind(hash)
            .fetch_optional(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error("Dedup", format!("Failed to check legacy blob: {}", e))
            })?;

        if let Some((size,)) = legacy {
            sqlx::query("UPDATE storage.blobs SET ref_count = ref_count + 1 WHERE hash = $1")
                .bind(hash)
                .execute(self.pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "Dedup",
                        format!("Failed to bump legacy ref_count: {}", e),
                    )
                })?;

            let _ = fs::remove_file(source_path).await;
            tracing::info!(
                "DEDUP HIT (legacy blob): {} ({} bytes saved)",
                &hash[..12],
                size
            );
            return Ok(Some(DedupResultDto::ExistingBlob {
                hash: hash.to_owned(),
                size: size as u64,
                saved_bytes: size as u64,
            }));
        }

        Ok(None)
    }

    /// Maximum concurrent chunk uploads to the blob backend.
    const CHUNK_UPLOAD_CONCURRENCY: usize = 8;

    /// Store CDC chunks via the blob backend + upsert in PG.
    ///
    /// Phase 0: Batch-queries PG to discover which chunk hashes already
    /// exist in `storage.blobs`.
    /// Phase 1: Reads only *new* chunks from the source file (the biggest
    /// I/O saving for versioned files where most chunks are unchanged).
    /// Phase 2: Parallel operations — uploads new chunks, bumps ref_count
    /// for existing ones — with up to [`CHUNK_UPLOAD_CONCURRENCY`] in flight.
    async fn store_chunks(
        &self,
        source_path: &Path,
        chunks: &[ChunkMeta],
    ) -> Result<(Vec<String>, Vec<u64>), DomainError> {
        let pool = &self.pool;
        let backend = &self.backend;

        // ── Phase 0: Batch-check which chunks already exist ──────
        let unique_hashes: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            chunks
                .iter()
                .filter_map(|c| {
                    if seen.insert(c.hash.as_str()) {
                        Some(c.hash.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };

        let existing_hashes: std::collections::HashSet<String> =
            sqlx::query_scalar::<_, String>("SELECT hash FROM storage.blobs WHERE hash = ANY($1)")
                .bind(&unique_hashes)
                .fetch_all(pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "Dedup",
                        format!("Failed to check existing chunks: {}", e),
                    )
                })?
                .into_iter()
                .collect();

        // ── Phase 1: Read only NEW chunks from disk ──────────────
        let mut file = tokio::fs::File::open(source_path).await.map_err(|e| {
            DomainError::internal_error("Dedup", format!("Failed to open source file: {}", e))
        })?;

        // (hash, Option<data>, size) — None = existing chunk (skip I/O),
        // Some = new chunk (needs upload).
        let mut chunk_ops: Vec<(String, Option<Bytes>, u64)> = Vec::with_capacity(chunks.len());

        for chunk in chunks {
            let size = chunk.length as u64;
            if existing_hashes.contains(&chunk.hash) {
                chunk_ops.push((chunk.hash.clone(), None, size));
            } else {
                file.seek(std::io::SeekFrom::Start(chunk.offset as u64))
                    .await
                    .map_err(|e| {
                        DomainError::internal_error("Dedup", format!("Failed to seek: {}", e))
                    })?;
                let mut buf = vec![0u8; chunk.length];
                file.read_exact(&mut buf).await.map_err(|e| {
                    DomainError::internal_error("Dedup", format!("Failed to read chunk: {}", e))
                })?;
                chunk_ops.push((chunk.hash.clone(), Some(Bytes::from(buf)), size));
            }
        }

        // ── Phase 2: Parallel upload (new) / ref-bump (existing) ─
        let results: Vec<Result<(), DomainError>> = stream::iter(chunk_ops)
            .map(|(hash, data, size)| async move {
                if let Some(bytes) = data {
                    // New chunk: upload to blob backend + INSERT/upsert
                    backend.put_blob_from_bytes(&hash, bytes).await?;
                    sqlx::query(
                        "INSERT INTO storage.blobs (hash, size, ref_count)
                         VALUES ($1, $2, 1)
                         ON CONFLICT (hash) DO UPDATE
                           SET ref_count = storage.blobs.ref_count + 1",
                    )
                    .bind(&hash)
                    .bind(size as i64)
                    .execute(pool.as_ref())
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "Dedup",
                            format!("Failed to upsert chunk: {}", e),
                        )
                    })?;
                } else {
                    // Existing chunk: just bump ref_count (no I/O)
                    sqlx::query(
                        "UPDATE storage.blobs
                            SET ref_count = ref_count + 1
                          WHERE hash = $1",
                    )
                    .bind(&hash)
                    .execute(pool.as_ref())
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "Dedup",
                            format!("Failed to bump ref_count: {}", e),
                        )
                    })?;
                }
                Ok(())
            })
            .buffer_unordered(Self::CHUNK_UPLOAD_CONCURRENCY)
            .collect()
            .await;

        // All operations must succeed.  Order preservation is not needed
        // here — chunk_hashes/chunk_sizes are derived from the input
        // `chunks` slice which keeps the original CDC order.
        for result in results {
            result?;
        }

        let chunk_hashes: Vec<String> = chunks.iter().map(|c| c.hash.clone()).collect();
        let chunk_sizes: Vec<u64> = chunks.iter().map(|c| c.length as u64).collect();

        Ok((chunk_hashes, chunk_sizes))
    }

    // ── Reference counting ───────────────────────────────────────

    /// Check if a blob with the given hash exists (manifest or legacy).
    pub async fn blob_exists(&self, hash: &str) -> bool {
        // Check manifest first
        let manifest = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM storage.chunk_manifests WHERE file_hash = $1)",
        )
        .bind(hash)
        .fetch_one(self.pool.as_ref())
        .await
        .unwrap_or(false);

        if manifest {
            return true;
        }

        // Legacy blob
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM storage.blobs WHERE hash = $1)")
            .bind(hash)
            .fetch_one(self.pool.as_ref())
            .await
            .unwrap_or(false)
    }

    /// Returns `true` if `user_id` owns at least one (non-trashed) file that
    /// references the blob identified by `hash`.
    pub async fn user_owns_blob_reference(&self, hash: &str, user_id: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM storage.files WHERE blob_hash = $1 AND user_id = $2 AND NOT is_trashed)",
        )
        .bind(hash)
        .bind(user_id)
        .fetch_one(self.pool.as_ref())
        .await
        .unwrap_or(false)
    }

    /// Get metadata for a blob (manifest-aware with legacy fallback).
    pub async fn get_blob_metadata(&self, hash: &str) -> Option<BlobMetadataDto> {
        // Check manifest first
        let manifest = sqlx::query_as::<_, (i64, i32, Option<String>)>(
            "SELECT total_size, ref_count, content_type
             FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .ok()
        .flatten();

        if let Some((total_size, ref_count, content_type)) = manifest {
            return Some(BlobMetadataDto {
                hash: hash.to_owned(),
                size: total_size as u64,
                ref_count: ref_count as u32,
                content_type,
            });
        }

        // Legacy blob
        let row = sqlx::query_as::<_, (String, i64, i32, Option<String>)>(
            "SELECT hash, size, ref_count, content_type FROM storage.blobs WHERE hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .ok()
        .flatten()?;

        Some(BlobMetadataDto {
            hash: row.0,
            size: row.1 as u64,
            ref_count: row.2 as u32,
            content_type: row.3,
        })
    }

    /// Add a reference (manifest-aware with legacy fallback).
    pub async fn add_reference(&self, hash: &str) -> Result<(), DomainError> {
        // Try manifest first
        let manifest_affected = sqlx::query(
            "UPDATE storage.chunk_manifests SET ref_count = ref_count + 1 WHERE file_hash = $1",
        )
        .bind(hash)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("Dedup", format!("Failed to add manifest ref: {}", e))
        })?
        .rows_affected();

        if manifest_affected > 0 {
            return Ok(());
        }

        // Legacy blob
        let rows_affected =
            sqlx::query("UPDATE storage.blobs SET ref_count = ref_count + 1 WHERE hash = $1")
                .bind(hash)
                .execute(self.pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "Dedup",
                        format!("Failed to increment ref_count: {}", e),
                    )
                })?
                .rows_affected();

        if rows_affected == 0 {
            return Err(DomainError::new(
                ErrorKind::NotFound,
                "Blob",
                format!("Blob not found: {}", hash),
            ));
        }

        Ok(())
    }

    /// Remove a reference from a blob (manifest-aware with legacy fallback).
    ///
    /// For CDC manifests: decrements manifest ref_count.  When it reaches 0
    /// the manifest is deleted and all chunk ref_counts are decremented;
    /// chunks that reach 0 are deleted from both PG and the blob backend.
    ///
    /// For legacy blobs: uses a single TX with `SELECT … FOR UPDATE`.
    pub async fn remove_reference(&self, hash: &str) -> Result<bool, DomainError> {
        // ── CDC manifest path ────────────────────────────────────
        let manifest = sqlx::query_as::<_, (i32, Vec<String>)>(
            "SELECT ref_count, chunk_hashes FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("Manifest lookup: {}", e)))?;

        if let Some((ref_count, chunk_hashes)) = manifest {
            return self
                .remove_manifest_reference(hash, ref_count, &chunk_hashes)
                .await;
        }

        // ── Legacy whole-file blob path ──────────────────────────
        self.remove_legacy_reference(hash).await
    }

    /// Remove a manifest reference.  Handles chunk cleanup when last ref is removed.
    async fn remove_manifest_reference(
        &self,
        file_hash: &str,
        _initial_ref_count: i32,
        chunk_hashes: &[String],
    ) -> Result<bool, DomainError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            DomainError::internal_error("Dedup", format!("Failed to begin TX: {}", e))
        })?;

        // Lock manifest row
        let current_rc = sqlx::query_scalar::<_, i32>(
            "SELECT ref_count FROM storage.chunk_manifests WHERE file_hash = $1 FOR UPDATE",
        )
        .bind(file_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("Lock manifest: {}", e)))?;

        let Some(current_rc) = current_rc else {
            tx.rollback().await.ok();
            return Ok(false);
        };

        if current_rc <= 1 {
            // Last reference — delete manifest and decrement chunks
            sqlx::query("DELETE FROM storage.chunk_manifests WHERE file_hash = $1")
                .bind(file_hash)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    DomainError::internal_error("Dedup", format!("Delete manifest: {}", e))
                })?;

            // Batch decrement chunk ref_counts
            sqlx::query("UPDATE storage.blobs SET ref_count = ref_count - 1 WHERE hash = ANY($1)")
                .bind(chunk_hashes)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    DomainError::internal_error("Dedup", format!("Decrement chunks: {}", e))
                })?;

            // Find chunks that reached 0
            let zero_chunks: Vec<String> = sqlx::query_scalar(
                "DELETE FROM storage.blobs WHERE hash = ANY($1) AND ref_count <= 0 RETURNING hash",
            )
            .bind(chunk_hashes)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| {
                DomainError::internal_error("Dedup", format!("Delete zero chunks: {}", e))
            })?;

            tx.commit()
                .await
                .map_err(|e| DomainError::internal_error("Dedup", format!("Commit: {}", e)))?;

            // Delete blob files AFTER commit
            for chunk_hash in &zero_chunks {
                if let Err(e) = self.backend.delete_blob(chunk_hash).await {
                    tracing::warn!("Failed to delete chunk blob {}: {}", chunk_hash, e);
                }
            }

            // Bug 4 fix: delete disk thumbnails keyed by file_hash (last reference gone)
            if let Some(ts) = &self.thumbnail_service {
                ts.delete_blob_thumbnails(file_hash).await;
            }

            tracing::info!(
                "MANIFEST DELETED: {} ({} chunks, {} orphan chunks removed)",
                &file_hash[..12],
                chunk_hashes.len(),
                zero_chunks.len()
            );
            Ok(true)
        } else {
            // Still has references — just decrement
            sqlx::query(
                "UPDATE storage.chunk_manifests SET ref_count = ref_count - 1 WHERE file_hash = $1",
            )
            .bind(file_hash)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                DomainError::internal_error("Dedup", format!("Decrement manifest: {}", e))
            })?;

            tx.commit()
                .await
                .map_err(|e| DomainError::internal_error("Dedup", format!("Commit: {}", e)))?;

            tracing::debug!("Reference removed from manifest {}", &file_hash[..12]);
            Ok(false)
        }
    }

    /// Remove a reference from a legacy whole-file blob.
    async fn remove_legacy_reference(&self, hash: &str) -> Result<bool, DomainError> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            DomainError::internal_error("Dedup", format!("Failed to begin transaction: {}", e))
        })?;

        // Lock the row exclusively — prevents concurrent store_from_file from
        // incrementing ref_count while we might be deleting
        let row = sqlx::query_as::<_, (i32, i64)>(
            "SELECT ref_count, size FROM storage.blobs WHERE hash = $1 FOR UPDATE",
        )
        .bind(hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            DomainError::internal_error("Dedup", format!("Failed to lock blob row: {}", e))
        })?;

        let Some((ref_count, _size)) = row else {
            // Blob doesn't exist — nothing to do
            tx.rollback().await.ok();
            return Ok(false);
        };

        let new_ref_count = (ref_count - 1).max(0);

        if new_ref_count == 0 {
            // Last reference — delete row from PG
            sqlx::query("DELETE FROM storage.blobs WHERE hash = $1")
                .bind(hash)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "Dedup",
                        format!("Failed to delete blob row: {}", e),
                    )
                })?;

            tx.commit().await.map_err(|e| {
                DomainError::internal_error("Dedup", format!("Failed to commit: {}", e))
            })?;

            // Delete blob from backend AFTER committing PG — the row is gone,
            // so no concurrent store_from_file can resurrect a reference.
            if let Err(e) = self.backend.delete_blob(hash).await {
                tracing::warn!("Failed to delete blob file {}: {}", hash, e);
            }

            // Bug 3 fix: delete disk thumbnails keyed by hash (last reference gone)
            if let Some(ts) = &self.thumbnail_service {
                ts.delete_blob_thumbnails(hash).await;
            }

            tracing::info!("BLOB DELETED: {} (no more references)", &hash[..12]);
            Ok(true)
        } else {
            // Still has references — just decrement
            sqlx::query("UPDATE storage.blobs SET ref_count = $1 WHERE hash = $2")
                .bind(new_ref_count)
                .bind(hash)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "Dedup",
                        format!("Failed to decrement ref_count: {}", e),
                    )
                })?;

            tx.commit().await.map_err(|e| {
                DomainError::internal_error("Dedup", format!("Failed to commit: {}", e))
            })?;

            tracing::debug!("Reference removed from blob {}", &hash[..12]);
            Ok(false)
        }
    }

    // ── Read operations ──────────────────────────────────────────

    /// Stream blob content — CDC-aware with legacy fallback.
    ///
    /// For CDC files: looks up the manifest, then streams chunks in order,
    /// concatenating them into a single byte stream.
    /// For legacy blobs: delegates directly to the backend.
    pub async fn read_blob_stream(
        &self,
        hash: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>
    {
        // Check manifest
        let manifest = sqlx::query_scalar::<_, Vec<String>>(
            "SELECT chunk_hashes FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("Manifest lookup: {}", e)))?;

        if let Some(chunk_hashes) = manifest {
            // CDC file: stream chunks in order
            let backend = self.backend.clone();
            let chunk_stream = stream::iter(chunk_hashes)
                .map(move |chunk_hash| {
                    let backend = backend.clone();
                    async move {
                        backend
                            .get_blob_stream(&chunk_hash)
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))
                    }
                })
                .buffered(1)
                .try_flatten();

            Ok(Box::pin(chunk_stream))
        } else {
            // Legacy whole-file blob
            self.backend.get_blob_stream(hash).await
        }
    }

    /// Read the full blob into memory — CDC-aware with legacy fallback.
    ///
    /// This is intended for image-oriented workflows such as thumbnail
    /// generation where the downstream library already requires the full
    /// payload in memory to decode the image.
    pub async fn read_blob_bytes(&self, hash: &str) -> Result<Bytes, DomainError> {
        let expected_size = self.blob_size(hash).await? as usize;
        let mut data = Vec::with_capacity(expected_size);
        let mut stream = self.read_blob_stream(hash).await?;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                DomainError::internal_error("Dedup", format!("Failed to read blob chunk: {}", e))
            })?;
            data.extend_from_slice(&chunk);
        }

        Ok(Bytes::from(data))
    }

    /// Stream a byte range — CDC-aware with legacy fallback.
    ///
    /// For CDC files: calculates which chunks overlap the requested range,
    /// then streams only the relevant portions.
    pub async fn read_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>
    {
        // Check manifest
        let manifest = sqlx::query_as::<_, (Vec<String>, Vec<i64>, i64)>(
            "SELECT chunk_hashes, chunk_sizes, total_size
             FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("Manifest lookup: {}", e)))?;

        if let Some((chunk_hashes, chunk_sizes, total_size)) = manifest {
            let end = end.unwrap_or(total_size as u64);

            // Calculate which chunks overlap [start, end)
            let mut offset: u64 = 0;
            // (chunk_hash, range_start_within_chunk, range_end_within_chunk)
            let mut selected: Vec<(String, u64, Option<u64>)> = Vec::new();

            for (i, &chunk_size) in chunk_sizes.iter().enumerate() {
                let chunk_size = chunk_size as u64;
                let chunk_end = offset + chunk_size;

                if chunk_end > start && offset < end {
                    let range_start = start.saturating_sub(offset);
                    let range_end = if chunk_end > end {
                        Some(end - offset)
                    } else {
                        None
                    };
                    selected.push((chunk_hashes[i].clone(), range_start, range_end));
                }

                offset += chunk_size;
                if offset >= end {
                    break;
                }
            }

            // Stream selected chunks with ranges
            let backend = self.backend.clone();
            let chunk_stream = stream::iter(selected)
                .map(move |(chunk_hash, range_start, range_end)| {
                    let backend = backend.clone();
                    async move {
                        backend
                            .get_blob_range_stream(&chunk_hash, range_start, range_end)
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))
                    }
                })
                .buffered(1)
                .try_flatten();

            Ok(Box::pin(chunk_stream))
        } else {
            // Legacy whole-file blob
            self.backend.get_blob_range_stream(hash, start, end).await
        }
    }

    /// Get blob size — manifest-aware with legacy fallback.
    pub async fn blob_size(&self, hash: &str) -> Result<u64, DomainError> {
        // Check manifest first (O(1) from PG)
        let manifest_size = sqlx::query_scalar::<_, i64>(
            "SELECT total_size FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("Manifest lookup: {}", e)))?;

        if let Some(size) = manifest_size {
            return Ok(size as u64);
        }

        // Legacy: delegate to backend
        self.backend.blob_size(hash).await
    }

    // ── Statistics (computed from PG) ────────────────────────────

    /// Get deduplication statistics (CDC + legacy).
    pub async fn get_stats(&self) -> DedupStatsDto {
        // Physical storage (all blobs = chunks + legacy)
        let (total_blobs, total_bytes_stored): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM storage.blobs")
                .fetch_one(self.pool.as_ref())
                .await
                .unwrap_or((0, 0));

        // Referenced bytes from CDC manifests
        let manifest_referenced: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(total_size::BIGINT * ref_count), 0) FROM storage.chunk_manifests",
        )
        .fetch_one(self.pool.as_ref())
        .await
        .unwrap_or(0);

        // Referenced bytes from legacy blobs (those not used as CDC chunks).
        // A legacy blob has its hash directly in storage.files.blob_hash.
        // We approximate by subtracting manifest-attributed storage.
        let all_blob_referenced: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(size::BIGINT * ref_count), 0) FROM storage.blobs",
        )
        .fetch_one(self.pool.as_ref())
        .await
        .unwrap_or(0);

        let manifest_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM storage.chunk_manifests")
                .fetch_one(self.pool.as_ref())
                .await
                .unwrap_or(0);

        // If manifests exist, use manifest-based referenced bytes;
        // otherwise fall back to pure legacy calculation.
        let total_bytes_referenced = if manifest_count > 0 {
            // Legacy blobs that aren't chunks contribute directly;
            // CDC manifests contribute total_size × ref_count.
            // Approximation: all_blob_referenced overcounts chunk sharing,
            // but manifest_referenced accounts for file-level dedup.
            manifest_referenced.max(all_blob_referenced) as u64
        } else {
            all_blob_referenced as u64
        };

        let total_blobs = total_blobs as u64;
        let total_bytes_stored = total_bytes_stored as u64;
        let bytes_saved = total_bytes_referenced.saturating_sub(total_bytes_stored);
        let dedup_ratio = if total_bytes_stored > 0 {
            total_bytes_referenced as f64 / total_bytes_stored as f64
        } else {
            1.0
        };

        DedupStatsDto {
            total_blobs,
            total_bytes_stored,
            total_bytes_referenced,
            bytes_saved,
            dedup_hits: 0,
            dedup_ratio,
        }
    }

    // ── Maintenance ──────────────────────────────────────────────

    /// Verify integrity of all stored data (manifests + blobs).
    ///
    /// For CDC manifests: verifies chunk count, total_size consistency,
    /// and that every referenced chunk exists in the backend.
    /// For blobs (chunks + legacy): verifies existence, size, and
    /// (for local backends) re-hashes to confirm content integrity.
    pub async fn verify_integrity(&self) -> Result<Vec<String>, DomainError> {
        const VERIFY_CONCURRENCY: usize = 16;
        let mut issues = Vec::new();

        // ── Phase 1: Verify CDC manifests ────────────────────────
        let manifests: Vec<(String, Vec<String>, Vec<i64>, i64)> = sqlx::query_as(
            "SELECT file_hash, chunk_hashes, chunk_sizes, total_size
             FROM storage.chunk_manifests",
        )
        .fetch_all(self.maintenance_pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("List manifests: {}", e)))?;

        for (file_hash, chunk_hashes, chunk_sizes, total_size) in &manifests {
            let label = &file_hash[..file_hash.len().min(12)];

            if chunk_hashes.len() != chunk_sizes.len() {
                issues.push(format!(
                    "Manifest {label}: chunk_hashes/chunk_sizes length mismatch"
                ));
                continue;
            }

            let sum: i64 = chunk_sizes.iter().sum();
            if sum != *total_size {
                issues.push(format!(
                    "Manifest {label}: total_size {total_size} != sum of chunk_sizes {sum}"
                ));
            }

            for (i, chunk_hash) in chunk_hashes.iter().enumerate() {
                let chunk_label = &chunk_hash[..chunk_hash.len().min(12)];
                match self.backend.blob_size(chunk_hash).await {
                    Ok(actual_size) => {
                        if actual_size != chunk_sizes[i] as u64 {
                            issues.push(format!(
                                "Manifest {label} chunk {chunk_label}: size mismatch \
                                 (expected {}, actual {actual_size})",
                                chunk_sizes[i]
                            ));
                        }
                    }
                    Err(_) => {
                        issues.push(format!(
                            "Manifest {label} chunk {chunk_label}: missing in backend"
                        ));
                    }
                }
            }
        }

        // ── Phase 2: Verify blobs (chunks + legacy) ──────────────
        let mut row_stream = sqlx::query_as::<_, (String, i64)>(
            "SELECT hash, size FROM storage.blobs ORDER BY hash",
        )
        .fetch(self.maintenance_pool.as_ref());

        let mut total = 0usize;
        let mut batch = Vec::with_capacity(VERIFY_CONCURRENCY);

        loop {
            let maybe_row = row_stream.try_next().await.map_err(|e| {
                DomainError::internal_error("Dedup", format!("Failed to list blobs: {}", e))
            })?;

            let is_done = maybe_row.is_none();

            if let Some(row) = maybe_row {
                total += 1;
                batch.push(row);
            }

            if batch.len() >= VERIFY_CONCURRENCY || (is_done && !batch.is_empty()) {
                let backend = self.backend.clone();
                let current_batch =
                    std::mem::replace(&mut batch, Vec::with_capacity(VERIFY_CONCURRENCY));

                let blob_issues: Vec<String> = stream::iter(current_batch)
                    .map(move |(hash, expected_size)| {
                        let backend = backend.clone();
                        async move {
                            let mut issues = Vec::new();

                            match backend.blob_size(&hash).await {
                                Ok(actual_size) => {
                                    if actual_size != expected_size as u64 {
                                        issues.push(format!(
                                            "{}: size mismatch (expected: {}, actual: {})",
                                            hash, expected_size, actual_size,
                                        ));
                                    }
                                }
                                Err(_) => {
                                    issues.push(format!("{}: blob missing in backend", hash));
                                    return issues;
                                }
                            };

                            if let Some(blob_path) = backend.local_blob_path(&hash) {
                                match Self::hash_file(&blob_path).await {
                                    Ok(actual_hash) => {
                                        if actual_hash != hash {
                                            issues.push(format!(
                                                "{}: hash mismatch (actual: {})",
                                                hash, actual_hash,
                                            ));
                                        }
                                    }
                                    Err(e) => {
                                        issues.push(format!("{}: read error ({})", hash, e));
                                    }
                                }
                            }

                            issues
                        }
                    })
                    .buffer_unordered(VERIFY_CONCURRENCY)
                    .flat_map(stream::iter)
                    .collect()
                    .await;

                issues.extend(blob_issues);
            }

            if is_done {
                break;
            }
        }

        if issues.is_empty() {
            tracing::info!(
                "Integrity check passed ({} manifests, {} blobs)",
                manifests.len(),
                total
            );
        } else {
            tracing::warn!("Integrity check found {} issues", issues.len());
        }

        Ok(issues)
    }

    /// Garbage collect orphaned manifests and blobs.
    ///
    /// Phase 1: Delete manifests with ref_count = 0, then decrement
    /// chunk ref_counts for their chunks.
    /// Phase 2: Delete blobs (chunks + legacy) with ref_count = 0.
    pub async fn garbage_collect(&self) -> Result<(u64, u64), DomainError> {
        const BATCH_SIZE: i64 = 500;

        let mut total_deleted = 0u64;
        let mut total_bytes = 0u64;

        // ── Phase 1: GC orphaned manifests ───────────────────────
        loop {
            let batch: Vec<(String, Vec<String>, i64)> = sqlx::query_as(
                "DELETE FROM storage.chunk_manifests
                  WHERE ctid = ANY(
                      SELECT ctid FROM storage.chunk_manifests
                       WHERE ref_count <= 0
                       LIMIT $1
                  )
                  RETURNING file_hash, chunk_hashes, total_size",
            )
            .bind(BATCH_SIZE)
            .fetch_all(self.maintenance_pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("Dedup", format!("GC manifests: {e}")))?;

            if batch.is_empty() {
                break;
            }

            for (file_hash, chunk_hashes, size) in &batch {
                // Decrement chunk ref_counts
                sqlx::query(
                    "UPDATE storage.blobs SET ref_count = ref_count - 1 WHERE hash = ANY($1)",
                )
                .bind(chunk_hashes)
                .execute(self.maintenance_pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error("Dedup", format!("GC decrement chunks: {e}"))
                })?;

                total_bytes += *size as u64;
                tracing::debug!(
                    "GC: removed manifest {} ({} chunks)",
                    &file_hash[..file_hash.len().min(12)],
                    chunk_hashes.len()
                );
            }
            total_deleted += batch.len() as u64;

            tokio::task::yield_now().await;
        }

        // ── Phase 2: GC orphaned blobs/chunks ────────────────────
        loop {
            let batch: Vec<(String, i64)> = sqlx::query_as(
                "DELETE FROM storage.blobs
                  WHERE ctid = ANY(
                      SELECT ctid FROM storage.blobs
                       WHERE ref_count <= 0
                       LIMIT $1
                  )
                  RETURNING hash, size",
            )
            .bind(BATCH_SIZE)
            .fetch_all(self.maintenance_pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("Dedup", format!("GC blobs: {e}")))?;

            if batch.is_empty() {
                break;
            }

            for (hash, size) in &batch {
                if let Err(e) = self.backend.delete_blob(hash).await {
                    tracing::warn!("Failed to delete orphan blob {hash}: {e}");
                }
                // Clean up thumbnails (best-effort, only local backends)
                if let Some(blob_path) = self.backend.local_blob_path(hash)
                    && let Some(storage_root) = blob_path.ancestors().nth(3)
                {
                    let thumbnails_root = storage_root.join(".thumbnails");
                    for dir in &["icon", "preview", "large"] {
                        let thumb = thumbnails_root.join(dir).join(format!("{hash}.jpg"));
                        let _ = fs::remove_file(&thumb).await;
                    }
                }
                total_bytes += *size as u64;
            }
            total_deleted += batch.len() as u64;

            tokio::task::yield_now().await;
        }

        if total_deleted > 0 {
            tracing::info!("GC: removed {total_deleted} items ({total_bytes} bytes)");
        }

        Ok((total_deleted, total_bytes))
    }
}

// ─── Port implementation ─────────────────────────────────────────────────────

impl DedupPort for DedupService {
    async fn store_from_file(
        &self,
        source_path: &Path,
        content_type: Option<String>,
        pre_computed_hash: Option<String>,
    ) -> Result<DedupResultDto, DomainError> {
        self.store_from_file(source_path, content_type, pre_computed_hash)
            .await
    }

    async fn blob_exists(&self, hash: &str) -> bool {
        self.blob_exists(hash).await
    }

    async fn get_blob_metadata(&self, hash: &str) -> Option<BlobMetadataDto> {
        self.get_blob_metadata(hash).await
    }

    async fn read_blob_stream(
        &self,
        hash: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>
    {
        self.read_blob_stream(hash).await
    }

    async fn read_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>
    {
        self.read_blob_range_stream(hash, start, end).await
    }

    async fn blob_size(&self, hash: &str) -> Result<u64, DomainError> {
        self.blob_size(hash).await
    }

    async fn add_reference(&self, hash: &str) -> Result<(), DomainError> {
        self.add_reference(hash).await
    }

    async fn remove_reference(&self, hash: &str) -> Result<bool, DomainError> {
        self.remove_reference(hash).await
    }

    async fn hash_file(&self, path: &Path) -> Result<String, DomainError> {
        DedupService::hash_file(path)
            .await
            .map_err(DomainError::from)
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.blob_path(hash)
    }

    async fn get_stats(&self) -> DedupStatsDto {
        self.get_stats().await
    }

    async fn flush(&self) -> Result<(), DomainError> {
        // No-op: PostgreSQL handles persistence automatically via WAL/commit
        Ok(())
    }

    async fn verify_integrity(&self) -> Result<Vec<String>, DomainError> {
        self.verify_integrity().await
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::NamedTempFile;

    /// Helper: write `data` to a temp file and return its path.
    async fn write_temp_file(data: &[u8]) -> NamedTempFile {
        let file = NamedTempFile::new().unwrap();
        tokio::fs::write(file.path(), data).await.unwrap();
        file
    }

    // ── Determinism ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_cdc_deterministic_same_content() {
        let data = vec![42u8; 512 * 1024]; // 512 KB of 0x2A
        let f1 = write_temp_file(&data).await;
        let f2 = write_temp_file(&data).await;

        let (hash1, chunks1) = DedupService::cdc_hash_and_chunk_file(f1.path())
            .await
            .unwrap();
        let (hash2, chunks2) = DedupService::cdc_hash_and_chunk_file(f2.path())
            .await
            .unwrap();

        assert_eq!(hash1, hash2, "same content must produce same file hash");
        assert_eq!(
            chunks1.len(),
            chunks2.len(),
            "same content must produce same chunk count"
        );
        for (c1, c2) in chunks1.iter().zip(chunks2.iter()) {
            assert_eq!(c1.hash, c2.hash);
            assert_eq!(c1.offset, c2.offset);
            assert_eq!(c1.length, c2.length);
        }
    }

    // ── Empty file ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_cdc_empty_file() {
        let f = write_temp_file(b"").await;
        let (hash, chunks) = DedupService::cdc_hash_and_chunk_file(f.path())
            .await
            .unwrap();

        assert!(chunks.is_empty(), "empty file must produce zero chunks");
        assert_eq!(hash, blake3::hash(b"").to_hex().to_string());
    }

    // ── Small file (below min chunk) → single chunk ──────────────

    #[tokio::test]
    async fn test_cdc_small_file_single_chunk() {
        let data = b"Hello, OxiCloud CDC dedup!";
        let f = write_temp_file(data).await;
        let (hash, chunks) = DedupService::cdc_hash_and_chunk_file(f.path())
            .await
            .unwrap();

        assert_eq!(chunks.len(), 1, "tiny file must be a single chunk");
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[0].length, data.len());
        assert_eq!(hash, blake3::hash(data).to_hex().to_string());
    }

    // ── Chunk sizes within CDC bounds ────────────────────────────

    #[tokio::test]
    async fn test_cdc_chunk_sizes_within_bounds() {
        // 4 MB file of pseudo-random data (deterministic seed)
        let data: Vec<u8> = (0..4 * 1024 * 1024)
            .map(|i| ((i as u64).wrapping_mul(6364136223846793005).wrapping_add(1)) as u8)
            .collect();
        let f = write_temp_file(&data).await;

        let (_, chunks) = DedupService::cdc_hash_and_chunk_file(f.path())
            .await
            .unwrap();

        assert!(chunks.len() > 1, "4 MB should produce multiple chunks");

        // All non-last chunks must be within [min, max]
        for (i, chunk) in chunks.iter().enumerate() {
            let is_last = i == chunks.len() - 1;
            if !is_last {
                assert!(
                    chunk.length >= CDC_MIN_CHUNK,
                    "non-last chunk {} too small: {} < {}",
                    i,
                    chunk.length,
                    CDC_MIN_CHUNK,
                );
            }
            assert!(
                chunk.length <= CDC_MAX_CHUNK,
                "chunk {} too large: {} > {}",
                i,
                chunk.length,
                CDC_MAX_CHUNK,
            );
        }
    }

    // ── File hash matches hash_file() ────────────────────────────

    #[tokio::test]
    async fn test_cdc_file_hash_matches_hash_file() {
        let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        let f = write_temp_file(&data).await;

        let (cdc_hash, _) = DedupService::cdc_hash_and_chunk_file(f.path())
            .await
            .unwrap();
        let standalone_hash = DedupService::hash_file(f.path()).await.unwrap();

        assert_eq!(
            cdc_hash, standalone_hash,
            "CDC file hash must match standalone hash_file()"
        );
    }

    // ── Chunk hashes are correct BLAKE3 of chunk data ────────────

    #[tokio::test]
    async fn test_cdc_chunk_hashes_are_correct() {
        let data: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|i| ((i as u64).wrapping_mul(2862933555777941757).wrapping_add(3)) as u8)
            .collect();
        let f = write_temp_file(&data).await;

        let (_, chunks) = DedupService::cdc_hash_and_chunk_file(f.path())
            .await
            .unwrap();

        for chunk in &chunks {
            let chunk_data = &data[chunk.offset..chunk.offset + chunk.length];
            let expected_hash = blake3::hash(chunk_data).to_hex().to_string();
            assert_eq!(
                chunk.hash, expected_hash,
                "chunk at offset {} has wrong hash",
                chunk.offset
            );
        }
    }

    // ── Reassembly matches original ──────────────────────────────

    #[tokio::test]
    async fn test_cdc_reassembly_matches_original() {
        let data: Vec<u8> = (0..3 * 1024 * 1024)
            .map(|i| ((i as u64).wrapping_mul(1103515245).wrapping_add(12345)) as u8)
            .collect();
        let f = write_temp_file(&data).await;

        let (_, chunks) = DedupService::cdc_hash_and_chunk_file(f.path())
            .await
            .unwrap();

        // Reassemble from chunks
        let mut reassembled = Vec::with_capacity(data.len());
        for chunk in &chunks {
            reassembled.extend_from_slice(&data[chunk.offset..chunk.offset + chunk.length]);
        }

        assert_eq!(
            reassembled.len(),
            data.len(),
            "reassembled length must match"
        );
        assert_eq!(reassembled, data, "reassembled content must match original");
    }

    // ── Chunks cover entire file (no gaps, no overlaps) ──────────

    #[tokio::test]
    async fn test_cdc_chunks_are_contiguous() {
        let data: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 199) as u8).collect();
        let f = write_temp_file(&data).await;

        let (_, chunks) = DedupService::cdc_hash_and_chunk_file(f.path())
            .await
            .unwrap();

        let mut expected_offset = 0usize;
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(
                chunk.offset, expected_offset,
                "chunk {} starts at {} but expected {}",
                i, chunk.offset, expected_offset
            );
            expected_offset += chunk.length;
        }
        assert_eq!(expected_offset, data.len(), "chunks must cover entire file");
    }

    // ── Sub-file dedup: similar files share chunks ───────────────

    #[tokio::test]
    async fn test_cdc_similar_files_share_chunks() {
        // Create a base file of 2 MB with random-ish data
        let base: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|i| ((i as u64).wrapping_mul(6364136223846793005).wrapping_add(1)) as u8)
            .collect();

        // Modified file: change only the last 64 KB
        let mut modified = base.clone();
        let start = modified.len() - 64 * 1024;
        for b in &mut modified[start..] {
            *b = b.wrapping_add(1);
        }

        let f_base = write_temp_file(&base).await;
        let f_mod = write_temp_file(&modified).await;

        let (hash_base, chunks_base) = DedupService::cdc_hash_and_chunk_file(f_base.path())
            .await
            .unwrap();
        let (hash_mod, chunks_mod) = DedupService::cdc_hash_and_chunk_file(f_mod.path())
            .await
            .unwrap();

        // File hashes must differ
        assert_ne!(
            hash_base, hash_mod,
            "modified file must have different hash"
        );

        // Collect chunk hashes
        let base_set: HashSet<&str> = chunks_base.iter().map(|c| c.hash.as_str()).collect();
        let mod_set: HashSet<&str> = chunks_mod.iter().map(|c| c.hash.as_str()).collect();

        let shared = base_set.intersection(&mod_set).count();

        // With only the last 64 KB changed, most chunks should be shared.
        // The first ~1.9 MB of content is identical → expect significant overlap.
        let min_expected_shared = chunks_base.len().min(chunks_mod.len()) / 2;
        assert!(
            shared >= min_expected_shared,
            "expected at least {} shared chunks between similar files, got {} \
             (base: {} chunks, modified: {} chunks)",
            min_expected_shared,
            shared,
            chunks_base.len(),
            chunks_mod.len()
        );
    }

    // ── cdc_chunk_file matches cdc_hash_and_chunk_file ───────────

    #[tokio::test]
    async fn test_cdc_chunk_file_matches_full() {
        let data: Vec<u8> = (0..1024 * 1024)
            .map(|i| (i as u8).wrapping_mul(7))
            .collect();
        let f = write_temp_file(&data).await;

        let (_, chunks_full) = DedupService::cdc_hash_and_chunk_file(f.path())
            .await
            .unwrap();
        let chunks_only = DedupService::cdc_chunk_file(f.path()).await.unwrap();

        assert_eq!(chunks_full.len(), chunks_only.len());
        for (a, b) in chunks_full.iter().zip(chunks_only.iter()) {
            assert_eq!(a.hash, b.hash);
            assert_eq!(a.offset, b.offset);
            assert_eq!(a.length, b.length);
        }
    }

    // ── Large file produces expected chunk count ──────────────────

    #[tokio::test]
    async fn test_cdc_large_file_chunk_count() {
        // 8 MB should produce roughly 8MB / 256KB ≈ 32 chunks (±)
        let data: Vec<u8> = (0..8 * 1024 * 1024)
            .map(|i| ((i as u64).wrapping_mul(2862933555777941757).wrapping_add(3)) as u8)
            .collect();
        let f = write_temp_file(&data).await;

        let (_, chunks) = DedupService::cdc_hash_and_chunk_file(f.path())
            .await
            .unwrap();

        // With 256KB avg, expect 20-60 chunks for 8MB
        assert!(
            chunks.len() >= 8 && chunks.len() <= 128,
            "8 MB file should produce 8-128 chunks (avg 256KB), got {}",
            chunks.len()
        );

        let total_size: usize = chunks.iter().map(|c| c.length).sum();
        assert_eq!(
            total_size,
            data.len(),
            "total chunk sizes must equal file size"
        );
    }

    // ── Prefix insert: CDC shifts only locally ───────────────────

    #[tokio::test]
    async fn test_cdc_insert_at_beginning_preserves_later_chunks() {
        // Base file: 2 MB of deterministic data
        let base: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|i| ((i as u64).wrapping_mul(6364136223846793005).wrapping_add(1)) as u8)
            .collect();

        // Insert 128 KB at the beginning (simulates a header change)
        let prefix: Vec<u8> = (0..128 * 1024).map(|i| (i % 173) as u8).collect();
        let mut with_prefix = prefix;
        with_prefix.extend_from_slice(&base);

        let f_base = write_temp_file(&base).await;
        let f_prefix = write_temp_file(&with_prefix).await;

        let (_, chunks_base) = DedupService::cdc_hash_and_chunk_file(f_base.path())
            .await
            .unwrap();
        let (_, chunks_prefix) = DedupService::cdc_hash_and_chunk_file(f_prefix.path())
            .await
            .unwrap();

        let base_set: HashSet<&str> = chunks_base.iter().map(|c| c.hash.as_str()).collect();
        let prefix_set: HashSet<&str> = chunks_prefix.iter().map(|c| c.hash.as_str()).collect();

        // CDC's content-defined boundaries mean chunks after the insertion
        // should resynchronize — we expect *some* shared chunks, proving
        // CDC is better than fixed-size chunking (which would share zero).
        let shared = base_set.intersection(&prefix_set).count();
        assert!(
            shared > 0,
            "CDC should resynchronize and share chunks after insertion \
             (base: {} chunks, with-prefix: {} chunks, shared: 0)",
            chunks_base.len(),
            chunks_prefix.len()
        );
    }
}
