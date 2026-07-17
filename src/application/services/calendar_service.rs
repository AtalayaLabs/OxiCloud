use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::calendar_dto::{
    CalendarDto, CalendarEventDto, CreateCalendarDto, CreateEventDto, CreateEventICalDto,
    UpdateCalendarDto, UpdateEventDto,
};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::calendar_ports::{
    CalendarStoragePort, CalendarUseCase, UpsertEventsResult,
};
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::services::authorization::{Permission, Resource, Role, Subject};
use crate::infrastructure::adapters::calendar_storage_adapter::CalendarStorageAdapter;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

/// Calendar service — the CalDAV / REST entry point for every calendar
/// or event operation. Every method routes through `AuthorizationEngine`;
/// the pre-Round-3 `check_calendar_access` bespoke helper is gone.
///
/// Ownership + sharing live entirely in `storage.role_grants`
/// (`resource_type='calendar'`). `caldav.calendars.owner_id` stays for
/// provenance and legacy queries but is no longer consulted for access
/// decisions.
pub struct CalendarService {
    calendar_storage: Arc<CalendarStorageAdapter>,
    /// ReBAC engine — every user-facing method calls `authz.require`
    /// with the appropriate `Permission`. `create_calendar` also
    /// uses it to seed an Owner grant for the caller so the common
    /// "owning my own calendar" case takes a single indexed
    /// role_grants lookup.
    authz: Arc<PgAclEngine>,
}

impl CalendarService {
    pub fn new(calendar_storage: Arc<CalendarStorageAdapter>, authz: Arc<PgAclEngine>) -> Self {
        Self {
            calendar_storage,
            authz,
        }
    }

    /// Parse `calendar_id` and enforce `permission` on `Resource::Calendar(uuid)`.
    /// On denial `authz.require` returns `NotFound` (anti-enum — same
    /// shape as "no such calendar") and emits the `authz.denied` audit
    /// line. Returns the parsed UUID on success so the caller doesn't
    /// have to parse it a second time.
    async fn require_calendar_perm(
        &self,
        calendar_id: &str,
        caller_id: Uuid,
        permission: Permission,
    ) -> Result<Uuid, DomainError> {
        let uuid = Uuid::parse_str(calendar_id)
            .map_err(|_| DomainError::new(ErrorKind::InvalidInput, "Calendar", "Invalid ID"))?;
        self.authz
            .require(
                Subject::User(caller_id),
                permission,
                Resource::Calendar(uuid),
            )
            .await?;
        Ok(uuid)
    }

    /// Check `permission` on a calendar without throwing. Used by the
    /// read paths that also allow a public-calendar bypass — they need
    /// a bool, not a `Result<(), NotFound>`.
    async fn has_calendar_perm(
        &self,
        calendar_id: &str,
        caller_id: Uuid,
        permission: Permission,
    ) -> Result<bool, DomainError> {
        let uuid = Uuid::parse_str(calendar_id)
            .map_err(|_| DomainError::new(ErrorKind::InvalidInput, "Calendar", "Invalid ID"))?;
        self.authz
            .check(
                Subject::User(caller_id),
                permission,
                Resource::Calendar(uuid),
            )
            .await
    }
}

impl CalendarUseCase for CalendarService {
    async fn create_calendar(
        &self,
        calendar: CreateCalendarDto,
        user_id: Uuid,
    ) -> Result<CalendarDto, DomainError> {
        // No pre-write gate: creating a calendar is a personal act
        // (like creating a folder in your own drive). Storage stamps
        // `owner_id = user_id`; we then seed an Owner role_grant so
        // the engine's cache warms on first-read.
        let created = self
            .calendar_storage
            .create_calendar(calendar, user_id)
            .await?;
        let calendar_uuid = Uuid::parse_str(&created.id).map_err(|_| {
            DomainError::internal_error("Calendar", "storage returned invalid calendar id")
        })?;
        // `set_role` is idempotent on the `(subject, resource)` unique
        // key — a re-run (rare — only if storage retried) is a no-op.
        // `granted_by = user_id` is the self-seeded creation event.
        self.authz
            .set_role(
                user_id,
                Subject::User(user_id),
                Role::Owner,
                Resource::Calendar(calendar_uuid),
                None,
            )
            .await?;
        Ok(created)
    }

    async fn update_calendar(
        &self,
        calendar_id: &str,
        update: UpdateCalendarDto,
        user_id: Uuid,
    ) -> Result<CalendarDto, DomainError> {
        self.require_calendar_perm(calendar_id, user_id, Permission::Update)
            .await?;
        self.calendar_storage
            .update_calendar(calendar_id, update)
            .await
    }

    async fn delete_calendar(&self, calendar_id: &str, user_id: Uuid) -> Result<(), DomainError> {
        let uuid = self
            .require_calendar_perm(calendar_id, user_id, Permission::Delete)
            .await?;
        self.calendar_storage.delete_calendar(calendar_id).await?;
        // Wipe every grant on this calendar so a re-used UUID (impossible
        // today but cheap to defend against) doesn't inherit stale ACLs.
        // The storage DELETE won't cascade to `storage.role_grants` — the
        // legacy `caldav.calendar_shares` had an FK, `role_grants`
        // doesn't (it's cross-schema).
        let _ = self
            .authz
            .revoke_all_for_resource(Resource::Calendar(uuid))
            .await;
        Ok(())
    }

    async fn get_calendar(
        &self,
        calendar_id: &str,
        user_id: Uuid,
    ) -> Result<CalendarDto, DomainError> {
        let calendar = self.calendar_storage.get_calendar(calendar_id).await?;
        // Public-calendar bypass: anonymous-ish read. `check` returns
        // bool (no throw); combine with the public flag before
        // deciding.
        let allowed = calendar.is_public
            || self
                .has_calendar_perm(calendar_id, user_id, Permission::Read)
                .await?;
        if !allowed {
            return Err(DomainError::not_found("Calendar", calendar_id));
        }
        Ok(calendar)
    }

    async fn list_my_calendars(&self, user_id: Uuid) -> Result<Vec<CalendarDto>, DomainError> {
        // Post-Round-3 semantics: every calendar the caller has any
        // grant on — owned + shared, one union. The pre-Round-3
        // `list_calendars_by_owner` returned owner-only; shared
        // calendars never surfaced through this method. See
        // `docs/plan/caldav-carddav-migration-to-authz.md`.
        let grants = self
            .authz
            .list_incoming_grants(Subject::User(user_id))
            .await?;

        // Deduplicate — a user can hold multiple grants on the same
        // calendar (direct + group-inherited). We only need one DTO
        // per resource.
        let calendar_ids: HashSet<Uuid> = grants
            .into_iter()
            .filter_map(|g| match g.resource {
                Resource::Calendar(id) => Some(id),
                _ => None,
            })
            .collect();

        // Hydrate DTOs in ONE `= ANY` round-trip (was one point SELECT
        // per accessible calendar — K serial round-trips on every
        // CalDAV discovery poll). Missing rows (deleted/trashed race)
        // drop out of the result set instead of erroring, so a
        // lifecycle-race still doesn't turn a PROPFIND into a 5xx.
        let ids: Vec<Uuid> = calendar_ids.into_iter().collect();
        self.calendar_storage.get_calendars_by_ids(&ids).await
    }

    async fn list_public_calendars(
        &self,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<CalendarDto>, DomainError> {
        // No caller gate: public listing by definition. Storage
        // filters on `is_public = true`.
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);
        self.calendar_storage
            .list_public_calendars(limit, offset)
            .await
    }

    async fn create_event(
        &self,
        event: CreateEventDto,
        user_id: Uuid,
    ) -> Result<CalendarEventDto, DomainError> {
        self.require_calendar_perm(&event.calendar_id, user_id, Permission::Create)
            .await?;
        self.calendar_storage.create_event(event).await
    }

    async fn create_event_from_ical(
        &self,
        event: CreateEventICalDto,
        user_id: Uuid,
    ) -> Result<CalendarEventDto, DomainError> {
        self.require_calendar_perm(&event.calendar_id, user_id, Permission::Create)
            .await?;
        self.calendar_storage.create_event_from_ical(event).await
    }

    async fn upsert_ical_events(
        &self,
        event: CreateEventICalDto,
        user_id: Uuid,
    ) -> Result<UpsertEventsResult, DomainError> {
        // Same gate as create_event_from_ical — a PUT to the collection
        // is a write. `Permission::Create` matches the single-event
        // path; per-instance exception updates ride on the same
        // permission because from the ACL's perspective it's still
        // a write to the calendar.
        self.require_calendar_perm(&event.calendar_id, user_id, Permission::Create)
            .await?;
        self.calendar_storage.upsert_ical_events(event).await
    }

    async fn update_event(
        &self,
        event_id: &str,
        update: UpdateEventDto,
        user_id: Uuid,
    ) -> Result<CalendarEventDto, DomainError> {
        // Only the owning calendar id is needed for the gate — skip the
        // full event hydration (`ical_data` can run to tens of KB).
        let calendar_id = self
            .calendar_storage
            .calendar_id_for_event(event_id)
            .await?;
        self.require_calendar_perm(&calendar_id, user_id, Permission::Update)
            .await?;
        self.calendar_storage.update_event(event_id, update).await
    }

    async fn delete_event(&self, event_id: &str, user_id: Uuid) -> Result<(), DomainError> {
        let calendar_id = self
            .calendar_storage
            .calendar_id_for_event(event_id)
            .await?;
        self.require_calendar_perm(&calendar_id, user_id, Permission::Delete)
            .await?;
        self.calendar_storage.delete_event(event_id).await
    }

    async fn get_event(
        &self,
        event_id: &str,
        user_id: Uuid,
    ) -> Result<CalendarEventDto, DomainError> {
        let event = self.calendar_storage.get_event(event_id).await?;
        let calendar = self
            .calendar_storage
            .get_calendar(&event.calendar_id)
            .await?;
        // Same public-calendar bypass as `get_calendar`.
        let allowed = calendar.is_public
            || self
                .has_calendar_perm(&event.calendar_id, user_id, Permission::Read)
                .await?;
        if !allowed {
            return Err(DomainError::not_found("Event", event_id));
        }
        Ok(event)
    }

    async fn get_event_by_ical_uid(
        &self,
        calendar_id: &str,
        ical_uid: &str,
        user_id: Uuid,
    ) -> Result<Option<CalendarEventDto>, DomainError> {
        let calendar = self.calendar_storage.get_calendar(calendar_id).await?;
        let allowed = calendar.is_public
            || self
                .has_calendar_perm(calendar_id, user_id, Permission::Read)
                .await?;
        if !allowed {
            return Err(DomainError::not_found("Calendar", calendar_id));
        }
        self.calendar_storage
            .find_event_by_ical_uid(calendar_id, ical_uid)
            .await
    }

    async fn get_events_by_ical_uids(
        &self,
        calendar_id: &str,
        ical_uids: &[String],
        user_id: Uuid,
    ) -> Result<Vec<CalendarEventDto>, DomainError> {
        let calendar = self.calendar_storage.get_calendar(calendar_id).await?;
        let allowed = calendar.is_public
            || self
                .has_calendar_perm(calendar_id, user_id, Permission::Read)
                .await?;
        if !allowed {
            return Err(DomainError::not_found("Calendar", calendar_id));
        }
        if ical_uids.is_empty() {
            return Ok(Vec::new());
        }
        self.calendar_storage
            .find_events_by_ical_uids(calendar_id, ical_uids)
            .await
    }

    async fn list_events(
        &self,
        calendar_id: &str,
        limit: Option<i64>,
        offset: Option<i64>,
        user_id: Uuid,
    ) -> Result<Vec<CalendarEventDto>, DomainError> {
        let calendar = self.calendar_storage.get_calendar(calendar_id).await?;
        let allowed = calendar.is_public
            || self
                .has_calendar_perm(calendar_id, user_id, Permission::Read)
                .await?;
        if !allowed {
            return Err(DomainError::not_found("Calendar", calendar_id));
        }
        if limit.is_some() || offset.is_some() {
            let limit = limit.unwrap_or(100);
            let offset = offset.unwrap_or(0);
            self.calendar_storage
                .list_events_by_calendar_paginated(calendar_id, limit, offset)
                .await
        } else {
            self.calendar_storage
                .list_events_by_calendar(calendar_id)
                .await
        }
    }

    async fn stream_events_uid_order(
        &self,
        calendar_id: &str,
        user_id: Uuid,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<CalendarEventDto, DomainError>>,
        DomainError,
    > {
        // Same Read gate as `list_events`, checked ONCE before the
        // cursor opens — the stream itself carries no further authz
        // (single request, same caller, same resource).
        let calendar = self.calendar_storage.get_calendar(calendar_id).await?;
        let allowed = calendar.is_public
            || self
                .has_calendar_perm(calendar_id, user_id, Permission::Read)
                .await?;
        if !allowed {
            return Err(DomainError::not_found("Calendar", calendar_id));
        }
        Ok(self.calendar_storage.stream_events_uid_order(calendar_id))
    }

    async fn get_events_in_range(
        &self,
        calendar_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        user_id: Uuid,
    ) -> Result<Vec<CalendarEventDto>, DomainError> {
        let calendar = self.calendar_storage.get_calendar(calendar_id).await?;
        let allowed = calendar.is_public
            || self
                .has_calendar_perm(calendar_id, user_id, Permission::Read)
                .await?;
        if !allowed {
            return Err(DomainError::not_found("Calendar", calendar_id));
        }
        self.calendar_storage
            .get_events_in_time_range(calendar_id, &start, &end)
            .await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DefaultCalendarLifecycleHook
//
// Ensures every internal user has at least one owned calendar so CalDAV
// clients (Thunderbird, Apple Calendar, DAVx⁵, Gnome Calendar) succeed at
// their PROPFIND-based calendar discovery on first connect. Without this,
// a fresh user's calendar home collection is empty and every mainstream
// client returns "no calendars found" rather than offering to create one
// (see AtalayaLabs/OxiCloud#545).
//
// Idempotency: keyed on "user owns at least one calendar" via
// `list_calendars_by_owner`. If the user has any owned calendar — whether
// auto-provisioned by an earlier run, manually created by the user, or
// migrated in from another source — the hook skips. A user who deletes
// their only calendar gets a fresh default on next login (Nextcloud-style
// safety-net), matching `PersonalDriveLifecycleHook`. If they don't want
// a default, they're free to leave one they never open — it's an entry
// in a list, not a bill.
//
// Skips `is_external = true`. External users don't own resources; they
// only receive shares. When an external is later upgraded to internal via
// `POST /api/auth/upgrade-to-internal`, `on_upgraded_to_internal` fires
// and provisions the default at that point.
// ─────────────────────────────────────────────────────────────────────────────

use crate::application::ports::user_lifecycle::{DeletionMode, LogoutReason, UserLifecycleHook};
use crate::domain::entities::user::User;
use async_trait::async_trait;

pub struct DefaultCalendarLifecycleHook {
    calendar_storage: Arc<CalendarStorageAdapter>,
    /// Concrete engine — same reasoning as `PersonalDriveLifecycleHook`:
    /// `AuthorizationEngine` isn't dyn-compatible (native async-fn-in-
    /// trait), so we hold the concrete `PgAclEngine`.
    authorization: Arc<PgAclEngine>,
    /// Display name for the default calendar. Matches the Nextcloud
    /// convention so switching users don't notice the difference.
    /// Not user-visible-only — CalDAV clients render this string.
    default_name: String,
}

impl DefaultCalendarLifecycleHook {
    pub fn new(
        calendar_storage: Arc<CalendarStorageAdapter>,
        authorization: Arc<PgAclEngine>,
    ) -> Self {
        Self {
            calendar_storage,
            authorization,
            // "Personal" mirrors the Nextcloud default. Kept as a
            // struct field so a future `OXICLOUD_DEFAULT_CALENDAR_NAME`
            // env var can override without touching the hook body.
            default_name: "Personal".to_string(),
        }
    }

    /// Idempotent provisioning. Shared by `on_user_created`,
    /// `on_user_login` (safety-net for pre-existing users), and
    /// `on_upgraded_to_internal` (external → internal promotion).
    async fn provision_if_needed(&self, user: &User) -> Result<(), DomainError> {
        if user.is_external() {
            return Ok(());
        }

        // Ownership-based idempotency check (see hook docstring for
        // the design rationale). Whether the existing calendar was
        // auto-provisioned by a prior run, manually created by the
        // user, or migrated in, we respect it and skip. `EXISTS`
        // short-circuits at the first owned row instead of hydrating them
        // all just to test emptiness — this runs on EVERY login
        // (benches/ROUND13.md §Q2).
        let has_calendar = self
            .calendar_storage
            .has_owned_calendar(user.id())
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "DefaultCalendarHook",
                    format!("has_owned_calendar: {e}"),
                )
            })?;
        if has_calendar {
            return Ok(());
        }

        // Provision. Two writes: calendar row + Owner role_grant. The
        // Owner grant makes the CalDAV engine's grant lookup on first
        // read a cache hit, matching the pattern in
        // `CalendarService::create_calendar`.
        let dto = CreateCalendarDto {
            name: self.default_name.clone(),
            description: None,
            color: None,
            is_public: Some(false),
        };
        let created = self
            .calendar_storage
            .create_calendar(dto, user.id())
            .await
            .map_err(|e| {
                DomainError::internal_error("DefaultCalendarHook", format!("create_calendar: {e}"))
            })?;
        let calendar_uuid = Uuid::parse_str(&created.id).map_err(|_| {
            DomainError::internal_error(
                "DefaultCalendarHook",
                "storage returned invalid calendar id",
            )
        })?;
        self.authorization
            .set_role(
                user.id(),
                Subject::User(user.id()),
                Role::Owner,
                Resource::Calendar(calendar_uuid),
                None,
            )
            .await?;

        tracing::info!(
            target: "user_lifecycle",
            hook = "default_calendar",
            user_id = %user.id(),
            calendar_id = %calendar_uuid,
            "Default calendar provisioned"
        );
        Ok(())
    }
}

#[async_trait]
impl UserLifecycleHook for DefaultCalendarLifecycleHook {
    fn name(&self) -> &'static str {
        "default_calendar"
    }

    async fn on_user_created(&self, user: &User) -> Result<(), DomainError> {
        self.provision_if_needed(user).await
    }

    /// Safety-net: fires on every login, provisions if the user has no
    /// owned calendar. This is what fixes pre-existing users after the
    /// hook ships — no data migration needed, they get their default on
    /// their next login. Same pattern as `PersonalDriveLifecycleHook`.
    async fn on_user_login(&self, user: &User) -> Result<(), DomainError> {
        self.provision_if_needed(user).await
    }

    /// External → internal upgrade. At creation the user was external
    /// (guarded off in `provision_if_needed`); now they're internal
    /// and eligible for a default calendar.
    async fn on_upgraded_to_internal(&self, user: &User) -> Result<(), DomainError> {
        self.provision_if_needed(user).await
    }

    async fn on_user_logout(&self, _user: &User, _reason: LogoutReason) -> Result<(), DomainError> {
        Ok(())
    }

    async fn on_user_deleted(
        &self,
        _user: &User,
        _mode: DeletionMode,
        _tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DomainError> {
        // `caldav.calendars.owner_id` has ON DELETE CASCADE on
        // `auth.users(id)`, and calendar_events cascade off calendar.
        // The trigger on `role_grants` reaps the token grants. No
        // hook-side cleanup needed.
        Ok(())
    }
}
