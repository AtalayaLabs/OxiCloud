//! JSON REST surface for calendars and their events.
//!
//! CalDAV (`/caldav/*`) remains the protocol endpoint for sync clients;
//! this module is the SPA's read/write path so the calendar UI does not
//! have to speak XML and iCalendar in the browser.
//!
//! Every method delegates to [`CalendarService`], which owns the AuthZ
//! gates (`Permission::Read` / `Create` / `Update` / `Manage`). The
//! handlers only authenticate the caller and translate DTOs — no
//! permission checks live here.
//!
//! Create and update both go through the iCalendar-backed service
//! methods (`create_event_from_ical` / `upsert_ical_objects`) rather
//! than the structured `create_event` one. The structured path stamps
//! a minimal ICS that contains only SUMMARY; writing the full component
//! keeps DESCRIPTION / LOCATION / RRULE visible to CalDAV clients and
//! lets an update clear a field or drop a recurrence rule, neither of
//! which the `Option`-means-"no change" update DTO can express.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::dtos::calendar_dto::{
    CalendarEventDto, CreateCalendarDto, CreateEventICalDto, UpdateCalendarDto,
};
use crate::application::ports::calendar_ports::{AccessibleCalendar, CalendarUseCase};
use crate::application::services::calendar_service::CalendarService;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

/// Range defaults used when the caller omits `start` / `end`.
const DEFAULT_RANGE_DAYS: i64 = 366;
/// Ceiling on a single query's span. The calendar UI asks for exactly
/// the visible month/week; a decade-wide request is a client bug, not a
/// feature, and would serialise every event the caller can see.
const MAX_RANGE_DAYS: i64 = 3660;

/// State for the REST calendar router.
#[derive(Clone)]
pub struct CalendarApiState {
    pub calendar_service: Arc<CalendarService>,
}

/// A calendar plus the caller's access level on it.
#[derive(Debug, Serialize, ToSchema)]
pub struct CalendarResponse {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub description: Option<String>,
    pub color: Option<String>,
    /// `read` | `contribute` | `edit` | `own`. The SPA gates the
    /// rename/delete controls on this.
    pub access: String,
    /// True when the caller cannot even add objects (Viewer/Commenter).
    pub is_readonly: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateCalendarRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateCalendarRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

/// Event projection for the UI. Deliberately drops `ical_data` — the
/// stored iCalendar blob is the CalDAV read path's payload, not a field
/// the JSON client needs (and it can run to tens of KB per row).
#[derive(Debug, Serialize, ToSchema)]
pub struct CalendarEventResponse {
    pub id: String,
    pub calendar_id: String,
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub all_day: bool,
    pub rrule: Option<String>,
    pub ical_uid: String,
    pub recurrence_id: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<CalendarEventDto> for CalendarEventResponse {
    fn from(event: CalendarEventDto) -> Self {
        Self {
            id: event.id,
            calendar_id: event.calendar_id,
            summary: event.summary,
            description: event.description,
            location: event.location,
            start_time: event.start_time,
            end_time: event.end_time,
            all_day: event.all_day,
            rrule: event.rrule,
            ical_uid: event.ical_uid,
            recurrence_id: event.recurrence_id,
            created_at: event.created_at,
            updated_at: event.updated_at,
        }
    }
}

/// Full replacement payload shared by create and update. The update
/// path re-writes the whole VEVENT, so "absent" means "cleared", not
/// "unchanged".
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SaveEventRequest {
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default)]
    pub rrule: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EventRangeQuery {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

fn calendar_response(accessible: AccessibleCalendar) -> CalendarResponse {
    let access = accessible.access;
    let calendar = accessible.calendar;
    CalendarResponse {
        id: calendar.id,
        name: calendar.name,
        owner_id: calendar.owner_id,
        description: calendar.description,
        color: calendar.color,
        access: access.as_str().to_string(),
        is_readonly: access == crate::application::ports::calendar_ports::CalendarAccess::Read,
        created_at: calendar.created_at,
        updated_at: calendar.updated_at,
    }
}

fn normalized_request(mut body: SaveEventRequest) -> SaveEventRequest {
    body.summary = body.summary.trim().to_string();
    body.description = body.description.filter(|s| !s.is_empty());
    body.location = body.location.filter(|s| !s.is_empty());
    body.rrule = body
        .rrule
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());
    body
}

/// Escape an iCalendar TEXT value (RFC 5545 §3.3.11).
fn escape_ical_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

fn ical_datetime(value: DateTime<Utc>) -> String {
    value.format("%Y%m%dT%H%M%SZ").to_string()
}

fn ical_date(value: DateTime<Utc>) -> String {
    value.format("%Y%m%d").to_string()
}

/// Render one VEVENT as a standalone VCALENDAR body.
///
/// `recurrence_id` is set only when rewriting an exception override; it
/// preserves the master/exception split the storage layer routes on.
fn build_event_ical(
    uid: &str,
    recurrence_id: Option<DateTime<Utc>>,
    body: &SaveEventRequest,
) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//OxiCloud//NONSGML Calendar//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
        "BEGIN:VEVENT".to_string(),
        // Raw: the UID is the object's identity and came from the
        // server's own parser. Escaping it here would change the key
        // the upsert routes on if the UID ever contained `,` or `;`.
        format!("UID:{}", uid),
        format!("DTSTAMP:{}", ical_datetime(Utc::now())),
    ];

    if body.all_day {
        lines.push(format!("DTSTART;VALUE=DATE:{}", ical_date(body.start_time)));
        lines.push(format!("DTEND;VALUE=DATE:{}", ical_date(body.end_time)));
    } else {
        lines.push(format!("DTSTART:{}", ical_datetime(body.start_time)));
        lines.push(format!("DTEND:{}", ical_datetime(body.end_time)));
    }

    lines.push(format!("SUMMARY:{}", escape_ical_text(&body.summary)));
    if let Some(description) = &body.description {
        lines.push(format!("DESCRIPTION:{}", escape_ical_text(description)));
    }
    if let Some(location) = &body.location {
        lines.push(format!("LOCATION:{}", escape_ical_text(location)));
    }
    if let Some(rrule) = &body.rrule {
        // RRULE is structured (RECUR), not TEXT — escaping the `;`
        // separators would corrupt the rule.
        lines.push(format!("RRULE:{}", rrule));
    }
    if let Some(recurrence_id) = recurrence_id {
        lines.push(format!("RECURRENCE-ID:{}", ical_datetime(recurrence_id)));
    }

    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());
    lines.push(String::new());
    lines
        .iter()
        .map(|line| fold_ical_line(line))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Fold a content line at 75 octets (RFC 5545 §3.1). Continuation
/// lines begin with a space, which counts toward their limit.
///
/// Without this a long DESCRIPTION reaches CalDAV clients as an
/// unfolded line. Most parsers tolerate it, but the spec does not,
/// and the byte limit is cheap to honour here.
fn fold_ical_line(line: &str) -> String {
    const LIMIT: usize = 75;
    if line.len() <= LIMIT {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + line.len() / LIMIT * 3);
    let mut used = 0;
    for ch in line.chars() {
        if used + ch.len_utf8() > LIMIT {
            out.push_str("\r\n ");
            used = 1;
        }
        out.push(ch);
        used += ch.len_utf8();
    }
    out
}

/// Validate an RRULE before it is written verbatim into the VEVENT.
///
/// The rule is structured (RECUR), not TEXT, so it is not escaped:
/// a newline or a stray `:` would inject arbitrary calendar
/// properties. Restrict it to the characters the grammar uses.
fn validate_rrule(rrule: &str) -> Result<(), AppError> {
    // RRULE token names are case-insensitive (RFC 5545 §3.3.10); the
    // CalDAV write path stores what the client sent, so accept either
    // case here rather than forcing a rewrite of a custom rule.
    if !rrule
        .as_bytes()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"FREQ="))
    {
        return Err(AppError::bad_request(
            "Recurrence rule must start with FREQ=",
        ));
    }
    if !rrule
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b';' | b',' | b'+' | b'-'))
    {
        return Err(AppError::bad_request(
            "Recurrence rule contains invalid characters",
        ));
    }
    Ok(())
}

fn validate_event(body: &SaveEventRequest) -> Result<(), AppError> {
    if body.summary.is_empty() {
        return Err(AppError::bad_request("Event summary cannot be empty"));
    }
    if body.end_time < body.start_time {
        return Err(AppError::bad_request(
            "Event end time cannot be before its start time",
        ));
    }
    if let Some(rrule) = &body.rrule {
        validate_rrule(rrule)?;
    }
    Ok(())
}

/// Resolve the requested window, applying defaults and the span cap.
fn resolve_range(query: EventRangeQuery) -> Result<(DateTime<Utc>, DateTime<Utc>), AppError> {
    let now = Utc::now();
    let default_span = Duration::days(DEFAULT_RANGE_DAYS);
    let (start, end) = match (query.start, query.end) {
        (Some(start), Some(end)) => (start, end),
        (Some(start), None) => (start, start + default_span),
        (None, Some(end)) => (end - default_span, end),
        (None, None) => (now - default_span, now + default_span),
    };
    if end <= start {
        return Err(AppError::bad_request("`end` must be after `start`"));
    }
    if (end - start) > Duration::days(MAX_RANGE_DAYS) {
        return Err(AppError::bad_request(format!(
            "Requested range exceeds the {} day maximum",
            MAX_RANGE_DAYS
        )));
    }
    Ok((start, end))
}

/// List every calendar the caller can see (owned + shared).
#[utoipa::path(
    get,
    path = "/api/calendars",
    responses(
        (status = 200, body = Vec<CalendarResponse>, description = "List of accessible calendars"),
    ),
    security(("bearerAuth" = [])),
    tag = "calendars"
)]
pub async fn list_calendars(
    State(state): State<CalendarApiState>,
    auth_user: AuthUser,
) -> Result<Response, AppError> {
    let calendars = state
        .calendar_service
        .list_my_calendars(auth_user.id)
        .await?;
    Ok((
        StatusCode::OK,
        Json(
            calendars
                .into_iter()
                .map(calendar_response)
                .collect::<Vec<_>>(),
        ),
    )
        .into_response())
}

/// Create a calendar owned by the caller.
#[utoipa::path(
    post,
    path = "/api/calendars",
    request_body = CreateCalendarRequest,
    responses(
        (status = 201, body = CalendarResponse, description = "Calendar created"),
        (status = 400, description = "Invalid input"),
    ),
    security(("bearerAuth" = [])),
    tag = "calendars"
)]
pub async fn create_calendar(
    State(state): State<CalendarApiState>,
    auth_user: AuthUser,
    Json(body): Json<CreateCalendarRequest>,
) -> Result<Response, AppError> {
    let dto = CreateCalendarDto {
        name: body.name.trim().to_string(),
        description: body.description,
        color: body.color,
    };
    let created = state
        .calendar_service
        .create_calendar(dto, auth_user.id)
        .await?;
    let accessible = state
        .calendar_service
        .get_calendar_with_access(&created.id, auth_user.id)
        .await?;
    Ok((StatusCode::CREATED, Json(calendar_response(accessible))).into_response())
}

/// Rename / recolour a calendar. Owner only (the service gates it).
#[utoipa::path(
    put,
    path = "/api/calendars/{calendar_id}",
    params(("calendar_id" = String, Path, description = "Calendar UUID")),
    request_body = UpdateCalendarRequest,
    responses(
        (status = 200, body = CalendarResponse, description = "Calendar updated"),
        (status = 403, description = "Not the owner"),
        (status = 404, description = "Calendar not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "calendars"
)]
pub async fn update_calendar(
    State(state): State<CalendarApiState>,
    auth_user: AuthUser,
    Path(calendar_id): Path<String>,
    Json(body): Json<UpdateCalendarRequest>,
) -> Result<Response, AppError> {
    let dto = UpdateCalendarDto {
        name: body.name.map(|n| n.trim().to_string()),
        description: body.description,
        color: body.color,
    };
    state
        .calendar_service
        .update_calendar(&calendar_id, dto, auth_user.id)
        .await?;
    let accessible = state
        .calendar_service
        .get_calendar_with_access(&calendar_id, auth_user.id)
        .await?;
    Ok((StatusCode::OK, Json(calendar_response(accessible))).into_response())
}

/// Delete a calendar and everything in it. Owner only.
#[utoipa::path(
    delete,
    path = "/api/calendars/{calendar_id}",
    params(("calendar_id" = String, Path, description = "Calendar UUID")),
    responses(
        (status = 204, description = "Calendar deleted"),
        (status = 403, description = "Not the owner"),
        (status = 404, description = "Calendar not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "calendars"
)]
pub async fn delete_calendar(
    State(state): State<CalendarApiState>,
    auth_user: AuthUser,
    Path(calendar_id): Path<String>,
) -> Result<Response, AppError> {
    state
        .calendar_service
        .delete_calendar(&calendar_id, auth_user.id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Events overlapping the requested window. Recurring masters are
/// returned once (not expanded); the client expands occurrences for the
/// visible range.
#[utoipa::path(
    get,
    path = "/api/calendars/{calendar_id}/events",
    params(
        ("calendar_id" = String, Path, description = "Calendar UUID"),
        ("start" = Option<DateTime<Utc>>, Query, description = "Window start (RFC 3339)"),
        ("end" = Option<DateTime<Utc>>, Query, description = "Window end (RFC 3339)"),
    ),
    responses(
        (status = 200, body = Vec<CalendarEventResponse>, description = "Events in range"),
        (status = 400, description = "Invalid range"),
        (status = 404, description = "Calendar not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "calendars"
)]
pub async fn list_events(
    State(state): State<CalendarApiState>,
    auth_user: AuthUser,
    Path(calendar_id): Path<String>,
    Query(query): Query<EventRangeQuery>,
) -> Result<Response, AppError> {
    let (start, end) = resolve_range(query)?;
    let events = state
        .calendar_service
        .get_events_in_range(&calendar_id, start, end, auth_user.id)
        .await?;
    Ok((
        StatusCode::OK,
        Json(
            events
                .into_iter()
                .map(CalendarEventResponse::from)
                .collect::<Vec<_>>(),
        ),
    )
        .into_response())
}

/// Create an event. The iCalendar body is generated server-side so the
/// UID is authoritative and CalDAV clients see the full component.
#[utoipa::path(
    post,
    path = "/api/calendars/{calendar_id}/events",
    params(("calendar_id" = String, Path, description = "Calendar UUID")),
    request_body = SaveEventRequest,
    responses(
        (status = 201, body = CalendarEventResponse, description = "Event created"),
        (status = 400, description = "Invalid input"),
        (status = 403, description = "No create permission"),
        (status = 404, description = "Calendar not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "calendars"
)]
pub async fn create_event(
    State(state): State<CalendarApiState>,
    auth_user: AuthUser,
    Path(calendar_id): Path<String>,
    Json(body): Json<SaveEventRequest>,
) -> Result<Response, AppError> {
    let body = normalized_request(body);
    validate_event(&body)?;
    let uid = format!("{}@oxicloud", Uuid::new_v4());
    let dto = CreateEventICalDto {
        calendar_id,
        ical_data: build_event_ical(&uid, None, &body),
    };
    let created = state
        .calendar_service
        .create_event_from_ical(dto, auth_user.id)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CalendarEventResponse::from(created)),
    )
        .into_response())
}

/// Replace an event. The service re-checks that the caller may modify
/// the owning calendar; an Editor-or-above is required even though the
/// underlying write is a `Create`-gated upsert.
#[utoipa::path(
    put,
    path = "/api/calendars/{calendar_id}/events/{event_id}",
    params(
        ("calendar_id" = String, Path, description = "Calendar UUID"),
        ("event_id" = String, Path, description = "Event UUID"),
    ),
    request_body = SaveEventRequest,
    responses(
        (status = 200, body = CalendarEventResponse, description = "Event updated"),
        (status = 400, description = "Invalid input"),
        (status = 403, description = "No update permission"),
        (status = 404, description = "Event not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "calendars"
)]
pub async fn update_event(
    State(state): State<CalendarApiState>,
    auth_user: AuthUser,
    Path((calendar_id, event_id)): Path<(String, String)>,
    Json(body): Json<SaveEventRequest>,
) -> Result<Response, AppError> {
    let body = normalized_request(body);
    validate_event(&body)?;

    // Read first: the rewrite must keep the object's UID (and, for an
    // exception row, its RECURRENCE-ID) so it replaces the same object
    // instead of inserting a second one.
    let existing = state
        .calendar_service
        .get_event(&event_id, auth_user.id)
        .await?;
    if existing.calendar_id != calendar_id {
        return Err(AppError::not_found("Event not found in this calendar"));
    }

    let dto = CreateEventICalDto {
        calendar_id,
        ical_data: build_event_ical(&existing.ical_uid, existing.recurrence_id, &body),
    };
    let result = state
        .calendar_service
        .upsert_ical_objects(dto, auth_user.id)
        .await?;
    // Storage deletes + reinserts, so the row id changes. Identity is
    // the (uid, recurrence_id) pair the upsert routed on.
    let updated = result
        .events
        .into_iter()
        .find(|e| e.ical_uid == existing.ical_uid && e.recurrence_id == existing.recurrence_id);

    match updated {
        Some(event) => {
            Ok((StatusCode::OK, Json(CalendarEventResponse::from(event))).into_response())
        }
        None => Err(AppError::internal_error(
            "Event update produced no matching event",
        )),
    }
}

/// Delete an event.
#[utoipa::path(
    delete,
    path = "/api/calendars/{calendar_id}/events/{event_id}",
    params(
        ("calendar_id" = String, Path, description = "Calendar UUID"),
        ("event_id" = String, Path, description = "Event UUID"),
    ),
    responses(
        (status = 204, description = "Event deleted"),
        (status = 403, description = "No delete permission"),
        (status = 404, description = "Event not found"),
    ),
    security(("bearerAuth" = [])),
    tag = "calendars"
)]
pub async fn delete_event(
    State(state): State<CalendarApiState>,
    auth_user: AuthUser,
    Path((calendar_id, event_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let existing = state
        .calendar_service
        .get_event(&event_id, auth_user.id)
        .await?;
    if existing.calendar_id != calendar_id {
        return Err(AppError::not_found("Event not found in this calendar"));
    }
    state
        .calendar_service
        .delete_event(&event_id, auth_user.id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> SaveEventRequest {
        SaveEventRequest {
            summary: "Team sync".to_string(),
            description: Some("Weekly".to_string()),
            location: Some("Room 1".to_string()),
            start_time: DateTime::parse_from_rfc3339("2026-11-05T09:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            end_time: DateTime::parse_from_rfc3339("2026-11-05T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            all_day: false,
            rrule: Some("FREQ=WEEKLY;BYDAY=TH".to_string()),
        }
    }

    /// The whole point of generating the component server-side: fields
    /// the structured create path drops must reach the stored ICS, or
    /// CalDAV clients see a summary-only event.
    #[test]
    fn generated_ical_carries_every_field() {
        let ics = build_event_ical("abc@oxicloud", None, &body());
        assert!(ics.contains("UID:abc@oxicloud\r\n"));
        assert!(ics.contains("SUMMARY:Team sync\r\n"));
        assert!(ics.contains("DESCRIPTION:Weekly\r\n"));
        assert!(ics.contains("LOCATION:Room 1\r\n"));
        assert!(ics.contains("RRULE:FREQ=WEEKLY;BYDAY=TH\r\n"));
        assert!(ics.contains("DTSTART:20261105T090000Z\r\n"));
        assert!(ics.contains("DTEND:20261105T100000Z\r\n"));
        assert!(!ics.contains("RECURRENCE-ID"));
    }

    /// RFC 5545 TEXT escaping — an unescaped comma in SUMMARY would be
    /// parsed as a value-list separator by every client.
    #[test]
    fn text_values_are_escaped() {
        assert_eq!(escape_ical_text("a,b;c\\d"), "a\\,b\\;c\\\\d");
        assert_eq!(escape_ical_text("line1\nline2"), "line1\\nline2");
        assert_eq!(escape_ical_text("crlf\r\n"), "crlf\\n");
    }

    /// All-day events use DATE values; writing `...T090000Z` instead
    /// would turn every all-day event into a timed one on sync.
    #[test]
    fn all_day_uses_date_values() {
        let mut event = body();
        event.all_day = true;
        let ics = build_event_ical("abc@oxicloud", None, &event);
        assert!(ics.contains("DTSTART;VALUE=DATE:20261105\r\n"));
        assert!(ics.contains("DTEND;VALUE=DATE:20261105\r\n"));
    }

    /// An exception override must keep its RECURRENCE-ID or the upsert
    /// writes a second master and the series gains a duplicate.
    #[test]
    fn recurrence_id_is_preserved() {
        let rid = DateTime::parse_from_rfc3339("2026-11-12T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ics = build_event_ical("abc@oxicloud", Some(rid), &body());
        assert!(ics.contains("RECURRENCE-ID:20261112T090000Z\r\n"));
    }

    #[test]
    fn range_defaults_are_a_year_each_way() {
        let (start, end) = resolve_range(EventRangeQuery {
            start: None,
            end: None,
        })
        .unwrap();
        assert_eq!(end - start, Duration::days(2 * DEFAULT_RANGE_DAYS));
    }

    #[test]
    fn inverted_and_oversized_ranges_are_rejected() {
        let start = Utc::now();
        assert!(
            resolve_range(EventRangeQuery {
                start: Some(start),
                end: Some(start - Duration::days(1)),
            })
            .is_err()
        );
        assert!(
            resolve_range(EventRangeQuery {
                start: Some(start),
                end: Some(start + Duration::days(MAX_RANGE_DAYS + 1)),
            })
            .is_err()
        );
    }

    /// RRULE is inserted into the stored VEVENT without TEXT escaping.
    /// A CRLF would end the property and let the client inject arbitrary
    /// calendar lines, so the validator must reject it before the write.
    #[test]
    fn rrule_rejects_crlf_and_non_freq_shapes() {
        let mut injected = body();
        injected.rrule = Some("FREQ=DAILY\r\nDESCRIPTION:pwned".to_string());
        assert!(validate_event(&injected).is_err());

        let mut malformed = body();
        malformed.rrule = Some("DAILY".to_string());
        assert!(validate_event(&malformed).is_err());

        let mut valid = body();
        valid.rrule = Some("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,TU".to_string());
        assert!(validate_event(&valid).is_ok());

        let mut lowercase = body();
        lowercase.rrule = Some("freq=daily".to_string());
        assert!(validate_event(&lowercase).is_ok());
    }

    /// RFC 5545 §3.1: physical lines cap at 75 octets. Unfolding the
    /// generated body must reproduce the logical line.
    #[test]
    fn long_content_lines_are_folded() {
        let mut long = body();
        long.summary = "x".repeat(200);
        let ics = build_event_ical("abc@oxicloud", None, &long);
        assert!(ics.contains("\r\n "), "expected a folded continuation");
        for physical in ics.split("\r\n") {
            assert!(physical.len() <= 75, "line too long: {physical:?}");
        }
        let unfolded = ics.replace("\r\n ", "");
        assert!(unfolded.contains(&format!("SUMMARY:{}\r\n", "x".repeat(200))));
    }

    #[test]
    fn blank_optional_fields_normalize_to_none() {
        let normalized = normalized_request(SaveEventRequest {
            summary: "  x  ".to_string(),
            description: Some(String::new()),
            location: Some(String::new()),
            rrule: Some("  ".to_string()),
            ..body()
        });
        assert_eq!(normalized.summary, "x");
        assert!(normalized.description.is_none());
        assert!(normalized.location.is_none());
        assert!(normalized.rrule.is_none());
    }
}
