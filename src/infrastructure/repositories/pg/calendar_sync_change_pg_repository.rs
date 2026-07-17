//! CalDAV sync-collection change log — a `SyncChangeLogSchema` instance
//! over `caldav.calendar_sync_changes` (see `sync_change_log_pg_repository.rs`
//! for the shared implementation; populated by triggers defined in
//! `migrations/20260911000001_calendar_sync_changes.sql`).

use crate::infrastructure::repositories::pg::sync_change_log_pg_repository::{
    SyncChangeLogPgRepository, SyncChangeLogSchema,
};

pub struct CalendarSyncChangeSchema;

impl SyncChangeLogSchema for CalendarSyncChangeSchema {
    const TABLE: &'static str = "caldav.calendar_sync_changes";
    const WATERMARK_TABLE: &'static str = "caldav.calendar_sync_watermark";
    const COLLECTION_ID_COLUMN: &'static str = "collection_calendar_id";
    const LABEL_COLUMN: &'static str = "member_ical_uid";
    const LOG_NAME: &'static str = "calendar_sync_changes";
}

/// Type alias, not a newtype — `CalendarSyncChangePgRepository::new(pool)`
/// keeps compiling unchanged at every existing DI/call site.
pub type CalendarSyncChangePgRepository = SyncChangeLogPgRepository<CalendarSyncChangeSchema>;
