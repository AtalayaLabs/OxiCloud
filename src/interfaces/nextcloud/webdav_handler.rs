use axum::{
    body::{self, Body},
    http::{HeaderName, Request, StatusCode, header},
    response::Response,
};
use bytes::Buf;
use chrono::Utc;
use quick_xml::{
    Writer,
    events::{BytesEnd, BytesStart, BytesText, Event},
};
use std::collections::HashSet;
use std::sync::Arc;

use crate::application::adapters::webdav_adapter::{PropFindRequest, WebDavAdapter};
use crate::application::ports::favorites_ports::FavoritesUseCase;
use crate::application::ports::file_ports::{
    FileManagementUseCase, FileRetrievalUseCase, FileUploadUseCase,
};
use crate::application::ports::inbound::FolderUseCase;
use crate::application::ports::trash_ports::TrashUseCase;
use crate::common::di::AppState;
use crate::common::mime_detect::{filename_from_path, refine_content_type};
use crate::infrastructure::services::audio_metadata_service::AudioMetadataService;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::{AuthUser, CurrentUser};

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

/// Resolve the internal OxiCloud path from a Nextcloud DAV subpath.
///
/// Nextcloud: /remote.php/dav/files/{user}/{subpath}
/// Internal:  My Folder - {username}/{subpath}
///
/// An empty subpath maps to the user's home folder root.
pub fn nc_to_internal_path(username: &str, subpath: &str) -> Result<String, AppError> {
    let home = format!("My Folder - {}", username);
    let subpath = subpath.trim_matches('/');
    if subpath.is_empty() {
        return Ok(home);
    }
    // Reject path traversal attempts.
    if subpath.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err(AppError::bad_request("Invalid path: traversal not allowed"));
    }
    Ok(format!("{}/{}", home, subpath))
}

/// Build the Nextcloud DAV href for a resource.
///
/// Each path segment is URL-encoded individually so filenames with spaces,
/// `#`, `%`, or non-ASCII characters produce valid PROPFIND hrefs.
pub fn nc_href(username: &str, subpath: &str) -> String {
    let subpath = subpath.trim_matches('/');
    let encoded_user = urlencoding::encode(username);
    if subpath.is_empty() {
        format!("/remote.php/dav/files/{}/", encoded_user)
    } else {
        let encoded_segments: Vec<_> = subpath
            .split('/')
            .map(|seg| urlencoding::encode(seg))
            .collect();
        format!(
            "/remote.php/dav/files/{}/{}",
            encoded_user,
            encoded_segments.join("/")
        )
    }
}

/// Dispatch Nextcloud WebDAV request to the appropriate handler.
///
/// `subpath` is everything after `/remote.php/dav/files/{user}/`.
pub async fn handle_nc_webdav(
    state: Arc<AppState>,
    req: Request<Body>,
    user: AuthUser,
    subpath: String,
) -> Result<Response<Body>, AppError> {
    let method = req.method().clone();
    match method.as_str() {
        "OPTIONS" => handle_options(),
        "PROPFIND" => handle_propfind(state, req, &user, &subpath).await,
        "GET" => handle_get(state, &user, &subpath).await,
        "PUT" => handle_put(state, req, &user, &subpath).await,
        "MKCOL" => handle_mkcol(state, &user, &subpath).await,
        "DELETE" => handle_delete(state, &user, &subpath).await,
        "MOVE" => handle_move(state, req, &user, &subpath).await,
        "HEAD" => handle_head(state, &user, &subpath).await,
        "PROPPATCH" => handle_proppatch(state, req, &user, &subpath).await,
        "REPORT" | "SEARCH" => {
            crate::interfaces::nextcloud::report_handler::handle_nc_report(
                state, req, &user, &subpath,
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
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(HEADER_DAV, "1, 2, 3")
        .header(
            header::ALLOW,
            "OPTIONS, GET, HEAD, PUT, DELETE, MKCOL, MOVE, PROPFIND, PROPPATCH, REPORT, SEARCH",
        )
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── PROPFIND ────────────────────

async fn handle_propfind(
    state: Arc<AppState>,
    req: Request<Body>,
    user: &CurrentUser,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
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

    let propfind = if body_bytes.is_empty() {
        PropFindRequest {
            prop_find_type: crate::application::adapters::webdav_adapter::PropFindType::AllProp,
        }
    } else {
        WebDavAdapter::parse_propfind(body_bytes.reader())
            .map_err(|e| AppError::bad_request(format!("Invalid PROPFIND XML: {}", e)))?
    };

    let internal_path = nc_to_internal_path(&user.username, subpath)?;
    let folder_service = &state.applications.folder_service;
    let file_service = &state.applications.file_retrieval_service;

    // Try to resolve as folder first.
    let folder_result = folder_service.get_folder_by_path(&internal_path).await;

    if let Ok(folder) = folder_result {
        // It's a folder.
        let (files, subfolders) = if depth != "0" {
            let files = file_service
                .list_files(Some(&folder.id))
                .await
                .unwrap_or_default();
            let subfolders = folder_service
                .list_folders(Some(&folder.id))
                .await
                .unwrap_or_default();
            (files, subfolders)
        } else {
            (vec![], vec![])
        };

        // Batch-check favorites for all items in this listing.
        let favorite_ids = if let Some(fav_svc) = state.favorites_service.as_ref() {
            let mut items: Vec<(&str, &str)> = Vec::new();
            items.push((&folder.id, "folder"));
            for f in &files {
                items.push((&f.id, "file"));
            }
            for sf in &subfolders {
                items.push((&sf.id, "folder"));
            }
            fav_svc
                .batch_check_favorites(user.id, &items)
                .await
                .unwrap_or_default()
        } else {
            HashSet::new()
        };

        // Generate Nextcloud-aware XML.
        let nc = state.nextcloud.as_ref();
        let file_id_svc = nc.map(|n| &n.file_ids);

        let mut buf = Vec::new();
        write_nc_multistatus(
            &mut buf,
            Some(&folder),
            &files,
            &subfolders,
            &propfind,
            &depth,
            &user.username,
            subpath,
            file_id_svc,
            &favorite_ids,
        )
        .await
        .map_err(|e| AppError::internal_error(format!("XML generation failed: {}", e)))?;

        return Ok(Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Body::from(buf))
            .unwrap());
    }

    // Not a folder — try as a file.
    let file_result = file_service.get_file_by_path(&internal_path).await;
    if let Ok(file) = file_result {
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

        let mut buf = Vec::new();
        write_nc_multistatus(
            &mut buf,
            None,
            &[file],
            &[],
            &propfind,
            "0",
            &user.username,
            subpath,
            file_id_svc,
            &favorite_ids,
        )
        .await
        .map_err(|e| AppError::internal_error(format!("XML generation failed: {}", e)))?;

        return Ok(Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Body::from(buf))
            .unwrap());
    }

    Err(AppError::not_found("Resource not found"))
}

// ──────────────────── GET ────────────────────

async fn handle_get(
    state: Arc<AppState>,
    user: &CurrentUser,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    // GET on root folder — NC clients use this as an existence check
    if subpath.is_empty() || subpath == "/" {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("DAV", "1, 3")
            .body(Body::empty())
            .unwrap());
    }

    let internal_path = nc_to_internal_path(&user.username, subpath)?;
    let file_service = &state.applications.file_retrieval_service;
    let folder_service = &state.applications.folder_service;

    // Check if path is a folder first (NC clients use GET as existence check)
    if folder_service
        .get_folder_by_path(&internal_path)
        .await
        .is_ok()
    {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("DAV", "1, 3")
            .body(Body::empty())
            .unwrap());
    }

    let file = file_service
        .get_file_by_path(&internal_path)
        .await
        .map_err(|_| AppError::not_found("File not found"))?;

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
        .header(header::ETAG, format!("\"{}\"", file.id))
        .header(header::LAST_MODIFIED, modified_at.to_rfc2822())
        .body(Body::from_stream(std::pin::Pin::from(stream)))
        .unwrap())
}

// ──────────────────── HEAD ────────────────────

async fn handle_head(
    state: Arc<AppState>,
    user: &CurrentUser,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    // HEAD on root folder — NC clients use this as an existence check
    if subpath.is_empty() || subpath == "/" {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("DAV", "1, 3")
            .body(Body::empty())
            .unwrap());
    }

    let internal_path = nc_to_internal_path(&user.username, subpath)?;
    let file_service = &state.applications.file_retrieval_service;
    let folder_service = &state.applications.folder_service;

    // Check if path is a folder (NC clients use HEAD as existence check)
    if folder_service
        .get_folder_by_path(&internal_path)
        .await
        .is_ok()
    {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("DAV", "1, 3")
            .body(Body::empty())
            .unwrap());
    }

    let file = file_service
        .get_file_by_path(&internal_path)
        .await
        .map_err(|_| AppError::not_found("File not found"))?;

    let modified_at =
        chrono::DateTime::<Utc>::from_timestamp(timestamp_to_i64(file.modified_at), 0)
            .unwrap_or_else(Utc::now);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, file.mime_type.as_ref())
        .header(header::CONTENT_LENGTH, file.size)
        .header(header::ETAG, format!("\"{}\"", file.id))
        .header(header::LAST_MODIFIED, modified_at.to_rfc2822())
        .body(Body::empty())
        .unwrap())
}

// ──────────────────── PROPPATCH ────────────────────

async fn handle_proppatch(
    state: Arc<AppState>,
    req: Request<Body>,
    user: &CurrentUser,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let body_bytes = body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read body: {}", e)))?;

    let body_str = String::from_utf8_lossy(&body_bytes);

    // Parse oc:favorite value from PROPPATCH XML.
    let favorite_value = parse_proppatch_favorite(&body_str);

    if let Some(value) = favorite_value {
        let internal_path = nc_to_internal_path(&user.username, subpath)?;
        let file_service = &state.applications.file_retrieval_service;
        let folder_service = &state.applications.folder_service;

        // Determine item_id and item_type.
        let (item_id, item_type) =
            if let Ok(file) = file_service.get_file_by_path(&internal_path).await {
                (file.id, "file")
            } else if let Ok(folder) = folder_service.get_folder_by_path(&internal_path).await {
                (folder.id, "folder")
            } else {
                return Err(AppError::not_found("Resource not found"));
            };

        if let Some(fav_svc) = state.favorites_service.as_ref() {
            if value == 1 {
                fav_svc
                    .add_to_favorites(user.id, &item_id, item_type)
                    .await
                    .map_err(|e| {
                        AppError::internal_error(format!("Failed to add favorite: {}", e))
                    })?;
            } else {
                fav_svc
                    .remove_from_favorites(user.id, &item_id, item_type)
                    .await
                    .map_err(|e| {
                        AppError::internal_error(format!("Failed to remove favorite: {}", e))
                    })?;
            }
        }
    }

    // Return 207 Multi-Status with success response using quick_xml for safe escaping.
    let href = nc_href(&user.username, subpath);
    let mut buf = Vec::new();
    {
        let mut xml = Writer::new(&mut buf);
        xml.write_event(Event::Text(BytesText::new(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>",
        )))
        .map_err(|e| AppError::internal_error(format!("XML write failed: {}", e)))?;

        let mut ms = BytesStart::new("d:multistatus");
        ms.push_attribute(("xmlns:d", "DAV:"));
        ms.push_attribute(("xmlns:oc", "http://owncloud.org/ns"));
        xml.write_event(Event::Start(ms))
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;

        xml.write_event(Event::Start(BytesStart::new("d:response")))
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
        write_text_element(&mut xml, "d:href", &href)
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
        xml.write_event(Event::Start(BytesStart::new("d:propstat")))
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
        xml.write_event(Event::Start(BytesStart::new("d:prop")))
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
        xml.write_event(Event::Empty(BytesStart::new("oc:favorite")))
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
        xml.write_event(Event::End(BytesEnd::new("d:prop")))
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
        write_text_element(&mut xml, "d:status", "HTTP/1.1 200 OK")
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
        xml.write_event(Event::End(BytesEnd::new("d:propstat")))
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
        xml.write_event(Event::End(BytesEnd::new("d:response")))
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
        xml.write_event(Event::End(BytesEnd::new("d:multistatus")))
            .map_err(|e| AppError::internal_error(format!("XML: {}", e)))?;
    }

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(buf))
        .unwrap())
}

/// Parse the oc:favorite value from a PROPPATCH XML body using quick_xml.
fn parse_proppatch_favorite(body: &str) -> Option<u8> {
    use quick_xml::Reader;

    let mut reader = Reader::from_str(body);
    let mut inside_favorite = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let local = e.local_name();
                if local.as_ref() == b"favorite" {
                    inside_favorite = true;
                }
            }
            Ok(Event::Text(ref e)) if inside_favorite => {
                let text = e.decode().ok()?;
                return text.trim().parse::<u8>().ok();
            }
            Ok(Event::End(ref e)) => {
                if e.local_name().as_ref() == b"favorite" {
                    inside_favorite = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    None
}

// ──────────────────── PUT ────────────────────

async fn handle_put(
    state: Arc<AppState>,
    req: Request<Body>,
    user: &CurrentUser,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let internal_path = nc_to_internal_path(&user.username, subpath)?;
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

    let max_upload = state.core.config.storage.max_upload_size;
    let body_bytes = body::to_bytes(req.into_body(), max_upload)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read body: {}", e)))?;

    // Detect real MIME type via magic bytes + extension, falling back to client header.
    let filename = filename_from_path(subpath);
    let content_type = refine_content_type(&body_bytes, filename, &claimed_type);

    // Check if the file already exists (update vs create).
    let existing = file_service.get_file_by_path(&internal_path).await;

    if existing.is_ok() {
        // Update existing file — returns FileDto with fresh content-hash etag.
        let updated = upload_service
            .update_file(&internal_path, &body_bytes, &content_type, oc_mtime)
            .await
            .map_err(|e| AppError::internal_error(format!("Failed to update file: {}", e)))?;

        // Update audio metadata for supported audio files.
        if let Some(ref audio_service) = state.applications.audio_metadata_service
            && let Ok(file_id) = uuid::Uuid::parse_str(&updated.id)
        {
            let file_path = state.core.dedup_service.blob_path(&updated.etag);
            let is_audio = AudioMetadataService::is_audio_file(&content_type);

            if is_audio {
                AudioMetadataService::spawn_extraction_with_delete_background(
                    audio_service.clone(),
                    file_id,
                    file_path,
                );
            }
        }

        return Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ETAG, format!("\"{}\"", updated.etag))
            .header("oc-etag", format!("\"{}\"", updated.etag))
            .body(Body::empty())
            .unwrap());
    }

    // Create new file — split subpath into parent dir and filename.
    let (parent_subpath, filename) = match subpath.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", subpath),
    };

    let parent_internal = nc_to_internal_path(&user.username, parent_subpath)?;

    let file_dto = upload_service
        .create_file(&parent_internal, filename, &body_bytes, &content_type)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to create file: {}", e)))?;

    // Extract audio metadata for supported audio files in background.
    if let Some(ref audio_service) = state.applications.audio_metadata_service
        && AudioMetadataService::is_audio_file(&file_dto.mime_type)
        && let Ok(file_id) = uuid::Uuid::parse_str(&file_dto.id)
    {
        let file_path = state.core.dedup_service.blob_path(&file_dto.etag);
        AudioMetadataService::spawn_extraction_background(
            audio_service.clone(),
            file_id,
            file_path,
        );
    }

    let builder = Response::builder()
        .status(StatusCode::CREATED)
        .header(header::ETAG, format!("\"{}\"", file_dto.etag))
        .header("oc-etag", format!("\"{}\"", file_dto.etag));

    Ok(builder.body(Body::empty()).unwrap())
}

// ──────────────────── MKCOL ────────────────────

async fn handle_mkcol(
    state: Arc<AppState>,
    user: &CurrentUser,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    use crate::application::dtos::folder_dto::EnsurePathOutcome;

    let folder_service = &state.applications.folder_service;
    let internal_path = nc_to_internal_path(&user.username, subpath)?;

    match folder_service.ensure_path(&internal_path, user.id).await {
        Ok(EnsurePathOutcome::Created(_)) => Ok(Response::builder()
            .status(StatusCode::CREATED)
            .body(Body::empty())
            .unwrap()),
        Ok(EnsurePathOutcome::Existed(_)) => Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .unwrap()),
        Err(e) if e.kind == crate::common::errors::ErrorKind::AlreadyExists => {
            // A non-folder resource (e.g. a file) sits at the leaf path.
            // RFC 4918 §9.3.1: MKCOL on any existing resource → 405.
            Ok(Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body(Body::empty())
                .unwrap())
        }
        Err(e) => Err(e.into()),
    }
}

// ──────────────────── DELETE ────────────────────

async fn handle_delete(
    state: Arc<AppState>,
    user: &CurrentUser,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let internal_path = nc_to_internal_path(&user.username, subpath)?;
    let folder_service = &state.applications.folder_service;
    let file_service = &state.applications.file_retrieval_service;

    // Prefer soft-delete (move to trash) when trash service is available.
    // This is what Nextcloud clients expect — items appear in the trashbin.
    if let Some(trash_svc) = state.trash_service.as_ref() {
        if let Ok(folder) = folder_service.get_folder_by_path(&internal_path).await {
            trash_svc
                .move_to_trash(&folder.id, "folder", user.id)
                .await
                .map_err(|e| AppError::internal_error(format!("Failed to trash folder: {}", e)))?;
            return Ok(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .unwrap());
        }
        if let Ok(file) = file_service.get_file_by_path(&internal_path).await {
            trash_svc
                .move_to_trash(&file.id, "file", user.id)
                .await
                .map_err(|e| AppError::internal_error(format!("Failed to trash file: {}", e)))?;
            return Ok(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .unwrap());
        }
        return Err(AppError::not_found("Resource not found"));
    }

    // Fallback: hard delete when trash service is not available.
    let file_mgmt = &state.applications.file_management_service;

    if let Ok(folder) = folder_service.get_folder_by_path(&internal_path).await {
        folder_service
            .delete_folder(&folder.id, user.id)
            .await
            .map_err(|e| AppError::internal_error(format!("Failed to delete folder: {}", e)))?;

        return Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap());
    }

    if let Ok(file) = file_service.get_file_by_path(&internal_path).await {
        file_mgmt
            .delete_file(&file.id)
            .await
            .map_err(|e| AppError::internal_error(format!("Failed to delete file: {}", e)))?;

        return Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap());
    }

    Err(AppError::not_found("Resource not found"))
}

// ──────────────────── MOVE ────────────────────

async fn handle_move(
    state: Arc<AppState>,
    req: Request<Body>,
    user: &CurrentUser,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    let destination = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::bad_request("Missing Destination header"))?
        .to_string();

    // Parse destination path: extract subpath after /remote.php/dav/files/{user}/
    let dest_subpath = extract_nc_subpath_from_dest(&destination, &user.username)
        .ok_or_else(|| AppError::bad_request("Invalid Destination URL"))?;

    let src_internal = nc_to_internal_path(&user.username, subpath)?;
    let folder_service = &state.applications.folder_service;
    let file_service = &state.applications.file_retrieval_service;
    let file_mgmt = &state.applications.file_management_service;

    // Try as file first.
    if let Ok(file) = file_service.get_file_by_path(&src_internal).await {
        let (dest_parent_sub, dest_name) = match dest_subpath.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", dest_subpath.as_str()),
        };
        let dest_parent_internal = nc_to_internal_path(&user.username, dest_parent_sub)?;

        // Rename if only the name changes (same parent).
        let src_parent_sub = match subpath.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => "",
        };

        if src_parent_sub == dest_parent_sub {
            // Same parent → rename.
            file_mgmt
                .rename_file(&file.id, dest_name)
                .await
                .map_err(|e| AppError::internal_error(format!("Rename failed: {}", e)))?;
        } else {
            // Different parent → move.
            let dest_parent = folder_service
                .get_folder_by_path(&dest_parent_internal)
                .await
                .map_err(|_| AppError::not_found("Destination folder not found"))?;

            file_mgmt
                .move_file(&file.id, Some(dest_parent.id.clone()))
                .await
                .map_err(|e| AppError::internal_error(format!("Move failed: {}", e)))?;

            // If the filename changed too, rename after move.
            if file.name != dest_name {
                file_mgmt
                    .rename_file(&file.id, dest_name)
                    .await
                    .map_err(|e| AppError::internal_error(format!("Rename failed: {}", e)))?;
            }
        }

        // Return ETag and OC-ETag so Nextcloud clients can track the moved file.
        let dest_internal = nc_to_internal_path(&user.username, &dest_subpath)?;
        let mut builder = Response::builder().status(StatusCode::CREATED);
        if let Ok(moved) = file_service.get_file_by_path(&dest_internal).await {
            builder = builder
                .header(header::ETAG, format!("\"{}\"", moved.id))
                .header("oc-etag", format!("\"{}\"", moved.id));
        }

        return Ok(builder.body(Body::empty()).unwrap());
    }

    // Try as folder.
    if let Ok(folder) = folder_service.get_folder_by_path(&src_internal).await {
        let (dest_parent_sub, dest_name) = match dest_subpath.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", dest_subpath.as_str()),
        };
        let dest_parent_internal = nc_to_internal_path(&user.username, dest_parent_sub)?;

        let src_parent_sub = match subpath.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => "",
        };

        if src_parent_sub == dest_parent_sub {
            // Same parent → rename.
            use crate::application::dtos::folder_dto::RenameFolderDto;
            folder_service
                .rename_folder(
                    &folder.id,
                    RenameFolderDto {
                        name: dest_name.to_string(),
                    },
                    user.id,
                )
                .await
                .map_err(|e| AppError::internal_error(format!("Rename failed: {}", e)))?;
        } else {
            // Different parent → move.
            let dest_parent = folder_service
                .get_folder_by_path(&dest_parent_internal)
                .await
                .map_err(|_| AppError::not_found("Destination parent not found"))?;

            use crate::application::dtos::folder_dto::MoveFolderDto;
            folder_service
                .move_folder(
                    &folder.id,
                    MoveFolderDto {
                        parent_id: Some(dest_parent.id.clone()),
                    },
                    user.id,
                )
                .await
                .map_err(|e| AppError::internal_error(format!("Move failed: {}", e)))?;

            // If the name changed too, rename.
            if folder.name != dest_name {
                use crate::application::dtos::folder_dto::RenameFolderDto;
                folder_service
                    .rename_folder(
                        &folder.id,
                        RenameFolderDto {
                            name: dest_name.to_string(),
                        },
                        user.id,
                    )
                    .await
                    .map_err(|e| AppError::internal_error(format!("Rename failed: {}", e)))?;
            }
        }

        return Ok(Response::builder()
            .status(StatusCode::CREATED)
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
fn extract_nc_subpath_from_dest(dest: &str, username: &str) -> Option<String> {
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

/// Generate a complete Nextcloud-compatible multistatus XML response.
#[allow(clippy::too_many_arguments)]
async fn write_nc_multistatus<W: std::io::Write>(
    writer: W,
    folder: Option<&FolderDto>,
    files: &[FileDto],
    subfolders: &[FolderDto],
    _request: &PropFindRequest,
    depth: &str,
    username: &str,
    subpath: &str,
    file_id_svc: Option<&Arc<NextcloudFileIdService>>,
    favorite_ids: &HashSet<String>,
) -> Result<(), String> {
    let mut xml = Writer::new(writer);

    // Root element with all required namespaces.
    let mut ms = BytesStart::new("d:multistatus");
    ms.push_attribute(("xmlns:d", "DAV:"));
    ms.push_attribute(("xmlns:oc", "http://owncloud.org/ns"));
    ms.push_attribute(("xmlns:nc", "http://nextcloud.org/ns"));
    ms.push_attribute(("xmlns:ocs", "http://open-collaboration-services.org/ns"));
    xml.write_event(Event::Start(ms)).xml_err()?;

    // Current folder entry.
    if let Some(f) = folder {
        let href = nc_href(username, subpath);
        let file_id = resolve_folder_id(file_id_svc, &f.id).await;
        let oc_id = file_id.map(|id| format_oc_id(id, file_id_svc));
        write_folder_response(
            &mut xml,
            f,
            &href,
            file_id,
            oc_id.as_deref(),
            username,
            favorite_ids,
        )?;
    }

    // When folder is None, files are the target resource itself (single-file
    // PROPFIND) and must always be emitted. When folder is Some, files/subfolders
    // are children and should only be listed when depth > 0.
    let emit_children = folder.is_none() || depth != "0";

    if emit_children {
        // Files.
        for file in files {
            let child_sub = if folder.is_none() {
                // Single-file PROPFIND — subpath already points to the file.
                subpath.to_string()
            } else if subpath.is_empty() {
                file.name.clone()
            } else {
                format!("{}/{}", subpath.trim_end_matches('/'), file.name)
            };
            let href = nc_href(username, &child_sub);
            let file_id = resolve_file_id(file_id_svc, &file.id).await;
            let oc_id = file_id.map(|id| format_oc_id(id, file_id_svc));
            write_file_response(
                &mut xml,
                file,
                &href,
                file_id,
                oc_id.as_deref(),
                username,
                favorite_ids,
            )?;
        }

        // Subfolders.
        for sf in subfolders {
            let child_sub = if subpath.is_empty() {
                sf.name.clone()
            } else {
                format!("{}/{}", subpath.trim_end_matches('/'), sf.name)
            };
            let href = format!("{}/", nc_href(username, &child_sub));
            let file_id = resolve_folder_id(file_id_svc, &sf.id).await;
            let oc_id = file_id.map(|id| format_oc_id(id, file_id_svc));
            write_folder_response(
                &mut xml,
                sf,
                &href,
                file_id,
                oc_id.as_deref(),
                username,
                favorite_ids,
            )?;
        }
    }

    xml.write_event(Event::End(BytesEnd::new("d:multistatus")))
        .xml_err()?;

    Ok(())
}

pub fn write_folder_response<W: std::io::Write>(
    xml: &mut Writer<W>,
    folder: &FolderDto,
    href: &str,
    file_id: Option<i64>,
    oc_id: Option<&str>,
    owner: &str,
    favorite_ids: &HashSet<String>,
) -> Result<(), String> {
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

    let created_at =
        chrono::DateTime::<Utc>::from_timestamp(timestamp_to_i64(folder.created_at), 0)
            .unwrap_or_else(Utc::now);
    let modified_at =
        chrono::DateTime::<Utc>::from_timestamp(timestamp_to_i64(folder.modified_at), 0)
            .unwrap_or_else(Utc::now);

    write_text_element(xml, "d:getlastmodified", &modified_at.to_rfc2822())?;
    write_text_element(xml, "d:getetag", &format!("\"{}\"", folder.id))?;
    write_text_element(xml, "d:getcontenttype", "httpd/unix-directory")?;
    write_text_element(xml, "d:getcontentlength", "0")?;
    write_text_element(xml, "d:creationdate", &created_at.to_rfc3339())?;

    // Nextcloud/ownCloud properties
    if let Some(id) = file_id {
        write_text_element(xml, "oc:fileid", &id.to_string())?;
    }
    if let Some(oid) = oc_id {
        write_text_element(xml, "oc:id", oid)?;
    }
    write_text_element(xml, "oc:permissions", "RGDNVCK")?;
    // Numeric share-permissions bitmask: Read=1 + Update=2 + Create=4 + Delete=8 + Share=16 = 31
    write_text_element(xml, "ocs:share-permissions", "31")?;
    write_text_element(xml, "oc:size", "0")?;
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

    xml.write_event(Event::End(BytesEnd::new("d:response")))
        .xml_err()?;

    Ok(())
}

pub fn write_file_response<W: std::io::Write>(
    xml: &mut Writer<W>,
    file: &FileDto,
    href: &str,
    file_id: Option<i64>,
    oc_id: Option<&str>,
    owner: &str,
    favorite_ids: &HashSet<String>,
) -> Result<(), String> {
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
    write_text_element(xml, "d:getcontentlength", &file.size.to_string())?;

    let created_at = chrono::DateTime::<Utc>::from_timestamp(timestamp_to_i64(file.created_at), 0)
        .unwrap_or_else(Utc::now);
    let modified_at =
        chrono::DateTime::<Utc>::from_timestamp(timestamp_to_i64(file.modified_at), 0)
            .unwrap_or_else(Utc::now);

    write_text_element(xml, "d:getlastmodified", &modified_at.to_rfc2822())?;
    write_text_element(xml, "d:getetag", &format!("\"{}\"", file.etag))?;
    write_text_element(xml, "d:creationdate", &created_at.to_rfc3339())?;

    // Nextcloud/ownCloud properties
    if let Some(id) = file_id {
        write_text_element(xml, "oc:fileid", &id.to_string())?;
    }
    if let Some(oid) = oc_id {
        write_text_element(xml, "oc:id", oid)?;
    }
    write_text_element(xml, "oc:permissions", "RGDNVW")?;
    // Numeric share-permissions bitmask: Read=1 + Update=2 + Delete=8 + Share=16 = 27
    write_text_element(xml, "ocs:share-permissions", "27")?;
    write_text_element(xml, "oc:size", &file.size.to_string())?;
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
    write_text_element(xml, "nc:creation_time", &file.created_at.to_string())?;
    write_text_element(xml, "nc:upload_time", &file.modified_at.to_string())?;

    xml.write_event(Event::End(BytesEnd::new("d:prop")))
        .xml_err()?;
    write_text_element(xml, "d:status", "HTTP/1.1 200 OK")?;
    xml.write_event(Event::End(BytesEnd::new("d:propstat")))
        .xml_err()?;

    xml.write_event(Event::End(BytesEnd::new("d:response")))
        .xml_err()?;

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

pub async fn resolve_file_id(
    svc: Option<&Arc<NextcloudFileIdService>>,
    file_uuid: &str,
) -> Option<i64> {
    let svc = svc?;
    svc.get_or_create_file_id(file_uuid).await.ok()
}

pub async fn resolve_folder_id(
    svc: Option<&Arc<NextcloudFileIdService>>,
    folder_uuid: &str,
) -> Option<i64> {
    let svc = svc?;
    svc.get_or_create_folder_id(folder_uuid).await.ok()
}

pub fn format_oc_id(id: i64, svc: Option<&Arc<NextcloudFileIdService>>) -> String {
    match svc {
        Some(s) => s.format_oc_id(id),
        None => format!("{:08}ocnca", id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── nc_to_internal_path ──

    #[test]
    fn test_empty_subpath_returns_home() {
        assert_eq!(
            nc_to_internal_path("alice", "").unwrap(),
            "My Folder - alice"
        );
    }

    #[test]
    fn test_subpath_appended() {
        assert_eq!(
            nc_to_internal_path("alice", "Documents/work").unwrap(),
            "My Folder - alice/Documents/work"
        );
    }

    #[test]
    fn test_strips_surrounding_slashes() {
        assert_eq!(
            nc_to_internal_path("alice", "/Photos/").unwrap(),
            "My Folder - alice/Photos"
        );
    }

    #[test]
    fn test_rejects_dot_dot_traversal() {
        assert!(nc_to_internal_path("alice", "../etc/passwd").is_err());
    }

    #[test]
    fn test_rejects_single_dot() {
        assert!(nc_to_internal_path("alice", "foo/./bar").is_err());
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
