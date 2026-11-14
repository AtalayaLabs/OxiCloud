//! `GET /api/files/recent-changes` — what changed lately, and who changed it.
//!
//! Design: `docs/plan/recently-modified.md` §4. Distinct from `/api/recent`,
//! which answers "what did *I* open" from a per-viewer access log; this reads
//! shared content provenance and names other people.
//!
//! ## Paging a lane-major list
//!
//! On the `modifier` axis the list is grouped by person, lanes ordered by each
//! person's own most recent change, and a page can therefore end *inside* a
//! lane. Continuing takes two reads:
//!
//!   1. the remainder of the lane the cursor landed in, keyed on
//!      `(changed_at, file_id)` within that one person;
//!   2. then whole lanes after it, keyed on `(lane_latest, user_id)`.
//!
//! Both are index scans. The alternative — one query with
//! `MAX(touched_at) OVER (PARTITION BY user_id)` — cannot apply a `LIMIT`
//! before aggregating, so every page would cost a full-window sort.

use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
    response::IntoResponse,
};
use std::sync::Arc;
use tracing::error;
use uuid::Uuid;

use crate::application::dtos::cursor::{CursorListResponse, PageCursor};
use crate::application::dtos::display_helpers::{category_for, intern_mime};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::recent_changes_dto::{
    ChangeLane, DEFAULT_DAYS, FileChangeItemDto, FileChangeRow, MAX_DAYS, MAX_PER_LANE,
    RecentChangesCursor, RecentChangesOrderBy, RecentChangesQuery,
};
use crate::common::di::AppState;
use crate::domain::entities::file::File;
use crate::interfaces::api::etag::{if_none_match_matches, not_modified, with_cache_headers};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

/// `GET /api/files/recent-changes`
///
/// Returns the standard `CursorListResponse` envelope. A file may appear more
/// than once in a page — once per lane that touched it — which is why
/// `changed_at` and `modifier` are per-row rather than per-file.
#[utoipa::path(
    get,
    path = "/api/files/recent-changes",
    params(RecentChangesQuery),
    responses(
        (status = 200, body = crate::application::dtos::cursor::CursorListResponse<crate::application::dtos::recent_changes_dto::FileChangeItemDto>, description = "Page of content changes"),
        (status = 400, description = "Invalid cursor, unsupported order_by, or days out of range", body = crate::interfaces::errors::ApiErrorBody),
        (status = 401, description = "Unauthorized", body = crate::interfaces::errors::ApiErrorBody),
        (status = 403, description = "External users cannot reach this surface", body = crate::interfaces::errors::ApiErrorBody),
        (status = 500, description = "Internal server error", body = crate::interfaces::errors::ApiErrorBody)
    ),
    security(("bearerAuth" = [])),
    tag = "files"
)]
pub async fn list_recent_changes(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    headers: HeaderMap,
    Query(params): Query<RecentChangesQuery>,
) -> impl IntoResponse {
    let caller_id = auth_user.id;
    let limit = params.limit.clamp(1, 200) as i64;
    let order_by = params.order_by.unwrap_or_default();

    // `days` is validated, never clamped: a client asking for a year is told
    // so rather than quietly served a month. The lane aggregate's cost is
    // proportional to the window, which is what the ceiling protects.
    let days = params.days.unwrap_or(DEFAULT_DAYS);
    if days == 0 || days > MAX_DAYS {
        return AppError::bad_request(format!(
            "days must be between 1 and {MAX_DAYS} (got {days})"
        ))
        .into_response();
    }

    // `per_lane` caps each lane at its N most recent files. `0` and absent
    // both mean "no cap" — the client sends one or the other depending on a
    // toggle, and treating them alike keeps that trivial.
    let per_lane: Option<i64> = match params.per_lane {
        None | Some(0) => None,
        Some(n) if n <= MAX_PER_LANE => Some(n as i64),
        Some(n) => {
            return AppError::bad_request(format!(
                "per_lane must be between 1 and {MAX_PER_LANE}, or 0 for no cap (got {n})"
            ))
            .into_response();
        }
    };
    // Capping is a property of LANES, so it means nothing on the
    // chronological axis. Refused rather than ignored: a silently dropped
    // parameter is how a client ends up believing it is paging capped data.
    if per_lane.is_some() && order_by == RecentChangesOrderBy::ChangedAt {
        return AppError::bad_request(
            "per_lane requires order_by=modifier; there are no lanes on changed_at",
        )
        .into_response();
    }
    let per_lane_echo = per_lane.map(|c| c as u32);

    // A cursor minted under a different axis, window, person filter or cap
    // cannot be continued: each of those changes which rows exist, in what
    // order, or where the page boundaries fall, so carrying on would skip or
    // repeat silently. A cap is the sharpest of the four — capped pages break
    // on lane boundaries and uncapped ones anywhere. 400, not a reset to page
    // one: a mangled cursor must not look like a fresh listing.
    let decoded = match params.cursor.as_deref() {
        None => None,
        Some(raw) => match RecentChangesCursor::decode(raw) {
            Some(c)
                if c.order_by == order_by
                    && c.days == days
                    && c.person == params.person
                    && c.per_lane == per_lane_echo =>
            {
                Some(c)
            }
            Some(_) => {
                return AppError::bad_request(
                    "cursor does not match this request's order_by / days / person",
                )
                .into_response();
            }
            None => return AppError::bad_request("malformed cursor").into_response(),
        },
    };

    let Some(repo) = state.repositories.recent_changes_repository.as_ref() else {
        // Unreachable in practice: the route is not mounted when the feature
        // is off. Kept so the handler cannot panic if that ever changes.
        return AppError::not_found("recent changes are not enabled").into_response();
    };

    let days_i32 = days as i32;
    let person_filter: Option<Vec<Uuid>> = params.person.map(|p| vec![p]);

    let (rows, lanes, next_cursor) = match order_by {
        // Chronological: one stream, one index scan, no lanes.
        RecentChangesOrderBy::ChangedAt => {
            let fetched = match repo
                .rows_for_lanes(
                    caller_id,
                    days_i32,
                    person_filter.as_deref().unwrap_or(&[]),
                    decoded.as_ref().map(|c| c.item_ts),
                    decoded.as_ref().map(|c| c.resource_id),
                    limit + 1,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    error!("recent-changes rows: {e}");
                    return AppError::from(e).into_response();
                }
            };
            let (page, next) = split_page(fetched, limit, order_by, days, params.person, None);
            (page, Vec::new(), next)
        }

        // People lanes: finish the cursor's lane, then take whole lanes.
        RecentChangesOrderBy::Modifier => {
            match collect_lane_page(
                repo,
                LanePageRequest {
                    caller_id,
                    days_i32,
                    days,
                    limit,
                    person: params.person,
                    per_lane,
                },
                decoded.as_ref(),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    error!("recent-changes lanes: {e}");
                    return AppError::from(e).into_response();
                }
            }
        }
    };

    let items: Vec<FileChangeItemDto> = rows
        .into_iter()
        .map(|row| {
            let lane_total = lanes
                .iter()
                .find(|l| l.user_id == row.changed_by)
                .map(|l| l.lane_total);
            let changed_at = row.changed_at;
            let modifier_id = row.changed_by;
            FileChangeItemDto {
                resource: file_dto_from(row),
                changed_at,
                modifier_id,
                lane_total,
            }
        })
        .collect();

    let envelope = CursorListResponse::<FileChangeItemDto>::with_cursor(
        items,
        next_cursor.map(|c| c.encode()),
    );

    // Fresh signal: the newest change on the page, so a repeat fetch after
    // someone else's edit misses the ETag and refetches.
    let fresh_signal: u64 = envelope
        .items
        .iter()
        .map(|i| i.changed_at.timestamp().max(0) as u64)
        .max()
        .unwrap_or(0);
    let etag = envelope.weak_etag(params.cursor.as_deref(), limit as usize, fresh_signal);
    if if_none_match_matches(&headers, &etag) {
        return not_modified(&etag).into_response();
    }
    with_cache_headers(Json(envelope), &etag).into_response()
}

/// Everything one lane page is shaped by, after validation.
///
/// A struct rather than six positional parameters: `days` and `days_i32` are
/// the same number in two types, and `limit` / `per_lane` are both row counts
/// — adjacent integers that a call site can silently transpose.
struct LanePageRequest {
    caller_id: Uuid,
    days_i32: i32,
    days: u32,
    limit: i64,
    person: Option<Uuid>,
    per_lane: Option<i64>,
}

/// The cap as the cursor records it — `u32` on the wire, `i64` in SQL.
fn per_lane_echo(per_lane: Option<i64>) -> Option<u32> {
    per_lane.map(|c| c as u32)
}

/// Assemble one page of lane-major rows, plus the lanes it covers.
async fn collect_lane_page(
    repo: &crate::infrastructure::repositories::pg::RecentChangesPgRepository,
    req: LanePageRequest,
    cursor: Option<&RecentChangesCursor>,
) -> Result<
    (
        Vec<FileChangeRow>,
        Vec<ChangeLane>,
        Option<RecentChangesCursor>,
    ),
    crate::common::errors::DomainError,
> {
    let LanePageRequest {
        caller_id,
        days_i32,
        days,
        limit,
        person,
        per_lane,
    } = req;
    let only: Option<Vec<Uuid>> = person.map(|p| vec![p]);
    let mut rows: Vec<FileChangeRow> = Vec::new();
    let mut lanes: Vec<ChangeLane> = Vec::new();

    // ── CAPPED: whole lanes only ────────────────────────────────────────
    //
    // With a cap, a lane's size is known before any row is read —
    // `min(lane_total, per_lane)` — so the page can take lanes while they
    // fit and never split one. That is the whole point of the mode: a page
    // boundary is always a lane boundary, so the cursor is a lane keyset and
    // there is no "resume inside a lane" to express.
    //
    // Rounding `limit` up to a multiple of the cap would NOT achieve this:
    // lanes vary in size, so 50 rows is five cap-sized lanes only if every
    // lane happens to be full. Packing by actual lane size does.
    if let Some(cap) = per_lane {
        let after = cursor.and_then(|c| c.lane_ts.zip(c.lane_id));
        // `limit + 1` lanes is always enough to fill a page of `limit` rows,
        // since a lane holds at least one row.
        let batch = repo
            .lanes(caller_id, days_i32, only.as_deref(), after, limit + 1)
            .await?;

        let fetched = batch.len();
        let mut budget = 0i64;
        let mut taken: Vec<ChangeLane> = Vec::new();
        for lane in batch {
            let size = lane.lane_total.min(cap);
            // Always take the first lane, even when it alone exceeds the
            // limit: refusing would stall the walk forever on a lane bigger
            // than the page. The page then overshoots by less than `cap`,
            // which the API documents.
            if !taken.is_empty() && budget + size > limit {
                break;
            }
            budget += size;
            taken.push(lane);
        }

        // More to come if the budget stopped us short, or if the aggregate
        // filled every slot it was asked for — the `+ 1` is the probe that
        // tells "exactly enough" from "there is more".
        let more = taken.len() < fetched || fetched as i64 == limit + 1;

        if !taken.is_empty() {
            let ids: Vec<Uuid> = taken.iter().map(|l| l.user_id).collect();
            rows = repo
                .capped_rows_for_lanes(caller_id, days_i32, &ids, cap)
                .await?;
            lanes = taken;
        }
        let next = match (more, lanes.last()) {
            (true, Some(l)) => Some(RecentChangesCursor {
                order_by: RecentChangesOrderBy::Modifier,
                lane_ts: Some(l.lane_latest),
                lane_id: Some(l.user_id),
                // Carried but unread in this mode: the resume point is the
                // lane boundary above. Kept so the cursor stays one type.
                item_ts: l.lane_latest,
                resource_id: l.user_id,
                days,
                person,
                per_lane: per_lane_echo(per_lane),
            }),
            _ => None,
        };
        return Ok((rows, lanes, next));
    }

    // ── UNCAPPED: rows stream, a page may end inside a lane ─────────────
    //
    // 1. The lane the previous page ended inside, if any. Its totals come
    //    from a one-lane aggregate so the header can still say "Bob · 14".
    if let Some(c) = cursor
        && let Some(lane_id) = c.lane_id
    {
        let partial = repo
            .lanes(caller_id, days_i32, Some(&[lane_id]), None, 1)
            .await?;
        lanes.extend(partial);
        let more = repo
            .rows_for_lanes(
                caller_id,
                days_i32,
                &[lane_id],
                Some(c.item_ts),
                Some(c.resource_id),
                limit + 1,
            )
            .await?;
        rows.extend(more);
    }

    // 2. Whole lanes after the cursor's, taking as many as the page can hold.
    //    One lane holds at least one row, so `limit + 1` lanes always covers
    //    a page of `limit` rows — and the totals from step 1 let the loop
    //    stop early rather than over-fetching rows.
    if (rows.len() as i64) <= limit {
        let after = cursor.and_then(|c| c.lane_ts.zip(c.lane_id));
        let batch = repo
            .lanes(
                caller_id,
                days_i32,
                only.as_deref(),
                after,
                limit + 1 - rows.len() as i64,
            )
            .await?;
        if !batch.is_empty() {
            let ids: Vec<Uuid> = batch.iter().map(|l| l.user_id).collect();
            let more = repo
                .rows_for_lanes(
                    caller_id,
                    days_i32,
                    &ids,
                    None,
                    None,
                    limit + 1 - rows.len() as i64,
                )
                .await?;
            rows.extend(more);
            lanes.extend(batch);
        }
    }

    let lanes_for_cursor = lanes.clone();
    let (page, next) = split_page(
        rows,
        limit,
        RecentChangesOrderBy::Modifier,
        days,
        person,
        Some(&lanes_for_cursor),
    );
    Ok((page, lanes, next))
}

/// Truncate an over-fetched page and mint the cursor from the last kept row.
///
/// Over-fetching by one is how "is there a next page" is answered without a
/// count — the same trick `CursorListResponse::from_oversized` uses, done by
/// hand here because the cursor needs the row's lane as well as the row.
fn split_page(
    mut rows: Vec<FileChangeRow>,
    limit: i64,
    order_by: RecentChangesOrderBy,
    days: u32,
    person: Option<Uuid>,
    lanes: Option<&[ChangeLane]>,
) -> (Vec<FileChangeRow>, Option<RecentChangesCursor>) {
    if (rows.len() as i64) <= limit {
        return (rows, None);
    }
    rows.truncate(limit as usize);
    let last = rows.last().expect("limit >= 1");
    let (lane_ts, lane_id) = match lanes {
        Some(ls) => (
            ls.iter()
                .find(|l| l.user_id == last.changed_by)
                .map(|l| l.lane_latest),
            Some(last.changed_by),
        ),
        None => (None, None),
    };
    let cursor = RecentChangesCursor {
        order_by,
        lane_ts,
        lane_id,
        item_ts: last.changed_at,
        resource_id: last.file_id,
        days,
        person,
        // Always `None` here: this mints the cursor for the two UNCAPPED
        // paths. A capped page mints its own, on the lane boundary, because
        // it never ends mid-lane and has nothing to truncate.
        per_lane: None,
    };
    (rows, Some(cursor))
}

fn file_dto_from(row: FileChangeRow) -> FileDto {
    // `etag` and `category` are derived exactly as `/api/recent` derives
    // them, so the same file carries the same ETag on both listings and a
    // client's cache is not split by which endpoint it came from.
    let etag = File::etag_for_listing(row.blob_hash.as_deref(), row.content_modified_at);
    let category = category_for(&row.name, &row.mime_type);
    FileDto {
        id: row.file_id.to_string(),
        name: row.name,
        // **Empty on purpose**, matching `/api/grants/incoming/resources`,
        // whose query selects no path at all.
        //
        // This feed reaches files through a grant on the file or a cascading
        // grant on one folder — so the caller is authorised for that FILE, and
        // not for its ancestors. The path is a list of ancestor folder NAMES,
        // which is a different question: "/HR/Salaries 2026/offer.pdf" tells
        // the recipient of one shared file what the folders above it are
        // called. Nothing on this surface needs it, so it is not sent.
        //
        // "Open parent folder" still works, and deliberately: it navigates by
        // `folder_id`, and `/files/{id}` enforces its own read check — the
        // menu entry greys out when the caller cannot read the parent
        // (`folderAccessCached`). An id the caller cannot use leaks nothing;
        // a name does.
        path: String::new(),
        size: row.size.max(0) as u64,
        mime_type: intern_mime(&row.mime_type),
        folder_id: row.folder_id.map(|u| u.to_string()),
        created_at: row.created_at,
        modified_at: row.content_modified_at,
        category,
        content_hash: row.blob_hash.unwrap_or_default(),
        etag,
        created_by: row.created_by,
        updated_by: row.updated_by,
        is_favorite: row.is_favorite,
        is_shared: row.is_shared,
    }
}
