//! Drive endpoints.
//!
//! - `GET    /api/drives`                              — list every drive the caller can read (D0)
//! - `GET    /api/drives/{id}/members`                 — list role grants on a drive (D2)
//! - `POST   /api/drives/{id}/members`                 — add a member (D2)
//! - `PATCH  /api/drives/{id}/members/{kind}/{sid}`    — change a member's role / expiry (D2)
//! - `DELETE /api/drives/{id}/members/{kind}/{sid}`    — remove a member (D2)
//!
//! D3 adds the create-shared-drive flow under `POST /api/drives`. The
//! membership endpoints are thin wrappers around `DriveManagementService`,
//! which layers the personal-drive guard and shared-drive last-owner
//! protection on top of the generic `role_grants` write path.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use tracing::error;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::dtos::drive_dto::DriveDto;
use crate::application::dtos::grant_dto::{GrantDto, RoleDto, SubjectDto, SubjectTypeDto};
use crate::common::di::AppState;
use crate::domain::entities::user::UserRole;
use crate::domain::repositories::drive_repository::DriveRepository;
use crate::domain::services::authorization::Subject;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

#[utoipa::path(
    get,
    path = "/api/drives",
    responses(
        (status = 200, description = "Drives the caller can read", body = Vec<DriveDto>),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearerAuth" = [])),
    tag = "drives"
)]
pub async fn list_drives(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
) -> impl IntoResponse {
    let caller_id = auth_user.id;

    match state.drive_repo.list_readable_by(caller_id).await {
        Ok(drives) => {
            let dtos: Vec<DriveDto> = drives.iter().cloned().map(DriveDto::from).collect();
            (StatusCode::OK, Json(dtos)).into_response()
        }
        Err(e) => {
            error!("list_drives: repo lookup failed: {e}");
            AppError::internal_error(format!("Failed to list drives: {e}")).into_response()
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Membership API (D2)
// ════════════════════════════════════════════════════════════════════════════

/// Body for `POST /api/drives/{id}/members`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AddDriveMemberDto {
    pub subject: SubjectDto,
    pub role: RoleDto,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Body for `PATCH /api/drives/{id}/members/{kind}/{sid}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateDriveMemberDto {
    pub role: RoleDto,
    /// Optional. Pass `null` (or omit) to clear an existing expiry.
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Body for `POST /api/drives` (D3a — create drive).
///
/// `kind` discriminates the drive flavour. D3a wires the `shared` branch
/// end-to-end; the `personal` branch (secondary personal drives, distinct
/// from the lifecycle-created default) is a recognised wire shape but
/// returns 501 today — its authz model (self-service vs admin-only) and
/// quota source (borrowed from per-user pool? separate cap?) are still
/// open product questions. The body shape stays stable so future PRs only
/// need to flip the service's `kind=personal` arm from rejecting to
/// dispatching `create_personal_drive_atomic` with `default_for_user=NULL`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateDriveDto {
    /// Drive flavour. `"shared"` is implemented; `"personal"` is reserved.
    pub kind: DriveKindDto,
    /// Drive name (becomes the root folder's name). Trimmed; must be
    /// non-empty after trim.
    pub name: String,
    /// Initial Owner subject. For `kind="shared"`: either a `user` (sole
    /// drive Owner) or a `group` (transitive user members all gain Owner
    /// via subject expansion). `token` is refused at the service edge.
    /// For `kind="personal"` (when implemented): MUST be a `user`.
    pub owner: SubjectDto,
    /// Optional storage cap in bytes. `None` / omitted → no quota.
    /// Quota mutation post-creation is OxiCloud-admin-only (D4).
    #[serde(default)]
    pub quota_bytes: Option<i64>,
}

/// Wire-shape enum for the drive flavour. Mirrors backend `DriveKind`.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DriveKindDto {
    Personal,
    Shared,
}

fn parse_subject(kind: SubjectTypeDto, id: Uuid) -> Subject {
    match kind {
        SubjectTypeDto::User => Subject::User(id),
        SubjectTypeDto::Group => Subject::Group(id),
        SubjectTypeDto::Token => Subject::Token(id),
    }
}

/// Create a drive (D3a — shared today; personal kind reserved).
///
/// **AuthZ**: OxiCloud-`admin` role only. The plan (`drive.md §6`) reads
/// "admin OR group owner triggers" — D3a starts with admin-only and later
/// iterations can broaden the gate without changing the wire shape.
///
/// Body:
/// ```json
/// {
///   "kind": "shared",
///   "name": "Engineering",
///   "owner": { "type": "group", "id": "<group-uuid>" },
///   "quota_bytes": 53687091200
/// }
/// ```
///
/// Returns the new `DriveDto`. If `owner.type == "group"`, the group must
/// have ≥1 direct member or the request is refused with 400 — otherwise
/// the drive would be created with no effective Owner-user.
///
/// `kind: "personal"` is recognised on the wire but returns 501 — the
/// authz model (self-service vs admin-only) and quota source for
/// secondary personal drives are still open product questions.
#[utoipa::path(
    post,
    path = "/api/drives",
    request_body = CreateDriveDto,
    responses(
        (status = 201, description = "Drive created", body = DriveDto),
        (status = 400, description = "Empty name, empty owner group, or invalid input"),
        (status = 403, description = "Caller is not an OxiCloud admin"),
        (status = 501, description = "kind=personal not yet implemented"),
    ),
    security(("bearerAuth" = [])),
    tag = "drives"
)]
pub async fn create_drive(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(dto): Json<CreateDriveDto>,
) -> impl IntoResponse {
    let caller_is_admin = UserRole::str_at_least(&auth_user.role, UserRole::Admin);

    // Personal kind is a wire-shape placeholder — see DTO doc.
    if dto.kind == DriveKindDto::Personal {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "error": "Creating secondary personal drives is not yet implemented. \
                          The authz model and quota source are still open product \
                          questions — this body shape is reserved for the future PR."
            })),
        )
            .into_response();
    }

    let owner = parse_subject(dto.owner.kind, dto.owner.id);
    match state
        .drive_management_service
        .create_shared_drive(
            auth_user.id,
            caller_is_admin,
            &dto.name,
            owner,
            dto.quota_bytes,
        )
        .await
    {
        Ok(drive) => (StatusCode::CREATED, Json(DriveDto::from(drive))).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/api/drives/{id}/members",
    params(("id" = Uuid, Path, description = "Drive UUID")),
    responses(
        (status = 200, description = "Role grants on this drive", body = Vec<GrantDto>),
        (status = 404, description = "Drive not found or caller lacks Read"),
    ),
    security(("bearerAuth" = [])),
    tag = "drives"
)]
pub async fn list_drive_members(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(drive_id): Path<Uuid>,
) -> impl IntoResponse {
    match state
        .drive_management_service
        .list_members(auth_user.id, drive_id)
        .await
    {
        Ok(grants) => {
            let dtos: Vec<GrantDto> = grants.into_iter().map(GrantDto::from).collect();
            (StatusCode::OK, Json(dtos)).into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/api/drives/{id}/members",
    params(("id" = Uuid, Path, description = "Drive UUID")),
    request_body = AddDriveMemberDto,
    responses(
        (status = 201, description = "Member added", body = GrantDto),
        (status = 400, description = "Validation error (e.g. last-owner constraint)"),
        (status = 404, description = "Drive not found or caller lacks Manage"),
        (status = 405, description = "Personal drive — membership is immutable"),
    ),
    security(("bearerAuth" = [])),
    tag = "drives"
)]
pub async fn add_drive_member(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(drive_id): Path<Uuid>,
    Json(dto): Json<AddDriveMemberDto>,
) -> impl IntoResponse {
    let subject = parse_subject(dto.subject.kind, dto.subject.id);
    match state
        .drive_management_service
        .set_member_role(
            auth_user.id,
            false, // caller_is_admin — user-facing route, always require Manage
            drive_id,
            subject,
            dto.role.into(),
            dto.expires_at,
        )
        .await
    {
        Ok(grant) => (StatusCode::CREATED, Json(GrantDto::from(grant))).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[utoipa::path(
    patch,
    path = "/api/drives/{id}/members/{kind}/{sid}",
    params(
        ("id" = Uuid, Path, description = "Drive UUID"),
        ("kind" = String, Path, description = "Subject kind: user|group|token"),
        ("sid" = Uuid, Path, description = "Subject UUID"),
    ),
    request_body = UpdateDriveMemberDto,
    responses(
        (status = 200, description = "Member role updated", body = GrantDto),
        (status = 400, description = "Validation error (e.g. last-owner demotion)"),
        (status = 404, description = "Drive not found or caller lacks Manage"),
        (status = 405, description = "Personal drive — membership is immutable"),
    ),
    security(("bearerAuth" = [])),
    tag = "drives"
)]
pub async fn update_drive_member(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path((drive_id, kind, subject_id)): Path<(Uuid, SubjectTypeDto, Uuid)>,
    Json(dto): Json<UpdateDriveMemberDto>,
) -> impl IntoResponse {
    let subject = parse_subject(kind, subject_id);
    match state
        .drive_management_service
        .set_member_role(
            auth_user.id,
            false, // caller_is_admin — user-facing route, always require Manage
            drive_id,
            subject,
            dto.role.into(),
            dto.expires_at,
        )
        .await
    {
        Ok(grant) => (StatusCode::OK, Json(GrantDto::from(grant))).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/api/drives/{id}/members/{kind}/{sid}",
    params(
        ("id" = Uuid, Path, description = "Drive UUID"),
        ("kind" = String, Path, description = "Subject kind: user|group|token"),
        ("sid" = Uuid, Path, description = "Subject UUID"),
    ),
    responses(
        (status = 204, description = "Member removed (or was never a member — idempotent)"),
        (status = 400, description = "Last-owner protection — promote another member first"),
        (status = 404, description = "Drive not found or caller lacks Manage"),
        (status = 405, description = "Personal drive — membership is immutable"),
    ),
    security(("bearerAuth" = [])),
    tag = "drives"
)]
pub async fn remove_drive_member(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path((drive_id, kind, subject_id)): Path<(Uuid, SubjectTypeDto, Uuid)>,
) -> impl IntoResponse {
    let subject = parse_subject(kind, subject_id);
    match state
        .drive_management_service
        .remove_member(auth_user.id, false, drive_id, subject)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// `DELETE /api/drives/{id}` — Owner-only deletion (D3b).
///
/// Refuses (per `DriveManagementService::delete_drive`):
/// - `404` when the caller lacks Manage on the drive (anti-enum).
/// - `405` when the drive is the user's default Personal drive.
/// - `409` when the drive still holds live folders/files; the caller
///   must trash or move them first.
///
/// On success the drive row, its root folder, and every role grant
/// scoped to the drive are removed in one transaction; cached drive
/// roles are invalidated.
#[utoipa::path(
    delete,
    path = "/api/drives/{id}",
    params(("id" = Uuid, Path, description = "Drive UUID")),
    responses(
        (status = 204, description = "Drive deleted"),
        (status = 404, description = "Drive not found or caller lacks Manage"),
        (status = 405, description = "Default Personal drive — undeletable"),
        (status = 409, description = "Drive is not empty — move/trash contents first"),
    ),
    security(("bearerAuth" = [])),
    tag = "drives"
)]
pub async fn delete_drive(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(drive_id): Path<Uuid>,
) -> impl IntoResponse {
    match state
        .drive_management_service
        .delete_drive(auth_user.id, false, drive_id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// Body for `PATCH /api/drives/{id}/policies` (D5).
///
/// Partial merge: any field left out of the JSON keeps its current
/// JSONB value (the repo uses `policies || $partial`). Each field
/// defaults to `false` in `DrivePolicies`, but the merge is keyed on
/// presence — so omitting a field means "leave it alone", not "set
/// it to false". Clients flip a single key at a time without
/// round-tripping the whole bag.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct UpdateDrivePoliciesDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbid_sharing: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbid_external_sharing: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbid_public_links: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbid_cross_drive_move: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbid_owner_role_change: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_in_photo_index: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_in_music_index: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

/// `PATCH /api/drives/{id}/policies` — **OxiCloud-admin only** policy
/// update (D5).
///
/// Policies were originally owner-mutable, but that made them
/// self-policing soft caps — an owner could disable
/// `forbid_external_sharing`, create the grant, and re-enable. For
/// compliance-grade enforcement, mutation is restricted to the
/// tenant operator (admin role), mirroring the same carve-out that
/// guards `drives.quota_bytes` and `users.storage_quota_bytes` (§7).
///
/// Non-admin callers receive `404` (anti-enumeration — same response
/// as "drive does not exist", so a probe can't tell apart "no such
/// drive" from "policies are admin-managed").
///
/// Partial merge into the JSONB `policies` column; the post-merge
/// typed view is returned.
///
/// Audit: emits `drive.policy_changed` with `by = <admin_user_id>`
/// and every key's post-merge value (steady-state observability).
#[utoipa::path(
    patch,
    path = "/api/drives/{id}/policies",
    params(("id" = Uuid, Path, description = "Drive UUID")),
    request_body = UpdateDrivePoliciesDto,
    responses(
        (status = 200, description = "Policies merged"),
        (status = 404, description = "Drive not found OR caller is not OxiCloud admin"),
    ),
    security(("bearerAuth" = [])),
    tag = "drives"
)]
pub async fn update_drive_policies(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(drive_id): Path<Uuid>,
    axum::Json(dto): axum::Json<UpdateDrivePoliciesDto>,
) -> impl IntoResponse {
    // OxiCloud-admin only. Anti-enumeration: return the same 404 a
    // non-existent drive would carry, never 403, so the policy
    // existence isn't probable by error shape.
    if !UserRole::str_at_least(&auth_user.role, UserRole::Admin) {
        tracing::info!(
            target: "audit",
            event = "drive.policy_change_rejected",
            reason = "not_admin",
            caller_id = %auth_user.id,
            drive_id = %drive_id,
            "👮🏻‍♂️ policy mutation refused: caller is not OxiCloud admin",
        );
        return AppError::not_found(format!("Drive {drive_id} not found")).into_response();
    }
    // Translate the Option-per-field DTO into a serde_json partial that
    // only carries the supplied keys, so the JSONB merge in
    // `update_policies` skips fields the caller didn't touch. Building a
    // `DrivePolicies` and serialising would lose the partial-update
    // semantics (every field defaults to false → omitted vs. "set to
    // false" become indistinguishable on the wire).
    let mut partial_obj = serde_json::Map::new();
    if let Some(v) = dto.forbid_sharing {
        partial_obj.insert("forbid_sharing".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = dto.forbid_external_sharing {
        partial_obj.insert("forbid_external_sharing".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = dto.forbid_public_links {
        partial_obj.insert("forbid_public_links".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = dto.forbid_cross_drive_move {
        partial_obj.insert("forbid_cross_drive_move".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = dto.forbid_owner_role_change {
        partial_obj.insert(
            "forbid_owner_role_change".into(),
            serde_json::Value::Bool(v),
        );
    }
    if let Some(v) = dto.include_in_photo_index {
        partial_obj.insert("include_in_photo_index".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = dto.include_in_music_index {
        partial_obj.insert("include_in_music_index".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = dto.read_only {
        partial_obj.insert("read_only".into(), serde_json::Value::Bool(v));
    }
    // Pass the raw JSON straight through so the JSONB `||` merge in
    // the repo only touches keys the caller supplied. Round-tripping
    // via `DrivePolicies` (which has `#[serde(default)]`) would
    // silently fill every omitted field with `false` — the merge
    // would then clobber every unmentioned policy on the row.
    let partial_value = serde_json::Value::Object(partial_obj);

    match state
        .drive_management_service
        .update_policies(auth_user.id, drive_id, partial_value)
        .await
    {
        Ok(merged) => (StatusCode::OK, axum::Json(merged)).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// Body for `PATCH /api/drives/{id}/quota` (D4).
///
/// `quota_bytes = null` (or ≤ 0) means unlimited — matches the DB
/// convention where NULL on the row is treated as "no cap" by
/// `storage_usage_service::check_drive_quota`. The service
/// normalises 0/negative to None before writing.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct UpdateDriveQuotaDto {
    /// New quota in bytes. `null` (or omitted) or ≤ 0 → unlimited.
    /// A value below the drive's current `used_bytes` is accepted
    /// intentionally (soft-quota semantic — new writes gated,
    /// existing content untouched; owners recover by deleting
    /// until the drive comes back under the cap).
    #[serde(default)]
    pub quota_bytes: Option<i64>,
}

/// `PATCH /api/drives/{id}/quota` — **OxiCloud-admin only** storage-cap
/// mutation for **shared** drives (D4).
///
/// Personal drives are refused with `400 InvalidInput` — their
/// effective cap comes from the owner user's
/// `users.storage_quota_bytes` envelope (memory
/// `project_user_envelope_quota_model`); use
/// `PUT /api/admin/users/{id}/quota` instead. Allowing a per-personal-
/// drive quota here would fork the model into two competing paths.
///
/// Non-admin callers receive `404` (anti-enumeration — same shape as
/// "no such drive", so a probe can't distinguish "drive doesn't
/// exist" from "quota edit is admin-only"). Matches the pattern
/// established by `update_drive_policies` above.
///
/// **Soft-quota semantic on reduction.** A newly-lowered quota may
/// land BELOW the drive's current `used_bytes`. The write succeeds;
/// `storage_usage_service` then blocks new writes on
/// `used + delta > quota`, so owners of a shared drive that's now
/// over its freshly-reduced cap can only shrink (delete) until they
/// come back under. Existing content is never retroactively touched
/// — matches xfs `xfs_quota` / ext4 `edquota` behaviour on quota
/// shrink.
///
/// Cache invalidation: the repo drops `readable_cache` +
/// `default_drive_cache` (both embed the whole drive row incl.
/// `quota_bytes`), matching the `update_policies` pattern.
///
/// Audit: emits `drive.quota_changed` with `new_quota_bytes`,
/// `used_bytes`, and `over_quota` — so an operator grepping
/// `audit drive.quota_changed` can spot a shrink that landed the
/// drive in the over-quota delete-only state.
#[utoipa::path(
    patch,
    path = "/api/drives/{id}/quota",
    params(("id" = Uuid, Path, description = "Drive UUID")),
    request_body = UpdateDriveQuotaDto,
    responses(
        (status = 200, description = "Quota updated"),
        (status = 400, description = "Personal drive — quota is envelope-managed via the owner user"),
        (status = 404, description = "Drive not found OR caller is not OxiCloud admin"),
    ),
    security(("bearerAuth" = [])),
    tag = "drives"
)]
pub async fn update_drive_quota(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(drive_id): Path<Uuid>,
    axum::Json(dto): axum::Json<UpdateDriveQuotaDto>,
) -> impl IntoResponse {
    // Same admin gate + anti-enum shape as `update_drive_policies`.
    // Refusing with 404 (rather than 403) means an unauthorised
    // caller can't distinguish "no such drive" from "you're not
    // admin" — the endpoint's existence isn't probable by error
    // shape.
    if !UserRole::str_at_least(&auth_user.role, UserRole::Admin) {
        tracing::info!(
            target: "audit",
            event = "drive.quota_change_rejected",
            reason = "not_admin",
            caller_id = %auth_user.id,
            drive_id = %drive_id,
            "👮🏻‍♂️ quota mutation refused: caller is not OxiCloud admin",
        );
        return AppError::not_found(format!("Drive {drive_id} not found")).into_response();
    }

    match state
        .drive_management_service
        .update_quota(auth_user.id, drive_id, dto.quota_bytes)
        .await
    {
        Ok(persisted) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "quota_bytes": persisted })),
        )
            .into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}
