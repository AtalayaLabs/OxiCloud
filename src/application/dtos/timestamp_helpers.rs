//! Timestamp conversion between the internal `u64` epoch-seconds
//! representation used by domain entities (`File::modified_at`,
//! `Folder::modified_at`) and the wire-level `DateTime<Utc>` the DTOs
//! surface as RFC 3339 strings.
//!
//! Domain entities keep `u64` because `File::compute_etag` embeds the
//! value verbatim as a decimal in the ETag string (changing that would
//! invalidate every cached ETag on upgrade). Wire DTOs use
//! `DateTime<Utc>` so `#[utoipa::path]` emits `format: date-time` and
//! every language's generated client (TS, Python, Java, Go) picks up a
//! real timestamp type instead of a raw integer. This module is the
//! single-source conversion between those two conventions — reach for
//! these helpers rather than open-coding `.timestamp() as u64` or
//! `DateTime::from_timestamp(... as i64, 0).unwrap_or_else(...)` at
//! every mapper site.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};

/// Convert an internal epoch-seconds `u64` to a wire `DateTime<Utc>`.
/// Falls back to the Unix epoch (`1970-01-01T00:00:00Z`) on the
/// (unreachable in practice) overflow past year 2262.
pub fn epoch_to_datetime(secs: u64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(secs as i64, 0).unwrap_or_else(epoch_zero)
}

/// Convert a wire `DateTime<Utc>` back to the internal epoch-seconds
/// `u64`. Negative epochs clamp to 0 — a pre-1970 timestamp would wrap
/// into a huge positive on the unsigned cast otherwise.
pub fn datetime_to_epoch(dt: DateTime<Utc>) -> u64 {
    dt.timestamp().max(0) as u64
}

/// `1970-01-01T00:00:00Z` as a `DateTime<Utc>`. Used as the "unset" /
/// stub sentinel on DTO defaults, matching the pre-flip `created_at: 0`
/// / `modified_at: 0` behaviour of the stub builders.
pub fn epoch_zero() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).expect("epoch 0 is a valid DateTime<Utc>")
}

/// Convert a `DateTime<Utc>` to the integer-seconds value used as input to
/// [`crate::domain::entities::file::File::compute_etag`].
///
/// This MUST match PostgreSQL's `EXTRACT(EPOCH FROM x)::bigint` behaviour
/// (round half-up) rather than chrono's `.timestamp()` (floor), because the
/// File entity is loaded via the SQL cast in `file_blob_read_repository.rs`
/// and WebDAV / NC sync clients (NC desktop, Cyberduck, Finder) have been
/// caching ETags computed from the ROUNDED value since the project started.
/// Switching every ETag input to `.timestamp()` floor would invalidate every
/// sync client's local cache in one go — same value here keeps those caches
/// valid across the pivot.
///
/// The listing REST endpoints (`/api/folders/<id>/resources`,
/// `/api/favorites/resources`, `/api/recent/resources`,
/// `/api/trash/resources`, NC REPORT/search) previously used `.timestamp()
/// as u64` which floors, producing an ETag that drifted by one second from
/// the WebDAV path on any file whose sub-second was ≥ 0.5. Cyberduck
/// surfaced this: GET /webdav/ returned `...-1783489231` while the SPA
/// listing returned `...-1783489230` for the same file. This helper is the
/// single source of truth; both paths now agree.
pub fn datetime_to_etag_seconds(dt: DateTime<Utc>) -> u64 {
    let secs = dt.timestamp().max(0) as u64;
    // Postgres `::bigint` cast on numeric uses round-half-to-even
    // (banker's rounding). For nanosecond resolution the half-boundary
    // sits at exactly 500_000_000; everything else is unambiguous, so
    // round-half-up produces the same output as banker's rounding for
    // every value chrono can represent EXCEPT exactly `.5` seconds.
    // Our source values are microsecond-granular (Postgres TIMESTAMPTZ),
    // so a true `.5` boundary is vanishingly rare in practice.
    if dt.timestamp_subsec_nanos() >= 500_000_000 {
        secs.saturating_add(1)
    } else {
        secs
    }
}

#[cfg(test)]
mod etag_seconds_tests {
    //! Byte-level freeze of the ETag-input rounding rule so WebDAV + NC
    //! sync-client caches survive a refactor. Cross-check the Hurl guard
    //! in `webdav_etag_cross_check.hurl` (end-to-end) and the ETag
    //! byte-equality test on `file.rs::compute_etag` (formula shape).

    use super::*;
    use chrono::TimeZone;

    #[test]
    fn below_half_second_floors() {
        let dt = Utc.timestamp_opt(1_783_489_230, 499_999_999).unwrap();
        assert_eq!(datetime_to_etag_seconds(dt), 1_783_489_230);
    }

    #[test]
    fn at_exactly_half_second_rounds_up() {
        let dt = Utc.timestamp_opt(1_783_489_230, 500_000_000).unwrap();
        assert_eq!(datetime_to_etag_seconds(dt), 1_783_489_231);
    }

    #[test]
    fn above_half_second_rounds_up() {
        // The Cyberduck finding: 1783489230.883931 → 1783489231
        let dt = Utc.timestamp_opt(1_783_489_230, 883_931_000).unwrap();
        assert_eq!(datetime_to_etag_seconds(dt), 1_783_489_231);
    }

    #[test]
    fn whole_second_passes_through() {
        let dt = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        assert_eq!(datetime_to_etag_seconds(dt), 1_700_000_000);
    }

    #[test]
    fn negative_epoch_clamps_to_zero() {
        // Hypothetical pre-1970 timestamp; the unsigned cast on
        // `.timestamp()` without clamping would wrap into a huge positive.
        let dt = Utc.timestamp_opt(-5, 0).unwrap();
        assert_eq!(datetime_to_etag_seconds(dt), 0);
    }
}

// ══════════════════════════════════════════════════════════════════════
// Lenient inbound deserialization
// ══════════════════════════════════════════════════════════════════════
//
// Request DTOs (`CreateShareDto.expires_at`, `UpdateShareDto.expires_at`,
// `CreateGrantDto.expires_at`, admin-side drive-membership expiries) all
// used to accept an integer epoch second on the wire. The ISO-8601
// pivot made the Rust field a `DateTime<Utc>`, whose default serde
// deserializer rejects integers → every API client that still sent
// `{"expires_at": 1700000000}` would 422.
//
// The two helpers below are glued to those request fields via
// `#[serde(deserialize_with = …)]` so the server accepts BOTH shapes
// transparently:
//
//   - A JSON string → parsed as RFC 3339 (`"2026-01-01T00:00:00Z"`)
//   - A JSON integer → treated as Unix epoch seconds
//   - `null` or missing → `None`
//
/// Earliest instant a client may assert as a content mtime, as epoch
/// seconds: `2000-01-01T00:00:00Z`. Mirrored by the
/// `files_content_modified_at_floor` CHECK constraint.
pub const CLIENT_MTIME_FLOOR_SECS: i64 = 946_684_800;

/// How far ahead of server time a client-asserted mtime may sit. Clocks
/// drift and timezones get mishandled; an hour absorbs that without
/// admitting a value from next year.
pub const CLIENT_MTIME_FUTURE_SLACK_SECS: i64 = 3_600;

/// Validate a client-asserted content mtime (`X-OC-Mtime`, or the
/// multipart `source_modified_at` field) in epoch seconds.
///
/// Returns the value unchanged when it is plausible, or `Err` with a
/// stable reason key when it is not. **Callers must reject with 400 and
/// must never clamp** — a wrong client clock should be visible to whoever
/// can fix it, not silently absorbed into stored data. See
/// `docs/architecture/dates.md` § Trust bounds.
///
/// Bounds are deliberately loose: this is a sanity gate against garbage
/// and obvious forgery, not an attempt to verify a timestamp we have no
/// way of verifying. The value lands in `content_modified_at`, which is
/// presentational and ordering-relevant rather than security-relevant —
/// so the cost of a wrong-but-plausible value is a misordered timeline,
/// and the gate only has to stop the implausible.
pub fn validate_client_mtime(secs: i64, now_secs: i64) -> Result<i64, ClientMtimeRejection> {
    if secs < CLIENT_MTIME_FLOOR_SECS {
        return Err(ClientMtimeRejection::TooOld);
    }
    if secs > now_secs + CLIENT_MTIME_FUTURE_SLACK_SECS {
        return Err(ClientMtimeRejection::TooFarFuture);
    }
    Ok(secs)
}

/// Why a client-asserted mtime was refused. The `reason` strings are
/// stable keys for audit lines and log filters — do not reword them, and
/// add a new variant rather than repurposing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientMtimeRejection {
    /// Before `2000-01-01Z`. Catches zero, negative and uninitialised
    /// clocks, which would otherwise sort to the start of every timeline.
    TooOld,
    /// More than an hour ahead of server time. Catches a badly set client
    /// clock, which would otherwise pin the file to the top of every
    /// "newest first" listing permanently.
    TooFarFuture,
}

impl ClientMtimeRejection {
    /// Stable machine-readable key for the `reason` field of an audit line.
    pub fn reason(self) -> &'static str {
        match self {
            Self::TooOld => "mtime_before_2000",
            Self::TooFarFuture => "mtime_too_far_future",
        }
    }

    /// Operator- and user-facing explanation. Names the bound that was
    /// violated so the client author can fix it without reading our source.
    pub fn message(self) -> &'static str {
        match self {
            Self::TooOld => {
                "X-OC-Mtime is before 2000-01-01Z; send a plausible modification time or omit the header"
            }
            Self::TooFarFuture => {
                "X-OC-Mtime is more than one hour in the future; check the client clock or omit the header"
            }
        }
    }
}

// Output stays canonical: chrono's default `Serialize` for
// `DateTime<Utc>` always emits RFC 3339, so GET responses are
// unchanged. These helpers are inbound-only — a server-to-client
// response field never routes through them.

/// Lenient deserializer for `Option<DateTime<Utc>>` — accepts an RFC
/// 3339 string, an integer epoch second, or `null`/missing.
pub fn deserialize_flex_datetime_option<'de, D>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Flex {
        Str(String),
        Int(i64),
    }

    let opt: Option<Flex> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(Flex::Int(secs)) => DateTime::<Utc>::from_timestamp(secs, 0)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("epoch seconds out of range")),
        Some(Flex::Str(s)) => DateTime::parse_from_rfc3339(&s)
            .map(|dt| Some(dt.with_timezone(&Utc)))
            .map_err(|e| serde::de::Error::custom(format!("not a valid RFC 3339 timestamp: {e}"))),
    }
}

#[cfg(test)]
mod lenient_tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Row {
        #[serde(default, deserialize_with = "deserialize_flex_datetime_option")]
        at: Option<DateTime<Utc>>,
    }

    #[test]
    fn accepts_integer_epoch() {
        let r: Row = serde_json::from_str(r#"{"at": 1700000000}"#).unwrap();
        assert_eq!(r.at, Some(epoch_to_datetime(1_700_000_000)));
    }

    #[test]
    fn accepts_rfc3339_string() {
        let r: Row = serde_json::from_str(r#"{"at": "2023-11-14T22:13:20Z"}"#).unwrap();
        assert_eq!(r.at, Some(epoch_to_datetime(1_700_000_000)));
    }

    #[test]
    fn accepts_null() {
        let r: Row = serde_json::from_str(r#"{"at": null}"#).unwrap();
        assert_eq!(r.at, None);
    }

    #[test]
    fn accepts_missing() {
        let r: Row = serde_json::from_str("{}").unwrap();
        assert_eq!(r.at, None);
    }

    #[test]
    fn rejects_garbage_string() {
        let err = serde_json::from_str::<Row>(r#"{"at": "not-a-date"}"#).unwrap_err();
        assert!(err.to_string().contains("RFC 3339"));
    }
}

#[cfg(test)]
mod client_mtime_tests {
    use super::*;

    /// 2026-10-17T00:00:00Z — a fixed "now" so these never depend on the
    /// wall clock.
    const NOW: i64 = 1_792_195_200;

    #[test]
    fn accepts_a_plausible_past_mtime() {
        // A 2015 photo being uploaded today: the whole point of the header.
        assert_eq!(validate_client_mtime(1_420_070_400, NOW), Ok(1_420_070_400));
    }

    #[test]
    fn accepts_now_and_the_slack_window() {
        assert_eq!(validate_client_mtime(NOW, NOW), Ok(NOW));
        let edge = NOW + CLIENT_MTIME_FUTURE_SLACK_SECS;
        assert_eq!(validate_client_mtime(edge, NOW), Ok(edge));
    }

    #[test]
    fn rejects_just_past_the_future_slack() {
        let over = NOW + CLIENT_MTIME_FUTURE_SLACK_SECS + 1;
        assert_eq!(
            validate_client_mtime(over, NOW),
            Err(ClientMtimeRejection::TooFarFuture)
        );
    }

    #[test]
    fn rejects_zero_negative_and_pre_2000() {
        // Zero is what an uninitialised clock sends, and it would sort to
        // the start of every timeline.
        for bad in [0, -1, CLIENT_MTIME_FLOOR_SECS - 1] {
            assert_eq!(
                validate_client_mtime(bad, NOW),
                Err(ClientMtimeRejection::TooOld),
                "expected {bad} to be refused"
            );
        }
    }

    #[test]
    fn accepts_the_floor_itself() {
        assert_eq!(
            validate_client_mtime(CLIENT_MTIME_FLOOR_SECS, NOW),
            Ok(CLIENT_MTIME_FLOOR_SECS)
        );
    }

    #[test]
    fn floor_matches_the_check_constraint() {
        // `files_content_modified_at_floor` compares against '2000-01-01Z'.
        // If these drift apart, the DB rejects what the app accepted and the
        // client sees a 500 instead of a 400.
        let floor = DateTime::<Utc>::from_timestamp(CLIENT_MTIME_FLOOR_SECS, 0).unwrap();
        assert_eq!(floor.to_rfc3339(), "2000-01-01T00:00:00+00:00");

        // Pinning the literal is not enough — the COMPARISON has to match too.
        // This validator rejects `secs < FLOOR`, so the floor instant itself is
        // valid input, which is why the constraint is `>=` and not `>`. An
        // earlier draft used `>`: the one second at exactly 2000-01-01T00:00:00Z
        // passed validation and then violated the constraint, producing the 500
        // the comment above warns about.
        let now = CLIENT_MTIME_FLOOR_SECS + 86_400;
        assert_eq!(
            validate_client_mtime(CLIENT_MTIME_FLOOR_SECS, now),
            Ok(CLIENT_MTIME_FLOOR_SECS),
            "the floor instant itself must be accepted; the CHECK constraint is >= to match"
        );
        assert_eq!(
            validate_client_mtime(CLIENT_MTIME_FLOOR_SECS - 1, now),
            Err(ClientMtimeRejection::TooOld),
            "one second before the floor must be rejected"
        );
    }

    #[test]
    fn reason_keys_are_distinct_and_stable() {
        assert_eq!(ClientMtimeRejection::TooOld.reason(), "mtime_before_2000");
        assert_eq!(
            ClientMtimeRejection::TooFarFuture.reason(),
            "mtime_too_far_future"
        );
    }
}
