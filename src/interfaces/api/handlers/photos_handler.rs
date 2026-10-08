use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info};

use crate::application::dtos::cursor::{CursorListResponse, PageCursor};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::geo_dto::GeoBounds;
use crate::application::dtos::grant_dto::{ResourceContentDto, ResourceTypeDto};
use crate::application::dtos::photos_dto::{
    PhotoKind, PhotoOrderBy, PhotoResourceItemDto, PhotosCursor, PhotosFilter, PhotosResourcesDto,
};
use crate::common::di::AppState;
use crate::interfaces::api::etag::{if_none_match_matches, not_modified, with_cache_headers};
use crate::interfaces::middleware::auth::AuthUser;
use uuid::Uuid;

// §4 of docs/plan/photos-resources-migration.md hard-cut the legacy
// `GET /api/photos` route in favour of `/api/photos/resources`. The
// old `PhotoDto`, `PhotosQueryParams`, and `list_photos` handler
// were removed together with `FileDto::sort_date` — the field was
// only ever populated by this endpoint, and the new envelope carries
// `sort_date` at the item level alongside the other photo signals.

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
    /// Narrow to one media family (§6). `photo` matches `image/%`,
    /// `video` matches `video/%`, `all` (default) keeps both
    /// interleaved — the pre-filter behaviour.
    pub kind: Option<PhotoKind>,
    /// Restrict the listing to one accessible drive (§6b). Omit for
    /// the cross-drive feed. A `drive_id` the caller cannot see
    /// returns an empty page (same anti-enum shape as a drive with
    /// no photos) — no 403/404 disclosure difference.
    pub drive_id: Option<Uuid>,
    /// Narrow to the caller's favourited rows only. Omit (or send
    /// `false`) for the full feed; `true` filters via the same
    /// `auth.user_favorites` EXISTS the `is_favorite` projection
    /// already runs on every row.
    pub favorite_only: Option<bool>,
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
    headers: HeaderMap,
    Query(params): Query<PhotosResourcesQueryParams>,
) -> impl IntoResponse {
    let caller_id = auth_user.id;
    let limit = params.limit.clamp(1, 200) as i64;
    let requested_order = params.order_by.unwrap_or_default();
    let requested_filter = PhotosFilter {
        kind: params.kind.unwrap_or_default(),
        drive_id: params.drive_id,
        favorite_only: params.favorite_only.unwrap_or(false),
    };

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
    // on a mangled cursor. A cursor whose `order_by`, `kind`, or
    // `drive_id` disagrees with the request fails the same way —
    // §3 / §6 / §6b all share the "cursor must match the axes the
    // request names" rule so pagination can't drift across a filter
    // flip mid-scroll.
    let decoded = match params.cursor.as_deref() {
        None => None,
        Some(raw) => match PhotosCursor::decode(raw) {
            Some(c)
                if c.order_by == requested_order
                    && c.kind == requested_filter.kind
                    && c.drive_id == requested_filter.drive_id
                    && c.favorite_only == requested_filter.favorite_only =>
            {
                Some(c)
            }
            Some(c) => {
                let axis = if c.order_by != requested_order {
                    "order_by"
                } else if c.kind != requested_filter.kind {
                    "kind"
                } else if c.drive_id != requested_filter.drive_id {
                    "drive_id"
                } else {
                    "favorite_only"
                };
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error_type": "bad_request",
                        "message": format!("cursor was issued against a different {axis} axis"),
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
        .list_media_resources(caller_id, decoded.as_ref(), requested_filter, limit + 1)
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
    // Freshness signal for the ETag: the newest `media_sort_date`
    // in this page. Empty page collapses to 0 — stable identity
    // across repeat empty revalidations (`photos_resources.hurl`
    // step 3), bumps the moment a row lands.
    let fresh_signal: u64 = rows
        .iter()
        .map(|r| r.sort_date_ts.timestamp().max(0) as u64)
        .max()
        .unwrap_or(0);
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
                kind: requested_filter.kind,
                drive_id: requested_filter.drive_id,
                favorite_only: requested_filter.favorite_only,
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
            PhotoResourceItemDto {
                resource_type: ResourceTypeDto::File,
                resource: ResourceContentDto::File(dto),
                width: r.width,
                height: r.height,
                sort_date: r.sort_date,
                captured_at: r.captured_at,
                orientation: r.orientation,
                has_gps: r.has_gps,
                has_blob_siblings: r.has_blob_siblings,
            }
        })
        .collect();

    let envelope = CursorListResponse::<PhotoResourceItemDto>::with_cursor(items, next_cursor);

    // Compute the ETag from (cursor-input, limit, max media_sort_date,
    // row count, next_cursor) and short-circuit a repeated page fetch
    // with an empty 304 — §2 of the plan. The browser's HTTP cache
    // then re-serves the kept body without reshipping any tile.
    let etag = envelope.weak_etag(params.cursor.as_deref(), limit as usize, fresh_signal);
    if if_none_match_matches(&headers, &etag) {
        return not_modified(&etag).into_response();
    }

    with_cache_headers(Json(envelope), &etag).into_response()
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
