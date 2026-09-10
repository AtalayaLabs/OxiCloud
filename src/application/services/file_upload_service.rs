use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::file_lifecycle::FileLifecycleHook;
use crate::application::ports::file_ports::{FileUploadUseCase, StoredBlob};
use crate::application::ports::resource_access_hook::ResourceAccessHook;
use crate::application::ports::storage_ports::{FileReadPort, FileWritePort, StorageUsagePort};
use crate::application::services::storage_usage_service::StorageUsageService;
use crate::common::errors::DomainError;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::repositories::pg::FileBlobReadRepository;
use crate::infrastructure::repositories::pg::FileBlobWriteRepository;
use crate::infrastructure::services::dedup_service::DedupService;
use crate::infrastructure::services::file_content_cache::FileContentCache;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use tracing::{Instrument, info, warn};

/// Service for file upload operations.
///
/// Content never passes through this service: the interface layer streams
/// the request body straight into the CDC chunk store (no spool file, no
/// full-body buffering) and hands over a [`StoredBlob`] reference. This
/// service registers the metadata row, keeps caches coherent and fires
/// lifecycle hooks. Blob-reference ownership is consumed by the write
/// port, which releases it on failure — callers never compensate.
pub struct FileUploadService {
    /// Write port — registers file rows against ingested blobs.
    file_write: Arc<FileBlobWriteRepository>,
    /// Read port — needed for WebDAV/WOPI update-by-path.
    file_read: Option<Arc<FileBlobReadRepository>>,
    /// Optional storage usage tracking
    storage_usage_service: Option<Arc<StorageUsageService>>,
    /// Content cache — invalidated on file update so stale content is never served.
    content_cache: Option<Arc<FileContentCache>>,
    /// Single lifecycle dispatcher — fires on_file_created / on_file_updated.
    file_lifecycle_hook: Option<Arc<dyn FileLifecycleHook>>,
    /// Read-event hook — fires "caller just touched this file" so Recent
    /// records uploads / overwrites alongside reads. Distinct from
    /// `file_lifecycle_hook` because the lifecycle dispatcher only knows
    /// `(file_id, blob_hash, content_type)`; the recording side needs the
    /// `caller_id` the service already has in hand.
    resource_access_hook: Option<Arc<dyn ResourceAccessHook>>,
    /// ReBAC engine — enforces `Permission::Update` on
    /// overwrite-existing and `Permission::Create` on new-file paths
    /// inside `update_file_streaming_with_perms`. Optional at the
    /// struct level for the minimal test constructors (`new`,
    /// `new_with_read`) but the WebDAV/NC/WOPI put paths refuse
    /// (fail-closed internal error) if this isn't wired. Set by
    /// either `with_instant_upload` or `with_authorization` — both
    /// stash the same Arc so DI callers wiring instant upload get
    /// the streaming gate for free.
    authorization: Option<Arc<PgAclEngine>>,
    /// Dependencies of the instant-upload path
    /// (`create_file_from_owned_blob_with_perms`); `None` in minimal test
    /// wiring.
    instant_upload: Option<InstantUploadDeps>,
    /// Message bus. When wired, `upload_file_streaming`
    /// publishes a `FileCreated` event on `Topic::Folder(parent_id)`
    /// after the DB commit — subscribers see the new file appear in
    /// their live folder view. Optional so stub / test factories can
    /// build the service without a bus; a `None` bus is a silent no-op
    /// on the publish path.
    bus: Option<Arc<dyn crate::application::ports::message_bus_ports::MessageBus>>,
}

/// Everything the instant-upload path needs beyond the upload service's own
/// ports: permission checks, the dedup index, and quota enforcement.
struct InstantUploadDeps {
    authz: Arc<PgAclEngine>,
    dedup: Arc<DedupService>,
    quota: Arc<StorageUsageService>,
}

impl FileUploadService {
    /// Constructor with write port only (minimal).
    pub fn new(file_repository: Arc<FileBlobWriteRepository>) -> Self {
        Self {
            file_write: file_repository,
            file_read: None,
            storage_usage_service: None,
            content_cache: None,
            file_lifecycle_hook: None,
            resource_access_hook: None,
            authorization: None,
            instant_upload: None,
            bus: None,
        }
    }

    /// Constructor for blob-storage model: write + read ports.
    pub fn new_with_read(
        file_write: Arc<FileBlobWriteRepository>,
        file_read: Arc<FileBlobReadRepository>,
    ) -> Self {
        Self {
            file_write,
            file_read: Some(file_read),
            storage_usage_service: None,
            content_cache: None,
            file_lifecycle_hook: None,
            resource_access_hook: None,
            authorization: None,
            instant_upload: None,
            bus: None,
        }
    }

    /// Wires the authorization engine used by
    /// `update_file_streaming_with_perms` on the WebDAV / NC / WOPI
    /// PUT path. Independent of `with_instant_upload` so callers can
    /// enable the streaming gate without also opting into the
    /// dedup-instant-upload check (test wiring, minimal deployments).
    pub fn with_authorization(mut self, authz: Arc<PgAclEngine>) -> Self {
        self.authorization = Some(authz);
        self
    }

    /// Wire the message bus. Enables live folder-view updates:
    /// after `upload_file_streaming` commits, a `FileCreated` event
    /// fires on `Topic::Folder(parent_id)` — subscribers see the new
    /// file appear without polling.
    pub fn with_message_bus(
        mut self,
        bus: Arc<dyn crate::application::ports::message_bus_ports::MessageBus>,
    ) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Wires the authorization engine, dedup index and quota service that
    /// power the instant-upload path.
    ///
    /// Also stashes the `authz` handle in `self.authorization` so
    /// DI callers wiring instant upload get the streaming-put gate
    /// for free — a single `Arc` clone, no behavioural coupling.
    pub fn with_instant_upload(
        mut self,
        authz: Arc<PgAclEngine>,
        dedup: Arc<DedupService>,
        quota: Arc<StorageUsageService>,
    ) -> Self {
        self.authorization = Some(authz.clone());
        self.instant_upload = Some(InstantUploadDeps {
            authz,
            dedup,
            quota,
        });
        self
    }

    /// Configures the content cache for invalidation on file updates.
    pub fn with_content_cache(mut self, cache: Arc<FileContentCache>) -> Self {
        self.content_cache = Some(cache);
        self
    }

    /// Registers the lifecycle hook dispatcher (thumbnails, audio metadata, …).
    pub fn with_file_lifecycle_hook(mut self, hook: Arc<dyn FileLifecycleHook>) -> Self {
        self.file_lifecycle_hook = Some(hook);
        self
    }

    /// Registers the read/write access hook (Recent list recorder).
    pub fn with_resource_access_hook(mut self, hook: Arc<dyn ResourceAccessHook>) -> Self {
        self.resource_access_hook = Some(hook);
        self
    }

    /// Internal helper: fire the access hook if registered.
    fn notify_file_accessed(&self, caller_id: Uuid, file_id: &str) {
        if let Some(hook) = &self.resource_access_hook {
            hook.on_file_accessed(caller_id, file_id);
        }
    }

    /// Configures the storage usage service
    pub fn with_storage_usage_service(
        mut self,
        storage_usage_service: Arc<StorageUsageService>,
    ) -> Self {
        self.storage_usage_service = Some(storage_usage_service);
        self
    }

    // ── Instant upload (zero content bytes) ──────────────────────

    /// Register a new file row pointing at a blob the caller **already
    /// owns** — the instant-upload path: the client proved it has the
    /// content by hash, so no bytes travel and no chunk is written. Pure
    /// metadata: one ref_count bump + one row INSERT.
    ///
    /// Security model (mirrors `GET /api/dedup/check/{hash}`):
    /// - The caller must have `Create` permission on the target folder.
    /// - The hash is only claimable when the caller owns at least one
    ///   non-trashed file referencing it — never a global content oracle.
    ///   A non-owned hash returns `NotFound` (anti-enumeration: same shape
    ///   as "no such blob") and emits an `instant_upload.rejected` audit
    ///   event with the real reason.
    /// - Quota is enforced on the logical size, exactly like a byte upload.
    pub async fn create_file_from_owned_blob_with_perms(
        &self,
        caller_id: Uuid,
        name: String,
        folder_id: String,
        hash: &str,
    ) -> Result<FileDto, DomainError> {
        let Some(InstantUploadDeps {
            authz,
            dedup,
            quota,
        }) = &self.instant_upload
        else {
            return Err(DomainError::internal_error(
                "FileUpload",
                "instant upload is not wired (authz/dedup/quota missing)",
            ));
        };

        // ── AuthZ: Create on the target folder ───────────────────
        let folder_uuid = Uuid::parse_str(&folder_id)
            .map_err(|_| DomainError::not_found("Folder", folder_id.clone()))?;
        authz
            .require(
                Subject::User(caller_id),
                Permission::Create,
                Resource::Folder(folder_uuid),
            )
            .await?;

        // ── Ownership: only blobs the caller can already read ────
        if !dedup
            .user_owns_blob_reference(hash, &caller_id.to_string())
            .await
        {
            tracing::info!(
                target: "audit",
                event = "instant_upload.rejected",
                reason = "hash_not_owned",
                caller_id = %caller_id,
                blob_hash = %hash,
                "👮🏻‍♂️ Instant upload rejected: caller owns no file referencing the claimed hash",
            );
            return Err(DomainError::not_found("Blob", hash));
        }

        let Some(metadata) = dedup.get_blob_metadata(hash).await else {
            // Lost a race with the last-reference delete — same shape as
            // "never existed".
            return Err(DomainError::not_found("Blob", hash));
        };

        // ── Quota on the logical size, before taking any reference ──
        quota.check_storage_quota(caller_id, metadata.size).await?;

        // The manifest knows the original content type; fall back to the
        // new name's extension when the stored one is generic.
        let claimed = metadata.content_type.as_deref().unwrap_or("");
        let content_type =
            match crate::common::mime_detect::refine_content_type(&[], &name, claimed) {
                ct if ct.is_empty() => "application/octet-stream".to_string(),
                ct => ct,
            };

        // Take the reference the row registration will consume (it releases
        // it again on any failure). A concurrent GC between the ownership
        // check and this bump surfaces as NotFound — the client falls back
        // to a normal byte upload.
        dedup.add_reference(hash).await?;

        let dto = self
            .upload_file_streaming(
                name,
                Some(folder_id),
                content_type,
                StoredBlob {
                    hash: hash.to_string(),
                    size: metadata.size,
                    is_new_blob: false,
                },
                caller_id,
            )
            .await?;

        info!(
            "⚡ INSTANT UPLOAD: {} ({} bytes, 0 transferred, ID: {})",
            dto.name, metadata.size, dto.id
        );
        Ok(dto)
    }

    /// Swap an existing file's content to an already-ingested blob — the
    /// update mode of the delta-upload commit. The caller needs `Write`
    /// permission on the file; the blob reference is consumed (released on
    /// failure by the write port, like every other registration path).
    pub async fn update_file_content_by_id_with_perms(
        &self,
        caller_id: Uuid,
        file_id: &str,
        blob: StoredBlob,
    ) -> Result<FileDto, DomainError> {
        let Some(InstantUploadDeps { authz, .. }) = &self.instant_upload else {
            return Err(DomainError::internal_error(
                "FileUpload",
                "instant upload is not wired (authz/dedup/quota missing)",
            ));
        };
        let Some(file_read) = &self.file_read else {
            return Err(DomainError::internal_error(
                "FileUpload",
                "read port is not wired",
            ));
        };

        let file_uuid = Uuid::parse_str(file_id)
            .map_err(|_| DomainError::not_found("File", file_id.to_string()))?;
        authz
            .require(
                Subject::User(caller_id),
                Permission::Update,
                Resource::File(file_uuid),
            )
            .await?;

        let file = file_read.get_file(file_id).await?;
        let (new_hash, updated_at) = self
            .file_write
            .update_file_content_with_blob(file_id, &blob.hash, blob.size, None, caller_id, None)
            .await?;
        // The file maps to a different blob now — stale cached content must
        // never be served for the rest of its TTI window.
        if let Some(cc) = &self.content_cache {
            cc.invalidate(file_id).await;
        }

        let parts = file.into_parts();
        let updated = crate::domain::entities::file::File::with_timestamps_and_blob_hash(
            parts.id,
            parts.name,
            parts.storage_path,
            blob.size,
            parts.mime_type,
            parts.folder_id,
            parts.created_at,
            updated_at as u64,
            new_hash,
        )
        .map_err(|e| DomainError::internal_error("FileUpload", format!("rebuild entity: {e}")))?;
        let dto = FileDto::from(updated);
        if let Some(hook) = &self.file_lifecycle_hook {
            hook.on_file_updated(file_id, &dto.content_hash, &dto.mime_type);
        }
        // Delta-upload commit path — record the swap so Recent reflects
        // "this is the file I just delta-updated".
        self.notify_file_accessed(caller_id, file_id);
        Ok(dto)
    }

    // ── private helpers ──────────────────────────────────────────

    /// Bump the owner's cached storage usage after a successful upload.
    ///
    /// Incremental (`+size`, O(1)) and fire-and-forget on a background task, so
    /// it adds neither latency nor a `SUM(size)` over the user's whole library
    /// to the upload path (the previous full recompute was O(N) per upload,
    /// O(N²) for a bulk upload). Drift — e.g. deletes, which don't decrement —
    /// is reconciled by the periodic sweep.
    ///
    /// Post-D7: `file.owner_id` is now nullable and unpopulated on new
    /// rows, so the envelope owner comes from `caller_id` (the user who
    /// just did the upload). The user-side delta is guarded by
    /// `add_user_storage_usage_delta_if_personal` — it only fires when
    /// the target drive is `kind='personal'`, so a shared-drive upload
    /// still doesn't touch any user envelope.
    fn maybe_update_storage_usage(&self, file: &FileDto, caller_id: Uuid) {
        self.apply_storage_usage_delta(file.size as i64, &file.folder_id, caller_id);
    }

    /// Same as [`Self::maybe_update_storage_usage`] but takes an explicit
    /// `delta` instead of assuming "whole file size" — the overwrite path
    /// (`update_file_streaming_with_perms`) needs `new_size - old_size`,
    /// not the new size added a second time on top of what the old
    /// content already contributed.
    fn apply_storage_usage_delta(&self, delta: i64, folder_id: &Option<String>, caller_id: Uuid) {
        let Some(storage_service) = &self.storage_usage_service else {
            return;
        };
        if delta == 0 {
            return;
        }

        let owner = Some(caller_id);
        let folder = folder_id.as_deref().and_then(|s| Uuid::parse_str(s).ok());

        // Per-user delta — only when the target drive is `kind='personal'`.
        // The user envelope (`auth.users.storage_quota_bytes`) caps the SUM
        // of `used_bytes` across the user's personal drives; shared-drive
        // uploads do NOT count against any user. See
        // `docs/plan/drive.md` §7.
        //
        // The discrimination happens in one SQL statement via an EXISTS
        // subquery on the folder's drive kind — no extra round-trip vs
        // the unconditional delta. Without a folder id (root-level
        // upload — folder service refuses these) the user-side delta is
        // simply skipped; the sweep reconciles regardless.
        if let (Some(owner), Some(folder)) = (owner, folder) {
            let service_clone = Arc::clone(storage_service);
            tokio::spawn(
                async move {
                    if let Err(e) = service_clone
                        .add_user_storage_usage_delta_if_personal(owner, folder, delta)
                        .await
                    {
                        warn!("Failed to bump user storage for {owner} (folder {folder}): {e}");
                    }
                }
                .in_current_span(),
            );
        }

        // Per-drive delta (D4) — same fire-and-forget shape, resolves
        // the drive id from the file's parent folder in one SQL
        // statement. `storage.drives.used_bytes` is what the per-drive
        // quota check and the picker quota bar read; drift from
        // deletes / trash is reconciled by the same sweep that handles
        // user-side drift.
        if let Some(folder) = folder {
            let service_clone = Arc::clone(storage_service);
            tokio::spawn(
                async move {
                    if let Err(e) = service_clone
                        .add_drive_storage_usage_delta_by_folder(folder, delta)
                        .await
                    {
                        warn!("Failed to bump drive usage for folder {folder}: {e}");
                    }
                }
                .in_current_span(),
            );
        }
    }
}

impl FileUploadUseCase for FileUploadService {
    /// Register a new file row pointing at an already-ingested blob.
    async fn upload_file_streaming(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob: StoredBlob,
        caller_id: Uuid,
    ) -> Result<FileDto, DomainError> {
        let file = self
            .file_write
            .save_file_with_blob(
                name.clone(),
                folder_id,
                content_type,
                &blob.hash,
                blob.size,
                caller_id,
            )
            .await?;
        let dto = FileDto::from(file);
        info!(
            "📡 STREAMING UPLOAD: {} ({} bytes, ID: {})",
            name, blob.size, dto.id
        );
        self.maybe_update_storage_usage(&dto, caller_id);
        if let Some(hook) = &self.file_lifecycle_hook {
            hook.on_file_created(&dto.id, &dto.content_hash, &dto.mime_type, blob.is_new_blob);
        }
        // The caller just created this file — surface it in Recent so the
        // "I just uploaded X" UX matches the pre-SvelteKit behaviour.
        self.notify_file_accessed(caller_id, &dto.id);

        // Bus fan-out AFTER commit — subscribers to the parent
        // folder's topic see the new file appear live. Silent no-op if
        // the bus isn't wired (stubs / tests) or the file landed at
        // drive-root (no folder id → nothing to publish on).
        if let (Some(bus), Some(parent_folder_id)) = (&self.bus, dto.folder_id.as_deref())
            && let (Ok(parent_uuid), Ok(file_uuid)) =
                (Uuid::parse_str(parent_folder_id), Uuid::parse_str(&dto.id))
        {
            use crate::application::ports::message_bus_ports::{MessageBusEvent, Topic};
            bus.publish(
                &Topic::Folder(parent_uuid),
                MessageBusEvent::FileCreated {
                    file_id: file_uuid,
                    name: dto.name.clone(),
                    parent_id: parent_uuid,
                    actor: caller_id,
                },
            );
        }

        Ok(dto)
    }

    /// AuthZ audit #17 — `Create` on target folder is re-verified here
    /// so mid-session grant revocations take effect at finalize. When
    /// `folder_id` is `None` the write lands at drive-root; the drive
    /// resolution for that case isn't plumbed through the chunked-
    /// upload session (`UploadSession.folder_id` alone), so we fall
    /// back to the pre-audit behaviour there. That drive-root path is
    /// tracked separately as part of the D0 folder-id-walking work;
    /// closing it here would require session-scoped drive_id.
    async fn upload_file_streaming_with_perms(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob: StoredBlob,
        caller_id: Uuid,
    ) -> Result<FileDto, DomainError> {
        if let Some(fid) = folder_id.as_deref() {
            let Some(authz) = &self.authorization else {
                return Err(DomainError::internal_error(
                    "FileUpload",
                    "upload_file_streaming_with_perms called without authorization engine wired",
                ));
            };
            let folder_uuid = Uuid::parse_str(fid)
                .map_err(|_| DomainError::not_found("Folder", fid.to_string()))?;
            authz
                .require(
                    Subject::User(caller_id),
                    Permission::Create,
                    Resource::Folder(folder_uuid),
                )
                .await?;
        }

        self.upload_file_streaming(name, folder_id, content_type, blob, caller_id)
            .await
    }

    /// Swap the content of the file at `path` to an already-ingested blob,
    /// creating the file when it doesn't exist (WebDAV/NextCloud/WOPI PUT).
    ///
    /// AuthZ (post-Drive audit Round 2 fix): overwrite path requires
    /// `Update` on the target file; new-file path requires `Create`
    /// on the parent folder (or on the drive when writing at drive
    /// root). Fail-closed if the engine wasn't wired — this method
    /// is the last line of defence between a Viewer/Commenter drive
    /// member and cross-tenant PUT. See
    /// `docs/plan/authz_audit/nextcloud.md` and the sibling native
    /// `/webdav/*` handler.
    #[allow(clippy::too_many_arguments)]
    async fn update_file_streaming_with_perms(
        &self,
        path: &str,
        drive_id: Uuid,
        blob: StoredBlob,
        content_type: &str,
        modified_at: Option<i64>,
        caller_id: Uuid,
        expected_hash: Option<&str>,
    ) -> Result<FileDto, DomainError> {
        let Some(authz) = &self.authorization else {
            return Err(DomainError::internal_error(
                "FileUpload",
                "update_file_streaming_with_perms called without authorization engine wired",
            ));
        };

        // Try to find the existing file first
        if let Some(file_read) = &self.file_read
            && let Some(file) = file_read.find_file_by_path(path, drive_id).await?
        {
            // Overwrite branch — caller must have `Update` on the
            // target file. Denial routes through `require` → 404
            // (anti-enum, matches read-side shape). Before the D7
            // audit this whole branch ran unchecked; Viewer members
            // of shared drives could PUT freely.
            let file_uuid = Uuid::parse_str(file.id()).map_err(|_| {
                DomainError::internal_error("FileUpload", "invalid file id from repository")
            })?;
            authz
                .require(
                    Subject::User(caller_id),
                    Permission::Update,
                    Resource::File(file_uuid),
                )
                .await?;

            let old_size = file.size();
            let file_id = file.id().to_string();
            let (new_hash, updated_at) = self
                .file_write
                .update_file_content_with_blob(
                    &file_id,
                    &blob.hash,
                    blob.size,
                    modified_at,
                    caller_id,
                    expected_hash,
                )
                .await?;
            // Invalidate content cache — file content has changed.
            if let Some(cc) = &self.content_cache {
                cc.invalidate(&file_id).await;
            }
            // Rebuild the fresh DTO from the entity already in hand plus the
            // values the UPDATE just returned — a re-read would only fetch
            // what we already know, at one extra round-trip per overwrite
            // (WebDAV sync clients overwrite constantly).
            let parts = file.into_parts();
            let updated = crate::domain::entities::file::File::with_timestamps_and_blob_hash(
                parts.id,
                parts.name,
                parts.storage_path,
                blob.size,
                parts.mime_type,
                parts.folder_id,
                parts.created_at,
                updated_at as u64,
                new_hash,
            )
            .map_err(|e| {
                DomainError::internal_error("FileUpload", format!("rebuild entity: {e}"))
            })?;
            let dto = FileDto::from(updated);
            self.apply_storage_usage_delta(
                blob.size as i64 - old_size as i64,
                &dto.folder_id,
                caller_id,
            );
            if let Some(hook) = &self.file_lifecycle_hook {
                hook.on_file_updated(&file_id, &dto.content_hash, content_type);
            }
            self.notify_file_accessed(caller_id, &file_id);
            return Ok(dto);
        }

        // File doesn't exist — create it via streaming upload
        let path_normalized = path.trim_start_matches('/').trim_end_matches('/');
        let (_, filename) = if let Some(idx) = path_normalized.rfind('/') {
            (&path_normalized[..idx], &path_normalized[idx + 1..])
        } else {
            ("", path_normalized)
        };

        // get_parent_folder_id expects the full file path — it strips the
        // last segment (filename) internally to find the parent folder.
        // `drive_id` scopes the parent lookup to the same drive as the
        // incoming write (post-D0 `storage.folders.path` repeats across
        // drives).
        let parent_id = if path_normalized.contains('/') {
            if let Some(file_read) = &self.file_read {
                file_read
                    .get_parent_folder_id(path_normalized, drive_id)
                    .await
                    .ok()
            } else {
                None
            }
        } else {
            None
        };

        // Create branch — caller must have `Create` on the parent
        // scope. Two cases:
        //   * `parent_id.is_some()` → caller needs Create on the
        //     parent Folder resource.
        //   * `parent_id.is_none()` → the write lands at the drive
        //     root (either the path was single-segment, or the
        //     parent-folder lookup failed). We require Create on
        //     the Drive itself — bundled with owner/editor/contributor
        //     role_grants, refused for viewer/commenter.
        let create_resource = match &parent_id {
            Some(pid) => {
                let uuid = Uuid::parse_str(pid).map_err(|_| {
                    DomainError::internal_error("FileUpload", "invalid parent folder id")
                })?;
                Resource::Folder(uuid)
            }
            None => Resource::Drive(drive_id),
        };
        authz
            .require(
                Subject::User(caller_id),
                Permission::Create,
                create_resource,
            )
            .await?;

        let is_new_blob = blob.is_new_blob;
        let created = self
            .file_write
            .save_file_with_blob(
                filename.to_string(),
                parent_id,
                content_type.to_string(),
                &blob.hash,
                blob.size,
                caller_id,
            )
            .await?;
        let dto = FileDto::from(created);
        self.maybe_update_storage_usage(&dto, caller_id);
        if let Some(hook) = &self.file_lifecycle_hook {
            hook.on_file_created(&dto.id, &dto.content_hash, content_type, is_new_blob);
        }
        self.notify_file_accessed(caller_id, &dto.id);
        Ok(dto)
    }
}
