//! Chunked Upload Service — TUS-like Protocol for Large File Uploads
//!
//! Enables parallel chunk uploads for files >10 MB with:
//! - **Persistent, resumable uploads** — progress survives server restarts.
//!   A `session.json` (written once on create) + a `progress.bin` bitmask
//!   (updated atomically on each chunk) are stored alongside the chunk files.
//!   On boot the service scans `temp_base_dir` and recovers any active sessions.
//! - Parallel chunk transfers (up to 6 concurrent)
//! - Completion streams the ordered parts straight into the CDC blob store
//! - Expiration cleanup (24 h)
//!
//! Protocol:
//! 1. POST /api/uploads     → Create upload session, get upload_id
//! 2. PATCH /api/uploads/:id → Upload chunks (parallel OK)
//! 3. HEAD /api/uploads/:id  → Check progress
//! 4. POST /api/uploads/:id/complete → Stream parts into the blob store

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::application::ports::chunked_upload_ports::{
    ChunkUploadResponseDto, ChunkedUploadPort, CompletedUploadParts, CreateUploadResponseDto,
    UploadStatusResponseDto,
};
use crate::domain::errors::{DomainError, ErrorKind};

/// Minimum file size to use chunked upload (10 MB)
pub const CHUNKED_UPLOAD_THRESHOLD: usize = 10 * 1024 * 1024;

/// Default chunk size (5 MB) — optimised for parallel transfers
pub const DEFAULT_CHUNK_SIZE: usize = 5 * 1024 * 1024;

/// Maximum concurrent chunks per upload
pub const MAX_PARALLEL_CHUNKS: usize = 6;

/// Upload session expiration time (24 h)
const SESSION_EXPIRATION: Duration = Duration::from_secs(24 * 60 * 60);

/// Prefix every session directory name with this string so the cleanup
/// loop can be safely co-located with unrelated writers (PUT spool
/// tempfiles, the NC chunked subtree, anything else a sysadmin places
/// under the same `OXICLOUD_CHUNK_DIR`). The orphan-cleanup scan
/// filters by this prefix, so non-OxiCloud directories sharing the
/// root are never touched.
const SESSION_DIR_PREFIX: &str = "oxi-chunk-";

/// Sentinel file names inside each session directory
const SESSION_META_FILE: &str = "session.json";
const PROGRESS_FILE: &str = "progress.bin";

/// Build a session directory name from an upload_id by attaching the
/// well-known prefix. Symmetric with [`strip_session_prefix`].
fn session_dir_name(upload_id: &str) -> String {
    format!("{}{}", SESSION_DIR_PREFIX, upload_id)
}

/// Extract the upload_id from a session directory name. Returns
/// `None` when the directory wasn't created by this service (no
/// `oxi-chunk-` prefix) — the recovery and cleanup paths use this to
/// skip foreign directories cohabiting under `OXICLOUD_CHUNK_DIR`.
fn strip_session_prefix(dir_name: &str) -> Option<&str> {
    dir_name.strip_prefix(SESSION_DIR_PREFIX)
}

// ─── Serialisable types ──────────────────────────────────────────────────────

/// Chunk status
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChunkStatus {
    Pending,
    Uploading,
    Complete,
    Failed(String),
}

/// Individual chunk metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkInfo {
    pub index: usize,
    pub offset: u64,
    pub size: usize,
    pub status: ChunkStatus,
    pub checksum: Option<String>,
}

/// Upload session state — fully serialisable for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadSession {
    pub id: String,
    pub user_id: String,
    pub filename: String,
    pub folder_id: Option<String>,
    pub content_type: String,
    pub total_size: u64,
    pub chunk_size: usize,
    pub chunks: Vec<ChunkInfo>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub temp_dir: PathBuf,
    pub bytes_received: u64,
}

impl UploadSession {
    /// Calculate number of chunks needed
    pub fn calculate_chunk_count(total_size: u64, chunk_size: usize) -> usize {
        (total_size as usize).div_ceil(chunk_size).max(1)
    }

    /// Get upload progress (0.0 – 1.0)
    pub fn progress(&self) -> f64 {
        if self.total_size == 0 {
            return 1.0;
        }
        self.bytes_received as f64 / self.total_size as f64
    }

    /// Check if all chunks are complete
    pub fn is_complete(&self) -> bool {
        self.chunks
            .iter()
            .all(|c| c.status == ChunkStatus::Complete)
    }

    /// Get pending chunk indices
    pub fn pending_chunks(&self) -> Vec<usize> {
        self.chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| c.status == ChunkStatus::Pending)
            .map(|(i, _)| i)
            .collect()
    }

    /// Check if session has expired
    pub fn is_expired(&self) -> bool {
        let elapsed = Utc::now()
            .signed_duration_since(self.last_activity)
            .to_std()
            .unwrap_or(Duration::ZERO);
        elapsed > SESSION_EXPIRATION
    }

    // ── Persistence helpers ──────────────────────────────────────────────

    /// Build the completed-chunks bitmask (1 bit per chunk).
    fn build_progress_bitmask(&self) -> Vec<u8> {
        let len = self.chunks.len().div_ceil(8);
        let mut bitmask = vec![0u8; len];
        for chunk in &self.chunks {
            if chunk.status == ChunkStatus::Complete {
                bitmask[chunk.index / 8] |= 1 << (chunk.index % 8);
            }
        }
        bitmask
    }

    /// Apply a bitmask read from disk, marking matching chunks as `Complete`
    /// and recalculating `bytes_received`.
    fn apply_progress_bitmask(&mut self, bitmask: &[u8]) {
        self.bytes_received = 0;
        for chunk in &mut self.chunks {
            let byte_idx = chunk.index / 8;
            let bit_idx = chunk.index % 8;
            if byte_idx < bitmask.len() && (bitmask[byte_idx] & (1 << bit_idx)) != 0 {
                chunk.status = ChunkStatus::Complete;
                self.bytes_received += chunk.size as u64;
            }
        }
    }

    /// Persist the full session metadata once (on create).
    async fn persist_metadata(&self) -> Result<(), String> {
        let path = self.temp_dir.join(SESSION_META_FILE);
        let json =
            serde_json::to_vec(self).map_err(|e| format!("Failed to serialise session: {e}"))?;
        // Atomic write: write to .tmp then rename
        let tmp = self.temp_dir.join("session.json.tmp");
        fs::write(&tmp, &json)
            .await
            .map_err(|e| format!("Failed to write session metadata: {e}"))?;
        fs::rename(&tmp, &path)
            .await
            .map_err(|e| format!("Failed to rename session metadata: {e}"))?;
        Ok(())
    }

    /// Persist the lightweight progress bitmask (on each chunk upload).
    async fn persist_progress(&self) -> Result<(), String> {
        let bitmask = self.build_progress_bitmask();
        let path = self.temp_dir.join(PROGRESS_FILE);
        // Bitmask is tiny (< 512 B for up to 4 096 chunks).
        // A single write() of < 512 B is atomic on POSIX.
        fs::write(&path, &bitmask)
            .await
            .map_err(|e| format!("Failed to write progress bitmask: {e}"))?;
        Ok(())
    }
}

// ─── Service ─────────────────────────────────────────────────────────────────

/// Chunked Upload Service
///
/// Uses `DashMap` (sharded concurrent map) instead of a global `RwLock<HashMap>`
/// so that operations on independent upload sessions never contend with each
/// other.  Disk I/O (temp-dir cleanup) is always performed **outside** any
/// map lock to avoid blocking concurrent uploads.
pub struct ChunkedUploadService {
    sessions: Arc<DashMap<String, UploadSession>>,
    temp_base_dir: PathBuf,
}

/// What one cleanup pass did, so the job can report numbers instead of
/// leaving them in log lines nobody aggregates.
///
/// `failures` is the one that matters operationally: the failure this
/// cleanup guards is "the disk fills up over the weekend", and an
/// operator watching that happen had no way to ask how many sessions
/// were reaped or how many unlinks failed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanupReport {
    /// Sessions past their expiry window whose directory is now gone.
    pub sessions_expired: u64,
    /// Directories removed by the mtime walk — no live session, older
    /// than the window.
    pub orphan_dirs_removed: u64,
    /// Expired sessions whose directory could not be removed. Their map
    /// entry is retained on purpose, so the next pass retries.
    pub failures: u64,
}

impl ChunkedUploadService {
    /// Create the service, recover any persisted sessions, and start the
    /// background cleanup task.
    pub async fn new(temp_base_dir: PathBuf) -> Self {
        // Ensure the base directory exists
        let _ = fs::create_dir_all(&temp_base_dir).await;

        // One-shot migration of pre-prefix session directories to the
        // `oxi-chunk-{uuid}/` layout. Without this, any chunked upload
        // in flight at the moment an admin upgrades from a pre-prefix
        // build to this one would be orphaned — the recovery scan
        // filters strictly on the `oxi-chunk-` prefix and would skip
        // legacy `{uuid}/` directories. Idempotent: subsequent boots
        // find no legacy dirs left to rename and do nothing.
        Self::migrate_pre_prefix_sessions(&temp_base_dir).await;

        // Recover sessions that survived a restart
        let recovered = Self::recover_sessions(&temp_base_dir).await;
        let recovered_count = recovered.len();

        let service = Self {
            sessions: Arc::new(DashMap::from_iter(recovered)),
            temp_base_dir,
        };

        if recovered_count > 0 {
            tracing::info!("♻️  Recovered {recovered_count} chunked-upload session(s) from disk");
        }

        // No cleanup task is spawned here any more. It is the
        // `uploads_cleanup` job — see [`UploadsCleanup`]. An hourly
        // `tokio::spawn` had no admin trigger, no run history, no
        // findings and no way to answer "is it even running", which is
        // the same gap `backend_rechunk` was promoted out of.
        service
    }

    /// Hand the cleanup job what it needs to sweep: the live session map
    /// and the directory root.
    ///
    /// Shared `Arc`, not a copy — the job must see sessions created after
    /// it was registered, which is the entire point of the map being
    /// owned here.
    pub fn cleanup_handles(&self) -> (Arc<DashMap<String, UploadSession>>, PathBuf) {
        (self.sessions.clone(), self.temp_base_dir.clone())
    }

    /// Lightweight constructor that skips recovery and cleanup.
    /// Used only by `AppState::default()` (stub wiring).
    pub fn new_stub(temp_base_dir: PathBuf) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            temp_base_dir,
        }
    }

    // ── Upgrade migration ────────────────────────────────────────────────

    /// One-shot upgrade migration: rename any pre-prefix session
    /// directory to the new `oxi-chunk-{uuid}/` layout so the recovery
    /// scan picks it up.
    ///
    /// A pre-prefix session is identified by: a directory under
    /// `temp_base_dir` whose name does NOT start with
    /// `SESSION_DIR_PREFIX` but whose contents include `session.json`.
    /// That signature can only come from a chunked upload created by
    /// a pre-prefix OxiCloud build — admins don't normally drop
    /// session.json files into the chunk dir.
    ///
    /// Idempotent: on a fresh boot all dirs are already prefixed, the
    /// scan finds nothing to rename, no-op. Safe against concurrent
    /// boots: `fs::rename` is atomic, so a racing migration sees the
    /// source disappear and proceeds.
    async fn migrate_pre_prefix_sessions(base: &Path) {
        let mut entries = match fs::read_dir(base).await {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut migrated_count = 0usize;

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if dir_name.starts_with(SESSION_DIR_PREFIX) {
                continue; // already migrated
            }
            // Identify pre-prefix sessions by `session.json` presence.
            // Avoids touching the NC subtree (`nextcloud/` — no
            // session.json at that level) and any operator-placed
            // sibling directories without the marker.
            if !fs::try_exists(path.join(SESSION_META_FILE))
                .await
                .unwrap_or(false)
            {
                continue;
            }

            let new_path = base.join(session_dir_name(&dir_name));
            match fs::rename(&path, &new_path).await {
                Ok(()) => {
                    migrated_count += 1;
                    tracing::info!(
                        old = %path.display(),
                        new = %new_path.display(),
                        "Migrated pre-prefix chunked-upload session to new layout"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        old = %path.display(),
                        "Failed to migrate pre-prefix session — left orphaned on disk; \
                         next chunk PATCH from the client will 404 and the client should \
                         restart its upload session. Manual rm safe."
                    );
                }
            }
        }

        if migrated_count > 0 {
            tracing::info!(
                count = migrated_count,
                "🔧 Upgraded chunked-upload session layout (one-shot migration)"
            );
        }
    }

    // ── Recovery ─────────────────────────────────────────────────────────

    /// Scan `temp_base_dir` for directories containing `session.json`,
    /// deserialise each session, apply the `progress.bin` bitmask, and
    /// verify the chunk files on disk actually exist for completed chunks.
    async fn recover_sessions(base: &Path) -> HashMap<String, UploadSession> {
        let mut sessions = HashMap::new();
        let mut entries = match fs::read_dir(base).await {
            Ok(e) => e,
            Err(_) => return sessions,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            // Only consider directories WE created — anything without the
            // `oxi-chunk-` prefix belongs to a sibling writer (NC subtree,
            // PUT spool tempfiles, sysadmin-placed dirs) and must be left
            // strictly alone. See `SESSION_DIR_PREFIX`.
            let dir_name = match dir.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if strip_session_prefix(dir_name).is_none() {
                continue;
            }

            let meta_path = dir.join(SESSION_META_FILE);
            let meta_bytes = match fs::read(&meta_path).await {
                Ok(b) => b,
                Err(_) => continue, // no session.json → orphaned dir, skip
            };

            let mut session: UploadSession = match serde_json::from_slice(&meta_bytes) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Skipping corrupt session in {:?}: {e}", dir);
                    continue;
                }
            };

            // Apply progress bitmask if present
            if let Ok(bitmask) = fs::read(dir.join(PROGRESS_FILE)).await {
                session.apply_progress_bitmask(&bitmask);
            } else {
                // No progress file → all chunks still pending (freshly created)
                session.bytes_received = 0;
                for chunk in &mut session.chunks {
                    chunk.status = ChunkStatus::Pending;
                }
            }

            // Verify chunk files on disk — downgrade to Pending if missing
            for chunk in &mut session.chunks {
                if chunk.status == ChunkStatus::Complete {
                    let chunk_path = dir.join(format!("chunk_{:06}", chunk.index));
                    if !chunk_path.exists() {
                        tracing::warn!(
                            "Chunk {} missing on disk for session {}, marking pending",
                            chunk.index,
                            session.id
                        );
                        chunk.status = ChunkStatus::Pending;
                        session.bytes_received =
                            session.bytes_received.saturating_sub(chunk.size as u64);
                    }
                }
            }

            // Skip expired sessions
            if session.is_expired() {
                tracing::info!("Skipping expired recovered session: {}", session.id);
                let _ = fs::remove_dir_all(&dir).await;
                continue;
            }

            tracing::info!(
                "♻️  Recovered session {} — {}/{} chunks ({:.0}%)",
                session.id,
                session
                    .chunks
                    .iter()
                    .filter(|c| c.status == ChunkStatus::Complete)
                    .count(),
                session.chunks.len(),
                session.progress() * 100.0
            );

            sessions.insert(session.id.clone(), session);
        }

        sessions
    }

    // ── Cleanup ──────────────────────────────────────────────────────────

    /// One cleanup pass: expire sessions past their window, then sweep
    /// orphaned directories. Returns what it did, so a caller can report
    /// it instead of leaving the numbers in log lines nobody aggregates.
    ///
    /// Extracted from the old `cleanup_loop` — see
    /// [`UploadsCleanup`] for why a job replaced the detached task.
    pub(crate) async fn cleanup_once(
        sessions: &Arc<DashMap<String, UploadSession>>,
        temp_base_dir: &PathBuf,
    ) -> CleanupReport {
        let mut report = CleanupReport::default();

        // Collect expired session ids + temp dirs (lock-free iteration)
        let expired: Vec<(String, PathBuf)> = sessions
            .iter()
            .filter(|entry| entry.value().is_expired())
            .map(|entry| (entry.key().clone(), entry.value().temp_dir.clone()))
            .collect();

        // Backend first, record second — the inverse of what this used to
        // do, and the same rule `backend_reclaim` states.
        //
        // Dropping the map entry first meant a failed `remove_dir_all` was
        // unrecoverable by this pass: once the entry is gone, the
        // map-driven loop can never see that directory again. It was saved
        // only by the mtime walk below, which is a second mechanism and not
        // the one the code appeared to rely on.
        //
        // **Absent counts as done**: `NotFound` is success, not an error to
        // retain the entry for. Anything else keeps the entry so the next
        // pass retries it.
        //
        // Inverting is enough here *because the record is private*. The map
        // is internal, so nothing outside observes the entry surviving one
        // extra hour. `storage.blobs` could not be inverted — there, the
        // row's removal is what makes the deletion visible to the GC — so
        // the intent had to live in `storage.pending_actions` instead.
        for (id, temp_dir) in expired {
            match fs::remove_dir_all(&temp_dir).await {
                Ok(()) => {
                    sessions.remove(&id);
                    report.sessions_expired += 1;
                    tracing::info!("🧹 Cleaned expired upload session: {}", id);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    sessions.remove(&id);
                    report.sessions_expired += 1;
                }
                Err(e) => {
                    report.failures += 1;
                    tracing::warn!(
                        target: "audit",
                        event = "uploads_cleanup.unlink_failed",
                        reason = "temp_dir_unlink_failed",
                        upload_id = %id,
                        path = %temp_dir.display(),
                        error = %e,
                        "👮🏻‍♂️ could not remove an expired upload's temp dir; the \
                         session entry is retained so the next pass retries it: {e}"
                    );
                }
            }
        }

        report.orphan_dirs_removed = Self::sweep_orphan_dirs(sessions, temp_base_dir).await;
        report
    }

    /// The mtime-driven half: directories with no live session that are
    /// older than the expiry window.
    ///
    /// This is what saved the ordering bug above from being an active
    /// leak — it walks the filesystem rather than the map, so it can see
    /// a directory whose record is already gone. Kept as the backstop it
    /// is, now that the primary path no longer depends on it.
    async fn sweep_orphan_dirs(
        sessions: &Arc<DashMap<String, UploadSession>>,
        temp_base_dir: &PathBuf,
    ) -> u64 {
        let mut removed = 0u64;
        {
            // Also clean orphaned temp directories (no session.json or very old).
            // Filter strictly on the `oxi-chunk-` prefix so we never touch
            // sibling directories sharing `OXICLOUD_CHUNK_DIR` (NC subtree
            // `nextcloud/`, PUT spool tempfiles which are files anyway,
            // operator-placed dirs). Without the prefix filter this loop
            // would silently delete anything older than 24 h sitting at the
            // root of the chunked-upload dir.
            if let Ok(mut entries) = fs::read_dir(&temp_base_dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let upload_id = match strip_session_prefix(dir_name) {
                        Some(id) => id,
                        None => continue, // not ours — never touch
                    };

                    if !sessions.contains_key(upload_id)
                        && let Ok(metadata) = fs::metadata(&path).await
                        && let Ok(modified) = metadata.modified()
                        && modified.elapsed().unwrap_or_default() > SESSION_EXPIRATION
                    {
                        let _ = fs::remove_dir_all(&path).await;
                        removed += 1;
                        tracing::info!("🧹 Cleaned orphaned upload dir: {:?}", path);
                    }
                }
            }
        }
        removed
    }

    // ── Core operations ──────────────────────────────────────────────────

    /// Verify that the given session belongs to the given user.
    /// Returns 404 (not 403) to avoid revealing the existence of other users' sessions.
    fn verify_session_owner(&self, upload_id: &str, user_id: &str) -> Result<(), String> {
        let session = self
            .sessions
            .get(upload_id)
            .ok_or_else(|| format!("Upload session not found: {}", upload_id))?;
        if session.user_id != user_id {
            return Err(format!("Upload session not found: {}", upload_id));
        }
        Ok(())
    }

    /// Alloc-free owner compare for the per-chunk hot path: the caller's
    /// `Uuid` is stack-encoded (hyphenated, the format sessions store) —
    /// `prepare_chunk`/`commit_chunk` used to pay a `Uuid::to_string` each
    /// plus a dedicated `verify_session_owner` map lookup per chunk
    /// (benches/ROUND12.md §M5, 1.28x / −2 allocs per chunk).
    #[inline]
    fn owner_matches(session_user_id: &str, user_id: Uuid) -> bool {
        let mut buf = [0u8; 36];
        session_user_id == user_id.hyphenated().encode_lower(&mut buf) as &str
    }

    /// Create a new upload session (persists `session.json` + empty `progress.bin`)
    async fn create_session_inner(
        &self,
        user_id: String,
        filename: String,
        folder_id: Option<String>,
        content_type: String,
        total_size: u64,
        chunk_size: Option<usize>,
    ) -> Result<CreateUploadResponseDto, String> {
        let upload_id = Uuid::new_v4().to_string();
        let chunk_size = chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE);
        let chunk_count = UploadSession::calculate_chunk_count(total_size, chunk_size);

        // Create temp directory for chunks. The `oxi-chunk-` prefix
        // tags the directory as belonging to this service so the
        // shared-`OXICLOUD_CHUNK_DIR` story holds — see
        // `SESSION_DIR_PREFIX` for the full rationale.
        let temp_dir = self.temp_base_dir.join(session_dir_name(&upload_id));
        fs::create_dir_all(&temp_dir)
            .await
            .map_err(|e| format!("Failed to create temp directory: {e}"))?;

        // Initialise chunk metadata
        let mut chunks = Vec::with_capacity(chunk_count);
        let mut offset: u64 = 0;

        for i in 0..chunk_count {
            let size = if i == chunk_count - 1 {
                (total_size - offset) as usize
            } else {
                chunk_size
            };

            chunks.push(ChunkInfo {
                index: i,
                offset,
                size,
                status: ChunkStatus::Pending,
                checksum: None,
            });

            offset += size as u64;
        }

        let now = Utc::now();
        let session = UploadSession {
            id: upload_id.clone(),
            user_id,
            filename,
            folder_id,
            content_type,
            total_size,
            chunk_size,
            chunks,
            created_at: now,
            last_activity: now,
            temp_dir,
            bytes_received: 0,
        };

        let expires_at = SESSION_EXPIRATION.as_secs();

        // Persist metadata + empty progress to disk BEFORE inserting into RAM
        session.persist_metadata().await?;
        session.persist_progress().await?;

        self.sessions.insert(upload_id.clone(), session);

        tracing::info!(
            "📤 Created chunked upload session: {} ({} chunks, {} bytes each)",
            upload_id,
            chunk_count,
            chunk_size
        );

        Ok(CreateUploadResponseDto {
            upload_id,
            chunk_size,
            total_chunks: chunk_count,
            expires_at,
        })
    }

    /// Prepare a chunk write — validates session ownership and chunk
    /// index, returns the on-disk path the caller should stream the
    /// HTTP body to plus the expected byte count for that chunk.
    ///
    /// Used by the streaming REST PUT path: the handler calls
    /// `prepare_chunk` → streams body to disk via
    /// `interfaces::upload_ingest::stream_body_to_path` → calls
    /// `commit_chunk` to finalise. This lets the body bypass the
    /// in-memory `Bytes` allocation entirely (peak heap ~one HTTP
    /// frame instead of "chunk size").
    ///
    /// Returns `Err` if the session is unknown, owned by another user,
    /// the chunk index is out of range, or the chunk is already complete.
    pub async fn prepare_chunk(
        &self,
        upload_id: &str,
        user_id: Uuid,
        chunk_index: usize,
    ) -> Result<(PathBuf, usize), DomainError> {
        // Single map lookup: the owner gate rides the same guard (same
        // anti-enum not-found for unknown session and foreign session).
        let session = self.sessions.get(upload_id).ok_or_else(|| {
            DomainError::new(
                ErrorKind::NotFound,
                "ChunkedUpload",
                format!("Upload session not found: {}", upload_id),
            )
        })?;
        if !Self::owner_matches(&session.user_id, user_id) {
            return Err(DomainError::new(
                ErrorKind::NotFound,
                "ChunkedUpload",
                format!("Upload session not found: {}", upload_id),
            ));
        }

        if chunk_index >= session.chunks.len() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "ChunkedUpload",
                format!(
                    "Invalid chunk index: {} (max: {})",
                    chunk_index,
                    session.chunks.len() - 1
                ),
            ));
        }

        let chunk = &session.chunks[chunk_index];
        if chunk.status == ChunkStatus::Complete {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "ChunkedUpload",
                format!("Chunk {} already uploaded", chunk_index),
            ));
        }

        Ok((
            session.temp_dir.join(format!("chunk_{:06}", chunk_index)),
            chunk.size,
        ))
    }

    /// Finalise a chunk write — verifies the actually-written byte count
    /// matches the chunk's declared size, validates an optional
    /// algorithm-tagged checksum, and updates session state. The chunk
    /// file at `{session.temp_dir}/chunk_{index:06}` must already have
    /// been written by the caller (typically via
    /// `stream_body_to_path`).
    ///
    /// `actual_size` is the byte count the streaming write reported;
    /// `computed_checksum` is the hex digest computed during streaming
    /// (or `None` if the client didn't request a checksum). When
    /// `expected_checksum` is supplied the two are compared; a
    /// mismatch removes the partial file and returns `ValidationError`
    /// so a client retry against the same chunk index gets a clean
    /// slot. A size mismatch does the same.
    pub async fn commit_chunk(
        &self,
        upload_id: &str,
        user_id: Uuid,
        chunk_index: usize,
        actual_size: u64,
        computed_checksum: Option<String>,
        expected_checksum: Option<String>,
    ) -> Result<ChunkUploadResponseDto, DomainError> {
        // Owner gate folded into the metadata read below — one lookup
        // instead of two, same anti-enum not-found semantics.
        let (chunk_path, expected_size, persist_path) = {
            let session = self.sessions.get(upload_id).ok_or_else(|| {
                DomainError::new(
                    ErrorKind::NotFound,
                    "ChunkedUpload",
                    format!("Upload session not found: {}", upload_id),
                )
            })?;
            if !Self::owner_matches(&session.user_id, user_id) {
                return Err(DomainError::new(
                    ErrorKind::NotFound,
                    "ChunkedUpload",
                    format!("Upload session not found: {}", upload_id),
                ));
            }
            if chunk_index >= session.chunks.len() {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "ChunkedUpload",
                    format!("Invalid chunk index: {}", chunk_index),
                ));
            }
            (
                session.temp_dir.join(format!("chunk_{:06}", chunk_index)),
                session.chunks[chunk_index].size,
                session.temp_dir.join(PROGRESS_FILE),
            )
        };

        // Size check — the streaming body may have been truncated by
        // the client mid-flight or exceeded the chunk's declared
        // length. Either way we don't want a partial chunk to count
        // as complete; nuke it and ask the client to retry.
        if actual_size != expected_size as u64 {
            let _ = fs::remove_file(&chunk_path).await;
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "ChunkedUpload",
                format!(
                    "Invalid chunk size: expected {} bytes, got {} bytes",
                    expected_size, actual_size
                ),
            ));
        }

        // Checksum check — case-insensitive compare so clients that
        // send uppercase hex still match.
        if let Some(expected) = expected_checksum.as_ref()
            && let Some(actual) = computed_checksum.as_ref()
            && !expected.eq_ignore_ascii_case(actual)
        {
            let _ = fs::remove_file(&chunk_path).await;
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "ChunkedUpload",
                format!("Checksum mismatch: expected {}, got {}", expected, actual),
            ));
        }

        // Update session state — DashMap shard lock held only for the
        // RAM updates (~µs). The bitmask write happens AFTER the ref
        // is dropped so concurrent uploads to other sessions are never
        // blocked. Mirrors the legacy `upload_chunk_inner` semantics.
        let (bytes_received, progress, is_complete, persist_bitmask) = {
            let mut session = self.sessions.get_mut(upload_id).ok_or_else(|| {
                DomainError::new(
                    ErrorKind::NotFound,
                    "ChunkedUpload",
                    "Session disappeared".to_string(),
                )
            })?;
            session.chunks[chunk_index].status = ChunkStatus::Complete;
            session.chunks[chunk_index].checksum = expected_checksum;
            session.bytes_received += actual_size;
            session.last_activity = Utc::now();
            let bitmask = session.build_progress_bitmask();
            (
                session.bytes_received,
                session.progress(),
                session.is_complete(),
                bitmask,
            )
        };

        if let Err(e) = fs::write(&persist_path, &persist_bitmask).await {
            tracing::warn!("Failed to persist progress for {upload_id}: {e}");
        }

        tracing::debug!(
            "📦 Chunk {} committed for {} ({:.1}% complete)",
            chunk_index,
            upload_id,
            progress * 100.0
        );

        Ok(ChunkUploadResponseDto {
            chunk_index,
            bytes_received,
            progress,
            is_complete,
        })
    }

    /// Upload a single chunk (persists `progress.bin` after success)
    async fn upload_chunk_inner(
        &self,
        upload_id: &str,
        user_id: &str,
        chunk_index: usize,
        data: bytes::Bytes,
        checksum: Option<String>,
    ) -> Result<ChunkUploadResponseDto, String> {
        // Verify session exists AND belongs to the requesting user
        self.verify_session_owner(upload_id, user_id)?;

        // Validate chunk index is valid
        let (chunk_path, expected_size) = {
            let session = self
                .sessions
                .get(upload_id)
                .ok_or_else(|| format!("Upload session not found: {}", upload_id))?;

            if chunk_index >= session.chunks.len() {
                return Err(format!(
                    "Invalid chunk index: {} (max: {})",
                    chunk_index,
                    session.chunks.len() - 1
                ));
            }

            let chunk = &session.chunks[chunk_index];
            if chunk.status == ChunkStatus::Complete {
                return Err(format!("Chunk {} already uploaded", chunk_index));
            }

            (
                session.temp_dir.join(format!("chunk_{:06}", chunk_index)),
                chunk.size,
            )
        };

        // Validate chunk size
        if data.len() != expected_size {
            return Err(format!(
                "Invalid chunk size: expected {} bytes, got {} bytes",
                expected_size,
                data.len()
            ));
        }

        // Verify checksum if provided — MD5 is CPU-bound (~1.2 ms per 5 MB),
        // so we offload it to the blocking thread-pool to keep the Tokio
        // worker free for other connections.
        if let Some(ref expected_checksum) = checksum {
            let data_clone = data.clone(); // Bytes::clone is O(1) — just an Arc increment
            let actual_checksum = tokio::task::spawn_blocking(move || {
                use md5::{Digest, Md5};
                crate::common::fmt::hex_lower(&Md5::digest(&data_clone))
            })
            .await
            .map_err(|e| format!("MD5 checksum task failed: {e}"))?;

            if actual_checksum != *expected_checksum {
                return Err(format!(
                    "Checksum mismatch: expected {}, got {}",
                    expected_checksum, actual_checksum
                ));
            }
        }

        // Write chunk to temp file
        let mut file = File::create(&chunk_path)
            .await
            .map_err(|e| format!("Failed to create chunk file: {e}"))?;

        file.write_all(&data)
            .await
            .map_err(|e| format!("Failed to write chunk: {e}"))?;

        // Update session state — DashMap shard lock held only for RAM updates (~µs).
        // Disk I/O (persist_progress) is done AFTER the ref is dropped so
        // concurrent uploads to other sessions are never blocked.
        let (bytes_received, progress, is_complete, persist_path, persist_bitmask) = {
            let mut session = self
                .sessions
                .get_mut(upload_id)
                .ok_or_else(|| "Session disappeared".to_string())?;

            session.chunks[chunk_index].status = ChunkStatus::Complete;
            session.chunks[chunk_index].checksum = checksum;
            session.bytes_received += data.len() as u64;
            session.last_activity = Utc::now();

            // Build bitmask while under lock (CPU-only, ~microseconds)
            let bitmask = session.build_progress_bitmask();
            let path = session.temp_dir.join(PROGRESS_FILE);

            (
                session.bytes_received,
                session.progress(),
                session.is_complete(),
                path,
                bitmask,
            )
        }; // DashMap shard ref dropped here — held only for RAM updates (~µs)

        // Persist bitmask to disk OUTSIDE the lock — no longer blocks other uploads
        if let Err(e) = fs::write(&persist_path, &persist_bitmask).await {
            tracing::warn!("Failed to persist progress for {upload_id}: {e}");
        }

        tracing::debug!(
            "📦 Chunk {}/{} uploaded for {} ({:.1}% complete)",
            chunk_index + 1,
            expected_size,
            upload_id,
            progress * 100.0
        );

        Ok(ChunkUploadResponseDto {
            chunk_index,
            bytes_received,
            progress,
            is_complete,
        })
    }

    /// Get upload status
    async fn get_status_inner(
        &self,
        upload_id: &str,
        user_id: &str,
    ) -> Result<UploadStatusResponseDto, String> {
        self.verify_session_owner(upload_id, user_id)?;

        let session = self
            .sessions
            .get(upload_id)
            .ok_or_else(|| format!("Upload session not found: {}", upload_id))?;

        let completed_chunks = session
            .chunks
            .iter()
            .filter(|c| c.status == ChunkStatus::Complete)
            .count();

        Ok(UploadStatusResponseDto {
            upload_id: session.id.clone(),
            filename: session.filename.clone(),
            total_size: session.total_size,
            bytes_received: session.bytes_received,
            progress: session.progress(),
            total_chunks: session.chunks.len(),
            completed_chunks,
            pending_chunks: session.pending_chunks(),
            is_complete: session.is_complete(),
        })
    }

    /// Validate completion and return the chunk parts in assembly order.
    ///
    /// No assembled file is written and nothing is hashed here — the caller
    /// streams the parts straight into the CDC chunk store, which computes
    /// BLAKE3 and dedup-checks each CDC chunk in that single read pass. The
    /// part files stay on disk until `finalize_upload`, so a completion
    /// that fails downstream (e.g. client checksum mismatch) is retryable.
    async fn complete_upload_inner(
        &self,
        upload_id: &str,
        user_id: &str,
    ) -> Result<CompletedUploadParts, String> {
        // Verify ownership before completion
        self.verify_session_owner(upload_id, user_id)?;

        // Get session and validate completion.
        // Clone the session data and drop the DashMap ref immediately.
        let session = {
            let entry = self
                .sessions
                .get(upload_id)
                .ok_or_else(|| format!("Upload session not found: {}", upload_id))?;

            if !entry.is_complete() {
                let pending = entry.pending_chunks();
                return Err(format!(
                    "Upload not complete. Missing chunks: {:?}",
                    pending
                ));
            }

            entry.clone()
        };

        // Chunk indices are unique (upload_chunk rejects duplicates) but may
        // have arrived out of order — sort to recover the assembly order.
        let mut indices: Vec<usize> = session.chunks.iter().map(|c| c.index).collect();
        indices.sort_unstable();
        let chunk_paths: Vec<PathBuf> = indices
            .iter()
            .map(|index| session.temp_dir.join(format!("chunk_{:06}", index)))
            .collect();

        tracing::info!(
            "✅ Chunked upload complete: {} ({} bytes in {} chunks, streamed to CAS)",
            session.filename,
            session.total_size,
            chunk_paths.len()
        );

        Ok(CompletedUploadParts {
            chunk_paths,
            filename: session.filename.clone(),
            folder_id: session.folder_id.clone(),
            content_type: session.content_type.clone(),
            total_size: session.total_size,
        })
    }

    /// Finalize upload: remove session from RAM, then clean disk OUTSIDE lock.
    async fn finalize_upload_inner(&self, upload_id: &str, user_id: &str) -> Result<(), String> {
        self.verify_session_owner(upload_id, user_id)?;

        // Remove from map (~µs) — releases shard immediately
        let removed = self.sessions.remove(upload_id).map(|(_, s)| s);

        // Disk I/O happens with NO lock held
        if let Some(session) = removed
            && let Err(e) = fs::remove_dir_all(&session.temp_dir).await
        {
            tracing::warn!("Failed to cleanup upload {}: {}", upload_id, e);
        }
        Ok(())
    }

    /// Cancel an upload and cleanup — disk I/O outside lock.
    ///
    /// Returns:
    /// - `DomainError::NotFound` if no session matches `upload_id` for `user_id`
    ///   (covers both "session missing" and "owned by someone else" — same
    ///   error for anti-enumeration).
    /// - `DomainError::InternalError` for unexpected disk I/O failures.
    async fn cancel_upload_inner(&self, upload_id: &str, user_id: &str) -> Result<(), DomainError> {
        self.verify_session_owner(upload_id, user_id)
            .map_err(|_| DomainError::not_found("Upload", upload_id))?;

        // Remove from map (~µs)
        let removed = self.sessions.remove(upload_id).map(|(_, s)| s);

        // Disk I/O with NO lock held
        if let Some(session) = removed {
            if let Err(e) = fs::remove_dir_all(&session.temp_dir).await {
                tracing::warn!("Failed to cleanup cancelled upload {}: {}", upload_id, e);
            }
            tracing::info!("❌ Cancelled chunked upload: {}", upload_id);
        }
        Ok(())
    }

    /// Check if file size qualifies for chunked upload
    pub fn should_use_chunked(size: u64) -> bool {
        size as usize >= CHUNKED_UPLOAD_THRESHOLD
    }

    /// Get active session count (for monitoring)
    pub async fn active_sessions(&self) -> usize {
        self.sessions.len()
    }
}

// ─── Port implementation ─────────────────────────────────────────────────────

impl ChunkedUploadPort for ChunkedUploadService {
    async fn create_session(
        &self,
        user_id: Uuid,
        filename: String,
        folder_id: Option<String>,
        content_type: String,
        total_size: u64,
        chunk_size: Option<usize>,
    ) -> Result<CreateUploadResponseDto, DomainError> {
        self.create_session_inner(
            user_id.to_string(),
            filename,
            folder_id,
            content_type,
            total_size,
            chunk_size,
        )
        .await
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "ChunkedUpload", e))
    }

    async fn upload_chunk(
        &self,
        upload_id: &str,
        user_id: Uuid,
        chunk_index: usize,
        data: bytes::Bytes,
        checksum: Option<String>,
    ) -> Result<ChunkUploadResponseDto, DomainError> {
        self.upload_chunk_inner(upload_id, &user_id.to_string(), chunk_index, data, checksum)
            .await
            .map_err(|e| DomainError::new(ErrorKind::InternalError, "ChunkedUpload", e))
    }

    async fn get_status(
        &self,
        upload_id: &str,
        user_id: Uuid,
    ) -> Result<UploadStatusResponseDto, DomainError> {
        self.get_status_inner(upload_id, &user_id.to_string())
            .await
            .map_err(|e| DomainError::new(ErrorKind::NotFound, "ChunkedUpload", e))
    }

    async fn complete_upload(
        &self,
        upload_id: &str,
        user_id: Uuid,
    ) -> Result<CompletedUploadParts, DomainError> {
        self.complete_upload_inner(upload_id, &user_id.to_string())
            .await
            .map_err(|e| DomainError::new(ErrorKind::InternalError, "ChunkedUpload", e))
    }

    async fn finalize_upload(&self, upload_id: &str, user_id: Uuid) -> Result<(), DomainError> {
        self.finalize_upload_inner(upload_id, &user_id.to_string())
            .await
            .map_err(|e| DomainError::new(ErrorKind::InternalError, "ChunkedUpload", e))
    }

    async fn cancel_upload(&self, upload_id: &str, user_id: Uuid) -> Result<(), DomainError> {
        // Inner function now returns DomainError with proper variants
        // (NotFound for missing/wrong-owner sessions, InternalError otherwise),
        // so no mapping needed here.
        self.cancel_upload_inner(upload_id, &user_id.to_string())
            .await
    }

    fn should_use_chunked(&self, size: u64) -> bool {
        ChunkedUploadService::should_use_chunked(size)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A session whose `last_activity` is far enough in the past to be
    /// expired, pointing at `temp_dir`.
    fn expired_session(id: &str, temp_dir: PathBuf) -> UploadSession {
        UploadSession {
            id: id.to_string(),
            user_id: "u".into(),
            filename: "f.bin".into(),
            folder_id: None,
            content_type: "application/octet-stream".into(),
            total_size: 1,
            chunk_size: 1,
            chunks: Vec::new(),
            created_at: Utc::now(),
            last_activity: Utc::now()
                - chrono::Duration::seconds(SESSION_EXPIRATION.as_secs() as i64 + 60),
            temp_dir,
            bytes_received: 0,
        }
    }

    /// The happy path, and the baseline for the two below: an expired
    /// session's directory goes, and so does its map entry.
    #[tokio::test]
    async fn an_expired_session_is_removed_from_disk_and_from_the_map() {
        let base = std::env::temp_dir().join(format!("oxi-cleanup-ok-{}", Uuid::new_v4()));
        let dir = base.join("oxi-chunk-s1");
        fs::create_dir_all(&dir).await.unwrap();
        let sessions = Arc::new(DashMap::new());
        sessions.insert("s1".to_string(), expired_session("s1", dir.clone()));

        let report = ChunkedUploadService::cleanup_once(&sessions, &base).await;

        assert_eq!(report.sessions_expired, 1);
        assert_eq!(report.failures, 0);
        assert!(!dir.exists(), "temp dir survived");
        assert!(sessions.is_empty(), "map entry survived");
        let _ = fs::remove_dir_all(&base).await;
    }

    /// **Absent counts as done.** A directory that is already gone is
    /// success, not a failure to retry — the same rule
    /// `backend_reclaim` states for an object the backend no longer has.
    #[tokio::test]
    async fn a_missing_temp_dir_settles_the_session() {
        let base = std::env::temp_dir().join(format!("oxi-cleanup-gone-{}", Uuid::new_v4()));
        fs::create_dir_all(&base).await.unwrap();
        let sessions = Arc::new(DashMap::new());
        sessions.insert(
            "s2".to_string(),
            expired_session("s2", base.join("never-existed")),
        );

        let report = ChunkedUploadService::cleanup_once(&sessions, &base).await;

        assert_eq!(report.failures, 0, "NotFound must not count as a failure");
        assert_eq!(report.sessions_expired, 1);
        assert!(sessions.is_empty(), "a settled session must leave the map");
        let _ = fs::remove_dir_all(&base).await;
    }

    /// The fix this test exists for: a failed unlink must **keep** the
    /// map entry so the next pass retries it.
    ///
    /// Before, the entry was removed first and the unlink was
    /// best-effort — so a failure meant the map-driven pass could never
    /// see that directory again, and only the mtime walk could still
    /// find it. That walk is a backstop, not the mechanism this path
    /// should depend on.
    ///
    /// The failure is injected by pointing `temp_dir` at a regular
    /// **file**: `remove_dir_all` fails with `NotADirectory`, which is
    /// neither success nor `NotFound`. A permissions trick would have
    /// been the obvious alternative and is useless here — CI often runs
    /// as root, where `chmod` does not stop a removal.
    #[tokio::test]
    async fn a_failed_unlink_retains_the_session_for_the_next_pass() {
        let base = std::env::temp_dir().join(format!("oxi-cleanup-fail-{}", Uuid::new_v4()));
        fs::create_dir_all(&base).await.unwrap();
        let not_a_dir = base.join("oxi-chunk-s3");
        fs::write(&not_a_dir, b"this is a file").await.unwrap();

        let sessions = Arc::new(DashMap::new());
        sessions.insert("s3".to_string(), expired_session("s3", not_a_dir.clone()));

        let report = ChunkedUploadService::cleanup_once(&sessions, &base).await;

        assert_eq!(report.failures, 1, "the failure must be reported");
        assert_eq!(report.sessions_expired, 0);
        assert!(
            sessions.contains_key("s3"),
            "a failed unlink dropped the record anyway — the next pass can \
             never retry it"
        );
        let _ = fs::remove_dir_all(&base).await;
    }

    #[test]
    fn test_chunk_count_calculation() {
        assert_eq!(
            UploadSession::calculate_chunk_count(10 * 1024 * 1024, 5 * 1024 * 1024),
            2
        );
        assert_eq!(
            UploadSession::calculate_chunk_count(11 * 1024 * 1024, 5 * 1024 * 1024),
            3
        );
        assert_eq!(UploadSession::calculate_chunk_count(1, 5 * 1024 * 1024), 1);
        assert_eq!(UploadSession::calculate_chunk_count(0, 5 * 1024 * 1024), 1);
    }

    #[test]
    fn test_should_use_chunked() {
        assert!(!ChunkedUploadService::should_use_chunked(9 * 1024 * 1024));
        assert!(ChunkedUploadService::should_use_chunked(10 * 1024 * 1024));
        assert!(ChunkedUploadService::should_use_chunked(100 * 1024 * 1024));
    }

    #[test]
    fn test_bitmask_roundtrip() {
        let now = Utc::now();
        let mut session = UploadSession {
            id: "test-id".into(),
            user_id: "user-1".into(),
            filename: "file.bin".into(),
            folder_id: None,
            content_type: "application/octet-stream".into(),
            total_size: 15 * 1024 * 1024,
            chunk_size: 5 * 1024 * 1024,
            chunks: (0..3)
                .map(|i| ChunkInfo {
                    index: i,
                    offset: i as u64 * 5 * 1024 * 1024,
                    size: 5 * 1024 * 1024,
                    status: ChunkStatus::Pending,
                    checksum: None,
                })
                .collect(),
            created_at: now,
            last_activity: now,
            temp_dir: PathBuf::from("/tmp/test-id"),
            bytes_received: 0,
        };

        // Mark chunks 0 and 2 as complete
        session.chunks[0].status = ChunkStatus::Complete;
        session.chunks[2].status = ChunkStatus::Complete;
        session.bytes_received = 10 * 1024 * 1024;

        let bitmask = session.build_progress_bitmask();
        assert_eq!(bitmask, vec![0b00000101]); // bits 0 and 2 set

        // Reset and re-apply
        for chunk in &mut session.chunks {
            chunk.status = ChunkStatus::Pending;
        }
        session.bytes_received = 0;
        session.apply_progress_bitmask(&bitmask);

        assert_eq!(session.chunks[0].status, ChunkStatus::Complete);
        assert_eq!(session.chunks[1].status, ChunkStatus::Pending);
        assert_eq!(session.chunks[2].status, ChunkStatus::Complete);
        assert_eq!(session.bytes_received, 10 * 1024 * 1024);
    }

    #[test]
    fn test_session_serialisation_roundtrip() {
        let now = Utc::now();
        let session = UploadSession {
            id: "abc-123".into(),
            user_id: "user-1".into(),
            filename: "photo.jpg".into(),
            folder_id: Some("folder-1".into()),
            content_type: "image/jpeg".into(),
            total_size: 1024,
            chunk_size: 512,
            chunks: vec![
                ChunkInfo {
                    index: 0,
                    offset: 0,
                    size: 512,
                    status: ChunkStatus::Complete,
                    checksum: Some("aabb".into()),
                },
                ChunkInfo {
                    index: 1,
                    offset: 512,
                    size: 512,
                    status: ChunkStatus::Pending,
                    checksum: None,
                },
            ],
            created_at: now,
            last_activity: now,
            temp_dir: PathBuf::from("/tmp/abc-123"),
            bytes_received: 512,
        };

        let json = serde_json::to_vec(&session).expect("serialise");
        let restored: UploadSession = serde_json::from_slice(&json).expect("deserialise");

        assert_eq!(restored.id, session.id);
        assert_eq!(restored.user_id, session.user_id);
        assert_eq!(restored.filename, session.filename);
        assert_eq!(restored.folder_id, session.folder_id);
        assert_eq!(restored.total_size, session.total_size);
        assert_eq!(restored.chunks.len(), 2);
        assert_eq!(restored.chunks[0].status, ChunkStatus::Complete);
        assert_eq!(restored.chunks[1].status, ChunkStatus::Pending);
        assert_eq!(restored.bytes_received, 512);
    }

    #[test]
    fn test_session_expiry_check() {
        let mut session = UploadSession {
            id: "exp-test".into(),
            user_id: "user-1".into(),
            filename: "f".into(),
            folder_id: None,
            content_type: "x".into(),
            total_size: 0,
            chunk_size: 1,
            chunks: vec![],
            created_at: Utc::now(),
            last_activity: Utc::now(),
            temp_dir: PathBuf::from("/tmp"),
            bytes_received: 0,
        };

        assert!(!session.is_expired(), "Fresh session must not be expired");

        // Simulate old activity
        session.last_activity = Utc::now() - chrono::Duration::hours(25);
        assert!(session.is_expired(), "25h-old session must be expired");
    }

    #[tokio::test]
    async fn test_persist_and_recover_session() {
        // Use a unique temp dir for this test
        let base = std::env::temp_dir().join(format!("oxicloud_test_{}", Uuid::new_v4()));
        let _ = fs::create_dir_all(&base).await;

        let service = ChunkedUploadService::new(base.clone()).await;

        // Create a session
        let resp = service
            .create_session_inner(
                "test-user".into(),
                "bigfile.bin".into(),
                Some("folder-x".into()),
                "application/octet-stream".into(),
                10 * 1024 * 1024,
                Some(5 * 1024 * 1024),
            )
            .await
            .expect("create_session");

        let upload_id = resp.upload_id.clone();

        // Upload first chunk (5 MB of zeros)
        let chunk_data = bytes::Bytes::from(vec![0u8; 5 * 1024 * 1024]);
        service
            .upload_chunk_inner(&upload_id, "test-user", 0, chunk_data, None)
            .await
            .expect("upload_chunk 0");

        // Verify files exist on disk
        let session_dir = base.join(session_dir_name(&upload_id));
        assert!(session_dir.join(SESSION_META_FILE).exists());
        assert!(session_dir.join(PROGRESS_FILE).exists());
        assert!(session_dir.join("chunk_000000").exists());

        // Simulate restart: drop service, recover from disk
        drop(service);

        let recovered = ChunkedUploadService::recover_sessions(&base).await;
        assert_eq!(recovered.len(), 1);
        let session = recovered
            .get(&upload_id)
            .expect("session must be recovered");
        assert_eq!(session.filename, "bigfile.bin");
        assert_eq!(session.folder_id, Some("folder-x".into()));
        assert_eq!(session.chunks[0].status, ChunkStatus::Complete);
        assert_eq!(session.chunks[1].status, ChunkStatus::Pending);
        assert_eq!(session.bytes_received, 5 * 1024 * 1024);

        // Cleanup
        let _ = fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn test_full_upload_lifecycle() {
        let base = std::env::temp_dir().join(format!("oxicloud_test_{}", Uuid::new_v4()));
        let service = ChunkedUploadService::new(base.clone()).await;

        // 1. Create session (1024 bytes, 512 byte chunks → 2 chunks)
        let resp = service
            .create_session_inner(
                "test-user".into(),
                "test.txt".into(),
                None,
                "text/plain".into(),
                1024,
                Some(512),
            )
            .await
            .expect("create");

        assert_eq!(resp.total_chunks, 2);
        assert_eq!(resp.chunk_size, 512);
        let id = resp.upload_id;

        // 2. Upload chunks
        let chunk0 = bytes::Bytes::from(vec![b'A'; 512]);
        let r0 = service
            .upload_chunk_inner(&id, "test-user", 0, chunk0, None)
            .await
            .expect("chunk 0");
        assert!(!r0.is_complete);

        let chunk1 = bytes::Bytes::from(vec![b'B'; 512]);
        let r1 = service
            .upload_chunk_inner(&id, "test-user", 1, chunk1, None)
            .await
            .expect("chunk 1");
        assert!(r1.is_complete);
        assert_eq!(r1.bytes_received, 1024);

        // 3. Status check
        let status = service
            .get_status_inner(&id, "test-user")
            .await
            .expect("status");
        assert!(status.is_complete);
        assert_eq!(status.completed_chunks, 2);
        assert!(status.pending_chunks.is_empty());

        // 4. Complete — returns the ordered chunk parts, no assembled file
        let parts = service
            .complete_upload_inner(&id, "test-user")
            .await
            .expect("complete");
        assert_eq!(parts.filename, "test.txt");
        assert_eq!(parts.total_size, 1024);
        assert_eq!(parts.chunk_paths.len(), 2);

        // 5. Verify the parts concatenate to the original content in order
        let mut content = Vec::new();
        for path in &parts.chunk_paths {
            assert!(path.exists(), "chunk part must remain until finalize");
            content.extend_from_slice(&fs::read(path).await.expect("read part"));
        }
        assert_eq!(&content[..512], &[b'A'; 512]);
        assert_eq!(&content[512..], &[b'B'; 512]);

        // 6. Finalize
        service
            .finalize_upload_inner(&id, "test-user")
            .await
            .expect("finalize");
        assert_eq!(service.active_sessions().await, 0);

        let _ = fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn test_cancel_removes_files() {
        let base = std::env::temp_dir().join(format!("oxicloud_test_{}", Uuid::new_v4()));
        let service = ChunkedUploadService::new(base.clone()).await;

        let resp = service
            .create_session_inner(
                "test-user".into(),
                "x.bin".into(),
                None,
                "application/octet-stream".into(),
                512,
                Some(512),
            )
            .await
            .expect("create");

        let session_dir = base.join(session_dir_name(&resp.upload_id));
        assert!(session_dir.exists());

        service
            .cancel_upload_inner(&resp.upload_id, "test-user")
            .await
            .expect("cancel");

        assert!(!session_dir.exists(), "temp dir must be removed on cancel");
        assert_eq!(service.active_sessions().await, 0);

        let _ = fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn test_recovery_skips_expired_sessions() {
        let base = std::env::temp_dir().join(format!("oxicloud_test_{}", Uuid::new_v4()));
        let _ = fs::create_dir_all(&base).await;

        // Manually create an expired session on disk. The dir name MUST
        // carry the `oxi-chunk-` prefix or recovery will (correctly) skip
        // it as belonging to another writer co-located in chunk_dir.
        let session_dir = base.join(session_dir_name("expired-session"));
        let _ = fs::create_dir_all(&session_dir).await;

        let expired_session = UploadSession {
            id: "expired-session".into(),
            user_id: "user-1".into(),
            filename: "old.bin".into(),
            folder_id: None,
            content_type: "application/octet-stream".into(),
            total_size: 1024,
            chunk_size: 1024,
            chunks: vec![ChunkInfo {
                index: 0,
                offset: 0,
                size: 1024,
                status: ChunkStatus::Pending,
                checksum: None,
            }],
            created_at: Utc::now() - chrono::Duration::hours(48),
            last_activity: Utc::now() - chrono::Duration::hours(48),
            temp_dir: session_dir.clone(),
            bytes_received: 0,
        };

        let json = serde_json::to_vec(&expired_session).unwrap();
        fs::write(session_dir.join(SESSION_META_FILE), &json)
            .await
            .unwrap();

        let recovered = ChunkedUploadService::recover_sessions(&base).await;
        assert!(
            recovered.is_empty(),
            "Expired sessions must not be recovered"
        );

        let _ = fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn test_recovery_downgrades_missing_chunk_files() {
        let base = std::env::temp_dir().join(format!("oxicloud_test_{}", Uuid::new_v4()));
        let _ = fs::create_dir_all(&base).await;

        let session_dir = base.join(session_dir_name("partial-session"));
        let _ = fs::create_dir_all(&session_dir).await;

        let session = UploadSession {
            id: "partial-session".into(),
            user_id: "user-1".into(),
            filename: "file.bin".into(),
            folder_id: None,
            content_type: "application/octet-stream".into(),
            total_size: 1024,
            chunk_size: 512,
            chunks: vec![
                ChunkInfo {
                    index: 0,
                    offset: 0,
                    size: 512,
                    status: ChunkStatus::Pending,
                    checksum: None,
                },
                ChunkInfo {
                    index: 1,
                    offset: 512,
                    size: 512,
                    status: ChunkStatus::Pending,
                    checksum: None,
                },
            ],
            created_at: Utc::now(),
            last_activity: Utc::now(),
            temp_dir: session_dir.clone(),
            bytes_received: 0,
        };

        // Write metadata
        let json = serde_json::to_vec(&session).unwrap();
        fs::write(session_dir.join(SESSION_META_FILE), &json)
            .await
            .unwrap();

        // Write progress marking both chunks complete
        let bitmask = vec![0b00000011u8]; // bits 0 and 1
        fs::write(session_dir.join(PROGRESS_FILE), &bitmask)
            .await
            .unwrap();

        // But only create chunk_000000 on disk — chunk_000001 is "missing"
        fs::write(session_dir.join("chunk_000000"), &[0u8; 512])
            .await
            .unwrap();

        let recovered = ChunkedUploadService::recover_sessions(&base).await;
        let s = recovered.get("partial-session").expect("must be recovered");

        assert_eq!(s.chunks[0].status, ChunkStatus::Complete);
        assert_eq!(
            s.chunks[1].status,
            ChunkStatus::Pending,
            "Missing chunk file must be downgraded to Pending"
        );
        assert_eq!(s.bytes_received, 512);

        let _ = fs::remove_dir_all(&base).await;
    }

    /// Upgrade-path scenario: a pre-prefix session directory (the layout
    /// used by builds before the `oxi-chunk-` prefix change) gets renamed
    /// in place when the service starts, then recovered normally. Without
    /// the migration, the upgrade would orphan every in-flight REST
    /// chunked upload because recovery filters strictly on the prefix.
    #[tokio::test]
    async fn test_migrate_pre_prefix_session_on_boot() {
        let base = std::env::temp_dir().join(format!("oxicloud_test_{}", Uuid::new_v4()));
        let _ = fs::create_dir_all(&base).await;

        // Pre-upgrade layout: `{base}/legacy-upload-id/session.json`
        // (no `oxi-chunk-` prefix on the directory name).
        let legacy_id = "legacy-upload-id";
        let legacy_dir = base.join(legacy_id);
        let _ = fs::create_dir_all(&legacy_dir).await;

        let session = UploadSession {
            id: legacy_id.into(),
            user_id: "user-1".into(),
            filename: "in-flight.bin".into(),
            folder_id: None,
            content_type: "application/octet-stream".into(),
            total_size: 1024,
            chunk_size: 1024,
            chunks: vec![ChunkInfo {
                index: 0,
                offset: 0,
                size: 1024,
                status: ChunkStatus::Pending,
                checksum: None,
            }],
            created_at: Utc::now(),
            last_activity: Utc::now(),
            // `temp_dir` here is the legacy path — after migration the
            // session.json's path won't match the new location on disk,
            // but recovery doesn't use temp_dir for lookups; chunk
            // I/O within commit_chunk computes paths from the current
            // session dir, which is the renamed location.
            temp_dir: legacy_dir.clone(),
            bytes_received: 0,
        };
        fs::write(
            legacy_dir.join(SESSION_META_FILE),
            serde_json::to_vec(&session).unwrap(),
        )
        .await
        .unwrap();

        // Boot the service — migration runs as part of `new`.
        let svc = ChunkedUploadService::new(base.clone()).await;

        // Legacy path must be gone, prefixed path must exist + carry the
        // session.json.
        assert!(
            !legacy_dir.exists(),
            "Legacy un-prefixed dir should have been renamed away"
        );
        let new_dir = base.join(session_dir_name(legacy_id));
        assert!(
            new_dir.exists(),
            "Prefixed dir should exist post-migration at {}",
            new_dir.display()
        );
        assert!(
            new_dir.join(SESSION_META_FILE).exists(),
            "session.json must travel with the rename"
        );

        // Recovery picks it up — the session is now in the live map
        // keyed by its original upload_id.
        assert!(
            svc.sessions.contains_key(legacy_id),
            "Recovered session must be keyed by its original upload_id"
        );

        let _ = fs::remove_dir_all(&base).await;
    }

    /// Idempotency: running migration twice (a second boot after a
    /// successful migration) must be a no-op. The already-prefixed
    /// directory is left untouched, no spurious double-prefixing.
    #[tokio::test]
    async fn test_migration_is_idempotent() {
        let base = std::env::temp_dir().join(format!("oxicloud_test_{}", Uuid::new_v4()));
        let _ = fs::create_dir_all(&base).await;

        let upload_id = "already-prefixed";
        let prefixed_dir = base.join(session_dir_name(upload_id));
        let _ = fs::create_dir_all(&prefixed_dir).await;

        // Minimal session.json — just enough to count as a session for
        // the migration's identification heuristic.
        let session = UploadSession {
            id: upload_id.into(),
            user_id: "user-1".into(),
            filename: "x.bin".into(),
            folder_id: None,
            content_type: "application/octet-stream".into(),
            total_size: 1,
            chunk_size: 1,
            chunks: vec![ChunkInfo {
                index: 0,
                offset: 0,
                size: 1,
                status: ChunkStatus::Pending,
                checksum: None,
            }],
            created_at: Utc::now(),
            last_activity: Utc::now(),
            temp_dir: prefixed_dir.clone(),
            bytes_received: 0,
        };
        fs::write(
            prefixed_dir.join(SESSION_META_FILE),
            serde_json::to_vec(&session).unwrap(),
        )
        .await
        .unwrap();

        // Two migration runs in a row.
        ChunkedUploadService::migrate_pre_prefix_sessions(&base).await;
        ChunkedUploadService::migrate_pre_prefix_sessions(&base).await;

        // The dir is still there, with the SAME (single) prefix —
        // not `oxi-chunk-oxi-chunk-already-prefixed/`.
        assert!(prefixed_dir.exists());
        let double_prefixed = base.join(session_dir_name(&session_dir_name(upload_id)));
        assert!(
            !double_prefixed.exists(),
            "Migration must not double-prefix an already-prefixed dir"
        );

        let _ = fs::remove_dir_all(&base).await;
    }
}
