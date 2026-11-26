//! Request validation shared by every RFC 6578 `sync-collection` REPORT
//! handler (plain WebDAV, NextCloud, CalDAV, CardDAV), so the four apply the
//! same sync-level rules and token parsing.
//!
//! The `Depth` header is deliberately ignored. RFC 6578 §3.2 says only
//! Depth 0 is defined and other values are a 400, but Thunderbird sends
//! `Depth: 1` on both its CalDAV and CardDAV sync-collection REPORTs (left
//! over from an older draft of the spec, "some servers still depend on the
//! depth header, therefore we send both"), and sabre/dav (Nextcloud) and
//! Radicale ignore the header too. `<D:sync-level>` decides what is listed.

use uuid::Uuid;

use crate::domain::entities::sync_token::{SyncToken, SyncTokenError};
use crate::interfaces::errors::AppError;

/// Checks a sync-collection REPORT's `<D:sync-level>` (RFC 6578 §3.3).
///
/// - Missing: treated as `1`. The RFC says clients MUST send it, but
///   sabre/dav accepts a body without one and so does this server.
/// - `1`: the collection's immediate members.
/// - `infinite`: valid RFC but not implemented, so 403. The RFC defines no
///   request-level error for it, and 403 is how this server refuses other
///   unsupported traversals.
/// - Anything else: malformed, 400.
pub(crate) fn check_sync_level(sync_level: Option<&str>) -> Result<(), AppError> {
    match sync_level.map(str::trim) {
        None | Some("1") => Ok(()),
        Some("infinite") => Err(AppError::forbidden(
            "sync-collection sync-level infinite is not supported",
        )),
        Some(other) => Err(AppError::bad_request(format!(
            "sync-collection sync-level must be 1 or infinite, got {other:?}"
        ))),
    }
}

/// Parses a client sync-token and checks it belongs to `collection_id`.
///
/// A token that does not parse, or was issued for another collection, gets
/// the same uniform `DAV:valid-sync-token` 403 as an expired one
/// (`AppError::invalid_sync_token`), so the response never reveals which
/// check failed. The reason goes to the audit log.
pub(crate) fn parse_sync_token(
    raw: &str,
    collection_id: Uuid,
    caller_id: Uuid,
) -> Result<SyncToken, AppError> {
    SyncToken::parse_for_collection(raw, collection_id).map_err(|e| {
        let reason = match e {
            SyncTokenError::Malformed => "malformed",
            SyncTokenError::CollectionMismatch => "foreign_collection",
        };
        tracing::info!(
            target: "audit",
            event = "sync.token_rejected",
            reason,
            caller_id = %caller_id,
            collection_id = %collection_id,
            "👮🏻‍♂️ sync-token rejected: {reason}",
        );
        AppError::invalid_sync_token()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn status(level: Option<&str>) -> Option<StatusCode> {
        check_sync_level(level).err().map(|e| e.status_code)
    }

    /// A missing sync-level used to be a 400 under a strict reading of
    /// RFC 6578 §3.3. sabre/dav accepts it, so Nextcloud users' clients may
    /// rely on that. Pin the lenient default alongside the two values the
    /// RFC defines.
    #[test]
    fn sync_level_defaults_to_one_and_only_one_is_served() {
        assert_eq!(status(None), None);
        assert_eq!(status(Some("1")), None);
        assert_eq!(status(Some(" 1 ")), None);
        assert_eq!(status(Some("infinite")), Some(StatusCode::FORBIDDEN));
        assert_eq!(status(Some("2")), Some(StatusCode::BAD_REQUEST));
    }

    /// A malformed token and another collection's token used to get a 400
    /// whose message named the failed check ("malformed sync-token" vs a
    /// collection mismatch), letting a client tell them apart. Both must
    /// now produce the identical `valid-sync-token` 403.
    #[test]
    fn every_bad_token_gets_the_same_response() {
        let (coll, caller) = (Uuid::new_v4(), Uuid::new_v4());
        let foreign = SyncToken::mint(
            Uuid::new_v4(),
            crate::domain::entities::sync_token::SyncCursor {
                seq: 3,
                ..Default::default()
            },
        )
        .to_string();
        let malformed = parse_sync_token("garbage", coll, caller).unwrap_err();
        let mismatch = parse_sync_token(&foreign, coll, caller).unwrap_err();
        for err in [&malformed, &mismatch] {
            assert_eq!(err.status_code, StatusCode::FORBIDDEN);
            assert_eq!(
                err.error_type,
                crate::interfaces::errors::INVALID_SYNC_TOKEN
            );
        }
        assert_eq!(malformed.message, mismatch.message);
    }
}
