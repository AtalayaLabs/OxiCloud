//! Domain persistence port for the WebDAV `sync-collection` change log
//! (`storage.folder_sync_changes` / `storage.folder_sync_watermark`).
//!
//! Populated entirely by DB triggers (see
//! `migrations/20260911000000_folder_sync_changes.sql`) — this port is
//! read-only plus the retention sweep's cleanup call. No repository method
//! writes a change row; the application layer never needs to (and must
//! not) duplicate that bookkeeping.

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
}
