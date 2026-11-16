use axum::{
    body::Body,
    http::{HeaderName, Request, StatusCode, header},
    response::Response,
};
use quick_xml::{
    Writer,
    events::{BytesEnd, BytesStart, Event},
};
use std::sync::Arc;

use crate::application::ports::file_ports::FileRetrievalUseCase;
use crate::application::ports::folder_ports::FolderUseCase;
use crate::application::ports::trash_ports::TrashUseCase;
use crate::common::di::AppState;
use crate::interfaces::errors::AppError;
use crate::interfaces::nextcloud::webdav_handler::{
    batch_resolve_ids, extract_nc_subpath_from_dest, format_oc_id, nc_id_of, nc_to_internal_path,
    write_date_element, write_etag_element, write_text_element,
};

const HEADER_DAV: HeaderName = HeaderName::from_static("dav");

/// Dispatch Nextcloud WebDAV trashbin request to the appropriate handler.
///
/// `subpath` is everything after `/remote.php/dav/trashbin/{user}/`.
pub async fn handle_nc_trashbin(
    state: Arc<AppState>,
    req: Request<Body>,
    session: crate::interfaces::nextcloud::session::SharedNcSession,
    subpath: String,
) -> Result<Response<Body>, AppError> {
    let method = req.method().clone();
    let subpath_trimmed = subpath.trim_matches('/');

    match method.as_str() {
        "OPTIONS" => handle_options(),
        "PROPFIND" if subpath_trimmed == "trash" || subpath_trimmed.is_empty() => {
            handle_propfind(state, &session).await
        }
        "MOVE" if subpath_trimmed.starts_with("trash/") => {
            // Keep the destination-collision-check feature added on HEAD
            // (RFC 4918 §9.9.4: refuse restore with 412 when the
            // destination is taken by a live resource). The chroot lookup
            // moves into `handle_restore` via the session.
            let dest_header = req
                .headers()
                .get("destination")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            handle_restore(state, dest_header, &session, subpath_trimmed).await
        }
        "DELETE" if subpath_trimmed == "trash" || subpath_trimmed.is_empty() => {
            handle_empty_trash(state, &session).await
        }
        "DELETE" if subpath_trimmed.starts_with("trash/") => {
            handle_delete_permanent(state, &session, subpath_trimmed).await
        }
        _ => Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .unwrap()),
    }
}

// ──────────────────── OPTIONS ────────────────────

fn handle_options() -> Result<Response<Body>, AppError> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(HEADER_DAV, "1, 2, 3")
        .header(header::ALLOW, "OPTIONS, PROPFIND, MOVE, DELETE")
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── PROPFIND (list trash) ────────────────────

async fn handle_propfind(
    state: Arc<AppState>,
    session: &crate::interfaces::nextcloud::session::NcSession,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    // Chroot-scope the trashbin view: `get_trash_items(user.id)`
    // spans every drive the caller is a member of, but NC's
    // trashbin surface is a single-drive concept from the client's
    // POV. Items outside the chroot are dropped from the multistatus
    // (see `write_trashbin_multistatus` → `strip_home_prefix` →
    // `webdav_handler::strip_chroot_prefix`) and remain reachable
    // via REST `/api/trash/resources`.
    let chroot = session.require_chroot()?;
    let trash_svc = state
        .trash_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Trash service not available"))?;

    let items = trash_svc
        .get_trash_items(user.id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to list trash: {}", e)))?;

    let nc = state.nextcloud.as_ref();
    let file_id_svc = nc.map(|n| &n.file_ids);

    // Emit hrefs with `session.raw_username` (composite `admin~<uuid>` on
    // non-home drives), NOT `user.username` (bare `admin`). The
    // `NcSession` extractor cross-checks the URL `{user}` segment
    // against `raw_username` and 403s on mismatch (see
    // `session.rs::from_request_parts`). Emitting the bare form here
    // would make every follow-up MOVE/DELETE from a non-home client
    // 403 before the handler runs — the composite-credential Hurl
    // regression caught this (B5 in `nc_multidrive_move_regression`).
    let mut buf = Vec::new();
    write_trashbin_multistatus(&mut buf, &items, &session.raw_username, chroot, file_id_svc)
        .await
        .map_err(|e| AppError::internal_error(format!("XML generation failed: {}", e)))?;

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(buf))
        .unwrap())
}

// ──────────────────── MOVE (restore) ────────────────────

async fn handle_restore(
    state: Arc<AppState>,
    dest_header: Option<String>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    let chroot = session.require_chroot()?;
    let id = extract_trash_id(subpath)?;

    let trash_svc = state
        .trash_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Trash service not available"))?;

    // RFC 4918 §9.9.4 + Sabre convention: clients send `Destination` to
    // tell the server where the restored item should land. We don't yet
    // honor it for relocation (restore always lands at the original
    // path), but we DO honor it for the collision check: if the requested
    // destination is taken by a live resource the move must be refused
    // with 412 — there is no `Overwrite: T` workflow for trash restore in
    // either Sabre/DAV or the NC desktop client (a live file being
    // silently replaced by an undeleted one would be a footgun).
    // Use `session.raw_username` (composite `admin~<drive-uuid>` on
    // non-home drives) to strip the destination prefix, NOT
    // `user.username` (bare `admin`). NC clients send `Destination:
    // /remote.php/dav/files/{raw_username}/…`; passing the bare
    // username would leave the `~<uuid>/` marker glued to the leading
    // subpath segment and turn the collision-check into a lookup at
    // a fabricated path. See `uploads_handler::handle_assemble` for
    // the same fix in the chunked-upload MOVE.
    if let Some(dest_header) = dest_header
        && let Some(dest_subpath) =
            extract_nc_subpath_from_dest(&dest_header, &session.raw_username)
    {
        let dest_internal = nc_to_internal_path(chroot, &dest_subpath)?;
        let folder_service = &state.applications.folder_service;
        let file_service = &state.applications.file_retrieval_service;
        let dest_taken = file_service
            .get_file_by_path(&dest_internal, chroot.drive_id)
            .await
            .is_ok()
            || folder_service
                .get_folder_by_path(&dest_internal, chroot.drive_id)
                .await
                .is_ok();
        if dest_taken {
            return Ok(Response::builder()
                .status(StatusCode::PRECONDITION_FAILED)
                .body(Body::empty())
                .unwrap());
        }
    }

    match trash_svc.restore_item(&id, user.id).await {
        Ok(()) => Ok(Response::builder()
            .status(StatusCode::CREATED)
            .body(Body::empty())
            .unwrap()),
        Err(e) => {
            // Collision at the original path — a live file/folder is sitting
            // where the trashed one wants to come back to. Mirrors the G4/G5
            // semantics in webdav_handler::handle_move ("Overwrite: F to an
            // existing path → 412"); restore has no Overwrite header so the
            // refusal is unconditional. The caller can resolve by renaming
            // or trashing the conflicting live resource first.
            //
            // We string-match for the unique-index / duplicate-key signature
            // because restore_item currently re-wraps every storage error as
            // InternalError, so the original DomainError::AlreadyExists kind
            // is not propagated. A follow-up should thread the kind through
            // and let this be a kind-based check.
            let msg = format!("{}", e);
            if msg.contains("duplicate key")
                || msg.contains("unique constraint")
                || msg.to_ascii_lowercase().contains("already exists")
            {
                return Ok(Response::builder()
                    .status(StatusCode::PRECONDITION_FAILED)
                    .body(Body::empty())
                    .unwrap());
            }
            Err(AppError::internal_error(format!(
                "Failed to restore item: {}",
                e
            )))
        }
    }
}

// ──────────────────── DELETE (empty trash) ────────────────────

async fn handle_empty_trash(
    state: Arc<AppState>,
    session: &crate::interfaces::nextcloud::session::NcSession,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    let trash_svc = state
        .trash_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Trash service not available"))?;

    trash_svc
        .empty_trash(user.id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to empty trash: {}", e)))?;

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── DELETE (single item) ────────────────────

async fn handle_delete_permanent(
    state: Arc<AppState>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    let id = extract_trash_id(subpath)?;

    let trash_svc = state
        .trash_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Trash service not available"))?;

    trash_svc
        .delete_permanently(&id, user.id)
        .await
        .map_err(|e| {
            AppError::internal_error(format!("Failed to permanently delete item: {}", e))
        })?;

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}

// ────────────── Helpers ──────────────

/// Extract the item ID from a trashbin subpath like `trash/{id}`.
fn extract_trash_id(subpath: &str) -> Result<String, AppError> {
    // subpath is already trimmed, e.g. "trash/some-uuid"
    subpath
        .strip_prefix("trash/")
        .map(|s| s.trim_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad_request("Missing trash item ID in path"))
}

/// Infer MIME content type from filename extension.
fn mime_from_name(name: &str) -> String {
    mime_guess::from_path(name)
        .first_or_octet_stream()
        .to_string()
}

/// Strip the caller's chroot prefix from an original path to produce
/// the Nextcloud-relative original-location value.
///
/// Delegates to `webdav_handler::strip_chroot_prefix` — chroot-aware,
/// multi-segment safe, and returns `None` when the item is outside
/// the chroot (e.g. a trashed item in another drive the caller is a
/// member of). The `_username` arg stays for signature stability
/// with call sites that thread it; the strip itself no longer uses it.
///
/// See the doc on `strip_chroot_prefix` for the AuthZ caveat — this
/// is a display helper, not an ownership check.
fn strip_home_prefix<'a>(
    original_path: &'a str,
    _username: &str,
    chroot: &crate::application::dtos::folder_dto::FolderDto,
) -> Option<&'a str> {
    crate::interfaces::nextcloud::webdav_handler::strip_chroot_prefix(chroot, original_path)
}

// ────────────── Trashbin PROPFIND XML Generation ──────────────

use crate::application::dtos::trash_dto::TrashedItemDto;
use crate::application::services::nextcloud_file_id_service::NextcloudFileIdService;
use std::collections::HashMap;
use uuid::Uuid;

/// Generate a complete Nextcloud-compatible multistatus XML response for the trashbin.
///
/// `chroot` scopes the response — items whose original path is outside
/// the chroot (other drives the caller is a member of) are dropped
/// silently. NC's trashbin surface is single-drive from the client's
/// perspective; cross-drive items remain reachable via REST.
async fn write_trashbin_multistatus<W: std::io::Write>(
    writer: W,
    items: &[TrashedItemDto],
    username: &str,
    chroot: &crate::application::dtos::folder_dto::FolderDto,
    file_id_svc: Option<&Arc<NextcloudFileIdService>>,
) -> Result<(), String> {
    let mut xml = Writer::new(writer);

    // Root element with all required namespaces.
    let mut ms = BytesStart::new("d:multistatus");
    ms.push_attribute(("xmlns:d", "DAV:"));
    ms.push_attribute(("xmlns:oc", "http://owncloud.org/ns"));
    ms.push_attribute(("xmlns:nc", "http://nextcloud.org/ns"));
    xml.write_event(Event::Start(ms))
        .map_err(|e| e.to_string())?;

    // Root container entry for the trash collection itself.
    write_trash_root_response(&mut xml, username)?;

    // Pre-resolve every oc:fileid in two batch queries by object type (was one
    // INSERT round-trip per item). File and folder UUIDs are disjoint, so the
    // two maps merge cleanly into one keyed by parsed original-id UUID.
    let mut file_uuids: Vec<&str> = Vec::new();
    let mut folder_uuids: Vec<&str> = Vec::new();
    for item in items {
        if item.item_type == "folder" {
            folder_uuids.push(item.original_id.as_str());
        } else {
            file_uuids.push(item.original_id.as_str());
        }
    }
    let (mut id_map, folder_id_map) =
        batch_resolve_ids(file_id_svc, &file_uuids, &folder_uuids).await;
    id_map.extend(folder_id_map);

    // Individual trashed items — skip those whose original path is
    // outside the chroot (other-drive trash reachable via REST).
    for item in items {
        if crate::interfaces::nextcloud::webdav_handler::strip_chroot_prefix(
            chroot,
            &item.original_path,
        )
        .is_none()
        {
            tracing::debug!(
                target: "oxicloud::nc",
                "trashbin PROPFIND: dropping cross-chroot item '{}' at '{}'",
                item.id,
                item.original_path,
            );
            continue;
        }
        write_trash_item_response(&mut xml, item, username, chroot, file_id_svc, &id_map)?;
    }

    xml.write_event(Event::End(BytesEnd::new("d:multistatus")))
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Write the root collection response entry for the trash folder.
fn write_trash_root_response<W: std::io::Write>(
    xml: &mut Writer<W>,
    username: &str,
) -> Result<(), String> {
    xml.write_event(Event::Start(BytesStart::new("d:response")))
        .map_err(|e| e.to_string())?;

    let href = format!(
        "{}/remote.php/dav/trashbin/{}/trash/",
        crate::common::config::server_base_path(),
        username
    );
    write_text_element(xml, "d:href", &href)?;

    xml.write_event(Event::Start(BytesStart::new("d:propstat")))
        .map_err(|e| e.to_string())?;
    xml.write_event(Event::Start(BytesStart::new("d:prop")))
        .map_err(|e| e.to_string())?;

    // resourcetype = collection
    xml.write_event(Event::Start(BytesStart::new("d:resourcetype")))
        .map_err(|e| e.to_string())?;
    xml.write_event(Event::Empty(BytesStart::new("d:collection")))
        .map_err(|e| e.to_string())?;
    xml.write_event(Event::End(BytesEnd::new("d:resourcetype")))
        .map_err(|e| e.to_string())?;

    xml.write_event(Event::End(BytesEnd::new("d:prop")))
        .map_err(|e| e.to_string())?;
    write_text_element(xml, "d:status", "HTTP/1.1 200 OK")?;
    xml.write_event(Event::End(BytesEnd::new("d:propstat")))
        .map_err(|e| e.to_string())?;

    xml.write_event(Event::End(BytesEnd::new("d:response")))
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Write a single trashed item as a `<d:response>` element.
///
/// Caller is expected to have already verified the item is inside
/// `chroot` — see the guard in `write_trashbin_multistatus`. This
/// function trusts the invariant and expects `strip_home_prefix` to
/// return `Some(_)`; if it ever returns `None` (chroot drift between
/// the guard and the emit, defensive-only), the original-location
/// falls back to an empty string.
fn write_trash_item_response<W: std::io::Write>(
    xml: &mut Writer<W>,
    item: &TrashedItemDto,
    username: &str,
    chroot: &crate::application::dtos::folder_dto::FolderDto,
    file_id_svc: Option<&Arc<NextcloudFileIdService>>,
    id_map: &HashMap<Uuid, i64>,
) -> Result<(), String> {
    xml.write_event(Event::Start(BytesStart::new("d:response")))
        .map_err(|e| e.to_string())?;

    // href
    let href = format!(
        "{}/remote.php/dav/trashbin/{}/trash/{}",
        crate::common::config::server_base_path(),
        username,
        item.id
    );
    write_text_element(xml, "d:href", &href)?;

    xml.write_event(Event::Start(BytesStart::new("d:propstat")))
        .map_err(|e| e.to_string())?;
    xml.write_event(Event::Start(BytesStart::new("d:prop")))
        .map_err(|e| e.to_string())?;

    // d:displayname
    write_text_element(xml, "d:displayname", &item.name)?;

    // d:getlastmodified — stack-rendered (common::fmt), chrono fallback for
    // out-of-range timestamps; byte-identical to the old `to_rfc2822()`.
    write_date_element(xml, "d:getlastmodified", item.trashed_at.timestamp(), true)?;

    // d:getetag — exact-size quoted alloc instead of the format! interpreter.
    write_etag_element(xml, "d:getetag", &item.original_id)?;

    // d:resourcetype
    if item.item_type == "folder" {
        xml.write_event(Event::Start(BytesStart::new("d:resourcetype")))
            .map_err(|e| e.to_string())?;
        xml.write_event(Event::Empty(BytesStart::new("d:collection")))
            .map_err(|e| e.to_string())?;
        xml.write_event(Event::End(BytesEnd::new("d:resourcetype")))
            .map_err(|e| e.to_string())?;
    } else {
        xml.write_event(Event::Empty(BytesStart::new("d:resourcetype")))
            .map_err(|e| e.to_string())?;
    }

    // d:getcontenttype — the folder constant is borrowed (`Cow::Borrowed`, 0
    // allocs per trashed folder row); only the file branch (mime_guess) still
    // allocates its owned String (ROUND16 §M1 `Cow<'static, str>` pattern).
    let content_type: std::borrow::Cow<'static, str> = if item.item_type == "folder" {
        std::borrow::Cow::Borrowed("httpd/unix-directory")
    } else {
        std::borrow::Cow::Owned(mime_from_name(&item.name))
    };
    write_text_element(xml, "d:getcontenttype", &content_type)?;

    // d:getcontentlength
    write_text_element(xml, "d:getcontentlength", "0")?;

    // oc:fileid and oc:id — resolved up front in a batch query.
    let file_id = nc_id_of(id_map, &item.original_id);
    if let Some(id) = file_id {
        let mut ibuf = [0u8; 21];
        write_text_element(xml, "oc:fileid", crate::common::fmt::i64_str(&mut ibuf, id))?;
        let oc_id = format_oc_id(id, file_id_svc);
        write_text_element(xml, "oc:id", &oc_id)?;
    }

    // nc:trashbin-filename
    write_text_element(xml, "nc:trashbin-filename", &item.name)?;

    // nc:trashbin-original-location
    let original_location = strip_home_prefix(&item.original_path, username, chroot).unwrap_or("");
    write_text_element(xml, "nc:trashbin-original-location", original_location)?;

    // nc:trashbin-deletion-time
    {
        let mut ibuf = [0u8; 21];
        write_text_element(
            xml,
            "nc:trashbin-deletion-time",
            crate::common::fmt::i64_str(&mut ibuf, item.trashed_at.timestamp()),
        )?;
    }

    // oc:permissions — empty in trash
    write_text_element(xml, "oc:permissions", "")?;

    // oc:size
    write_text_element(xml, "oc:size", "0")?;

    xml.write_event(Event::End(BytesEnd::new("d:prop")))
        .map_err(|e| e.to_string())?;
    write_text_element(xml, "d:status", "HTTP/1.1 200 OK")?;
    xml.write_event(Event::End(BytesEnd::new("d:propstat")))
        .map_err(|e| e.to_string())?;

    xml.write_event(Event::End(BytesEnd::new("d:response")))
        .map_err(|e| e.to_string())?;

    Ok(())
}
