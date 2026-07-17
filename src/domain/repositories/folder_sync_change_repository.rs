//! Domain persistence port for the WebDAV `sync-collection` change log
//! (`storage.folder_sync_changes` / `storage.folder_sync_watermark`).
//!
//! Populated entirely by DB triggers (see
//! `migrations/20260911000000_folder_sync_changes.sql`) — this port is
//! read-only plus the retention sweep's cleanup call. No repository method
//! writes a change row; the application layer never needs to (and must
//! not) duplicate that bookkeeping.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::common::errors::DomainError;

/// What kind of DAV resource a change-log row's member is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMemberType {
    File,
    Folder,
}

/// What happened to the member, as recorded at trigger time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncChangeKind {
    Created,
    Updated,
    Deleted,
}

/// One raw change-log row, before the application layer resolves
/// `Created`/`Updated` members to their current DTO (a `Deleted` row
/// carries everything the response needs — `href_name` is the last-known
/// leaf name, captured at tombstone time).
#[derive(Debug, Clone)]
pub struct FolderSyncChangeRow {
    pub seq: u64,
    pub member_type: SyncMemberType,
    pub member_id: Uuid,
    pub href_name: String,
    pub kind: SyncChangeKind,
}

pub trait FolderSyncChangeRepository: Send + Sync + 'static {
    /// Every change recorded for `collection_folder_id` with
    /// `seq > since_seq` (pass `None` to mean "since the beginning" —
    /// callers only do this to get a token for a later poll, since an
    /// initial sync response is a full listing, not a delta), collapsed
    /// to at most one row per `member_id` (the latest `seq` wins — this
    /// nets churn like trash-then-restore within one poll window down to
    /// the correct single outcome instead of contradictory duplicates).
    ///
    /// Returns the rows plus the current max `seq` for the collection
    /// (0 if the collection has no change-log activity at all) — the
    /// caller mints the response's `sync-token` from that value, bound to
    /// the same snapshot the delta was read from.
    async fn changes_since(
        &self,
        collection_folder_id: Uuid,
        since_seq: Option<u64>,
    ) -> Result<(Vec<FolderSyncChangeRow>, u64), DomainError>;

    /// Whether `seq` predates the retention watermark — i.e. rows that
    /// would have answered a `changes_since(collection_folder_id, Some(seq))`
    /// call have already been purged, so the client must be told to
    /// discard local state and restart with a fresh initial sync
    /// (RFC 6578 §3.6, HTTP 507).
    async fn is_seq_expired(&self, seq: u64) -> Result<bool, DomainError>;

    /// The collection's current max `seq` (0 if it has no change-log
    /// activity), for minting the token an **initial** sync response
    /// hands back — cheaper than `changes_since` with `since_seq: None`,
    /// which would also walk (and discard) the collection's full change
    /// history just to read the same number.
    async fn current_seq(&self, collection_folder_id: Uuid) -> Result<u64, DomainError>;

    /// Retention sweep: deletes every row with `changed_at < cutoff`, then
    /// advances `folder_sync_watermark.low_water_seq` to the highest `seq`
    /// deleted (never decreases it) — in one transaction, so a crash
    /// mid-sweep cannot advance the watermark past rows that are still
    /// actually present (which would wrongly reject a still-valid token
    /// as expired). Returns the number of rows deleted.
    async fn delete_expired_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError>;
}
