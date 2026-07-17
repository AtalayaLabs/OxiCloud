//! Application-layer orchestration for CalDAV `sync-collection` REPORT —
//! wraps `SyncCollectionEngine` over `CalendarSyncChangePgRepository`.
//!
//! Kept as its own dedicated service (mirroring `CarddavSyncCollectionService`
//! and `WebdavSyncCollectionService`) rather than the inline methods
//! `CalendarService` used to carry — moved out so all three protocols
//! share the same "dedicated sync-collection service" shape, with CalDAV
//! and CardDAV additionally sharing one generic engine underneath.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::dtos::calendar_dto::CalendarEventDto;
use crate::application::ports::calendar_ports::CalendarStoragePort;
use crate::application::ports::change_log_port::SyncDelta;
use crate::application::services::sync_collection_engine::{
    SyncCollectionEngine, SyncMemberResolver,
};
use crate::common::errors::DomainError;
use crate::domain::entities::sync_token::SyncToken;
use crate::domain::services::authorization::Resource;
use crate::infrastructure::adapters::calendar_storage_adapter::CalendarStorageAdapter;
use crate::infrastructure::repositories::pg::calendar_sync_change_pg_repository::CalendarSyncChangePgRepository;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

struct CalendarEventResolver {
    calendar_storage: Arc<CalendarStorageAdapter>,
}

impl SyncMemberResolver for CalendarEventResolver {
    type Member = CalendarEventDto;

    async fn resolve(&self, member_id: Uuid) -> Option<CalendarEventDto> {
        // Hard-deleted/purged after this row was logged but before we
        // resolved it — degrade to `None` (→ `Deleted`) rather than
        // surfacing the lookup error.
        self.calendar_storage
            .get_event(&member_id.to_string())
            .await
            .ok()
    }
}

pub struct CaldavSyncCollectionService {
    engine: SyncCollectionEngine<CalendarSyncChangePgRepository, CalendarEventResolver>,
}

impl CaldavSyncCollectionService {
    pub fn new(
        change_log: Arc<CalendarSyncChangePgRepository>,
        calendar_storage: Arc<CalendarStorageAdapter>,
        authz: Arc<PgAclEngine>,
    ) -> Self {
        Self {
            engine: SyncCollectionEngine::new(
                change_log,
                CalendarEventResolver { calendar_storage },
                authz,
                Resource::Calendar,
                |label| format!("{label}.ics"),
                "CalendarSyncChange",
            ),
        }
    }

    pub async fn mint_initial_token(
        &self,
        collection_calendar_id: Uuid,
        caller_id: Uuid,
    ) -> Result<SyncToken, DomainError> {
        self.engine
            .mint_initial_token(collection_calendar_id, caller_id)
            .await
    }

    pub async fn list_changes_with_perms(
        &self,
        collection_calendar_id: Uuid,
        since_token: Option<SyncToken>,
        caller_id: Uuid,
    ) -> Result<SyncDelta<CalendarEventDto>, DomainError> {
        self.engine
            .list_changes_with_perms(collection_calendar_id, since_token, caller_id)
            .await
    }
}
