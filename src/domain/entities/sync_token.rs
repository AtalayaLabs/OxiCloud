//! RFC 6578 `sync-token` value type.
//!
//! Opaque to clients, but structured server-side as a collection id plus a
//! monotonic sequence number sourced from that collection's change-log
//! table (see the `*_sync_changes` migrations). Framework-free: no DB or
//! HTTP dependency, matching the rest of `domain/entities`.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::str::FromStr;

use uuid::Uuid;

const SYNC_TOKEN_PREFIX: &str = "http://oxicloud.local/ns/sync/";

/// A parsed, validated sync-token: which collection it was minted for, and
/// the change-log sequence number it represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SyncToken {
    collection_id: Uuid,
    seq: u64,
}

/// Why a client-supplied sync-token string was rejected before it ever
/// reached the change-log query — distinct from `SyncTokenExpired`
/// (domain/errors.rs), which is a valid-but-too-old token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncTokenError {
    /// Doesn't parse as `{prefix}{uuid}/{seq}` at all.
    Malformed,
    /// Parses fine, but was minted for a different collection than the
    /// one it's being presented against.
    CollectionMismatch,
}

impl Display for SyncTokenError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            SyncTokenError::Malformed => write!(f, "malformed sync-token"),
            SyncTokenError::CollectionMismatch => {
                write!(f, "sync-token was not issued for this collection")
            }
        }
    }
}

impl std::error::Error for SyncTokenError {}

impl SyncToken {
    /// Mints a token for `collection_id` at change-log sequence `seq`.
    pub fn mint(collection_id: Uuid, seq: u64) -> Self {
        Self { collection_id, seq }
    }

    pub fn collection_id(&self) -> Uuid {
        self.collection_id
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Parses `raw` and checks it was minted for `expected_collection_id`.
    /// This is the entry point handlers should use — a bare `FromStr`
    /// parse alone doesn't confirm the token belongs to the collection
    /// the request targets.
    pub fn parse_for_collection(
        raw: &str,
        expected_collection_id: Uuid,
    ) -> Result<Self, SyncTokenError> {
        let token: SyncToken = raw.parse()?;
        if token.collection_id != expected_collection_id {
            return Err(SyncTokenError::CollectionMismatch);
        }
        Ok(token)
    }
}

impl Display for SyncToken {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(
            f,
            "{}{}/{}",
            SYNC_TOKEN_PREFIX, self.collection_id, self.seq
        )
    }
}

impl FromStr for SyncToken {
    type Err = SyncTokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .strip_prefix(SYNC_TOKEN_PREFIX)
            .ok_or(SyncTokenError::Malformed)?;
        let mut parts = rest.rsplitn(2, '/');
        let seq_str = parts.next().ok_or(SyncTokenError::Malformed)?;
        let collection_str = parts.next().ok_or(SyncTokenError::Malformed)?;

        let collection_id =
            Uuid::parse_str(collection_str).map_err(|_| SyncTokenError::Malformed)?;
        let seq = seq_str
            .parse::<u64>()
            .map_err(|_| SyncTokenError::Malformed)?;

        Ok(Self { collection_id, seq })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_display_and_from_str() {
        let collection_id = Uuid::new_v4();
        let token = SyncToken::mint(collection_id, 42);
        let rendered = token.to_string();
        let parsed: SyncToken = rendered.parse().expect("should parse");
        assert_eq!(parsed, token);
        assert_eq!(parsed.collection_id(), collection_id);
        assert_eq!(parsed.seq(), 42);
    }

    #[test]
    fn parse_for_collection_accepts_matching_collection() {
        let collection_id = Uuid::new_v4();
        let token = SyncToken::mint(collection_id, 7);
        let raw = token.to_string();
        let parsed = SyncToken::parse_for_collection(&raw, collection_id).unwrap();
        assert_eq!(parsed.seq(), 7);
    }

    #[test]
    fn parse_for_collection_rejects_mismatched_collection() {
        let token = SyncToken::mint(Uuid::new_v4(), 7);
        let raw = token.to_string();
        let err = SyncToken::parse_for_collection(&raw, Uuid::new_v4()).unwrap_err();
        assert_eq!(err, SyncTokenError::CollectionMismatch);
    }

    #[test]
    fn rejects_missing_prefix() {
        let err = "not-a-token".parse::<SyncToken>().unwrap_err();
        assert_eq!(err, SyncTokenError::Malformed);
    }

    #[test]
    fn rejects_non_uuid_collection_segment() {
        let raw = format!("{}not-a-uuid/5", SYNC_TOKEN_PREFIX);
        let err = raw.parse::<SyncToken>().unwrap_err();
        assert_eq!(err, SyncTokenError::Malformed);
    }

    #[test]
    fn rejects_non_numeric_seq_segment() {
        let raw = format!("{}{}/not-a-number", SYNC_TOKEN_PREFIX, Uuid::new_v4());
        let err = raw.parse::<SyncToken>().unwrap_err();
        assert_eq!(err, SyncTokenError::Malformed);
    }

    #[test]
    fn rejects_missing_seq_segment() {
        let raw = format!("{}{}", SYNC_TOKEN_PREFIX, Uuid::new_v4());
        let err = raw.parse::<SyncToken>().unwrap_err();
        assert_eq!(err, SyncTokenError::Malformed);
    }
}
