use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info};

use crate::application::dtos::file_dto::FileDto;
use crate::common::di::AppState;
use crate::interfaces::middleware::auth::AuthUser;

/// Query parameters for the photos timeline endpoint.
#[derive(Deserialize)]
pub struct PhotosQueryParams {
    /// Cursor: only return items with sort_date < this value (epoch seconds).
    pub before: Option<i64>,
    /// Max items to return (default 200, max 500).
    pub limit: Option<i64>,
}

/// Lists all image/video files for the authenticated user, sorted by
/// capture date (EXIF DateTimeOriginal) falling back to upload date.
///
/// Supports cursor-based pagination via the `before` parameter.
/// The `X-Next-Cursor` response header contains the cursor for the next page.
#[utoipa::path(
    get,
    path = "/api/photos",
    params(
        ("before" = Option<i64>, Query, description = "Cursor: only return items with sort_date before this epoch value"),
        ("limit" = Option<i64>, Query, description = "Max items to return (default 200, max 500)")
    ),
    responses(
        (status = 200, description = "List of media files sorted by capture date"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    tag = "photos"
)]
pub async fn list_photos(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Query(params): Query<PhotosQueryParams>,
) -> impl IntoResponse {
    let user_id = auth_user.id;
    let limit = params.limit.unwrap_or(200).clamp(1, 500);

    let file_read = &state.repositories.file_read_repository;

    match file_read
        .list_media_files(user_id, params.before, limit)
        .await
    {
        Ok((files, sort_dates)) => {
            info!("Photos: returned {} media files for user", files.len());

            // Convert to DTOs with sort_date populated
            let dtos: Vec<FileDto> = files
                .into_iter()
                .zip(sort_dates.iter())
                .map(|(file, &sd)| {
                    let mut dto = FileDto::from(file);
                    dto.sort_date = Some(sd as u64);
                    dto
                })
                .collect();

            // Set cursor header for next page
            let mut response = Json(&dtos).into_response();
            if let Some(&last_sd) = sort_dates.last() {
                response
                    .headers_mut()
                    .insert("X-Next-Cursor", last_sd.to_string().parse().unwrap());
            }

            response
        }
        Err(err) => {
            error!("Error listing photos: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to list photos: {}", err)
                })),
            )
                .into_response()
        }
    }
}
