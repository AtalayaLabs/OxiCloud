//! Domain persistence port shared by CalDAV (`caldav.calendar_sync_changes`)
//! and CardDAV (`carddav.contact_sync_changes`) RFC 6578 `sync-collection`
//! change logs — both are homogeneous, single-source-table logs (one
//! member kind, no member-type dimension), unlike WebDAV's
//! `FolderSyncChangeRepository` (heterogeneous folder+file membership
//! populated from two source tables, storage.files + storage.folders,
//! with move/trash branches) — kept as its own trait since it doesn't fit
//! this shape.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::common::errors::DomainError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncChangeKind {
    Created,
    Updated,
    Deleted,
}

/// One change-log row for a homogeneous sync-collection. `label` is the
/// resource's own identifying string — `ical_uid` for events, `uid` for
/// contacts — used purely for href construction at the call site
/// (`format!("{label}.ics")` / `format!("{label}.vcf")`); nothing in this
/// layer or the service layer needs it typed as anything more specific
/// than `String`.
#[derive(Debug, Clone)]
pub struct SyncChangeRow {
    pub member_id: Uuid,
    pub label: String,
    pub kind: SyncChangeKind,
}

pub trait SyncChangeLogRepository: Send + Sync + 'static {
    /// Every change recorded for `collection_id` with `seq > since_seq`
    /// (`None` = since the beginning), collapsed to one row per
    /// `member_id` (latest `seq` wins). Returns the rows plus the
    /// collection's current max `seq`, for minting the response's
    /// sync-token.
    async fn changes_since(
        &self,
        collection_id: Uuid,
        since_seq: Option<u64>,
    ) -> Result<(Vec<SyncChangeRow>, u64), DomainError>;

    /// Whether `seq` predates the retention watermark (RFC 6578 §3.6 →
    /// HTTP 507).
    async fn is_seq_expired(&self, seq: u64) -> Result<bool, DomainError>;

    /// The collection's current max `seq` (0 if none) — for minting an
    /// initial-sync token cheaply.
    async fn current_seq(&self, collection_id: Uuid) -> Result<u64, DomainError>;

    /// Retention sweep — see `FolderSyncChangeRepository::delete_expired_before`.
    async fn delete_expired_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError>;
}
