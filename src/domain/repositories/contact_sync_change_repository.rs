//! Domain persistence port for the CardDAV `sync-collection` change log
//! (`carddav.contact_sync_changes` / `carddav.contact_sync_watermark`).
//!
//! Populated entirely by DB triggers (see
//! `migrations/20260911000002_contact_sync_changes.sql`). Mirrors
//! `domain/repositories/calendar_sync_change_repository.rs`, simplified for
//! a homogeneous (contacts-only) collection: no member-type distinction.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::common::errors::DomainError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncChangeKind {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone)]
pub struct ContactSyncChangeRow {
    pub member_id: Uuid,
    pub uid: String,
    pub kind: SyncChangeKind,
}

pub trait ContactSyncChangeRepository: Send + Sync + 'static {
    /// Every change recorded for `collection_address_book_id` with
    /// `seq > since_seq`, collapsed to at most one row per contact
    /// (latest `seq` wins). Returns the rows plus the collection's
    /// current max `seq` (0 if none), for minting the response's
    /// sync-token.
    async fn changes_since(
        &self,
        collection_address_book_id: Uuid,
        since_seq: Option<u64>,
    ) -> Result<(Vec<ContactSyncChangeRow>, u64), DomainError>;

    async fn is_seq_expired(&self, seq: u64) -> Result<bool, DomainError>;

    async fn current_seq(&self, collection_address_book_id: Uuid) -> Result<u64, DomainError>;

    /// Retention sweep — see `FolderSyncChangeRepository::delete_expired_before`.
    async fn delete_expired_before(&self, cutoff: DateTime<Utc>) -> Result<u64, DomainError>;
}
