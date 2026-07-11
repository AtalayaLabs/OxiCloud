use axum::{
    body::{self, Body},
    http::{HeaderName, Request, StatusCode, header},
    response::Response,
};
use bytes::{Buf, Bytes};
use chrono::Utc;
use quick_xml::{
    Writer,
    events::{BytesEnd, BytesStart, BytesText, Event},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

use crate::application::adapters::webdav_adapter::{
    PropFindRequest, PropPatchOp, QualifiedName, WebDavAdapter, is_protected_property,
};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::favorites_ports::FavoritesUseCase;
use crate::application::ports::file_ports::{
    FileManagementUseCase, FileRetrievalUseCase, FileUploadUseCase,
};
use crate::application::ports::folder_ports::FolderUseCase;
use crate::application::ports::storage_ports::StorageUsagePort;
use crate::application::ports::trash_ports::TrashUseCase;
use crate::common::di::AppState;
use crate::common::mime_detect::filename_from_path;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::services::path_resolver_service::ResolvedResource;
use crate::infrastructure::services::webdav_dead_property_store::ResourceRef;
use crate::interfaces::api::handlers::webdav_handler::{
    PROPFIND_BATCH_SIZE, cas_write_patch, dead_props_for, enforce_native_lock, file_dead_props,
    files_dead_props_map, folder_dead_props, folders_dead_props_map, if_match_precondition_fails,
    if_none_match_precondition_fails, parse_update_range, splice_patch_streams,
};
use crate::interfaces::errors::AppError;
use crate::interfaces::range_requests::{not_modified_response, range_response};
use crate::interfaces::upload_ingest::{
    PatchIngestBudget, discard_ingested, ingest_body_to_cas, ingest_range_patch_to_cas,
};

/// Extension trait to map XML write errors to `String` concisely.
trait XmlResultExt<T> {
    fn xml_err(self) -> Result<T, String>;
}

impl<T, E: std::fmt::Display> XmlResultExt<T> for Result<T, E> {
    fn xml_err(self) -> Result<T, String> {
        self.map_err(|e| e.to_string())
    }
}

/// Convert a `u64` timestamp to `i64` safely, falling back to 0 on overflow.
fn timestamp_to_i64(ts: u64) -> i64 {
    i64::try_from(ts).unwrap_or(0)
}

const HEADER_DAV: HeaderName = HeaderName::from_static("dav");

/// Resolve the internal OxiCloud path from a NextCloud DAV subpath
/// and the storage chroot the request is confined to.
///
/// `chroot` is the storage path the request is "jailed" inside —
/// the route glue (`routes.rs::handle_dav_*`) computes it once per
/// request:
/// - Legacy `/files/{user}/…` or explicit `~{home_folder_uuid}` →
///   `"My Folder - {username}"` (no DB lookup needed).
/// - `~{some_other_folder_uuid}` → the folder's stored `path` after
///   a `get_folder_with_perms` check (404 if missing / no access).
///
/// By the time we get here `chroot` is known to be a legitimate
/// target — validation and permission live in the route layer, not
/// in the path mapper. This function stays sync and free of any
/// folder-service handle. The chroot's `path` is the canonical root
/// segment (e.g. `"Personal"` for default personal drives provisioned
/// by D0, the original sibling-root folder name for secondary drives).
/// Replaces the pre-D0 hardcoded `"My Folder - {username}/"` prefix.
pub fn nc_to_internal_path(chroot: &FolderDto, subpath: &str) -> Result<String, AppError> {
    let subpath = subpath.trim_matches('/');
    // `chroot.path` comes from `Folder::path_string()` /
    // `StoragePath::to_string()`, which prepends a leading `/` (e.g.
    // `"/Personal"`) — trim it so the result matches the leading-
    // slash-free convention `storage.folders.path` (and the plain
    // WebDAV surface's `db_path`) actually use. Without this, exact-
    // string comparisons against a plain-surface path (e.g. the
    // in-memory WebDAV lock store's key) silently mismatch even
    // though DB-backed lookups tolerate the discrepancy.
    let chroot_path = chroot.path.trim_start_matches('/');
    if subpath.is_empty() {
        return Ok(chroot_path.to_string());
    }
    // Reject path traversal attempts.
    if subpath.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err(AppError::bad_request("Invalid path: traversal not allowed"));
    }

    Ok(format!("{}/{}", chroot_path, subpath))
}

/// Strip the caller's chroot prefix from an internal
/// `storage.folders.path` so the DAV subpath surfaced to the NC
/// client is chroot-relative. Handles multi-segment chroots
/// correctly (e.g. a future `"Personal/folderA/subfolder"` chroot
/// against an item at `"Personal/folderA/subfolder/file.txt"`
/// returns `"file.txt"`, not `"folderA/subfolder/file.txt"`).
///
/// Returns `None` when the path is NOT inside the chroot. Callers
/// should skip such items from the response (they belong to a
/// different drive or the caller's read scope has drifted) — do NOT
/// fall back to a naive segment strip, which would surface a
/// misleading display path.
///
/// **Defensive but not an AuthZ boundary.** Every current caller
/// reaches items through a `_with_perms` method upstream that
/// already gates Read; this helper is the display-string layer
/// that also serves as a "does this item belong under the chroot"
/// sanity check.
pub fn strip_chroot_prefix<'a>(chroot: &FolderDto, internal_path: &'a str) -> Option<&'a str> {
    // Normalize both sides: `FolderDto.path` comes from
    // `StoragePath::to_string()` which prepends a leading `/`
    // (e.g. `"/Personal"`), but DB-side paths coming from
    // `storage.folders.path` (composed by the `compute_folder_path`
    // trigger) never have a leading slash. Trim both so `"/Personal"`
    // vs `"Personal/g9-tree"` matches the intended prefix.
    let root = chroot.path.trim_matches('/');
    if root.is_empty() {
        // Guard against a mis-set chroot with an empty root path —
        // stripping "" from anything would return the whole path.
        return None;
    }
    let path = internal_path.trim_start_matches('/');
    let rest = path.strip_prefix(root)?;
    // Reject a partial prefix match — a chroot of "Personal" must
    // not match an item at "PersonalSecrets/…".
    match rest.strip_prefix('/') {
        Some(subpath) => Some(subpath),
        // Item path equals the chroot exactly — the chroot itself
        // (i.e. a folder) is not a legitimate response item, so
        // treat as an empty subpath.
        None if rest.is_empty() => Some(""),
        None => None,
    }
}

/// Naive fallback: strip the first path segment from an internal
/// `storage.folders.path`. Post-D0 every path starts with its drive's
/// root folder name (single segment), so for the current schema this
/// gives the drive-relative subpath.
///
/// Use this ONLY when the caller doesn't have a chroot in scope
/// (e.g. OCS unified search, whose results legitimately span every
/// drive the caller has Read on — no single chroot covers them all).
/// Every path-scoped NC handler that DOES have `session` in scope
/// should prefer [`strip_chroot_prefix`] — it validates the item
/// belongs under the chroot instead of trusting the schema
/// invariant, and it survives a future composed chroot like
/// `"Personal/folderA/subfolder"`.
///
/// **Not an AuthZ boundary.** Same caveat as `strip_chroot_prefix`
/// — AuthZ is enforced upstream via `_with_perms` methods; this
/// helper only formats display strings.
///
/// Returns `""` when the path is a single segment (i.e. the drive
/// root itself, which is never a legitimate item target).
pub fn strip_drive_root_segment(internal_path: &str) -> &str {
    match internal_path.split_once('/') {
        Some((_root, rest)) => rest,
        None => "",
    }
}

/// Build the Nextcloud DAV href for a **collection** (folder). Always
/// terminates with `/` — RFC 4918 §5.2 requires collection URLs to end
/// in a slash, and the Nextcloud desktop client strictly enforces this
/// for the "own entry" href in PROPFIND multi-status responses: a
/// PROPFIND on `/remote.php/dav/files/admin/ext/` whose first response
/// `<d:href>` doesn't end in `/` aborts the parse with
/// `Invalid href "<…>" expected starting with "<requested-url>"` and
/// surfaces as `Network request error "Erreur inconnue" HTTP status
/// 207` in the client log. Files use [`nc_href`] (no trailing slash).
pub fn nc_collection_href(username: &str, subpath: &str) -> String {
    let encoded_user = urlencoding::encode(username);
    let mut out = String::new();
    nc_collection_href_into(&mut out, &encoded_user, subpath);
    out
}

/// Build the Nextcloud DAV href for a resource.
///
/// Each path segment is URL-encoded individually so filenames with spaces,
/// `#`, `%`, or non-ASCII characters produce valid PROPFIND hrefs.
///
/// Returns NO trailing slash for non-empty subpaths. Callers rendering
/// a **collection** must use [`nc_collection_href`] (or append `/`
/// manually) to satisfy RFC 4918 §5.2 and the NC client's parser.
pub fn nc_href(username: &str, subpath: &str) -> String {
    let encoded_user = urlencoding::encode(username);
    let mut out = String::new();
    nc_href_into(&mut out, &encoded_user, subpath);
    out
}

/// Per-row form of [`nc_href`]: write the href into a REUSED buffer given the
/// already-URL-encoded username.
///
/// The emit loops (PROPFIND children, REPORT results) call this instead of
/// [`nc_href`] so each row rewrites one buffer rather than allocating a fresh
/// `String`, and the constant `encoded_user` is encoded ONCE per page instead of
/// re-encoded for every row (benches/ROUND29.md §A — the same reused-buffer shape
/// the PROPFIND child loop already uses for its href prefix). Byte-identical to
/// [`nc_href`].
pub fn nc_href_into(out: &mut String, encoded_user: &str, subpath: &str) {
    let subpath = subpath.trim_matches('/');
    // Write the prefix, user and each encoded segment straight into one
    // pre-sized buffer — avoids the per-segment `Vec<Cow>`, the joined String and
    // the `format!` result the previous `.map(...).collect().join("/")` allocated
    // on every NC PROPFIND/REPORT href (mirrors the native `encode_uri_path`).
    // Keeps `urlencoding::encode` so the emitted bytes are unchanged.
    const PREFIX: &str = "/remote.php/dav/files/";
    out.clear();
    out.reserve(PREFIX.len() + encoded_user.len() + subpath.len() + 8);
    out.push_str(PREFIX);
    out.push_str(encoded_user);
    out.push('/');
    // No empty-segment filter: `split('/')` on an empty (root) subpath yields a
    // single "" whose encode is "" — leaving the trailing slash above intact —
    // and any internal "//" is preserved byte-for-byte, exactly as the old
    // `split → map → join("/")` produced.
    for (i, seg) in subpath.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&urlencoding::encode(seg));
    }
}

/// Per-row form of [`nc_collection_href`]: [`nc_href_into`] plus the trailing
/// `/` RFC 4918 §5.2 / the NC client require for a collection. Byte-identical to
/// [`nc_collection_href`].
pub fn nc_collection_href_into(out: &mut String, encoded_user: &str, subpath: &str) {
    nc_href_into(out, encoded_user, subpath);
    if !out.ends_with('/') {
        out.push('/');
    }
}

/// Dispatch Nextcloud WebDAV request to the appropriate handler.
///
/// `subpath` is everything after `/remote.php/dav/files/{user}/`.
/// `session.chroot` is the storage path the request is confined to
/// — see [`nc_to_internal_path`] for what gets resolved upstream.
/// `session.raw_username` is the literal wire identifier — bare
/// `admin` for single-drive sync, composite `admin~{drive_uuid}` for
/// multi-drive. **Hrefs in every response MUST be built from
/// `session.raw_username`, not from `session.user.username`** — the
/// NC desktop client validates that PROPFIND/MOVE response hrefs
/// share the requested URL's prefix and aborts the parse otherwise
/// (`Invalid href "<…>" expected starting with "<requested-url>"`).
/// The bare `session.user.username` is still the right value for
/// the storage-side owner identity (`oc:owner-id`).
pub async fn handle_nc_webdav(
    state: Arc<AppState>,
    req: Request<Body>,
    session: crate::interfaces::nextcloud::session::SharedNcSession,
    subpath: String,
) -> Result<Response<Body>, AppError> {
    // Validate up-front that we have a chroot — every method below is
    // path-scoped, so a missing chroot is a route-wiring bug we want to
    // surface as a 500 immediately rather than re-checking inside each
    // handler.
    session.require_chroot()?;
    let method = req.method().clone();
    match method.as_str() {
        "OPTIONS" => handle_options(),
        "PROPFIND" => handle_propfind(state, req, &session, &subpath).await,
        "GET" => handle_get(state, &session, &subpath, req.headers()).await,
        "PUT" => handle_put(state, req, &session, &subpath).await,
        "PATCH" => handle_patch(state, req, &session, &subpath).await,
        "MKCOL" => handle_mkcol(state, &session, &subpath).await,
        "DELETE" => handle_delete(state, &session, &subpath).await,
        "MOVE" => handle_move(state, req, &session, &subpath).await,
        "HEAD" => handle_head(state, &session, &subpath).await,
        "PROPPATCH" => handle_proppatch(state, req, &session, &subpath).await,
        "REPORT" | "SEARCH" => {
            crate::interfaces::nextcloud::report_handler::handle_nc_report(
                state, req, &session, &subpath,
            )
            .await
        }
        _ => Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .unwrap()),
    }
}

// ──────────────────── OPTIONS ────────────────────

fn handle_options() -> Result<Response<Body>, AppError> {
    // Advertise WebDAV compliance classes 1 + 3 only.
    // Class 2 (LOCK/UNLOCK) is intentionally omitted because the NC
    // surface has no LOCK/UNLOCK dispatch arm — claiming class 2
    // would invite clients (notably the NC desktop sync engine) to
    // start sending LOCK requests we then 405. Class 3 covers the
    // weak-resource-validators behaviour PROPFIND already implements.
    // If LOCK is ever wired in here, restore "1, 2, 3" in the same
    // commit as the LOCK arm — never split the advertisement from
    // the implementation.
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(HEADER_DAV, "1, 3")
        .header(
            header::ALLOW,
            "OPTIONS, GET, HEAD, PUT, PATCH, DELETE, MKCOL, MOVE, PROPFIND, PROPPATCH, REPORT, SEARCH",
        )
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── PROPFIND ────────────────────

async fn handle_propfind(
    state: Arc<AppState>,
    req: Request<Body>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    let chroot = session.require_chroot()?;
    let url_user = &session.raw_username;
    let depth = req
        .headers()
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1")
        .to_string();

    // Parse the PROPFIND XML body (or assume allprop if empty).
    let body_bytes = body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read body: {}", e)))?;

    // Parse (and thereby validate) the PROPFIND body. The NC response
    // always emits the full property set; the parsed request is consulted
    // only to skip the quota DB round-trips when the client's explicit
    // prop list never names a quota prop. Malformed XML still fails 400.
    let propfind = if body_bytes.is_empty() {
        PropFindRequest {
            prop_find_type: crate::application::adapters::webdav_adapter::PropFindType::AllProp,
        }
    } else {
        WebDavAdapter::parse_propfind(body_bytes.reader())
            .map_err(|e| AppError::bad_request(format!("Invalid PROPFIND XML: {}", e)))?
    };

    let internal_path = nc_to_internal_path(chroot, subpath)?;

    // Single-query path resolution (drive-scoped) — same shared
    // resolver as native `/webdav/…`. Post-D7 the resolver is not
    // owner-scoped, so we `authz.require(Read, …)` on the returned
    // resource explicitly before emitting the multistatus.
    let resolved = nc_resolve_or_fallback(&state, &internal_path, chroot.drive_id)
        .await
        .ok_or_else(|| AppError::not_found("Resource not found"))?;

    match resolved {
        ResolvedResource::Folder(folder) => {
            let folder_uuid = Uuid::parse_str(&folder.id)
                .map_err(|_| AppError::not_found("Resource not found"))?;
            state
                .authorization
                .require(
                    Subject::User(user.id),
                    Permission::Read,
                    Resource::Folder(folder_uuid),
                )
                .await?;

            // It's a folder — stream the multistatus: children are fetched in
            // pages and serialized chunk by chunk, so memory stays O(batch)
            // regardless of how many entries the folder holds.
            //
            // Multi-drive POC: the hrefs in the response must echo the
            // wire form (`{user}~{drive}`) the client requested, so we
            // pass `url_user` (not `user.username`) as the streaming
            // function's username arg. Refining the owner-id usages
            // back to the canonical username is deferred to the
            // NcSession commit.
            // Explicit prop lists that never name a quota prop skip the
            // 2-query quota resolution (benches/QUOTA-PATH.md).
            let quota = if propfind.wants_quota() {
                state.resolve_webdav_quota(user.id, chroot.drive_id).await
            } else {
                None
            };
            Ok(build_nc_streaming_propfind(
                state.clone(),
                folder,
                depth,
                user.id,
                url_user.to_string(),
                subpath.to_string(),
                quota,
            ))
        }
        ResolvedResource::File(file) => {
            let file_uuid =
                Uuid::parse_str(&file.id).map_err(|_| AppError::not_found("Resource not found"))?;
            state
                .authorization
                .require(
                    Subject::User(user.id),
                    Permission::Read,
                    Resource::File(file_uuid),
                )
                .await?;

            // Batch-check favorites for this single file.
            let favorite_ids = if let Some(fav_svc) = state.favorites_service.as_ref() {
                let items: Vec<(&str, &str)> = vec![(&file.id, "file")];
                fav_svc
                    .batch_check_favorites(user.id, &items)
                    .await
                    .unwrap_or_default()
            } else {
                HashSet::new()
            };

            let nc = state.nextcloud.as_ref();
            let file_id_svc = nc.map(|n| &n.file_ids);

            let dead_props = file_dead_props(&state, &file).await;

            let mut buf = Vec::new();
            write_nc_file_multistatus(
                &mut buf,
                &file,
                url_user,
                &user.username,
                subpath,
                file_id_svc,
                (&favorite_ids, &dead_props),
            )
            .await
            .map_err(|e| AppError::internal_error(format!("XML generation failed: {}", e)))?;

            Ok(Response::builder()
                .status(StatusCode::MULTI_STATUS)
                .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(Body::from(buf))
                .unwrap())
        }
    }
}

/// NC-surface path resolution: try the single-query resolver, fall back
/// to the double-query `get_*_by_path` pair when the resolver isn't
/// configured. Same shape and drive-scope as the native surface —
/// callers `authz.require(…)` on the returned resource.
///
/// `pub(crate)` so `report_handler.rs::handle_sync_collection` can reuse
/// it instead of duplicating the resolver/fallback logic.
pub(crate) async fn nc_resolve_or_fallback(
    state: &Arc<AppState>,
    internal_path: &str,
    drive_id: Uuid,
) -> Option<ResolvedResource> {
    if let Some(resolver) = &state.path_resolver
        && let Ok(r) = resolver
            .resolve_path_in_drive(internal_path, drive_id)
            .await
    {
        return Some(r);
    }
    let folder_service = &state.applications.folder_service;
    if let Ok(folder) = folder_service
        .get_folder_by_path(internal_path, drive_id)
        .await
    {
        return Some(ResolvedResource::Folder(folder));
    }
    let file_service = &state.applications.file_retrieval_service;
    if let Ok(file) = file_service.get_file_by_path(internal_path, drive_id).await {
        return Some(ResolvedResource::File(file));
    }
    None
}

// ──────────────────── GET ────────────────────

async fn handle_get(
    state: Arc<AppState>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
    headers: &axum::http::HeaderMap,
) -> Result<Response<Body>, AppError> {
    let chroot = session.require_chroot()?;
    // GET on root folder — NC clients use this as an existence check
    if subpath.is_empty() || subpath == "/" {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("DAV", "1, 3")
            .body(Body::empty())
            .unwrap());
    }

    let user = &session.user;
    let internal_path = nc_to_internal_path(chroot, subpath)?;
    let file_service = &state.applications.file_retrieval_service;

    // Single-query path resolution. NC clients use GET on a folder as
    // an existence probe (returns 200 empty); file GETs serve content.
    // Post-D7 the resolver is drive-scoped, so both branches
    // `authz.require(Read, …)` before responding.
    let resolved = nc_resolve_or_fallback(&state, &internal_path, chroot.drive_id)
        .await
        .ok_or_else(|| AppError::not_found("File not found"))?;

    let file = match resolved {
        ResolvedResource::Folder(folder) => {
            let folder_uuid =
                Uuid::parse_str(&folder.id).map_err(|_| AppError::not_found("File not found"))?;
            state
                .authorization
                .require(
                    Subject::User(user.id),
                    Permission::Read,
                    Resource::Folder(folder_uuid),
                )
                .await?;
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("DAV", "1, 3")
                .body(Body::empty())
                .unwrap());
        }
        ResolvedResource::File(f) => {
            let file_uuid =
                Uuid::parse_str(&f.id).map_err(|_| AppError::not_found("File not found"))?;
            state
                .authorization
                .require(
                    Subject::User(user.id),
                    Permission::Read,
                    Resource::File(file_uuid),
                )
                .await?;
            f
        }
    };

    // ETag comes from `FileDto::etag` (populated from `File::etag()`
    // in the `From<File>` impl) — single source of truth, so GET,
    // HEAD, PUT-response, MOVE, and PROPFIND all emit byte-identical
    // values for the same file. NC's sync engine compares cached
    // PROPFIND ETags against GET/HEAD responses; using `file.id` here
    // (a UUID) while PROPFIND emitted the blob hash made NC see
    // every file as "remotely changed" on first descent.
    let etag = format!("\"{}\"", file.etag);

    // Conditional GET — sync clients revalidating get a 304, not the body.
    if let Some(resp) = not_modified_response(headers, &etag) {
        return Ok(resp);
    }

    // Recent recording deliberately does NOT fire here: NC's primary
    // client (Nextcloud desktop, davx5, mobile NC apps) is a sync
    // engine, and a first-time descent of a large library would push
    // every file into Recent, drowning out the SPA's "what I actually
    // opened" signal. See memory note
    // `project_recent_session_intent.md` — the planned session-intent
    // gate (interactive JWT vs app-password) will turn this back on
    // for human-driven NC web access in the same browser session.

    // Range Requests — serve 206/416 instead of the whole file on seeks.
    if let Some(resp) = range_response(headers, &file, &etag, file_service).await {
        return Ok(resp);
    }

    let stream = file_service
        .get_file_stream(&file.id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to read file: {}", e)))?;

    let modified_at =
        chrono::DateTime::<Utc>::from_timestamp(timestamp_to_i64(file.modified_at), 0)
            .unwrap_or_else(Utc::now);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, file.mime_type.as_ref())
        .header(header::CONTENT_LENGTH, file.size)
        .header(header::ETAG, etag)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::LAST_MODIFIED, modified_at.to_rfc2822())
        .body(Body::from_stream(std::pin::Pin::from(stream)))
        .unwrap())
}

// ──────────────────── HEAD ────────────────────

async fn handle_head(
    state: Arc<AppState>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let chroot = session.require_chroot()?;
    // HEAD on root folder — NC clients use this as an existence check
    if subpath.is_empty() || subpath == "/" {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("DAV", "1, 3")
            .body(Body::empty())
            .unwrap());
    }

    let user = &session.user;
    let internal_path = nc_to_internal_path(chroot, subpath)?;

    // Single-query path resolution. Both branches `authz.require(Read, …)`
    // on the returned resource before responding.
    let resolved = nc_resolve_or_fallback(&state, &internal_path, chroot.drive_id)
        .await
        .ok_or_else(|| AppError::not_found("File not found"))?;

    let file = match resolved {
        ResolvedResource::Folder(folder) => {
            let folder_uuid =
                Uuid::parse_str(&folder.id).map_err(|_| AppError::not_found("File not found"))?;
            state
                .authorization
                .require(
                    Subject::User(user.id),
                    Permission::Read,
                    Resource::Folder(folder_uuid),
                )
                .await?;
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("DAV", "1, 3")
                .body(Body::empty())
                .unwrap());
        }
        ResolvedResource::File(f) => {
            let file_uuid =
                Uuid::parse_str(&f.id).map_err(|_| AppError::not_found("File not found"))?;
            state
                .authorization
                .require(
                    Subject::User(user.id),
                    Permission::Read,
                    Resource::File(file_uuid),
                )
                .await?;
            f
        }
    };

    let modified_at =
        chrono::DateTime::<Utc>::from_timestamp(timestamp_to_i64(file.modified_at), 0)
            .unwrap_or_else(Utc::now);

    // ETag comes from `FileDto::etag` — see the same comment block on
    // the GET handler. HEAD and GET must agree byte-for-byte; pulling
    // both from the same DTO field guarantees that.
    //
    // We deliberately do NOT set `Content-Length: file.size` here even
    // though RFC 7231 §4.3.2 says HEAD SHOULD return the same headers
    // GET would. Our body is `Body::empty()`, so declaring a non-zero
    // Content-Length tells the client "20 bytes are coming" — and on a
    // keep-alive connection the client waits forever for them. Hyper
    // derives `Content-Length: 0` from the empty body, which is honest
    // about what's actually on the wire. Clients that need the file
    // size use PROPFIND (which is what NC and Sabre clients do).
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, file.mime_type.as_ref())
        .header(header::ETAG, format!("\"{}\"", file.etag))
        .header(header::LAST_MODIFIED, modified_at.to_rfc2822())
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── PROPPATCH ────────────────────

/// The `oc:favorite` element is live server state routed through the
/// favorites service, not a dead property — every other
/// namespace/local-name pair PROPPATCH sends is stored verbatim via
/// `DeadPropertyStore`.
const OC_FAVORITE_NS: &str = "http://owncloud.org/ns";

async fn handle_proppatch(
    state: Arc<AppState>,
    req: Request<Body>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    let chroot = session.require_chroot()?;
    let url_user = &session.raw_username;
    let body_bytes = body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read body: {}", e)))?;

    // Resolve the target resource — needed for three things:
    //  1. The dead-property store key is the resource id (folder_id
    //     XOR file_id), so we need a `ResourceRef`.
    //  2. Applying the oc:favorite mutation (`item_type` distinguishes
    //     file vs folder rows in the favorites table).
    //  3. Picking the right `<d:href>` shape in the multi-status
    //     response: collection (folder) hrefs MUST end in `/` per
    //     RFC 4918 §5.2 — see `nc_collection_href` for the full
    //     reasoning.
    //
    // A missing resource is now always a 404: unlike the previous
    // favorite-only implementation (which merely re-declared success
    // without doing anything), this handler performs real writes, so
    // silently no-opping on a nonexistent path would be a foot-gun —
    // matches the native `/webdav/` handler's contract.
    let internal_path = nc_to_internal_path(chroot, subpath)?;
    // Single-query path resolution — PROPPATCH may target either a
    // folder or a file. Post-D7 the resolver is drive-scoped, so we
    // `authz.require(Read, …)` on the returned resource before
    // reading its type. The favorite mutation below itself doesn't
    // require additional authz (favorites are per-user; the caller can
    // favourite any resource they can see).
    let (resource_ref, item_id, item_type, is_collection) =
        match nc_resolve_or_fallback(&state, &internal_path, chroot.drive_id).await {
            Some(ResolvedResource::File(file)) => {
                let id = Uuid::parse_str(&file.id)
                    .map_err(|_| AppError::not_found("Resource not found"))?;
                state
                    .authorization
                    .require(Subject::User(user.id), Permission::Read, Resource::File(id))
                    .await?;
                (ResourceRef::File(id), file.id, "file", false)
            }
            Some(ResolvedResource::Folder(folder)) => {
                let id = Uuid::parse_str(&folder.id)
                    .map_err(|_| AppError::not_found("Resource not found"))?;
                state
                    .authorization
                    .require(
                        Subject::User(user.id),
                        Permission::Read,
                        Resource::Folder(id),
                    )
                    .await?;
                (ResourceRef::Folder(id), folder.id, "folder", true)
            }
            None => return Err(AppError::not_found("Resource not found")),
        };

    let ops = WebDavAdapter::parse_proppatch(body_bytes.reader())
        .map_err(|e| AppError::bad_request(format!("Failed to parse PROPPATCH request: {}", e)))?;

    let dead_props = &state.webdav_dead_props;
    let mut results: Vec<(&QualifiedName, bool)> = Vec::new();
    for op in &ops {
        let is_favorite =
            |name: &QualifiedName| name.namespace == OC_FAVORITE_NS && name.name == "favorite";
        match op {
            PropPatchOp::Set(pv) if is_favorite(&pv.name) => {
                if let Some(fav_svc) = state.favorites_service.as_ref() {
                    if pv.value.as_deref().map(str::trim) == Some("1") {
                        fav_svc
                            .add_to_favorites(user.id, &item_id, item_type)
                            .await
                            .map_err(|e| {
                                AppError::internal_error(format!("Failed to add favorite: {e}"))
                            })?;
                    } else {
                        fav_svc
                            .remove_from_favorites(user.id, &item_id, item_type)
                            .await
                            .map_err(|e| {
                                AppError::internal_error(format!("Failed to remove favorite: {e}"))
                            })?;
                    }
                }
                results.push((&pv.name, true));
            }
            PropPatchOp::Remove(name) if is_favorite(name) => {
                if let Some(fav_svc) = state.favorites_service.as_ref() {
                    fav_svc
                        .remove_from_favorites(user.id, &item_id, item_type)
                        .await
                        .map_err(|e| {
                            AppError::internal_error(format!("Failed to remove favorite: {e}"))
                        })?;
                }
                results.push((name, true));
            }
            PropPatchOp::Set(pv) if is_protected_property(&pv.name) => {
                results.push((&pv.name, false));
            }
            PropPatchOp::Remove(name) if is_protected_property(name) => {
                results.push((name, false));
            }
            PropPatchOp::Set(pv) => {
                dead_props
                    .set(resource_ref, pv.name.clone(), pv.value.clone())
                    .await
                    .map_err(|e| {
                        AppError::internal_error(format!("Failed to store dead property: {e}"))
                    })?;
                results.push((&pv.name, true));
            }
            PropPatchOp::Remove(name) => {
                dead_props.remove(resource_ref, name).await.map_err(|e| {
                    AppError::internal_error(format!("Failed to remove dead property: {e}"))
                })?;
                results.push((name, true));
            }
        }
    }

    // Collection vs file href chosen by resource type to satisfy the
    // RFC 4918 §5.2 trailing-slash invariant — see the comment block
    // at the top of this function.
    let href = if is_collection {
        nc_collection_href(url_user, subpath)
    } else {
        nc_href(url_user, subpath)
    };
    let mut response_body = Vec::new();
    WebDavAdapter::generate_proppatch_response(&mut response_body, &href, &results).map_err(
        |e| AppError::internal_error(format!("Failed to generate PROPPATCH response: {}", e)),
    )?;

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(response_body))
        .unwrap())
}

// ──────────────────── PUT ────────────────────

fn precondition_failed_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::PRECONDITION_FAILED)
        .body(Body::empty())
        .unwrap()
}

async fn handle_put(
    state: Arc<AppState>,
    req: Request<Body>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let chroot = session.require_chroot()?;
    let internal_path = nc_to_internal_path(chroot, subpath)?;
    let file_service = &state.applications.file_retrieval_service;
    let upload_service = &state.applications.file_upload_service;

    let claimed_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let oc_mtime = req
        .headers()
        .get("x-oc-mtime")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());

    // Extract before consuming `req` into the body stream further down.
    let if_header = req
        .headers()
        .get("If")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // ── Conditional preconditions (RFC 7232 §3.1 / §3.2) ─────────────
    // Evaluated BEFORE body ingestion so a rejected PUT doesn't waste
    // bandwidth or disk I/O on a body the server is going to throw away.
    // The lookup is reused for the create-vs-update distinction below,
    // so this is also free of an extra DB hit.
    let existing = file_service
        .get_file_by_path(&internal_path, chroot.drive_id)
        .await
        .ok();
    let current_etag = existing.as_ref().map(|f| f.etag.as_str());

    if let Some(value) = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && if_none_match_precondition_fails(value, current_etag)
    {
        return Ok(precondition_failed_response());
    }
    if let Some(value) = req
        .headers()
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        && if_match_precondition_fails(value, current_etag)
    {
        return Ok(precondition_failed_response());
    }

    // ── Existence-check depth (RFC 4918 §9.7.1) ───────────────────────
    // Mirrors the plain WebDAV surface's `handle_put`: PUT to an existing
    // directory is 400, PUT under a missing parent is 409 (not the generic
    // 500 a downstream `NotFound` would otherwise surface as).
    if existing.is_none() {
        if state
            .applications
            .folder_service
            .get_folder_by_path(&internal_path, chroot.drive_id)
            .await
            .is_ok()
        {
            return Err(AppError::bad_request("Cannot PUT to a directory"));
        }
        let parent_path = internal_path
            .rfind('/')
            .map(|i| &internal_path[..i])
            .unwrap_or("");
        if !parent_path.is_empty() {
            state
                .applications
                .folder_service
                .get_folder_by_path(parent_path, chroot.drive_id)
                .await
                .map_err(|_| {
                    AppError::conflict(format!("Parent folder not found: {}", parent_path))
                })?;
        }
    }

    // ── Active-lock guard (RFC 4918 §10.4 If: evaluation) ─────────────
    // Shared with the plain WebDAV surface and with this surface's own
    // `handle_patch`, so a LOCK taken via /webdav/ also protects the same
    // file reached through /remote.php/dav/.
    if let Some(resp) = enforce_native_lock(
        &state.webdav_lock_store,
        if_header.as_deref(),
        &internal_path,
        current_etag,
    ) {
        return Ok(resp);
    }

    // ── Direct PUT cap ───────────────────────────────────────────────
    // We use `direct_put_max_bytes` (default 1 GiB), not `max_upload_size`
    // (default 10 GB). Larger files must come through the chunked upload
    // protocol (`/dav/uploads/...`) which is resumable on failure and
    // bounded per-request by `chunk_max_bytes`. Trying to stream a
    // multi-GB body through a single PUT is a footgun: a connection drop
    // at 95 % loses everything.
    let max_upload = state.core.config.storage.direct_put_max_bytes;

    // Stream the body straight into the CDC chunk store — never buffer the
    // full upload in RAM and never spool it to disk. Chunking, hashing,
    // dedup checks and MIME sniffing (magic bytes off the first frames)
    // all run while the body arrives; chunks the store already has are
    // never written at all. Shared with the native WebDAV PUT handler.
    //
    // `filename` is owned so we don't hold a borrow of the `subpath` param
    // across the await (which would make the handler future non-Send).
    let filename = filename_from_path(subpath).to_string();
    let ingested = ingest_body_to_cas(
        req.into_body(),
        &state.core.dedup_service,
        &filename,
        &claimed_type,
        max_upload,
    )
    .await?;
    let content_type = ingested.content_type.clone();

    // Distinguish create (201) vs update (204) for the response status,
    // using the lookup already done above for the precondition check.
    let existed = existing.is_some();

    // ── Quota enforcement ─────────────────────────────────────────────
    if let Some(storage_svc) = state.storage_usage_service.as_ref()
        && let Err(err) = storage_svc
            .check_storage_quota(session.user.id, ingested.size)
            .await
    {
        discard_ingested(&state.core.dedup_service, &ingested).await;
        tracing::warn!(
            "⛔ NC WEBDAV PUT REJECTED (quota): user={}, file={}, size={}",
            session.user.id,
            internal_path,
            ingested.size
        );
        return Err(AppError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            err.message,
            "QuotaExceeded",
        ));
    }

    // Single streaming path — handles both update and create internally,
    // swapping the file row onto the already-ingested blob.
    // AuthZ audit #6 (2026-07-12): route `_with_perms` errors through
    // `AppError::from` so authz denials surface as 404 (the anti-enum
    // shape) instead of a `map_err → internal_error` 500 that gives a
    // probing caller an "exists-but-denied" oracle. Also preserves
    // `QuotaExceeded → 507`, `AlreadyExists → 409`, `InvalidInput → 400` —
    // matching this surface's own `handle_patch` and the plain WebDAV
    // `handle_put`.
    let stored = upload_service
        .update_file_streaming_with_perms(
            &internal_path,
            chroot.drive_id,
            ingested.stored(),
            &content_type,
            oc_mtime,
            session.user.id,
            None,
        )
        .await
        .map_err(AppError::from)?;

    let status = if existed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    };

    Ok(Response::builder()
        .status(status)
        .header(header::ETAG, format!("\"{}\"", stored.etag))
        .header("oc-etag", format!("\"{}\"", stored.etag))
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── PATCH ────────────────────

/// Handles PATCH requests (RFC 5789) for partial byte-range content
/// updates on the NextCloud file surface — extends the plain WebDAV
/// surface's `X-Update-Range` mechanism (see
/// `api/handlers/webdav_handler.rs::handle_patch` / `parse_update_range`)
/// here. New content is assembled by splicing the request body between
/// the file's untouched prefix/suffix byte ranges and re-ingesting the
/// result as one continuous stream through the same content-addressable
/// pipeline `handle_put` uses ([`ingest_range_patch_to_cas`]) — unedited
/// chunks on either side of the edit typically dedup for free.
///
/// Shares an active-lock guard with the plain WebDAV surface (see below) so
/// a LOCK taken via `/webdav/` also protects the same file reached through
/// `/remote.php/dav/`.
async fn handle_patch(
    state: Arc<AppState>,
    req: Request<Body>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let chroot = session.require_chroot()?;
    let internal_path = nc_to_internal_path(chroot, subpath)?;
    let file_service = &state.applications.file_retrieval_service;
    let upload_service = &state.applications.file_upload_service;

    if subpath.is_empty() || subpath == "/" {
        return Err(AppError::bad_request("Cannot PATCH the root folder"));
    }

    // RFC 5789 doesn't define Content-Range semantics; this server uses a
    // dedicated `X-Update-Range` header instead (see `parse_update_range`)
    // to avoid ambiguity with HTTP Range-Request semantics.
    if req.headers().contains_key(header::CONTENT_RANGE) {
        return Err(AppError::bad_request(
            "PATCH must not use Content-Range; use the X-Update-Range header instead",
        ));
    }

    let update_range_header = req
        .headers()
        .get("X-Update-Range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::bad_request("PATCH requires an X-Update-Range header"))?;

    // Extract all headers before consuming `req` into the body stream.
    let if_none_match = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let if_match = req
        .headers()
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_length = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let claimed_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let if_header = req
        .headers()
        .get("If")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let max_upload = state.core.config.storage.direct_put_max_bytes;

    // ── Existence check ───────────────────────────────────────────────
    // Unlike PUT, PATCH requires an existing file — a partial update of
    // nothing isn't meaningful. On lookup failure, distinguish "it's a
    // directory" (409, matching the plain WebDAV surface) from "it
    // doesn't exist at all" (404) instead of collapsing both to 404.
    let file = match file_service
        .get_file_by_path(&internal_path, chroot.drive_id)
        .await
    {
        Ok(file) => file,
        Err(_) => {
            if state
                .applications
                .folder_service
                .get_folder_by_path(&internal_path, chroot.drive_id)
                .await
                .is_ok()
            {
                return Err(AppError::conflict("Cannot PATCH a directory"));
            }
            return Err(AppError::not_found(format!(
                "File not found: {}",
                internal_path
            )));
        }
    };

    // `get_file_by_path` performs no authorization check (see its own
    // doc comment) — mirrors the plain WebDAV surface's explicit
    // defense-in-depth Read check right after resolving the file, so a
    // caller without Read on this specific file can't learn its size or
    // ETag via the precondition/range-bounds responses below.
    let file_uuid = Uuid::parse_str(&file.id)
        .map_err(|_| AppError::not_found(format!("File not found: {}", internal_path)))?;
    state
        .authorization
        .require(
            Subject::User(session.user.id),
            Permission::Read,
            Resource::File(file_uuid),
        )
        .await?;

    // ── Active-lock guard (RFC 4918 §10.4 If: evaluation) ─────────────
    // Shared with the plain WebDAV surface so a LOCK taken via /webdav/
    // also protects the same file reached through /remote.php/dav/.
    if let Some(resp) = enforce_native_lock(
        &state.webdav_lock_store,
        if_header.as_deref(),
        &internal_path,
        Some(&file.etag),
    ) {
        return Ok(resp);
    }

    // ── RFC 7232 conditional preconditions ────────────────────────────
    let current_etag = Some(file.etag.as_str());
    if let Some(ref value) = if_none_match
        && if_none_match_precondition_fails(value, current_etag)
    {
        return Ok(precondition_failed_response());
    }
    if let Some(ref value) = if_match
        && if_match_precondition_fails(value, current_etag)
    {
        return Ok(precondition_failed_response());
    }

    // ── Range parsing + validation ─────────────────────────────────────
    let (start, end) = parse_update_range(&update_range_header, file.size)?;
    if let (Some(end), Some(len)) = (end, content_length) {
        let expected = end - start + 1;
        if len != expected {
            return Err(AppError::bad_request(format!(
                "Content-Length {len} does not match X-Update-Range span {expected}"
            )));
        }
    }

    // ── Splice prefix/suffix around the patched span ───────────────────
    let (prefix_segment, suffix_segment) = splice_patch_streams(
        file_service,
        &file.id,
        session.user.id,
        start,
        end,
        file.size,
    )
    .await?;
    let filename = filename_from_path(subpath).to_string();
    let ingested = ingest_range_patch_to_cas(
        prefix_segment,
        req.into_body(),
        suffix_segment,
        &state.core.dedup_service,
        &filename,
        &claimed_type,
        PatchIngestBudget {
            max_bytes: max_upload,
            expected_body_len: end.map(|end| end - start + 1),
        },
    )
    .await?;

    // ── Quota enforcement + atomic store, compare-and-swap on the
    // pre-splice content hash ─────────────────────────────────────────
    // `file.content_hash` was snapshotted before the (potentially slow)
    // splice + CAS-ingest above. Passing it as `expected_hash` makes the
    // write itself a compare-and-swap: the repository checks and applies
    // under the same row lock, so nothing else can write to this file
    // between the check and the write. This is what actually closes the
    // race two concurrent PATCHes to disjoint ranges could otherwise hit
    // — each individually passing its own If-Match check against the
    // same stale snapshot, then blindly overwriting each other.
    let new_size = ingested.size;
    let stored = cas_write_patch(
        &state,
        upload_service,
        &internal_path,
        chroot.drive_id,
        &ingested,
        session.user.id,
        &file.content_hash,
        "NC WEBDAV PATCH",
    )
    .await?;

    // Everything from `start` to the new EOF reflects the patch (the
    // untouched suffix, if any, may have shifted when the body's length
    // differs from the replaced span).
    let range_end = new_size.saturating_sub(1);
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ETAG, format!("\"{}\"", stored.etag))
        .header("oc-etag", format!("\"{}\"", stored.etag))
        .header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, range_end, new_size),
        )
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── MKCOL ────────────────────

async fn handle_mkcol(
    state: Arc<AppState>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    let chroot = session.require_chroot()?;
    use crate::application::dtos::folder_dto::CreateFolderDto;

    let folder_service = &state.applications.folder_service;
    let internal_path = nc_to_internal_path(chroot, subpath)?;

    // RFC 4918 §9.3.1:
    //   - target already exists                                → 405 Method Not Allowed
    //   - parent collection of the target does NOT exist       → 409 Conflict
    //   - parent exists and target does not                    → 201 Created
    //
    // Previous behaviour effectively performed `mkdir -p` and returned
    // 201 even when intermediate ancestors were missing. Sabre/DAV and
    // the actual NC server both return 409 here, so the legacy
    // auto-create deviated from the reference implementation. NC desktop
    // walks ancestors one MKCOL at a time anyway, so dropping the
    // auto-create doesn't break real clients.

    if folder_service
        .get_folder_by_path(&internal_path, chroot.drive_id)
        .await
        .is_ok()
    {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .unwrap());
    }

    let segments: Vec<&str> = subpath.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(AppError::bad_request(
            "MKCOL on the user root is not allowed",
        ));
    }
    let (target_name, parent_segments) = segments.split_last().expect("checked non-empty above");

    // Take POC's `chroot`-based root resolution (drive-aware mount
    // point) but keep HEAD's parent_path lookup pattern — the
    // continuation below uses `get_folder_by_path(&parent_path,
    // user.id)` (user-scoped lookup added in the D0 rewind).
    let user_root = nc_to_internal_path(chroot, "")?;
    let parent_path = if parent_segments.is_empty() {
        user_root.clone()
    } else {
        format!("{}/{}", user_root, parent_segments.join("/"))
    };

    let parent_folder = match folder_service
        .get_folder_by_path(&parent_path, chroot.drive_id)
        .await
    {
        Ok(folder) => folder,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::CONFLICT)
                .body(Body::empty())
                .unwrap());
        }
    };

    let dto = CreateFolderDto {
        name: target_name.to_string(),
        parent_id: Some(parent_folder.id.clone()),
    };
    // AuthZ audit #7 (2026-07-12): route `_with_perms` errors through
    // `AppError::from` so authz denials surface as 404 (the anti-enum
    // shape) instead of a `map_err → internal_error` 500. Also preserves
    // `AlreadyExists → 409`, `QuotaExceeded → 507`, `InvalidInput → 400`.
    folder_service
        .create_folder_with_perms(dto, user.id)
        .await
        .map_err(AppError::from)?;

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── DELETE ────────────────────

async fn handle_delete(
    state: Arc<AppState>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    let chroot = session.require_chroot()?;
    let internal_path = nc_to_internal_path(chroot, subpath)?;
    let folder_service = &state.applications.folder_service;

    // Single-query path resolution. Post-D7 the resolver is drive-scoped,
    // so we `authz.require(Read, …)` on the returned resource before
    // dispatching. The actual delete is authorised as `Permission::Delete`
    // inside the downstream service (`trash_svc.move_to_trash` /
    // `delete_folder_with_perms` / `delete_file_with_perms` all take
    // `caller_id`).
    let resolved = nc_resolve_or_fallback(&state, &internal_path, chroot.drive_id)
        .await
        .ok_or_else(|| AppError::not_found("Resource not found"))?;

    match resolved {
        ResolvedResource::Folder(folder) => {
            let folder_uuid = Uuid::parse_str(&folder.id)
                .map_err(|_| AppError::not_found("Resource not found"))?;
            state
                .authorization
                .require(
                    Subject::User(user.id),
                    Permission::Read,
                    Resource::Folder(folder_uuid),
                )
                .await?;
            // AuthZ audit #8 (2026-07-12): route service errors through
            // `AppError::from` so authz denials surface as 404 (the
            // anti-enum shape) instead of a `map_err → internal_error`
            // 500 that gives a probing caller an "exists-but-denied"
            // oracle. `move_to_trash` and `delete_folder_with_perms`
            // both return `DomainError` and both call `authz.require`.
            if let Some(trash_svc) = state.trash_service.as_ref() {
                trash_svc
                    .move_to_trash(&folder.id, "folder", user.id)
                    .await
                    .map_err(AppError::from)?;
            } else {
                folder_service
                    .delete_folder_with_perms(&folder.id, user.id)
                    .await
                    .map_err(AppError::from)?;
            }
        }
        ResolvedResource::File(file) => {
            let file_uuid =
                Uuid::parse_str(&file.id).map_err(|_| AppError::not_found("Resource not found"))?;
            state
                .authorization
                .require(
                    Subject::User(user.id),
                    Permission::Read,
                    Resource::File(file_uuid),
                )
                .await?;
            // AuthZ audit #8 (2026-07-12): same anti-enum fix as folder branch above.
            if let Some(trash_svc) = state.trash_service.as_ref() {
                trash_svc
                    .move_to_trash(&file.id, "file", user.id)
                    .await
                    .map_err(AppError::from)?;
            } else {
                let file_mgmt = &state.applications.file_management_service;
                file_mgmt
                    .delete_file_with_perms(&file.id, user.id)
                    .await
                    .map_err(AppError::from)?;
            }
        }
    }

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── MOVE ────────────────────

async fn handle_move(
    state: Arc<AppState>,
    req: Request<Body>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    let chroot = session.require_chroot()?;
    let url_user = &session.raw_username;
    let destination = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::bad_request("Missing Destination header"))?
        .to_string();

    // RFC 4918 §9.9.3: the `Overwrite` header has the default value `T`.
    // `F` MUST cause the request to fail with 412 when the destination
    // already exists; `T` (or absent) MUST replace the destination as if
    // it didn't exist (the response then drops from 201 Created to 204
    // No Content per §9.9.4 because the URI's resource was replaced
    // rather than newly created).
    let overwrite_forbidden = req
        .headers()
        .get("overwrite")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("F"))
        .unwrap_or(false);

    // Parse destination path: extract subpath after /remote.php/dav/files/{user}/
    // — the URL user-segment carries the drive marker on multi-drive
    // sessions, so we strip the *composite* prefix to find the real
    // subpath. Using `user.username` here would fail to match for any
    // request hitting a non-home drive.
    let dest_subpath = extract_nc_subpath_from_dest(&destination, url_user)
        .ok_or_else(|| AppError::bad_request("Invalid Destination URL"))?;

    let src_internal = nc_to_internal_path(chroot, subpath)?;
    let folder_service = &state.applications.folder_service;
    let file_service = &state.applications.file_retrieval_service;
    let file_mgmt = &state.applications.file_management_service;

    // ── Destination-collision precondition (RFC 4918 §9.9.4) ──────────
    // Single-query probe via the shared resolver — the destination is
    // either a file, a folder, or absent. `dest_existed_before`
    // becomes the 204-vs-201 selector at response time. Post-D7 the
    // resolver is drive-scoped; on the overwrite path we
    // `authz.require(Read, …)` explicitly and the downstream delete
    // enforces `Permission::Delete`.
    let dest_internal_precheck = nc_to_internal_path(chroot, &dest_subpath)?;
    let dest_existing =
        nc_resolve_or_fallback(&state, &dest_internal_precheck, chroot.drive_id).await;
    let dest_existed_before = dest_existing.is_some();

    if let Some(existing) = dest_existing {
        if overwrite_forbidden {
            return Ok(Response::builder()
                .status(StatusCode::PRECONDITION_FAILED)
                .body(Body::empty())
                .unwrap());
        }
        // Overwrite: T (or absent) → delete the existing destination first,
        // then proceed with the move. Trashing is fine: per RFC the source
        // resource appears at the destination URI; what happens to the
        // overwritten one is up to the server.
        //
        // AuthZ audit #9 (2026-07-12): route the `_with_perms` delete
        // errors through `AppError::from` so authz denials surface as 404
        // (anti-enum) instead of `map_err → internal_error` 500. Also
        // preserves `QuotaExceeded → 507`, `AlreadyExists → 409`,
        // `InvalidInput → 400`.
        match existing {
            ResolvedResource::File(existing_file) => {
                let file_uuid = Uuid::parse_str(&existing_file.id).map_err(|_| {
                    AppError::internal_error("Failed to overwrite destination file")
                })?;
                state
                    .authorization
                    .require(
                        Subject::User(user.id),
                        Permission::Read,
                        Resource::File(file_uuid),
                    )
                    .await?;
                file_mgmt
                    .delete_and_cleanup_with_perms(&existing_file.id, user.id)
                    .await
                    .map_err(AppError::from)?;
            }
            ResolvedResource::Folder(existing_folder) => {
                let folder_uuid = Uuid::parse_str(&existing_folder.id).map_err(|_| {
                    AppError::internal_error("Failed to overwrite destination folder")
                })?;
                state
                    .authorization
                    .require(
                        Subject::User(user.id),
                        Permission::Read,
                        Resource::Folder(folder_uuid),
                    )
                    .await?;
                folder_service
                    .delete_folder_with_perms(&existing_folder.id, user.id)
                    .await
                    .map_err(AppError::from)?;
            }
        }
    }

    let final_status = if dest_existed_before {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    };

    // Try as file first.
    if let Ok(file) = file_service
        .get_file_by_path(&src_internal, chroot.drive_id)
        .await
    {
        let (dest_parent_sub, dest_name) = match dest_subpath.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", dest_subpath.as_str()),
        };
        let dest_parent_internal = nc_to_internal_path(chroot, dest_parent_sub)?;

        // Rename if only the name changes (same parent).
        let src_parent_sub = match subpath.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => "",
        };

        // AuthZ audit #9 (2026-07-12): route `_with_perms` errors
        // through `AppError::from` so authz denials surface as 404
        // (anti-enum) instead of `map_err → internal_error` 500.
        if src_parent_sub == dest_parent_sub {
            // Same parent → rename.
            file_mgmt
                .rename_file_with_perms(&file.id, user.id, dest_name)
                .await
                .map_err(AppError::from)?;
        } else {
            // Different parent → move.
            let dest_parent = folder_service
                .get_folder_by_path(&dest_parent_internal, chroot.drive_id)
                .await
                .map_err(|_| AppError::not_found("Destination folder not found"))?;

            file_mgmt
                .move_file_with_perms(&file.id, user.id, Some(dest_parent.id.clone()))
                .await
                .map_err(AppError::from)?;

            // If the filename changed too, rename after move.
            if file.name != dest_name {
                file_mgmt
                    .rename_file_with_perms(&file.id, user.id, dest_name)
                    .await
                    .map_err(AppError::from)?;
            }
        }

        // Return ETag and OC-ETag so Nextcloud clients can track the moved file.
        // Take POC's chroot-based path resolution; keep HEAD's
        // final_status (201 vs 204 depending on whether the destination
        // existed — RFC 4918 §9.9.4 distinguishes create vs overwrite).
        let dest_internal = nc_to_internal_path(chroot, &dest_subpath)?;
        let mut builder = Response::builder().status(final_status);
        if let Ok(moved) = file_service
            .get_file_by_path(&dest_internal, chroot.drive_id)
            .await
        {
            // Route through `FileDto::etag` so the MOVE response
            // matches what a subsequent PROPFIND on the destination
            // will return — `moved.id` (UUID) would differ from the
            // blob hash and trigger NC's "remote changed" detection.
            builder = builder
                .header(header::ETAG, format!("\"{}\"", moved.etag))
                .header("oc-etag", format!("\"{}\"", moved.etag));
        }

        return Ok(builder.body(Body::empty()).unwrap());
    }

    // Try as folder.
    if let Ok(folder) = folder_service
        .get_folder_by_path(&src_internal, chroot.drive_id)
        .await
    {
        let (dest_parent_sub, dest_name) = match dest_subpath.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", dest_subpath.as_str()),
        };
        let dest_parent_internal = nc_to_internal_path(chroot, dest_parent_sub)?;

        let src_parent_sub = match subpath.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => "",
        };

        // AuthZ audit #9 (2026-07-12): route `_with_perms` errors
        // through `AppError::from` so authz denials surface as 404
        // (anti-enum) instead of `map_err → internal_error` 500.
        if src_parent_sub == dest_parent_sub {
            // Same parent → rename.
            use crate::application::dtos::folder_dto::RenameFolderDto;
            folder_service
                .rename_folder_with_perms(
                    &folder.id,
                    RenameFolderDto {
                        name: dest_name.to_string(),
                    },
                    user.id,
                )
                .await
                .map_err(AppError::from)?;
        } else {
            // Different parent → move.
            let dest_parent = folder_service
                .get_folder_by_path(&dest_parent_internal, chroot.drive_id)
                .await
                .map_err(|_| AppError::not_found("Destination parent not found"))?;

            use crate::application::dtos::folder_dto::MoveFolderDto;
            folder_service
                .move_folder_with_perms(
                    &folder.id,
                    MoveFolderDto {
                        parent_id: Some(dest_parent.id.clone()),
                    },
                    user.id,
                )
                .await
                .map_err(AppError::from)?;

            // If the name changed too, rename.
            if folder.name != dest_name {
                use crate::application::dtos::folder_dto::RenameFolderDto;
                folder_service
                    .rename_folder_with_perms(
                        &folder.id,
                        RenameFolderDto {
                            name: dest_name.to_string(),
                        },
                        user.id,
                    )
                    .await
                    .map_err(AppError::from)?;
            }
        }

        return Ok(Response::builder()
            .status(final_status)
            .body(Body::empty())
            .unwrap());
    }

    Err(AppError::not_found("Source resource not found"))
}

/// Extract the subpath from a Destination header URL.
///
/// Only accepts relative paths or absolute URLs whose path starts with the
/// expected DAV prefix.  For full URLs the host is ignored — the path alone is
/// used — so an attacker cannot redirect the server to a different host.
pub fn extract_nc_subpath_from_dest(dest: &str, username: &str) -> Option<String> {
    let prefix = format!("/remote.php/dav/files/{}/", username);
    // For full URLs, extract the path portion (everything after the authority).
    let path = if dest.starts_with("http://") || dest.starts_with("https://") {
        // Find the start of the path after "scheme://host".
        let after_scheme = dest.split_once("://")?.1;
        let path_start = after_scheme.find('/').unwrap_or(after_scheme.len());
        &after_scheme[path_start..]
    } else {
        dest
    };
    let decoded = urlencoding::decode(path).ok()?;
    let decoded = decoded.trim_end_matches('/');
    decoded
        .strip_prefix(prefix.trim_end_matches('/'))
        .map(|s| s.trim_start_matches('/').to_string())
}

// ────────────── Nextcloud PROPFIND XML Generation ──────────────

use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::services::nextcloud_file_id_service::NextcloudFileIdService;

/// Write the `<d:multistatus>` opening tag with the full NC namespace set.
/// Shared by the streaming folder PROPFIND and the single-file variant so
/// the namespace list can never diverge between the two.
fn write_nc_multistatus_open<W: std::io::Write>(xml: &mut Writer<W>) -> Result<(), String> {
    let mut ms = BytesStart::new("d:multistatus");
    ms.push_attribute(("xmlns:d", "DAV:"));
    ms.push_attribute(("xmlns:oc", "http://owncloud.org/ns"));
    ms.push_attribute(("xmlns:nc", "http://nextcloud.org/ns"));
    ms.push_attribute(("xmlns:ocs", "http://open-collaboration-services.org/ns"));
    xml.write_event(Event::Start(ms)).xml_err()
}

/// Generate the multistatus XML for a single-file PROPFIND. The folder
/// case streams via [`build_nc_streaming_propfind`] instead.
///
/// `extras` bundles `(favorite_ids, dead_props)` — both are per-resource
/// decorations fetched by the caller — to stay under clippy's
/// argument-count lint.
async fn write_nc_file_multistatus<W: std::io::Write>(
    writer: W,
    file: &FileDto,
    url_user: &str,
    username: &str,
    subpath: &str,
    file_id_svc: Option<&Arc<NextcloudFileIdService>>,
    extras: (&HashSet<String>, &[(QualifiedName, Option<String>)]),
) -> Result<(), String> {
    let (favorite_ids, dead_props) = extras;
    let (file_id_map, _) = batch_resolve_ids(file_id_svc, &[file.id.as_str()], &[]).await;

    let mut xml = Writer::new(writer);
    write_nc_multistatus_open(&mut xml)?;

    // Single-file PROPFIND — subpath already points to the file.
    // `url_user` is the wire identifier (may carry a `~{drive}`
    // marker); the NC client validates that the returned `<d:href>`
    // shares the requested URL's prefix. `username` is the canonical
    // identity for the `oc:owner-id` field.
    let href = nc_href(url_user, subpath);
    let file_id = nc_id_of(&file_id_map, &file.id);
    let oc_id = file_id.map(|id| format_oc_id(id, file_id_svc));
    write_file_response(
        &mut xml,
        file,
        &href,
        (file_id, oc_id.as_deref()),
        username,
        favorite_ids,
        dead_props,
    )?;

    xml.write_event(Event::End(BytesEnd::new("d:multistatus")))
        .xml_err()?;

    Ok(())
}

/// Build a streaming 207 Multi-Status response for a folder PROPFIND.
///
/// Mirrors the native WebDAV handler's `build_streaming_propfind_response`:
/// children are fetched in pages of [`PROPFIND_BATCH_SIZE`], each page's
/// favorites and `oc:fileid`s are resolved with two batch queries, and the
/// XML is yielded chunk by chunk — memory stays O(batch) and the response
/// starts flowing immediately, instead of materializing the full listing
/// plus its entire multistatus (~2 KB/entry) in RAM before the first byte.
fn build_nc_streaming_propfind(
    state: Arc<AppState>,
    folder: FolderDto,
    depth: String,
    user_id: Uuid,
    username: String,
    subpath: String,
    quota: Option<(i64, Option<i64>)>,
) -> Response<Body> {
    let stream = async_stream::try_stream! {
        let file_id_svc = state.nextcloud.as_ref().map(|n| &n.file_ids);
        let fav_svc = state.favorites_service.as_ref();
        let folder_service = &state.applications.folder_service;
        let file_service = &state.applications.file_retrieval_service;

        // ── <d:multistatus> + the folder's own entry ─────────────────
        // Collection hrefs MUST end in `/` (RFC 4918 §5.2 + strict
        // NC-client enforcement — see `nc_collection_href`).
        // Same three independent reads as the per-page child triple below,
        // and on the critical path of EVERY folder PROPFIND's first byte —
        // overlapped with `join!` (ROUND10; the header trio was left serial
        // when ROUND9 converted the page loops).
        let folder_id_arr = [folder.id.as_str()];
        let (folder_favs, (_, folder_id_map), folder_dead) = tokio::join!(
            async {
                if let Some(fav) = fav_svc {
                    fav.batch_check_favorites(user_id, &[(folder.id.as_str(), "folder")])
                        .await
                        .unwrap_or_default()
                } else {
                    HashSet::new()
                }
            },
            batch_resolve_ids(file_id_svc, &[], &folder_id_arr),
            folder_dead_props(&state.webdav_dead_props, &folder),
        );

        let mut buf = Vec::with_capacity(4096);
        {
            let mut xml = Writer::new(&mut buf);
            write_nc_multistatus_open(&mut xml).map_err(std::io::Error::other)?;
            let href = nc_collection_href(&username, &subpath);
            let fid = nc_id_of(&folder_id_map, &folder.id);
            let oc_id = fid.map(|id| format_oc_id(id, file_id_svc));
            write_folder_response(&mut xml, &folder, &href, (fid, oc_id.as_deref()), &username, &folder_favs, quota, &folder_dead)
                .map_err(std::io::Error::other)?;
        }
        yield Bytes::from(buf);

        // ── Children (only if Depth != 0) ────────────────────────────
        if depth != "0" {
            // Encoded href prefix for every child: username + parent
            // path encode ONCE here — the old per-row `nc_href` call
            // re-split and re-encoded the constant prefix for each of
            // the up-to-500 children of every page.
            let child_href_prefix = {
                let base = nc_href(&username, &subpath);
                if base.ends_with('/') {
                    base
                } else {
                    format!("{base}/")
                }
            };

            // Files in pages (keyset cursor — O(page) per page instead of
            // the quadratic LIMIT/OFFSET walk).
            let mut after_name: Option<String> = None;
            loop {
                let batch = file_service
                    .list_files_batch_with_perms(
                        Some(&folder.id),
                        user_id,
                        after_name.as_deref(),
                        PROPFIND_BATCH_SIZE,
                    )
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                if batch.is_empty() {
                    break;
                }
                let batch_len = batch.len();

                // Per-page enrichment: favorites + oc:fileids + dead props —
                // three independent reads over the same id batch, overlapped
                // with `join!` so a page pays ~max(RTT) instead of 3×RTT
                // (each query still batched per page: DEAD-PROPS.md). The
                // round-7 deferred "serial pairs" item, adopted for this
                // per-page triple after the injected-latency A/B in
                // benches/ROUND9.md showed no local-PG regression.
                let fav_items: Vec<(&str, &str)> =
                    batch.iter().map(|f| (f.id.as_str(), "file")).collect();
                let file_uuids: Vec<&str> = batch.iter().map(|f| f.id.as_str()).collect();
                let (favs, (file_id_map, _), file_deads) = tokio::join!(
                    async {
                        if let Some(fav) = fav_svc {
                            fav.batch_check_favorites(user_id, &fav_items)
                                .await
                                .unwrap_or_default()
                        } else {
                            HashSet::new()
                        }
                    },
                    batch_resolve_ids(file_id_svc, &file_uuids, &[]),
                    files_dead_props_map(&state.webdav_dead_props, &batch),
                );

                let mut chunk = Vec::with_capacity(batch_len * 1024);
                {
                    let mut xml = Writer::new(&mut chunk);
                    // One href buffer reused across the page instead of a fresh
                    // format! String per child (benches/ROUND19.md §M6); likewise
                    // one oc:id buffer (benches/ROUND27.md §H1).
                    let mut href = String::new();
                    let mut oc_buf = String::new();
                    for file in batch.iter() {
                        let dead = dead_props_for(&file.id, &file_deads);
                        // Only the name varies per row — the encoded
                        // username + parent prefix is computed once
                        // outside the loops (the old `nc_href` call
                        // re-encoded both for every child).
                        href.clear();
                        href.push_str(&child_href_prefix);
                        href.push_str(&urlencoding::encode(&file.name));
                        let fid = nc_id_of(&file_id_map, &file.id);
                        let oc_id: Option<&str> = match fid {
                            Some(id) => {
                                format_oc_id_into(&mut oc_buf, id, file_id_svc);
                                Some(oc_buf.as_str())
                            }
                            None => None,
                        };
                        write_file_response(&mut xml, file, &href, (fid, oc_id), &username, &favs, dead)
                            .map_err(std::io::Error::other)?;
                    }
                }
                yield Bytes::from(chunk);

                if (batch_len as i64) < PROPFIND_BATCH_SIZE {
                    break;
                }
                after_name = batch.last().map(|f| f.name.clone());
            }

            // Subfolders in pages — also collections, same trailing-slash
            // rule. Keyset cursor: O(page) per page off
            // idx_folders_unique_name instead of the quadratic
            // COUNT(*) OVER() + LIMIT/OFFSET walk (benches/FOLDER-KEYSET.md).
            let mut after_folder: Option<String> = None;
            loop {
                let batch = folder_service
                    .list_folders_batch_with_perms(
                        Some(&folder.id),
                        user_id,
                        after_folder.as_deref(),
                        PROPFIND_BATCH_SIZE as usize,
                    )
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                if batch.is_empty() {
                    break;
                }

                // Same overlapped enrichment triple as the file pages above.
                let fav_items: Vec<(&str, &str)> =
                    batch.iter().map(|sf| (sf.id.as_str(), "folder")).collect();
                let folder_uuids: Vec<&str> = batch.iter().map(|sf| sf.id.as_str()).collect();
                let (favs, (_, sub_id_map), sub_deads) = tokio::join!(
                    async {
                        if let Some(fav) = fav_svc {
                            fav.batch_check_favorites(user_id, &fav_items)
                                .await
                                .unwrap_or_default()
                        } else {
                            HashSet::new()
                        }
                    },
                    batch_resolve_ids(file_id_svc, &[], &folder_uuids),
                    folders_dead_props_map(&state.webdav_dead_props, &batch),
                );

                let mut chunk = Vec::with_capacity(batch.len() * 1024);
                {
                    let mut xml = Writer::new(&mut chunk);
                    // One href buffer reused across the page (benches/ROUND19.md
                    // §M6); likewise one oc:id buffer (benches/ROUND27.md §H1).
                    let mut href = String::new();
                    let mut oc_buf = String::new();
                    for sf in batch.iter() {
                        let dead = dead_props_for(&sf.id, &sub_deads);
                        // Collections carry the trailing slash; prefix
                        // precomputed once like the file loop above.
                        href.clear();
                        href.push_str(&child_href_prefix);
                        href.push_str(&urlencoding::encode(&sf.name));
                        href.push('/');
                        let fid = nc_id_of(&sub_id_map, &sf.id);
                        let oc_id: Option<&str> = match fid {
                            Some(id) => {
                                format_oc_id_into(&mut oc_buf, id, file_id_svc);
                                Some(oc_buf.as_str())
                            }
                            None => None,
                        };
                        write_folder_response(&mut xml, sf, &href, (fid, oc_id), &username, &favs, quota, dead)
                            .map_err(std::io::Error::other)?;
                    }
                }
                let has_more = (batch.len() as i64) == PROPFIND_BATCH_SIZE;
                after_folder = batch.last().map(|sf| sf.name.clone());
                yield Bytes::from(chunk);

                if !has_more {
                    break;
                }
            }
        }

        // ── </d:multistatus> ─────────────────────────────────────────
        let mut buf = Vec::with_capacity(32);
        {
            let mut xml = Writer::new(&mut buf);
            xml.write_event(Event::End(BytesEnd::new("d:multistatus")))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        yield Bytes::from(buf);
    };

    use futures::TryStreamExt;
    let stream = stream
        .map_err(|e: std::io::Error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) });

    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// `oc_ids` bundles `(file_id, oc_id)` — always fetched and passed
/// together (`oc_id` is derived from `file_id`) — to stay under
/// clippy's argument-count lint now that `dead_props` is also threaded
/// through.
#[allow(clippy::too_many_arguments)]
pub fn write_folder_response<W: std::io::Write>(
    xml: &mut Writer<W>,
    folder: &FolderDto,
    href: &str,
    oc_ids: (Option<i64>, Option<&str>),
    owner: &str,
    favorite_ids: &HashSet<String>,
    quota: Option<(i64, Option<i64>)>,
    dead_props: &[(QualifiedName, Option<String>)],
) -> Result<(), String> {
    let (file_id, oc_id) = oc_ids;
    xml.write_event(Event::Start(BytesStart::new("d:response")))
        .xml_err()?;

    // href
    write_text_element(xml, "d:href", href)?;

    xml.write_event(Event::Start(BytesStart::new("d:propstat")))
        .xml_err()?;
    xml.write_event(Event::Start(BytesStart::new("d:prop")))
        .xml_err()?;

    // resourcetype
    xml.write_event(Event::Start(BytesStart::new("d:resourcetype")))
        .xml_err()?;
    xml.write_event(Event::Empty(BytesStart::new("d:collection")))
        .xml_err()?;
    xml.write_event(Event::End(BytesEnd::new("d:resourcetype")))
        .xml_err()?;

    write_text_element(xml, "d:displayname", &folder.name)?;

    write_date_element(
        xml,
        "d:getlastmodified",
        timestamp_to_i64(folder.modified_at),
        true,
    )?;
    // Route through `FolderDto::etag` (= `Folder::etag()`: the
    // descendant-aware `{id[..16]}-{tree_modified_at}` — see the
    // entity for the formula and the async-bump freshness contract).
    write_etag_element(xml, "d:getetag", &folder.etag)?;
    write_text_element(xml, "d:getcontenttype", "httpd/unix-directory")?;
    write_text_element(xml, "d:getcontentlength", "0")?;
    write_date_element(
        xml,
        "d:creationdate",
        timestamp_to_i64(folder.created_at),
        false,
    )?;

    // Nextcloud/ownCloud properties
    if let Some(id) = file_id {
        let mut buf = [0u8; 21];
        write_text_element(xml, "oc:fileid", crate::common::fmt::i64_str(&mut buf, id))?;
    }
    if let Some(oid) = oc_id {
        write_text_element(xml, "oc:id", oid)?;
    }
    write_text_element(xml, "oc:permissions", "RGDNVCK")?;
    // Numeric share-permissions bitmask: Read=1 + Update=2 + Create=4 + Delete=8 + Share=16 = 31
    write_text_element(xml, "ocs:share-permissions", "31")?;
    write_text_element(xml, "oc:size", "0")?;
    // RFC 4331 — same account/drive-wide value regardless of which
    // folder entry is being described, mirroring the native WebDAV
    // surface's `write_folder_standard_props` (see
    // `AppState::resolve_webdav_quota`).
    if let Some((used, available)) = quota {
        let mut buf = [0u8; 21];
        write_text_element(
            xml,
            "d:quota-used-bytes",
            crate::common::fmt::i64_str(&mut buf, used),
        )?;
        if let Some(avail) = available {
            write_text_element(
                xml,
                "d:quota-available-bytes",
                crate::common::fmt::i64_str(&mut buf, avail),
            )?;
        }
    }
    write_text_element(xml, "oc:owner-id", owner)?;
    write_text_element(xml, "oc:owner-display-name", owner)?;
    write_text_element(xml, "nc:has-preview", "false")?;
    write_text_element(xml, "nc:is-encrypted", "0")?;
    write_text_element(xml, "nc:mount-type", "")?;

    let is_fav = if favorite_ids.contains(&folder.id) {
        "1"
    } else {
        "0"
    };
    write_text_element(xml, "oc:favorite", is_fav)?;
    // Empty share-types (no sharing API yet)
    xml.write_event(Event::Empty(BytesStart::new("oc:share-types")))
        .xml_err()?;

    xml.write_event(Event::End(BytesEnd::new("d:prop")))
        .xml_err()?;
    write_text_element(xml, "d:status", "HTTP/1.1 200 OK")?;
    xml.write_event(Event::End(BytesEnd::new("d:propstat")))
        .xml_err()?;

    WebDavAdapter::write_dead_props_propstat(xml, dead_props).xml_err()?;

    xml.write_event(Event::End(BytesEnd::new("d:response")))
        .xml_err()?;

    Ok(())
}

/// See `write_folder_response` for why `(file_id, oc_id)` are bundled
/// into `oc_ids`.
pub fn write_file_response<W: std::io::Write>(
    xml: &mut Writer<W>,
    file: &FileDto,
    href: &str,
    oc_ids: (Option<i64>, Option<&str>),
    owner: &str,
    favorite_ids: &HashSet<String>,
    dead_props: &[(QualifiedName, Option<String>)],
) -> Result<(), String> {
    let (file_id, oc_id) = oc_ids;
    xml.write_event(Event::Start(BytesStart::new("d:response")))
        .xml_err()?;

    write_text_element(xml, "d:href", href)?;

    xml.write_event(Event::Start(BytesStart::new("d:propstat")))
        .xml_err()?;
    xml.write_event(Event::Start(BytesStart::new("d:prop")))
        .xml_err()?;

    // resourcetype (empty for files)
    xml.write_event(Event::Empty(BytesStart::new("d:resourcetype")))
        .xml_err()?;

    write_text_element(xml, "d:displayname", &file.name)?;
    write_text_element(xml, "d:getcontenttype", &file.mime_type)?;
    {
        let mut buf = [0u8; 20];
        write_text_element(
            xml,
            "d:getcontentlength",
            crate::common::fmt::u64_str(&mut buf, file.size),
        )?;
    }

    write_date_element(
        xml,
        "d:getlastmodified",
        timestamp_to_i64(file.modified_at),
        true,
    )?;
    write_etag_element(xml, "d:getetag", &file.etag)?;
    write_date_element(
        xml,
        "d:creationdate",
        timestamp_to_i64(file.created_at),
        false,
    )?;

    // Nextcloud/ownCloud properties
    if let Some(id) = file_id {
        let mut buf = [0u8; 21];
        write_text_element(xml, "oc:fileid", crate::common::fmt::i64_str(&mut buf, id))?;
    }
    if let Some(oid) = oc_id {
        write_text_element(xml, "oc:id", oid)?;
    }
    write_text_element(xml, "oc:permissions", "RGDNVW")?;
    // Numeric share-permissions bitmask: Read=1 + Update=2 + Delete=8 + Share=16 = 27
    write_text_element(xml, "ocs:share-permissions", "27")?;
    {
        let mut buf = [0u8; 20];
        write_text_element(
            xml,
            "oc:size",
            crate::common::fmt::u64_str(&mut buf, file.size),
        )?;
    }
    write_text_element(xml, "oc:owner-id", owner)?;
    write_text_element(xml, "oc:owner-display-name", owner)?;

    let is_fav = if favorite_ids.contains(&file.id) {
        "1"
    } else {
        "0"
    };
    write_text_element(xml, "oc:favorite", is_fav)?;
    // Empty share-types (no sharing API yet)
    xml.write_event(Event::Empty(BytesStart::new("oc:share-types")))
        .xml_err()?;

    // Check if file is an image that can have previews
    let has_preview = matches!(
        &*file.mime_type,
        "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/webp"
    );
    write_text_element(
        xml,
        "nc:has-preview",
        if has_preview { "true" } else { "false" },
    )?;

    write_text_element(xml, "nc:is-encrypted", "0")?;
    write_text_element(xml, "nc:mount-type", "")?;
    {
        let mut buf = [0u8; 20];
        write_text_element(
            xml,
            "nc:creation_time",
            crate::common::fmt::u64_str(&mut buf, file.created_at),
        )?;
        write_text_element(
            xml,
            "nc:upload_time",
            crate::common::fmt::u64_str(&mut buf, file.modified_at),
        )?;
    }

    xml.write_event(Event::End(BytesEnd::new("d:prop")))
        .xml_err()?;
    write_text_element(xml, "d:status", "HTTP/1.1 200 OK")?;
    xml.write_event(Event::End(BytesEnd::new("d:propstat")))
        .xml_err()?;

    WebDavAdapter::write_dead_props_propstat(xml, dead_props).xml_err()?;

    xml.write_event(Event::End(BytesEnd::new("d:response")))
        .xml_err()?;

    Ok(())
}

/// Stack-rendered `d:getlastmodified` / `d:creationdate` bodies
/// (`common::fmt`) — the old per-row `to_rfc2822()` / `to_rfc3339()`
/// ran chrono's format interpreter and allocated a String each.
/// Out-of-range timestamps keep the chrono path, byte-identical.
pub fn write_date_element<W: std::io::Write>(
    xml: &mut Writer<W>,
    tag: &str,
    secs: i64,
    rfc2822: bool,
) -> Result<(), String> {
    if rfc2822 {
        let mut buf = [0u8; 31];
        if let Some(s) = crate::common::fmt::rfc2822_utc(&mut buf, secs) {
            return write_text_element(xml, tag, s);
        }
        let dt = chrono::DateTime::<Utc>::from_timestamp(secs, 0).unwrap_or_else(Utc::now);
        write_text_element(xml, tag, &dt.to_rfc2822())
    } else {
        let mut buf = [0u8; 25];
        if let Some(s) = crate::common::fmt::rfc3339_utc(&mut buf, secs) {
            return write_text_element(xml, tag, s);
        }
        let dt = chrono::DateTime::<Utc>::from_timestamp(secs, 0).unwrap_or_else(Utc::now);
        write_text_element(xml, tag, &dt.to_rfc3339())
    }
}

/// `d:getetag` with the HTTP quoting — zero allocations.
///
/// The two `"` quotes are emitted as borrowed pre-escaped text events around
/// the escaped etag body. `quick_xml` renders a literal `"` as `&quot;`, so
/// this is byte-identical to escaping `"{etag}"` as one owned string — but with
/// no `with_capacity` quoted String and no escape re-allocation (the whole-string
/// escape re-allocated an owned Cow because the string contained `"`). On a
/// 500-child PROPFIND page this is called per file AND per folder row
/// (benches/ROUND20.md §C1: 3 → 0 allocs/row).
pub fn write_etag_element<W: std::io::Write>(
    xml: &mut Writer<W>,
    tag: &str,
    etag: &str,
) -> Result<(), String> {
    xml.write_event(Event::Start(BytesStart::new(tag)))
        .xml_err()?;
    xml.write_event(Event::Text(BytesText::from_escaped("&quot;")))
        .xml_err()?;
    xml.write_event(Event::Text(BytesText::new(etag)))
        .xml_err()?;
    xml.write_event(Event::Text(BytesText::from_escaped("&quot;")))
        .xml_err()?;
    xml.write_event(Event::End(BytesEnd::new(tag))).xml_err()?;
    Ok(())
}

pub fn write_text_element<W: std::io::Write>(
    xml: &mut Writer<W>,
    tag: &str,
    value: &str,
) -> Result<(), String> {
    xml.write_event(Event::Start(BytesStart::new(tag)))
        .xml_err()?;
    xml.write_event(Event::Text(BytesText::new(value)))
        .xml_err()?;
    xml.write_event(Event::End(BytesEnd::new(tag))).xml_err()?;
    Ok(())
}

/// Resolve every `oc:fileid` for a listing in two batch queries (one per
/// object type) instead of one INSERT round-trip per child. Returns
/// `(file_map, folder_map)` keyed by parsed object UUID; entries are absent
/// when the service is disabled or an id can't be resolved, mirroring the
/// previous per-call `Option` behaviour. The two batches run concurrently.
/// Borrowed inputs + `Uuid` keys keep the whole resolution alloc-free.
pub async fn batch_resolve_ids(
    svc: Option<&Arc<NextcloudFileIdService>>,
    file_uuids: &[&str],
    folder_uuids: &[&str],
) -> (HashMap<Uuid, i64>, HashMap<Uuid, i64>) {
    let Some(svc) = svc else {
        return (HashMap::new(), HashMap::new());
    };
    let (files, folders) = tokio::join!(
        svc.get_or_create_file_ids(file_uuids),
        svc.get_or_create_folder_ids(folder_uuids),
    );
    (files.unwrap_or_default(), folders.unwrap_or_default())
}

/// Look up a batch-resolved `oc:fileid` by a DTO's string UUID.
pub fn nc_id_of(map: &HashMap<Uuid, i64>, id: &str) -> Option<i64> {
    Uuid::parse_str(id).ok().and_then(|u| map.get(&u).copied())
}

pub fn format_oc_id(id: i64, svc: Option<&Arc<NextcloudFileIdService>>) -> String {
    match svc {
        Some(s) => s.format_oc_id(id),
        None => format!("{:08}ocnca", id),
    }
}

/// Write `oc:id` (`{:08}{instance_id}`) into a caller-provided buffer reused
/// across a PROPFIND/REPORT page — the 0-alloc form of [`format_oc_id`] for the
/// emit loops, replacing a fresh `String` per child (benches/ROUND27.md §H1).
/// Output is byte-identical to `format_oc_id`.
pub fn format_oc_id_into(out: &mut String, id: i64, svc: Option<&Arc<NextcloudFileIdService>>) {
    use std::fmt::Write as _;
    out.clear();
    let _ = write!(out, "{id:08}");
    out.push_str(svc.map(|s| s.instance_id()).unwrap_or("ocnca"));
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── nc_to_internal_path ──
    //
    // The route glue resolves the `chroot` FolderDto once per request
    // (legacy/home → user's home folder DTO; explicit `~{folder_uuid}` →
    // folder's stored DTO after permission check). These tests cover only
    // the path-mapping function itself; the resolver logic lives in
    // `routes.rs::verify_url_user_and_resolve_chroot`.

    /// Build a stub `FolderDto` carrying only the `path` field (all the
    /// path mapper looks at). Keeps the tests focused on path mapping
    /// without dragging in folder-construction machinery.
    fn stub_folder(path: &str) -> FolderDto {
        FolderDto {
            id: "00000000-0000-0000-0000-000000000000".to_string(),
            name: path.rsplit('/').next().unwrap_or("").to_string(),
            path: path.to_string(),
            parent_id: None,
            // Test stub — path mapper doesn't read drive_id.
            drive_id: uuid::Uuid::nil(),
            created_at: 0,
            modified_at: 0,
            is_root: false,
            icon_class: std::sync::Arc::from("fas fa-folder"),
            icon_special_class: std::sync::Arc::from("folder-icon"),
            category: std::sync::Arc::from("Folder"),
            etag: String::new(),
            // §14 provenance not relevant to path-mapper tests.
            created_by: None,
            updated_by: None,
            // Caller state flags not read by the WebDAV path mapper;
            // Nextcloud DAV surfaces don't render the SPA badges.
            is_favorite: false,
            is_shared: false,
        }
    }

    #[test]
    fn test_empty_subpath_returns_chroot() {
        let home = stub_folder("My Folder - alice");
        assert_eq!(nc_to_internal_path(&home, "").unwrap(), "My Folder - alice");
    }

    #[test]
    fn test_subpath_appended_to_chroot() {
        let home = stub_folder("My Folder - alice");
        assert_eq!(
            nc_to_internal_path(&home, "Documents/work").unwrap(),
            "My Folder - alice/Documents/work"
        );
    }

    #[test]
    fn test_strips_surrounding_slashes() {
        let home = stub_folder("My Folder - alice");
        assert_eq!(
            nc_to_internal_path(&home, "/Photos/").unwrap(),
            "My Folder - alice/Photos"
        );
    }

    /// Regression: `chroot.path` as returned by `folder_service.get_folder`
    /// in production carries a leading `/` (from `StoragePath::to_string()`
    /// — see `Folder::path_string`), unlike this module's `stub_folder`
    /// test helper which builds the path directly. A real chroot must
    /// still map to the leading-slash-free convention the plain WebDAV
    /// surface's `db_path` uses, or exact-string comparisons against it
    /// (e.g. the WebDAV lock store's key) silently mismatch.
    #[test]
    fn test_strips_leading_slash_from_chroot_path() {
        let home = stub_folder("/Personal");
        assert_eq!(
            nc_to_internal_path(&home, "report.pdf").unwrap(),
            "Personal/report.pdf"
        );
        assert_eq!(nc_to_internal_path(&home, "").unwrap(), "Personal");
    }

    #[test]
    fn test_rejects_dot_dot_traversal() {
        let home = stub_folder("My Folder - alice");
        assert!(nc_to_internal_path(&home, "../etc/passwd").is_err());
    }

    #[test]
    fn test_rejects_single_dot() {
        let home = stub_folder("My Folder - alice");
        assert!(nc_to_internal_path(&home, "foo/./bar").is_err());
    }

    /// Confines a subfolder chroot (the multi-drive form once
    /// resolved). Same path-mapping logic — only the chroot differs.
    #[test]
    fn test_subfolder_chroot_with_subpath() {
        let chroot = stub_folder("My Folder - alice/ext");
        assert_eq!(
            nc_to_internal_path(&chroot, "report.pdf").unwrap(),
            "My Folder - alice/ext/report.pdf"
        );
    }

    // ── strip_chroot_prefix ──
    //
    // Regression guard for the "chroot.path has a leading slash from
    // StoragePath::to_string() but DB-side original_path doesn't" trap
    // that broke the NC trashbin PROPFIND after Round 2 rolled out.
    // Also pins the composed-chroot behaviour Ed asked about.

    #[test]
    fn strip_chroot_prefix_default_drive_root() {
        // FolderDto.path carries a leading slash (StoragePath Display);
        // DB paths do not. Both must normalise to the same prefix.
        let chroot = stub_folder("/Personal");
        assert_eq!(
            strip_chroot_prefix(&chroot, "Personal/g9-tree"),
            Some("g9-tree")
        );
    }

    #[test]
    fn strip_chroot_prefix_deep_path() {
        let chroot = stub_folder("/Personal");
        assert_eq!(
            strip_chroot_prefix(&chroot, "Personal/inner/deep.txt"),
            Some("inner/deep.txt")
        );
    }

    #[test]
    fn strip_chroot_prefix_out_of_chroot_returns_none() {
        // Items on a different drive (whose root isn't "Personal")
        // must NOT be surfaced under the caller's chroot.
        let chroot = stub_folder("/Personal");
        assert_eq!(strip_chroot_prefix(&chroot, "team-drive/report.pdf"), None);
    }

    #[test]
    fn strip_chroot_prefix_rejects_partial_prefix_match() {
        // "Personal" is a prefix substring of "PersonalSecrets" but
        // NOT a path-segment prefix — must reject.
        let chroot = stub_folder("/Personal");
        assert_eq!(
            strip_chroot_prefix(&chroot, "PersonalSecrets/foo.txt"),
            None
        );
    }

    #[test]
    fn strip_chroot_prefix_composed_chroot() {
        // The future composed-chroot case Ed raised: chroot points at
        // a subfolder inside a drive. The strip must remove the ENTIRE
        // composed prefix, not just the first segment.
        let chroot = stub_folder("/Personal/folderA/subfolder");
        assert_eq!(
            strip_chroot_prefix(&chroot, "Personal/folderA/subfolder/foo.txt"),
            Some("foo.txt")
        );
    }

    #[test]
    fn strip_chroot_prefix_composed_chroot_sibling_leaks_blocked() {
        // Same composed chroot, but the item lives in a sibling
        // subfolder — must be rejected, not naively strip 1 segment.
        let chroot = stub_folder("/Personal/folderA/subfolder");
        assert_eq!(
            strip_chroot_prefix(&chroot, "Personal/folderA/other/foo.txt"),
            None
        );
    }

    #[test]
    fn strip_chroot_prefix_chroot_root_itself() {
        // Item path equals chroot exactly — legitimate for a PROPFIND
        // Depth:0 on the chroot itself. Subpath is empty.
        let chroot = stub_folder("/Personal");
        assert_eq!(strip_chroot_prefix(&chroot, "Personal"), Some(""));
    }

    #[test]
    fn strip_chroot_prefix_empty_chroot_returns_none() {
        // Defensive: a mis-set chroot with an empty path must not
        // strip anything (stripping "" from any path would return
        // the whole path — a silent leak).
        let chroot = stub_folder("/");
        assert_eq!(strip_chroot_prefix(&chroot, "Personal/foo.txt"), None);
    }

    // ── nc_href ──

    #[test]
    fn test_href_root() {
        assert_eq!(nc_href("alice", ""), "/remote.php/dav/files/alice/");
    }

    #[test]
    fn test_href_encodes_spaces() {
        assert_eq!(
            nc_href("alice", "My Photos/vacation pic.jpg"),
            "/remote.php/dav/files/alice/My%20Photos/vacation%20pic.jpg"
        );
    }

    #[test]
    fn test_href_encodes_special_chars() {
        let href = nc_href("alice", "file#1.txt");
        assert!(href.contains("file%231.txt"));
    }

    // ── nc_collection_href ──
    // RFC 4918 §5.2 requires a collection URL to end in '/'. The NC
    // desktop client at `networkjobs.cpp:234` aborts the PROPFIND
    // parse with `Invalid href "<…>" expected starting with
    // "<requested-url>"` if the own-entry href is missing the slash.
    // These tests pin the helper's behaviour so the regression can't
    // come back silently.

    #[test]
    fn test_collection_href_appends_slash_when_missing() {
        assert_eq!(
            nc_collection_href("alice", "ext"),
            "/remote.php/dav/files/alice/ext/"
        );
    }

    #[test]
    fn test_collection_href_idempotent_at_root() {
        // Root subpath already ends in '/' — don't double-append.
        assert_eq!(
            nc_collection_href("alice", ""),
            "/remote.php/dav/files/alice/"
        );
    }

    #[test]
    fn test_collection_href_preserves_encoding() {
        // Wrapping must not re-encode or double-encode already-encoded
        // segments.
        assert_eq!(
            nc_collection_href("alice", "My Photos/2024"),
            "/remote.php/dav/files/alice/My%20Photos/2024/"
        );
    }

    // ── extract_nc_subpath_from_dest ──

    #[test]
    fn test_extract_relative_path() {
        let result = extract_nc_subpath_from_dest(
            "/remote.php/dav/files/alice/Documents/moved.txt",
            "alice",
        );
        assert_eq!(result.as_deref(), Some("Documents/moved.txt"));
    }

    #[test]
    fn test_extract_absolute_url() {
        let result = extract_nc_subpath_from_dest(
            "https://cloud.example.com/remote.php/dav/files/alice/new.txt",
            "alice",
        );
        assert_eq!(result.as_deref(), Some("new.txt"));
    }

    #[test]
    fn test_extract_url_encoded() {
        let result = extract_nc_subpath_from_dest(
            "/remote.php/dav/files/alice/My%20Folder/file.txt",
            "alice",
        );
        assert_eq!(result.as_deref(), Some("My Folder/file.txt"));
    }

    #[test]
    fn test_extract_wrong_user_returns_none() {
        let result = extract_nc_subpath_from_dest("/remote.php/dav/files/bob/secret.txt", "alice");
        assert!(result.is_none());
    }

    // ── timestamp_to_i64 ──

    #[test]
    fn test_timestamp_normal() {
        assert_eq!(timestamp_to_i64(1700000000), 1700000000i64);
    }

    #[test]
    fn test_timestamp_overflow_returns_zero() {
        assert_eq!(timestamp_to_i64(u64::MAX), 0);
    }
}
