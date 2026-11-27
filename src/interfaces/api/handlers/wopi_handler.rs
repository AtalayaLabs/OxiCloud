//! WOPI protocol handler.
//!
//! Implements the WOPI host endpoints called by document editors
//! (Collabora Online, OnlyOffice) to access and modify files.
//!
//! These endpoints use `?access_token=` query parameter auth, NOT the
//! regular JWT auth middleware.
//!
//! Reference: docs/config/wopi.md

use crate::interfaces::middleware::auth::AuthUser;
use axum::{
    Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{any, get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::file_ports::{FileRetrievalUseCase, FileUploadUseCase};
use crate::application::ports::storage_ports::StorageUsagePort;
use crate::application::services::wopi_lock_service::WopiLockService;
use crate::application::services::wopi_token_service::{WopiTokenClaims, WopiTokenService};
use crate::domain::errors::ErrorKind;
use crate::domain::repositories::drive_repository::DriveRepository;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::domain::services::path_service::{normalize_storage_name, normalize_storage_name_owned};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use crate::infrastructure::services::wopi_discovery_service::WopiDiscoveryService;
use crate::interfaces::upload_ingest;

/// Shared state for WOPI handlers.
#[derive(Clone)]
pub struct WopiState {
    pub token_service: Arc<WopiTokenService>,
    pub lock_service: Arc<WopiLockService>,
    pub discovery_service: Arc<WopiDiscoveryService>,
    pub app_state: Arc<crate::common::di::AppState>,
    /// Public base URL for host page origin and postMessage origin
    pub public_base_url: String,
    /// Base URL used for WOPISrc callbacks from Collabora to OxiCloud
    pub wopi_base_url: String,
}

/// Query parameter for WOPI access token.
#[derive(Deserialize)]
pub struct WopiTokenQuery {
    pub access_token: String,
}

/// CheckFileInfo response (WOPI spec).
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CheckFileInfoResponse {
    pub base_file_name: String,
    pub owner_id: String,
    pub size: u64,
    pub user_id: String,
    pub version: String,
    pub supports_locks: bool,
    pub supports_update: bool,
    pub supports_rename: bool,
    pub user_can_write: bool,
    pub user_can_not_write_relative: bool,
    pub user_friendly_name: String,
    pub post_message_origin: String,
    pub last_modified_time: String,
    pub close_url: String,
}

/// PutRelativeFile response (WOPI spec).
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PutRelativeFileResponse {
    pub name: String,
    pub url: String,
    pub host_view_url: String,
    pub host_edit_url: String,
}

/// Extract the lowercase file extension without the leading dot, or `""`
/// when the filename has no extension (e.g. `"Makefile"`, `".gitignore"`).
fn file_extension(name: &str) -> String {
    std::path::Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default()
}

/// Downgrade an `"edit"` request to `"view"` when the caller cannot write,
/// keeping `get_editor_url` and `host_page` aligned on the same discovery action.
fn effective_requested_action(requested: &str, can_write: bool) -> &str {
    if !can_write && requested == "edit" {
        "view"
    } else {
        requested
    }
}

/// Never mint a write-capable token when discovery resolved to a read-only
/// `"view"` action (e.g. OnlyOffice `.doc` fallback from `"edit"` to `"view"`).
fn effective_can_write(can_write: bool, resolved_action: &str) -> bool {
    can_write && resolved_action != "view"
}

/// Effective session action recorded in the token and returned to the SPA.
///
/// When a read-only request falls back to a discovery `"edit"` URL (e.g.
/// Collabora edit-only `.xlsx` opened with `action=view`), the session must
/// stay `"view"` so `host_page` does not re-elevate a write-capable caller
/// back to `can_write = true` and the SPA still shows the read-only badge.
fn effective_session_action(resolved_action: &str, token_can_write: bool) -> &str {
    if token_can_write {
        resolved_action
    } else {
        "view"
    }
}

/// Decode a UTF-7 (RFC 2152) string used by WOPI headers such as
/// `X-WOPI-SuggestedTarget` and `X-WOPI-RelativeTarget`.
fn decode_wopi_utf7(input: &str) -> Result<String, ()> {
    fn b64_val(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'+' {
            if !bytes[i].is_ascii() {
                return Err(());
            }
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        i += 1; // skip '+'
        if i < bytes.len() && bytes[i] == b'-' {
            out.push('+');
            i += 1;
            continue;
        }

        let start = i;
        let mut bit_buf: u32 = 0;
        let mut bit_count: u8 = 0;
        let mut units: Vec<u16> = Vec::new();

        while i < bytes.len() {
            if let Some(val) = b64_val(bytes[i]) {
                bit_buf = (bit_buf << 6) | val;
                bit_count += 6;
                if bit_count >= 16 {
                    bit_count -= 16;
                    units.push(((bit_buf >> bit_count) & 0xFFFF) as u16);
                    bit_buf &= (1u32 << bit_count) - 1;
                }
                i += 1;
            } else {
                break;
            }
        }

        if i == start {
            return Err(());
        }
        // RFC 2152: leftover bits at the end of a shifted sequence must be < 6 and zero,
        // and no half UTF-16 code unit (8+ bits) may remain.
        if bit_count >= 6 || bit_buf != 0 || units.is_empty() {
            return Err(());
        }

        for decoded in char::decode_utf16(units) {
            out.push(decoded.map_err(|_| ())?);
        }

        if i < bytes.len() && bytes[i] == b'-' {
            i += 1;
        }
    }

    Ok(out)
}

/// Encode a filename in RFC 2152 UTF-7 for WOPI response headers such as
/// `X-WOPI-ValidRelativeTarget`.
fn encode_wopi_utf7(input: &str) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len());
    let mut utf16_buf: Vec<u16> = Vec::new();

    let flush_shifted = |buf: &mut Vec<u16>, out: &mut String| {
        if buf.is_empty() {
            return;
        }
        out.push('+');
        let mut bit_buf: u32 = 0;
        let mut bit_count: u8 = 0;
        for &unit in buf.iter() {
            bit_buf = (bit_buf << 16) | u32::from(unit);
            bit_count += 16;
            while bit_count >= 6 {
                bit_count -= 6;
                let idx = ((bit_buf >> bit_count) & 0x3F) as usize;
                out.push(B64[idx] as char);
            }
        }
        if bit_count > 0 {
            let idx = ((bit_buf << (6 - bit_count)) & 0x3F) as usize;
            out.push(B64[idx] as char);
        }
        out.push('-');
        buf.clear();
    };

    for c in input.chars() {
        if c == '+' {
            flush_shifted(&mut utf16_buf, &mut out);
            out.push_str("+-");
        } else if c.is_ascii()
            && (c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    ' ' | '\'' | '(' | ')' | ',' | '-' | '.' | '/' | ':' | '?' | '_'
                ))
        {
            flush_shifted(&mut utf16_buf, &mut out);
            out.push(c);
        } else {
            let mut tmp = [0u16; 2];
            utf16_buf.extend_from_slice(c.encode_utf16(&mut tmp));
        }
    }
    flush_shifted(&mut utf16_buf, &mut out);
    out
}

/// Validate a WOPI relative filename against path traversal and control bytes,
/// normalizing to NFC to match `FileBlobWriteRepository`.
fn validate_relative_filename(name: &str) -> Result<String, ()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
        return Err(());
    }
    let normalized = normalize_storage_name_owned(name.to_string());
    if normalized.is_empty() || normalized == "." || normalized == ".." {
        return Err(());
    }
    Ok(normalized)
}

/// Resolve `X-WOPI-SuggestedTarget` against the source filename.
///
/// Per MS-WOPI:
/// - A leading `'.'` (e.g. `".docx"`) replaces the source file's extension.
/// - Otherwise the value is treated as a complete suggested filename.
fn resolve_suggested_target(source_name: &str, suggested: &str) -> Result<String, ()> {
    if let Some(ext) = suggested.strip_prefix('.') {
        if ext.is_empty()
            || ext.starts_with('.')
            || ext.contains('/')
            || ext.contains('\\')
            || ext.chars().any(char::is_control)
        {
            return Err(());
        }
        let stem = match source_name.rsplit_once('.') {
            Some((s, _)) if !s.is_empty() => s,
            _ => source_name,
        };
        validate_relative_filename(&format!("{stem}.{ext}"))
    } else {
        validate_relative_filename(suggested)
    }
}

/// Pick a non-colliding filename (`"stem (1).ext"`, `"stem (2).ext"`, …) against
/// lowercase filenames already present in the folder.
fn deduplicate_filename(candidate: &str, existing_lower: &HashSet<String>) -> String {
    if !existing_lower.contains(&candidate.to_lowercase()) {
        return candidate.to_string();
    }
    let (stem, ext_with_dot) = match candidate.rsplit_once('.') {
        Some((s, ext)) if !s.is_empty() => (s, format!(".{ext}")),
        _ => (candidate, String::new()),
    };
    for i in 1..=10_000u32 {
        let numbered = format!("{stem} ({i}){ext_with_dot}");
        if !existing_lower.contains(&numbered.to_lowercase()) {
            return numbered;
        }
    }
    format!("{stem} ({}){ext_with_dot}", uuid::Uuid::new_v4().simple())
}

/// Check whether `caller_uuid` has `Permission::Create` on the parent folder.
async fn can_create_in_parent_folder(
    authz: &PgAclEngine,
    caller_uuid: uuid::Uuid,
    folder_id: Option<&str>,
) -> bool {
    let Some(fid) = folder_id else {
        return false;
    };
    let Ok(folder_uuid) = uuid::Uuid::parse_str(fid) else {
        return false;
    };
    authz
        .check(
            Subject::User(caller_uuid),
            Permission::Create,
            Resource::Folder(folder_uuid),
        )
        .await
        .unwrap_or(false)
}

/// Enforce `Permission::Create` on the parent folder (emits `authz.denied` audit
/// log internally when denied).
async fn require_create_in_parent_folder(
    authz: &PgAclEngine,
    caller_uuid: uuid::Uuid,
    folder_id: Option<&str>,
) -> Result<uuid::Uuid, StatusCode> {
    let Some(fid) = folder_id else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let folder_uuid = uuid::Uuid::parse_str(fid).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    authz
        .require(
            Subject::User(caller_uuid),
            Permission::Create,
            Resource::Folder(folder_uuid),
        )
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    Ok(folder_uuid)
}

/// Enforce that the WOPI caller (`claims.sub`) still has `perm` on the
/// file at redemption time — not just at token-mint time.
///
/// **Why every verb needs this.** WOPI tokens are validated locally
/// (HMAC over claims), so a token that was legitimately minted stays
/// verify-able until its TTL. If a grant is revoked after mint, or the
/// token was minted for view but is used to POST content, the token's
/// signature alone doesn't catch it. This helper re-checks against the
/// live authorization engine on every verb — the memory note
/// `wopi-authz-bypass` calls out the class of bugs this fences.
///
/// Returns 404 (anti-enumeration — same shape as "file doesn't exist")
/// on both bad UUID and authorization denial. The engine emits a
/// structured `audit` line on denial internally, so ops sees the real
/// reason without the attacker being able to distinguish "gone" from
/// "revoked".
/// Shared id parsing for the WOPI authz paths: a malformed caller sub is a
/// bad token (401), a malformed file id can't exist (404, anti-enum).
fn parse_wopi_ids(caller_sub: &str, file_id: &str) -> Result<(uuid::Uuid, uuid::Uuid), StatusCode> {
    let caller_uuid = uuid::Uuid::parse_str(caller_sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    let file_uuid = uuid::Uuid::parse_str(file_id).map_err(|_| StatusCode::NOT_FOUND)?;
    Ok((caller_uuid, file_uuid))
}

async fn require_wopi_perm(
    authz: &PgAclEngine,
    caller_sub: &str,
    file_id: &str,
    perm: Permission,
) -> Result<(uuid::Uuid, uuid::Uuid), StatusCode> {
    let (caller_uuid, file_uuid) = parse_wopi_ids(caller_sub, file_id)?;
    authz
        .require(Subject::User(caller_uuid), perm, Resource::File(file_uuid))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok((caller_uuid, file_uuid))
}

/// GET /wopi/files/{file_id} — CheckFileInfo
async fn check_file_info(
    Path(file_id): Path<String>,
    Query(token_query): Query<WopiTokenQuery>,
    State(state): State<WopiState>,
) -> Response {
    let claims = match state
        .token_service
        .validate_token(&token_query.access_token)
    {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if claims.file_id != file_id {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Redemption-time authz: even with a valid token, the caller must
    // still hold Read on this file. Catches revoked-grant-mid-session.
    //
    // The Read gate, the metadata fetch and the Update probe are three
    // independent lookups keyed only off (caller, file) — overlapped with
    // `tokio::join!` (benches/ROUND12.md §5). Results are evaluated in the
    // original precedence: Read gate first, then file existence.
    let (caller_uuid, file_uuid) = match parse_wopi_ids(&claims.sub, &file_id) {
        Ok(ids) => ids,
        Err(status) => return status.into_response(),
    };
    let is_convert_token = claims.action.as_deref() == Some("convert");
    let authz = state.app_state.authorization.as_ref();
    let (read_gate, file, can_write_now) = tokio::join!(
        authz.require(
            Subject::User(caller_uuid),
            Permission::Read,
            Resource::File(file_uuid)
        ),
        state
            .app_state
            .applications
            .file_retrieval_service
            .get_file(&file_id),
        // `user_can_write` = actual current Update permission ∧ token's
        // can_write flag (for convert sessions, parent folder Create decides
        // relative conversion ability without requiring Update on the legacy
        // source file). The stricter gates in put_file / put_relative_file are
        // the actual enforcement; these CheckFileInfo fields are UI hints.
        async {
            if claims.can_write && !is_convert_token {
                authz
                    .check(
                        Subject::User(caller_uuid),
                        Permission::Update,
                        Resource::File(file_uuid),
                    )
                    .await
                    .unwrap_or(false)
            } else {
                false
            }
        }
    );
    if read_gate.is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file = match file {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let can_create_in_parent = if claims.can_write && (is_convert_token || can_write_now) {
        can_create_in_parent_folder(authz, caller_uuid, file.folder_id.as_deref()).await
    } else {
        false
    };

    let (supports_update, user_can_write, user_can_not_write_relative) = if is_convert_token {
        let can_convert_now = claims.can_write && can_create_in_parent;
        (can_convert_now, can_convert_now, !can_convert_now)
    } else {
        (
            can_write_now,
            can_write_now,
            !(can_write_now && can_create_in_parent),
        )
    };

    // `file.modified_at` is now `DateTime<Utc>` — format directly.
    let last_modified = file.modified_at.to_rfc3339();

    let response = CheckFileInfoResponse {
        base_file_name: file.name.clone(),
        // WOPI's `OwnerId` field is required. Post-D7 the DTO no
        // longer carries `owner_id`; fall back to `created_by`
        // (§14 provenance) with the requesting user as a final default.
        owner_id: file
            .created_by
            .map(|u| u.to_string())
            .unwrap_or_else(|| claims.sub.clone()),
        size: file.size,
        user_id: claims.sub.clone(),
        version: file.modified_at.to_string(),
        supports_locks: true,
        supports_update,
        supports_rename: false,
        user_can_write,
        user_can_not_write_relative,
        user_friendly_name: claims.username.clone(),
        post_message_origin: state.public_base_url.clone(),
        last_modified_time: last_modified,
        close_url: state.public_base_url.clone(),
    };

    axum::Json(response).into_response()
}

/// GET /wopi/files/{file_id}/contents — GetFile
///
/// Streams the file content to Collabora/OnlyOffice in 64 KB chunks.
/// Memory usage is constant (~64 KB) regardless of file size.
async fn get_file(
    Path(file_id): Path<String>,
    Query(token_query): Query<WopiTokenQuery>,
    State(state): State<WopiState>,
) -> Response {
    let claims = match state
        .token_service
        .validate_token(&token_query.access_token)
    {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if claims.file_id != file_id {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Redemption-time authz — see require_wopi_perm docstring.
    if let Err(status) = require_wopi_perm(
        state.app_state.authorization.as_ref(),
        &claims.sub,
        &file_id,
        Permission::Read,
    )
    .await
    {
        return status.into_response();
    }

    match state
        .app_state
        .applications
        .file_retrieval_service
        .get_file_stream(&file_id)
        .await
    {
        Ok(stream) => {
            let body = axum::body::Body::from_stream(std::pin::Pin::from(stream));
            (StatusCode::OK, body).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// POST /wopi/files/{file_id}/contents — PutFile
///
/// **Streaming implementation**: the request body is spooled to a temp file
/// with incremental BLAKE3 hashing.  Peak RAM usage is ~256 KB regardless
/// of file size (previously buffered the entire body as `Bytes`).
async fn put_file(
    Path(file_id): Path<String>,
    Query(token_query): Query<WopiTokenQuery>,
    headers: HeaderMap,
    State(state): State<WopiState>,
    req: Request<Body>,
) -> Response {
    let claims = match state
        .token_service
        .validate_token(&token_query.access_token)
    {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if claims.file_id != file_id || !claims.can_write {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    if claims.action.as_deref() == Some("convert") {
        tracing::info!(
            target: "audit",
            event = "wopi.put_file_rejected",
            reason = "convert_token_cannot_overwrite",
            caller_id = %claims.sub,
            file_id = %file_id,
            "👮🏻‍♂️ WOPI PutFile rejected: convert token cannot overwrite source file"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Redemption-time authz: the token says the caller could write when
    // it was minted, but Update permission may have been revoked since.
    // Re-check now so a stale write-capable token can't survive a
    // downgrade / share removal / drive-membership change until its TTL.
    if let Err(status) = require_wopi_perm(
        state.app_state.authorization.as_ref(),
        &claims.sub,
        &file_id,
        Permission::Update,
    )
    .await
    {
        return status.into_response();
    }

    // Check lock
    let request_lock = headers
        .get("X-WOPI-Lock")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let current_lock = state.lock_service.get_lock(&file_id).await;

    if let Some(ref current) = current_lock {
        match &request_lock {
            Some(req_lock) if req_lock == current => {
                // Lock matches — proceed
            }
            _ => {
                // Lock mismatch
                return (
                    StatusCode::CONFLICT,
                    [("X-WOPI-Lock", current.as_str())],
                    "Lock mismatch",
                )
                    .into_response();
            }
        }
    }

    // Get file metadata for the path
    let file = match state
        .app_state
        .applications
        .file_retrieval_service
        .get_file(&file_id)
        .await
    {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    // ── Streaming ingest: body → CDC chunk store (no temp file) ──
    let content_type = file.mime_type.clone();
    let ingested = match crate::interfaces::upload_ingest::ingest_body_to_cas(
        req.into_body(),
        &state.app_state.core.blob_handler,
        &file.name,
        &content_type,
        usize::MAX,
    )
    .await
    {
        Ok(ingested) => ingested,
        Err(e) => {
            tracing::error!("WOPI PutFile: ingest failed: {}", e.message);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // ── Atomic store: swap the file row onto the ingested blob ──
    // `drive_id` scopes the path-based lookups in
    // `update_file_streaming_with_perms` post-D0.
    //
    // AuthZ audit #18 (2026-07-12): the pre-fix path resolved
    // `drive_id` via `find_default_for_user(claims_sub_uuid)` —
    // ALWAYS the caller's own default personal drive, regardless of
    // where the file actually lived. Shared-drive edits either
    // misrouted the write into the caller's personal drive (if the
    // filename happened to collide with a personal-drive path) or
    // 500'd on the parent-folder lookup. Resolve from the file's
    // own parent folder instead — one PK probe, returns the drive
    // the file genuinely belongs to. Also unlocks shared-drive WOPI
    // editing.
    let claims_sub_uuid = match uuid::Uuid::parse_str(&claims.sub) {
        Ok(u) => u,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let Some(folder_id_str) = file.folder_id.as_deref() else {
        // Files always live under a folder (drive-root files use the
        // drive-root folder id). A `None` here means the file entity
        // is malformed — safest is a 500.
        tracing::error!(
            "WOPI PutFile: file {} has no parent folder id — cannot resolve drive",
            file_id
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let folder_uuid = match uuid::Uuid::parse_str(folder_id_str) {
        Ok(u) => u,
        Err(_) => {
            tracing::error!(
                "WOPI PutFile: file {} parent folder id '{}' is not a UUID",
                file_id,
                folder_id_str
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let drive_id = match state
        .app_state
        .drive_repo
        .drive_id_for_folder(folder_uuid)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(
                "WOPI PutFile: drive-id lookup for folder {} failed: {:?}",
                folder_uuid,
                e
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let result = state
        .app_state
        .applications
        .file_upload_service
        .update_file_streaming_with_perms(
            &file.path,
            drive_id,
            ingested.stored(),
            &content_type,
            None,
            claims_sub_uuid,
            None,
        )
        .await;

    match result {
        Ok(_file_dto) => StatusCode::OK.into_response(),
        Err(e) => {
            tracing::error!("WOPI PutFile failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Enforce both user storage quota and per-drive quota for `additional_bytes`,
/// discarding `ingested` and returning `507 Insufficient Storage` on failure.
async fn check_relative_quotas(
    state: &WopiState,
    caller_uuid: uuid::Uuid,
    folder_uuid: uuid::Uuid,
    additional_bytes: u64,
    ingested: &upload_ingest::IngestedBlob,
) -> Result<(), Response> {
    if additional_bytes == 0 {
        return Ok(());
    }
    let Some(storage_svc) = state.app_state.storage_usage_service.as_ref() else {
        return Ok(());
    };
    let dedup = &state.app_state.core.blob_handler;
    if let Err(err) = storage_svc
        .check_storage_quota(caller_uuid, additional_bytes)
        .await
    {
        tracing::warn!(
            "⛔ WOPI PutRelativeFile rejected (user quota): caller={}, size={} — {}",
            caller_uuid,
            additional_bytes,
            err
        );
        upload_ingest::discard_ingested(dedup, ingested).await;
        return Err(StatusCode::INSUFFICIENT_STORAGE.into_response());
    }
    if let Err(err) = storage_svc
        .check_drive_quota_by_folder(folder_uuid, additional_bytes)
        .await
    {
        tracing::warn!(
            "⛔ WOPI PutRelativeFile rejected (drive quota): caller={}, folder={}, size={} — {}",
            caller_uuid,
            folder_uuid,
            additional_bytes,
            err
        );
        upload_ingest::discard_ingested(dedup, ingested).await;
        return Err(StatusCode::INSUFFICIENT_STORAGE.into_response());
    }
    Ok(())
}

fn map_ingest_err(e: crate::interfaces::errors::AppError) -> Response {
    if e.status_code == StatusCode::PAYLOAD_TOO_LARGE {
        StatusCode::PAYLOAD_TOO_LARGE.into_response()
    } else {
        tracing::error!("WOPI PutRelativeFile: ingest failed: {}", e.message);
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

fn relative_conflict_response(
    valid_target_utf7: Option<&str>,
    existing_lock: Option<&str>,
) -> Response {
    let mut headers = HeaderMap::new();
    if let Some(valid) = valid_target_utf7
        && let Ok(val) = HeaderValue::from_str(valid)
    {
        headers.insert(
            axum::http::HeaderName::from_static("x-wopi-validrelativetarget"),
            val,
        );
    }
    if let Some(lock) = existing_lock
        && let Ok(val) = HeaderValue::from_str(lock)
    {
        headers.insert(axum::http::HeaderName::from_static("x-wopi-lock"), val);
    }
    (StatusCode::CONFLICT, headers, "Target file already exists").into_response()
}

async fn ingest_and_create_relative_file(
    state: &WopiState,
    target_name: String,
    folder_id_str: &str,
    folder_uuid: uuid::Uuid,
    caller_uuid: uuid::Uuid,
    body: Body,
) -> Result<FileDto, Response> {
    let max_bytes = state.app_state.core.config.storage.max_upload_size;
    let ingested = upload_ingest::ingest_body_to_cas(
        body,
        &state.app_state.core.blob_handler,
        &target_name,
        "application/octet-stream",
        max_bytes,
    )
    .await
    .map_err(map_ingest_err)?;

    check_relative_quotas(state, caller_uuid, folder_uuid, ingested.size, &ingested).await?;

    state
        .app_state
        .applications
        .file_upload_service
        .upload_file_streaming_with_perms(
            target_name,
            Some(folder_id_str.to_string()),
            ingested.content_type.clone(),
            ingested.stored(),
            caller_uuid,
            None,
        )
        .await
        .map_err(|e| {
            tracing::error!("WOPI PutRelativeFile create failed: {}", e);
            match e.kind {
                ErrorKind::AlreadyExists | ErrorKind::Conflict => {
                    StatusCode::CONFLICT.into_response()
                }
                ErrorKind::AccessDenied => StatusCode::FORBIDDEN.into_response(),
                ErrorKind::NotFound => StatusCode::NOT_FOUND.into_response(),
                ErrorKind::QuotaExceeded => StatusCode::INSUFFICIENT_STORAGE.into_response(),
                _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        })
}

async fn ingest_and_overwrite_relative_file(
    state: &WopiState,
    existing: &FileDto,
    drive_id: uuid::Uuid,
    folder_uuid: uuid::Uuid,
    caller_uuid: uuid::Uuid,
    body: Body,
) -> Result<FileDto, Response> {
    let max_bytes = state.app_state.core.config.storage.max_upload_size;
    let ingested = upload_ingest::ingest_body_to_cas(
        body,
        &state.app_state.core.blob_handler,
        &existing.name,
        "application/octet-stream",
        max_bytes,
    )
    .await
    .map_err(map_ingest_err)?;

    let additional_bytes = ingested.size.saturating_sub(existing.size);
    check_relative_quotas(state, caller_uuid, folder_uuid, additional_bytes, &ingested).await?;

    state
        .app_state
        .applications
        .file_upload_service
        .update_file_streaming_with_perms(
            &existing.path,
            drive_id,
            ingested.stored(),
            &ingested.content_type,
            None,
            caller_uuid,
            None,
        )
        .await
        .map_err(|e| {
            tracing::error!("WOPI PutRelativeFile overwrite failed: {}", e);
            match e.kind {
                ErrorKind::AccessDenied => StatusCode::FORBIDDEN.into_response(),
                ErrorKind::NotFound => StatusCode::NOT_FOUND.into_response(),
                ErrorKind::Conflict => StatusCode::CONFLICT.into_response(),
                ErrorKind::QuotaExceeded => StatusCode::INSUFFICIENT_STORAGE.into_response(),
                _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        })
}

/// Handle `X-WOPI-Override: PUT_RELATIVE` (`POST /wopi/files/{file_id}`).
///
/// Creates a related file in the source file's folder — used by OnlyOffice's
/// `convert-and-edit` flow (`.doc` -> `.docx`) and "Save Copy As".
async fn put_relative_file(
    file_id: String,
    claims: WopiTokenClaims,
    headers: HeaderMap,
    state: WopiState,
    req: Request<Body>,
) -> Response {
    if !claims.can_write {
        tracing::info!(
            target: "audit",
            event = "wopi.put_relative_rejected",
            reason = "read_only_token",
            caller_id = %claims.sub,
            file_id = %file_id,
            "👮🏻‍♂️ WOPI PutRelativeFile rejected: read-only token"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let authz = state.app_state.authorization.as_ref();
    let (caller_uuid, _) =
        match require_wopi_perm(authz, &claims.sub, &file_id, Permission::Read).await {
            Ok(ids) => ids,
            Err(status) => return status.into_response(),
        };

    let file = match state
        .app_state
        .applications
        .file_retrieval_service
        .get_file(&file_id)
        .await
    {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(folder_id_str) = file.folder_id.as_deref() else {
        tracing::error!(
            "WOPI PutRelativeFile: file {} has no parent folder id",
            file_id
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(folder_uuid) = uuid::Uuid::parse_str(folder_id_str) else {
        tracing::error!(
            "WOPI PutRelativeFile: file {} parent folder id '{}' is not a UUID",
            file_id,
            folder_id_str
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let drive_id = match state
        .app_state
        .drive_repo
        .drive_id_for_folder(folder_uuid)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(
                "WOPI PutRelativeFile: drive-id lookup for folder {} failed: {:?}",
                folder_uuid,
                e
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let suggested_hdr = headers
        .get("X-WOPI-SuggestedTarget")
        .and_then(|v| v.to_str().ok());
    let relative_hdr = headers
        .get("X-WOPI-RelativeTarget")
        .and_then(|v| v.to_str().ok());

    // MS-WOPI: SuggestedTarget and RelativeTarget are mutually exclusive.
    let (is_suggested, raw_target) = match (suggested_hdr, relative_hdr) {
        (Some(s), None) => (true, s),
        (None, Some(r)) => (false, r),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };

    let decoded_target = match decode_wopi_utf7(raw_target) {
        Ok(s) => s,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let existing_files = match state
        .app_state
        .applications
        .file_retrieval_service
        .list_files(Some(folder_id_str))
        .await
    {
        Ok(list) => list,
        Err(e) => {
            tracing::error!(
                "WOPI PutRelativeFile: failed to list folder {}: {}",
                folder_id_str,
                e
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut existing_by_exact: HashMap<String, FileDto> =
        HashMap::with_capacity(existing_files.len());
    let mut existing_lower: HashSet<String> = HashSet::with_capacity(existing_files.len());
    for item in existing_files {
        let normalized = normalize_storage_name(&item.name);
        existing_lower.insert(normalized.to_lowercase());
        existing_by_exact.insert(normalized, item);
    }

    let created = if is_suggested {
        let Ok(candidate) = resolve_suggested_target(&file.name, &decoded_target) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if let Err(status) =
            require_create_in_parent_folder(authz, caller_uuid, Some(folder_id_str)).await
        {
            return status.into_response();
        }
        let target_name = deduplicate_filename(&candidate, &existing_lower);
        match ingest_and_create_relative_file(
            &state,
            target_name,
            folder_id_str,
            folder_uuid,
            caller_uuid,
            req.into_body(),
        )
        .await
        {
            Ok(f) => f,
            Err(resp) => return resp,
        }
    } else {
        let Ok(target_name) = validate_relative_filename(&decoded_target) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let overwrite = headers
            .get("X-WOPI-OverwriteRelativeTarget")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));

        if let Some(existing) = existing_by_exact.get(&target_name) {
            let existing_lock = state.lock_service.get_lock(&existing.id).await;
            if !overwrite {
                let valid = encode_wopi_utf7(&deduplicate_filename(&target_name, &existing_lower));
                return relative_conflict_response(Some(&valid), existing_lock.as_deref());
            }
            if let Some(ref lock_val) = existing_lock {
                return relative_conflict_response(None, Some(lock_val));
            }
            let Ok(existing_uuid) = uuid::Uuid::parse_str(&existing.id) else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            if authz
                .require(
                    Subject::User(caller_uuid),
                    Permission::Update,
                    Resource::File(existing_uuid),
                )
                .await
                .is_err()
            {
                return StatusCode::FORBIDDEN.into_response();
            }
            match ingest_and_overwrite_relative_file(
                &state,
                existing,
                drive_id,
                folder_uuid,
                caller_uuid,
                req.into_body(),
            )
            .await
            {
                Ok(f) => f,
                Err(resp) => return resp,
            }
        } else {
            if let Err(status) =
                require_create_in_parent_folder(authz, caller_uuid, Some(folder_id_str)).await
            {
                return status.into_response();
            }
            match ingest_and_create_relative_file(
                &state,
                target_name,
                folder_id_str,
                folder_uuid,
                caller_uuid,
                req.into_body(),
            )
            .await
            {
                Ok(f) => f,
                Err(resp) => return resp,
            }
        }
    };

    let (new_token, _) = match state.token_service.generate_token_with_action(
        &created.id,
        &claims.sub,
        &claims.username,
        true,
        Some("edit"),
        None,
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("WOPI PutRelativeFile token generation failed: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let wopi_base = state.wopi_base_url.trim_end_matches('/');
    let public_base = state.public_base_url.trim_end_matches('/');
    let host_url = format!("{public_base}/files/{folder_id_str}?file={}", created.id);

    axum::Json(PutRelativeFileResponse {
        name: created.name,
        url: format!(
            "{wopi_base}/wopi/files/{}?access_token={new_token}",
            created.id
        ),
        host_view_url: host_url.clone(),
        host_edit_url: host_url,
    })
    .into_response()
}

/// POST /wopi/files/{file_id} — Dispatches lock & relative-file operations based on X-WOPI-Override header
async fn file_operations(
    Path(file_id): Path<String>,
    Query(token_query): Query<WopiTokenQuery>,
    headers: HeaderMap,
    State(state): State<WopiState>,
    req: Request<Body>,
) -> Response {
    let claims = match state
        .token_service
        .validate_token(&token_query.access_token)
    {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if claims.file_id != file_id {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let override_header = headers
        .get("X-WOPI-Override")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if override_header == "PUT_RELATIVE" {
        return put_relative_file(file_id, claims, headers, state, req).await;
    }

    if claims.action.as_deref() == Some("convert") {
        tracing::info!(
            target: "audit",
            event = "wopi.lock_rejected",
            reason = "convert_token_cannot_lock",
            caller_id = %claims.sub,
            file_id = %file_id,
            "👮🏻‍♂️ WOPI lock operation rejected: convert token cannot lock source file"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Every lock op mutates shared state (LOCK / UNLOCK / REFRESH_LOCK
    // change the lock; GET_LOCK reads it but the read is only useful
    // to a caller who could subsequently take a write action — so gate
    // on Update uniformly rather than splitting per-op). A Viewer with
    // a stale token must not be able to hold or contend for a lock.
    if let Err(status) = require_wopi_perm(
        state.app_state.authorization.as_ref(),
        &claims.sub,
        &file_id,
        Permission::Update,
    )
    .await
    {
        return status.into_response();
    }

    let lock_id = headers
        .get("X-WOPI-Lock")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    match override_header {
        "LOCK" => {
            if lock_id.is_empty() {
                return StatusCode::BAD_REQUEST.into_response();
            }
            match state.lock_service.lock(&file_id, lock_id).await {
                Ok(()) => StatusCode::OK.into_response(),
                Err(conflict) => (
                    StatusCode::CONFLICT,
                    [("X-WOPI-Lock", conflict.existing_lock_id.as_str())],
                    "",
                )
                    .into_response(),
            }
        }
        "UNLOCK" => match state.lock_service.unlock(&file_id, lock_id).await {
            Ok(()) => StatusCode::OK.into_response(),
            Err(conflict) => (
                StatusCode::CONFLICT,
                [("X-WOPI-Lock", conflict.existing_lock_id.as_str())],
                "",
            )
                .into_response(),
        },
        "REFRESH_LOCK" => match state.lock_service.refresh_lock(&file_id, lock_id).await {
            Ok(()) => StatusCode::OK.into_response(),
            Err(conflict) => (
                StatusCode::CONFLICT,
                [("X-WOPI-Lock", conflict.existing_lock_id.as_str())],
                "",
            )
                .into_response(),
        },
        "GET_LOCK" => {
            let current = state.lock_service.get_lock(&file_id).await;
            let lock_val = current.unwrap_or_default();
            (StatusCode::OK, [("X-WOPI-Lock", lock_val.as_str())], "").into_response()
        }
        _ => (StatusCode::NOT_IMPLEMENTED, "Unknown WOPI override").into_response(),
    }
}

/// Parameters for the editor URL API endpoint.
#[derive(Deserialize)]
pub struct EditorUrlParams {
    pub file_id: String,
    #[serde(default = "default_action")]
    pub action: String,
    #[serde(default)]
    pub target_ext: Option<String>,
}

fn default_action() -> String {
    "edit".to_string()
}

/// Response from the editor URL API endpoint.
#[derive(Serialize)]
pub struct EditorUrlResponse {
    pub editor_url: String,
    pub access_token: String,
    pub access_token_ttl: i64,
    pub action: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub convert_targets: Vec<String>,
}

/// Resolve the WOPI mint target: gate on real permissions and derive
/// the `can_write` flag from the caller's ACTUAL Update rights.
///
/// Prior behaviour used a naive `requested_action != "view"` heuristic
/// so a Viewer clicking "Edit in Collabora" received a write-capable
/// token, promoting themselves to Editor for the token's TTL. The
/// memory note `wopi-authz-bypass` fix #12 calls this out explicitly.
///
/// Contract:
///
/// 1. **Read** is the bar to open the file in any mode. If the caller
///    has no Read grant, return 404 (anti-enum — same shape as "no such
///    file").
/// 2. **Update** determines the returned `can_write` bit — INDEPENDENT
///    of what the client's `requested_action` said. A Viewer who
///    requested `action=edit` gets `can_write=false` and Collabora
///    opens in view mode; the token stays authorised for view-only
///    ops and put_file will 404 at redemption regardless.
/// 3. `requested_action == "view"` is respected as a downgrade — an
///    Editor can explicitly request view mode (co-browsing a doc
///    without accidentally editing) and get `can_write=false`.
///
/// The `PgAclEngine::require`/`check` calls emit structured audit
/// lines on denial (`authz.denied` event), so a Viewer's "edit"
/// attempt shows up in the audit stream as a rejected Update check.
async fn authorize_wopi_access<S: FileRetrievalUseCase>(
    authz: &PgAclEngine,
    file_retrieval: &S,
    file_id: &str,
    caller_id: uuid::Uuid,
    requested_action: &str,
) -> Result<(crate::application::dtos::file_dto::FileDto, bool), StatusCode> {
    let file_uuid = uuid::Uuid::parse_str(file_id).map_err(|_| StatusCode::NOT_FOUND)?;

    // The Read gate (step 1), the metadata fetch and the Update probe
    // (step 2) are independent — overlapped with `tokio::join!`
    // (benches/ROUND12.md §5); results evaluated in the original order.
    //
    // Step 2 rationale — can_write reflects real Update, not the client's
    // action-string. `check` returns bool without throwing; failure
    // just means the caller lacks Update, so we degrade the token to
    // read-only. Deliberately no `require` there — a Viewer opening
    // the file is legitimate; only the write claim is suppressed.
    let (read_gate, file, has_update) = tokio::join!(
        authz.require(
            Subject::User(caller_id),
            Permission::Read,
            Resource::File(file_uuid),
        ),
        file_retrieval.get_file(file_id),
        authz.check(
            Subject::User(caller_id),
            Permission::Update,
            Resource::File(file_uuid),
        )
    );
    read_gate.map_err(|_| StatusCode::NOT_FOUND)?;
    let file = file.map_err(|_| StatusCode::NOT_FOUND)?;
    let has_update = has_update.unwrap_or(false);

    // Step 3 — allow explicit view-mode downgrade for Editors.
    let can_write = has_update && requested_action != "view";
    Ok((file, can_write))
}

struct ResolvedSession {
    file: FileDto,
    editor_url: String,
    action: String,
    convert_targets: Vec<String>,
    access_token: String,
    access_token_ttl: i64,
}

struct SessionLookup<'a> {
    file_id: &'a str,
    caller_id: uuid::Uuid,
    username: &'a str,
    requested_action: &'a str,
    target_ext: Option<&'a str>,
    include_convert_targets: bool,
}

/// Resolve permissions, discovery URL, conversion targets, and freshly minted
/// WOPI token for both `get_editor_url` and `host_page`.
async fn resolve_editor_session(
    state: &WopiState,
    req: SessionLookup<'_>,
) -> Result<ResolvedSession, Response> {
    let authz = state.app_state.authorization.as_ref();
    let file_retrieval = state.app_state.applications.file_retrieval_service.as_ref();
    let wopi_src = format!("{}/wopi/files/{}", state.wopi_base_url, req.file_id);

    if req.requested_action == "convert" {
        let file_uuid = uuid::Uuid::parse_str(req.file_id)
            .map_err(|_| StatusCode::NOT_FOUND.into_response())?;
        let (read_gate, file) = tokio::join!(
            authz.require(
                Subject::User(req.caller_id),
                Permission::Read,
                Resource::File(file_uuid),
            ),
            file_retrieval.get_file(req.file_id),
        );
        read_gate.map_err(|_| StatusCode::NOT_FOUND.into_response())?;
        let file = file.map_err(|_| StatusCode::NOT_FOUND.into_response())?;

        require_create_in_parent_folder(authz, req.caller_id, file.folder_id.as_deref())
            .await
            .map_err(IntoResponse::into_response)?;

        let extension = file_extension(&file.name);
        let resolved = match state
            .discovery_service
            .resolve_action_url(&extension, "convert", req.target_ext, &wopi_src)
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("No conversion available for .{} files", extension),
                )
                    .into_response());
            }
            Err(e) => {
                tracing::error!("WOPI discovery error: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        };

        let (access_token, access_token_ttl) = state
            .token_service
            .generate_token_with_action(
                req.file_id,
                &req.caller_id.to_string(),
                req.username,
                true,
                Some("convert"),
                resolved.target_ext.as_deref(),
            )
            .map_err(|e| {
                tracing::error!("Failed to generate WOPI convert token: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?;

        return Ok(ResolvedSession {
            file,
            editor_url: resolved.url,
            action: resolved.action,
            convert_targets: Vec::new(),
            access_token,
            access_token_ttl,
        });
    }

    let (file, can_write) = authorize_wopi_access(
        authz,
        file_retrieval,
        req.file_id,
        req.caller_id,
        req.requested_action,
    )
    .await
    .map_err(IntoResponse::into_response)?;

    let extension = file_extension(&file.name);
    let effective_req = effective_requested_action(req.requested_action, can_write);

    let resolved = match state
        .discovery_service
        .resolve_action_url(&extension, effective_req, None, &wopi_src)
        .await
    {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("No editor available for .{} files", extension),
            )
                .into_response());
        }
        Err(e) => {
            tracing::error!("WOPI discovery error: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };

    let token_can_write = effective_can_write(can_write, &resolved.action);
    let session_action = effective_session_action(&resolved.action, token_can_write);

    let convert_targets = if req.include_convert_targets
        && resolved.action == "view"
        && can_create_in_parent_folder(authz, req.caller_id, file.folder_id.as_deref()).await
    {
        state
            .discovery_service
            .get_convert_targets(&extension)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let (access_token, access_token_ttl) = state
        .token_service
        .generate_token_with_action(
            req.file_id,
            &req.caller_id.to_string(),
            req.username,
            token_can_write,
            Some(session_action),
            None,
        )
        .map_err(|e| {
            tracing::error!("Failed to generate WOPI token: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;

    Ok(ResolvedSession {
        file,
        editor_url: resolved.url,
        action: session_action.to_string(),
        convert_targets,
        access_token,
        access_token_ttl,
    })
}

/// GET /api/wopi/editor-url — Returns the editor iframe URL + WOPI token.
///
/// This endpoint is behind normal auth middleware. The authenticated user
/// requests a WOPI session for a specific file.
pub async fn get_editor_url(
    auth_user: AuthUser,
    Query(params): Query<EditorUrlParams>,
    State(state): State<WopiState>,
) -> Response {
    let session = match resolve_editor_session(
        &state,
        SessionLookup {
            file_id: &params.file_id,
            caller_id: auth_user.id,
            username: &auth_user.username,
            requested_action: &params.action,
            target_ext: params.target_ext.as_deref(),
            include_convert_targets: true,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    axum::Json(EditorUrlResponse {
        editor_url: session.editor_url,
        access_token: session.access_token,
        access_token_ttl: session.access_token_ttl,
        action: session.action,
        convert_targets: session.convert_targets,
    })
    .into_response()
}

/// GET /wopi/edit/{file_id} — Server-rendered host page for new-tab editing.
///
/// Returns a minimal HTML page that POSTs the access token to the editor iframe.
async fn host_page(
    Path(file_id): Path<String>,
    Query(token_query): Query<WopiTokenQuery>,
    State(state): State<WopiState>,
) -> Response {
    let claims = match state
        .token_service
        .validate_token(&token_query.access_token)
    {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if claims.file_id != file_id {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let caller_uuid = match uuid::Uuid::parse_str(&claims.sub) {
        Ok(u) => u,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let requested_action =
        claims
            .action
            .as_deref()
            .unwrap_or(if claims.can_write { "edit" } else { "view" });

    if requested_action == "convert" && !claims.can_write {
        tracing::info!(
            target: "audit",
            event = "wopi.host_page_rejected",
            reason = "read_only_convert_token",
            caller_id = %claims.sub,
            file_id = %file_id,
            "👮🏻‍♂️ WOPI host page rejected: convert action requires write-capable token"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let session = match resolve_editor_session(
        &state,
        SessionLookup {
            file_id: &file_id,
            caller_id: caller_uuid,
            username: &claims.username,
            requested_action,
            target_ext: claims.target_ext.as_deref(),
            include_convert_targets: false,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let file = session.file;
    let editor_url = session.editor_url;
    let token = session.access_token;
    let ttl = session.access_token_ttl;

    // Escape HTML entities in anything interpolated into the markup
    // below. `editor_url` comes from the admin-configured discovery
    // document, so it is not attacker-controlled in a normal
    // deployment — but it lands in an HTML attribute, and a stray `"`
    // in a hand-edited discovery XML would otherwise break out of it.
    let safe_name = crate::common::text::escape_markup(&file.name);
    let safe_editor_url = crate::common::text::escape_markup(&editor_url);
    let safe_token = crate::common::text::escape_markup(&token);

    // Per-response nonce for the one inline <script> (the auto-submit)
    // and the one inline <style>. A nonce rather than 'unsafe-inline'
    // so the page's CSP can stay closed: nothing but these two blocks
    // can execute or style here.
    //
    // `Uuid::new_v4` is 122 bits of randomness, regenerated per
    // response — well past what a nonce needs, and already a dependency
    // (no base64/rand plumbing for one string).
    let nonce = uuid::Uuid::new_v4().simple().to_string();

    // THE POINT OF THIS PAGE. The editor handshake is a cross-origin
    // form POST carrying a WOPI access token, and the origin it goes to
    // is operator config (`OXICLOUD_WOPI_DISCOVERY_URL`), unknowable
    // when the SPA is built. The SPA's own policy is a build-time
    // <meta>, so it could only permit that POST by allowing a whole
    // SCHEME (`form-action https:`) — which excludes a cleartext editor
    // entirely and, for everyone else, lets ANY https origin receive a
    // token-bearing form.
    //
    // Rendering the handshake here instead moves the decision to where
    // the value is known: this response names the ONE origin, computed
    // from the same discovery document the POST targets. Cleartext,
    // loopback, internal DNS and TLS hosts all work, and no policy
    // anywhere has to say `https:`.
    //
    // `frame-ancestors 'self'` + the explicit `X-Frame-Options:
    // SAMEORIGIN` below: the SPA frames this page. The global layer
    // defaults XFO to DENY and only skips responses that set their own
    // (see `middleware::security_headers`), so this header is what
    // makes the page embeddable at all.
    let editor_origin = origin_of(&editor_url);
    let csp = format!(
        "default-src 'none'; \
         script-src 'nonce-{nonce}'; \
         style-src 'nonce-{nonce}'; \
         form-action {editor_origin}; \
         frame-src {editor_origin}; \
         frame-ancestors 'self'; \
         base-uri 'none'"
    );

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>{safe_name} - OxiCloud Editor</title>
    <style nonce="{nonce}">
        body {{ margin: 0; overflow: hidden; }}
        iframe {{ width: 100%; height: 100vh; border: none; }}
    </style>
</head>
<body>
    <form id="wopi_form" action="{safe_editor_url}" method="post" target="wopi_frame">
        <input name="access_token" value="{safe_token}" type="hidden"/>
        <input name="access_token_ttl" value="{ttl}" type="hidden"/>
    </form>
    <iframe name="wopi_frame" allowfullscreen
        sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-top-navigation allow-popups-to-escape-sandbox">
    </iframe>
    <script nonce="{nonce}">document.getElementById('wopi_form').submit();</script>
</body>
</html>"#
    );

    let mut res = Html(html).into_response();
    let headers = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    headers.insert(
        axum::http::HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("SAMEORIGIN"),
    );
    // The page embeds a freshly-minted access token. Never cache it.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// `scheme://host[:port]` of `url`, or `'none'` when it will not parse.
///
/// `'none'` is the fail-CLOSED answer, and fail-closed is right here: a
/// host page that cannot name its editor origin must not post the token
/// anywhere. The editor then fails to load, visibly, instead of the
/// handshake going somewhere unverified.
fn origin_of(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => match (u.host_str(), u.port()) {
            // `Url::port()` is None when the port is the scheme default,
            // which is exactly what a CSP source should omit too.
            (Some(h), Some(p)) => format!("{}://{}:{}", u.scheme(), h, p),
            (Some(h), None) => format!("{}://{}", u.scheme(), h),
            (None, _) => "'none'".to_string(),
        },
        Err(_) => "'none'".to_string(),
    }
}

/// GET /wopi/supported-extensions — Returns extensions the editor supports.
///
/// Public endpoint (no auth) so the frontend can dynamically show/hide
/// the "Edit in Office" context menu option.
async fn get_supported_extensions(State(state): State<WopiState>) -> Response {
    match state.discovery_service.get_supported_extensions().await {
        Ok(exts) => axum::Json(exts).into_response(),
        Err(e) => {
            tracing::error!("Failed to get supported extensions: {}", e);
            axum::Json(Vec::<String>::new()).into_response()
        }
    }
}

/// Build all WOPI routes.
///
/// Returns a tuple: (wopi_protocol_router, wopi_api_router)
/// - wopi_protocol_router: mounted at `/wopi` (no auth middleware)
/// - wopi_api_router: mounted at `/api/wopi` (behind auth middleware)
pub fn wopi_routes(
    wopi_state: WopiState,
) -> (
    Router<Arc<crate::common::di::AppState>>,
    Router<Arc<crate::common::di::AppState>>,
) {
    let protocol_router = Router::new()
        // CheckFileInfo
        .route("/files/{file_id}", get(check_file_info))
        // Lock/Unlock/RefreshLock/GetLock
        .route("/files/{file_id}", post(file_operations))
        // GetFile
        .route("/files/{file_id}/contents", get(get_file))
        // PutFile
        .route("/files/{file_id}/contents", post(put_file))
        // Host page for new-tab editing
        .route("/edit/{file_id}", get(host_page))
        // Supported extensions (public, no auth)
        .route("/supported-extensions", get(get_supported_extensions))
        // Collector for any unknown `/wopi/*` path — keeps the
        // access-log target as `http::wopi` instead of letting
        // M365/Collabora probes leak into `http::web` via the
        // ServeDir fallback. Same rationale as the NC `/ocs/*`
        // catch-all in interfaces/nextcloud/routes.rs.
        .route("/{*rest}", any(wopi_not_found))
        .with_state(wopi_state.clone());

    let api_router = Router::new()
        .route("/editor-url", get(get_editor_url))
        .with_state(wopi_state);

    (protocol_router, api_router)
}

/// Catch-all 404 for unknown paths nested under `/wopi`. Exists
/// purely to anchor the access-log target to `http::wopi` instead
/// of letting the request fall through Axum's matcher to
/// ServeDir and being mis-attributed to `http::web`.
async fn wopi_not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap()
}

#[cfg(test)]
mod origin_of_tests {
    use super::origin_of;

    // `origin_of` is the single source for the editor origin the host
    // page's CSP names. Get it wrong and the failure is not loud — the
    // browser silently refuses the POST, and the editor shows an empty
    // frame. Hence a pin per input shape.
    //
    // Note the source: the chosen action's `urlsrc` from the fetched
    // discovery document, NOT `OXICLOUD_WOPI_DISCOVERY_URL`. Those two
    // can legitimately differ — the Hurl mock has discovery on
    // 127.0.0.1:9100 handing back a `urlsrc` on mock-editor.invalid —
    // and only `urlsrc` is where the POST actually goes.

    #[test]
    fn keeps_a_non_default_port() {
        // The dev/Docker shape: cleartext loopback on an explicit port,
        // which is precisely the case the old `form-action https:`
        // policy excluded.
        assert_eq!(
            origin_of("http://127.0.0.1:8099/hosting/wopi/word/edit?WOPISrc=x"),
            "http://127.0.0.1:8099"
        );
    }

    #[test]
    fn omits_a_default_port() {
        // A CSP source should not spell out :443.
        assert_eq!(
            origin_of("https://collabora.example.com/cool/word/edit?"),
            "https://collabora.example.com"
        );
        assert_eq!(
            origin_of("https://collabora.example.com:443/cool/word/edit?"),
            "https://collabora.example.com"
        );
    }

    #[test]
    fn drops_path_query_and_fragment() {
        // Only scheme://host[:port] is a valid CSP source expression; a
        // path would make the whole directive invalid, not just loose.
        assert_eq!(
            origin_of("https://host.test/a/b?x=1#f"),
            "https://host.test"
        );
    }

    #[test]
    fn an_unusable_url_fails_closed() {
        // `'none'` blocks the POST. The alternative — omitting the
        // directive, or emitting a truncated origin — would let the
        // token form go somewhere unverified.
        for bad in [
            "",
            "   ",
            "not-a-url",
            "/cool/word/edit",       // relative — no origin
            "mock-editor.invalid/x", // scheme-less
        ] {
            assert_eq!(origin_of(bad), "'none'", "{bad:?} must fail closed");
        }
    }

    #[test]
    fn the_hurl_mocks_urlsrc_resolves() {
        // The exact value `tests/common/wopi_mock_discovery.js` serves,
        // so the Hurl pin on the host page's CSP and this unit test
        // cannot disagree about what the header should say.
        assert_eq!(
            origin_of("http://mock-editor.invalid/edit?"),
            "http://mock-editor.invalid"
        );
    }
}

#[cfg(test)]
mod wopi_helper_tests {
    use super::*;

    #[test]
    fn test_file_extension() {
        assert_eq!(file_extension("MODELE TESTAMENTS.doc"), "doc");
        assert_eq!(file_extension("Report.FINAL.DOCX"), "docx");
        assert_eq!(file_extension("doc"), "");
        assert_eq!(file_extension("Makefile"), "");
        assert_eq!(file_extension(".gitignore"), "");
    }

    #[test]
    fn test_effective_action_and_can_write() {
        assert_eq!(effective_requested_action("edit", true), "edit");
        assert_eq!(effective_requested_action("edit", false), "view");
        assert_eq!(effective_requested_action("view", true), "view");
        assert_eq!(effective_requested_action("view", false), "view");

        // Regression pin: a fallback from "edit" to "view" (OnlyOffice `.doc`)
        // must never mint a write-capable token even when the caller has Update.
        assert!(!effective_can_write(true, "view"));
        assert!(effective_can_write(true, "edit"));
        // Regression pin: a fallback from "view" to "edit" (Collabora `.xlsx`)
        // must never elevate a Viewer or view-mode request into a write token,
        // and must stay "view" so host_page keeps UserCanWrite=false.
        assert!(!effective_can_write(false, "edit"));
        assert_eq!(effective_session_action("edit", false), "view");
        assert_eq!(effective_session_action("view", false), "view");
        assert_eq!(effective_session_action("edit", true), "edit");
    }

    #[test]
    fn test_wopi_utf7_roundtrip_and_decode() {
        assert_eq!(decode_wopi_utf7(".docx").unwrap(), ".docx");
        assert_eq!(
            decode_wopi_utf7("Mod+AOg-le test+AC0-1.docx").unwrap(),
            "Modèle test-1.docx"
        );
        assert_eq!(decode_wopi_utf7("A+-B.docx").unwrap(), "A+B.docx");
        // UTF-16 surrogate pair (emoji U+1F4C4)
        let original = "Modèle + test 📄.docx";
        let encoded = encode_wopi_utf7(original);
        assert_eq!(decode_wopi_utf7(&encoded).unwrap(), original);

        // Malformed UTF-7
        assert!(decode_wopi_utf7("bad+").is_err());
        assert!(decode_wopi_utf7("bad+A").is_err());
        assert!(decode_wopi_utf7("bad+!").is_err());
    }

    #[test]
    fn test_validate_and_resolve_targets() {
        assert_eq!(
            resolve_suggested_target("MODELE TESTAMENTS.doc", ".docx").unwrap(),
            "MODELE TESTAMENTS.docx"
        );
        assert_eq!(
            resolve_suggested_target("readme", ".docx").unwrap(),
            "readme.docx"
        );
        assert_eq!(
            resolve_suggested_target(".hidden", ".docx").unwrap(),
            ".hidden.docx"
        );
        assert_eq!(
            resolve_suggested_target("old.doc", "Custom Name.docx").unwrap(),
            "Custom Name.docx"
        );

        // Path traversal / invalid filenames must be rejected.
        for bad in [
            "",
            ".",
            "..",
            "../evil.docx",
            "sub/file.docx",
            "sub\\file.docx",
            "a\0b.docx",
        ] {
            assert!(validate_relative_filename(bad).is_err(), "{bad:?}");
            assert!(resolve_suggested_target("a.doc", bad).is_err(), "{bad:?}");
        }
        assert!(resolve_suggested_target("a.doc", ".").is_err());
        assert!(resolve_suggested_target("a.doc", "./x").is_err());
    }

    #[test]
    fn test_deduplicate_filename() {
        let mut existing = HashSet::new();
        assert_eq!(
            deduplicate_filename("legacy.docx", &existing),
            "legacy.docx"
        );

        existing.insert("legacy.docx".to_string());
        assert_eq!(
            deduplicate_filename("legacy.docx", &existing),
            "legacy (1).docx"
        );

        existing.insert("legacy (1).docx".to_string());
        assert_eq!(
            deduplicate_filename("Legacy.docx", &existing),
            "Legacy (2).docx"
        );

        existing.insert("readme".to_string());
        assert_eq!(deduplicate_filename("README", &existing), "README (1)");
    }
}
