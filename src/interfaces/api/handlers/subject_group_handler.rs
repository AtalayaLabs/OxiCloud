//! REST handler for ReBAC subject groups (`/api/groups/...`).
//!
//! All mutating endpoints require admin role (see
//! `crate::interfaces::middleware::admin::require_admin`). The read-only
//! `/api/groups/search` endpoint requires only authentication so the share
//! dialog can offer groups as recipients.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Json, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, patch, post},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::di::AppState;
use crate::domain::entities::subject_group::{GroupMember, SubjectGroup};
use crate::domain::entities::user::UserRole;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::admin::{require_admin, require_authenticated};

// ── DTOs ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateGroupRequest {
    /// RFC 5321 local-part shape (starts alnum, then alnum/dot/dash/underscore; 1–64 chars).
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateGroupRequest {
    pub name: Option<String>,
    /// Reserved for future use; v1 only persists the name in `rename`.
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ListGroupsQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
    /// Optional case-insensitive substring filter on group name.
    pub q: Option<String>,
}

fn default_limit() -> u32 {
    50
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SearchGroupsQuery {
    pub q: String,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
}

fn default_search_limit() -> u32 {
    20
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AddSubjectGroupMemberRequest {
    /// Set exactly one of `user_id` or `group_id` — the other field must be
    /// absent or null. Adding a `group_id` triggers a write-time cycle and
    /// depth check; max nesting depth is 8.
    pub user_id: Option<Uuid>,
    pub group_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GroupDto {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    /// True for system-managed groups (e.g. `Internal`). Membership and
    /// metadata on virtual groups are immutable.
    pub is_virtual: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// True when the caller may rename, delete, or change the membership
    /// of this group. v1 computes this as `caller.role == "admin"`. v2
    /// will compute it from per-group `Manage` grants on
    /// `Resource::SubjectGroup(id)` once that resource type lands in
    /// `access_grants`. Frontend reads this unconditionally so the v2
    /// migration is backend-only.
    pub can_manage: bool,
    /// Direct-member count (users + nested groups, one level only). The
    /// management UI shows this as a chip on each list row; for transitive
    /// expansion size, see the `/effective-members` endpoint.
    pub member_count: i64,
}

impl GroupDto {
    /// Build the DTO for a given caller. `can_manage` is decided per call:
    /// v1 just delegates to the admin flag; v2 will consult the
    /// authorization engine here. `member_count` comes from the same query
    /// that fetched the group (list/search) or a dedicated `COUNT(*)`
    /// helper (create/get/update).
    pub fn from_group(g: SubjectGroup, can_manage: bool, member_count: i64) -> Self {
        Self {
            id: g.id,
            name: g.name,
            description: g.description,
            is_virtual: g.is_virtual,
            created_at: g.created_at,
            updated_at: g.updated_at,
            can_manage,
            member_count,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GroupListDto {
    pub items: Vec<GroupDto>,
    pub total: u64,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum GroupMemberDto {
    User { id: Uuid },
    Group { id: Uuid },
}

impl From<GroupMember> for GroupMemberDto {
    fn from(m: GroupMember) -> Self {
        match m {
            GroupMember::User(id) => GroupMemberDto::User { id },
            GroupMember::Group(id) => GroupMemberDto::Group { id },
        }
    }
}

// ── Routes ───────────────────────────────────────────────────────────────────

/// Routes mounted under `/api/groups`.
pub fn subject_group_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", post(create_group))
        .route("/", get(list_groups))
        .route("/search", get(search_groups))
        .route("/{id}", get(get_group))
        .route("/{id}", patch(update_group))
        .route("/{id}", delete(delete_group))
        .route("/{id}/members", get(list_members))
        .route("/{id}/members", post(add_member))
        .route("/{id}/members/user/{uid}", delete(remove_user_member))
        .route("/{id}/members/group/{gid}", delete(remove_group_member))
        .route("/{id}/effective-members", get(list_effective_members))
}

fn service(
    state: &AppState,
) -> Result<&Arc<crate::application::services::subject_group_service::SubjectGroupService>, AppError>
{
    state
        .subject_group_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Subject-group service not configured"))
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// Create a new ReBAC subject group. Admin-only. The name must match the
/// RFC 5321 local-part shape and be globally unique (case-insensitive).
#[utoipa::path(
    post,
    path = "/api/groups",
    request_body = CreateGroupRequest,
    responses(
        (status = 201, description = "Group created", body = GroupDto),
        (status = 400, description = "Invalid name (RFC 5321 violation)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 409, description = "Group with this name already exists"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_create"
)]
pub async fn create_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateGroupRequest>,
) -> Result<impl IntoResponse, AppError> {
    let (caller_id, _) = require_admin(&state, &headers).await?;
    let svc = service(&state)?;
    let group = svc
        .create(&req.name, req.description, caller_id)
        .await
        .map_err(AppError::from)?;
    // Reached require_admin → caller is admin → can_manage is always true here.
    // New groups start with zero direct members.
    Ok((
        StatusCode::CREATED,
        Json(GroupDto::from_group(group, true, 0)),
    ))
}

/// List subject groups (paginated). Admin-only.
#[utoipa::path(
    get,
    path = "/api/groups",
    params(ListGroupsQuery),
    responses(
        (status = 200, description = "Paginated list of groups", body = GroupListDto),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_list"
)]
pub async fn list_groups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ListGroupsQuery>,
) -> Result<impl IntoResponse, AppError> {
    require_admin(&state, &headers).await?;
    let svc = service(&state)?;
    let (items, total) = svc
        .list_with_counts(q.limit, q.offset, q.q.as_deref())
        .await
        .map_err(AppError::from)?;
    // Admin-gated handler → every row is manageable by the caller.
    Ok(Json(GroupListDto {
        items: items
            .into_iter()
            .map(|(g, member_count)| GroupDto::from_group(g, true, member_count))
            .collect(),
        total,
    }))
}

/// Search non-virtual groups by name substring. Authenticated only (no
/// admin role required) — backs the share-dialog recipient autocomplete.
#[utoipa::path(
    get,
    path = "/api/groups/search",
    params(SearchGroupsQuery),
    responses(
        (status = 200, description = "Matching non-virtual groups", body = [GroupDto]),
        (status = 401, description = "Unauthorized"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_search"
)]
pub async fn search_groups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<SearchGroupsQuery>,
) -> Result<impl IntoResponse, AppError> {
    // Any authenticated INTERNAL user can discover groups for the
    // share dialog — membership lists remain admin-only via
    // list_members. External users have no business enumerating
    // groups; defence-in-depth on top of the ReBAC layer (which
    // already prevents them from being added to any group anyway).
    let (caller_id, role) = require_authenticated(&state, &headers).await?;
    if let Some(auth_svc) = state.auth_service.as_ref()
        && let Err(err) = crate::interfaces::middleware::user::require_internal_user(
            &auth_svc.auth_application_service,
            caller_id,
        )
        .await
    {
        tracing::info!(
            target: "audit",
            event = "groups.search_rejected",
            reason = "external_user",
            caller_id = %caller_id,
            "👮🏻‍♂️ External user blocked from /api/groups/search"
        );
        return Err(err);
    }
    let can_manage = UserRole::str_at_least(&role, UserRole::Admin);
    let svc = service(&state)?;
    // The share-dialog autocomplete doesn't render a member-count chip, so
    // emit 0 rather than spending a `COUNT(*)` per row. Frontend consumers
    // that need the real count fetch it via `/api/groups/{id}` instead.
    let items = svc
        .search_for_share(&q.q, q.limit)
        .await
        .map_err(AppError::from)?;
    Ok(Json(
        items
            .into_iter()
            .map(|g| GroupDto::from_group(g, can_manage, 0))
            .collect::<Vec<_>>(),
    ))
}

/// Fetch a single group's details. Admin-only.
#[utoipa::path(
    get,
    path = "/api/groups/{id}",
    params(("id" = Uuid, Path, description = "Group ID")),
    responses(
        (status = 200, description = "Group details", body = GroupDto),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Group not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_get"
)]
pub async fn get_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    require_admin(&state, &headers).await?;
    let svc = service(&state)?;
    let group = svc.get_by_id(id).await.map_err(AppError::from)?;
    let member_count = svc.count_members(id).await.map_err(AppError::from)?;
    Ok(Json(GroupDto::from_group(group, true, member_count)))
}

/// Update a group's metadata. Admin-only. v1 only persists name renames.
#[utoipa::path(
    patch,
    path = "/api/groups/{id}",
    params(("id" = Uuid, Path, description = "Group ID")),
    request_body = UpdateGroupRequest,
    responses(
        (status = 200, description = "Updated group", body = GroupDto),
        (status = 400, description = "Invalid name"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required, or group is virtual"),
        (status = 404, description = "Group not found"),
        (status = 409, description = "Name already taken"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_update"
)]
pub async fn update_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateGroupRequest>,
) -> Result<impl IntoResponse, AppError> {
    let (caller_id, _) = require_admin(&state, &headers).await?;
    let svc = service(&state)?;

    // v1 only supports renaming; description-only updates are silently
    // accepted as a no-op so the API surface is forward-compatible.
    let group = match req.name {
        Some(new_name) => svc
            .rename(id, &new_name, caller_id)
            .await
            .map_err(AppError::from)?,
        None => svc.get_by_id(id).await.map_err(AppError::from)?,
    };
    let member_count = svc.count_members(id).await.map_err(AppError::from)?;
    Ok(Json(GroupDto::from_group(group, true, member_count)))
}

/// Delete a group. Cascades to `subject_group_members` (FK) and to
/// `access_grants` rows referencing this group as a subject. Admin-only.
#[utoipa::path(
    delete,
    path = "/api/groups/{id}",
    params(("id" = Uuid, Path, description = "Group ID")),
    responses(
        (status = 204, description = "Group deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required, or group is virtual"),
        (status = 404, description = "Group not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_delete"
)]
pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let (caller_id, _) = require_admin(&state, &headers).await?;
    let svc = service(&state)?;
    svc.delete(id, caller_id).await.map_err(AppError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// List the *direct* members of a group (one level only). Admin-only.
#[utoipa::path(
    get,
    path = "/api/groups/{id}/members",
    params(("id" = Uuid, Path, description = "Group ID")),
    responses(
        (status = 200, description = "Direct members", body = [GroupMemberDto]),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Group not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_list_members"
)]
pub async fn list_members(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    require_admin(&state, &headers).await?;
    let svc = service(&state)?;
    let members = svc.list_direct_members(id).await.map_err(AppError::from)?;
    Ok(Json(
        members
            .into_iter()
            .map(GroupMemberDto::from)
            .collect::<Vec<_>>(),
    ))
}

/// Add a member to a group. Exactly one of `user_id` / `group_id` must be
/// provided. Adding a group-member runs a write-time cycle check and a
/// nesting-depth check (max 8). Admin-only.
#[utoipa::path(
    post,
    path = "/api/groups/{id}/members",
    params(("id" = Uuid, Path, description = "Group ID")),
    request_body = AddSubjectGroupMemberRequest,
    responses(
        (status = 201, description = "Member added"),
        (status = 400, description = "Invalid request, cycle would be created, or depth limit exceeded"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required, or virtual group"),
        (status = 404, description = "Group not found"),
        (status = 409, description = "Member already in group"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_add_member"
)]
pub async fn add_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(group_id): Path<Uuid>,
    Json(req): Json<AddSubjectGroupMemberRequest>,
) -> Result<impl IntoResponse, AppError> {
    let (caller_id, _) = require_admin(&state, &headers).await?;
    let svc = service(&state)?;

    let member = match (req.user_id, req.group_id) {
        (Some(uid), None) => GroupMember::User(uid),
        (None, Some(gid)) => GroupMember::Group(gid),
        (Some(_), Some(_)) => {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "Provide exactly one of user_id or group_id, not both",
                "InvalidInput",
            ));
        }
        (None, None) => {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "Provide user_id or group_id",
                "InvalidInput",
            ));
        }
    };

    svc.add_member(group_id, member, caller_id)
        .await
        .map_err(AppError::from)?;
    Ok(StatusCode::CREATED)
}

/// Remove a user-member from a group. Admin-only.
#[utoipa::path(
    delete,
    path = "/api/groups/{id}/members/user/{uid}",
    params(
        ("id" = Uuid, Path, description = "Group ID"),
        ("uid" = Uuid, Path, description = "User ID to remove"),
    ),
    responses(
        (status = 204, description = "Member removed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required, or virtual group"),
        (status = 404, description = "Group or member not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_remove_user_member"
)]
pub async fn remove_user_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((group_id, uid)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, AppError> {
    let (caller_id, _) = require_admin(&state, &headers).await?;
    let svc = service(&state)?;
    svc.remove_member(group_id, GroupMember::User(uid), caller_id)
        .await
        .map_err(AppError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a nested group-member from a group. Admin-only.
#[utoipa::path(
    delete,
    path = "/api/groups/{id}/members/group/{gid}",
    params(
        ("id" = Uuid, Path, description = "Parent group ID"),
        ("gid" = Uuid, Path, description = "Child group ID to remove"),
    ),
    responses(
        (status = 204, description = "Member removed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required, or virtual group"),
        (status = 404, description = "Group or member not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_remove_group_member"
)]
pub async fn remove_group_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((group_id, gid)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, AppError> {
    let (caller_id, _) = require_admin(&state, &headers).await?;
    let svc = service(&state)?;
    svc.remove_member(group_id, GroupMember::Group(gid), caller_id)
        .await
        .map_err(AppError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// List every user transitively reached through this group (members of
/// members of members, etc.). Used by admin / audit tooling. Admin-only.
#[utoipa::path(
    get,
    path = "/api/groups/{id}/effective-members",
    params(("id" = Uuid, Path, description = "Group ID")),
    responses(
        (status = 200, description = "Flat list of transitively-reached user IDs", body = [Uuid]),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Group not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "groups",
    operation_id = "subject_group_effective_members"
)]
pub async fn list_effective_members(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    require_admin(&state, &headers).await?;
    let svc = service(&state)?;
    let users = svc
        .list_transitive_users(id)
        .await
        .map_err(AppError::from)?;
    Ok(Json(users))
}
