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
//! **Single-pass streaming ingest** (store_from_stream):
//!   1. FastCDC boundaries, per-chunk BLAKE3 and the whole-file BLAKE3 are
//!      all computed WHILE the bytes arrive — no spool file, no mmap
//!      re-read. Peak RAM stays bounded (current chunk + one small batch).
//!   2. Per batch of distinct chunks, ONE `UPDATE … RETURNING` bumps
//!      ref_count on already-known chunks (pinning them against concurrent
//!      reclaim for the rest of the upload) and atomically classifies the
//!      rest as new — no check-then-bump TOCTOU window.
//!   3. Only *new* chunks are written to the blob backend (unsynced,
//!      bounded concurrency). Bytes the store already knows never touch
//!      disk — a full dedup hit performs zero content writes.
//!   4. At end of stream ONE batched fsync sweep makes the new chunks
//!      durable, then ONE batched INSERT registers them — durability
//!      before visibility.
//!   5. Single manifest INSERT (~few ms). An identical concurrent upload
//!      is resolved via ON CONFLICT: the loser releases its chunk
//!      references and turns into a dedup hit.
//!   6. PG connections are never held during disk I/O.
//!
//! Benefits:
//! - Each uploaded byte hits the disk at most ONCE (dedup hits: zero)
//! - Sub-file dedup: edited files share unchanged chunks
//! - ACID durability — crash-safe, zero orphaned index entries
//! - 60-80% storage reduction for versioned / edited files

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use futures::{Stream, TryStreamExt};

use sqlx::PgPool;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio_util::io::StreamReader;

use crate::application::ports::blob_lifecycle::BlobLifecycleHook;
use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::application::ports::dedup_ports::{
    BlobMetadataDto, DedupPort, DedupResultDto, DedupStatsDto,
};
use crate::application::services::blob_lifecycle_service::BlobLifecycleService;
use crate::domain::errors::{DomainError, ErrorKind};

// ── CDC Constants ────────────────────────────────────────────────────────────

/// Minimum CDC chunk size (64 KB).
pub const CDC_MIN_CHUNK: usize = 65_536;
/// Average CDC chunk size (256 KB).
pub const CDC_AVG_CHUNK: usize = 262_144;
/// Maximum CDC chunk size (1 MB).
pub const CDC_MAX_CHUNK: usize = 1_048_576;

// ── CDC helper types ─────────────────────────────────────────────────────────

/// Everything a streaming chunk ingest learned about its byte stream.
///
/// Produced by [`DedupService::ingest_chunks_from_stream`]. On success the
/// ingest session holds exactly ONE `storage.blobs.ref_count` reference per
/// *distinct* chunk hash; the caller must either attach those references to
/// a manifest or hand them back via `release_chunk_refs`.
struct ChunkIngestOutcome {
    /// BLAKE3 of the complete byte stream (the future manifest key).
    file_hash: String,
    /// Total bytes consumed from the stream.
    total_size: u64,
    /// Per-occurrence chunk hashes, in file order (the manifest layout).
    chunk_hashes: Vec<String>,
    /// Per-occurrence chunk sizes, in file order.
    chunk_sizes: Vec<u64>,
    /// How many distinct chunks were actually written to the backend.
    newly_written: usize,
}

impl ChunkIngestOutcome {
    /// Distinct chunk hashes — the set this ingest holds one reference on each.
    fn distinct_hashes(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.chunk_hashes
            .iter()
            .filter(|h| seen.insert(h.as_str()))
            .cloned()
            .collect()
    }
}

/// Compensation guard for an in-flight ingest session.
///
/// Tracks the two side effects a session accumulates before its chunks are
/// fully registered: ref_count pins taken on pre-existing chunks and freshly
/// written (still unregistered) chunk files. If the session future is dropped
/// mid-stream — a client disconnect aborts the whole handler future — the
/// guard spawns a rollback so pinned chunks don't leak references forever and
/// written files become GC-collectible rows instead of invisible orphans.
struct IngestGuard {
    pool: Arc<PgPool>,
    backend: Arc<dyn BlobStorageBackend>,
    /// Pre-existing chunks whose ref_count this session bumped (distinct).
    pinned: Vec<String>,
    /// Chunks written to the backend but not yet registered: (hash, size).
    written: Vec<(String, i64)>,
    armed: bool,
}

impl IngestGuard {
    fn new(pool: Arc<PgPool>, backend: Arc<dyn BlobStorageBackend>) -> Self {
        Self {
            pool,
            backend,
            pinned: Vec::new(),
            written: Vec::new(),
            armed: true,
        }
    }

    /// The session's chunks are fully registered — references now belong to
    /// the caller, nothing to compensate.
    fn disarm(mut self) {
        self.armed = false;
    }

    /// Deterministic rollback for handled errors (awaited inline, unlike the
    /// spawned Drop path).
    async fn rollback(mut self) {
        self.armed = false;
        let pinned = std::mem::take(&mut self.pinned);
        let written = std::mem::take(&mut self.written);
        Self::run_rollback(self.pool.clone(), self.backend.clone(), pinned, written).await;
    }

    /// Release pins and surface written-but-unregistered chunk files to GC.
    ///
    /// Best-effort: every step logs instead of failing — the worst outcome of
    /// a failed rollback is a bounded ref_count over-count (storage leak),
    /// never data loss.
    async fn run_rollback(
        pool: Arc<PgPool>,
        backend: Arc<dyn BlobStorageBackend>,
        pinned: Vec<String>,
        written: Vec<(String, i64)>,
    ) {
        if !pinned.is_empty()
            && let Err(e) = sqlx::query(
                "UPDATE storage.blobs SET ref_count = GREATEST(ref_count - 1, 0)
                  WHERE hash = ANY($1)",
            )
            .bind(&pinned)
            .execute(pool.as_ref())
            .await
        {
            tracing::warn!(
                "Ingest rollback: failed to release {} chunk pins: {e}",
                pinned.len()
            );
        }

        if written.is_empty() {
            return;
        }
        // Durability first, then visibility at ref_count 0 so the existing GC
        // sweep can reclaim the bytes — a backend file with no PG row would be
        // invisible to it. ON CONFLICT DO NOTHING keeps a concurrent
        // uploader's row (and its references) intact.
        let hashes: Vec<String> = written.iter().map(|(h, _)| h.clone()).collect();
        let sizes: Vec<i64> = written.iter().map(|(_, s)| *s).collect();
        if let Err(e) = backend.sync_blobs(&hashes).await {
            tracing::warn!(
                "Ingest rollback: sync of {} chunks failed: {e}",
                hashes.len()
            );
        }
        if let Err(e) = sqlx::query(
            "INSERT INTO storage.blobs (hash, size, ref_count)
             SELECT h, s, 0 FROM UNNEST($1::text[], $2::bigint[]) AS t(h, s)
             ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&hashes)
        .bind(&sizes)
        .execute(pool.as_ref())
        .await
        {
            tracing::warn!(
                "Ingest rollback: failed to register {} orphan chunks for GC: {e}",
                hashes.len()
            );
        }
    }
}

impl Drop for IngestGuard {
    fn drop(&mut self) {
        if !self.armed || (self.pinned.is_empty() && self.written.is_empty()) {
            return;
        }
        let pinned = std::mem::take(&mut self.pinned);
        let written = std::mem::take(&mut self.written);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let pool = self.pool.clone();
                let backend = self.backend.clone();
                handle.spawn(async move {
                    Self::run_rollback(pool, backend, pinned, written).await;
                });
            }
            Err(_) => tracing::warn!(
                "Ingest guard dropped outside a runtime: {} pins / {} written chunks \
                 stay leaked until the next GC sweep",
                pinned.len(),
                written.len()
            ),
        }
    }
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
    /// used by request-path operations (store_from_stream, etc.).
    pool: Arc<PgPool>,
    /// Isolated maintenance pool for long-running operations
    /// (verify_integrity, garbage_collect) that must never starve the primary.
    maintenance_pool: Arc<PgPool>,
    /// Single lifecycle dispatcher — fired on blob created / deleted.
    blob_lifecycle: Option<Arc<BlobLifecycleService>>,
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
            blob_lifecycle: None,
        }
    }

    /// Registers the blob lifecycle dispatcher (thumbnail cleanup, …).
    pub fn with_blob_lifecycle(mut self, lifecycle: Arc<BlobLifecycleService>) -> Self {
        self.blob_lifecycle = Some(lifecycle);
        self
    }

    fn fire_blob_creation_hooks(&self, hash: &str, content_type: Option<&str>) {
        if let Some(lc) = &self.blob_lifecycle {
            lc.on_blob_created(hash, content_type);
        }
    }

    fn fire_blob_hooks(&self, hash: &str) {
        if let Some(lc) = &self.blob_lifecycle {
            lc.on_blob_deleted(hash);
        }
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
            blob_lifecycle: None,
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

    // ── Hash helpers ─────────────────────────────────────────────

    /// Calculate BLAKE3 hash of a file (~5× faster than SHA-256).
    ///
    /// Uses memory-mapped I/O with rayon parallelism.  Used by
    /// `verify_integrity` to re-hash local blob files.
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

    // ── Core store operations (streaming CDC) ───────────────────

    /// Maximum concurrent chunk uploads to the blob backend.
    const CHUNK_UPLOAD_CONCURRENCY: usize = 8;
    /// Flush the pending distinct-chunk batch after this many chunks…
    const FLUSH_MAX_CHUNKS: usize = 32;
    /// …or after this many buffered bytes, whichever comes first. Together
    /// with the ≤ 1 MiB chunk in flight this bounds peak RAM per upload to
    /// ~9 MiB regardless of file size.
    const FLUSH_MAX_BYTES: usize = 8 * 1024 * 1024;

    /// Store content with CDC deduplication, straight from a byte stream —
    /// the single write path for every upload surface (REST multipart,
    /// WebDAV PUT, NextCloud PUT, chunked-upload assembly, WOPI PutFile).
    ///
    /// One pass over the incoming bytes: FastCDC boundary detection,
    /// per-chunk BLAKE3, the whole-file BLAKE3, dedup lookups and blob
    /// writes all happen while the stream is still arriving. There is no
    /// spool file and no re-read — each uploaded byte touches the disk at
    /// most once, and not at all when the store already has its chunk.
    ///
    /// Identical-content races (two clients uploading the same file
    /// concurrently) are resolved at the manifest INSERT via ON CONFLICT:
    /// the loser releases its chunk references and returns `ExistingBlob`.
    pub async fn store_from_stream<S>(
        &self,
        source: S,
        content_type: Option<String>,
    ) -> Result<DedupResultDto, DomainError>
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Send,
    {
        let outcome = self.ingest_chunks_from_stream(source).await?;
        tracing::debug!(
            "CDC stream ingested: {} ({} bytes, {} chunks, {} written)",
            &outcome.file_hash[..12],
            outcome.total_size,
            outcome.chunk_hashes.len(),
            outcome.newly_written,
        );
        let distinct = outcome.distinct_hashes();
        self.attach_manifest(
            &outcome.file_hash,
            &outcome.chunk_hashes,
            &outcome.chunk_sizes,
            outcome.total_size,
            content_type,
            &distinct,
        )
        .await
    }

    /// Attach a manifest to chunk references the caller already holds (one
    /// per distinct chunk hash) — the shared accounting tail of both
    /// [`store_from_stream`] and the delta-upload commit.
    ///
    /// On a lost insert race or an already-existing manifest, the existing
    /// manifest's ref_count is bumped FIRST and only then are the held chunk
    /// references released (`distinct_held`); the reverse order could leave
    /// the caller's file row without any manifest reference behind it.
    pub async fn attach_manifest(
        &self,
        file_hash: &str,
        chunk_hashes: &[String],
        chunk_sizes: &[u64],
        total_size: u64,
        content_type: Option<String>,
        distinct_held: &[String],
    ) -> Result<DedupResultDto, DomainError> {
        // A bounded retry covers the rare interleaving where the manifest
        // that beat our INSERT is deleted again before our ref bump lands.
        for _ in 0..3 {
            let inserted = sqlx::query(
                "INSERT INTO storage.chunk_manifests
                     (file_hash, chunk_hashes, chunk_sizes, total_size, chunk_count, content_type, ref_count)
                 VALUES ($1, $2, $3, $4, $5, $6, 1)
                 ON CONFLICT (file_hash) DO NOTHING",
            )
            .bind(file_hash)
            .bind(chunk_hashes)
            .bind(chunk_sizes.iter().map(|s| *s as i64).collect::<Vec<_>>())
            .bind(total_size as i64)
            .bind(chunk_hashes.len() as i32)
            .bind(&content_type)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error("Dedup", format!("Failed to insert manifest: {}", e))
            })?
            .rows_affected();

            if inserted > 0 {
                tracing::info!(
                    "NEW BLOB (CDC): {} ({} bytes, {} chunks)",
                    &file_hash[..12],
                    total_size,
                    chunk_hashes.len(),
                );
                self.fire_blob_creation_hooks(file_hash, content_type.as_deref());
                return Ok(DedupResultDto::NewBlob {
                    hash: file_hash.to_string(),
                    size: total_size,
                });
            }

            // The manifest already exists — either this exact content was
            // stored before or an identical concurrent upload just won the
            // race. Bump ITS ref_count, then hand back the held references.
            if let Some(existing_size) = self.bump_manifest_if_exists(file_hash).await? {
                self.release_chunk_refs(self.pool.as_ref(), distinct_held)
                    .await;
                tracing::info!(
                    "DEDUP HIT (manifest): {} ({} bytes saved)",
                    &file_hash[..12],
                    existing_size,
                );
                return Ok(DedupResultDto::ExistingBlob {
                    hash: file_hash.to_string(),
                    size: existing_size as u64,
                    saved_bytes: existing_size as u64,
                });
            }
        }

        self.release_chunk_refs(self.pool.as_ref(), distinct_held)
            .await;
        Err(DomainError::internal_error(
            "Dedup",
            format!("Manifest insert/bump kept racing for {file_hash}"),
        ))
    }

    // ── Delta-upload primitives ──────────────────────────────────
    //
    // The delta protocol ("upload only what changed") lets a client claim
    // chunks by hash instead of sending their bytes. Two invariants keep
    // that from becoming a content oracle or a poisoning vector:
    //
    // 1. **Ownership**: without bytes, a caller may only claim chunks that
    //    are already reachable through their own (non-trashed) files, or
    //    unreferenced orphans (ref_count = 0 — i.e. "I just uploaded it").
    //    Everything else must be uploaded; the store dedups it on write.
    // 2. **Verification**: a declared file_hash is never trusted — the
    //    commit re-reads the proposed chunk sequence server-side and
    //    recomputes BLAKE3 before any manifest row exists. A forged hash
    //    would otherwise poison future whole-file dedup hits for OTHER
    //    users uploading the genuine content.
    //
    // The download direction reuses invariant 1: a chunk's bytes are only
    // served to callers whose own files already reference it.

    /// The ordered chunk list composing `file_hash`, for the delta-download
    /// manifest: `(chunks[(hash, size)], total_size)`.
    ///
    /// Legacy whole-file blobs (pre-CDC, not yet re-chunked) are presented
    /// as a single-chunk manifest of themselves — the chunk download path
    /// can serve them directly, so sync clients need no special case.
    pub async fn manifest_chunk_list(
        &self,
        file_hash: &str,
    ) -> Result<Option<(Vec<(String, u64)>, u64)>, DomainError> {
        let manifest = sqlx::query_as::<_, (Vec<String>, Vec<i64>, i64)>(
            "SELECT chunk_hashes, chunk_sizes, total_size
               FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(file_hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("Manifest lookup: {e}")))?;

        if let Some((hashes, sizes, total)) = manifest {
            let chunks = hashes
                .into_iter()
                .zip(sizes.into_iter().map(|s| s as u64))
                .collect();
            return Ok(Some((chunks, total as u64)));
        }

        // Legacy fallback: the blob is its own single chunk.
        let legacy = sqlx::query_scalar::<_, i64>("SELECT size FROM storage.blobs WHERE hash = $1")
            .bind(file_hash)
            .fetch_optional(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error("Dedup", format!("Legacy blob lookup: {e}"))
            })?;
        Ok(legacy.map(|size| (vec![(file_hash.to_string(), size as u64)], size as u64)))
    }

    /// Sizes of the given chunk hashes from the dedup index, keyed by hash.
    /// Hashes without a row are simply absent from the result.
    pub async fn chunk_sizes(
        &self,
        hashes: &[String],
    ) -> Result<std::collections::HashMap<String, u64>, DomainError> {
        if hashes.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        sqlx::query_as::<_, (String, i64)>(
            "SELECT hash, size FROM storage.blobs WHERE hash = ANY($1)",
        )
        .bind(hashes)
        .fetch_all(self.pool.as_ref())
        .await
        .map(|rows| rows.into_iter().map(|(h, s)| (h, s as u64)).collect())
        .map_err(|e| DomainError::internal_error("Dedup", format!("chunk_sizes query: {e}")))
    }

    /// Stream one chunk's raw bytes from the backend. The caller is
    /// responsible for entitlement (see [`claimable_chunks`]).
    pub async fn chunk_stream(
        &self,
        hash: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>
    {
        self.backend.get_blob_stream(hash).await
    }

    /// Of `hashes` (distinct), the subset `caller_id` may claim without
    /// uploading bytes: chunks referenced by manifests of the caller's
    /// non-trashed files, or directly referenced as (legacy) whole-file
    /// blobs. Backed by the GIN index on `chunk_manifests.chunk_hashes`.
    pub async fn claimable_chunks(
        &self,
        caller_id: uuid::Uuid,
        hashes: &[String],
    ) -> Result<HashSet<String>, DomainError> {
        if hashes.is_empty() {
            return Ok(HashSet::new());
        }
        sqlx::query_scalar::<_, String>(
            "SELECT c.h FROM UNNEST($1::text[]) AS c(h)
              WHERE EXISTS (
                        SELECT 1
                          FROM storage.files f
                          JOIN storage.chunk_manifests m ON m.file_hash = f.blob_hash
                         WHERE f.user_id = $2 AND NOT f.is_trashed
                           AND m.chunk_hashes @> ARRAY[c.h]
                    )
                 OR EXISTS (
                        SELECT 1 FROM storage.files f2
                         WHERE f2.user_id = $2 AND NOT f2.is_trashed
                           AND f2.blob_hash = c.h
                    )",
        )
        .bind(hashes)
        .bind(caller_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map(|rows| rows.into_iter().collect())
        .map_err(|e| DomainError::internal_error("Dedup", format!("claimable_chunks query: {e}")))
    }

    /// Pin one reference on each of `hashes` (distinct) that the caller is
    /// entitled to claim — owned chunks (see [`claimable_chunks`]) or
    /// unreferenced orphans (`ref_count = 0`, the just-uploaded state).
    /// One statement: entitlement check and bump are atomic per row, so a
    /// concurrent last-reference delete can never be resurrected and a
    /// non-entitled hash is simply not returned.
    ///
    /// Returns the set actually pinned; the caller compares against its
    /// input and reports the difference as `still_missing`.
    pub async fn pin_claimable_chunks(
        &self,
        caller_id: uuid::Uuid,
        hashes: &[String],
    ) -> Result<HashSet<String>, DomainError> {
        if hashes.is_empty() {
            return Ok(HashSet::new());
        }
        sqlx::query_scalar::<_, String>(
            "UPDATE storage.blobs b
                SET ref_count = ref_count + 1
              WHERE b.hash = ANY($1)
                AND ( b.ref_count = 0
                      OR EXISTS (
                             SELECT 1
                               FROM storage.files f
                               JOIN storage.chunk_manifests m ON m.file_hash = f.blob_hash
                              WHERE f.user_id = $2 AND NOT f.is_trashed
                                AND m.chunk_hashes @> ARRAY[b.hash::text]
                         )
                      OR EXISTS (
                             SELECT 1 FROM storage.files f2
                              WHERE f2.user_id = $2 AND NOT f2.is_trashed
                                AND f2.blob_hash = b.hash
                         ) )
              RETURNING b.hash",
        )
        .bind(hashes)
        .bind(caller_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map(|rows| rows.into_iter().collect())
        .map_err(|e| {
            DomainError::internal_error("Dedup", format!("pin_claimable_chunks query: {e}"))
        })
    }

    /// Release one reference per distinct hash — the public counterpart of
    /// [`pin_claimable_chunks`] for aborted commits. Best-effort.
    pub async fn release_pinned_chunks(&self, hashes: &[String]) {
        self.release_chunk_refs(self.pool.as_ref(), hashes).await;
    }

    /// Store client-provided loose chunks (delta upload, step 2).
    ///
    /// Each element of `frames` is one chunk's raw bytes (the wire framing
    /// is the interface layer's concern). The hash is ALWAYS computed
    /// server-side — a declared hash is never trusted for content
    /// addressing. Chunks are written unsynced, made durable with one
    /// batched sweep, then registered at `ref_count = 0`: unreferenced
    /// orphans that either get pinned by a following commit or swept by
    /// the periodic GC if the client never returns. `ON CONFLICT DO
    /// NOTHING` keeps existing rows' reference counts untouched.
    ///
    /// Returns `(hash, size)` per frame, in input order.
    pub async fn store_loose_chunks<S>(&self, frames: S) -> Result<Vec<(String, u64)>, DomainError>
    where
        S: Stream<Item = Result<Bytes, DomainError>> + Send,
    {
        futures::pin_mut!(frames);

        let mut received: Vec<(String, u64)> = Vec::new();
        let mut new_rows: Vec<(String, i64)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        while let Some(frame) = frames.next().await {
            let data = frame?;
            if data.len() > CDC_MAX_CHUNK {
                return Err(DomainError::validation_error(format!(
                    "Chunk frame of {} bytes exceeds the {CDC_MAX_CHUNK}-byte maximum",
                    data.len()
                )));
            }
            let hash = blake3::hash(&data).to_hex().to_string();
            received.push((hash.clone(), data.len() as u64));
            if seen.insert(hash.clone()) {
                let len = data.len() as i64;
                self.backend
                    .put_blob_from_bytes_unsynced(&hash, data)
                    .await?;
                new_rows.push((hash, len));
            }
        }

        if !new_rows.is_empty() {
            // Durability before visibility — same invariant as the ingest
            // engine: no PG row may ever point at unsynced bytes.
            let hashes: Vec<String> = new_rows.iter().map(|(h, _)| h.clone()).collect();
            let sizes: Vec<i64> = new_rows.iter().map(|(_, s)| *s).collect();
            self.backend.sync_blobs(&hashes).await?;
            sqlx::query(
                "INSERT INTO storage.blobs (hash, size, ref_count)
                 SELECT h, s, 0 FROM UNNEST($1::text[], $2::bigint[]) AS t(h, s)
                 ON CONFLICT (hash) DO NOTHING",
            )
            .bind(&hashes)
            .bind(&sizes)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error("Dedup", format!("Failed to register chunks: {e}"))
            })?;
        }

        Ok(received)
    }

    /// Verification read for the delta commit: stream the proposed chunk
    /// sequence from the backend, recompute the whole-file BLAKE3 and
    /// capture the first bytes for MIME sniffing. The caller must hold a
    /// pin on every chunk (so a concurrent GC cannot pull bytes out from
    /// under the read). Also validates each chunk's actual size against
    /// the declared one — the manifest's Range arithmetic depends on it.
    pub async fn hash_chunk_sequence(
        &self,
        chunks: &[(String, u64)],
        sniff_len: usize,
    ) -> Result<(String, Vec<u8>), DomainError> {
        let mut hasher = blake3::Hasher::new();
        let mut head: Vec<u8> = Vec::with_capacity(sniff_len.min(16 * 1024));

        for (hash, declared_size) in chunks {
            let mut stream = self.backend.get_blob_stream(hash).await?;
            let mut actual: u64 = 0;
            while let Some(part) = stream.next().await {
                let part = part.map_err(|e| {
                    DomainError::internal_error(
                        "Dedup",
                        format!("Verification read of chunk {hash}: {e}"),
                    )
                })?;
                actual += part.len() as u64;
                hasher.update(&part);
                if head.len() < sniff_len {
                    let take = (sniff_len - head.len()).min(part.len());
                    head.extend_from_slice(&part[..take]);
                }
            }
            if actual != *declared_size {
                return Err(DomainError::validation_error(format!(
                    "Chunk {hash} is {actual} bytes, manifest declares {declared_size}"
                )));
            }
        }

        Ok((hasher.finalize().to_hex().to_string(), head))
    }

    /// Bump a manifest's ref_count if it exists; returns its total_size.
    /// Single statement — no window between the existence check and the bump.
    async fn bump_manifest_if_exists(&self, file_hash: &str) -> Result<Option<i64>, DomainError> {
        sqlx::query_scalar::<_, i64>(
            "UPDATE storage.chunk_manifests SET ref_count = ref_count + 1
              WHERE file_hash = $1
              RETURNING total_size",
        )
        .bind(file_hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("Dedup", format!("Failed to bump manifest ref_count: {e}"))
        })
    }

    /// Stream → chunk store, WITHOUT creating a manifest.
    ///
    /// Splits the stream with FastCDC while computing per-chunk and
    /// whole-stream BLAKE3 hashes, then settles each batch of distinct
    /// chunks against PG:
    ///
    /// 1. ONE `UPDATE … RETURNING` per batch pins every already-known chunk
    ///    (`ref_count + 1` — protecting it from a concurrent last-reference
    ///    delete for the rest of the upload) and atomically classifies the
    ///    remaining hashes as new. No check-then-bump TOCTOU window.
    /// 2. New chunks are written to the backend unsynced with bounded
    ///    concurrency; chunks the store already has are dropped from RAM
    ///    without any disk I/O.
    /// 3. At end of stream, ONE `sync_blobs` sweep makes the new chunks
    ///    durable, then ONE batched INSERT registers them (`ON CONFLICT`
    ///    bumps instead — a concurrent identical upload may have registered
    ///    the same brand-new chunk first). Durability before visibility.
    ///
    /// `ref_count` is taken once per *distinct* chunk — symmetric with
    /// `remove_manifest_reference`, which decrements via
    /// `WHERE hash = ANY(chunk_hashes)` (each row once). A repeated chunk
    /// (zero-filled regions, concatenated archives) must not over-count or
    /// the blob leaks forever.
    ///
    /// If the returned references are not attached to a manifest, the caller
    /// must hand them back via `release_chunk_refs`. If this future is
    /// dropped mid-stream (client disconnect), the internal guard rolls the
    /// session back in a spawned task.
    async fn ingest_chunks_from_stream<S>(
        &self,
        source: S,
    ) -> Result<ChunkIngestOutcome, DomainError>
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Send,
    {
        let mut guard = IngestGuard::new(self.pool.clone(), self.backend.clone());

        let reader = StreamReader::new(Box::pin(source));
        let mut chunker = fastcdc::v2020::AsyncStreamCDC::new(
            reader,
            CDC_MIN_CHUNK,
            CDC_AVG_CHUNK,
            CDC_MAX_CHUNK,
        );
        let chunk_stream = chunker.as_stream();
        futures::pin_mut!(chunk_stream);

        let mut file_hasher = blake3::Hasher::new();
        let mut total_size: u64 = 0;
        let mut chunk_hashes: Vec<String> = Vec::new();
        let mut chunk_sizes: Vec<u64> = Vec::new();
        let mut session_seen: HashSet<String> = HashSet::new();
        let mut pending: Vec<(String, Bytes)> = Vec::new();
        let mut pending_bytes: usize = 0;

        while let Some(item) = chunk_stream.next().await {
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(e) => {
                    guard.rollback().await;
                    return Err(DomainError::internal_error(
                        "Dedup",
                        format!("Upload stream failed: {e}"),
                    ));
                }
            };

            let data = chunk.data;
            total_size += data.len() as u64;
            // Per-chunk hashing is ≤ 1 MiB of BLAKE3 (< 1 ms) — cheaper than
            // a spawn_blocking round-trip per chunk.
            file_hasher.update(&data);
            let hash = blake3::hash(&data).to_hex().to_string();
            chunk_sizes.push(data.len() as u64);
            chunk_hashes.push(hash.clone());

            if session_seen.insert(hash.clone()) {
                pending_bytes += data.len();
                pending.push((hash, Bytes::from(data)));
                if pending.len() >= Self::FLUSH_MAX_CHUNKS || pending_bytes >= Self::FLUSH_MAX_BYTES
                {
                    if let Err(e) = self.flush_pending(&mut guard, &mut pending).await {
                        guard.rollback().await;
                        return Err(e);
                    }
                    pending_bytes = 0;
                }
            }
        }

        if let Err(e) = self.flush_pending(&mut guard, &mut pending).await {
            guard.rollback().await;
            return Err(e);
        }

        // ── Durability before visibility for the new chunks ──────
        // One batched fsync sweep (no-op for remote backends, durable on
        // PUT), then one batched INSERT. A crash before the INSERT leaves
        // only unreferenced files; never a row pointing at unsynced bytes.
        if !guard.written.is_empty() {
            let new_hashes: Vec<String> = guard.written.iter().map(|(h, _)| h.clone()).collect();
            let new_sizes: Vec<i64> = guard.written.iter().map(|(_, s)| *s).collect();

            if let Err(e) = self.backend.sync_blobs(&new_hashes).await {
                guard.rollback().await;
                return Err(e);
            }

            let registered = sqlx::query(
                "INSERT INTO storage.blobs (hash, size, ref_count)
                 SELECT h, s, 1 FROM UNNEST($1::text[], $2::bigint[]) AS t(h, s)
                 ON CONFLICT (hash) DO UPDATE
                   SET ref_count = storage.blobs.ref_count + 1",
            )
            .bind(&new_hashes)
            .bind(&new_sizes)
            .execute(self.pool.as_ref())
            .await;

            if let Err(e) = registered {
                guard.rollback().await;
                return Err(DomainError::internal_error(
                    "Dedup",
                    format!("Failed to register chunks: {e}"),
                ));
            }
        }

        let newly_written = guard.written.len();
        guard.disarm();

        Ok(ChunkIngestOutcome {
            file_hash: file_hasher.finalize().to_hex().to_string(),
            total_size,
            chunk_hashes,
            chunk_sizes,
            newly_written,
        })
    }

    /// Settle one batch of distinct in-RAM chunks against PG + the backend.
    ///
    /// Successfully pinned hashes and written chunks are recorded on the
    /// guard as they happen, so a failure mid-batch leaves nothing
    /// untracked for rollback.
    async fn flush_pending(
        &self,
        guard: &mut IngestGuard,
        pending: &mut Vec<(String, Bytes)>,
    ) -> Result<(), DomainError> {
        if pending.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(pending);
        let hashes: Vec<String> = batch.iter().map(|(h, _)| h.clone()).collect();

        // Pin-or-classify in one statement: rows that exist take this
        // session's reference NOW; hashes not returned don't exist and are
        // ours to write.
        let pinned: HashSet<String> = sqlx::query_scalar::<_, String>(
            "UPDATE storage.blobs SET ref_count = ref_count + 1
              WHERE hash = ANY($1)
              RETURNING hash",
        )
        .bind(&hashes)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("Dedup", format!("Failed to pin existing chunks: {e}"))
        })?
        .into_iter()
        .collect();

        let mut to_write: Vec<(String, Bytes)> = Vec::with_capacity(batch.len());
        for (hash, data) in batch {
            if pinned.contains(&hash) {
                guard.pinned.push(hash);
            } else {
                to_write.push((hash, data));
            }
        }
        if to_write.is_empty() {
            return Ok(());
        }

        // Unsynced writes — durability comes from the single end-of-stream
        // sweep, before any PG row references these chunks.
        let backend = self.backend.clone();
        let results: Vec<Result<(String, i64), DomainError>> = stream::iter(to_write)
            .map(|(hash, data)| {
                let backend = backend.clone();
                async move {
                    let len = data.len() as i64;
                    backend.put_blob_from_bytes_unsynced(&hash, data).await?;
                    Ok((hash, len))
                }
            })
            .buffer_unordered(Self::CHUNK_UPLOAD_CONCURRENCY)
            .collect()
            .await;

        let mut first_err: Option<DomainError> = None;
        for result in results {
            match result {
                Ok(row) => guard.written.push(row),
                Err(e) => first_err = first_err.or(Some(e)),
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
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
            "SELECT EXISTS(SELECT 1 FROM storage.files WHERE blob_hash = $1 AND user_id = $2::uuid AND NOT is_trashed)",
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

            // Bug 4 fix: notify hooks — e.g. thumbnail cleanup keyed by file_hash
            self.fire_blob_hooks(file_hash);

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

        // Lock the row exclusively — prevents a concurrent ingest from
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
            // so no concurrent ingest can resurrect a reference.
            if let Err(e) = self.backend.delete_blob(hash).await {
                tracing::warn!("Failed to delete blob file {}: {}", hash, e);
            }

            // Bug 3 fix: notify hooks — e.g. thumbnail cleanup keyed by hash
            self.fire_blob_hooks(hash);

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

    /// Targeted cleanup for a single blob after the PG trigger has already
    /// decremented its ref_count.  Deletes the blob row, disk file, and
    /// blob-keyed thumbnails if ref_count has reached 0.
    ///
    /// Handles both the legacy whole-file blob path (storage.blobs) and the
    /// CDC manifest path (storage.chunk_manifests).  Best-effort: logs
    /// warnings on failure rather than returning an error.
    pub async fn cleanup_if_orphaned(&self, hash: &str) {
        let short = &hash[..hash.len().min(12)];

        // ── CDC manifest path (must run FIRST) ───────────────────
        // For single-chunk CDC files file_hash == chunk_hash, so the PG
        // trigger on storage.files already decremented storage.blobs.ref_count
        // when this function is called.  try_dedup_hit increments
        // chunk_manifests.ref_count but NOT storage.blobs.ref_count, so
        // blobs.ref_count can reach 0 while the manifest still has ref_count > 1
        // (other files sharing the same blob).  Checking the manifest first
        // prevents premature blob + manifest deletion.
        let manifest = sqlx::query_as::<_, (i32, Vec<String>)>(
            "SELECT ref_count, chunk_hashes \
               FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .unwrap_or(None);

        if let Some((ref_count, chunk_hashes)) = manifest {
            if ref_count <= 1 {
                // Last reference — remove manifest and all its chunks.
                if let Err(e) = self
                    .remove_manifest_reference(hash, ref_count, &chunk_hashes)
                    .await
                {
                    tracing::warn!("cleanup_if_orphaned: manifest cleanup failed for {short}: {e}");
                }
            } else {
                // Other files still share this blob: just decrement the manifest
                // counter and undo the PG trigger's premature chunk ref_count
                // decrement (blobs.ref_count is chunk-level; the manifest is the
                // authoritative file-level counter).
                sqlx::query(
                    "UPDATE storage.chunk_manifests \
                        SET ref_count = ref_count - 1 WHERE file_hash = $1",
                )
                .bind(hash)
                .execute(self.pool.as_ref())
                .await
                .ok();
                // Undo the PG trigger's decrement of storage.blobs.ref_count.
                // The trigger fired with blob_hash = file_hash, so only the row
                // WHERE hash = file_hash is affected.  For single-chunk files
                // file_hash == chunk_hash and that row exists; for multi-chunk
                // files file_hash is not in storage.blobs, making this a no-op.
                sqlx::query("UPDATE storage.blobs SET ref_count = ref_count + 1 WHERE hash = $1")
                    .bind(hash)
                    .execute(self.pool.as_ref())
                    .await
                    .ok();
                tracing::debug!(
                    "cleanup_if_orphaned: manifest {short} ref_count {ref_count}→{}",
                    ref_count - 1
                );
            }
            return;
        }

        // ── Legacy blob path (no manifest) ───────────────────────
        let deleted_blob = sqlx::query_scalar::<_, String>(
            "DELETE FROM storage.blobs WHERE hash = $1 AND ref_count <= 0 RETURNING hash",
        )
        .bind(hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .unwrap_or(None);

        if deleted_blob.is_some() {
            if let Err(e) = self.backend.delete_blob(hash).await {
                tracing::warn!("cleanup_if_orphaned: disk delete failed for {short}: {e}");
            }
            self.fire_blob_hooks(hash);
            tracing::info!("cleanup_if_orphaned: removed orphaned blob {short}");
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
            // CDC file: stream chunks in order. Read-ahead depth is the
            // backend's hint (1 for local disk, higher for remote object
            // stores where overlapping fetches hide per-chunk latency).
            let prefetch = self.backend.read_prefetch().max(1);
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
                .buffered(prefetch)
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

            // Stream selected chunks with ranges. Read-ahead depth from the
            // backend hint (local=1; remote overlaps fetches — see read_blob_stream).
            let prefetch = self.backend.read_prefetch().max(1);
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
                .buffered(prefetch)
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
        // A manifest is collectible when:
        //   • ref_count has been decremented to 0 by cleanup_if_orphaned
        //     on the single-file-delete service path, OR
        //   • no `storage.files.blob_hash` references its file_hash
        //     (covers bulk-delete paths: user cascade, empty_trash —
        //     where the PG trigger only touches storage.blobs and the
        //     per-file cleanup_if_orphaned call is skipped).
        loop {
            let batch: Vec<(String, Vec<String>, i64)> = sqlx::query_as(
                "DELETE FROM storage.chunk_manifests
                  WHERE ctid = ANY(
                      SELECT ctid FROM storage.chunk_manifests m
                       WHERE m.ref_count <= 0
                          OR NOT EXISTS (
                              SELECT 1 FROM storage.files f
                               WHERE f.blob_hash = m.file_hash
                          )
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
                // Decrement chunk ref_counts. GREATEST(.., 0) guards against the
                // single-chunk file case where the PG file-delete trigger already
                // decremented blobs.ref_count (because file_hash == chunk_hash);
                // without the clamp this would underflow the CHECK constraint.
                sqlx::query(
                    "UPDATE storage.blobs
                        SET ref_count = GREATEST(ref_count - 1, 0)
                      WHERE hash = ANY($1)",
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
                self.fire_blob_hooks(hash);
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

    // ── Legacy whole-file blob re-chunk migration ────────────────
    //
    // Files uploaded before CDC chunking landed (migration
    // 20260414000000_chunk_manifests) are stored as ONE whole-file blob with
    // no manifest. Every legacy fallback in this service exists to serve
    // them — and with encryption enabled, a Range read of one decrypts the
    // ENTIRE blob (AES-GCM is all-or-nothing per blob).
    //
    // This migration converts each legacy blob into a regular CDC file:
    // after it, the converted file is indistinguishable from a native CDC
    // upload, every read takes the chunked path, and the legacy fallbacks
    // go permanently cold (they remain as the safety net while a deployment
    // is mid-migration; they can be deleted from the codebase once fleets
    // report `legacy re-chunk: nothing to do`).
    //
    // Per-hash algorithm:
    //   1. Stream the blob through the normal read path (this decrypts it
    //      when encryption is on) straight into the chunk-ingest engine —
    //      no spool file — verifying BLAKE3 == hash before keeping the
    //      chunks (each distinct chunk bumped once — the manifest's
    //      reference).
    //   2. One short accounting TX with the blob row locked:
    //      manifest INSERT with ref_count = N (current file rows referencing
    //      the hash), blob ref_count -= N (those references now live on the
    //      manifest), DELETE the blob row only if it hits exactly 0.
    //   3. Physically delete the whole-file blob only when its row was
    //      removed. Single-chunk files (chunk hash == file hash) keep the
    //      physical blob — it IS the chunk; only the bookkeeping moves.
    //
    // Concurrency: the row lock serializes against the file-delete trigger
    // and the legacy dedup-hit path. A racing identical upload can land one
    // legacy reference after our commit; the blob row then survives (> 0)
    // and that file stays readable through the legacy fallback — a bounded
    // space leak, never data loss. A crash between step 1 and 2 leaks one
    // +1 on that file's chunk refs (re-run re-bumps); also a bounded leak,
    // never data loss.

    /// Count legacy whole-file blobs still referenced by at least one file
    /// row (the migration's work queue). Runs on the maintenance pool.
    pub async fn count_legacy_blobs(&self) -> Result<i64, DomainError> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.blobs b
              WHERE NOT EXISTS (SELECT 1 FROM storage.chunk_manifests m
                                 WHERE m.file_hash = b.hash)
                AND EXISTS (SELECT 1 FROM storage.files f
                             WHERE f.blob_hash = b.hash)",
        )
        .fetch_one(self.maintenance_pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("Count legacy blobs: {e}")))
    }

    /// Spawn the legacy re-chunk migration as a background task.
    ///
    /// Zero-cost when no legacy blobs exist (one COUNT query, debug log).
    /// Called from the composition root after `initialize()`.
    pub fn spawn_legacy_rechunk(self: &Arc<Self>) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            match svc.count_legacy_blobs().await {
                Ok(0) => {
                    tracing::debug!("Legacy re-chunk: no legacy whole-file blobs — nothing to do");
                }
                Ok(n) => {
                    tracing::info!(
                        "Legacy re-chunk: {n} pre-CDC whole-file blob(s) referenced by files — \
                         starting background migration (maintenance pool)"
                    );
                    match svc.rechunk_legacy_blobs().await {
                        Ok(report) => tracing::info!(
                            migrated = report.migrated,
                            failed = report.failed,
                            freed_bytes = report.freed_bytes,
                            "Legacy re-chunk complete: {} blob(s) converted to CDC manifests, \
                             {} failed (left untouched), {} bytes of whole-file blobs freed",
                            report.migrated,
                            report.failed,
                            report.freed_bytes,
                        ),
                        Err(e) => tracing::error!("Legacy re-chunk aborted: {e}"),
                    }
                }
                Err(e) => tracing::error!("Legacy re-chunk: startup count failed: {e}"),
            }
        });
    }

    /// Convert every legacy whole-file blob into CDC chunks + manifest.
    ///
    /// Incremental and resumable: a manifest row is the per-hash "done"
    /// marker, so re-running after a crash continues where it left off.
    /// Per-hash failures (e.g. a corrupt blob that no longer matches its
    /// hash) are logged, counted, and skipped — they never block the sweep.
    pub async fn rechunk_legacy_blobs(&self) -> Result<LegacyRechunkReport, DomainError> {
        const BATCH_SIZE: i64 = 64;
        /// Hard cap on per-hash failures before aborting the sweep — if
        /// this many blobs are corrupt something is systemically wrong and
        /// an operator should look before we touch anything else.
        const MAX_FAILURES: usize = 1_000;

        let mut report = LegacyRechunkReport::default();
        // Failed hashes are excluded from the candidate query so a corrupt
        // blob cannot make the sweep loop forever.
        let mut failed_hashes: Vec<String> = Vec::new();

        loop {
            let batch: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT b.hash, b.content_type FROM storage.blobs b
                  WHERE NOT EXISTS (SELECT 1 FROM storage.chunk_manifests m
                                     WHERE m.file_hash = b.hash)
                    AND EXISTS (SELECT 1 FROM storage.files f
                                 WHERE f.blob_hash = b.hash)
                    AND NOT (b.hash = ANY($2))
                  ORDER BY b.hash
                  LIMIT $1",
            )
            .bind(BATCH_SIZE)
            .bind(&failed_hashes)
            .fetch_all(self.maintenance_pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error("Dedup", format!("Legacy candidate query: {e}"))
            })?;

            if batch.is_empty() {
                break;
            }

            for (hash, content_type) in batch {
                match self.rechunk_one_legacy_blob(&hash, content_type).await {
                    Ok(freed) => {
                        report.migrated += 1;
                        report.freed_bytes += freed;
                        if report.migrated % 50 == 0 {
                            tracing::info!(
                                "Legacy re-chunk progress: {} migrated, {} failed",
                                report.migrated,
                                report.failed
                            );
                        }
                    }
                    Err(e) => {
                        report.failed += 1;
                        tracing::error!(
                            "Legacy re-chunk: blob {} failed (left untouched): {e}",
                            &hash[..hash.len().min(12)],
                        );
                        failed_hashes.push(hash);
                        if failed_hashes.len() >= MAX_FAILURES {
                            return Err(DomainError::internal_error(
                                "Dedup",
                                format!(
                                    "Legacy re-chunk: aborting after {MAX_FAILURES} per-blob \
                                     failures — inspect blob storage integrity"
                                ),
                            ));
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
        }

        Ok(report)
    }

    /// Migrate a single legacy whole-file blob. Returns the number of
    /// physical bytes freed (0 when the blob doubles as its own chunk).
    async fn rechunk_one_legacy_blob(
        &self,
        hash: &str,
        content_type: Option<String>,
    ) -> Result<u64, DomainError> {
        // ── 1. Stream + verify (decrypts via the normal read path) ──
        // The chunk store is fed directly from the blob read stream — no
        // spool file. Sizes come from the CDC pass over the hash-verified
        // plaintext; `storage.blobs.size` is legacy metadata we don't trust
        // for the manifest's Range arithmetic.
        let (chunk_hashes, chunk_sizes) = self.ingest_legacy_blob(hash).await?;
        let total_size: u64 = chunk_sizes.iter().sum();

        // ── 2. Accounting TX: move the file references onto the manifest ──
        let mut tx =
            self.maintenance_pool.begin().await.map_err(|e| {
                DomainError::internal_error("Dedup", format!("Rechunk TX begin: {e}"))
            })?;

        // Lock the legacy blob row — serializes against the file-delete
        // trigger and the legacy dedup-hit path for this hash.
        let blob_row_exists = sqlx::query_scalar::<_, i32>(
            "SELECT ref_count FROM storage.blobs WHERE hash = $1 FOR UPDATE",
        )
        .bind(hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("Rechunk lock blob: {e}")))?
        .is_some();

        let file_refs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM storage.files WHERE blob_hash = $1")
                .bind(hash)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| {
                    DomainError::internal_error("Dedup", format!("Rechunk count refs: {e}"))
                })?;

        // ref_count = N file references; if every reference vanished while
        // we were spooling, the zero-ref manifest is swept by the existing
        // GC (which also unwinds the chunk refs taken in store_chunks).
        let inserted = sqlx::query(
            "INSERT INTO storage.chunk_manifests
                 (file_hash, chunk_hashes, chunk_sizes, total_size, chunk_count,
                  content_type, ref_count)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (file_hash) DO NOTHING",
        )
        .bind(hash)
        .bind(&chunk_hashes)
        .bind(chunk_sizes.iter().map(|s| *s as i64).collect::<Vec<_>>())
        .bind(total_size as i64)
        .bind(chunk_hashes.len() as i32)
        .bind(&content_type)
        .bind(file_refs as i32)
        .execute(&mut *tx)
        .await
        .map_err(|e| DomainError::internal_error("Dedup", format!("Rechunk manifest: {e}")))?
        .rows_affected();

        if inserted == 0 {
            // A manifest appeared concurrently — only possible if the same
            // content was re-uploaded and fully stored while we streamed.
            // Their bookkeeping is already correct; drop ours.
            tx.rollback().await.ok();
            self.release_chunk_refs(self.maintenance_pool.as_ref(), &chunk_hashes)
                .await;
            return Ok(0);
        }

        // The N file references now live on the manifest; remove them from
        // the legacy blob and drop its row only when nothing else (other
        // manifests using this blob as a chunk, racing legacy references)
        // still points at it.
        let mut blob_row_deleted = false;
        if blob_row_exists {
            sqlx::query(
                "UPDATE storage.blobs
                    SET ref_count = GREATEST(ref_count - $2, 0)
                  WHERE hash = $1",
            )
            .bind(hash)
            .bind(file_refs as i32)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                DomainError::internal_error("Dedup", format!("Rechunk deref blob: {e}"))
            })?;

            blob_row_deleted =
                sqlx::query("DELETE FROM storage.blobs WHERE hash = $1 AND ref_count = 0")
                    .bind(hash)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| {
                        DomainError::internal_error("Dedup", format!("Rechunk drop blob: {e}"))
                    })?
                    .rows_affected()
                    > 0;
        }

        tx.commit()
            .await
            .map_err(|e| DomainError::internal_error("Dedup", format!("Rechunk commit: {e}")))?;

        // ── 3. Physical cleanup (after commit) ──
        // Deleted row ⇒ the hash is not one of its own chunks (a single-chunk
        // file keeps ref_count ≥ 1 from the manifest), but guard anyway.
        let mut freed = 0;
        if blob_row_deleted && !chunk_hashes.iter().any(|c| c == hash) {
            match self.backend.delete_blob(hash).await {
                Ok(()) => freed = total_size,
                Err(e) => tracing::warn!(
                    "Legacy re-chunk: converted {} but failed to delete the \
                     old whole-file blob (GC will not retry — row is gone): {e}",
                    &hash[..hash.len().min(12)],
                ),
            }
        }

        tracing::debug!(
            "Legacy re-chunk: {} → {} chunk(s), {} file ref(s) moved to manifest{}",
            &hash[..hash.len().min(12)],
            chunk_hashes.len(),
            file_refs,
            if blob_row_deleted {
                ", whole-file blob freed"
            } else {
                ""
            },
        );

        Ok(freed)
    }

    /// Re-chunk one legacy whole-file blob straight from the backend read
    /// stream (no spool file), verifying that the streamed content still
    /// matches its recorded BLAKE3 before the chunks are kept.
    ///
    /// On mismatch the freshly taken chunk references are released — the
    /// written chunk bytes become unreferenced rows the GC sweeps — and an
    /// error is returned; the legacy blob itself stays untouched.
    async fn ingest_legacy_blob(&self, hash: &str) -> Result<(Vec<String>, Vec<u64>), DomainError> {
        let stream = self.read_blob_stream(hash).await?;
        let outcome = self.ingest_chunks_from_stream(stream).await?;
        if outcome.file_hash != hash {
            let distinct = outcome.distinct_hashes();
            self.release_chunk_refs(self.maintenance_pool.as_ref(), &distinct)
                .await;
            return Err(DomainError::internal_error(
                "Dedup",
                format!(
                    "Blob content does not match its hash (expected {hash}, got {})",
                    outcome.file_hash
                ),
            ));
        }
        Ok((outcome.chunk_hashes, outcome.chunk_sizes))
    }

    /// Best-effort compensation: drop one reference per *distinct* chunk
    /// hash (clamped at 0). Used whenever an ingest session's references end
    /// up not being attached to a manifest — dedup hit, lost insert race, or
    /// content-verification failure.
    async fn release_chunk_refs(&self, pool: &PgPool, chunk_hashes: &[String]) {
        if chunk_hashes.is_empty() {
            return;
        }
        if let Err(e) = sqlx::query(
            "UPDATE storage.blobs SET ref_count = GREATEST(ref_count - 1, 0)
              WHERE hash = ANY($1)",
        )
        .bind(chunk_hashes)
        .execute(pool)
        .await
        {
            tracing::warn!("Dedup: failed to release chunk refs: {e}");
        }
    }
}

/// Outcome of a [`DedupService::rechunk_legacy_blobs`] sweep.
#[derive(Debug, Default, Clone, Copy)]
pub struct LegacyRechunkReport {
    /// Legacy blobs successfully converted to CDC manifests.
    pub migrated: u64,
    /// Blobs that failed (corrupt / unreadable) and were left untouched.
    pub failed: u64,
    /// Physical bytes of whole-file blobs deleted after conversion.
    pub freed_bytes: u64,
}

// ─── Port implementation ─────────────────────────────────────────────────────

impl DedupPort for DedupService {
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

    /// One chunk as seen by the streaming analyser.
    struct TestChunk {
        hash: String,
        offset: usize,
        length: usize,
    }

    /// Run the exact same streaming chunker the ingest engine uses
    /// (`AsyncStreamCDC` + the production CDC parameters) over an in-memory
    /// buffer, feeding it in `frame`-sized pieces to exercise the refill
    /// logic the same way HTTP body frames do.
    ///
    /// Returns the whole-stream BLAKE3 plus per-chunk metadata.
    async fn stream_cdc(data: &[u8], frame: usize) -> (String, Vec<TestChunk>) {
        let frames: Vec<Result<Bytes, std::io::Error>> = data
            .chunks(frame.max(1))
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let reader = StreamReader::new(Box::pin(stream::iter(frames)));
        let mut chunker = fastcdc::v2020::AsyncStreamCDC::new(
            reader,
            CDC_MIN_CHUNK,
            CDC_AVG_CHUNK,
            CDC_MAX_CHUNK,
        );
        let chunk_stream = chunker.as_stream();
        futures::pin_mut!(chunk_stream);

        let mut file_hasher = blake3::Hasher::new();
        let mut chunks = Vec::new();
        while let Some(item) = chunk_stream.next().await {
            let chunk = item.expect("in-memory stream cannot fail");
            file_hasher.update(&chunk.data);
            chunks.push(TestChunk {
                hash: blake3::hash(&chunk.data).to_hex().to_string(),
                offset: chunk.offset as usize,
                length: chunk.length,
            });
        }
        (file_hasher.finalize().to_hex().to_string(), chunks)
    }

    const TEST_FRAME: usize = 64 * 1024; // typical HTTP body frame size

    // ── Stream chunking ≡ slice chunking ─────────────────────────
    //
    // The whole dedup index hinges on this invariant: the boundaries (and
    // therefore the chunk hashes) produced by the streaming chunker must be
    // identical to FastCDC over the full in-memory slice, regardless of how
    // the bytes were framed on the wire. Pre-streaming blobs were chunked
    // via mmap + slice FastCDC — their chunks must keep deduplicating
    // against newly streamed uploads.

    #[tokio::test]
    async fn test_stream_chunking_matches_slice_chunking() {
        let data: Vec<u8> = (0..4 * 1024 * 1024)
            .map(|i| ((i as u64).wrapping_mul(6364136223846793005).wrapping_add(1)) as u8)
            .collect();

        let slice_chunks: Vec<(usize, usize)> =
            fastcdc::v2020::FastCDC::new(&data, CDC_MIN_CHUNK, CDC_AVG_CHUNK, CDC_MAX_CHUNK)
                .map(|c| (c.offset, c.length))
                .collect();

        for frame in [7usize, 4096, TEST_FRAME, data.len()] {
            let (_, streamed) = stream_cdc(&data, frame).await;
            assert_eq!(
                streamed.len(),
                slice_chunks.len(),
                "chunk count must not depend on framing (frame={frame})"
            );
            for (s, (offset, length)) in streamed.iter().zip(slice_chunks.iter()) {
                assert_eq!((s.offset, s.length), (*offset, *length), "frame={frame}");
                let expected = blake3::hash(&data[*offset..*offset + *length])
                    .to_hex()
                    .to_string();
                assert_eq!(s.hash, expected, "frame={frame}");
            }
        }
    }

    // ── Determinism ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_cdc_deterministic_same_content() {
        let data = vec![42u8; 512 * 1024]; // 512 KB of 0x2A

        let (hash1, chunks1) = stream_cdc(&data, TEST_FRAME).await;
        let (hash2, chunks2) = stream_cdc(&data, 4096).await;

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

    // ── Empty stream ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_cdc_empty_stream() {
        let (hash, chunks) = stream_cdc(b"", TEST_FRAME).await;

        assert!(chunks.is_empty(), "empty stream must produce zero chunks");
        assert_eq!(hash, blake3::hash(b"").to_hex().to_string());
    }

    // ── Small file (below min chunk) → single chunk ──────────────

    #[tokio::test]
    async fn test_cdc_small_file_single_chunk() {
        let data = b"Hello, OxiCloud CDC dedup!";
        let (hash, chunks) = stream_cdc(data, TEST_FRAME).await;

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

        let (_, chunks) = stream_cdc(&data, TEST_FRAME).await;

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

        let (cdc_hash, _) = stream_cdc(&data, TEST_FRAME).await;
        let standalone_hash = DedupService::hash_file(f.path()).await.unwrap();

        assert_eq!(
            cdc_hash, standalone_hash,
            "streamed file hash must match standalone hash_file()"
        );
    }

    // ── Reassembly: chunks are contiguous and cover the file ─────

    #[tokio::test]
    async fn test_cdc_chunks_are_contiguous() {
        let data: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 199) as u8).collect();

        let (_, chunks) = stream_cdc(&data, TEST_FRAME).await;

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

        let (hash_base, chunks_base) = stream_cdc(&base, TEST_FRAME).await;
        let (hash_mod, chunks_mod) = stream_cdc(&modified, TEST_FRAME).await;

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

    // ── Large file produces expected chunk count ──────────────────

    #[tokio::test]
    async fn test_cdc_large_file_chunk_count() {
        // 8 MB should produce roughly 8MB / 256KB ≈ 32 chunks (±)
        let data: Vec<u8> = (0..8 * 1024 * 1024)
            .map(|i| ((i as u64).wrapping_mul(2862933555777941757).wrapping_add(3)) as u8)
            .collect();

        let (_, chunks) = stream_cdc(&data, TEST_FRAME).await;

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

        let (_, chunks_base) = stream_cdc(&base, TEST_FRAME).await;
        let (_, chunks_prefix) = stream_cdc(&with_prefix, TEST_FRAME).await;

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

    // ── ChunkIngestOutcome helpers ───────────────────────────────

    #[test]
    fn test_distinct_hashes_deduplicates_preserving_order() {
        let outcome = ChunkIngestOutcome {
            file_hash: String::new(),
            total_size: 0,
            chunk_hashes: vec!["a".into(), "b".into(), "a".into(), "c".into(), "b".into()],
            chunk_sizes: vec![1, 2, 1, 3, 2],
            newly_written: 0,
        };
        assert_eq!(outcome.distinct_hashes(), vec!["a", "b", "c"]);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Integration tests for the legacy re-chunk migration — require the test
// database (run via `just test-integration`, which spawns it and applies
// migrations). Gated on `--cfg integration_tests` like the other PG suites.
//
// Each test seeds its own synthetic "legacy" state (a whole-file blob row in
// `storage.blobs` + file rows pointing at it, no manifest) with unique
// `rust-test-rechunk-*` names, then runs the sweep and asserts on the DB
// state for ITS hash only — concurrent test sweeps may migrate each other's
// blobs first, which is fine (and exercises the idempotency paths).
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(integration_tests)]
#[allow(dead_code)]
mod rechunk_integration_tests {
    use super::*;
    use crate::infrastructure::services::encrypted_blob_backend::EncryptedBlobBackend;
    use crate::infrastructure::services::local_blob_backend::LocalBlobBackend;
    use crate::integration_test_support::{ensure_clean_test_db, test_db_url};
    use sqlx::Row;
    use sqlx::postgres::PgPoolOptions;
    use tempfile::TempDir;
    use uuid::Uuid;

    async fn test_pool() -> Arc<PgPool> {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&test_db_url())
            .await
            .expect("connect to test DB — run tests/common/spawn-db.sh first");
        ensure_clean_test_db(&pool).await;
        Arc::new(pool)
    }

    async fn seed_user(pool: &PgPool) -> Uuid {
        sqlx::query("SELECT id FROM auth.users LIMIT 1")
            .fetch_one(pool)
            .await
            .map(|r| r.get::<Uuid, _>("id"))
            .expect("auth.users must be seeded (init-test-schema.sh)")
    }

    /// Plain local backend in a fresh temp dir.
    async fn local_svc(pool: &Arc<PgPool>, dir: &TempDir) -> DedupService {
        let backend = Arc::new(LocalBlobBackend::new(&dir.path().join("blobs")));
        backend.initialize().await.expect("init backend");
        DedupService::new(backend, pool.clone(), pool.clone())
    }

    /// AES-256-GCM-encrypted local backend in a fresh temp dir.
    async fn encrypted_svc(pool: &Arc<PgPool>, dir: &TempDir) -> DedupService {
        let inner = Arc::new(LocalBlobBackend::new(&dir.path().join("blobs")));
        inner.initialize().await.expect("init backend");
        let key = EncryptedBlobBackend::generate_key();
        let backend = Arc::new(EncryptedBlobBackend::new(inner, &key));
        DedupService::new(backend, pool.clone(), pool.clone())
    }

    /// Non-trivial content of `len` bytes + a random 16-byte tail, so every
    /// invocation produces a unique hash — stale rows left behind by a
    /// previously failed run (panics skip cleanup) can never collide with
    /// the current one.
    fn content(len: usize, salt: u8) -> Vec<u8> {
        let mut data: Vec<u8> = (0..len)
            .map(|i| {
                ((i % 251) as u8)
                    .wrapping_add(salt)
                    .wrapping_add((i / 7919) as u8)
            })
            .collect();
        data.extend_from_slice(Uuid::new_v4().as_bytes());
        data
    }

    /// Seed a pre-CDC legacy blob: physical blob via the backend + a
    /// `storage.blobs` row (ref_count = n_files) + `n_files` file rows.
    /// Returns (hash, file row ids). When `corrupt_stored_bytes` is Some,
    /// the PHYSICAL content differs from the indexed hash.
    async fn seed_legacy(
        svc: &DedupService,
        pool: &PgPool,
        dir: &TempDir,
        data: &[u8],
        n_files: i32,
        label: &str,
        corrupt_stored_bytes: Option<&[u8]>,
    ) -> (String, Vec<Uuid>) {
        let hash = blake3::hash(data).to_hex().to_string();
        let stored = corrupt_stored_bytes.unwrap_or(data);

        let src = dir.path().join(format!("seed-{label}.tmp"));
        tokio::fs::write(&src, stored).await.expect("write seed");
        svc.backend().put_blob(&hash, &src).await.expect("put blob");

        sqlx::query(
            "INSERT INTO storage.blobs (hash, size, ref_count, content_type)
             VALUES ($1, $2, $3, 'application/octet-stream')
             ON CONFLICT (hash) DO UPDATE SET ref_count = storage.blobs.ref_count + $3",
        )
        .bind(&hash)
        .bind(data.len() as i64)
        .bind(n_files)
        .execute(pool)
        .await
        .expect("insert legacy blob row");

        let user_id = seed_user(pool).await;
        let mut file_ids = Vec::new();
        for i in 0..n_files {
            let name = format!(
                "rust-test-rechunk-{label}-{}-{i}",
                &Uuid::new_v4().to_string()[..8]
            );
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO storage.files (name, user_id, blob_hash, size)
                 VALUES ($1, $2, $3, $4) RETURNING id",
            )
            .bind(&name)
            .bind(user_id)
            .bind(&hash)
            .bind(data.len() as i64)
            .fetch_one(pool)
            .await
            .expect("insert file row");
            file_ids.push(id);
        }
        (hash, file_ids)
    }

    /// Best-effort cleanup of everything a test seeded/created for `hash`.
    async fn cleanup(pool: &PgPool, hash: &str, file_ids: &[Uuid]) {
        let chunks: Option<Vec<String>> = sqlx::query_scalar(
            "SELECT chunk_hashes FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(hash)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);

        let _ = sqlx::query("DELETE FROM storage.files WHERE id = ANY($1)")
            .bind(file_ids)
            .execute(pool)
            .await;
        // Also scrub test-named rows from previously failed runs (panics
        // skip the end-of-test cleanup) that reference the same hash.
        let _ = sqlx::query(
            "DELETE FROM storage.files
              WHERE blob_hash = $1 AND name LIKE 'rust-test-rechunk-%'",
        )
        .bind(hash)
        .execute(pool)
        .await;
        let _ = sqlx::query("DELETE FROM storage.chunk_manifests WHERE file_hash = $1")
            .bind(hash)
            .execute(pool)
            .await;
        let mut to_drop = chunks.unwrap_or_default();
        to_drop.push(hash.to_string());
        let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = ANY($1)")
            .bind(&to_drop)
            .execute(pool)
            .await;
    }

    async fn collect(svc: &DedupService, hash: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut stream = svc.read_blob_stream(hash).await.expect("stream");
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        out
    }

    /// Manifest row (ref_count, total_size, chunk_hashes), if present.
    async fn manifest(pool: &PgPool, hash: &str) -> Option<(i32, i64, Vec<String>)> {
        sqlx::query_as(
            "SELECT ref_count, total_size, chunk_hashes
               FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(hash)
        .fetch_optional(pool)
        .await
        .expect("manifest query")
    }

    async fn blob_row(pool: &PgPool, hash: &str) -> Option<i32> {
        sqlx::query_scalar("SELECT ref_count FROM storage.blobs WHERE hash = $1")
            .bind(hash)
            .fetch_optional(pool)
            .await
            .expect("blob query")
    }

    // ── 1. Multi-chunk blob: refs move to manifest, whole-file blob freed ──
    #[tokio::test]
    async fn rechunk_multi_chunk_moves_refs_and_frees_blob() {
        let pool = test_pool().await;
        let dir = TempDir::new().unwrap();
        let svc = local_svc(&pool, &dir).await;

        // 3 MiB ⇒ ≥ 3 CDC chunks (max chunk = 1 MiB), 2 referencing files.
        let data = content(3 * 1024 * 1024, 1);
        let (hash, files) = seed_legacy(&svc, &pool, &dir, &data, 2, "multi", None).await;

        assert!(svc.count_legacy_blobs().await.unwrap() >= 1);
        svc.rechunk_legacy_blobs().await.expect("sweep");

        let (rc, total, chunks) = manifest(&pool, &hash).await.expect("manifest created");
        assert_eq!(rc, 2, "both file references must move to the manifest");
        assert_eq!(total, data.len() as i64);
        assert!(chunks.len() >= 3, "3 MiB must split into ≥3 chunks");

        // Whole-file blob fully dereferenced: row gone, physical file gone.
        assert_eq!(blob_row(&pool, &hash).await, None);
        assert!(!svc.backend().blob_exists(&hash).await.unwrap());

        // Every chunk row carries exactly the manifest's reference.
        for c in &chunks {
            assert_eq!(blob_row(&pool, c).await, Some(1), "chunk {c}");
        }

        // Content integrity through the chunked read path + a Range that
        // crosses a chunk boundary.
        assert_eq!(collect(&svc, &hash).await, data);
        let mut ranged = Vec::new();
        let mut s = svc
            .read_blob_range_stream(&hash, 1_500_000, Some(1_500_100))
            .await
            .expect("range");
        while let Some(chunk) = s.next().await {
            ranged.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(ranged, &data[1_500_000..1_500_100]);

        cleanup(&pool, &hash, &files).await;
    }

    // ── 2. Single-chunk blob: physical blob IS the chunk and must survive ──
    #[tokio::test]
    async fn rechunk_single_chunk_keeps_physical_blob() {
        let pool = test_pool().await;
        let dir = TempDir::new().unwrap();
        let svc = local_svc(&pool, &dir).await;

        // 50 KB < CDC_MIN_CHUNK ⇒ exactly one chunk whose hash == file hash.
        let data = content(50 * 1024, 2);
        let (hash, files) = seed_legacy(&svc, &pool, &dir, &data, 1, "single", None).await;

        svc.rechunk_legacy_blobs().await.expect("sweep");

        let (rc, total, chunks) = manifest(&pool, &hash).await.expect("manifest created");
        assert_eq!(rc, 1);
        assert_eq!(total, data.len() as i64);
        assert_eq!(chunks, vec![hash.clone()], "the file IS its single chunk");

        // Blob row survives with exactly the manifest's chunk reference;
        // the physical bytes were never rewritten.
        assert_eq!(blob_row(&pool, &hash).await, Some(1));
        assert!(svc.backend().blob_exists(&hash).await.unwrap());
        assert_eq!(collect(&svc, &hash).await, data);

        cleanup(&pool, &hash, &files).await;
    }

    // ── 3. Corrupt blob (content ≠ hash): fail, count, leave untouched ──
    #[tokio::test]
    async fn rechunk_corrupt_blob_left_untouched() {
        let pool = test_pool().await;
        let dir = TempDir::new().unwrap();
        let svc = local_svc(&pool, &dir).await;

        let data = content(100 * 1024, 3);
        let mut wrong = data.clone();
        wrong[0] ^= 0xFF;
        let (hash, files) = seed_legacy(&svc, &pool, &dir, &data, 1, "corrupt", Some(&wrong)).await;

        let report = svc.rechunk_legacy_blobs().await.expect("sweep");
        assert!(report.failed >= 1, "the corrupt blob must be counted");

        // Nothing was touched: no manifest, blob row + refs + file intact.
        assert_eq!(manifest(&pool, &hash).await, None);
        assert_eq!(blob_row(&pool, &hash).await, Some(1));
        assert!(svc.backend().blob_exists(&hash).await.unwrap());
        let files_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM storage.files WHERE blob_hash = $1")
                .bind(&hash)
                .fetch_one(pool.as_ref())
                .await
                .unwrap();
        assert_eq!(files_left, 1);

        cleanup(&pool, &hash, &files).await;
    }

    // ── 4. Empty blob: empty manifest, empty stream ──
    #[tokio::test]
    async fn rechunk_empty_blob() {
        let pool = test_pool().await;
        let dir = TempDir::new().unwrap();
        let svc = local_svc(&pool, &dir).await;

        // The empty-content hash is a constant (no per-run uniqueness is
        // possible), so scrub any leftovers from a previously failed run.
        let empty_hash = blake3::hash(&[]).to_hex().to_string();
        cleanup(&pool, &empty_hash, &[]).await;

        let (hash, files) = seed_legacy(&svc, &pool, &dir, &[], 1, "empty", None).await;

        svc.rechunk_legacy_blobs().await.expect("sweep");

        let (rc, total, chunks) = manifest(&pool, &hash).await.expect("manifest created");
        assert_eq!((rc, total), (1, 0));
        assert!(chunks.is_empty());
        assert!(collect(&svc, &hash).await.is_empty());

        cleanup(&pool, &hash, &files).await;
    }

    // ── 5. Encrypted backend: spool decrypts, chunks re-encrypt, Range works ──
    #[tokio::test]
    async fn rechunk_encrypted_multi_chunk_roundtrip() {
        let pool = test_pool().await;
        let dir = TempDir::new().unwrap();
        let svc = encrypted_svc(&pool, &dir).await;

        let data = content(2 * 1024 * 1024 + 333, 4);
        let (hash, files) = seed_legacy(&svc, &pool, &dir, &data, 1, "enc", None).await;

        svc.rechunk_legacy_blobs().await.expect("sweep");

        let (rc, total, chunks) = manifest(&pool, &hash).await.expect("manifest created");
        assert_eq!(rc, 1);
        assert_eq!(total, data.len() as i64);
        assert!(chunks.len() >= 2);
        assert_eq!(blob_row(&pool, &hash).await, None, "whole-file blob freed");

        // The point of the whole migration: a Range read now decrypts only
        // the overlapping ≤1 MiB chunks, and returns correct plaintext.
        assert_eq!(collect(&svc, &hash).await, data);
        let mut ranged = Vec::new();
        let mut s = svc
            .read_blob_range_stream(&hash, 1_100_000, Some(1_100_064))
            .await
            .expect("range");
        while let Some(chunk) = s.next().await {
            ranged.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(ranged, &data[1_100_000..1_100_064]);

        cleanup(&pool, &hash, &files).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Integration tests for the delta-upload primitives — the entitlement and
// verification rules the chunk-negotiation protocol stands on. Same gating
// and DB conventions as the re-chunk suite above.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(integration_tests)]
#[allow(dead_code)]
mod delta_upload_integration_tests {
    use super::*;
    use crate::infrastructure::services::local_blob_backend::LocalBlobBackend;
    use crate::integration_test_support::{ensure_clean_test_db, test_db_url};
    use sqlx::Row;
    use sqlx::postgres::PgPoolOptions;
    use tempfile::TempDir;
    use uuid::Uuid;

    async fn test_pool() -> Arc<PgPool> {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&test_db_url())
            .await
            .expect("connect to test DB — run tests/common/spawn-db.sh first");
        ensure_clean_test_db(&pool).await;
        Arc::new(pool)
    }

    async fn seed_user(pool: &PgPool) -> Uuid {
        sqlx::query("SELECT id FROM auth.users LIMIT 1")
            .fetch_one(pool)
            .await
            .map(|r| r.get::<Uuid, _>("id"))
            .expect("auth.users must be seeded (init-test-schema.sh)")
    }

    async fn local_svc(pool: &Arc<PgPool>, dir: &TempDir) -> DedupService {
        let backend = Arc::new(LocalBlobBackend::new(&dir.path().join("blobs")));
        backend.initialize().await.expect("init backend");
        DedupService::new(backend, pool.clone(), pool.clone())
    }

    /// Store `data` through the streaming path and give `user_id` a file
    /// row referencing it — making its chunks claimable by that user.
    async fn seed_owned_content(
        svc: &DedupService,
        pool: &PgPool,
        user_id: Uuid,
        data: &[u8],
        label: &str,
    ) -> (String, Vec<String>, Uuid) {
        let source = stream::iter(vec![Ok::<_, std::io::Error>(Bytes::copy_from_slice(data))]);
        let stored = svc
            .store_from_stream(source, Some("application/octet-stream".into()))
            .await
            .expect("store");
        let file_hash = stored.hash().to_string();
        let chunks: Vec<String> = sqlx::query_scalar(
            "SELECT UNNEST(chunk_hashes) FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(&file_hash)
        .fetch_all(pool)
        .await
        .expect("chunks");

        let file_id: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.files (name, user_id, blob_hash, size)
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(format!(
            "rust-test-delta-{label}-{}",
            &Uuid::new_v4().to_string()[..8]
        ))
        .bind(user_id)
        .bind(&file_hash)
        .bind(data.len() as i64)
        .fetch_one(pool)
        .await
        .expect("file row");
        (file_hash, chunks, file_id)
    }

    async fn blob_ref(pool: &PgPool, hash: &str) -> Option<i32> {
        sqlx::query_scalar("SELECT ref_count FROM storage.blobs WHERE hash = $1")
            .bind(hash)
            .fetch_optional(pool)
            .await
            .expect("blob query")
    }

    async fn cleanup(pool: &PgPool, file_hash: &str, file_id: Uuid, extra_hashes: &[String]) {
        let chunks: Option<Vec<String>> = sqlx::query_scalar(
            "SELECT chunk_hashes FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(file_hash)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
        let _ = sqlx::query("DELETE FROM storage.files WHERE id = $1")
            .bind(file_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM storage.chunk_manifests WHERE file_hash = $1")
            .bind(file_hash)
            .execute(pool)
            .await;
        let mut to_drop = chunks.unwrap_or_default();
        to_drop.push(file_hash.to_string());
        to_drop.extend_from_slice(extra_hashes);
        let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = ANY($1)")
            .bind(&to_drop)
            .execute(pool)
            .await;
    }

    fn content(len: usize, salt: u8) -> Vec<u8> {
        let mut data: Vec<u8> = (0..len)
            .map(|i| {
                ((i % 251) as u8)
                    .wrapping_add(salt)
                    .wrapping_add((i / 7919) as u8)
            })
            .collect();
        data.extend_from_slice(Uuid::new_v4().as_bytes());
        data
    }

    // ── Entitlement: claimable vs pin ────────────────────────────
    #[tokio::test]
    async fn claim_and_pin_respect_ownership_and_orphans() {
        let pool = test_pool().await;
        let dir = TempDir::new().unwrap();
        let svc = local_svc(&pool, &dir).await;
        let user = seed_user(&pool).await;

        // Owned content (multi-chunk), one foreign chunk (ref 1, no file
        // row for this user), one orphan (ref 0), one unknown hash.
        let data = content(3 * 1024 * 1024, 21);
        let (file_hash, owned_chunks, file_id) =
            seed_owned_content(&svc, &pool, user, &data, "claim").await;
        assert!(owned_chunks.len() >= 3, "3 MiB must split into ≥3 chunks");

        let foreign = blake3::hash(format!("foreign-{}", Uuid::new_v4()).as_bytes())
            .to_hex()
            .to_string();
        let orphan = blake3::hash(format!("orphan-{}", Uuid::new_v4()).as_bytes())
            .to_hex()
            .to_string();
        sqlx::query(
            "INSERT INTO storage.blobs (hash, size, ref_count) VALUES ($1, 10, 1), ($2, 10, 0)",
        )
        .bind(&foreign)
        .bind(&orphan)
        .execute(pool.as_ref())
        .await
        .expect("seed foreign+orphan");
        let unknown = blake3::hash(format!("unknown-{}", Uuid::new_v4()).as_bytes())
            .to_hex()
            .to_string();

        let mut probe: Vec<String> = owned_chunks.clone();
        probe.push(foreign.clone());
        probe.push(orphan.clone());
        probe.push(unknown.clone());

        // claimable: only the owned chunks (advisory view — orphans are
        // intentionally NOT advertised; the commit pin may still take them).
        let claimable = svc.claimable_chunks(user, &probe).await.expect("claimable");
        for c in &owned_chunks {
            assert!(claimable.contains(c), "owned chunk {c} must be claimable");
        }
        assert!(
            !claimable.contains(&foreign),
            "foreign chunk must not be claimable"
        );
        assert!(
            !claimable.contains(&unknown),
            "unknown chunk must not be claimable"
        );

        // pin: owned + orphan succeed; foreign and unknown are refused.
        let pinned = svc.pin_claimable_chunks(user, &probe).await.expect("pin");
        for c in &owned_chunks {
            assert!(pinned.contains(c), "owned chunk {c} must pin");
        }
        assert!(
            pinned.contains(&orphan),
            "ref-0 orphan must pin (just-uploaded state)"
        );
        assert!(
            !pinned.contains(&foreign),
            "foreign owned chunk must NOT pin"
        );
        assert!(!pinned.contains(&unknown), "unknown hash must NOT pin");

        // Ref counts moved exactly where they should.
        assert_eq!(blob_ref(&pool, &orphan).await, Some(1), "orphan 0→1");
        assert_eq!(
            blob_ref(&pool, &foreign).await,
            Some(1),
            "foreign untouched"
        );
        assert_eq!(
            blob_ref(&pool, &owned_chunks[0]).await,
            Some(2),
            "owned chunk 1→2 (manifest + pin)"
        );

        // Release restores the original counts (clamped at 0).
        let pinned_vec: Vec<String> = pinned.into_iter().collect();
        svc.release_pinned_chunks(&pinned_vec).await;
        assert_eq!(blob_ref(&pool, &orphan).await, Some(0));
        assert_eq!(blob_ref(&pool, &owned_chunks[0]).await, Some(1));

        cleanup(&pool, &file_hash, file_id, &[foreign, orphan]).await;
    }

    // ── Loose chunk store ────────────────────────────────────────
    #[tokio::test]
    async fn loose_chunks_register_as_orphans_without_touching_existing_refs() {
        let pool = test_pool().await;
        let dir = TempDir::new().unwrap();
        let svc = local_svc(&pool, &dir).await;
        let user = seed_user(&pool).await;

        // An owned chunk that the client redundantly re-uploads.
        let data = content(100 * 1024, 22);
        let (file_hash, owned_chunks, file_id) =
            seed_owned_content(&svc, &pool, user, &data, "loose").await;
        let owned_chunk_bytes = {
            let mut stream = svc.read_blob_stream(&file_hash).await.expect("stream");
            let mut out = Vec::new();
            while let Some(part) = stream.next().await {
                out.extend_from_slice(&part.expect("part"));
            }
            out
        };

        let fresh = content(50 * 1024, 23);
        let frames = stream::iter(vec![
            Ok::<_, DomainError>(Bytes::from(fresh.clone())),
            Ok(Bytes::from(fresh.clone())), // duplicate frame
            Ok(Bytes::from(owned_chunk_bytes.clone())), // already-referenced chunk
        ]);

        let received = svc.store_loose_chunks(frames).await.expect("store loose");
        assert_eq!(received.len(), 3, "every frame is answered, in order");
        assert_eq!(
            received[0].0, received[1].0,
            "duplicate frames share a hash"
        );
        let fresh_hash = received[0].0.clone();

        assert_eq!(
            blob_ref(&pool, &fresh_hash).await,
            Some(0),
            "fresh chunk lands as an unreferenced orphan"
        );
        assert_eq!(
            blob_ref(&pool, &owned_chunks[0]).await,
            Some(1),
            "re-uploading an existing chunk must not disturb its refs"
        );

        // The orphan's bytes are really there and addressable.
        assert_eq!(
            svc.backend().blob_exists(&fresh_hash).await.unwrap(),
            true,
            "orphan chunk bytes must exist in the backend"
        );

        cleanup(&pool, &file_hash, file_id, &[fresh_hash]).await;
    }

    // ── Verification read ────────────────────────────────────────
    #[tokio::test]
    async fn hash_chunk_sequence_recomputes_and_validates_sizes() {
        let pool = test_pool().await;
        let dir = TempDir::new().unwrap();
        let svc = local_svc(&pool, &dir).await;
        let user = seed_user(&pool).await;

        let data = content(2 * 1024 * 1024 + 137, 24);
        let (file_hash, _chunks, file_id) =
            seed_owned_content(&svc, &pool, user, &data, "verify").await;

        let manifest: (Vec<String>, Vec<i64>) = sqlx::query_as(
            "SELECT chunk_hashes, chunk_sizes FROM storage.chunk_manifests WHERE file_hash = $1",
        )
        .bind(&file_hash)
        .fetch_one(pool.as_ref())
        .await
        .expect("manifest");
        let sequence: Vec<(String, u64)> = manifest
            .0
            .iter()
            .cloned()
            .zip(manifest.1.iter().map(|s| *s as u64))
            .collect();

        let (computed, head) = svc
            .hash_chunk_sequence(&sequence, 16)
            .await
            .expect("verification read");
        assert_eq!(computed, file_hash, "recomputed hash must match");
        assert_eq!(
            &head[..],
            &data[..16],
            "sniff head must be the file's first bytes"
        );

        // A wrong declared size must be rejected — Range arithmetic
        // depends on manifest sizes being true.
        let mut lying = sequence.clone();
        lying[0].1 += 1;
        assert!(
            svc.hash_chunk_sequence(&lying, 0).await.is_err(),
            "size lie must fail verification"
        );

        cleanup(&pool, &file_hash, file_id, &[]).await;
    }
}
