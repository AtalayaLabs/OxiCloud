use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::blob_storage_ports::BlobStream;
use crate::application::ports::external_mount_ports::MountStat;
use crate::application::ports::file_ports::{
    FileRetrievalUseCase, OptimizedFileContent, RangeContent,
};
use crate::application::ports::resource_access_hook::ResourceAccessHook;
use crate::application::ports::storage_ports::FileReadPort;
use crate::application::services::mount_registry::MountConfig;
use crate::common::errors::DomainError;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::domain::services::external_mount_id::NodeId;
use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
use crate::infrastructure::services::file_content_cache::FileContentCache;
use crate::infrastructure::services::image_transcode_service::{
    ImageTranscodeService, OutputFormat,
};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use tracing::{debug, info};
use uuid::Uuid;

/// Threshold below which files are served from RAM cache (10 MB).
const CACHE_THRESHOLD: u64 = 10 * 1024 * 1024;

/// Service for file retrieval operations
///
/// Implements a multi-tier download strategy:
/// - Tier 0: Write-behind cache (just-uploaded files still in RAM)
/// - Tier 1: Hot cache + optional WebP transcoding (<10 MB)
/// - Tier 2: Streaming for everything ≥10 MB — CDC chunk reassembly with the
///   backend's read-ahead (`read_prefetch`); no whole-file buffering.
pub struct FileRetrievalService {
    file_read: Arc<FileBlobReadRepository>,
    content_cache: Option<Arc<FileContentCache>>,
    transcode: Option<Arc<ImageTranscodeService>>,
    authz: Option<Arc<PgAclEngine>>,
    /// External-mount classifier for path-based resolution (WebDAV/NextCloud).
    /// `None` in the simple/test constructor → no mount support.
    mount_router: Option<Arc<crate::application::services::external_mount_router::MountRouter>>,
    /// Optional read-event observer. Currently fans out to the Recent-list
    /// recorder; future observers (audit trail, "last seen by", …) attach
    /// to the same hook so service code only knows the trait, not the impl.
    /// `None` for the test/stub path that constructs via [`Self::new`].
    resource_access_hook: Option<Arc<dyn ResourceAccessHook>>,
}

impl FileRetrievalService {
    /// Backward-compatible constructor (simple pass-through). Without the
    /// authorization engine, the `*_owned`/`*_with_perms` methods fail closed.
    /// Use `new_with_cache` in production.
    pub fn new(file_repository: Arc<FileBlobReadRepository>) -> Self {
        Self {
            file_read: file_repository,
            content_cache: None,
            transcode: None,
            authz: None,
            mount_router: None,
            resource_access_hook: None,
        }
    }

    /// Constructor for blob-storage model: read + content cache + transcode +
    /// ReBAC authorization.
    pub fn new_with_cache(
        file_read: Arc<FileBlobReadRepository>,
        content_cache: Arc<FileContentCache>,
        transcode: Arc<ImageTranscodeService>,
        authz: Arc<PgAclEngine>,
    ) -> Self {
        Self {
            file_read,
            content_cache: Some(content_cache),
            transcode: Some(transcode),
            authz: Some(authz),
            mount_router: None,
            resource_access_hook: None,
        }
    }

    /// Injects the external-mount classifier so path-based lookups
    /// (`get_file_by_path`) can resolve mount paths to the provider.
    pub fn with_mount_router(
        mut self,
        router: Arc<crate::application::services::external_mount_router::MountRouter>,
    ) -> Self {
        self.mount_router = Some(router);
        self
    }

    /// Builder: attach a [`ResourceAccessHook`] that fires after every
    /// authorised `_with_perms` read. Without it the service is silent —
    /// existing behaviour for stub / test paths.
    pub fn with_resource_access_hook(mut self, hook: Arc<dyn ResourceAccessHook>) -> Self {
        self.resource_access_hook = Some(hook);
        self
    }

    /// Test-only constructor: authorization engine without the cache/transcode
    /// tiers. The external-mount read methods only consult `authz` + the
    /// provider, so this is sufficient to exercise their authorization.
    #[cfg(all(test, integration_tests))]
    pub(crate) fn new_with_authz_for_test(
        file_read: Arc<FileBlobReadRepository>,
        authz: Arc<PgAclEngine>,
    ) -> Self {
        Self {
            file_read,
            content_cache: None,
            transcode: None,
            authz: Some(authz),
            mount_router: None,
            resource_access_hook: None,
        }
    }

    /// Fire the access hook if registered. Called from every `_with_perms`
    /// read after the authZ + lookup has succeeded (never on failure
    /// paths — denied reads must not surface in Recent).
    ///
    /// `pub` because the WebDAV / NextCloud DAV handlers resolve files
    /// by path and authorise via that resolver, not via the
    /// `*_with_perms` service methods — they then serve content through
    /// the no-perms `get_file_stream` / `get_file_range_stream`. Those
    /// handlers must call this directly after their own authZ has
    /// passed so cross-protocol downloads (NC desktop, davx5, native
    /// `/webdav/`) also surface in Recent.
    pub fn notify_file_accessed(&self, caller_id: Uuid, file_id: &str) {
        if let Some(hook) = &self.resource_access_hook {
            hook.on_file_accessed(caller_id, file_id);
        }
    }

    // ── private helpers ──────────────────────────────────────────

    /// Read a file's full content through the streaming API into a single
    /// `Bytes` buffer. Working memory stays at one chunk while reading; the
    /// returned buffer holds the whole (sub-threshold) file.
    async fn read_full(
        file_read: &FileBlobReadRepository,
        id: &str,
        capacity: usize,
    ) -> Result<Bytes, DomainError> {
        let stream = file_read.get_file_stream(id).await?;
        let mut stream = Pin::from(stream);
        // Most sub-threshold reads arrive as ONE owned contiguous frame from the
        // backend (the local ReaderStream emits ≤256 KiB frames, and a
        // sub-threshold blob fits in one). Return that frame directly instead of
        // copying the whole payload a second time into a fresh BytesMut; only a
        // multi-frame read pays the pre-sized concat — byte-identical output
        // (benches/ROUND29.md §C).
        let Some(first) = stream.next().await else {
            return Ok(Bytes::new());
        };
        let first = first.map_err(|e| {
            DomainError::internal_error("File", format!("Stream read error: {}", e))
        })?;
        let Some(second) = stream.next().await else {
            return Ok(first);
        };
        let mut buf = BytesMut::with_capacity(capacity.max(first.len()));
        buf.extend_from_slice(&first);
        buf.extend_from_slice(&second.map_err(|e| {
            DomainError::internal_error("File", format!("Stream read error: {}", e))
        })?);
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.map_err(|e| {
                DomainError::internal_error("File", format!("Stream read error: {}", e))
            })?);
        }
        Ok(buf.freeze())
    }

    /// Helper: require the caller has `perm` on the given file id.
    /// Fail-closed if no engine was injected (stub/test path).
    async fn require_file(
        &self,
        file_id: &str,
        perm: Permission,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        let authz = self.authz.as_ref().ok_or_else(|| {
            DomainError::internal_error("FileRetrieval", "Authorization engine unavailable")
        })?;
        let uuid = Uuid::parse_str(file_id).map_err(|_| DomainError::not_found("File", file_id))?;
        authz
            .require(Subject::User(caller_id), perm, Resource::File(uuid))
            .await
    }

    /// Engine check for a target folder. `None` is allowed (root namespace,
    /// implicitly owned by the caller).
    async fn require_target_folder_perm(
        &self,
        folder_id: Option<&str>,
        perm: Permission,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        let Some(target) = folder_id else {
            return Ok(());
        };
        let authz = self.authz.as_ref().ok_or_else(|| {
            DomainError::internal_error("FileRetrieval", "Authorization engine unavailable")
        })?;
        let uuid = Uuid::parse_str(target).map_err(|_| DomainError::not_found("Folder", target))?;
        authz
            .require(Subject::User(caller_id), perm, Resource::Folder(uuid))
            .await
    }

    /// Authorize then `stat` a file inside an external mount. Authorization
    /// collapses onto the mount-root folder (a `Read` grant there covers
    /// everything in the mount).
    pub async fn stat_mount_file_with_perms(
        &self,
        cfg: &MountConfig,
        node_id: &NodeId,
        caller_id: Uuid,
    ) -> Result<MountStat, DomainError> {
        let authz = self.authz.as_ref().ok_or_else(|| {
            DomainError::internal_error("FileRetrieval", "Authorization engine unavailable")
        })?;
        authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Resource::Folder(cfg.mount_id),
            )
            .await?;
        cfg.provider.stat(node_id).await
    }

    /// Authorize then open a (optionally ranged) read stream over a mount file.
    /// `range` is `(start, end_inclusive_opt)`.
    pub async fn open_mount_file_with_perms(
        &self,
        cfg: &MountConfig,
        node_id: &NodeId,
        caller_id: Uuid,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<BlobStream, DomainError> {
        let authz = self.authz.as_ref().ok_or_else(|| {
            DomainError::internal_error("FileRetrieval", "Authorization engine unavailable")
        })?;
        authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Resource::Folder(cfg.mount_id),
            )
            .await?;
        cfg.provider.open_read_stream(node_id, range).await
    }

    /// If `id` is an `ext:` mount FILE id, return the mount config + node id.
    /// `None` for native ids, mount roots, or when no router is wired.
    fn mount_file_node(
        &self,
        id: &str,
    ) -> Option<(
        Arc<crate::application::services::mount_registry::MountConfig>,
        NodeId,
    )> {
        use crate::application::services::external_mount_router::ResolvedId;
        match self.mount_router.as_ref()?.classify(id) {
            ResolvedId::MountChild { cfg, node_id } => Some((cfg, node_id)),
            _ => None,
        }
    }

    /// Try to transcode image content to WebP and return transcoded variant.
    ///
    /// `source_hash` keys the durable derived tier. The caller has it as
    /// `dto.content_hash`; passing it rather than hashing here matters,
    /// because hashing would be a BLAKE3 over the whole file on every
    /// request that reaches this path.
    async fn try_transcode(
        &self,
        id: &str,
        source_hash: Option<&str>,
        content: &Bytes,
        mime: &str,
        file_size: u64,
        accept_webp: bool,
    ) -> Option<(Bytes, Arc<str>)> {
        if !accept_webp {
            return None;
        }
        let transcode = self.transcode.as_ref()?;
        if !ImageTranscodeService::should_transcode(mime, file_size) {
            return None;
        }
        let format = OutputFormat::WebP;
        match transcode
            .get_transcoded(id, source_hash, content.clone(), mime, format)
            .await
        {
            Ok((transcoded, webp_mime, true)) => {
                debug!(
                    "🖼️ WebP transcode: {} -> {} bytes ({:.0}% smaller)",
                    content.len(),
                    transcoded.len(),
                    (1.0 - transcoded.len() as f64 / content.len().max(1) as f64) * 100.0
                );
                Some((transcoded, Arc::from(&*webp_mime)))
            }
            _ => None,
        }
    }

    /// Core multi-tier download logic shared by `get_file_optimized` and
    /// `get_file_optimized_preloaded`.
    async fn optimized_inner(
        &self,
        id: &str,
        dto: FileDto,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        let mime_type = dto.mime_type.clone();
        let file_size = dto.size;
        // The content cache is content-addressed: keyed by the blob hash, not
        // the file id. Identical content deduplicated to one blob on disk is
        // then cached ONCE in RAM and shared by every file/user that references
        // it — the cache benefits from dedup, not just the disk. Immutable by
        // construction, so entries never go stale (no invalidation needed). A
        // stub DTO without a hash disables caching for that request rather than
        // colliding every hash-less file on the key "".
        let cacheable = !dto.content_hash.is_empty();
        let do_transcode = accept_webp && !prefer_original;

        // ── Tier 1: Hot cache + transcode (<10 MB) ──────────
        if file_size < CACHE_THRESHOLD {
            // Probe the content cache with a BORROW first: a hit serves the blob
            // straight from RAM, and only a miss builds the owned load arguments
            // (the quoted-etag / key / id Strings) that a hit would otherwise
            // allocate and immediately discard (benches/ROUND29.md §B). On a miss
            // `load_and_cache` still coalesces concurrent requests for the same
            // blob hash into a SINGLE disk read (single-flight) — no thundering
            // herd. Hash-less stub DTOs are uncacheable and stream from disk.
            let content_bytes = if cacheable && let Some(cache) = &self.content_cache {
                if let Some((bytes, ..)) = cache.get(&dto.content_hash).await {
                    bytes
                } else {
                    let etag: Arc<str> = format!("\"{}\"", dto.content_hash).into();
                    let ct: Arc<str> = mime_type.clone();
                    let file_read = Arc::clone(&self.file_read);
                    let id_owned = id.to_string();
                    let cap = file_size as usize;
                    let (bytes, ..) = cache
                        .load_and_cache(dto.content_hash.to_string(), etag, ct, async move {
                            debug!("💾 TIER 1 Cache MISS: {} – loading from disk", id_owned);
                            Self::read_full(&file_read, &id_owned, cap).await
                        })
                        .await?;
                    bytes
                }
            } else {
                debug!(
                    "💾 TIER 1 (uncacheable): {} – streaming from disk",
                    dto.name
                );
                Self::read_full(&self.file_read, id, file_size as usize).await?
            };

            if do_transcode
                && let Some((t, m)) = self
                    .try_transcode(
                        id,
                        // Empty on the hash-less stub DTOs (external mounts),
                        // which the content-keyed tier cannot serve anyway.
                        Some(&*dto.content_hash).filter(|h: &&str| !h.is_empty()),
                        &content_bytes,
                        &mime_type,
                        file_size,
                        true,
                    )
                    .await
            {
                return Ok((
                    dto,
                    OptimizedFileContent::Bytes {
                        data: t,
                        mime_type: m,
                        was_transcoded: true,
                    },
                ));
            }
            return Ok((
                dto,
                OptimizedFileContent::Bytes {
                    data: content_bytes,
                    mime_type: mime_type.clone(),
                    was_transcoded: false,
                },
            ));
        }

        // ── Tier 2 + 3: Streaming (≥10 MB) ──────────────────
        info!(
            "📡 TIER 2 STREAMING: {} ({} MB)",
            dto.name,
            file_size / (1024 * 1024)
        );
        let stream = self.file_read.get_file_stream(id).await?;
        Ok((dto, OptimizedFileContent::Stream(Box::into_pin(stream))))
    }

    /// Batch counterpart of [`FileRetrievalUseCase::get_file`]: resolve many
    /// file ids in ONE query instead of one per id. Like `get_file` it
    /// performs no per-file authorization — both current callers (ACL grant
    /// listing, NextCloud favorites REPORT) resolve ids already vetted by the
    /// authorization engine or the favorites table. Missing or trashed ids are
    /// absent from the result; callers re-associate by `id`.
    pub async fn get_files_by_ids(&self, ids: &[String]) -> Result<Vec<FileDto>, DomainError> {
        let files = self.file_read.get_files_by_ids(ids).await?;
        Ok(files.into_iter().map(FileDto::from).collect())
    }

    /// Batched, authorized multi-get for the ZIP-download multi-select — the
    /// batch form of [`FileRetrievalUseCase::get_file_with_perms`] over an
    /// explicit id list.
    ///
    /// Authorizes `Read` on every id in ONE `check_files_read_batch`
    /// round-trip (which resolves all drives in a single query AND primes the
    /// resource→drive cache, so the per-file re-check the subsequent stream
    /// open performs becomes a cache hit), then fetches only the authorized ids
    /// in ONE `get_files_by_ids` query. Replaces `download_zip`'s per-file
    /// `require_file` + `get_file` loop — 2 round-trips/file → 2 total.
    ///
    /// Returns the authorized, existing files; a denied / missing / unparseable
    /// id is simply **absent** from the result (the caller re-associates by id
    /// and skips the rest, exactly as the per-file loop skipped a denied /
    /// missing `get_file_with_perms`). Read-authorization is identical to the
    /// per-file path (`check_files_read_batch` is documented and gated as
    /// semantically identical to looping `require`). Recents recording is left
    /// to the subsequent per-file stream open (`get_file_stream_with_perms`),
    /// which records it (throttle-coalesced) — same net effect as the old
    /// loop's `notify_file_accessed` + stream double-notify. Fail-closed if no
    /// engine was injected, mirroring [`Self::require_file`].
    pub async fn get_files_by_ids_with_perms(
        &self,
        ids: &[String],
        caller_id: Uuid,
    ) -> Result<Vec<FileDto>, DomainError> {
        let authz = self.authz.as_ref().ok_or_else(|| {
            DomainError::internal_error("FileRetrieval", "Authorization engine unavailable")
        })?;
        // Unparseable ids can't be authorized (the per-file path 404s on them),
        // so drop them here — they stay absent from the authorized set.
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect();
        if uuids.is_empty() {
            return Ok(Vec::new());
        }
        let allowed = authz
            .check_files_read_batch(Subject::User(caller_id), &uuids)
            .await?;
        if allowed.is_empty() {
            return Ok(Vec::new());
        }
        let allowed_ids: Vec<String> = allowed.iter().map(Uuid::to_string).collect();
        let files = self.file_read.get_files_by_ids(&allowed_ids).await?;
        Ok(files.into_iter().map(FileDto::from).collect())
    }

    /// Range read for HTTP Range Requests, cache-aware.
    ///
    /// Media players and PDF viewers fetch these files *exclusively* through
    /// Range requests (a `bytes=0-` probe, then seeks) — the plain streaming
    /// path paid 1 PG round-trip (blob-hash resolve) + a chunk open/seek for
    /// EVERY seek, even when the whole blob was already sitting in the moka
    /// content cache as one contiguous `Bytes`. For sub-`CACHE_THRESHOLD`
    /// files this now answers from the cache: `Bytes::slice` is a refcount
    /// bump — zero copy, zero I/O, zero PG (benches/RANGE-CACHE.md). A miss
    /// populates the cache via the same single-flight `get_or_load` Tier 1
    /// uses, so one probe warms every subsequent seek. `end` is exclusive
    /// (callers pass `Some(last_byte + 1)`), matching the streaming variant.
    pub async fn get_file_range_preloaded(
        &self,
        dto: &FileDto,
        start: u64,
        end: Option<u64>,
    ) -> Result<RangeContent, DomainError> {
        let cacheable = dto.size < CACHE_THRESHOLD && !dto.content_hash.is_empty();
        if cacheable && let Some(cache) = &self.content_cache {
            // Probe with a BORROW first: the video-scrub steady state is a cache
            // hit, and a hit must not allocate the owned load args (quoted-etag /
            // key / id Strings) it would immediately discard — those are built
            // only on the miss branch (benches/ROUND29.md §B). A miss still
            // populates via the same single-flight coalescing.
            let bytes = if let Some((bytes, ..)) = cache.get(&dto.content_hash).await {
                bytes
            } else {
                let etag: Arc<str> = format!("\"{}\"", dto.content_hash).into();
                let ct: Arc<str> = dto.mime_type.clone();
                let file_read = Arc::clone(&self.file_read);
                let id_owned = dto.id.clone();
                let cap = dto.size as usize;
                let (bytes, ..) = cache
                    .load_and_cache(dto.content_hash.to_string(), etag, ct, async move {
                        debug!("💾 Range cache MISS: {} – loading from disk", id_owned);
                        Self::read_full(&file_read, &id_owned, cap).await
                    })
                    .await?;
                bytes
            };
            let len = bytes.len() as u64;
            let s = start.min(len) as usize;
            let e = end.unwrap_or(len).min(len) as usize;
            if s <= e {
                return Ok(RangeContent::Bytes(bytes.slice(s..e)));
            }
            // Degenerate range the validator should have rejected — fall
            // through to the streaming path rather than panic on slice.
        }
        let stream = self
            .file_read
            .get_file_range_stream(&dto.id, start, end)
            .await?;
        Ok(RangeContent::Stream(stream))
    }
}

impl FileRetrievalUseCase for FileRetrievalService {
    async fn get_file(&self, id: &str) -> Result<FileDto, DomainError> {
        let file = self.file_read.get_file(id).await?;
        Ok(FileDto::from(file))
    }

    async fn get_file_with_perms(&self, id: &str, caller_id: Uuid) -> Result<FileDto, DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        let file = self.file_read.get_file(id).await?;
        // After authZ + lookup succeed: this caller has just inspected the
        // file. Recent listing observes via the hook. The throttle in the
        // recording impl coalesces repeat metadata fetches against the same
        // file (file viewer poll, browse-then-download pattern).
        self.notify_file_accessed(caller_id, id);
        Ok(FileDto::from(file))
    }

    async fn get_file_or_trashed_with_perms(
        &self,
        id: &str,
        caller_id: Uuid,
    ) -> Result<FileDto, DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        let file = self.file_read.get_file_or_trashed(id).await?;
        Ok(FileDto::from(file))
    }

    // FIXME no authorisation at all
    async fn get_file_by_path(&self, path: &str, drive_id: Uuid) -> Result<FileDto, DomainError> {
        // Direct SQL lookup — O(folder_depth) queries instead of O(total_files)
        // NOTE: This method does NOT perform any authorization check. Callers
        // that surface its result to a user-driven request MUST resolve the
        // file via get_file_owned afterwards, or call authz.require directly.
        // (Tracked in the audit punch-list under "path-based lookups".)
        // `drive_id` scope axis prevents cross-drive resolution — without
        // it, `find_file_by_path` would return a non-deterministic row
        // when the same path exists in multiple drives.
        // External mount: a path descending past a mount root resolves on the
        // provider (stat). The mount root itself has no file at its path.
        if let Some(router) = &self.mount_router
            && let Some((cfg, remainder)) = router.find_path(drive_id, path)
            && !remainder.is_empty()
        {
            let node = cfg.provider.resolve_path(&remainder);
            let stat = cfg.provider.stat(&node).await?;
            if stat.is_dir {
                return Err(DomainError::not_found("File", path));
            }
            let parent = crate::application::services::mount_dto::mount_parent_id(
                &cfg,
                stat.node_id.as_str(),
            );
            return Ok(crate::application::services::mount_dto::mount_file_dto(
                &cfg, &parent, &stat,
            ));
        }

        if let Some(file) = self.file_read.find_file_by_path(path, drive_id).await? {
            return Ok(FileDto::from(file));
        }

        Err(DomainError::not_found(
            "File",
            format!("not found at path: {}", path),
        ))
    }

    async fn list_files(&self, folder_id: Option<&str>) -> Result<Vec<FileDto>, DomainError> {
        let files = self.file_read.list_files(folder_id).await?;
        Ok(files.into_iter().map(FileDto::from).collect())
    }

    async fn list_files_with_perms(
        &self,
        folder_id: Option<&str>,
        owner_id: Uuid,
    ) -> Result<Vec<FileDto>, DomainError> {
        // Files always have a `folder_id` in the D0+ model — there is no
        // longer any concept of "root-level files". A `None` from the
        // caller means the query string was missing `folder_id`; reject
        // with a clear error rather than returning an empty set from a
        // meaningless root-level query.
        if folder_id.is_none() {
            return Err(DomainError::validation_error("folder_id is required"));
        }
        self.require_target_folder_perm(folder_id, Permission::Read, owner_id)
            .await?;
        self.list_files(folder_id).await
    }

    async fn get_file_stream(
        &self,
        id: &str,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        if let Some((cfg, node)) = self.mount_file_node(id) {
            let s = cfg.provider.open_read_stream(&node, None).await?;
            // `Pin<Box<dyn Stream>>` is itself a `Stream`, so re-box it.
            return Ok(Box::new(s));
        }
        self.file_read.get_file_stream(id).await
    }

    async fn get_file_stream_with_perms(
        &self,
        id: &str,
        caller_id: Uuid,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        self.notify_file_accessed(caller_id, id);
        self.file_read.get_file_stream(id).await
    }

    /// Multi-tier optimized download.
    async fn get_file_optimized(
        &self,
        id: &str,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        let file = self.file_read.get_file(id).await?;
        let dto = FileDto::from(file);
        self.optimized_inner(id, dto, accept_webp, prefer_original)
            .await
    }

    async fn get_file_optimized_with_perms(
        &self,
        id: &str,
        caller_id: Uuid,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        let file = self.file_read.get_file(id).await?;
        let dto = FileDto::from(file);
        self.notify_file_accessed(caller_id, id);
        self.optimized_inner(id, dto, accept_webp, prefer_original)
            .await
    }

    /// Like `get_file_optimized` but skips the metadata re-fetch.
    async fn get_file_optimized_preloaded(
        &self,
        id: &str,
        file_dto: FileDto,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        self.optimized_inner(id, file_dto, accept_webp, prefer_original)
            .await
    }

    /// Range-based streaming for HTTP Range Requests.
    async fn get_file_range_stream(
        &self,
        id: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        if let Some((cfg, node)) = self.mount_file_node(id) {
            // The native range convention is exclusive-end; the provider wants
            // an inclusive end.
            let range = Some((start, end.map(|e| e.saturating_sub(1))));
            let s = cfg.provider.open_read_stream(&node, range).await?;
            return Ok(Box::new(s));
        }
        self.file_read.get_file_range_stream(id, start, end).await
    }

    async fn get_file_range_stream_with_perms(
        &self,
        id: &str,
        caller_id: Uuid,
        start: u64,
        end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        // Range requests are bursty (video seeks, NC chunked downloads) —
        // the recording hook's per-(caller, file) throttle absorbs the
        // storm so one watched video lands as one Recent row, not 1000.
        self.notify_file_accessed(caller_id, id);
        self.file_read.get_file_range_stream(id, start, end).await
    }

    // TODO: check: no permission check
    async fn stream_files_in_subtree(
        &self,
        folder_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<FileDto, DomainError>> + Send>>, DomainError> {
        let inner = self.file_read.stream_files_in_subtree(folder_id).await?;
        let mapped = inner.map(|r| r.map(FileDto::from));
        Ok(Box::pin(mapped))
    }

    async fn list_files_batch(
        &self,
        folder_id: Option<&str>,
        after_name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<FileDto>, DomainError> {
        let files = self
            .file_read
            .list_files_batch(folder_id, after_name, limit)
            .await?;
        Ok(files.into_iter().map(FileDto::from).collect())
    }

    async fn list_files_batch_with_perms(
        &self,
        folder_id: Option<&str>,
        owner_id: Uuid,
        after_name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<FileDto>, DomainError> {
        // External mount: list files from the provider (WebDAV/NextCloud
        // PROPFIND Depth:1 file loop). Authz collapses on the mount root.
        // Keyset pagination by name mirrors `paginate_mount_entries` —
        // provider order isn't guaranteed, so sort before slicing on
        // `after_name`.
        if let Some(fid) = folder_id
            && let Some(router) = &self.mount_router
        {
            use crate::application::services::external_mount_router::ResolvedId;
            let resolved = match router.classify(fid) {
                ResolvedId::Regular => None,
                ResolvedId::MountRoot { cfg } => Some((cfg, NodeId::default())),
                ResolvedId::MountChild { cfg, node_id } => Some((cfg, node_id)),
            };
            if let Some((cfg, node)) = resolved {
                if let Some(authz) = &self.authz {
                    authz
                        .require(
                            Subject::User(owner_id),
                            Permission::Read,
                            Resource::Folder(cfg.mount_id),
                        )
                        .await?;
                }
                let mut entries: Vec<_> = cfg
                    .provider
                    .list_dir(&node)
                    .await?
                    .into_iter()
                    .filter(|e| !e.is_dir)
                    .collect();
                entries.sort_by_key(|e| e.name.to_lowercase());
                let start = match after_name {
                    Some(name) => entries
                        .iter()
                        .position(|e| name.eq_ignore_ascii_case(&e.name))
                        .map(|i| i + 1)
                        .unwrap_or(0),
                    None => 0,
                };
                let files: Vec<FileDto> = entries
                    .into_iter()
                    .skip(start)
                    .take(limit.max(0) as usize)
                    .map(|e| {
                        crate::application::services::mount_dto::mount_entry_file_dto(&cfg, fid, &e)
                    })
                    .collect();
                return Ok(files);
            }
        }

        // Post-D0: every file lives in a folder — `storage.files.folder_id`
        // is NOT NULL. `folder_id = None` means the caller is asking for
        // "root-level files", which by design return an empty set: the
        // WebDAV synthetic root only lists drive-root folders as
        // children. Skip the DB round-trip and the pre-D7 owner-fallback
        // query (which used to hit `_for_owner` and would have driven
        // the `files.user_id` filter this refactor is retiring).
        let Some(_) = folder_id else {
            return Ok(Vec::new());
        };
        self.require_target_folder_perm(folder_id, Permission::Read, owner_id)
            .await?;
        let files = self
            .file_read
            .list_files_batch(folder_id, after_name, limit)
            .await?;
        Ok(files.into_iter().map(FileDto::from).collect())
    }
}
