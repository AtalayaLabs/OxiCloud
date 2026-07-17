//! CardDAV sync-collection change log — a `SyncChangeLogSchema` instance
//! over `carddav.contact_sync_changes` (see `sync_change_log_pg_repository.rs`
//! for the shared implementation; populated by triggers defined in
//! `migrations/20260911000002_contact_sync_changes.sql`).

use crate::infrastructure::repositories::pg::sync_change_log_pg_repository::{
    SyncChangeLogPgRepository, SyncChangeLogSchema,
};

pub struct ContactSyncChangeSchema;

impl SyncChangeLogSchema for ContactSyncChangeSchema {
    const TABLE: &'static str = "carddav.contact_sync_changes";
    const WATERMARK_TABLE: &'static str = "carddav.contact_sync_watermark";
    const COLLECTION_ID_COLUMN: &'static str = "collection_address_book_id";
    const LABEL_COLUMN: &'static str = "member_uid";
    const LOG_NAME: &'static str = "contact_sync_changes";
}

/// Type alias, not a newtype — `ContactSyncChangePgRepository::new(pool)`
/// keeps compiling unchanged at every existing DI/call site.
pub type ContactSyncChangePgRepository = SyncChangeLogPgRepository<ContactSyncChangeSchema>;
