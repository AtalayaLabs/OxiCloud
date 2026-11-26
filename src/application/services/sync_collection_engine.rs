//! Shared engine behind every `sync-collection` service — WebDAV folders,
//! CalDAV calendars and CardDAV address books resolve their change log
//! into `SyncDelta<M>` with identical control flow: authz gate → expiry
//! check → changes_since → drop stale tombstones → one batch lookup of the
//! still-live members → tombstone whatever the lookup no longer finds.
//!
//! Authz here assumes a grant-only engine — see
//! `application::ports::change_log_port`'s module doc for the reasoning
//! and why deny rules would need this revisited.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use uuid::Uuid;

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::change_log_port::{SyncChange, SyncDelta};
use crate::common::errors::DomainError;
use crate::domain::entities::sync_token::{SyncCursor, SyncToken};
use crate::domain::repositories::sync_change_log_repository::{
    SyncChangeKind, SyncChangeLogRepository, SyncChangeRow,
};
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

/// Per-domain hook resolving the `Created`/`Updated` rows of one delta to
/// their current DTOs, in one batch.
///
/// Returns the members that still exist, keyed by `member_id`. A row whose
/// id is missing from the map is **gone** (hard-deleted, purged or trashed
/// after it was logged) and the engine emits a tombstone for it. Any lookup
/// failure must be `Err`: a read failure is never proof of absence, and a
/// wrongly emitted tombstone makes the client delete data that still exists.
///
/// One call per REPORT, never one per row: a delta can hold up to the
/// per-collection row cap (default 20k), and a per-row lookup would turn
/// that into as many database round trips.
pub trait SyncMemberResolver: Send + Sync + 'static {
    type Member;
    async fn resolve_batch(
        &self,
        collection_id: Uuid,
        rows: &[&SyncChangeRow],
    ) -> Result<HashMap<Uuid, Self::Member>, DomainError>;
}

pub struct SyncCollectionEngine<L, R, A = PgAclEngine>
where
    L: SyncChangeLogRepository,
    R: SyncMemberResolver,
    A: AuthorizationEngine,
{
    change_log: Arc<L>,
    resolver: R,
    authz: Arc<A>,
    /// `Resource::Folder` / `Resource::Calendar` / `Resource::AddressBook`
    /// are themselves valid `fn(Uuid) -> Resource` values (tuple-variant
    /// constructors) — no closure needed.
    resource_of: fn(Uuid) -> Resource,
    /// Turns a row's `label` into the `href_hint` a `Deleted` entry
    /// carries (the bare name for WebDAV, `"{label}.vcf"` for CardDAV).
    label_to_href_hint: fn(&str) -> String,
    /// `entity_type` passed to `DomainError::sync_token_expired`.
    error_tag: &'static str,
}

impl<L, R, A> SyncCollectionEngine<L, R, A>
where
    L: SyncChangeLogRepository,
    R: SyncMemberResolver,
    A: AuthorizationEngine,
{
    pub fn new(
        change_log: Arc<L>,
        resolver: R,
        authz: Arc<A>,
        resource_of: fn(Uuid) -> Resource,
        label_to_href_hint: fn(&str) -> String,
        error_tag: &'static str,
    ) -> Self {
        Self {
            change_log,
            resolver,
            authz,
            resource_of,
            label_to_href_hint,
            error_tag,
        }
    }

    async fn require_read(&self, collection_id: Uuid, caller_id: Uuid) -> Result<(), DomainError> {
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                (self.resource_of)(collection_id),
            )
            .await
    }

    /// Mints the token an **initial** sync response should hand back
    /// (empty/absent client `sync-token`). Cheaper than routing through
    /// `list_changes_with_perms`, which would also walk the collection's
    /// full change history only to discard it.
    pub async fn mint_initial_token(
        &self,
        collection_id: Uuid,
        caller_id: Uuid,
    ) -> Result<SyncToken, DomainError> {
        self.require_read(collection_id, caller_id).await?;
        let cursor = self.change_log.current_cursor(collection_id).await?;
        Ok(SyncToken::mint(collection_id, cursor))
    }

    /// Resolves the delta for `collection_id` since `since_token`.
    ///
    /// Returns `Err(ErrorKind::SyncTokenExpired)` when `since_token` can
    /// no longer be answered: it predates the retention watermark, or it
    /// is ahead of the change log (forged, or from before a database
    /// restore — answering it would silently skip every change until the
    /// log caught up; an `xmin` ahead of the database's is the same case). Either way the client must discard local state and
    /// restart with a fresh initial sync; the HTTP layer renders one
    /// uniform `DAV:valid-sync-token` 403 and the real reason goes to the
    /// audit log only.
    pub async fn list_changes_with_perms(
        &self,
        collection_id: Uuid,
        since_token: Option<SyncToken>,
        caller_id: Uuid,
    ) -> Result<SyncDelta<R::Member>, DomainError> {
        self.require_read(collection_id, caller_id).await?;

        if let Some(token) = &since_token {
            let cursor = token.cursor();
            let reason = if self
                .change_log
                .is_cursor_expired(collection_id, cursor)
                .await?
            {
                Some("expired")
            } else {
                let current = self.change_log.current_cursor(collection_id).await?;
                // A real snapshot's xmax never exceeds a later one's, so a
                // larger xmax was minted against another database (or before
                // a restore): its snapshot arm would skip every change since.
                (cursor.seq > current.seq || cursor.xmax > current.xmax).then_some("ahead_of_log")
            };
            if let Some(reason) = reason {
                return Err(self.reject_token(collection_id, cursor, caller_id, reason));
            }
        }

        let (rows, new_cursor) = self
            .change_log
            .changes_since(collection_id, since_token.as_ref().map(SyncToken::cursor))
            .await?;

        // The expiry check above and the delta read are separate
        // statements. A retention sweep committing between them deletes
        // rows this delta then never sees, while the new token's snapshot
        // already counts the sweep — so the next poll would accept it and
        // the purged changes would be skipped silently. Re-checking the
        // OLD cursor after the read closes that window: a sweep that
        // landed before the read is visible now, and one that lands after
        // it removed nothing this delta needed.
        if let Some(token) = &since_token
            && self
                .change_log
                .is_cursor_expired(collection_id, token.cursor())
                .await?
        {
            return Err(self.reject_token(
                collection_id,
                token.cursor(),
                caller_id,
                "purged_during_poll",
            ));
        }

        // A `label` (→ href) can appear on more than one row in this
        // batch: a delete followed by a fresh create of the same name
        // within one poll window mints a brand-new `member_id` (CalDAV /
        // CardDAV "update" is delete+recreate too), so it yields a
        // tombstone for the old id and a creation for the new one. The
        // response renders upserts and deletions as separate buckets,
        // which can't express "this happened before that", so a stale
        // tombstone sharing a href with a live row in the SAME batch is
        // dropped here — the href demonstrably still exists.
        let live: Vec<&SyncChangeRow> = rows
            .iter()
            .filter(|r| r.kind != SyncChangeKind::Deleted)
            .collect();
        let live_labels: HashSet<&str> = live.iter().map(|r| r.label.as_str()).collect();

        let mut resolved = if live.is_empty() {
            HashMap::new()
        } else {
            self.resolver.resolve_batch(collection_id, &live).await?
        };

        let mut changes = Vec::with_capacity(rows.len());
        let mut upserted = HashSet::new();
        for row in &rows {
            let member = match row.kind {
                SyncChangeKind::Deleted if live_labels.contains(row.label.as_str()) => continue,
                SyncChangeKind::Deleted => None,
                // One upsert per member: it renders under the member's
                // CURRENT href, whatever label the row carries. A second
                // live row for the same member must be skipped, never
                // tombstoned — its label may be the current href, and a
                // tombstone there deletes data that exists.
                SyncChangeKind::Created | SyncChangeKind::Updated
                    if !upserted.insert(row.member_id) =>
                {
                    continue;
                }
                SyncChangeKind::Created | SyncChangeKind::Updated => {
                    resolved.remove(&row.member_id)
                }
            };
            changes.push(match member {
                Some(member) => SyncChange::Upserted(member),
                None => SyncChange::Deleted {
                    member_id: row.member_id,
                    href_hint: (self.label_to_href_hint)(&row.label),
                    is_collection: row.is_collection,
                },
            });
        }

        Ok(SyncDelta {
            changes,
            new_token: SyncToken::mint(collection_id, new_cursor),
        })
    }

    /// Audit-logs a rejected sync-token and builds the uniform
    /// `SyncTokenExpired` error; `reason` goes to the audit line only.
    fn reject_token(
        &self,
        collection_id: Uuid,
        cursor: &SyncCursor,
        caller_id: Uuid,
        reason: &'static str,
    ) -> DomainError {
        tracing::info!(
            target: "audit",
            event = "sync.token_rejected",
            reason,
            caller_id = %caller_id,
            collection_id = %collection_id,
            token_seq = cursor.seq,
            token_xmax = cursor.xmax,
            collection_kind = self.error_tag,
            "sync-token rejected: {reason}",
        );
        DomainError::sync_token_expired(
            self.error_tag,
            format!("sync-token for collection {collection_id} rejected: {reason}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::authorization_ports::test_support::FakeEngine;
    use crate::domain::errors::ErrorKind;
    use chrono::{DateTime, Utc};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// FixedLog's head.
    const HEAD: SyncCursor = SyncCursor {
        seq: 9,
        xmin: 5,
        xmax: 7,
        xip: Vec::new(),
    };

    /// Change log that returns a fixed batch, never expired.
    struct FixedLog(Vec<SyncChangeRow>);

    impl SyncChangeLogRepository for FixedLog {
        async fn changes_since(
            &self,
            _: Uuid,
            _: Option<&SyncCursor>,
        ) -> Result<(Vec<SyncChangeRow>, SyncCursor), DomainError> {
            Ok((self.0.clone(), HEAD))
        }
        async fn is_cursor_expired(&self, _: Uuid, _: &SyncCursor) -> Result<bool, DomainError> {
            Ok(false)
        }
        async fn current_cursor(&self, _: Uuid) -> Result<SyncCursor, DomainError> {
            Ok(HEAD)
        }
        async fn delete_expired_before(&self, _: DateTime<Utc>) -> Result<u64, DomainError> {
            unimplemented!()
        }
        async fn enforce_row_cap(&self, _: u32) -> Result<u64, DomainError> {
            unimplemented!()
        }
    }

    /// What the resolver answers for every row.
    enum Answer {
        Live,
        Gone,
        Fails,
    }

    struct StubResolver {
        answer: Answer,
        calls: Arc<AtomicUsize>,
    }

    impl SyncMemberResolver for StubResolver {
        type Member = String;
        async fn resolve_batch(
            &self,
            _: Uuid,
            rows: &[&SyncChangeRow],
        ) -> Result<HashMap<Uuid, String>, DomainError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.answer {
                Answer::Live => Ok(rows
                    .iter()
                    .map(|r| (r.member_id, r.label.clone()))
                    .collect()),
                Answer::Gone => Ok(HashMap::new()),
                Answer::Fails => Err(DomainError::new(
                    ErrorKind::DatabaseError,
                    "Contact",
                    "connection reset",
                )),
            }
        }
    }

    fn row(label: &str, kind: SyncChangeKind) -> SyncChangeRow {
        SyncChangeRow {
            member_id: Uuid::new_v4(),
            label: label.to_owned(),
            kind,
            is_collection: false,
        }
    }

    fn engine(
        rows: Vec<SyncChangeRow>,
        answer: Answer,
        caller: Uuid,
    ) -> SyncCollectionEngine<FixedLog, StubResolver, FakeEngine> {
        engine_counting(rows, answer, caller, Arc::default())
    }

    fn engine_counting(
        rows: Vec<SyncChangeRow>,
        answer: Answer,
        caller: Uuid,
        calls: Arc<AtomicUsize>,
    ) -> SyncCollectionEngine<FixedLog, StubResolver, FakeEngine> {
        SyncCollectionEngine::new(
            Arc::new(FixedLog(rows)),
            StubResolver { answer, calls },
            Arc::new(FakeEngine::granting(vec![Subject::User(caller)])),
            Resource::AddressBook,
            |label| format!("{label}.vcf"),
            "TestSyncChange",
        )
    }

    fn token(collection: Uuid) -> Option<SyncToken> {
        Some(SyncToken::mint(collection, SyncCursor { seq: 1, ..HEAD }))
    }

    /// Regression: the resolvers used `.ok()`, so a failed DB read became
    /// `None` and the engine emitted a 404 tombstone. The client then
    /// deleted a member that still existed. A failed lookup must fail the
    /// REPORT, never turn into a deletion.
    #[tokio::test]
    async fn a_failed_lookup_is_an_error_not_a_tombstone() {
        let (caller, coll) = (Uuid::new_v4(), Uuid::new_v4());
        let e = engine(
            vec![row("a", SyncChangeKind::Updated)],
            Answer::Fails,
            caller,
        );
        let err = e
            .list_changes_with_perms(coll, token(coll), caller)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::DatabaseError);
    }

    /// A token numbered past the log's head (forged, or issued before a
    /// database restore) used to be answered with an empty delta, and the
    /// client then silently missed every change until the log caught up.
    /// It must be rejected like an expired token, so the client resyncs.
    #[tokio::test]
    async fn a_token_ahead_of_the_log_is_rejected() {
        let (caller, coll) = (Uuid::new_v4(), Uuid::new_v4());
        let calls = Arc::default();
        let e = engine_counting(
            vec![row("a", SyncChangeKind::Updated)],
            Answer::Live,
            caller,
            Arc::clone(&calls),
        );
        // A seq past the head, or a snapshot xmax past the database's (a
        // token from another database, or from before a restore: its
        // snapshot would skip every change since).
        for ahead in [
            SyncCursor {
                seq: HEAD.seq + 1,
                ..HEAD
            },
            SyncCursor {
                xmax: HEAD.xmax + 1,
                ..HEAD
            },
        ] {
            let err = e
                .list_changes_with_perms(coll, Some(SyncToken::mint(coll, ahead.clone())), caller)
                .await
                .unwrap_err();
            assert_eq!(err.kind, ErrorKind::SyncTokenExpired, "{ahead:?}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no delta is resolved");

        // The head itself is a valid token.
        assert!(
            e.list_changes_with_perms(coll, Some(SyncToken::mint(coll, HEAD)), caller)
                .await
                .is_ok()
        );
    }

    /// The legitimate degrade: the member was purged after its row was
    /// logged, so the client is told it is gone.
    #[tokio::test]
    async fn a_vanished_member_becomes_a_tombstone() {
        let (caller, coll) = (Uuid::new_v4(), Uuid::new_v4());
        let e = engine(
            vec![row("a", SyncChangeKind::Created)],
            Answer::Gone,
            caller,
        );
        let delta = e
            .list_changes_with_perms(coll, token(coll), caller)
            .await
            .unwrap();
        let (live, deleted) = delta.split_homogeneous("/b/");
        assert!(live.is_empty());
        assert_eq!(deleted, vec!["/b/a.vcf".to_owned()]);
    }

    /// Delete + recreate of one href in a single poll window: the stale
    /// tombstone must be dropped, or the client deletes the live copy.
    #[tokio::test]
    async fn a_stale_tombstone_for_a_live_href_is_dropped() {
        let (caller, coll) = (Uuid::new_v4(), Uuid::new_v4());
        let e = engine(
            vec![
                row("a", SyncChangeKind::Deleted),
                row("a", SyncChangeKind::Created),
            ],
            Answer::Live,
            caller,
        );
        let delta = e
            .list_changes_with_perms(coll, token(coll), caller)
            .await
            .unwrap();
        let (live, deleted) = delta.split_homogeneous("/b/");
        assert_eq!(live, vec!["a".to_owned()]);
        assert!(deleted.is_empty());
    }

    fn row_of(member_id: Uuid, label: &str, kind: SyncChangeKind) -> SyncChangeRow {
        SyncChangeRow {
            member_id,
            ..row(label, kind)
        }
    }

    /// Regression: a rename was one `updated` row under the new name, so
    /// the client got the new href and was never told the old one was gone
    /// — a ghost copy under the old name forever. A rename now arrives as
    /// a tombstone for the old label plus the member under its new one.
    #[tokio::test]
    async fn a_rename_tombstones_the_old_href_and_upserts_the_new() {
        let (caller, coll, m) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let e = engine(
            vec![
                row_of(m, "old", SyncChangeKind::Deleted),
                row_of(m, "new", SyncChangeKind::Created),
            ],
            Answer::Live,
            caller,
        );
        let delta = e
            .list_changes_with_perms(coll, token(coll), caller)
            .await
            .unwrap();
        let (live, deleted) = delta.split_homogeneous("/b/");
        assert_eq!(live, vec!["new".to_owned()]);
        assert_eq!(deleted, vec!["/b/old.vcf".to_owned()]);
    }

    /// The resolver hands back ONE member per id, so a second live row for
    /// the same member used to find nothing left in the map and became a
    /// tombstone — under a label that may be the member's current href, so
    /// the client deleted data that exists. It must be skipped instead.
    #[tokio::test]
    async fn a_second_live_row_of_one_member_is_never_a_tombstone() {
        let (caller, coll, m) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let e = engine(
            vec![
                row_of(m, "a", SyncChangeKind::Updated),
                row_of(m, "b", SyncChangeKind::Created),
            ],
            Answer::Live,
            caller,
        );
        let delta = e
            .list_changes_with_perms(coll, token(coll), caller)
            .await
            .unwrap();
        let (live, deleted) = delta.split_homogeneous("/b/");
        assert_eq!(live.len(), 1, "one upsert for the member");
        assert!(deleted.is_empty(), "a live member is never tombstoned");
    }

    /// Change log whose retention sweep commits while a poll is running:
    /// the expiry check before the delta read passes, every check after
    /// it sees the sweep, and the purged row is missing from the delta.
    struct PurgedMidPoll {
        expiry_checks: AtomicUsize,
    }

    impl SyncChangeLogRepository for PurgedMidPoll {
        async fn changes_since(
            &self,
            _: Uuid,
            _: Option<&SyncCursor>,
        ) -> Result<(Vec<SyncChangeRow>, SyncCursor), DomainError> {
            // The sweep deleted the only row this delta should have held.
            Ok((Vec::new(), HEAD))
        }
        async fn is_cursor_expired(&self, _: Uuid, _: &SyncCursor) -> Result<bool, DomainError> {
            Ok(self.expiry_checks.fetch_add(1, Ordering::SeqCst) > 0)
        }
        async fn current_cursor(&self, _: Uuid) -> Result<SyncCursor, DomainError> {
            Ok(HEAD)
        }
        async fn delete_expired_before(&self, _: DateTime<Utc>) -> Result<u64, DomainError> {
            unimplemented!()
        }
        async fn enforce_row_cap(&self, _: u32) -> Result<u64, DomainError> {
            unimplemented!()
        }
    }

    /// Regression: the token was checked for expiry once, BEFORE the delta
    /// read, in a separate statement. A retention sweep committing in
    /// between purged rows the delta then never returned, while the new
    /// token's snapshot already counted the sweep — so the next poll
    /// accepted it and the client silently missed those changes. The old
    /// token must be re-checked after the read and rejected, so the client
    /// resyncs instead of skipping.
    #[tokio::test]
    async fn a_purge_between_expiry_check_and_delta_read_rejects_the_token() {
        let (caller, coll) = (Uuid::new_v4(), Uuid::new_v4());
        let calls = Arc::new(AtomicUsize::new(0));
        let e = SyncCollectionEngine::new(
            Arc::new(PurgedMidPoll {
                expiry_checks: AtomicUsize::new(0),
            }),
            StubResolver {
                answer: Answer::Live,
                calls: calls.clone(),
            },
            Arc::new(FakeEngine::granting(vec![Subject::User(caller)])),
            Resource::AddressBook,
            |label| format!("{label}.vcf"),
            "TestSyncChange",
        );
        let err = e
            .list_changes_with_perms(coll, token(coll), caller)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::SyncTokenExpired);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no delta is resolved");
    }

    #[tokio::test]
    async fn a_caller_without_read_is_refused() {
        let (owner, stranger, coll) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let e = engine(vec![row("a", SyncChangeKind::Created)], Answer::Live, owner);
        assert!(
            e.list_changes_with_perms(coll, token(coll), stranger)
                .await
                .is_err()
        );
    }

    /// Regression: WebDAV and CardDAV looked members up one query per
    /// row, so a delta at the 20k row cap cost 20k round trips. The engine
    /// must hand every live row to the resolver in ONE call.
    #[tokio::test]
    async fn live_rows_are_resolved_in_one_batch() {
        let (caller, coll) = (Uuid::new_v4(), Uuid::new_v4());
        let calls = Arc::new(AtomicUsize::new(0));
        let rows = (0..50)
            .map(|i| row(&format!("m{i}"), SyncChangeKind::Updated))
            .collect();
        let e = engine_counting(rows, Answer::Live, caller, calls.clone());
        let delta = e
            .list_changes_with_perms(coll, token(coll), caller)
            .await
            .unwrap();
        assert_eq!(delta.changes.len(), 50);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A delta of pure deletions needs no lookup at all.
    #[tokio::test]
    async fn a_delete_only_delta_skips_the_resolver() {
        let (caller, coll) = (Uuid::new_v4(), Uuid::new_v4());
        let calls = Arc::new(AtomicUsize::new(0));
        let e = engine_counting(
            vec![row("gone", SyncChangeKind::Deleted)],
            Answer::Fails,
            caller,
            calls.clone(),
        );
        let delta = e
            .list_changes_with_perms(coll, token(coll), caller)
            .await
            .unwrap();
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// A deleted WebDAV subfolder's tombstone keeps its collection flag,
    /// so the handler renders its href with the trailing slash.
    #[tokio::test]
    async fn a_tombstone_keeps_the_collection_flag() {
        let (caller, coll) = (Uuid::new_v4(), Uuid::new_v4());
        let mut folder = row("docs", SyncChangeKind::Deleted);
        folder.is_collection = true;
        let e = engine(vec![folder], Answer::Live, caller);
        let delta = e
            .list_changes_with_perms(coll, token(coll), caller)
            .await
            .unwrap();
        assert!(matches!(
            delta.changes.as_slice(),
            [SyncChange::Deleted {
                is_collection: true,
                ..
            }]
        ));
    }
}
