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
