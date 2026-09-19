//! Calendar Storage Adapter
//!
//! This adapter implements the `CalendarStoragePort` application port using
//! the `CalendarRepository`, `CalendarEventRepository` and
//! `CalendarTodoRepository` domain repositories.
//! It bridges the gap between the application layer and the infrastructure layer.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::calendar_dto::{
    CalendarDto, CalendarEventDto, CalendarTodoDto, CreateCalendarDto, CreateEventDto,
    CreateEventICalDto, UpdateCalendarDto, UpdateEventDto,
};
use crate::application::ports::calendar_ports::{CalendarStoragePort, UpsertObjectsResult};
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::calendar::Calendar;
use crate::domain::entities::calendar_event::CalendarEvent;
use crate::domain::entities::calendar_todo::CalendarTodo;
use crate::domain::repositories::calendar_event_repository::CalendarEventRepository;
use crate::domain::repositories::calendar_repository::CalendarRepository;
use crate::domain::repositories::calendar_todo_repository::CalendarTodoRepository;
use crate::infrastructure::repositories::pg::CalendarEventPgRepository;
use crate::infrastructure::repositories::pg::CalendarPgRepository;
use crate::infrastructure::repositories::pg::CalendarTodoPgRepository;

/// Adapter that implements CalendarStoragePort using domain repositories
pub struct CalendarStorageAdapter {
    calendar_repository: Arc<CalendarPgRepository>,
    event_repository: Arc<CalendarEventPgRepository>,
    todo_repository: Arc<CalendarTodoPgRepository>,
}

impl CalendarStorageAdapter {
    /// Creates a new CalendarStorageAdapter with the given repositories
    pub fn new(
        calendar_repository: Arc<CalendarPgRepository>,
        event_repository: Arc<CalendarEventPgRepository>,
        todo_repository: Arc<CalendarTodoPgRepository>,
    ) -> Self {
        Self {
            calendar_repository,
            event_repository,
            todo_repository,
        }
    }

    /// Delegates to [`CalendarPgRepository::has_owned_calendar`] — the
    /// `EXISTS` short-circuit used by the login provisioning hook instead
    /// of hydrating every owned calendar to test emptiness
    /// (benches/ROUND13.md §Q2).
    pub async fn has_owned_calendar(&self, owner_id: Uuid) -> Result<bool, DomainError> {
        self.calendar_repository.has_owned_calendar(owner_id).await
    }
}

impl CalendarStoragePort for CalendarStorageAdapter {
    // Calendar operations

    async fn create_calendar(
        &self,
        dto: CreateCalendarDto,
        owner_id: Uuid,
    ) -> Result<CalendarDto, DomainError> {
        let calendar = Calendar::new(dto.name, owner_id, dto.description, dto.color)?;

        let created = self.calendar_repository.create_calendar(calendar).await?;
        Ok(CalendarDto::from(created))
    }

    async fn update_calendar(
        &self,
        calendar_id: &str,
        update: UpdateCalendarDto,
    ) -> Result<CalendarDto, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let mut calendar = self.calendar_repository.find_calendar_by_id(&uuid).await?;

        if let Some(name) = update.name {
            calendar.update_name(name)?;
        }
        if let Some(description) = update.description {
            calendar.update_description(Some(description));
        }
        if let Some(color) = update.color {
            calendar.update_color(Some(color))?;
        }

        let updated = self.calendar_repository.update_calendar(calendar).await?;
        Ok(CalendarDto::from(updated))
    }

    async fn delete_calendar(&self, calendar_id: &str) -> Result<(), DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        // First delete all events and todos in the calendar (the FK
        // cascade on `caldav.calendars` is the backstop).
        self.event_repository
            .delete_all_events_in_calendar(&uuid)
            .await?;
        self.todo_repository
            .delete_all_todos_in_calendar(&uuid)
            .await?;

        // Then delete the calendar itself
        self.calendar_repository.delete_calendar(&uuid).await
    }

    async fn get_calendar(&self, calendar_id: &str) -> Result<CalendarDto, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let calendar = self.calendar_repository.find_calendar_by_id(&uuid).await?;
        Ok(CalendarDto::from(calendar))
    }

    async fn get_calendars_by_ids(&self, ids: &[Uuid]) -> Result<Vec<CalendarDto>, DomainError> {
        let calendars = self.calendar_repository.find_calendars_by_ids(ids).await?;
        Ok(calendars.into_iter().map(CalendarDto::from).collect())
    }

    async fn list_calendars_by_owner(
        &self,
        owner_id: Uuid,
    ) -> Result<Vec<CalendarDto>, DomainError> {
        let calendars = self
            .calendar_repository
            .list_calendars_by_owner(owner_id)
            .await?;
        Ok(calendars.into_iter().map(CalendarDto::from).collect())
    }

    async fn list_public_calendars(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<CalendarDto>, DomainError> {
        let calendars = self
            .calendar_repository
            .list_public_calendars(limit, offset)
            .await?;
        Ok(calendars.into_iter().map(CalendarDto::from).collect())
    }

    // Calendar properties

    async fn set_calendar_property(
        &self,
        calendar_id: &str,
        property_name: &str,
        property_value: &str,
    ) -> Result<(), DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        self.calendar_repository
            .set_calendar_property(&uuid, property_name, property_value)
            .await
    }

    async fn get_calendar_property(
        &self,
        calendar_id: &str,
        property_name: &str,
    ) -> Result<Option<String>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        self.calendar_repository
            .get_calendar_property(&uuid, property_name)
            .await
    }

    async fn get_calendar_properties(
        &self,
        calendar_id: &str,
    ) -> Result<HashMap<String, String>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        self.calendar_repository
            .get_calendar_properties(&uuid)
            .await
    }

    // Event operations

    async fn create_event(&self, dto: CreateEventDto) -> Result<CalendarEventDto, DomainError> {
        let calendar_id = Uuid::parse_str(&dto.calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Event",
                "Invalid calendar ID format",
            )
        })?;

        // Verify calendar exists and user has access
        let _calendar = self
            .calendar_repository
            .find_calendar_by_id(&calendar_id)
            .await?;

        // Generate basic iCal data
        let ical_data = format!(
            "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//OxiCloud//EN\nBEGIN:VEVENT\nUID:{}@oxicloud\nDTSTAMP:{}\nDTSTART:{}\nDTEND:{}\nSUMMARY:{}\nEND:VEVENT\nEND:VCALENDAR",
            uuid::Uuid::new_v4(),
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            dto.start_time.format("%Y%m%dT%H%M%SZ"),
            dto.end_time.format("%Y%m%dT%H%M%SZ"),
            dto.summary
        );

        let event = CalendarEvent::new(
            calendar_id,
            dto.summary,
            dto.description,
            dto.location,
            dto.start_time,
            dto.end_time,
            dto.all_day.unwrap_or(false),
            dto.rrule,
            ical_data,
        )?;

        let created = self.event_repository.create_event(event).await?;
        Ok(CalendarEventDto::from(created))
    }

    async fn create_event_from_ical(
        &self,
        dto: CreateEventICalDto,
    ) -> Result<CalendarEventDto, DomainError> {
        let calendar_id = Uuid::parse_str(&dto.calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Event",
                "Invalid calendar ID format",
            )
        })?;

        // Verify calendar exists
        let _calendar = self
            .calendar_repository
            .find_calendar_by_id(&calendar_id)
            .await?;

        // Parse iCal data and create event
        let event = CalendarEvent::from_ical(calendar_id, dto.ical_data.clone())?;

        let created = self.event_repository.create_event(event).await?;
        Ok(CalendarEventDto::from(created))
    }

    async fn upsert_ical_objects(
        &self,
        dto: CreateEventICalDto,
    ) -> Result<UpsertObjectsResult, DomainError> {
        let calendar_id = Uuid::parse_str(&dto.calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Event",
                "Invalid calendar ID format",
            )
        })?;

        // Verify calendar exists before touching the object tables.
        let _calendar = self
            .calendar_repository
            .find_calendar_by_id(&calendar_id)
            .await?;

        // ONE raw-text split for the whole body: VTIMEZONEs ride
        // along into every stored row's shell (#689), VEVENTs and
        // VTODOs route to their own tables (#754). A body carrying
        // neither (or only VJOURNAL/VFREEBUSY) is a 400.
        let components = CalendarEvent::split_components(&dto.ical_data);
        if components.vevents.is_empty() && components.vtodos.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarObject",
                "No VEVENT or VTODO components found in iCalendar body",
            ));
        }

        let parsed_events = if components.vevents.is_empty() {
            Vec::new()
        } else {
            CalendarEvent::parse_events_from_components(calendar_id, &components)?
        };
        let parsed_todos = if components.vtodos.is_empty() {
            Vec::new()
        } else {
            CalendarTodo::parse_from_components(calendar_id, &components)?
        };

        let mut events = Vec::with_capacity(parsed_events.len());
        let mut todos = Vec::with_capacity(parsed_todos.len());
        let mut any_inserted = false;

        for event in parsed_events {
            let ical_uid = event.ical_uid().to_string();

            // Existing row lookup routes on the master/exception split.
            // Master: (calendar_id, ical_uid) WHERE recurrence_id IS NULL
            // Exception: (calendar_id, ical_uid, recurrence_id)
            let existing = match event.recurrence_id().copied() {
                Some(rid) => {
                    self.event_repository
                        .find_event_by_ical_uid_and_recurrence_id(&calendar_id, &ical_uid, &rid)
                        .await?
                }
                None => {
                    self.event_repository
                        .find_event_by_ical_uid(&calendar_id, &ical_uid)
                        .await?
                }
            };

            // Delete-then-insert keeps the DB-level partial unique
            // indexes happy and matches the pre-#528 update semantics
            // of the single-event path (fresh row id per replace,
            // ETag changes on update).
            if let Some(existing_event) = existing {
                self.event_repository
                    .delete_event(existing_event.id())
                    .await?;
            } else {
                any_inserted = true;
            }

            let created = self.event_repository.create_event(event).await?;
            events.push(CalendarEventDto::from(created));
        }

        for todo in parsed_todos {
            let ical_uid = todo.ical_uid().to_string();

            // Same master/exception routing as events — recurring
            // tasks override single occurrences via RECURRENCE-ID.
            let existing = match todo.recurrence_id().copied() {
                Some(rid) => {
                    self.todo_repository
                        .find_todo_by_ical_uid_and_recurrence_id(&calendar_id, &ical_uid, &rid)
                        .await?
                }
                None => {
                    self.todo_repository
                        .find_todo_by_ical_uid(&calendar_id, &ical_uid)
                        .await?
                }
            };

            if let Some(existing_todo) = existing {
                self.todo_repository.delete_todo(existing_todo.id()).await?;
            } else {
                any_inserted = true;
            }

            let created = self.todo_repository.create_todo(todo).await?;
            todos.push(CalendarTodoDto::from(created));
        }

        Ok(UpsertObjectsResult {
            events,
            todos,
            any_inserted,
        })
    }

    async fn update_event(
        &self,
        event_id: &str,
        update: UpdateEventDto,
    ) -> Result<CalendarEventDto, DomainError> {
        let uuid = Uuid::parse_str(event_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Event", "Invalid event ID format")
        })?;

        let mut event = self.event_repository.find_event_by_id(&uuid).await?;

        if let Some(summary) = update.summary {
            event.update_summary(summary)?;
        }
        if let Some(description) = update.description {
            event.update_description(Some(description));
        }
        if let Some(location) = update.location {
            event.update_location(Some(location));
        }
        if let Some(start_time) = update.start_time {
            if let Some(end_time) = update.end_time {
                event.update_time_range(start_time, end_time)?;
            } else {
                event.update_time_range(start_time, *event.end_time())?;
            }
        } else if let Some(end_time) = update.end_time {
            event.update_time_range(*event.start_time(), end_time)?;
        }
        if let Some(all_day) = update.all_day {
            event.update_all_day(all_day);
        }
        if let Some(rrule) = update.rrule {
            event.update_rrule(Some(rrule))?;
        }

        let updated = self.event_repository.update_event(event).await?;
        Ok(CalendarEventDto::from(updated))
    }

    async fn delete_event(&self, event_id: &str) -> Result<(), DomainError> {
        let uuid = Uuid::parse_str(event_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Event", "Invalid event ID format")
        })?;

        self.event_repository.delete_event(&uuid).await
    }

    async fn get_event(&self, event_id: &str) -> Result<CalendarEventDto, DomainError> {
        let uuid = Uuid::parse_str(event_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Event", "Invalid event ID format")
        })?;

        let event = self.event_repository.find_event_by_id(&uuid).await?;
        Ok(CalendarEventDto::from(event))
    }

    async fn calendar_id_for_event(&self, event_id: &str) -> Result<String, DomainError> {
        let uuid = Uuid::parse_str(event_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Event", "Invalid event ID format")
        })?;

        let calendar_id = self
            .event_repository
            .find_calendar_id_by_event_id(&uuid)
            .await?;
        Ok(calendar_id.to_string())
    }

    async fn find_event_by_ical_uid(
        &self,
        calendar_id: &str,
        ical_uid: &str,
    ) -> Result<Option<CalendarEventDto>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let event = self
            .event_repository
            .find_event_by_ical_uid(&uuid, ical_uid)
            .await?;
        Ok(event.map(CalendarEventDto::from))
    }

    async fn find_events_by_ical_uids(
        &self,
        calendar_id: &str,
        ical_uids: &[String],
    ) -> Result<Vec<CalendarEventDto>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let events = self
            .event_repository
            .find_events_by_ical_uids(&uuid, ical_uids)
            .await?;
        Ok(events.into_iter().map(CalendarEventDto::from).collect())
    }

    async fn list_events_by_calendar(
        &self,
        calendar_id: &str,
    ) -> Result<Vec<CalendarEventDto>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let events = self.event_repository.list_events_by_calendar(&uuid).await?;
        Ok(events.into_iter().map(CalendarEventDto::from).collect())
    }

    fn stream_events_uid_order(
        &self,
        calendar_id: &str,
    ) -> futures::stream::BoxStream<'static, Result<CalendarEventDto, DomainError>> {
        use futures::StreamExt;
        let uuid = match Uuid::parse_str(calendar_id) {
            Ok(u) => u,
            Err(_) => {
                return Box::pin(futures::stream::once(async {
                    Err(DomainError::new(
                        ErrorKind::InvalidInput,
                        "Calendar",
                        "Invalid calendar ID format",
                    ))
                }));
            }
        };
        Box::pin(
            self.event_repository
                .stream_events_uid_order(uuid)
                .map(|r| r.map(CalendarEventDto::from)),
        )
    }

    async fn list_events_by_calendar_paginated(
        &self,
        calendar_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<CalendarEventDto>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let events = self
            .event_repository
            .list_events_by_calendar_paginated(&uuid, limit, offset)
            .await?;
        Ok(events.into_iter().map(CalendarEventDto::from).collect())
    }

    async fn get_events_in_time_range(
        &self,
        calendar_id: &str,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
    ) -> Result<Vec<CalendarEventDto>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let events = self
            .event_repository
            .get_events_in_time_range(&uuid, start, end)
            .await?;
        Ok(events.into_iter().map(CalendarEventDto::from).collect())
    }

    // ─── Todo (VTODO) operations — #754 ──────────────────────────

    async fn find_todo_by_ical_uid(
        &self,
        calendar_id: &str,
        ical_uid: &str,
    ) -> Result<Option<CalendarTodoDto>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let todo = self
            .todo_repository
            .find_todo_by_ical_uid(&uuid, ical_uid)
            .await?;
        Ok(todo.map(CalendarTodoDto::from))
    }

    async fn find_todos_by_ical_uids(
        &self,
        calendar_id: &str,
        ical_uids: &[String],
    ) -> Result<Vec<CalendarTodoDto>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let todos = self
            .todo_repository
            .find_todos_by_ical_uids(&uuid, ical_uids)
            .await?;
        Ok(todos.into_iter().map(CalendarTodoDto::from).collect())
    }

    async fn get_todos_in_time_range(
        &self,
        calendar_id: &str,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
    ) -> Result<Vec<CalendarTodoDto>, DomainError> {
        let uuid = Uuid::parse_str(calendar_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Calendar",
                "Invalid calendar ID format",
            )
        })?;

        let todos = self
            .todo_repository
            .get_todos_in_time_range(&uuid, start, end)
            .await?;
        Ok(todos.into_iter().map(CalendarTodoDto::from).collect())
    }

    fn stream_todos_uid_order(
        &self,
        calendar_id: &str,
    ) -> futures::stream::BoxStream<'static, Result<CalendarTodoDto, DomainError>> {
        use futures::StreamExt;
        let uuid = match Uuid::parse_str(calendar_id) {
            Ok(u) => u,
            Err(_) => {
                return Box::pin(futures::stream::once(async {
                    Err(DomainError::new(
                        ErrorKind::InvalidInput,
                        "Calendar",
                        "Invalid calendar ID format",
                    ))
                }));
            }
        };
        Box::pin(
            self.todo_repository
                .stream_todos_uid_order(uuid)
                .map(|r| r.map(CalendarTodoDto::from)),
        )
    }

    async fn delete_todo(&self, todo_id: &str) -> Result<(), DomainError> {
        let uuid = Uuid::parse_str(todo_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Todo", "Invalid todo ID format")
        })?;

        self.todo_repository.delete_todo(&uuid).await
    }

    async fn calendar_id_for_todo(&self, todo_id: &str) -> Result<String, DomainError> {
        let uuid = Uuid::parse_str(todo_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Todo", "Invalid todo ID format")
        })?;

        let calendar_id = self
            .todo_repository
            .find_calendar_id_by_todo_id(&uuid)
            .await?;
        Ok(calendar_id.to_string())
    }
}

#[cfg(test)]
mod tests {
    // Tests would go here using mock repositories
}
