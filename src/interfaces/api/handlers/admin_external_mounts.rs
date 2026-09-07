//! Admin CRUD for external file mounts (`/api/admin/external-mounts`).
//!
//! Creating a mount: validate the backend config, create a mount-root folder
//! under the selected drive, insert the `external_mounts` row, then hot-reload
//! the in-memory registry. Deleting: remove the row + the folder and reload.
//! Every endpoint is admin-gated.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::application::ports::external_mount_ports::{
    ExternalMountRecord, ExternalMountRepositoryPort, MountProviderFactory, NewExternalMount,
};
use crate::common::di::AppState;
use crate::domain::repositories::drive_repository::{DriveRepository, DriveRepositoryError};
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::infrastructure::repositories::pg::ExternalMountPgRepository;
use crate::infrastructure::services::mount_provider_factory::DefaultMountProviderFactory;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::admin::require_admin;

/// JSON view of a configured mount.
#[derive(Debug, Serialize)]
pub struct ExternalMountResponse {
    pub mount_folder_id: String,
    pub name: String,
    pub kind: String,
    pub owner_id: String,
    pub read_only: bool,
    pub drive_id: String,
    pub mount_path: String,
    pub config: serde_json::Value,
}

impl From<ExternalMountRecord> for ExternalMountResponse {
    fn from(r: ExternalMountRecord) -> Self {
        Self {
            mount_folder_id: r.mount_folder_id.to_string(),
            name: r.name,
            kind: r.kind,
            owner_id: r.owner_id.to_string(),
            read_only: r.read_only,
            drive_id: r.drive_id.to_string(),
            mount_path: r.mount_path,
            config: r.config,
        }
    }
}

/// Request body for creating a mount.
#[derive(Debug, Deserialize)]
pub struct CreateExternalMountRequest {
    /// Display name (also the mount-root folder name).
    pub name: String,
    /// Drive that owns the mount and controls access through its grants.
    pub drive_id: Uuid,
    /// Absolute host path for the `local_fs` provider.
    pub host_path: String,
    /// Provider kind. Defaults to `local_fs`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// When true, the mount refuses all mutations.
    #[serde(default)]
    pub read_only: bool,
}

fn default_kind() -> String {
    "local_fs".to_string()
}

fn pool(state: &AppState) -> Result<Arc<sqlx::PgPool>, AppError> {
    state
        .db_pool
        .clone()
        .ok_or_else(|| AppError::internal_error("Database not available"))
}

/// `GET /api/admin/external-mounts` — list all configured mounts.
pub async fn list_external_mounts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    require_admin(&state, &headers).await?;
    let repo = ExternalMountPgRepository::new(pool(&state)?);
    let mounts = repo
        .list_all()
        .await
        .map_err(|e| AppError::internal_error(format!("list external mounts: {e}")))?;
    let out: Vec<ExternalMountResponse> = mounts.into_iter().map(Into::into).collect();
    Ok(Json(out))
}

/// `POST /api/admin/external-mounts` — create a mount in the selected drive.
pub async fn create_external_mount(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateExternalMountRequest>,
) -> Result<impl IntoResponse, AppError> {
    let (admin_id, _role) = require_admin(&state, &headers).await?;

    if req.name.trim().is_empty() {
        return Err(AppError::bad_request("Mount name must not be empty"));
    }

    // Build the provider config and validate it up front (path exists, etc.).
    let config = serde_json::json!({ "path": req.host_path, "read_only": req.read_only });
    let factory = DefaultMountProviderFactory::new();
    factory
        .build(&req.kind, &config)
        .await
        .map_err(|e| AppError::bad_request(format!("invalid mount configuration: {e}")))?;

    // The mount-root is a normal folder in the selected drive. All access to
    // the external provider is authorized against this folder, so personal
    // and shared drive membership automatically applies to the mount.
    let drive = state
        .drive_repo
        .get_by_id(req.drive_id)
        .await
        .map_err(|e| match e {
            DriveRepositoryError::NotFound(_) => {
                AppError::bad_request("Destination drive not found")
            }
            other => AppError::internal_error(format!("find destination drive: {other}")),
        })?;
    let root_folder_id = drive.drive.root_folder_id.to_string();

    let folder = state
        .repositories
        .folder_repository
        .create_folder(req.name.clone(), Some(root_folder_id), admin_id)
        .await
        .map_err(AppError::from)?;
    let mount_folder_id =
        Uuid::parse_str(folder.id()).map_err(|_| AppError::internal_error("bad folder id"))?;

    let repo = ExternalMountPgRepository::new(pool(&state)?);
    repo.create(&NewExternalMount {
        mount_folder_id,
        kind: req.kind.clone(),
        config,
        name: req.name.clone(),
        owner_id: admin_id,
        read_only: req.read_only,
    })
    .await
    .map_err(|e| AppError::internal_error(format!("create mount row: {e}")))?;

    // Hot-reload so the new mount is live immediately.
    state.mount_router.registry().reload(&repo, &factory).await;

    tracing::info!(
        target: "audit",
        event = "external_mount.config",
        action = "create",
        mount_id = %mount_folder_id,
        drive_id = %req.drive_id,
        caller_id = %admin_id,
        kind = %req.kind,
        reason = "external_mount_admin",
        "👮🏻‍♂️ external mount created",
    );

    // Return the freshly created mount.
    let created = repo
        .list_all()
        .await
        .map_err(|e| AppError::internal_error(format!("reload mounts: {e}")))?
        .into_iter()
        .find(|m| m.mount_folder_id == mount_folder_id)
        .map(ExternalMountResponse::from)
        .ok_or_else(|| AppError::internal_error("created mount not found"))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `DELETE /api/admin/external-mounts/{id}` — remove a mount (and its root
/// folder). The host filesystem content is untouched.
pub async fn delete_external_mount(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let (admin_id, _role) = require_admin(&state, &headers).await?;

    let repo = ExternalMountPgRepository::new(pool(&state)?);
    let removed = repo
        .delete(id)
        .await
        .map_err(|e| AppError::internal_error(format!("delete mount row: {e}")))?;
    if !removed {
        return Err(AppError::not_found("Mount not found"));
    }

    // Remove the mount-root folder row (host content is left intact).
    state
        .repositories
        .folder_repository
        .delete_folder(&id.to_string())
        .await
        .map_err(AppError::from)?;

    let factory = DefaultMountProviderFactory::new();
    state.mount_router.registry().reload(&repo, &factory).await;

    tracing::info!(
        target: "audit",
        event = "external_mount.config",
        action = "delete",
        mount_id = %id,
        caller_id = %admin_id,
        reason = "external_mount_admin",
        "👮🏻‍♂️ external mount deleted",
    );

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::CreateExternalMountRequest;

    #[test]
    fn create_request_requires_destination_drive() {
        let missing_drive =
            serde_json::from_value::<CreateExternalMountRequest>(serde_json::json!({
                "name": "Media",
                "host_path": "/srv/media"
            }));

        assert!(missing_drive.is_err());
    }

    #[test]
    fn create_request_accepts_destination_drive() {
        let drive_id = uuid::Uuid::new_v4();
        let request = serde_json::from_value::<CreateExternalMountRequest>(serde_json::json!({
            "name": "Media",
            "host_path": "/srv/media",
            "drive_id": drive_id
        }))
        .expect("valid external mount request");

        assert_eq!(request.drive_id, drive_id);
        assert_eq!(request.kind, "local_fs");
        assert!(!request.read_only);
    }
}
