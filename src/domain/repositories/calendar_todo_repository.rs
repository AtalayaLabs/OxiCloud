use crate::common::errors::DomainError;
use crate::domain::entities::calendar_todo::CalendarTodo;
use chrono::{DateTime, Utc};
use uuid::Uuid;

pub type CalendarTodoRepositoryResult<T> = Result<T, DomainError>;

/// Repository interface for CalendarTodo entity operations (#754).
///
/// Deliberately mirrors the surface `CalendarEventRepository` exposes to
/// the CalDAV paths — same master/exception UID routing, same cursor
/// stream — minus the legacy REST-era methods nothing calls.
pub trait CalendarTodoRepository: Send + Sync + 'static {
    /// Creates a new calendar todo
    async fn create_todo(&self, todo: CalendarTodo) -> CalendarTodoRepositoryResult<CalendarTodo>;

    /// Deletes a calendar todo by ID
    async fn delete_todo(&self, id: &Uuid) -> CalendarTodoRepositoryResult<()>;

    /// Narrow projection of `find_todo_by_id` for authorization gates:
    /// just the owning `calendar_id`, without dragging the full row —
    /// notably `ical_data` — off the wire.
    async fn find_calendar_id_by_todo_id(&self, id: &Uuid) -> CalendarTodoRepositoryResult<Uuid>;

    /// Finds a todo by its iCalendar UID in a specific calendar.
    ///
    /// **Master-only lookup** — filters `recurrence_id IS NULL`, same
    /// policy as `CalendarEventRepository::find_event_by_ical_uid`.
    async fn find_todo_by_ical_uid(
        &self,
        calendar_id: &Uuid,
        ical_uid: &str,
    ) -> CalendarTodoRepositoryResult<Option<CalendarTodo>>;

    /// Finds a specific per-instance exception override of a recurring
    /// task (RFC 5545 §3.8.4.4) — the VTODO twin of
    /// `find_event_by_ical_uid_and_recurrence_id`.
    async fn find_todo_by_ical_uid_and_recurrence_id(
        &self,
        calendar_id: &Uuid,
        ical_uid: &str,
        recurrence_id: &DateTime<Utc>,
    ) -> CalendarTodoRepositoryResult<Option<CalendarTodo>>;

    /// Finds the todos matching any of the given iCalendar UIDs in one
    /// indexed query (`ical_uid = ANY(...)`) — CalDAV multiget for
    /// tasks. Masters and exception overrides both come back; UIDs
    /// with no match are silently absent.
    async fn find_todos_by_ical_uids(
        &self,
        calendar_id: &Uuid,
        ical_uids: &[String],
    ) -> CalendarTodoRepositoryResult<Vec<CalendarTodo>>;

    /// Gets todos in a specific time range for a calendar, applying
    /// the RFC 4791 §9.9 VTODO overlap rules (DTSTART/DUE interval
    /// overlap, single-instants within the window, COMPLETED/CREATED
    /// fallback, recurring-master passthrough).
    async fn get_todos_in_time_range(
        &self,
        calendar_id: &Uuid,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
    ) -> CalendarTodoRepositoryResult<Vec<CalendarTodo>>;

    /// Cursor stream over every todo of `calendar_id` in bundle order
    /// (same window-function ordering as the events stream) — feeds
    /// the streaming CalDAV emitters.
    fn stream_todos_uid_order(
        &self,
        calendar_id: Uuid,
    ) -> futures::stream::BoxStream<'static, CalendarTodoRepositoryResult<CalendarTodo>>;

    /// Deletes all todos in a calendar (called before the calendar row
    /// itself is deleted, mirroring the events cleanup; the FK cascade
    /// is the backstop).
    async fn delete_all_todos_in_calendar(
        &self,
        calendar_id: &Uuid,
    ) -> CalendarTodoRepositoryResult<i64>;
}
