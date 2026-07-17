use axum::{
    body::{self, Body},
    http::{Request, StatusCode, header},
    response::Response,
};
use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesStart, Event},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

use crate::application::adapters::webdav_adapter::WebDavAdapter;
use crate::application::dtos::display_helpers::{format_file_size, intern_display};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::dtos::search_dto::SearchCriteriaDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::change_log_port::SyncChange;
use crate::application::ports::favorites_ports::FavoritesUseCase;
use crate::application::ports::file_ports::FileRetrievalUseCase;
use crate::application::ports::folder_ports::FolderUseCase;
use crate::application::ports::inbound::SearchUseCase;
use crate::application::services::webdav_sync_collection_service::WebdavSyncMember;
use crate::common::di::AppState;
use crate::domain::entities::file::File;
use crate::domain::entities::sync_token::SyncToken;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::services::path_resolver_service::ResolvedResource;
use crate::interfaces::api::handlers::webdav_handler::{
    dead_props_for, files_dead_props_map, folders_dead_props_map,
};
use crate::interfaces::errors::AppError;
use crate::interfaces::nextcloud::webdav_handler::{
    batch_resolve_ids, format_oc_id, format_oc_id_into, nc_collection_href_into, nc_href,
    nc_href_into, nc_id_of, nc_resolve_or_fallback, nc_to_internal_path, write_file_response,
    write_folder_response, write_text_element,
};

/// Handle WebDAV REPORT and SEARCH methods for Nextcloud compatibility.
///
/// Dispatches based on the XML body:
/// - `oc:filter-files` -- list favorited items (REPORT)
/// - `d:searchrequest`  -- search files by name (SEARCH)
/// - `sync-collection`  -- full re-sync of a collection's children (REPORT)
pub async fn handle_nc_report(
    state: Arc<AppState>,
    req: Request<Body>,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    // `Depth` must be read before `req.into_body()` consumes `req`.
    let depth = req
        .headers()
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1")
        .to_string();

    let body_bytes = body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read body: {}", e)))?;

    let body_str = String::from_utf8_lossy(&body_bytes);

    if body_str.contains("filter-files") {
        handle_filter_files(state, &body_str, session).await
    } else if body_str.contains("searchrequest") {
        handle_search(state, &body_str, session).await
    } else if body_str.contains("sync-collection") {
        handle_sync_collection(state, &body_str, session, subpath, &depth).await
    } else {
        // Unknown REPORT type -- return empty multistatus.
        Ok(empty_multistatus())
    }
}

// ──────────────────── Favorites filter (oc:filter-files) ────────────────────

async fn handle_filter_files(
    state: Arc<AppState>,
    _body: &str,
    session: &crate::interfaces::nextcloud::session::NcSession,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    let url_user = &session.raw_username;
    // Chroot-scope the response: NC's `oc:filter-files` REPORT is a
    // single-drive surface (the client PROPFINDs favorites under its
    // "home" URL and has no cross-drive concept). Favorites that live
    // in another drive the caller is a member of are dropped from
    // this response; they're still reachable via REST
    // `/api/favorites/resources`. `session.require_chroot()` is safe
    // here — the REPORT verb only reaches this handler through a
    // path-scoped route.
    let chroot = session.require_chroot()?;
    let fav_svc = match state.favorites_service.as_ref() {
        Some(svc) => svc,
        None => return Ok(empty_multistatus()),
    };

    let favorites = fav_svc
        .get_favorites(user.id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to get favorites: {}", e)))?;

    if favorites.is_empty() {
        return Ok(empty_multistatus());
    }

    let file_service = &state.applications.file_retrieval_service;
    let folder_service = &state.applications.folder_service;
    let nc = state.nextcloud.as_ref();
    let file_id_svc = nc.map(|n| &n.file_ids);

    // All items in this response are favorites.
    let favorite_ids: HashSet<String> = favorites.iter().map(|f| f.item_id.clone()).collect();

    // `home_prefix` is unused after the chroot-aware strip
    // (see `strip_home_prefix`); kept as a positional argument in
    // the emit calls below for signature stability with the
    // report-handler tests and the parallel search-pass caller.
    let home_prefix = "";

    // Pass 1: resolve the favorited DTOs in two batch queries (was one
    // get_* per favorite — up to N serial round-trips on a sync client's
    // REPORT). Results are looked up by id so the response keeps favorites
    // order; missing/trashed favorites simply drop out (as before).
    let mut file_ids: Vec<String> = Vec::new();
    let mut folder_ids: Vec<String> = Vec::new();
    for fav in &favorites {
        match fav.item_type.as_str() {
            "file" => file_ids.push(fav.item_id.clone()),
            "folder" => folder_ids.push(fav.item_id.clone()),
            _ => {}
        }
    }

    let mut file_map: HashMap<String, FileDto> = file_service
        .get_files_by_ids(&file_ids)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to resolve favorite files: {e}")))?
        .into_iter()
        .map(|f| (f.id.clone(), f))
        .collect();
    let mut folder_map: HashMap<String, FolderDto> = folder_service
        .get_folders_by_ids(&folder_ids)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to resolve favorite folders: {e}")))?
        .into_iter()
        .map(|f| (f.id.clone(), f))
        .collect();

    let mut files: Vec<FileDto> = Vec::new();
    let mut folders: Vec<FolderDto> = Vec::new();
    // Move the DTO out of the map instead of cloning it: the maps are built
    // just above solely to hydrate `files`/`folders` in favorites order and are
    // dropped at fn end, so the clone was pure waste. `favorites.item_id` is
    // unique per user, so `remove` drops nothing needed and the favorites order
    // is preserved (benches/ROUND20.md §C3).
    for fav in &favorites {
        match fav.item_type.as_str() {
            "file" => {
                if let Some(f) = file_map.remove(&fav.item_id) {
                    files.push(f);
                }
            }
            "folder" => {
                if let Some(f) = folder_map.remove(&fav.item_id) {
                    folders.push(f);
                }
            }
            _ => {}
        }
    }

    // Pass 2: resolve every oc:fileid in two batch queries (was one per item).
    let file_uuids: Vec<&str> = files.iter().map(|f| f.id.as_str()).collect();
    let folder_uuids: Vec<&str> = folders.iter().map(|f| f.id.as_str()).collect();
    let (file_id_map, folder_id_map) =
        batch_resolve_ids(file_id_svc, &file_uuids, &folder_uuids).await;

    // Pass 3: write the multistatus XML (pure synchronous map lookups).
    let mut buf = Vec::new();
    {
        let mut xml = Writer::new(&mut buf);

        write_multistatus_start(&mut xml)?;

        // Batched dead-props: one = ANY($1) query per type, not one per
        // result (benches/DEAD-PROPS.md).
        let file_deads = files_dead_props_map(&state.webdav_dead_props, &files).await;
        let folder_deads = folders_dead_props_map(&state.webdav_dead_props, &folders).await;

        // Keep main's batched-resolution structure (one batch query
        // per type, not 2N round-trips). Hrefs use `url_user` so the
        // multi-drive `~{drive}` form is echoed back to the client;
        // owner-id stays canonical via `&user.username`.
        // One oc:id buffer reused across both emit loops (benches/ROUND27.md §H1).
        let mut oc_buf = String::new();
        // One href buffer reused across both emit loops, with the URL-encoded
        // user computed once for the page instead of re-encoded per row — the
        // reused-buffer shape the PROPFIND child loop already uses
        // (benches/ROUND29.md §A).
        let encoded_user = urlencoding::encode(url_user);
        let mut href_buf = String::new();
        for file in &files {
            // Skip favorites that live outside the caller's chroot
            // (other-drive favorites); reachable via REST if needed.
            let Some(subpath) = strip_home_prefix(chroot, &file.path, home_prefix) else {
                tracing::debug!(
                    target: "oxicloud::nc",
                    "REPORT filter-files: dropping cross-chroot favorite '{}' at '{}'",
                    file.id,
                    file.path,
                );
                continue;
            };
            nc_href_into(&mut href_buf, &encoded_user, subpath);
            let fid = nc_id_of(&file_id_map, &file.id);
            let oc_id: Option<&str> = match fid {
                Some(id) => {
                    format_oc_id_into(&mut oc_buf, id, file_id_svc);
                    Some(oc_buf.as_str())
                }
                None => None,
            };
            let dead = dead_props_for(&file.id, &file_deads);
            write_file_response(
                &mut xml,
                file,
                &href_buf,
                (fid, oc_id),
                &user.username,
                &favorite_ids,
                dead,
            )
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
        }

        for folder in &folders {
            let Some(subpath) = strip_home_prefix(chroot, &folder.path, home_prefix) else {
                tracing::debug!(
                    target: "oxicloud::nc",
                    "REPORT filter-files: dropping cross-chroot favorite folder '{}' at '{}'",
                    folder.id,
                    folder.path,
                );
                continue;
            };
            nc_collection_href_into(&mut href_buf, &encoded_user, subpath);
            let fid = nc_id_of(&folder_id_map, &folder.id);
            let oc_id: Option<&str> = match fid {
                Some(id) => {
                    format_oc_id_into(&mut oc_buf, id, file_id_svc);
                    Some(oc_buf.as_str())
                }
                None => None,
            };
            let dead = dead_props_for(&folder.id, &folder_deads);
            write_folder_response(
                &mut xml,
                folder,
                &href_buf,
                (fid, oc_id),
                &user.username,
                &favorite_ids,
                // REPORT results are a flat filter/search listing, not a
                // PROPFIND on a specific collection — quota isn't
                // meaningful here (see `AppState::resolve_webdav_quota`).
                None,
                dead,
            )
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
        }

        xml.write_event(Event::End(BytesEnd::new("d:multistatus")))
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
    }

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(buf))
        .unwrap())
}

// ──────────────────── Search (d:searchrequest) ────────────────────

async fn handle_search(
    state: Arc<AppState>,
    body: &str,
    session: &crate::interfaces::nextcloud::session::NcSession,
) -> Result<Response<Body>, AppError> {
    let user = &session.user;
    // Chroot-scope the response: NC's search REPORT is a single-drive
    // surface. Results that live outside the chroot (other drives the
    // caller is a member of) are dropped from the multistatus and
    // recorded at debug — reachable via REST search if needed.
    // `resolve_scope_folder` below re-pulls chroot from the session
    // for the path-mapping step.
    let chroot = session.require_chroot()?;
    let url_user = &session.raw_username;
    let search_svc = match state.applications.search_service.as_ref() {
        Some(svc) => svc,
        None => return Ok(empty_multistatus()),
    };

    let term = parse_literal(body).unwrap_or_default();
    if term.is_empty() {
        return Ok(empty_multistatus());
    }

    let nresults = parse_nresults(body).unwrap_or(100);

    // Resolve folder scope from <d:href> inside <d:scope>.
    let folder_id = resolve_scope_folder(&state, body, session).await;

    let criteria = SearchCriteriaDto {
        name_contains: Some(term),
        recursive: true,
        limit: nresults,
        folder_id,
        ..Default::default()
    };

    let results = search_svc
        .search(criteria, user.id)
        .await
        .map_err(|e| AppError::internal_error(format!("Search failed: {}", e)))?;

    let nc = state.nextcloud.as_ref();
    let file_id_svc = nc.map(|n| &n.file_ids);
    // See the favorites pass above: `home_prefix` is unused after the
    // chroot-aware strip, kept only for signature stability.
    let home_prefix = "";

    // No favorite checking for search results -- pass an empty set.
    let favorite_ids: HashSet<String> = HashSet::new();

    // Materialize DTOs, then resolve every oc:fileid in two batch queries
    // (was one INSERT round-trip per result).
    let files: Vec<FileDto> = results.files.iter().map(file_dto_from_search).collect();
    let folders: Vec<FolderDto> = results.folders.iter().map(folder_dto_from_search).collect();
    let file_uuids: Vec<&str> = files.iter().map(|f| f.id.as_str()).collect();
    let folder_uuids: Vec<&str> = folders.iter().map(|f| f.id.as_str()).collect();
    let (file_id_map, folder_id_map) =
        batch_resolve_ids(file_id_svc, &file_uuids, &folder_uuids).await;

    let mut buf = Vec::new();
    {
        let mut xml = Writer::new(&mut buf);

        write_multistatus_start(&mut xml)?;

        // Batched dead-props: one = ANY($1) query per type, not one per
        // result (benches/DEAD-PROPS.md).
        let file_deads = files_dead_props_map(&state.webdav_dead_props, &files).await;
        let folder_deads = folders_dead_props_map(&state.webdav_dead_props, &folders).await;

        // Files.
        // One oc:id buffer reused across both emit loops (benches/ROUND27.md §H1).
        let mut oc_buf = String::new();
        // One href buffer reused across both emit loops, with the URL-encoded
        // user computed once for the page instead of re-encoded per row — the
        // reused-buffer shape the PROPFIND child loop already uses
        // (benches/ROUND29.md §A).
        let encoded_user = urlencoding::encode(url_user);
        let mut href_buf = String::new();
        for file in &files {
            let Some(subpath) = strip_home_prefix(chroot, &file.path, home_prefix) else {
                tracing::debug!(
                    target: "oxicloud::nc",
                    "REPORT search: dropping cross-chroot file '{}' at '{}'",
                    file.id,
                    file.path,
                );
                continue;
            };
            nc_href_into(&mut href_buf, &encoded_user, subpath);
            let fid = nc_id_of(&file_id_map, &file.id);
            let oc_id: Option<&str> = match fid {
                Some(id) => {
                    format_oc_id_into(&mut oc_buf, id, file_id_svc);
                    Some(oc_buf.as_str())
                }
                None => None,
            };
            let dead = dead_props_for(&file.id, &file_deads);
            write_file_response(
                &mut xml,
                file,
                &href_buf,
                (fid, oc_id),
                &user.username,
                &favorite_ids,
                dead,
            )
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
        }

        // Folders.
        for folder in &folders {
            let Some(subpath) = strip_home_prefix(chroot, &folder.path, home_prefix) else {
                tracing::debug!(
                    target: "oxicloud::nc",
                    "REPORT search: dropping cross-chroot folder '{}' at '{}'",
                    folder.id,
                    folder.path,
                );
                continue;
            };
            nc_collection_href_into(&mut href_buf, &encoded_user, subpath);
            let fid = nc_id_of(&folder_id_map, &folder.id);
            let oc_id: Option<&str> = match fid {
                Some(id) => {
                    format_oc_id_into(&mut oc_buf, id, file_id_svc);
                    Some(oc_buf.as_str())
                }
                None => None,
            };
            let dead = dead_props_for(&folder.id, &folder_deads);
            write_folder_response(
                &mut xml,
                folder,
                &href_buf,
                (fid, oc_id),
                &user.username,
                &favorite_ids,
                // REPORT results are a flat filter/search listing, not a
                // PROPFIND on a specific collection — quota isn't
                // meaningful here (see `AppState::resolve_webdav_quota`).
                None,
                dead,
            )
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
        }

        xml.write_event(Event::End(BytesEnd::new("d:multistatus")))
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
    }

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(buf))
        .unwrap())
}

// ──────────────────── Sync-collection (RFC 6578) ────────────────────

/// Handle the `<D:sync-collection>` REPORT (RFC 6578) on the NextCloud
/// surface — real incremental sync via `WebdavSyncCollectionService`,
/// same change log (`storage.folder_sync_changes`) the plain-WebDAV
/// surface's `webdav_handler.rs::handle_report` uses. An absent/empty
/// client `sync-token` renders the full current listing (paired with a
/// freshly minted token); a present token renders only the delta,
/// including RFC 6578 §3.7 404 sub-responses for removed members.
///
/// `Depth: infinity` is rejected (403), matching PROPFIND's depth cap;
/// the REPORT target must be a collection (409 if it resolves to a
/// file); an expired token yields 507 (RFC 6578 §3.6).
async fn handle_sync_collection(
    state: Arc<AppState>,
    body: &str,
    session: &crate::interfaces::nextcloud::session::NcSession,
    subpath: &str,
    depth: &str,
) -> Result<Response<Body>, AppError> {
    if depth == "infinity" {
        return Err(AppError::forbidden(
            "sync-collection Depth: infinity is not supported",
        ));
    }

    let sync_req = WebDavAdapter::parse_sync_collection(body.as_bytes())
        .map_err(|e| AppError::bad_request(format!("Failed to parse REPORT: {}", e)))?;

    let user = &session.user;
    let url_user = &session.raw_username;
    let chroot = session.require_chroot()?;

    let internal_path = nc_to_internal_path(chroot, subpath)?;
    let resolved = nc_resolve_or_fallback(&state, &internal_path, chroot.drive_id)
        .await
        .ok_or_else(|| AppError::not_found("Resource not found"))?;
    let folder = match resolved {
        ResolvedResource::Folder(folder) => folder,
        ResolvedResource::File(_) => {
            return Err(AppError::conflict(
                "sync-collection REPORT target must be a collection",
            ));
        }
    };
    let folder_uuid =
        Uuid::parse_str(&folder.id).map_err(|_| AppError::not_found("Resource not found"))?;
    state
        .authorization
        .require(
            Subject::User(user.id),
            Permission::Read,
            Resource::Folder(folder_uuid),
        )
        .await?;

    let folder_service = &state.applications.folder_service;
    let file_service = &state.applications.file_retrieval_service;
    let sync_service = &state.applications.webdav_sync_collection_service;
    let fav_svc = state.favorites_service.as_ref();
    let nc = state.nextcloud.as_ref();
    let file_id_svc = nc.map(|n| &n.file_ids);

    let requested_token = match sync_req.sync_token.as_deref() {
        Some(raw) => Some(
            SyncToken::parse_for_collection(raw, folder_uuid)
                .map_err(|e| AppError::bad_request(format!("Invalid sync-token: {e}")))?,
        ),
        None => None,
    };

    let (subfolders, files, deleted_hrefs, sync_token): (
        Vec<FolderDto>,
        Vec<FileDto>,
        Vec<String>,
        String,
    ) = if depth == "0" {
        // Depth:0 just refreshes the token — no children rendered, same
        // as this handler's pre-existing behavior.
        let new_token = sync_service
            .mint_initial_token(folder_uuid, user.id)
            .await?;
        (Vec::new(), Vec::new(), Vec::new(), new_token.to_string())
    } else {
        match requested_token {
            None => {
                let subfolders = folder_service
                    .list_folders_with_perms(Some(&folder.id), user.id)
                    .await
                    .map_err(|e| {
                        AppError::internal_error(format!("Failed to list folders: {}", e))
                    })?;
                let files = file_service
                    .list_files_with_perms(Some(&folder.id), user.id)
                    .await
                    .map_err(|e| {
                        AppError::internal_error(format!("Failed to list files: {}", e))
                    })?;
                let new_token = sync_service
                    .mint_initial_token(folder_uuid, user.id)
                    .await?;
                (subfolders, files, Vec::new(), new_token.to_string())
            }
            Some(token) => {
                let delta = sync_service
                    .list_changes_with_perms(folder_uuid, Some(token), user.id)
                    .await?;
                let mut subfolders = Vec::new();
                let mut files = Vec::new();
                let mut deleted_hrefs = Vec::new();
                for change in delta.changes {
                    match change {
                        SyncChange::Upserted(WebdavSyncMember::Folder(f)) => subfolders.push(f),
                        SyncChange::Upserted(WebdavSyncMember::File(f)) => files.push(f),
                        SyncChange::Deleted {
                            href_hint,
                            is_collection,
                            ..
                        } => {
                            let child_sub = if subpath.is_empty() {
                                href_hint
                            } else {
                                format!("{}/{}", subpath.trim_end_matches('/'), href_hint)
                            };
                            let mut href = nc_href(url_user, &child_sub);
                            if is_collection {
                                href.push('/');
                            }
                            deleted_hrefs.push(href);
                        }
                    }
                }
                (
                    subfolders,
                    files,
                    deleted_hrefs,
                    delta.new_token.to_string(),
                )
            }
        }
    };

    let favorite_ids = if let Some(fav) = fav_svc {
        let mut items: Vec<(&str, &str)> = subfolders
            .iter()
            .map(|f| (f.id.as_str(), "folder"))
            .collect();
        items.extend(files.iter().map(|f| (f.id.as_str(), "file")));
        fav.batch_check_favorites(user.id, &items)
            .await
            .unwrap_or_default()
    } else {
        HashSet::new()
    };

    let file_uuids: Vec<String> = files.iter().map(|f| f.id.clone()).collect();
    let folder_uuids: Vec<String> = subfolders.iter().map(|f| f.id.clone()).collect();
    let (file_id_map, folder_id_map) =
        batch_resolve_ids(file_id_svc, &file_uuids, &folder_uuids).await;

    let file_deads = files_dead_props_map(&state.webdav_dead_props, &files).await;
    let folder_deads = folders_dead_props_map(&state.webdav_dead_props, &subfolders).await;

    let mut buf = Vec::new();
    {
        let mut xml = Writer::new(&mut buf);
        write_multistatus_start(&mut xml)?;

        for file in &files {
            let child_sub = if subpath.is_empty() {
                file.name.clone()
            } else {
                format!("{}/{}", subpath.trim_end_matches('/'), file.name)
            };
            let href = nc_href(url_user, &child_sub);
            let fid = file_id_map.get(&file.id).copied();
            let oc_id = fid.map(|id| format_oc_id(id, file_id_svc));
            let dead = dead_props_for(&file.id, &file_deads);
            write_file_response(
                &mut xml,
                file,
                &href,
                (fid, oc_id.as_deref()),
                &user.username,
                &favorite_ids,
                dead,
            )
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
        }

        for sf in &subfolders {
            let child_sub = if subpath.is_empty() {
                sf.name.clone()
            } else {
                format!("{}/{}", subpath.trim_end_matches('/'), sf.name)
            };
            let href = format!("{}/", nc_href(url_user, &child_sub));
            let fid = folder_id_map.get(&sf.id).copied();
            let oc_id = fid.map(|id| format_oc_id(id, file_id_svc));
            let dead = dead_props_for(&sf.id, &folder_deads);
            write_folder_response(
                &mut xml,
                sf,
                &href,
                (fid, oc_id.as_deref()),
                &user.username,
                &favorite_ids,
                // sync-collection results aren't a PROPFIND on a single
                // collection — quota isn't meaningful here (matches the
                // favorites/search REPORT handlers above).
                None,
                dead,
            )
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
        }

        for href in &deleted_hrefs {
            write_deleted_response(&mut xml, href)?;
        }

        write_text_element(&mut xml, "d:sync-token", &sync_token)
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;

        xml.write_event(Event::End(BytesEnd::new("d:multistatus")))
            .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
    }

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(buf))
        .unwrap())
}

/// RFC 6578 §3.7 removed-member sub-response for the NC surface (lowercase
/// `d:` prefix, matching this file's other XML writers): a `<d:response>`
/// whose `<d:status>` is 404, with no `<d:propstat>` block.
fn write_deleted_response<W: std::io::Write>(
    xml: &mut Writer<W>,
    href: &str,
) -> Result<(), AppError> {
    xml.write_event(Event::Start(BytesStart::new("d:response")))
        .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
    write_text_element(xml, "d:href", href)
        .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
    write_text_element(xml, "d:status", "HTTP/1.1 404 Not Found")
        .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
    xml.write_event(Event::End(BytesEnd::new("d:response")))
        .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
    Ok(())
}

// ──────────────────── DTO conversions ────────────────────

/// Build a `FileDto` from a search file result.
fn file_dto_from_search(fr: &crate::application::dtos::search_dto::SearchFileResultDto) -> FileDto {
    // Route ETag through `File::compute_etag` so REPORT/SEARCH hits
    // emit the same opaque token NC's sync client cached from the
    // earlier PROPFIND walk — without this, NC's conditional-request
    // logic on search results disagrees with its own cached state
    // and triggers a spurious re-fetch.
    let etag = if fr.blob_hash.is_empty() {
        String::new()
    } else {
        File::compute_etag(&fr.blob_hash, fr.modified_at)
    };
    FileDto {
        id: fr.id.clone(),
        name: fr.name.clone(),
        path: fr.path.clone(),
        size: fr.size,
        // Interned `Arc<str>` carried through from enrichment — refcount
        // bumps; the old code re-ran all three display classifiers and
        // re-allocated each value per converted search row.
        mime_type: fr.mime_type.clone(),
        folder_id: fr.folder_id.clone(),
        created_at: fr.created_at,
        modified_at: fr.modified_at,
        icon_class: fr.icon_class.clone(),
        icon_special_class: fr.icon_special_class.clone(),
        category: fr.category.clone(),
        size_formatted: format_file_size(fr.size),
        sort_date: None,
        content_hash: fr.blob_hash.clone(),
        etag,
        // §14 provenance not selected by the search result DTO.
        created_by: None,
        updated_by: None,
        // NextCloud search doesn't render the SPA badges; safe
        // defaults, dropped by the WebDAV/NC XML property serializer.
        is_favorite: false,
        is_shared: false,
    }
}

/// Bench-only public wrapper (feature = "bench") over the private
/// search→FileDto conversion so `examples/bench_search_enrich.rs` can
/// measure and equivalence-gate it.
#[cfg(feature = "bench")]
pub fn file_dto_from_search_for_bench(
    fr: &crate::application::dtos::search_dto::SearchFileResultDto,
) -> FileDto {
    file_dto_from_search(fr)
}

/// Build a `FolderDto` from a search folder result.
fn folder_dto_from_search(
    sr: &crate::application::dtos::search_dto::SearchFolderResultDto,
) -> FolderDto {
    FolderDto {
        etag: sr.id.clone(),
        id: sr.id.clone(),
        name: sr.name.clone(),
        path: sr.path.clone(),
        parent_id: sr.parent_id.clone(),
        drive_id: sr.drive_id,
        created_at: sr.created_at,
        modified_at: sr.modified_at,
        is_root: sr.is_root,
        icon_class: intern_display("fas fa-folder"),
        icon_special_class: intern_display("folder-icon"),
        category: intern_display("Folder"),
        // §14 provenance not selected by search results.
        created_by: None,
        updated_by: None,
        // See file_dto_from_search: NC/DAV property serializer drops
        // these; safe default.
        is_favorite: false,
        is_shared: false,
    }
}

// ──────────────────── XML helpers ────────────────────

/// Write the opening `<d:multistatus>` element with namespace declarations.
fn write_multistatus_start<W: std::io::Write>(xml: &mut Writer<W>) -> Result<(), AppError> {
    let mut ms = BytesStart::new("d:multistatus");
    ms.push_attribute(("xmlns:d", "DAV:"));
    ms.push_attribute(("xmlns:oc", "http://owncloud.org/ns"));
    ms.push_attribute(("xmlns:nc", "http://nextcloud.org/ns"));
    xml.write_event(Event::Start(ms))
        .map_err(|e| AppError::internal_error(format!("XML write error: {}", e)))?;
    Ok(())
}

/// Build an empty 207 Multi-Status response.
fn empty_multistatus() -> Response<Body> {
    let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
</d:multistatus>"#;

    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(xml))
        .unwrap()
}

// ──────────────────── XML parsing helpers ────────────────────

/// Extract the search term from `<d:literal>%term%</d:literal>` using quick_xml.
fn parse_literal(body: &str) -> Option<String> {
    let text = xml_extract_text(body, b"literal")?;
    // Strip SQL-style % wildcards.
    let term = text.trim_matches('%').trim();
    if term.is_empty() {
        None
    } else {
        Some(term.to_string())
    }
}

/// Extract the result limit from `<d:nresults>100</d:nresults>` using quick_xml.
fn parse_nresults(body: &str) -> Option<usize> {
    let text = xml_extract_text(body, b"nresults")?;
    text.trim().parse::<usize>().ok()
}

/// Extract the scope href from `<d:href>` inside `<d:scope>` using quick_xml.
fn parse_scope_href(body: &str) -> Option<String> {
    let mut reader = Reader::from_str(body);
    let mut inside_scope = false;
    let mut inside_href = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let local = e.local_name();
                if local.as_ref() == b"scope" {
                    inside_scope = true;
                } else if inside_scope && local.as_ref() == b"href" {
                    inside_href = true;
                }
            }
            Ok(Event::Text(ref e)) if inside_href => {
                let text = e.decode().ok()?;
                let href = text.trim();
                if href.is_empty() {
                    return None;
                }
                return Some(href.to_string());
            }
            Ok(Event::End(ref e)) => {
                let local = e.local_name();
                if local.as_ref() == b"scope" {
                    inside_scope = false;
                } else if local.as_ref() == b"href" {
                    inside_href = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    None
}

/// Generic helper: extract text content from the first element matching a local name.
fn xml_extract_text(body: &str, local_name: &[u8]) -> Option<String> {
    let mut reader = Reader::from_str(body);
    let mut inside = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) if e.local_name().as_ref() == local_name => {
                inside = true;
            }
            Ok(Event::Text(ref e)) if inside => {
                return e.decode().ok().map(|s| s.to_string());
            }
            Ok(Event::End(ref e)) if e.local_name().as_ref() == local_name => {
                inside = false;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    None
}

/// Resolve a scope href (e.g. `/files/username/Documents`) to a folder ID.
///
/// Pulls everything it needs from the `NcSession`: the caller's id (so
/// `get_folder_by_path` can be user-scoped — post-D0 paths like
/// `Personal/Docs` are not globally unique), the chroot (provides the
/// path prefix that `nc_to_internal_path` prepends), and the raw wire
/// `{user}` segment (bare or `admin~{uuid}`) so we strip the prefix the
/// NC client actually sent.
async fn resolve_scope_folder(
    state: &AppState,
    body: &str,
    session: &crate::interfaces::nextcloud::session::NcSession,
) -> Option<String> {
    let chroot = session.require_chroot().ok()?;
    let url_user = &session.raw_username;
    let href = parse_scope_href(body)?;

    // The href is typically `/files/{url_user}/subpath` or
    // `/remote.php/dav/files/{url_user}/subpath`. On a multi-drive
    // session the `{url_user}` segment carries the `~{uuid}` marker,
    // so we strip with the composite to find the real subpath. Using
    // `user.username` here would fail to match for non-home drives.
    let subpath = extract_subpath_from_scope(&href, url_user)?;
    if subpath.is_empty() {
        // Root scope -- no folder_id filter needed.
        return None;
    }

    let internal_path =
        crate::interfaces::nextcloud::webdav_handler::nc_to_internal_path(chroot, &subpath).ok()?;

    let folder_service = &state.applications.folder_service;
    folder_service
        .get_folder_by_path(&internal_path, chroot.drive_id)
        .await
        .ok()
        .map(|f| f.id)
}

/// Extract the subpath portion from a scope href.
///
/// Handles both short form `/files/{user}/sub` and full
/// `/remote.php/dav/files/{user}/sub`. `url_user` is the literal URL
/// `{user}` segment — bare for legacy single-drive sync, composite
/// `admin~{uuid}` for multi-drive — so this matches whichever shape
/// the NC client actually sent.
fn extract_subpath_from_scope(href: &str, url_user: &str) -> Option<String> {
    let patterns = [
        format!("/remote.php/dav/files/{}/", url_user),
        format!("/files/{}/", url_user),
        format!("/remote.php/dav/files/{}", url_user),
        format!("/files/{}", url_user),
    ];

    for pat in &patterns {
        if let Some(rest) = href.strip_prefix(pat.as_str()) {
            return Some(rest.trim_matches('/').to_string());
        }
    }

    None
}

/// Strip the caller's chroot prefix from an internal path so the
/// caller-facing DAV subpath is chroot-relative. Delegates to
/// `webdav_handler::strip_chroot_prefix` — chroot-aware, multi-segment
/// safe, and rejects items outside the chroot. Callers must decide
/// per-response whether an out-of-chroot item is dropped or falls
/// back to the naive strip.
///
/// See `strip_chroot_prefix` for the full contract. The `_prefix`
/// legacy arg stays for signature stability with the emit helpers.
fn strip_home_prefix<'a>(
    chroot: &crate::application::dtos::folder_dto::FolderDto,
    path: &'a str,
    _prefix: &str,
) -> Option<&'a str> {
    crate::interfaces::nextcloud::webdav_handler::strip_chroot_prefix(chroot, path)
}
