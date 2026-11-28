//! RFC 6578 `sync-token` value type.
//!
//! Opaque to clients, but structured server-side as a collection id plus a
//! [`SyncCursor`] into that collection's change-log table (see the
//! `*_sync_changes` migrations). Framework-free: no DB or HTTP dependency,
//! matching the rest of `domain/entities`.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::str::FromStr;

use uuid::Uuid;

const SYNC_TOKEN_PREFIX: &str = "http://oxicloud.local/ns/sync/";

/// A position in a change log, exact even when transactions commit out of
/// `seq` order.
///
/// `seq` alone is not enough: it is assigned at INSERT but visible only at
/// COMMIT, so a slow writer's row can commit below a `seq` already handed
/// out. The cursor therefore also carries the PostgreSQL snapshot it was
/// taken in (`xmin`, `xmax`, `xip`, the `pg_snapshot` text form). The rows a
/// cursor has NOT covered are exactly those written by a transaction that
/// snapshot could not see — `xid >= xmax`, or `xid` in `xip` — whatever
/// their `seq`: no gap, and no change reported twice (see
/// `20261126000004_sync_changes_commit_order.sql`).
///
/// `seq` is kept for what it is still good for: the retention watermark
/// and rejecting a token that is ahead of the log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncCursor {
    /// Highest `seq` visible when the cursor was taken.
    pub seq: u64,
    /// Every transaction id below this had finished when the cursor was
    /// taken.
    pub xmin: u64,
    /// No transaction id at or above this had started.
    pub xmax: u64,
    /// The transactions in `xmin..xmax` still in flight.
    pub xip: Vec<u64>,
}

impl SyncCursor {
    /// Whether a change written by top-level transaction `xid` was already
    /// visible when this cursor was taken.
    pub fn saw(&self, xid: u64) -> bool {
        xid < self.xmax && !self.xip.contains(&xid)
    }
}

/// A parsed, validated sync-token: which collection it was minted for, and
/// the change-log position it represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncToken {
    collection_id: Uuid,
    cursor: SyncCursor,
}

/// Why a client-supplied sync-token string was rejected before it ever
/// reached the change-log query — distinct from `SyncTokenExpired`
/// (domain/errors.rs), which is a valid-but-too-old token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncTokenError {
    /// Doesn't parse as `{prefix}{uuid}/{seq}-{xmin}:{xmax}:{xip,...}`, or
    /// the snapshot part is inconsistent.
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
    /// Mints a token for `collection_id` at change-log position `cursor`.
    pub fn mint(collection_id: Uuid, cursor: SyncCursor) -> Self {
        Self {
            collection_id,
            cursor,
        }
    }

    pub fn collection_id(&self) -> Uuid {
        self.collection_id
    }

    pub fn cursor(&self) -> &SyncCursor {
        &self.cursor
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
        let c = &self.cursor;
        write!(
            f,
            "{}{}/{}-{}:{}:",
            SYNC_TOKEN_PREFIX, self.collection_id, c.seq, c.xmin, c.xmax
        )?;
        for (i, xid) in c.xip.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{xid}")?;
        }
        Ok(())
    }
}

impl FromStr for SyncToken {
    type Err = SyncTokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .strip_prefix(SYNC_TOKEN_PREFIX)
            .ok_or(SyncTokenError::Malformed)?;
        let (collection_str, cursor_str) =
            rest.rsplit_once('/').ok_or(SyncTokenError::Malformed)?;
        // A token without a snapshot (`.../{seq}`, or the interim
        // `.../{seq}-{xmin}`) is Malformed on purpose: it cannot say which
        // in-flight changes it missed, so its client must resync.
        let (seq_str, snapshot) = cursor_str
            .split_once('-')
            .ok_or(SyncTokenError::Malformed)?;
        let mut parts = snapshot.splitn(3, ':');
        let (Some(xmin_str), Some(xmax_str), Some(xip_str)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(SyncTokenError::Malformed);
        };

        let collection_id =
            Uuid::parse_str(collection_str).map_err(|_| SyncTokenError::Malformed)?;
        let parse = |s: &str| s.parse::<u64>().map_err(|_| SyncTokenError::Malformed);
        let cursor = SyncCursor {
            seq: parse(seq_str)?,
            xmin: parse(xmin_str)?,
            xmax: parse(xmax_str)?,
            xip: if xip_str.is_empty() {
                Vec::new()
            } else {
                xip_str.split(',').map(parse).collect::<Result<_, _>>()?
            },
        };
        // The invariants PostgreSQL guarantees for a real snapshot; a token
        // breaking them was not minted here.
        if cursor.xmin > cursor.xmax
            || cursor
                .xip
                .iter()
                .any(|x| *x < cursor.xmin || *x >= cursor.xmax)
        {
            return Err(SyncTokenError::Malformed);
        }

        Ok(Self {
            collection_id,
            cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(seq: u64) -> SyncCursor {
        SyncCursor {
            seq,
            xmin: 100,
            xmax: 105,
            xip: vec![100, 103],
        }
    }

    #[test]
    fn round_trips_through_display_and_from_str() {
        let collection_id = Uuid::new_v4();
        let cursor = cursor(42);
        let token = SyncToken::mint(collection_id, cursor.clone());
        let rendered = token.to_string();
        let parsed: SyncToken = rendered.parse().expect("should parse");
        assert_eq!(parsed, token);
        assert_eq!(parsed.collection_id(), collection_id);
        assert_eq!(parsed.cursor(), &cursor);
    }

    #[test]
    fn parse_for_collection_accepts_matching_collection() {
        let collection_id = Uuid::new_v4();
        let cursor = cursor(7);
        let token = SyncToken::mint(collection_id, cursor.clone());
        let raw = token.to_string();
        let parsed = SyncToken::parse_for_collection(&raw, collection_id).unwrap();
        assert_eq!(parsed.cursor(), &cursor);
    }

    #[test]
    fn parse_for_collection_rejects_mismatched_collection() {
        let token = SyncToken::mint(Uuid::new_v4(), cursor(7));
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
        let raw = format!("{}{}/x-1:2:", SYNC_TOKEN_PREFIX, Uuid::new_v4());
        let err = raw.parse::<SyncToken>().unwrap_err();
        assert_eq!(err, SyncTokenError::Malformed);
    }

    #[test]
    fn rejects_missing_seq_segment() {
        let raw = format!("{}{}", SYNC_TOKEN_PREFIX, Uuid::new_v4());
        let err = raw.parse::<SyncToken>().unwrap_err();
        assert_eq!(err, SyncTokenError::Malformed);
    }

    /// A token without the full snapshot can't say which in-flight
    /// changes it missed; answering it would reopen the commit-order gap,
    /// so it must be refused (the client resyncs).
    #[test]
    fn rejects_a_token_without_a_snapshot() {
        for cursor in ["5", "5-7"] {
            let raw = format!("{}{}/{cursor}", SYNC_TOKEN_PREFIX, Uuid::new_v4());
            let err = raw.parse::<SyncToken>().unwrap_err();
            assert_eq!(err, SyncTokenError::Malformed, "{cursor}");
        }
    }

    #[test]
    fn an_empty_in_flight_list_round_trips() {
        let c = SyncCursor {
            seq: 1,
            xmin: 9,
            xmax: 9,
            xip: Vec::new(),
        };
        let token = SyncToken::mint(Uuid::new_v4(), c.clone());
        let parsed: SyncToken = token.to_string().parse().expect("parse");
        assert_eq!(parsed.cursor(), &c);
    }

    #[test]
    fn rejects_an_inconsistent_snapshot() {
        for snap in ["9:5:", "5:9:4", "5:9:9", "5:9:x", "5:x:"] {
            let raw = format!("{}{}/1-{snap}", SYNC_TOKEN_PREFIX, Uuid::new_v4());
            let err = raw.parse::<SyncToken>().unwrap_err();
            assert_eq!(err, SyncTokenError::Malformed, "{snap}");
        }
    }

    #[test]
    fn saw_is_exactly_snapshot_visibility() {
        let c = cursor(1);
        assert!(c.saw(99), "finished before xmin");
        assert!(!c.saw(100), "in flight");
        assert!(c.saw(101), "finished between xmin and xmax");
        assert!(!c.saw(103), "in flight");
        assert!(!c.saw(105), "not started yet");
    }
}
