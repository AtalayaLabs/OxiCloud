use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::dtos::folder_dto::FolderDto;
use crate::application::ports::folder_ports::FolderUseCase;
use crate::common::di::AppState;
use crate::domain::services::authorization::Subject;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use std::sync::Arc;

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateLinkRequest {
    /// Folder or drive-root folder the caller holds a grant path to.
    pub target_id: Uuid,
}

/// Restores the caller's link to a shared folder or drive, clearing a decline.
#[utoipa::path(
    post,
    path = "/api/links",
    request_body = CreateLinkRequest,
    responses(
        (status = 201, description = "Link created or already present", body = FolderDto),
        (status = 404, description = "No grant path to the target"),
        (status = 409, description = "No free name for the link"),
    ),
    security(("bearerAuth" = [])),
    tag = "links"
)]
pub async fn create_link(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(req): Json<CreateLinkRequest>,
) -> Result<(StatusCode, Json<FolderDto>), AppError> {
    let mount = state
        .share_mount_service
        .remount(auth_user.id, req.target_id)
        .await?;
    let dto = state
        .applications
        .folder_service
        .get_folder_with_perms(&mount.mount_id.to_string(), Subject::User(auth_user.id))
        .await?;
    Ok((StatusCode::CREATED, Json(dto)))
}
