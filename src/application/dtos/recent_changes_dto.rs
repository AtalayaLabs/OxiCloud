//! DTOs for `GET /api/files/recent-changes` — "what changed lately, and who
//! changed it" (`docs/plan/recently-modified.md` §4).
//!
//! Distinct from `/api/recent`, which answers "what did *I* open": that reads
//! `auth.user_recent_files`, a per-viewer access log, and is private to the
//! viewer. This reads `storage.file_content_touches` — shared content
//! provenance, naming other people.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::cursor::{CursorQuery, PageCursor};
use super::file_dto::FileDto;

/// Default window, in days.
pub const DEFAULT_DAYS: u32 = 30;
/// Hard ceiling on the window.
///
/// The lane ordering aggregates over everything inside the window, so the cost
/// of a page is proportional to the WINDOW, not to the page. This is the bound
/// that keeps that honest — and it is a 400, never a clamp, so a client asking
/// for a year is told rather than quietly served a month
/// (`docs/plan/recently-modified.md` §4).
pub const MAX_DAYS: u32 = 90;

/// Hard ceiling on the per-lane cap.
///
/// The cap exists to bound a lane, so letting it grow to the page limit would
/// defeat it. 50 is generous for "the N most recent files this person
/// touched" — past that the honest answer is the `?person=` drill-in, which
/// pages that one lane properly. A 400, never a clamp, for the `MAX_DAYS`
/// reason.
pub const MAX_PER_LANE: u32 = 50;

/// Sort axis for `GET /api/files/recent-changes`.
///
/// Typed rather than `Option<String>` so an unknown axis is refused at
/// wire-decode time instead of silently falling back to the default — same
/// discipline as [`super::recent_dto::RecentOrderBy`] and
/// [`super::trash_dto::TrashOrderBy`]. Per-endpoint on purpose: `modifier`
/// means nothing on a listing with no modifier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecentChangesOrderBy {
    /// Default — chronological, newest change first, lanes ignored. The cheap
    /// path: a plain keyset walk down `idx_touches_touched`.
    #[default]
    ChangedAt,
    /// Grouped by the person who made the change, each person's lane ordered
    /// by their own most recent change. The people view.
    Modifier,
}

/// Opaque keyset cursor.
///
/// The `modifier` axis needs a FOUR-tuple, because lanes are ordered by their
/// own recency: `(lane_ts, lane_id, item_ts, resource_id)`. `changed_at` uses
/// only the last two. The axis travels in the cursor so the keyset predicate
/// and the `ORDER BY` cannot drift — a request that flips `order_by`
/// mid-scroll is a 400, not a silently reshuffled page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentChangesCursor {
    #[serde(default)]
    pub order_by: RecentChangesOrderBy,
    /// The lane's own latest change. `None` on the `changed_at` axis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane_ts: Option<DateTime<Utc>>,
    /// The lane's owner. `None` on the `changed_at` axis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane_id: Option<Uuid>,
    /// This row's own touch timestamp.
    pub item_ts: DateTime<Utc>,
    /// Tie-breaker, and the reason two changes in the same millisecond cannot
    /// repeat or skip a row.
    pub resource_id: Uuid,
    /// The window the cursor was minted under. A page-2 request with a
    /// different `days` is a 400: the window changes which rows exist, so
    /// continuing across it would skip or repeat silently.
    #[serde(default = "default_days")]
    pub days: u32,
    /// The `person` filter the cursor was minted under, for the same reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub person: Option<Uuid>,
    /// The per-lane cap the cursor was minted under. Changing it mid-scroll
    /// changes how pages are CUT — capped pages break on lane boundaries,
    /// uncapped ones anywhere — so continuing across the change would skip or
    /// repeat. A 400, like `days`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_lane: Option<u32>,
}

fn default_days() -> u32 {
    DEFAULT_DAYS
}

impl PageCursor for RecentChangesCursor {}

/// Query parameters for `GET /api/files/recent-changes`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct RecentChangesQuery {
    /// Maximum items per page (1–200, default 50).
    #[serde(default = "CursorQuery::default_limit")]
    pub limit: u32,
    /// Opaque cursor from a previous response. Omit to start from the first
    /// page.
    pub cursor: Option<String>,
    /// How far back to look, in days. Default 30, maximum 90 — a larger value
    /// is a 400 naming the bound rather than a silent clamp.
    pub days: Option<u32>,
    /// Sort / group-by axis — see [`RecentChangesOrderBy`]. Omit for
    /// `changed_at`.
    pub order_by: Option<RecentChangesOrderBy>,
    /// Restrict to one person's changes — the lane drill-in. A person the
    /// caller shares nothing with returns an empty page rather than a 403:
    /// same anti-enumeration shape the photos `drive_id` filter uses.
    pub person: Option<Uuid>,
    /// Cap each lane at its N most recent files. Omit — or send `0` — for no
    /// cap. Maximum [`MAX_PER_LANE`]; a larger value is a 400 naming the
    /// bound, like `days`.
    ///
    /// Capping changes how the page is CUT, not just how much it holds.
    /// Uncapped, a page is `limit` rows and its boundary can fall anywhere,
    /// including inside a lane. Capped, a lane's size is known before any row
    /// is read (`min(lane_total, per_lane)`), so the page takes whole lanes
    /// while they fit and a lane is never split across pages. The cursor is
    /// then a lane boundary, never a row inside one.
    ///
    /// Consequence worth knowing: a single lane bigger than `limit` still
    /// comes back whole — one lane is always taken — so a capped page can
    /// return up to `limit + per_lane - 1` rows. Refusing `limit < per_lane`
    /// instead would make two independent parameters interdependent.
    ///
    /// Meaningless without lanes, so pairing it with `order_by=changed_at` is
    /// a 400 rather than a silent no-op.
    pub per_lane: Option<u32>,
}

/// One row: a file, and the one person-and-time this row is about.
///
/// A file legitimately appears MORE THAN ONCE in a page — once per lane that
/// touched it — so everything per-row travels here rather than in a
/// context map keyed by file id (`docs/plan/recently-modified.md` §4).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FileChangeItemDto {
    /// The file itself, shape-identical to every other file listing.
    pub resource: FileDto,
    /// **This row's** touch: when *this* person changed it. NOT the file's
    /// latest change by anyone — in Bob's lane the row says when Bob changed
    /// it, or every lane reads as the most recent editor.
    pub changed_at: DateTime<Utc>,
    /// Who made that change — an id, resolved by the client the same way it
    /// resolves `created_by` on every other listing (`useOwnerCache` →
    /// `/api/users/{id}`, cached across pages). Embedding a user object here
    /// would be an N+1 per page and the only listing in the API that does it.
    ///
    /// Never null: `storage.file_content_touches.user_id` is NOT NULL and
    /// cascades on user delete, so a lane's person always exists.
    pub modifier_id: Uuid,
    /// How many files this person changed inside the window, counted AFTER
    /// the access filter — so a lane header can read "Bob · 14 files" without
    /// a second round trip and without hinting at a file the caller cannot
    /// see. Present on the `modifier` axis only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane_total: Option<i64>,
}

/// Raw row from the rows query, before `FileDto` assembly.
#[derive(Debug, Clone)]
pub struct FileChangeRow {
    pub file_id: Uuid,
    pub name: String,
    pub folder_id: Option<Uuid>,
    pub size: i64,
    pub mime_type: String,
    pub created_at: DateTime<Utc>,
    pub content_modified_at: DateTime<Utc>,
    pub drive_id: Uuid,
    pub blob_hash: Option<String>,
    pub created_by: Option<Uuid>,
    pub updated_by: Option<Uuid>,
    pub is_favorite: bool,
    pub is_shared: bool,
    /// The touch — this row's reason for existing.
    pub changed_at: DateTime<Utc>,
    pub changed_by: Uuid,
}

/// One lane, from the lane aggregate (step 1 of the two-step query).
#[derive(Debug, Clone)]
pub struct ChangeLane {
    pub user_id: Uuid,
    pub lane_latest: DateTime<Utc>,
    /// Number of files this person changed in the window — exact, because
    /// `storage.file_content_touches` holds one row per (file, person).
    pub lane_total: i64,
}
