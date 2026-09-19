use chrono::{DateTime, Utc};
/**
 * Calendar Todo Entity
 *
 * This module defines the CalendarTodo entity, which represents a task or
 * to-do item in a calendar, following the iCalendar VTODO component
 * (RFC 5545 §3.6.2). VTODO support was added for AtalayaLabs/OxiCloud#754
 * so task clients (DAVx⁵ + Tasks.org / jtx Board, Thunderbird, Apple
 * Reminders) can sync over the same CalDAV collections as events.
 *
 * Storage philosophy mirrors CalendarEvent: the complete iCalendar body
 * (`ical_data` — one VCALENDAR containing exactly one VTODO plus any
 * VTIMEZONE blocks the PUT carried, see #689) is authoritative and served
 * verbatim on GET/REPORT, so EVERY VTODO property (PRIORITY, CATEGORIES,
 * CLASS, RELATED-TO, ATTENDEE, X-*, nested VALARM, …) round-trips
 * byte-exact. The structured fields below are only the server-side filter
 * index (time-range queries per RFC 4791 §9.9); responses are never
 * regenerated from them.
 */
use uuid::Uuid;

use crate::common::errors::{DomainError, ErrorKind, Result};

use super::calendar_event::{
    CalendarComponents, CalendarEvent, ical_prop_value, ical_prop_value_date_tzid,
};

/// Owned decomposition of a [`CalendarTodo`] (see
/// [`CalendarTodo::into_parts`]).
pub struct CalendarTodoParts {
    pub id: Uuid,
    pub calendar_id: Uuid,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub status: Option<String>,
    pub percent_complete: Option<i16>,
    pub priority: Option<i16>,
    pub start_time: Option<DateTime<Utc>>,
    pub due_time: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub all_day: bool,
    pub rrule: Option<String>,
    pub recurrence_id: Option<DateTime<Utc>>,
    pub ical_uid: String,
    pub ical_data: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct CalendarTodo {
    /// Unique identifier for the todo
    id: Uuid,

    /// ID of the calendar this todo belongs to
    calendar_id: Uuid,

    /// Short summary/title of the todo. OPTIONAL on a VTODO (RFC 5545
    /// §3.6.2 mandates only UID + DTSTAMP), unlike VEVENT where
    /// OxiCloud requires SUMMARY.
    summary: Option<String>,

    /// Detailed description of the todo (optional)
    description: Option<String>,

    /// Location of the todo (optional)
    location: Option<String>,

    /// RFC 5545 §3.8.1.11 STATUS — NEEDS-ACTION / IN-PROCESS /
    /// COMPLETED / CANCELLED (free-form so client extensions survive)
    status: Option<String>,

    /// RFC 5545 §3.8.1.8 PERCENT-COMPLETE — 0..100
    percent_complete: Option<i16>,

    /// RFC 5545 §3.8.1.9 PRIORITY — 0..9 (0 = undefined)
    priority: Option<i16>,

    /// DTSTART of the todo (optional) — UTC instant; the original
    /// (possibly TZID-anchored) wall-clock form lives in `ical_data`
    start_time: Option<DateTime<Utc>>,

    /// DUE of the todo (optional)
    due_time: Option<DateTime<Utc>>,

    /// COMPLETED of the todo (optional)
    completed_at: Option<DateTime<Utc>>,

    /// Whether DTSTART carried `VALUE=DATE` (date-only task)
    all_day: bool,

    /// Recurrence rule in iCalendar RRULE format (optional — VTODOs
    /// can recur, RFC 5545 §3.6.2)
    rrule: Option<String>,

    /// RECURRENCE-ID (RFC 5545 §3.8.4.4) — same master/exception
    /// model as `CalendarEvent`: NULL on the master, non-NULL on
    /// per-instance overrides of a recurring task.
    recurrence_id: Option<DateTime<Utc>>,

    /// Unique identifier in iCalendar format (used for CalDAV sync)
    ical_uid: String,

    /// Complete iCalendar data (VCALENDAR with one VTODO + embedded
    /// VTIMEZONEs) — authoritative, served verbatim
    ical_data: String,

    /// Time when the todo was created
    created_at: DateTime<Utc>,

    /// Time when the todo was last modified
    updated_at: DateTime<Utc>,
}

impl CalendarTodo {
    /// Reconstruct a todo from a stored row. No invariant validation —
    /// every field maps a nullable column straight back; the row was
    /// validated at ingest.
    #[allow(clippy::too_many_arguments)]
    pub fn with_id(
        id: Uuid,
        calendar_id: Uuid,
        summary: Option<String>,
        description: Option<String>,
        location: Option<String>,
        status: Option<String>,
        percent_complete: Option<i16>,
        priority: Option<i16>,
        start_time: Option<DateTime<Utc>>,
        due_time: Option<DateTime<Utc>>,
        completed_at: Option<DateTime<Utc>>,
        all_day: bool,
        rrule: Option<String>,
        ical_uid: String,
        ical_data: String,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self> {
        Ok(Self {
            id,
            calendar_id,
            summary,
            description,
            location,
            status,
            percent_complete,
            priority,
            start_time,
            due_time,
            completed_at,
            all_day,
            rrule,
            recurrence_id: None,
            ical_uid,
            ical_data,
            created_at,
            updated_at,
        })
    }

    /// Creates a todo from an iCalendar body containing a VTODO
    /// component. Parses the body once via the `ical` crate (the same
    /// parser `CalendarEvent::from_ical` uses) and reads every indexed
    /// property off the parsed component.
    ///
    /// Only the component's presence is required — RFC 5545 §3.6.2
    /// mandates just UID + DTSTAMP, so a task without SUMMARY, DTSTART
    /// or DUE is perfectly valid and must not be rejected. Malformed
    /// datetimes degrade to `None` (the property still round-trips in
    /// the blob); unlike events, missing DTSTART is NOT an error.
    pub fn from_ical(calendar_id: Uuid, ical_data: String) -> Result<Self> {
        let todo = Self::parse_first_vtodo(&ical_data).ok_or_else(|| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarTodo",
                "Missing VTODO in iCalendar data",
            )
        })?;
        let props = &todo.properties;

        let ical_uid = ical_prop_value(props, "UID").unwrap_or_else(|| Uuid::new_v4().to_string());

        // DTSTART carries the all-day marker (`VALUE=DATE`) and may
        // carry a TZID (#689) — the shared params-aware reader is the
        // same one VEVENT DTSTART/DTEND go through.
        let (start_time, all_day) = match ical_prop_value_date_tzid(props, "DTSTART") {
            Some((value, is_date, tzid)) => {
                let parsed = CalendarEvent::parse_ical_datetime(&value, is_date, tzid.as_deref());
                (parsed.ok(), is_date)
            }
            None => (None, false),
        };

        // DUE — same parsing rules as DTSTART (TZID allowed; DATE form
        // allowed when DTSTART is a date). The all-day flag comes from
        // DTSTART per the VEVENT convention.
        let due_time = match ical_prop_value_date_tzid(props, "DUE") {
            Some((value, _is_date, tzid)) => {
                CalendarEvent::parse_ical_datetime(&value, all_day, tzid.as_deref()).ok()
            }
            None => None,
        };

        // COMPLETED is always a UTC DATE-TIME per RFC 5545 §3.8.2.1,
        // but parse tolerantly through the shared helper.
        let completed_at = match ical_prop_value_date_tzid(props, "COMPLETED") {
            Some((value, is_date, tzid)) => {
                CalendarEvent::parse_ical_datetime(&value, is_date, tzid.as_deref()).ok()
            }
            None => None,
        };

        // A parse failure on RECURRENCE-ID downgrades to `None` — same
        // policy as CalendarEvent::from_ical (persistence uniqueness
        // will surface a genuine conflict rather than silently
        // splitting rows).
        let recurrence_id = match ical_prop_value_date_tzid(props, "RECURRENCE-ID") {
            Some((value, is_date, tzid)) => {
                CalendarEvent::parse_ical_datetime(&value, is_date, tzid.as_deref()).ok()
            }
            None => None,
        };

        // PERCENT-COMPLETE (0..100) / PRIORITY (0..9): out-of-range or
        // non-numeric values degrade to None — the raw property still
        // round-trips in `ical_data`; only the filter index skips it.
        let percent_complete = ical_prop_value(props, "PERCENT-COMPLETE")
            .and_then(|v| v.parse::<i16>().ok())
            .filter(|v| (0..=100).contains(v));
        let priority = ical_prop_value(props, "PRIORITY")
            .and_then(|v| v.parse::<i16>().ok())
            .filter(|v| (0..=9).contains(v));

        let now = Utc::now();

        Ok(Self {
            id: Uuid::new_v4(),
            calendar_id,
            summary: ical_prop_value(props, "SUMMARY"),
            description: ical_prop_value(props, "DESCRIPTION"),
            location: ical_prop_value(props, "LOCATION"),
            status: ical_prop_value(props, "STATUS"),
            percent_complete,
            priority,
            start_time,
            due_time,
            completed_at,
            all_day,
            rrule: ical_prop_value(props, "RRULE"),
            recurrence_id,
            ical_uid,
            ical_data,
            created_at: now,
            updated_at: now,
        })
    }

    /// Parse a VCALENDAR body containing one or more VTODO components
    /// into one `CalendarTodo` per VTODO, mirroring
    /// [`CalendarEvent::parse_all_events`]. Every VTIMEZONE block in
    /// the body is embedded ahead of each VTODO in the stored shell so
    /// the row stays a self-contained VCALENDAR (#689).
    ///
    /// Returns `InvalidInput` if the body contains zero VTODOs — the
    /// caller (`upsert_ical_objects`) only routes bodies here when the
    /// split found at least one, so this is a defensive guard.
    pub fn parse_all_todos(calendar_id: Uuid, ical_data: &str) -> Result<Vec<Self>> {
        let components = CalendarEvent::split_components(ical_data);
        Self::parse_from_components(calendar_id, &components)
    }

    /// The [`CalendarComponents`]-based core of
    /// [`Self::parse_all_todos`] — the PUT fan-out splits the body ONCE
    /// for events and todos alike, then hands each kind its slice.
    pub(crate) fn parse_from_components(
        calendar_id: Uuid,
        components: &CalendarComponents,
    ) -> Result<Vec<Self>> {
        if components.vtodos.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarTodo",
                "No VTODO components found in iCalendar body",
            ));
        }

        let vtimezones = components.vtimezones.concat();

        let mut out = Vec::with_capacity(components.vtodos.len());
        for block in &components.vtodos {
            // Same wrapping contract as the events path: each stored
            // row is a self-describing VCALENDAR (VERSION + PRODID +
            // the body's VTIMEZONEs + the single VTODO).
            let wrapped = format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n{vtimezones}{block}END:VCALENDAR\r\n",
            );
            out.push(Self::from_ical(calendar_id, wrapped)?);
        }
        Ok(out)
    }

    /// Parse the raw iCalendar body and return the first VTODO
    /// component's properties. Returns `None` on any parse failure or
    /// if the body carries zero todos.
    ///
    /// Delegated to the `ical` crate's `IcalParser`, which handles
    /// line-folding, escaped characters, and RFC 5545 parameter syntax
    /// — VTODOs land in `IcalCalendar.todos` exactly like VEVENTs land
    /// in `.events`.
    fn parse_first_vtodo(ical_data: &str) -> Option<ical::parser::ical::component::IcalTodo> {
        use std::io::BufReader;
        let reader = BufReader::new(ical_data.as_bytes());
        let parser = ical::IcalParser::new(reader);
        for cal in parser {
            let Ok(cal) = cal else { continue };
            if let Some(todo) = cal.todos.into_iter().next() {
                return Some(todo);
            }
        }
        None
    }

    // Getters

    /// Returns the todo's unique identifier
    pub fn id(&self) -> &Uuid {
        &self.id
    }

    /// Returns the ID of the calendar this todo belongs to
    pub fn calendar_id(&self) -> &Uuid {
        &self.calendar_id
    }

    /// Returns the todo's summary/title, if any
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Returns the todo's description, if any
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Returns the todo's location, if any
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    /// Returns the todo's STATUS (NEEDS-ACTION / IN-PROCESS /
    /// COMPLETED / CANCELLED / client extension), if any
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Returns the todo's PERCENT-COMPLETE (0..100), if any
    pub fn percent_complete(&self) -> Option<i16> {
        self.percent_complete
    }

    /// Returns the todo's PRIORITY (0..9), if any
    pub fn priority(&self) -> Option<i16> {
        self.priority
    }

    /// Returns the todo's DTSTART, if any
    pub fn start_time(&self) -> Option<&DateTime<Utc>> {
        self.start_time.as_ref()
    }

    /// Returns the todo's DUE, if any
    pub fn due_time(&self) -> Option<&DateTime<Utc>> {
        self.due_time.as_ref()
    }

    /// Returns the todo's COMPLETED timestamp, if any
    pub fn completed_at(&self) -> Option<&DateTime<Utc>> {
        self.completed_at.as_ref()
    }

    /// Returns whether DTSTART carried `VALUE=DATE`
    pub fn all_day(&self) -> bool {
        self.all_day
    }

    /// Returns the todo's recurrence rule, if any
    pub fn rrule(&self) -> Option<&str> {
        self.rrule.as_deref()
    }

    /// Returns the RECURRENCE-ID for this todo, if any. `None` on
    /// masters and standalone tasks; `Some` on exception overrides of
    /// a recurring task.
    pub fn recurrence_id(&self) -> Option<&DateTime<Utc>> {
        self.recurrence_id.as_ref()
    }

    /// Set the RECURRENCE-ID on this todo. Used by the repository
    /// layer when reconstructing an entity from a stored row — mirrors
    /// [`CalendarEvent::set_recurrence_id`].
    pub fn set_recurrence_id(&mut self, recurrence_id: Option<DateTime<Utc>>) {
        self.recurrence_id = recurrence_id;
        self.updated_at = Utc::now();
    }

    /// Returns the todo's iCalendar UID
    pub fn ical_uid(&self) -> &str {
        &self.ical_uid
    }

    /// Returns the complete iCalendar data for the todo
    pub fn ical_data(&self) -> &str {
        &self.ical_data
    }

    /// Returns the time when the todo was created
    pub fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    /// Returns the time when the todo was last modified
    pub fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    /// Decompose into owned parts for DTO conversion — the
    /// [`CalendarEvent::into_parts`] pattern (moves the unbounded
    /// `ical_data` blob instead of deep-copying it).
    pub fn into_parts(self) -> CalendarTodoParts {
        CalendarTodoParts {
            id: self.id,
            calendar_id: self.calendar_id,
            summary: self.summary,
            description: self.description,
            location: self.location,
            status: self.status,
            percent_complete: self.percent_complete,
            priority: self.priority,
            start_time: self.start_time,
            due_time: self.due_time,
            completed_at: self.completed_at,
            all_day: self.all_day,
            rrule: self.rrule,
            recurrence_id: self.recurrence_id,
            ical_uid: self.ical_uid,
            ical_data: self.ical_data,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Minimal VTODO — RFC 5545 §3.6.2 mandates only UID + DTSTAMP.
    const MINIMAL_TODO: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//test//EN\r
BEGIN:VTODO\r
UID:todo-min@x\r
DTSTAMP:20260919T100000Z\r
END:VTODO\r
END:VCALENDAR\r
";

    /// Kitchen-sink VTODO — every indexed property present.
    const FULL_TODO: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//test//EN\r
BEGIN:VTODO\r
UID:todo-full@x\r
DTSTAMP:20260919T100000Z\r
SUMMARY:Buy milk\r
DESCRIPTION:2% or oat\r
LOCATION:Supermarket\r
STATUS:IN-PROCESS\r
PERCENT-COMPLETE:40\r
PRIORITY:1\r
DTSTART:20260920T090000Z\r
DUE:20260925T180000Z\r
CATEGORIES:ERRANDS,HOME\r
END:VTODO\r
END:VCALENDAR\r
";

    #[test]
    fn minimal_todo_parses_with_all_optionals_absent() {
        // A task with just UID + DTSTAMP must not be rejected —
        // rejecting it would 400 legitimate client payloads.
        let todo = CalendarTodo::from_ical(Uuid::new_v4(), MINIMAL_TODO.to_string())
            .expect("minimal VTODO must parse");
        assert_eq!(todo.ical_uid(), "todo-min@x");
        assert!(todo.summary().is_none());
        assert!(todo.start_time().is_none());
        assert!(todo.due_time().is_none());
        assert!(todo.status().is_none());
        assert!(!todo.all_day());
        assert!(todo.recurrence_id().is_none());
    }

    #[test]
    fn full_todo_indexes_every_structured_field() {
        let todo = CalendarTodo::from_ical(Uuid::new_v4(), FULL_TODO.to_string())
            .expect("full VTODO must parse");
        assert_eq!(todo.summary(), Some("Buy milk"));
        assert_eq!(todo.description(), Some("2% or oat"));
        assert_eq!(todo.location(), Some("Supermarket"));
        assert_eq!(todo.status(), Some("IN-PROCESS"));
        assert_eq!(todo.percent_complete(), Some(40));
        assert_eq!(todo.priority(), Some(1));
        assert_eq!(
            todo.start_time().unwrap().to_rfc3339(),
            "2026-09-20T09:00:00+00:00"
        );
        assert_eq!(
            todo.due_time().unwrap().to_rfc3339(),
            "2026-09-25T18:00:00+00:00"
        );
        // Non-indexed properties round-trip via the blob.
        assert!(todo.ical_data().contains("CATEGORIES:ERRANDS,HOME"));
    }

    #[test]
    fn out_of_range_percent_and_priority_degrade_to_none() {
        // The raw values still round-trip in ical_data — only the
        // filter index refuses them.
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VTODO\r
UID:todo-bad-ranges@x\r
DTSTAMP:20260919T100000Z\r
PERCENT-COMPLETE:140\r
PRIORITY:42\r
END:VTODO\r
END:VCALENDAR\r
";
        let todo = CalendarTodo::from_ical(Uuid::new_v4(), body.to_string()).unwrap();
        assert_eq!(todo.percent_complete(), None);
        assert_eq!(todo.priority(), None);
        assert!(todo.ical_data().contains("PERCENT-COMPLETE:140"));
    }

    #[test]
    fn completed_todo_marks_completed_at_and_status() {
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VTODO\r
UID:todo-done@x\r
DTSTAMP:20260919T100000Z\r
SUMMARY:Done thing\r
STATUS:COMPLETED\r
PERCENT-COMPLETE:100\r
COMPLETED:20260919T120000Z\r
END:VTODO\r
END:VCALENDAR\r
";
        let todo = CalendarTodo::from_ical(Uuid::new_v4(), body.to_string()).unwrap();
        assert_eq!(todo.status(), Some("COMPLETED"));
        assert_eq!(
            todo.completed_at().unwrap().to_rfc3339(),
            "2026-09-19T12:00:00+00:00"
        );
    }

    #[test]
    fn tzid_due_converts_to_utc_instant() {
        // #689 parity for tasks: Paris in September is CEST = UTC+2.
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VTODO\r
UID:todo-tzid@x\r
DTSTAMP:20260919T100000Z\r
SUMMARY:TZ task\r
DTSTART;TZID=Europe/Paris:20260920T090000\r
DUE;TZID=Europe/Paris:20260925T180000\r
END:VTODO\r
END:VCALENDAR\r
";
        let todo = CalendarTodo::from_ical(Uuid::new_v4(), body.to_string()).unwrap();
        assert_eq!(
            todo.start_time().unwrap().to_rfc3339(),
            "2026-09-20T07:00:00+00:00"
        );
        assert_eq!(
            todo.due_time().unwrap().to_rfc3339(),
            "2026-09-25T16:00:00+00:00"
        );
    }

    #[test]
    fn recurring_todo_master_and_exception_split() {
        // VTODOs can recur; the master/exception model mirrors events.
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VTODO\r
UID:todo-recur@x\r
DTSTAMP:20260919T100000Z\r
SUMMARY:Weekly chore\r
DTSTART:20260921T090000Z\r
RRULE:FREQ=WEEKLY;COUNT=4\r
END:VTODO\r
BEGIN:VTODO\r
UID:todo-recur@x\r
DTSTAMP:20260919T100000Z\r
SUMMARY:Weekly chore — moved\r
DTSTART:20260929T100000Z\r
RECURRENCE-ID:20260928T090000Z\r
END:VTODO\r
END:VCALENDAR\r
";
        let todos = CalendarTodo::parse_all_todos(Uuid::new_v4(), body)
            .expect("master + exception must parse");
        assert_eq!(todos.len(), 2);
        assert!(todos[0].recurrence_id().is_none());
        assert_eq!(todos[0].rrule(), Some("FREQ=WEEKLY;COUNT=4"));
        let rid = todos[1]
            .recurrence_id()
            .expect("exception carries RECURRENCE-ID");
        assert_eq!(*rid, Utc.with_ymd_and_hms(2026, 9, 28, 9, 0, 0).unwrap());
        for t in &todos {
            assert!(t.ical_data().starts_with("BEGIN:VCALENDAR"));
            assert!(t.ical_data().trim_end().ends_with("END:VCALENDAR"));
        }
    }

    #[test]
    fn valarm_inside_vtodo_survives_the_split() {
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VTODO\r
UID:todo-alarm@x\r
DTSTAMP:20260919T100000Z\r
SUMMARY:Pay rent\r
DUE:20261001T090000Z\r
BEGIN:VALARM\r
ACTION:DISPLAY\r
TRIGGER:-PT1H\r
END:VALARM\r
END:VTODO\r
END:VCALENDAR\r
";
        let todos = CalendarTodo::parse_all_todos(Uuid::new_v4(), body).unwrap();
        assert_eq!(todos.len(), 1);
        assert!(todos[0].ical_data().contains("BEGIN:VALARM"));
        assert!(todos[0].ical_data().contains("TRIGGER:-PT1H"));
    }

    #[test]
    fn parse_all_todos_zero_vtodos_is_invalid_input() {
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VEVENT\r
UID:e1@x\r
DTSTART:20260101T090000Z\r
DTEND:20260101T100000Z\r
SUMMARY:Not a task\r
END:VEVENT\r
END:VCALENDAR\r
";
        let err = CalendarTodo::parse_all_todos(Uuid::new_v4(), body)
            .expect_err("VEVENT-only body must be rejected on the todo path");
        assert_eq!(err.kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn from_ical_without_vtodo_is_invalid_input() {
        let err = CalendarTodo::from_ical(
            Uuid::new_v4(),
            "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".to_string(),
        )
        .expect_err("no VTODO component must error");
        assert_eq!(err.kind, ErrorKind::InvalidInput);
    }
}
