use crate::application::dtos::calendar_dto::{
    CalendarDto, CalendarEventDto, CalendarTodoDto, CreateCalendarDto, CreateEventDto,
    CreateEventICalDto, UpdateCalendarDto, UpdateEventDto,
};
use crate::common::errors::DomainError;
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Result of a multi-component PUT (`upsert_ical_objects`). See #528
/// for the master/exception event routing and #754 for the VTODO
/// fan-out.
#[derive(Debug, Clone)]
pub struct UpsertObjectsResult {
    /// Every event (VEVENT) that was persisted for this PUT, ordered
    /// as they appeared in the body — the master (if present) is
    /// typically first, followed by exception overrides.
    pub events: Vec<CalendarEventDto>,
    /// Every todo (VTODO) that was persisted for this PUT, same
    /// ordering contract.
    pub todos: Vec<CalendarTodoDto>,
    /// True if at least one row was newly created; false if every
    /// object replaced an existing row. Drives the handler's choice
    /// between 201 Created and 204 No Content.
    pub any_inserted: bool,
}

/// Port for external calendar storage mechanisms
pub trait CalendarStoragePort: Send + Sync + 'static {
    // Calendar operations
    async fn create_calendar(
        &self,
        calendar: CreateCalendarDto,
        owner_id: Uuid,
    ) -> Result<CalendarDto, DomainError>;
    async fn update_calendar(
        &self,
        calendar_id: &str,
        update: UpdateCalendarDto,
    ) -> Result<CalendarDto, DomainError>;
    async fn delete_calendar(&self, calendar_id: &str) -> Result<(), DomainError>;
    async fn get_calendar(&self, calendar_id: &str) -> Result<CalendarDto, DomainError>;

    /// Batch sibling of [`Self::get_calendar`]: hydrate a page of
    /// grant-derived calendar ids in ONE storage round-trip. Missing
    /// rows (deleted/trashed race) drop out silently; ordering is not
    /// guaranteed.
    async fn get_calendars_by_ids(&self, ids: &[Uuid]) -> Result<Vec<CalendarDto>, DomainError>;
    async fn list_calendars_by_owner(
        &self,
        owner_id: Uuid,
    ) -> Result<Vec<CalendarDto>, DomainError>;
    async fn list_public_calendars(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<CalendarDto>, DomainError>;
    // Calendar properties
    async fn set_calendar_property(
        &self,
        calendar_id: &str,
        property_name: &str,
        property_value: &str,
    ) -> Result<(), DomainError>;
    async fn get_calendar_property(
        &self,
        calendar_id: &str,
        property_name: &str,
    ) -> Result<Option<String>, DomainError>;
    async fn get_calendar_properties(
        &self,
        calendar_id: &str,
    ) -> Result<std::collections::HashMap<String, String>, DomainError>;

    // Event operations
    async fn create_event(&self, event: CreateEventDto) -> Result<CalendarEventDto, DomainError>;
    async fn create_event_from_ical(
        &self,
        event: CreateEventICalDto,
    ) -> Result<CalendarEventDto, DomainError>;
    /// Upsert every calendar object in an iCalendar body — VEVENTs
    /// and VTODOs alike (#754). A body carrying a master + N
    /// per-instance exception overrides (RFC 5545 §3.8.4.4) persists
    /// each component to its own row; VTIMEZONE blocks ride along into
    /// every stored row's shell (#689).
    ///
    /// Routing: a component whose `RECURRENCE-ID` is unset targets the
    /// master row `(calendar_id, ical_uid) WHERE recurrence_id IS NULL`
    /// of its kind's table; one whose `RECURRENCE-ID` is set targets
    /// its own exception row. Existing rows are replaced
    /// (delete-then-insert to stay compatible with the DB-level
    /// partial unique indexes and to keep the ETag surface identical
    /// to the pre-#528 single-event path).
    ///
    /// Returns `InvalidInput` when the body carries neither VEVENTs
    /// nor VTODOs — the handler maps that to 400.
    async fn upsert_ical_objects(
        &self,
        event: CreateEventICalDto,
    ) -> Result<UpsertObjectsResult, DomainError>;
    async fn update_event(
        &self,
        event_id: &str,
        update: UpdateEventDto,
    ) -> Result<CalendarEventDto, DomainError>;
    async fn delete_event(&self, event_id: &str) -> Result<(), DomainError>;
    async fn get_event(&self, event_id: &str) -> Result<CalendarEventDto, DomainError>;
    /// Narrow projection for authz gates: the owning calendar of an event
    /// without hydrating the full event row (notably `ical_data`, the raw
    /// iCalendar body, which can run to tens of KB on recurring events).
    async fn calendar_id_for_event(&self, event_id: &str) -> Result<String, DomainError>;
    /// Indexed single-row lookup by iCalendar UID — the CalDAV
    /// object-resource paths must use this instead of listing the whole
    /// calendar (every row + its `ical_data`) and filtering client-side.
    async fn find_event_by_ical_uid(
        &self,
        calendar_id: &str,
        ical_uid: &str,
    ) -> Result<Option<CalendarEventDto>, DomainError>;
    /// Indexed batch lookup by iCalendar UID (`ical_uid = ANY(...)`) — the
    /// CalDAV multiget REPORT must use this instead of listing the whole
    /// calendar (every row + its `ical_data`) and filtering client-side.
    async fn find_events_by_ical_uids(
        &self,
        calendar_id: &str,
        ical_uids: &[String],
    ) -> Result<Vec<CalendarEventDto>, DomainError>;
    async fn list_events_by_calendar(
        &self,
        calendar_id: &str,
    ) -> Result<Vec<CalendarEventDto>, DomainError>;
    /// Cursor stream over the calendar's events in bundle order (see
    /// the repository doc) — feeds the streaming CalDAV emitters.
    fn stream_events_uid_order(
        &self,
        calendar_id: &str,
    ) -> futures::stream::BoxStream<'static, Result<CalendarEventDto, DomainError>>;
    async fn list_events_by_calendar_paginated(
        &self,
        calendar_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<CalendarEventDto>, DomainError>;
    async fn get_events_in_time_range(
        &self,
        calendar_id: &str,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
    ) -> Result<Vec<CalendarEventDto>, DomainError>;

    // Todo (VTODO) operations — #754. Same lookup contracts as the
    // event methods above, on the `caldav.calendar_todos` store.

    /// Indexed single-row (master-only) lookup by iCalendar UID.
    async fn find_todo_by_ical_uid(
        &self,
        calendar_id: &str,
        ical_uid: &str,
    ) -> Result<Option<CalendarTodoDto>, DomainError>;
    /// Indexed batch lookup by iCalendar UID (`ical_uid = ANY(...)`)
    /// — CalDAV multiget for tasks.
    async fn find_todos_by_ical_uids(
        &self,
        calendar_id: &str,
        ical_uids: &[String],
    ) -> Result<Vec<CalendarTodoDto>, DomainError>;
    /// RFC 4791 §9.9 VTODO time-range overlap query.
    async fn get_todos_in_time_range(
        &self,
        calendar_id: &str,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
    ) -> Result<Vec<CalendarTodoDto>, DomainError>;
    /// Cursor stream over the calendar's todos in bundle order —
    /// feeds the streaming CalDAV emitters.
    fn stream_todos_uid_order(
        &self,
        calendar_id: &str,
    ) -> futures::stream::BoxStream<'static, Result<CalendarTodoDto, DomainError>>;
    async fn delete_todo(&self, todo_id: &str) -> Result<(), DomainError>;
    /// Narrow projection for authz gates: the owning calendar of a
    /// todo without hydrating the full row.
    async fn calendar_id_for_todo(&self, todo_id: &str) -> Result<String, DomainError>;
}

/// Port for calendar use cases.
///
/// All methods require an explicit `user_id` parameter for authorization.
/// The CalDAV protocol handler extracts the user identity from JWT claims
/// and passes it through.
pub trait CalendarUseCase: Send + Sync + 'static {
    // Calendar operations
    async fn create_calendar(
        &self,
        calendar: CreateCalendarDto,
        user_id: Uuid,
    ) -> Result<CalendarDto, DomainError>;
    async fn update_calendar(
        &self,
        calendar_id: &str,
        update: UpdateCalendarDto,
        user_id: Uuid,
    ) -> Result<CalendarDto, DomainError>;
    async fn delete_calendar(&self, calendar_id: &str, user_id: Uuid) -> Result<(), DomainError>;
    async fn get_calendar(
        &self,
        calendar_id: &str,
        user_id: Uuid,
    ) -> Result<CalendarDto, DomainError>;
    async fn list_my_calendars(&self, user_id: Uuid) -> Result<Vec<CalendarDto>, DomainError>;
    async fn list_public_calendars(
        &self,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<CalendarDto>, DomainError>;

    // Event operations
    async fn create_event(
        &self,
        event: CreateEventDto,
        user_id: Uuid,
    ) -> Result<CalendarEventDto, DomainError>;
    async fn create_event_from_ical(
        &self,
        event: CreateEventICalDto,
        user_id: Uuid,
    ) -> Result<CalendarEventDto, DomainError>;
    /// Route a PUT'd iCalendar body containing one or more calendar
    /// components (VEVENT and/or VTODO — #754) to their per-instance
    /// rows. See `CalendarStoragePort::upsert_ical_objects` for the
    /// routing rules; this method just adds the `Permission::Create`
    /// gate for the caller.
    async fn upsert_ical_objects(
        &self,
        event: CreateEventICalDto,
        user_id: Uuid,
    ) -> Result<UpsertObjectsResult, DomainError>;
    async fn update_event(
        &self,
        event_id: &str,
        update: UpdateEventDto,
        user_id: Uuid,
    ) -> Result<CalendarEventDto, DomainError>;
    async fn delete_event(&self, event_id: &str, user_id: Uuid) -> Result<(), DomainError>;
    async fn get_event(
        &self,
        event_id: &str,
        user_id: Uuid,
    ) -> Result<CalendarEventDto, DomainError>;
    /// Resolve one event by its iCalendar UID (the identifier CalDAV
    /// object resources are addressed by). `Ok(None)` when no event with
    /// that UID exists in the calendar.
    async fn get_event_by_ical_uid(
        &self,
        calendar_id: &str,
        ical_uid: &str,
        user_id: Uuid,
    ) -> Result<Option<CalendarEventDto>, DomainError>;
    /// Resolve a batch of events by their iCalendar UIDs with a single
    /// indexed query. UIDs without a matching event are silently absent
    /// from the result (CalDAV multiget semantics).
    async fn get_events_by_ical_uids(
        &self,
        calendar_id: &str,
        ical_uids: &[String],
        user_id: Uuid,
    ) -> Result<Vec<CalendarEventDto>, DomainError>;
    async fn list_events(
        &self,
        calendar_id: &str,
        limit: Option<i64>,
        offset: Option<i64>,
        user_id: Uuid,
    ) -> Result<Vec<CalendarEventDto>, DomainError>;
    /// Streaming support: cursor over the calendar's events in bundle
    /// order, behind the same Read authz gate as [`Self::list_events`].
    async fn stream_events_uid_order(
        &self,
        calendar_id: &str,
        user_id: Uuid,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<CalendarEventDto, DomainError>>,
        DomainError,
    >;
    async fn get_events_in_range(
        &self,
        calendar_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        user_id: Uuid,
    ) -> Result<Vec<CalendarEventDto>, DomainError>;

    // Todo (VTODO) operations — #754. Same authz gates as the event
    // methods above (a task lives under the same calendar resource).

    /// Resolve one todo (master row) by its iCalendar UID. `Ok(None)`
    /// when no todo with that UID exists in the calendar.
    async fn get_todo_by_ical_uid(
        &self,
        calendar_id: &str,
        ical_uid: &str,
        user_id: Uuid,
    ) -> Result<Option<CalendarTodoDto>, DomainError>;
    /// Resolve a batch of todos by their iCalendar UIDs with a single
    /// indexed query (CalDAV multiget semantics).
    async fn get_todos_by_ical_uids(
        &self,
        calendar_id: &str,
        ical_uids: &[String],
        user_id: Uuid,
    ) -> Result<Vec<CalendarTodoDto>, DomainError>;
    /// RFC 4791 §9.9 VTODO time-range query, behind the same Read
    /// authz gate as [`Self::get_events_in_range`].
    async fn get_todos_in_range(
        &self,
        calendar_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        user_id: Uuid,
    ) -> Result<Vec<CalendarTodoDto>, DomainError>;
    /// Streaming support: cursor over the calendar's todos in bundle
    /// order, behind the same Read authz gate.
    async fn stream_todos_uid_order(
        &self,
        calendar_id: &str,
        user_id: Uuid,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<CalendarTodoDto, DomainError>>,
        DomainError,
    >;
    async fn delete_todo(&self, todo_id: &str, user_id: Uuid) -> Result<(), DomainError>;
}
