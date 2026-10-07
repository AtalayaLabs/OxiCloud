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

use crate::application::dtos::cursor::{CursorListResponse, PageCursor};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::geo_dto::GeoBounds;
use crate::application::dtos::grant_dto::{ResourceContentDto, ResourceTypeDto};
use crate::application::dtos::photos_dto::{
    PhotoOrderBy, PhotoResourceItemDto, PhotosCursor, PhotosResourcesDto,
};
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

/// Query parameters for `GET /api/photos/resources` — the normalized
/// envelope endpoint. Deliberately NOT re-using the opaque base64 cursor
/// via `CursorQuery`'s `sort_by: Option<String>` because photos has a
/// typed [`PhotoOrderBy`] enum we want validated at wire-decode time;
/// `cursor` and `limit` reuse the same convention.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct PhotosResourcesQueryParams {
    /// Max items to return (1-200, default 50).
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Opaque cursor from a previous response. Absent on page 1.
    pub cursor: Option<String>,
    /// Sort axis. `captured_at` (default) orders by
    /// `COALESCE(captured_at, created_at)`; `created_at` is reserved
    /// for §3 and refused today with 400.
    pub order_by: Option<PhotoOrderBy>,
}

fn default_limit() -> u32 {
    50
}

/// `GET /api/photos/resources` — normalized photos listing (§1 of
/// `docs/plan/photos-resources-migration.md`).
///
/// Returns the standard `CursorListResponse` envelope every other
/// `/resources` endpoint uses, with photo-specific signals
/// (`width`, `height`, `sort_date`, `captured_at`, `orientation`,
/// `has_gps`) at the item level — siblings to `resource` — so
/// `FileDto` stays shape-identical to every other file-listing
/// endpoint. Compare `GET /api/photos` which flattens everything
/// into a bare array and is slated for removal in §4.
///
/// On a bad cursor (undecodable base64, mismatched `order_by`) the
/// handler returns 400; invalid payloads must fail loud rather than
/// paginate across axes.
#[utoipa::path(
    get,
    path = "/api/photos/resources",
    params(PhotosResourcesQueryParams),
    responses(
        (status = 200, body = PhotosResourcesDto, description = "Page of media files with photo-level signals"),
        (status = 400, description = "Invalid cursor or unsupported order_by"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearerAuth" = [])),
    tag = "photos"
)]
pub async fn list_photos_resources(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Query(params): Query<PhotosResourcesQueryParams>,
) -> impl IntoResponse {
    let caller_id = auth_user.id;
    let limit = params.limit.clamp(1, 200) as i64;
    let requested_order = params.order_by.unwrap_or_default();

    // §3 reserves `order_by=created_at`; today the only accepted axis
    // is `captured_at` (the default). Reject other values with 400
    // explicitly rather than silently falling back — a client that
    // sent `?order_by=created_at` would otherwise think it got the
    // alternate axis when it got the default.
    if requested_order != PhotoOrderBy::CapturedAt {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error_type": "bad_request",
                "message": "order_by=created_at is reserved for §3 of the photos-resources migration and not yet accepted"
            })),
        )
            .into_response();
    }

    // Decode the opaque cursor. An undecodable string yields 400
    // (not "start from the top") so pagination can't drift silently
    // on a mangled cursor. A cursor whose `order_by` disagrees with
    // the request fails the same way — same rule as §3 later.
    let decoded = match params.cursor.as_deref() {
        None => None,
        Some(raw) => match PhotosCursor::decode(raw) {
            Some(c) if c.order_by == requested_order => Some(c),
            Some(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error_type": "bad_request",
                        "message": "cursor was issued against a different order_by axis"
                    })),
                )
                    .into_response();
            }
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error_type": "bad_request",
                        "message": "cursor is not a valid photos cursor"
                    })),
                )
                    .into_response();
            }
        },
    };

    let file_read = &state.repositories.file_read_repository;

    // Over-fetch limit+1 so the handler can detect "another page
    // exists" without a second COUNT(*) query — same convention
    // `CursorListResponse::from_oversized` uses on favorites and
    // recents.
    let rows = match file_read
        .list_media_resources(caller_id, decoded.as_ref(), limit + 1)
        .await
    {
        Ok(r) => r,
        Err(err) => {
            error!("list_photos_resources: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error_type": "internal_error",
                    "message": format!("Failed to list photos: {}", err)
                })),
            )
                .into_response();
        }
    };

    info!(
        "list_photos_resources: {} media rows for caller",
        rows.len()
    );

    // Split off the over-fetched tail BEFORE building items; the
    // next cursor is derived from the LAST KEPT row (the row at
    // index `limit - 1` once the tail is popped).
    let mut rows = rows;
    let has_more = rows.len() > limit as usize;
    if has_more {
        rows.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        rows.last().map(|r| {
            // Full-precision `sort_date_ts` (not `sort_date`
            // epoch-seconds) — `storage.files.media_sort_date` has
            // microsecond precision and the WHERE predicate compares
            // against it; truncating to seconds in the cursor drops
            // rows at the page boundary when two uploads land in the
            // same wall-clock second.
            PhotosCursor {
                order_by: requested_order,
                sort_value: r.sort_date_ts,
                file_id: r.file.id().parse().unwrap_or_default(),
            }
            .encode()
        })
    } else {
        None
    };

    let items: Vec<PhotoResourceItemDto> = rows
        .into_iter()
        .map(|r| {
            let mut dto = FileDto::from(r.file);
            dto.is_favorite = r.is_favorite;
            dto.is_shared = r.is_shared;
            // sort_date is at the item level now (not on FileDto) —
            // §4 will remove the field on FileDto entirely.
            PhotoResourceItemDto {
                resource_type: ResourceTypeDto::File,
                resource: ResourceContentDto::File(dto),
                width: r.width,
                height: r.height,
                sort_date: r.sort_date,
                captured_at: r.captured_at,
                orientation: r.orientation,
                has_gps: r.has_gps,
            }
        })
        .collect();

    Json(CursorListResponse::<PhotoResourceItemDto>::with_cursor(
        items,
        next_cursor,
    ))
    .into_response()
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
