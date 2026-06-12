//! Delta-upload protocol — "upload only what changed".
//!
//! The CDC dedup store already shares unchanged chunks between file
//! versions *after* the bytes arrive; this protocol moves that detection
//! to the client side so unchanged bytes never cross the wire:
//!
//! 1. `negotiate`: the client sends the chunk hashes that compose its
//!    file; the server answers which of them it cannot claim — only those
//!    need uploading.
//! 2. `chunks`: the client uploads the missing chunks (raw frames). The
//!    server recomputes every hash itself and registers the chunks as
//!    unreferenced (`ref_count = 0`) orphans — pinned by the commit that
//!    follows, or swept by the periodic GC if the client never returns.
//! 3. `commit`: the server pins one reference per distinct chunk (only
//!    chunks the caller owns or unreferenced orphans — see the security
//!    notes in `dedup_service.rs`), **re-reads the proposed sequence and
//!    recomputes the whole-file BLAKE3** (a declared hash is never
//!    trusted: a forged manifest would poison future whole-file dedup
//!    hits for other users), attaches the manifest with the same
//!    accounting as the streaming ingest, and creates or updates the
//!    file row.
//!
//! Stateless by design: there is no session table. Every step re-derives
//! its facts from the chunk store, and the GC reclaims anything a client
//! abandons mid-protocol.

use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::file_ports::{FileUploadUseCase, StoredBlob};
use crate::application::ports::storage_ports::{FileReadPort, StorageUsagePort};
use crate::application::services::file_upload_service::FileUploadService;
use crate::application::services::storage_usage_service::StorageUsageService;
use crate::common::errors::DomainError;
use crate::common::mime_detect::{MAGIC_BYTES_LEN, refine_content_type};
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::repositories::pg::FileBlobReadRepository;
use crate::infrastructure::services::dedup_service::{CDC_MAX_CHUNK, DedupService};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

// ── Wire DTOs ────────────────────────────────────────────────────────────────

/// One chunk reference: `h` = BLAKE3 hex (64 chars), `s` = size in bytes.
/// Field names are deliberately terse — a 10 GB file is ~40 000 of these.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChunkRef {
    /// BLAKE3 hash of the chunk (64 hex chars).
    pub h: String,
    /// Chunk size in bytes (1 ..= 1 MiB).
    pub s: u64,
}

/// Request body of `POST /api/files/delta/negotiate`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeltaNegotiateRequest {
    /// The file's chunks, in order (duplicates allowed — repeated content).
    pub chunks: Vec<ChunkRef>,
}

/// Response of `POST /api/files/delta/negotiate`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeltaNegotiateResponse {
    /// Distinct chunk hashes the caller must upload (first-occurrence order).
    pub missing: Vec<String>,
}

/// Response of `PUT /api/files/delta/chunks` — the server-computed identity
/// of every received frame, in wire order. Clients compare against their
/// own hashes to detect corruption before committing.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeltaChunksResponse {
    pub received: Vec<ChunkRef>,
}

/// Request body of `POST /api/files/delta/commit`.
///
/// Exactly one of (`name` + `folder_id`) or `file_id` selects the mode:
/// create a new file, or replace an existing file's content.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeltaCommitRequest {
    /// BLAKE3 of the complete file (verified server-side, never trusted).
    pub file_hash: String,
    /// Full chunk sequence, in file order (per occurrence).
    pub chunks: Vec<ChunkRef>,
    /// Create mode: file name (basename).
    pub name: Option<String>,
    /// Create mode: target folder (caller needs Create permission).
    pub folder_id: Option<String>,
    /// Update mode: file whose content is replaced (caller needs Write).
    pub file_id: Option<String>,
}

/// Response of `GET /api/files/{id}/manifest` — the recipe to rebuild the
/// file from chunks. Immutable for a given `file_hash`, so clients cache
/// it keyed by hash (the endpoint also serves it with `ETag: file_hash`).
#[derive(Debug, Serialize, ToSchema)]
pub struct DeltaManifestResponse {
    /// BLAKE3 of the whole file (verify the local reassembly against it).
    pub file_hash: String,
    /// Total size in bytes.
    pub total_size: u64,
    /// Full chunk sequence, in file order (per occurrence).
    pub chunks: Vec<ChunkRef>,
}

/// Request body of `POST /api/files/delta/download` — distinct chunk
/// hashes to fetch, served back as `[u32 BE length][bytes]` frames in
/// request order (the same wire format the upload direction uses).
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeltaDownloadRequest {
    pub hashes: Vec<String>,
}

/// Outcome of a chunk-download authorization.
pub enum DeltaDownloadOutcome {
    /// Every requested chunk is servable: `(hash, size)` in request order.
    Ready(Vec<(String, u64)>),
    /// Some chunks are not available to this caller (not reachable through
    /// their files, or unknown — deliberately indistinguishable).
    NotAvailable(Vec<String>),
}

/// Resolved commit mode after request validation.
enum CommitMode {
    Create { name: String, folder_id: String },
    Update { file_id: String },
}

/// Outcome of a commit attempt.
pub enum DeltaCommitOutcome {
    /// The file row exists; `created` distinguishes 201 from 200.
    Done { file: Box<FileDto>, created: bool },
    /// Some chunks could not be pinned (GC race, skipped negotiate, or
    /// chunks the caller may not claim). The client uploads exactly these
    /// and retries the same commit.
    StillMissing(Vec<String>),
}

// ── Service ──────────────────────────────────────────────────────────────────

/// Orchestrates the three delta-upload steps. All authorization lives here
/// (service layer), per the project's AuthZ rule; handlers only
/// authenticate, rate-limit and translate the wire format.
pub struct DeltaUploadService {
    dedup: Arc<DedupService>,
    uploads: Arc<FileUploadService>,
    file_read: Arc<FileBlobReadRepository>,
    quota: Arc<StorageUsageService>,
    authz: Arc<PgAclEngine>,
    /// Whole-file ceiling — same `max_upload_size` that bounds byte uploads.
    max_total_size: u64,
    /// Per-request ceiling for batched chunk downloads — same budget as
    /// the chunk-upload requests (`chunk_max_bytes`).
    max_download_batch: u64,
}

impl DeltaUploadService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dedup: Arc<DedupService>,
        uploads: Arc<FileUploadService>,
        file_read: Arc<FileBlobReadRepository>,
        quota: Arc<StorageUsageService>,
        authz: Arc<PgAclEngine>,
        max_total_size: u64,
        max_download_batch: u64,
    ) -> Self {
        Self {
            dedup,
            uploads,
            file_read,
            quota,
            authz,
            max_total_size,
            max_download_batch,
        }
    }

    /// Most chunks a single request may reference: the whole-file ceiling
    /// divided by the smallest possible CDC chunk, with headroom for
    /// fixed-size client chunkers.
    fn max_chunk_count(&self) -> usize {
        (self.max_total_size as usize
            / crate::infrastructure::services::dedup_service::CDC_MIN_CHUNK)
            .saturating_mul(2)
            .max(1024)
    }

    /// Shape-validate a chunk list: hash format, per-chunk size bounds,
    /// count and total ceilings. Returns the total size.
    fn validate_chunk_list(&self, chunks: &[ChunkRef]) -> Result<u64, DomainError> {
        if chunks.len() > self.max_chunk_count() {
            return Err(DomainError::validation_error(format!(
                "Too many chunks: {} (maximum {})",
                chunks.len(),
                self.max_chunk_count()
            )));
        }
        let mut total: u64 = 0;
        for chunk in chunks {
            if !is_valid_hash(&chunk.h) {
                return Err(DomainError::validation_error(
                    "Invalid chunk hash format. Expected BLAKE3 (64 hex characters)",
                ));
            }
            if chunk.s == 0 || chunk.s > CDC_MAX_CHUNK as u64 {
                return Err(DomainError::validation_error(format!(
                    "Chunk size {} out of bounds (1 ..= {CDC_MAX_CHUNK})",
                    chunk.s
                )));
            }
            total = total.saturating_add(chunk.s);
        }
        if total > self.max_total_size {
            return Err(DomainError::validation_error(format!(
                "Declared total of {total} bytes exceeds the {}-byte upload ceiling",
                self.max_total_size
            )));
        }
        Ok(total)
    }

    /// Step 1: which of these chunks must the caller upload?
    ///
    /// Purely advisory and user-scoped — the commit re-checks entitlement
    /// atomically, so a stale answer can never leak content.
    pub async fn negotiate_with_perms(
        &self,
        caller_id: Uuid,
        request: &DeltaNegotiateRequest,
    ) -> Result<DeltaNegotiateResponse, DomainError> {
        self.validate_chunk_list(&request.chunks)?;

        let distinct = distinct_hashes(&request.chunks);
        let claimable = self.dedup.claimable_chunks(caller_id, &distinct).await?;
        let missing = distinct
            .into_iter()
            .filter(|h| !claimable.contains(h))
            .collect();
        Ok(DeltaNegotiateResponse { missing })
    }

    /// Step 2: store uploaded chunk frames. Hashes are computed
    /// server-side; chunks land as unreferenced orphans awaiting a commit.
    pub async fn receive_chunks<S>(&self, frames: S) -> Result<DeltaChunksResponse, DomainError>
    where
        S: Stream<Item = Result<Bytes, DomainError>> + Send,
    {
        let received = self.dedup.store_loose_chunks(frames).await?;
        Ok(DeltaChunksResponse {
            received: received
                .into_iter()
                .map(|(h, s)| ChunkRef { h, s })
                .collect(),
        })
    }

    /// Step 3: pin → verify → attach manifest → create/update the file row.
    pub async fn commit_with_perms(
        &self,
        caller_id: Uuid,
        request: DeltaCommitRequest,
    ) -> Result<DeltaCommitOutcome, DomainError> {
        // ── Shape ─────────────────────────────────────────────────
        if !is_valid_hash(&request.file_hash) {
            return Err(DomainError::validation_error(
                "Invalid file_hash format. Expected BLAKE3 (64 hex characters)",
            ));
        }
        let total_size = self.validate_chunk_list(&request.chunks)?;

        let mode = match (&request.name, &request.folder_id, &request.file_id) {
            (Some(name), Some(folder_id), None) => {
                let name = sanitize_file_name(name)?;
                CommitMode::Create {
                    name,
                    folder_id: folder_id.clone(),
                }
            }
            (None, None, Some(file_id)) => CommitMode::Update {
                file_id: file_id.clone(),
            },
            _ => {
                return Err(DomainError::validation_error(
                    "Provide either name + folder_id (create) or file_id (update)",
                ));
            }
        };

        // ── AuthZ first: nothing is pinned for callers who may not write ──
        match &mode {
            CommitMode::Create { folder_id, .. } => {
                let folder_uuid = Uuid::parse_str(folder_id)
                    .map_err(|_| DomainError::not_found("Folder", folder_id.clone()))?;
                self.authz
                    .require(
                        Subject::User(caller_id),
                        Permission::Create,
                        Resource::Folder(folder_uuid),
                    )
                    .await?;
            }
            CommitMode::Update { file_id } => {
                let file_uuid = Uuid::parse_str(file_id)
                    .map_err(|_| DomainError::not_found("File", file_id.clone()))?;
                self.authz
                    .require(
                        Subject::User(caller_id),
                        Permission::Update,
                        Resource::File(file_uuid),
                    )
                    .await?;
            }
        }

        // ── Quota on the logical size (same semantics as a byte upload) ──
        self.quota
            .check_storage_quota(caller_id, total_size)
            .await?;

        // ── Whole-file fast path: caller already owns this exact content ──
        // Mirrors the instant-upload endpoint: a reference bump, no chunk
        // work at all. Ownership is required — an existing-but-foreign
        // manifest must be earned through the pin + verify path below.
        if self
            .dedup
            .user_owns_blob_reference(&request.file_hash, &caller_id.to_string())
            .await
            && let Some(metadata) = self.dedup.get_blob_metadata(&request.file_hash).await
        {
            self.dedup.add_reference(&request.file_hash).await?;
            let blob = StoredBlob {
                hash: request.file_hash.clone(),
                size: metadata.size,
                is_new_blob: false,
            };
            let file = self
                .register_row(caller_id, &mode, metadata.content_type, blob)
                .await?;
            return Ok(DeltaCommitOutcome::Done {
                file: Box::new(file),
                created: matches!(mode, CommitMode::Create { .. }),
            });
        }

        // ── Pin: atomically take one reference per distinct entitled chunk ──
        let distinct = distinct_hashes(&request.chunks);
        let pinned = self
            .dedup
            .pin_claimable_chunks(caller_id, &distinct)
            .await?;
        if pinned.len() != distinct.len() {
            let still_missing: Vec<String> = distinct
                .iter()
                .filter(|h| !pinned.contains(*h))
                .cloned()
                .collect();
            let pinned_vec: Vec<String> = pinned.into_iter().collect();
            self.dedup.release_pinned_chunks(&pinned_vec).await;
            tracing::debug!(
                "Delta commit: {} of {} chunks not claimable — client must upload them",
                still_missing.len(),
                distinct.len()
            );
            return Ok(DeltaCommitOutcome::StillMissing(still_missing));
        }

        // ── Verify: the declared file_hash is recomputed from the pinned
        //    bytes before any manifest row can exist. ──
        let verification = self
            .dedup
            .hash_chunk_sequence(
                &request
                    .chunks
                    .iter()
                    .map(|c| (c.h.clone(), c.s))
                    .collect::<Vec<_>>(),
                MAGIC_BYTES_LEN,
            )
            .await;
        let (computed_hash, head) = match verification {
            Ok(v) => v,
            Err(e) => {
                self.dedup.release_pinned_chunks(&distinct).await;
                tracing::info!(
                    target: "audit",
                    event = "delta_upload.rejected",
                    reason = "chunk_verification_failed",
                    caller_id = %caller_id,
                    file_hash = %request.file_hash,
                    "👮🏻‍♂️ Delta commit rejected: chunk sequence failed verification read",
                );
                return Err(e);
            }
        };
        if computed_hash != request.file_hash {
            self.dedup.release_pinned_chunks(&distinct).await;
            tracing::info!(
                target: "audit",
                event = "delta_upload.rejected",
                reason = "file_hash_mismatch",
                caller_id = %caller_id,
                declared_hash = %request.file_hash,
                computed_hash = %computed_hash,
                "👮🏻‍♂️ Delta commit rejected: declared file_hash does not match the chunk sequence",
            );
            return Err(DomainError::validation_error(
                "file_hash does not match the chunk sequence",
            ));
        }

        // ── Attach the manifest (shared accounting with the byte path) ──
        let display_name = match &mode {
            CommitMode::Create { name, .. } => name.clone(),
            CommitMode::Update { file_id } => file_id.clone(),
        };
        let content_type = match refine_content_type(&head, &display_name, "") {
            ct if ct.is_empty() => "application/octet-stream".to_string(),
            ct => ct,
        };
        let chunk_hashes: Vec<String> = request.chunks.iter().map(|c| c.h.clone()).collect();
        let chunk_sizes: Vec<u64> = request.chunks.iter().map(|c| c.s).collect();
        let attached = self
            .dedup
            .attach_manifest(
                &request.file_hash,
                &chunk_hashes,
                &chunk_sizes,
                total_size,
                Some(content_type.clone()),
                &distinct,
            )
            .await?;

        let blob = StoredBlob {
            hash: request.file_hash.clone(),
            size: attached.size(),
            is_new_blob: !attached.was_deduplicated(),
        };
        let file = self
            .register_row(caller_id, &mode, Some(content_type), blob)
            .await?;
        Ok(DeltaCommitOutcome::Done {
            file: Box::new(file),
            created: matches!(mode, CommitMode::Create { .. }),
        })
    }

    // ── Delta download ("download only what changed") ────────────

    /// The chunk recipe of a file the caller owns — step 1 of a delta
    /// download. Owner-scoped like the rest of the delta protocol (shared
    /// files use the regular download endpoints); a non-owned or unknown
    /// file is `NotFound`, and the denial is audited.
    pub async fn file_manifest_with_perms(
        &self,
        caller_id: Uuid,
        file_id: &str,
    ) -> Result<DeltaManifestResponse, DomainError> {
        let file_uuid = Uuid::parse_str(file_id)
            .map_err(|_| DomainError::not_found("File", file_id.to_string()))?;
        // Engine check first (Read on the file): owners always pass and
        // the engine audits denials; the ownership constraint below is the
        // chunk layer's own entitlement standard.
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Resource::File(file_uuid),
            )
            .await?;

        let file = self.file_read.get_file(file_id).await?;
        let file_hash = file.content_hash().to_string();
        if !self
            .dedup
            .user_owns_blob_reference(&file_hash, &caller_id.to_string())
            .await
        {
            // Read permission without ownership (e.g. a grant): the delta
            // surface is owner-scoped — the regular download endpoints
            // serve shared content.
            tracing::info!(
                target: "audit",
                event = "delta_download.rejected",
                reason = "manifest_not_owner",
                caller_id = %caller_id,
                file_id = %file_id,
                "👮🏻‍♂️ Delta download rejected: manifest requested by a non-owner",
            );
            return Err(DomainError::not_found("File", file_id.to_string()));
        }

        let Some((chunks, total_size)) = self.dedup.manifest_chunk_list(&file_hash).await? else {
            return Err(DomainError::not_found("File", file_id.to_string()));
        };
        Ok(DeltaManifestResponse {
            file_hash,
            total_size,
            chunks: chunks.into_iter().map(|(h, s)| ChunkRef { h, s }).collect(),
        })
    }

    /// Authorize a batched chunk download — step 2. Every hash must be
    /// reachable through the caller's own files; otherwise the full list of
    /// unavailable hashes is returned (the same information N individual
    /// requests would reveal, in one round-trip). The total payload is
    /// bounded by the per-request budget so clients split large deltas.
    pub async fn authorize_chunk_download_with_perms(
        &self,
        caller_id: Uuid,
        request: &DeltaDownloadRequest,
    ) -> Result<DeltaDownloadOutcome, DomainError> {
        if request.hashes.is_empty() {
            return Err(DomainError::validation_error("hashes must not be empty"));
        }
        if request.hashes.len() > self.max_chunk_count() {
            return Err(DomainError::validation_error(format!(
                "Too many chunks: {} (maximum {})",
                request.hashes.len(),
                self.max_chunk_count()
            )));
        }
        let mut distinct_seen = HashSet::new();
        for hash in &request.hashes {
            if !is_valid_hash(hash) {
                return Err(DomainError::validation_error(
                    "Invalid chunk hash format. Expected BLAKE3 (64 hex characters)",
                ));
            }
            if !distinct_seen.insert(hash.as_str()) {
                return Err(DomainError::validation_error(
                    "Duplicate hashes in download request",
                ));
            }
        }

        let entitled = self
            .dedup
            .claimable_chunks(caller_id, &request.hashes)
            .await?;
        let not_available: Vec<String> = request
            .hashes
            .iter()
            .filter(|h| !entitled.contains(*h))
            .cloned()
            .collect();
        if !not_available.is_empty() {
            tracing::info!(
                target: "audit",
                event = "delta_download.rejected",
                reason = "chunks_not_owned",
                caller_id = %caller_id,
                requested = request.hashes.len(),
                denied = not_available.len(),
                "👮🏻‍♂️ Delta download rejected: caller requested chunks outside their files",
            );
            return Ok(DeltaDownloadOutcome::NotAvailable(not_available));
        }

        let sizes = self.dedup.chunk_sizes(&request.hashes).await?;
        let mut ordered = Vec::with_capacity(request.hashes.len());
        let mut total: u64 = 0;
        for hash in &request.hashes {
            // Entitled implies indexed; a vanished row between the two
            // queries surfaces as unavailable rather than a 500.
            let Some(size) = sizes.get(hash) else {
                return Ok(DeltaDownloadOutcome::NotAvailable(vec![hash.clone()]));
            };
            total = total.saturating_add(*size);
            ordered.push((hash.clone(), *size));
        }
        if total > self.max_download_batch {
            return Err(DomainError::validation_error(format!(
                "Requested chunks total {total} bytes; the per-request ceiling is {} —                  split the download into smaller batches",
                self.max_download_batch
            )));
        }
        Ok(DeltaDownloadOutcome::Ready(ordered))
    }

    /// Stream one authorized chunk's bytes (entitlement was established by
    /// [`authorize_chunk_download_with_perms`]).
    pub async fn chunk_stream(
        &self,
        hash: &str,
    ) -> Result<
        std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
        DomainError,
    > {
        self.dedup.chunk_stream(hash).await
    }

    /// Create or update the file row against a blob reference the commit
    /// already holds (the registration paths release it on failure).
    async fn register_row(
        &self,
        caller_id: Uuid,
        mode: &CommitMode,
        content_type: Option<String>,
        blob: StoredBlob,
    ) -> Result<FileDto, DomainError> {
        match mode {
            CommitMode::Create { name, folder_id } => {
                let content_type =
                    content_type.unwrap_or_else(|| "application/octet-stream".to_string());
                self.uploads
                    .upload_file_streaming(
                        name.clone(),
                        Some(folder_id.clone()),
                        content_type,
                        blob,
                    )
                    .await
            }
            CommitMode::Update { file_id } => {
                self.uploads
                    .update_file_content_by_id_with_perms(caller_id, file_id, blob)
                    .await
            }
        }
    }
}

/// 64 lowercase/uppercase hex characters.
fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

/// Basename only — same path-traversal guard as the upload handlers.
fn sanitize_file_name(name: &str) -> Result<String, DomainError> {
    let base = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .rsplit('\\')
        .next()
        .unwrap_or(name)
        .trim();
    if base.is_empty() {
        return Err(DomainError::validation_error("File name must not be empty"));
    }
    Ok(base.to_string())
}

/// Distinct hashes in first-occurrence order.
fn distinct_hashes(chunks: &[ChunkRef]) -> Vec<String> {
    let mut seen = HashSet::new();
    chunks
        .iter()
        .filter(|c| seen.insert(c.h.as_str()))
        .map(|c| c.h.clone())
        .collect()
}
