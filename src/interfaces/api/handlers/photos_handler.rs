use axum::{
    Json,
    body::Body,
    extract::{Query, State},
    http::{Response, StatusCode, header},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info};

use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::geo_dto::GeoBounds;
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

/// Photos-timeline item: a `FileDto` plus the image's original pixel
/// dimensions (from EXIF/metadata), flattened into the same JSON shape so
/// the gallery can lay tiles out at their true aspect ratio without a
/// second per-file metadata round-trip.
#[derive(Serialize, utoipa::ToSchema)]
struct PhotoDto {
    #[serde(flatten)]
    file: FileDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
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
        (status = 200, body = Vec<PhotoDto>, description = "List of media files sorted by capture date"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearerAuth" = [])),
    tag = "photos"
)]
pub async fn list_photos(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Query(params): Query<PhotosQueryParams>,
    req: axum::extract::Request,
) -> impl IntoResponse {
    // Borrow headers (`req.headers()`) instead of cloning the whole request
    // header table via the `HeaderMap` extractor to read one If-None-Match — the
    // gallery open + every pagination page hit this (benches/ROUND22.md §H1).
    let caller_id = auth_user.id;
    let limit = params.limit.unwrap_or(200).clamp(1, 500);

    let file_read = &state.repositories.file_read_repository;

    match file_read
        .list_media_files(caller_id, params.before, limit)
        .await
    {
        Ok((files, sort_dates, dims, flags)) => {
            // Lightweight revalidation ETag: page identity (cursor + limit) plus a
            // freshness signal (max modified_at + row count over the page),
            // mirroring the file-list endpoint. With `Cache-Control: no-cache` the
            // browser always revalidates with If-None-Match, so a "navigate away
            // and back" to an unchanged gallery returns an empty 304 instead of
            // rebuilding 500 DTOs + reserializing + reshipping the whole body.
            let max_mod = files.iter().map(|f| f.modified_at()).max().unwrap_or(0);
            let count = files.len();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&params.before, &mut hasher);
            std::hash::Hash::hash(&limit, &mut hasher);
            std::hash::Hash::hash(&max_mod, &mut hasher);
            std::hash::Hash::hash(&count, &mut hasher);
            let etag = format!("\"{:x}\"", std::hash::Hasher::finish(&hasher));

            if let Some(inm) = req.headers().get(header::IF_NONE_MATCH)
                && let Ok(client_etag) = inm.to_str()
                && client_etag == etag
            {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, &etag)
                    .header(header::CACHE_CONTROL, "private, no-cache")
                    .body(Body::empty())
                    .unwrap()
                    .into_response();
            }

            info!("Photos: returned {} media files for user", count);

            // Convert to DTOs with sort_date + pixel dimensions + inline
            // caller flags populated. `list_media_files` computes
            // `is_favorite` / `is_shared` via two per-row `EXISTS`
            // columns in its SELECT — the same pattern the four
            // `list_resources_paged` repos use — so this stays a
            // single round trip regardless of page size.
            let dtos: Vec<PhotoDto> = files
                .into_iter()
                .zip(sort_dates.iter())
                .zip(dims.iter())
                .zip(flags.iter())
                .map(|(((file, &sd), &(w, h)), &(is_fav, is_shr))| {
                    let mut dto = FileDto::from(file);
                    dto.sort_date = Some(sd as u64);
                    dto.is_favorite = is_fav;
                    dto.is_shared = is_shr;
                    PhotoDto {
                        file: dto,
                        width: w.map(|v| v.max(0) as u32),
                        height: h.map(|v| v.max(0) as u32),
                    }
                })
                .collect();

            // Pre-sized serialization (benches/ROUND12.md §M1).
            let mut response = crate::interfaces::api::sized_json::sized_json(
                64 + dtos.len() * crate::interfaces::api::sized_json::EST_WRAPPED_ROW_BYTES,
                &dtos,
            );
            {
                let h = response.headers_mut();
                h.insert(header::ETAG, header::HeaderValue::from_str(&etag).unwrap());
                h.insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("private, no-cache"),
                );
                if let Some(&last_sd) = sort_dates.last() {
                    h.insert("X-Next-Cursor", last_sd.to_string().parse().unwrap());
                }
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

/// Query parameters for the photos map (clustered) endpoint.
#[derive(Deserialize)]
pub struct GeoQueryParams {
    /// Bounding box as `west,south,east,north` (decimal degrees).
    pub bbox: String,
    /// Slippy-map zoom level (0–20); controls cluster granularity.
    pub zoom: Option<u8>,
}

/// Lists the caller's geotagged photos aggregated into map clusters within a
/// bounding box. Gated on `OXICLOUD_ENABLE_PLACES` (the route is only mounted
/// when the Places service is present).
#[utoipa::path(
    get,
    path = "/api/photos/geo",
    params(
        ("bbox" = String, Query, description = "Bounding box 'west,south,east,north' (decimal degrees)"),
        ("zoom" = Option<u8>, Query, description = "Map zoom level (0-20), controls cluster size")
    ),
    responses(
        (
            status = 200,
            body = [crate::application::dtos::geo_dto::GeoCluster],
            description = "Geotagged photos aggregated into map clusters. Flat array; each \
                           entry is one aggregation cell carrying its centroid (lng/lat), the \
                           number of photos in the cell, and one representative sample_file_id \
                           usable as the cluster thumbnail."
        ),
        (status = 400, description = "Invalid bounding box"),
        (status = 401, description = "Unauthorized")
    ),
    security(("bearerAuth" = [])),
    tag = "photos"
)]
pub async fn list_photos_geo(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Query(params): Query<GeoQueryParams>,
) -> impl IntoResponse {
    let Some(places) = state.places_service.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Places feature is disabled" })),
        )
            .into_response();
    };

    let coords: Vec<f64> = params
        .bbox
        .split(',')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .collect();
    if coords.len() != 4 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "bbox must be 'west,south,east,north'" })),
        )
            .into_response();
    }
    let bounds = GeoBounds {
        west: coords[0],
        south: coords[1],
        east: coords[2],
        north: coords[3],
    };
    let zoom = params.zoom.unwrap_or(3);

    match places.clusters(auth_user.id, bounds, zoom).await {
        Ok(clusters) => Json(clusters).into_response(),
        Err(err) => {
            error!("Error listing photo geo clusters: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("{}", err) })),
            )
                .into_response()
        }
    }
}
