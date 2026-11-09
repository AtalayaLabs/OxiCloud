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
use std::sync::Arc;

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::file_ports::{FileRetrievalUseCase, FileUploadUseCase};
use crate::application::services::wopi_lock_service::WopiLockService;
use crate::application::services::wopi_token_service::WopiTokenService;
use crate::domain::repositories::drive_repository::DriveRepository;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use crate::infrastructure::services::wopi_discovery_service::WopiDiscoveryService;

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
    pub user_friendly_name: String,
    pub post_message_origin: String,
    pub last_modified_time: String,
    pub close_url: String,
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
        // can_write flag. If the caller's Update was revoked since the
        // token was minted (e.g. their grant was downgraded from Editor
        // to Viewer), the editor sees the file as read-only and won't
        // even attempt PutFile. The stricter `require_wopi_perm(Update)`
        // in put_file is the actual gate; this field is a UI hint.
        async {
            if claims.can_write {
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
        supports_update: can_write_now,
        supports_rename: false,
        user_can_write: can_write_now,
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

/// POST /wopi/files/{file_id} — Dispatches lock operations based on X-WOPI-Override header
async fn file_operations(
    Path(file_id): Path<String>,
    Query(token_query): Query<WopiTokenQuery>,
    headers: HeaderMap,
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

    let override_header = headers
        .get("X-WOPI-Override")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

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

/// GET /api/wopi/editor-url — Returns the editor iframe URL + WOPI token.
///
/// This endpoint is behind normal auth middleware. The authenticated user
/// requests a WOPI session for a specific file.
pub async fn get_editor_url(
    auth_user: AuthUser,
    Query(params): Query<EditorUrlParams>,
    State(state): State<WopiState>,
) -> Response {
    let user_id = auth_user.id;
    let username = &auth_user.username;
    // Verify the caller owns the file (SQL-level check, no existence leak).
    let (file, can_write) = match authorize_wopi_access(
        state.app_state.authorization.as_ref(),
        state.app_state.applications.file_retrieval_service.as_ref(),
        &params.file_id,
        user_id,
        &params.action,
    )
    .await
    {
        Ok(result) => result,
        Err(status) => return status.into_response(),
    };

    // Extract extension from filename
    let extension = file.name.rsplit('.').next().unwrap_or("").to_lowercase();

    // Build WOPISrc
    let wopi_src = format!("{}/wopi/files/{}", state.wopi_base_url, params.file_id);

    // Get editor action URL from discovery
    let editor_url = match state
        .discovery_service
        .get_action_url(&extension, &params.action, &wopi_src)
        .await
    {
        Ok(Some(url)) => url,
        Ok(None) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("No editor available for .{} files", extension),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("WOPI discovery error: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Generate WOPI access token
    let (access_token, access_token_ttl) = match state.token_service.generate_token(
        &params.file_id,
        &user_id.to_string(),
        username,
        can_write,
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to generate WOPI token: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    axum::Json(EditorUrlResponse {
        editor_url,
        access_token,
        access_token_ttl,
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

    // Re-verify ownership even though the token was valid — defence in depth.
    let requested_action = if claims.can_write { "edit" } else { "view" };
    let caller_uuid = match uuid::Uuid::parse_str(&claims.sub) {
        Ok(u) => u,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let (file, can_write_now) = match authorize_wopi_access(
        state.app_state.authorization.as_ref(),
        state.app_state.applications.file_retrieval_service.as_ref(),
        &file_id,
        caller_uuid,
        requested_action,
    )
    .await
    {
        Ok((f, cw)) => (f, cw),
        Err(status) => return status.into_response(),
    };

    let extension = file.name.rsplit('.').next().unwrap_or("").to_lowercase();
    let action = requested_action;
    let wopi_src = format!("{}/wopi/files/{}", state.wopi_base_url, file_id);

    let editor_url = match state
        .discovery_service
        .get_action_url(&extension, action, &wopi_src)
        .await
    {
        Ok(Some(url)) => url,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // Use the freshly-computed `can_write_now` (real Update permission
    // ∧ requested_action) rather than the incoming token's `can_write`
    // flag. Otherwise a Viewer who somehow reached this host page with
    // a stale edit-capable token would get another one re-minted.
    let (token, ttl) = match state.token_service.generate_token(
        &file_id,
        &claims.sub,
        &claims.username,
        can_write_now,
    ) {
        Ok(t) => t,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

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
