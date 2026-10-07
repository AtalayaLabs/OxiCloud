//! DTOs for `GET /api/photos/resources` — the normalized photos listing.
//!
//! Shape family mirrors the other `/resources` endpoints (favorites,
//! recents, trash, folder contents): `CursorListResponse<T>` envelope,
//! opaque base64url cursor, item payload reusing `ResourceContentDto`.
//!
//! Photo-specific signals live at the item level (siblings to
//! `resource`), not inside `FileDto`, so `FileDto` can stay generic
//! across every file-listing shape — only the photos surface carries
//! `width`/`height`/`captured_at`/`orientation`/`has_gps`. See §1 of
//! `docs/plan/photos-resources-migration.md`.
//!
//! The cursor is opaque to callers but is a plain base64url'd JSON
//! object under the hood (`PageCursor` default impl). Its fields —
//! `order_by`, `sort_value`, `file_id` — are enough to resume the
//! keyset at the next row AFTER the last-returned item, including a
//! deterministic tie-break on `file_id` so pages at the same
//! second-granularity `sort_value` don't skip or re-emit rows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::dtos::cursor::{CursorListResponse, PageCursor};
use crate::application::dtos::grant_dto::{ResourceContentDto, ResourceTypeDto};

/// Sort axis for `GET /api/photos/resources`.
///
/// `CapturedAt` — the current default — orders by
/// `COALESCE(captured_at, created_at)` via the materialised
/// `storage.files.media_sort_date` column. The `?order_by=created_at`
/// override (§3) will land once a stakeholder actually asks for the
/// upload-time view; the cursor already encodes the axis so §3 is a
/// purely additive wiring change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhotoOrderBy {
    /// Default — EXIF capture date, falling back to `created_at` for
    /// files that carry no EXIF.
    #[default]
    CapturedAt,
    /// File-row `created_at` only. Reserved for §3; the handler does
    /// not accept this value today.
    CreatedAt,
}

/// Media-kind filter for `GET /api/photos/resources` (§6).
///
/// Default `All` preserves the pre-filter behaviour — both image and
/// video rows interleaved — so a client that never sends `?kind=` is
/// unaffected. `Photo` and `Video` narrow to one mime family at the
/// SQL layer; a client that wants a dedicated "Videos" tab stops
/// pulling and discarding video rows on the way to the next
/// `?limit=` worth of photos.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhotoKind {
    /// Default — both `image/*` and `video/*` rows. Matches the
    /// partial covering index exactly.
    #[default]
    All,
    /// `mime_type LIKE 'image/%'` only.
    Photo,
    /// `mime_type LIKE 'video/%'` only.
    Video,
}

/// Filter axes passed from the handler to the repository. Both
/// knobs opt into opt-in behaviour: `kind = All` + `drive_id = None`
/// matches today's cross-drive feed verbatim.
///
/// `drive_id` carries the caller's requested restriction when
/// present. The repo pre-filters the `accessible` CTE down to the
/// matching drive, which naturally yields the empty page for a
/// drive the caller cannot see — same anti-enum shape as every
/// other OxiCloud listing on an invisible resource (no 403/404
/// disclosure difference between "drive exists but you can't see
/// it" and "drive doesn't exist").
#[derive(Debug, Clone, Copy, Default)]
pub struct PhotosFilter {
    pub kind: PhotoKind,
    pub drive_id: Option<Uuid>,
}

/// Opaque keyset cursor for `GET /api/photos/resources`.
///
/// Serialised as URL-safe base64url (`PageCursor` default impl). The
/// three fields together form a `(sort_value, file_id)` keyset
/// position:
///
/// - `order_by` pins the axis the cursor was issued against; a cursor
///   taken with `order_by=captured_at` MUST NOT be used against a
///   request with `order_by=created_at` (§3 enforcement — the handler
///   returns 400 on mismatch so pagination can't silently drift).
/// - `sort_value` is the full-precision `timestamptz` of the chosen
///   sort column — microsecond precision, exactly matching
///   `storage.files.media_sort_date` so the WHERE predicate can
///   compare at column fidelity. Carrying only the item-level
///   `sort_date: i64` (epoch seconds) here loses sub-second
///   precision and silently drops rows that landed inside the same
///   wall-clock second as the page boundary.
/// - `file_id` is the tie-breaker on equal `sort_value` so rows that
///   do share an exact `media_sort_date` can still be paginated
///   without skipping or re-emitting.
///
/// The wire-level `sort_date` on `PhotoResourceItemDto` stays epoch
/// seconds — that's the client's view; the cursor's internal shape
/// is a server-only concern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotosCursor {
    pub order_by: PhotoOrderBy,
    /// Media-kind filter the page was issued under (§6). Encoded so
    /// a cursor from `?kind=photo` can't be reused against
    /// `?kind=video` — mismatch → 400. `#[serde(default)]` lets
    /// old-shape cursors from §1 (before this field existed) still
    /// decode as `PhotoKind::All` — correct for the cross-drive +
    /// all-media default the handler accepts without `?kind=`.
    #[serde(default)]
    pub kind: PhotoKind,
    /// Drive-scope filter the page was issued under (§6b). Encoded
    /// for the same round-trip-stability reason; a cursor from
    /// `?drive_id=A` can't be reused against `?drive_id=B` or the
    /// cross-drive view (`drive_id = None`). `#[serde(default)]`
    /// keeps §1 cursors readable as `None` — correct, since those
    /// pages were already cross-drive.
    #[serde(default)]
    pub drive_id: Option<Uuid>,
    /// Full-precision timestamp of the last-returned item's sort column.
    pub sort_value: DateTime<Utc>,
    /// Tie-breaker — the last-returned item's `file_id`.
    pub file_id: Uuid,
}

impl PageCursor for PhotosCursor {}

/// One item in a `GET /api/photos/resources` page.
///
/// `resource_type` is always `ResourceTypeDto::File` — the listing is
/// media-files-only — but the field stays for shape parity with other
/// `/resources` endpoints. Clients can pattern-match on it the same
/// way they do on favorites or trash items.
#[derive(Debug, Serialize, ToSchema)]
pub struct PhotoResourceItemDto {
    pub resource_type: ResourceTypeDto,
    /// Full file details — serialised as the inner object via
    /// `ResourceContentDto`'s `#[serde(untagged)]`, so consumers read
    /// `resource.id`, `resource.name`, … directly.
    pub resource: ResourceContentDto,

    /// Image/video width in pixels, from `storage.file_metadata`.
    /// `None` when the row has no metadata (very old uploads, or a
    /// format the metadata extractor doesn't understand).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    /// Image/video height in pixels, from `storage.file_metadata`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,

    /// Sort axis value, epoch seconds. Equals
    /// `COALESCE(captured_at, created_at)` when `order_by=captured_at`
    /// (the default); equals `created_at` when §3 lands and the
    /// caller selects `order_by=created_at`. The cursor keys off this
    /// value.
    pub sort_date: i64,

    /// Raw EXIF `DateTimeOriginal`, epoch seconds. `None` when the
    /// file carries no EXIF date. Lets the client distinguish "taken
    /// on this date" from the upload-date fallback that `sort_date`
    /// collapses into one value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<i64>,

    /// Raw EXIF orientation (TIFF 1–8 enum). `None` when absent. The
    /// client applies this to the full-resolution image tile BEFORE
    /// pixels arrive, so there is no "appears sideways, flips" flash
    /// on `<img>` load in browsers that don't auto-rotate. Note:
    /// thumbnails already arrive display-correct — the server-side
    /// thumbnail generator bakes rotation into the output pixels.
    /// This field is strictly for the full-res lightbox / download
    /// viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation: Option<i16>,

    /// `true` when the file has both GPS lat AND lng. Presence signal
    /// only — the raw coordinates stay off the main listing. Enables
    /// a "📍 has location" badge and a "photos with location"
    /// client-side filter without disclosing per-photo coordinates on
    /// every gallery tile. Important on public share links where a
    /// visitor seeing GPS for every photo would often be unintended
    /// by the sharer. For the Places map, continue to use
    /// `/api/photos/geo` which clusters coords server-side; for a
    /// specific photo's exact lat/lng, call `/api/files/{id}/metadata`
    /// — the explicit per-photo call is the right disclosure
    /// boundary.
    pub has_gps: bool,
}

/// Response envelope for `GET /api/photos/resources`.
pub type PhotosResourcesDto = CursorListResponse<PhotoResourceItemDto>;
