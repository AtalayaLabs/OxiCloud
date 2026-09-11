//! `/api/notifications/*` — the bell UI's REST surface.
//!
//! Five endpoints back the FE `NotificationBell`:
//!
//! - `GET  /api/notifications` — list newest-first; optional
//!   `unread=true` filter, `before` cursor, `limit` cap.
//! - `GET  /api/notifications/unread` — badge-only fast path (count).
//! - `POST /api/notifications/{id}/read` — mark one as read.
//! - `POST /api/notifications/read-all` — bulk mark-all-read.
//! - `DELETE /api/notifications/{id}` — hard-delete one row.
//!
//! Every endpoint scopes on `auth_user.id` at the SQL layer via the
//! application service, so an id enumeration against
//! `POST /api/notifications/{id}/read` returns the same 204 whether
//! the row exists-and-belongs-to-somebody-else, or doesn't exist at
//! all. Anti-enumeration is the reason the response body doesn't
//! distinguish "already read" from "not yours" — the service returns
//! a bool for our logs, we always return 204 to the wire.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::services::notification_application_service::NotificationApplicationService;
use crate::domain::entities::notification::Notification;
use crate::domain::repositories::notification_repository::NotificationListFilter;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

/// Wire shape for one notification row. `payload` stays a raw JSON
/// value — per-kind decoding happens on the FE using the `kind`
/// discriminant.
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationDto {
    pub id: Uuid,
    pub kind: String,
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
    /// `null` = unread.
    pub read_at: Option<DateTime<Utc>>,
}

impl From<Notification> for NotificationDto {
    fn from(n: Notification) -> Self {
        Self {
            id: n.id,
            kind: n.kind,
            payload: n.payload,
            created_at: n.created_at,
            read_at: n.read_at,
        }
    }
}

/// Query params for `GET /api/notifications`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ListQuery {
    /// When `true`, return only unread rows. Default: `false` (both).
    #[serde(default)]
    pub unread: bool,
    /// Older-than cursor — return rows strictly BEFORE this
    /// `created_at`. Used by the "load older page" pagination flow.
    /// Omit for the newest page.
    pub before: Option<DateTime<Utc>>,
    /// Newer-than cursor — return rows strictly AFTER this
    /// `created_at`. Used by the FE bell on WS reconnect / tab
    /// reactivation to catch up on rows that arrived during a
    /// disconnect window. Combines with `before` if both are set.
    pub after: Option<DateTime<Utc>>,
    /// Max rows returned. Server-side clamp at 500.
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListResponseDto {
    pub items: Vec<NotificationDto>,
    /// Unread rows for this user across the whole table — the bell
    /// badge reads this. Kept on the list response so a bell open
    /// doesn't need a second round-trip for the badge.
    pub unread_count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UnreadCountDto {
    pub unread_count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MarkAllReadResponseDto {
    /// Number of rows that transitioned unread → read.
    pub marked: u64,
}

/// GET /api/notifications
#[utoipa::path(
    get,
    path = "/api/notifications",
    params(
        ("unread" = Option<bool>, Query, description = "Only return unread rows"),
        ("before" = Option<DateTime<Utc>>, Query, description = "Cursor — rows strictly before this created_at (load-older pagination)"),
        ("after" = Option<DateTime<Utc>>, Query, description = "Cursor — rows strictly after this created_at (delta catch-up on WS reconnect / tab reactivation)"),
        ("limit" = Option<u32>, Query, description = "Max rows (server-side clamp at 500)"),
    ),
    responses(
        (status = 200, description = "List of notifications", body = ListResponseDto),
    ),
    security(("bearerAuth" = [])),
    tag = "notifications"
)]
pub async fn list_notifications(
    State(service): State<Arc<NotificationApplicationService>>,
    auth_user: AuthUser,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponseDto>, AppError> {
    let filter = NotificationListFilter {
        limit: query.limit,
        unread_only: query.unread,
        before: query.before,
        after: query.after,
    };
    let rows = service.list_for_user(auth_user.id, filter).await?;
    let unread_count = service.count_unread_for_user(auth_user.id).await?;
    Ok(Json(ListResponseDto {
        items: rows.into_iter().map(NotificationDto::from).collect(),
        unread_count,
    }))
}

/// GET /api/notifications/unread — badge-only fast path.
#[utoipa::path(
    get,
    path = "/api/notifications/unread",
    responses(
        (status = 200, description = "Unread count", body = UnreadCountDto),
    ),
    security(("bearerAuth" = [])),
    tag = "notifications"
)]
pub async fn unread_count(
    State(service): State<Arc<NotificationApplicationService>>,
    auth_user: AuthUser,
) -> Result<Json<UnreadCountDto>, AppError> {
    let unread_count = service.count_unread_for_user(auth_user.id).await?;
    Ok(Json(UnreadCountDto { unread_count }))
}

/// POST /api/notifications/{id}/read — mark one as read.
///
/// Always responds 204 regardless of whether the row existed and
/// belonged to the caller — the service's `bool` return is logged
/// (audit reason `notification.marked_read` on success), never
/// surfaced to the wire.
#[utoipa::path(
    post,
    path = "/api/notifications/{id}/read",
    params(("id" = Uuid, Path, description = "Notification id")),
    responses((status = 204, description = "Marked read (idempotent, anti-enum)")),
    security(("bearerAuth" = [])),
    tag = "notifications"
)]
pub async fn mark_read(
    State(service): State<Arc<NotificationApplicationService>>,
    auth_user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let transitioned = service.mark_read(id, auth_user.id).await?;
    if transitioned {
        tracing::debug!(
            target: "oxicloud::notifications",
            caller_id = %auth_user.id,
            notification_id = %id,
            "notification marked read"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/notifications/read-all — bulk mark-all-read.
#[utoipa::path(
    post,
    path = "/api/notifications/read-all",
    responses((status = 200, description = "Rows marked", body = MarkAllReadResponseDto)),
    security(("bearerAuth" = [])),
    tag = "notifications"
)]
pub async fn mark_all_read(
    State(service): State<Arc<NotificationApplicationService>>,
    auth_user: AuthUser,
) -> Result<Json<MarkAllReadResponseDto>, AppError> {
    let marked = service.mark_all_read(auth_user.id).await?;
    Ok(Json(MarkAllReadResponseDto { marked }))
}

/// DELETE /api/notifications/{id} — hard-delete one row.
///
/// Same anti-enum semantics as `mark_read` — always 204.
#[utoipa::path(
    delete,
    path = "/api/notifications/{id}",
    params(("id" = Uuid, Path, description = "Notification id")),
    responses((status = 204, description = "Deleted (idempotent, anti-enum)")),
    security(("bearerAuth" = [])),
    tag = "notifications"
)]
pub async fn delete_notification(
    State(service): State<Arc<NotificationApplicationService>>,
    auth_user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let deleted = service.delete(id, auth_user.id).await?;
    if deleted {
        tracing::debug!(
            target: "oxicloud::notifications",
            caller_id = %auth_user.id,
            notification_id = %id,
            "notification deleted"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}
