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
