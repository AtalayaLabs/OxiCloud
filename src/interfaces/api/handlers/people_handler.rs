//! HTTP handlers for the People (faces) feature.
//!
//! Every route is mounted only when `OXICLOUD_ENABLE_FACES` is on (the service
//! is present in `AppState`); each handler is also defensive. All work is
//! strictly caller-scoped by `PeopleService` (the repository filters by user).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::application::dtos::people_dto::{FaceBoxDto, PersonDto};
use crate::common::di::AppState;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use utoipa::ToSchema;

fn disabled() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "People feature is disabled" })),
    )
        .into_response()
}

fn bad_id() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": "invalid id" })),
    )
        .into_response()
}

/// GET /api/people — identity clusters for the caller.
#[utoipa::path(
    get,
    path = "/api/people",
    responses(
        (status = 200, description = "Identity clusters", body = [PersonDto]),
        (status = 404, description = "People feature disabled"),
    ),
    tag = "people"
)]
pub async fn list_people(State(state): State<Arc<AppState>>, auth_user: AuthUser) -> Response {
    let Some(svc) = state.people_service.as_ref() else {
        return disabled();
    };
    match svc.list_people(auth_user.id).await {
        Ok(people) => Json(people).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// GET /api/people/{id}/photos — file ids of a person's photos.
#[utoipa::path(
    get,
    path = "/api/people/{id}/photos",
    params(("id" = String, Path, description = "Person cluster id")),
    responses(
        (status = 200, description = "File ids where this person appears", body = [String]),
        (status = 400, description = "Malformed person id"),
        (status = 404, description = "People feature disabled"),
    ),
    tag = "people"
)]
pub async fn person_photos(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Some(svc) = state.people_service.as_ref() else {
        return disabled();
    };
    let Ok(person_id) = Uuid::parse_str(&id) else {
        return bad_id();
    };
    match svc.person_photos(auth_user.id, person_id).await {
        Ok(files) => Json(files).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[derive(Deserialize, ToSchema)]
pub struct RenameBody {
    /// New display name for the cluster. `null` or omitted clears
    /// the name, reverting the cluster to its "Unnamed" state.
    pub name: Option<String>,
}

/// PATCH /api/people/{id} — name (or clear the name of) a person.
#[utoipa::path(
    patch,
    path = "/api/people/{id}",
    params(("id" = String, Path, description = "Person cluster id")),
    request_body = RenameBody,
    responses(
        (status = 204, description = "Renamed"),
        (status = 400, description = "Malformed person id"),
        (status = 404, description = "People feature disabled"),
    ),
    tag = "people"
)]
pub async fn rename_person(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> Response {
    let Some(svc) = state.people_service.as_ref() else {
        return disabled();
    };
    let Ok(person_id) = Uuid::parse_str(&id) else {
        return bad_id();
    };
    match svc.rename_person(auth_user.id, person_id, body.name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

#[derive(Deserialize, ToSchema)]
pub struct MergeBody {
    /// Cluster id that will absorb the other one (`from`'s photos are
    /// reattributed to `into`; `from` is deleted). Typically the
    /// larger / named cluster wins.
    pub into: String,
    /// Cluster id being merged into `into`. Deleted after the merge.
    pub from: String,
}

/// POST /api/people/merge — merge `from` into `into`.
#[utoipa::path(
    post,
    path = "/api/people/merge",
    request_body = MergeBody,
    responses(
        (status = 204, description = "Merged"),
        (status = 400, description = "Malformed cluster id(s)"),
        (status = 404, description = "People feature disabled"),
    ),
    tag = "people"
)]
pub async fn merge_people(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(body): Json<MergeBody>,
) -> Response {
    let Some(svc) = state.people_service.as_ref() else {
        return disabled();
    };
    let (Ok(into), Ok(from)) = (Uuid::parse_str(&body.into), Uuid::parse_str(&body.from)) else {
        return bad_id();
    };
    match svc.merge(auth_user.id, into, from).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// POST /api/people/recluster — re-run identity clustering for the caller.
#[utoipa::path(
    post,
    path = "/api/people/recluster",
    responses(
        (status = 200, description = "Recluster complete", body = serde_json::Value),
        (status = 404, description = "People feature disabled"),
    ),
    tag = "people"
)]
pub async fn recluster(State(state): State<Arc<AppState>>, auth_user: AuthUser) -> Response {
    let Some(svc) = state.people_service.as_ref() else {
        return disabled();
    };
    match svc.recluster(auth_user.id).await {
        Ok(n) => Json(serde_json::json!({ "persons_created": n })).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// DELETE /api/people/data — erase all of the caller's face data.
#[utoipa::path(
    delete,
    path = "/api/people/data",
    responses(
        (status = 204, description = "All face data erased"),
        (status = 404, description = "People feature disabled"),
    ),
    tag = "people"
)]
pub async fn delete_all(State(state): State<Arc<AppState>>, auth_user: AuthUser) -> Response {
    let Some(svc) = state.people_service.as_ref() else {
        return disabled();
    };
    match svc.delete_all(auth_user.id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// GET /api/people/faces/{file_id} — face boxes within a photo (lightbox tags).
#[utoipa::path(
    get,
    path = "/api/people/faces/{file_id}",
    params(("file_id" = String, Path, description = "Photo file id")),
    responses(
        (status = 200, description = "Face bounding boxes in the photo", body = [FaceBoxDto]),
        (status = 400, description = "Malformed file id"),
        (status = 404, description = "People feature disabled"),
    ),
    tag = "people"
)]
pub async fn faces_for_file(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(file_id): Path<String>,
) -> Response {
    let Some(svc) = state.people_service.as_ref() else {
        return disabled();
    };
    let Ok(fid) = Uuid::parse_str(&file_id) else {
        return bad_id();
    };
    match svc.faces_for_file(auth_user.id, fid).await {
        Ok(boxes) => Json(boxes).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}
