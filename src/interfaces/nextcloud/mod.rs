pub mod avatar_handler;
pub mod basic_auth_middleware;
pub mod login_v2_handler;
pub mod ocs_handler;
pub mod preview_handler;
pub mod report_handler;
pub mod routes;
pub mod session;
pub mod status_handler;
pub mod trashbin_handler;
pub mod uploads_handler;
pub mod webdav_handler;

use std::collections::HashMap;

use axum::http::HeaderMap;

use crate::application::dtos::timestamp_helpers::validate_client_mtime;
use crate::infrastructure::repositories::pg::file_metadata_repository::StoredMetadata;
use crate::interfaces::errors::AppError;

/// Epoch seconds for the `nc:creation_time` PROPFIND property, or `None`
/// when the content claims no creation date.
///
/// ## Why this reads metadata and not a file clock
///
/// `nc:creation_time` means "when did this content come into being" — a
/// property of the **bytes**, not of our row. Only the content itself can
/// answer it: EXIF `DateTimeOriginal` today, and once the extractor widens
/// (`storage.content_metadata.created_at`) also OOXML `dcterms:created`
/// and PDF `CreationDate`. That makes the value stable: different bytes are
/// a different content row with their own claim, so it never drifts under
/// an edit.
///
/// Neither file clock can stand in for it, and both candidates were
/// rejected for a concrete reason:
///
/// * `content_modified_at` (and therefore `media_sort_date`, which is
///   `COALESCE(captured_at, content_modified_at)`) **drifts**. Its triggers
///   recompute it on every content mutation, so for any file without a
///   metadata date it tracks the POSIX mtime forever. A creation date that
///   advances each time you save is the same lie this plan removed from
///   `nc:upload_time`, which used to read the content clock.
/// * `created_at` is our receipt time, which is exactly what
///   `nc:upload_time` already carries — using it here would make the two
///   properties duplicate each other, the bug §0 set out to fix.
///
/// ## `None` is a real answer
///
/// A `.txt`, a `.zip`, a JPEG stripped of EXIF genuinely has no creation
/// date to report, and the emitter writes `0` for that case — NextCloud's
/// own representation of an unknown `creation_time` and its column default.
/// Guessing a file clock instead would turn "unknown" into a confident
/// wrong answer.
///
/// Both absence forms collapse here: no `file_metadata` row at all, and a
/// row whose `captured_at` is NULL. A pre-epoch claim clamps to `0`, which
/// lands in the same "unknown" bucket rather than emitting a negative.
pub(crate) fn nc_creation_time(
    meta: &HashMap<String, StoredMetadata>,
    file_id: &str,
) -> Option<i64> {
    meta.get(file_id)?.captured_at.map(|t| t.timestamp().max(0))
}

/// Parse and validate the NextCloud `X-OC-Mtime` header — the client
/// asserting the source file's POSIX mtime in epoch **seconds**.
///
/// Shared by the two surfaces that accept it, `webdav_handler::handle_put`
/// and `uploads_handler::handle_assemble`, which had identical copies of
/// this block. Neither the REST upload endpoints nor the native `/webdav/`
/// surface accept the header at all.
///
/// Three outcomes, and the asymmetry between the last two is deliberate:
///
/// * **Absent** → `Ok(None)`; the write stamps `NOW()`.
/// * **Malformed** (non-integer, or not even UTF-8) → `Ok(None)` plus a
///   `warn`. Ignored rather than refused: this is a NextCloud-compatibility
///   surface, and rejecting a request over a header we can simply disregard
///   would break clients that otherwise work. The warn exists so the broken
///   client is still visible to an operator instead of disappearing silently.
/// * **Implausible but parseable** (before 2000-01-01Z, or more than an hour
///   ahead of server time) → `Err(400)`. Here the client told us something
///   specific and wrong, and clamping it would bake a bad timestamp into
///   stored data. See `docs/architecture/dates.md` § Trust bounds.
///
/// `event_prefix` namespaces the audit events per surface, e.g.
/// `"webdav_put"` yields `webdav_put.mtime_rejected` /
/// `webdav_put.mtime_ignored`.
pub(crate) fn parse_client_mtime(
    headers: &HeaderMap,
    event_prefix: &str,
) -> Result<Option<i64>, AppError> {
    let Some(raw) = headers.get("x-oc-mtime") else {
        return Ok(None);
    };

    // `to_str` fails on non-UTF-8 header bytes; that is malformed in the
    // same sense as a non-integer and takes the same branch.
    let parsed = raw.to_str().ok().and_then(|v| v.parse::<i64>().ok());

    let Some(secs) = parsed else {
        tracing::warn!(
            target: "audit",
            event = format!("{event_prefix}.mtime_ignored"),
            reason = "malformed",
            raw = %String::from_utf8_lossy(raw.as_bytes()),
            "⚠️ ignoring unparseable X-OC-Mtime; storing the server clock instead",
        );
        return Ok(None);
    };

    match validate_client_mtime(secs, chrono::Utc::now().timestamp()) {
        Ok(valid) => Ok(Some(valid)),
        Err(rejection) => {
            tracing::info!(
                target: "audit",
                event = format!("{event_prefix}.mtime_rejected"),
                reason = rejection.reason(),
                asserted_mtime = secs,
                "👮🏻‍♂️ rejected client-asserted X-OC-Mtime: {}",
                rejection.message(),
            );
            Err(AppError::bad_request(rejection.message()))
        }
    }
}
