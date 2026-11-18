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
//!
//! Updates merge into the stored VEVENT instead of regenerating it, so
//! properties the JSON surface does not model (ATTENDEE, VALARM,
//! CATEGORIES, EXDATE/RDATE, X-*) survive an edit made in the SPA.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::dtos::calendar_dto::{
    CalendarEventDto, CreateCalendarDto, CreateEventICalDto, UpdateCalendarDto,
};
use crate::application::ports::calendar_ports::{AccessibleCalendar, CalendarUseCase};
use crate::application::services::calendar_service::CalendarService;
use crate::domain::entities::calendar_event::CalendarEvent;
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
    /// IANA zone from the stored DTSTART, when the event carries one.
    /// The SPA expands recurrence in this zone so wall-clock times do
    /// not drift across DST; absent means the series is UTC.
    pub timezone: Option<String>,
    pub ical_uid: String,
    pub recurrence_id: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<CalendarEventDto> for CalendarEventResponse {
    fn from(event: CalendarEventDto) -> Self {
        let timezone = ical_dtstart_timezone(&event.ical_data);
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
            timezone,
            ical_uid: event.ical_uid,
            recurrence_id: event.recurrence_id,
            created_at: event.created_at,
            updated_at: event.updated_at,
        }
    }
}

/// Full replacement payload shared by create and update. The update
/// path replaces only the VEVENT properties this DTO models, so
/// "absent" means "cleared" for those; unmodelled properties in the
/// stored component are preserved.
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
    /// IANA zone the caller entered the wall-clock time in (for example
    /// `Europe/Paris`). Timed events are stored with a matching
    /// `DTSTART;TZID=…`, so their recurrence keeps the same local time
    /// across DST changes. Absent/`null` keeps the legacy UTC storage.
    #[serde(default)]
    pub timezone: Option<String>,
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
    body.timezone = body
        .timezone
        .map(|tz| tz.trim().trim_matches('"').to_string())
        .filter(|tz| !tz.is_empty());
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

/// DTSTART/DTEND lines for the payload. A timed event with a valid
/// IANA timezone is written in that zone (`TZID`), which is what keeps
/// its weekly wall-clock time stable across DST; an absent or unknown
/// zone falls back to the legacy UTC form.
fn timed_event_lines(body: &SaveEventRequest, timezone: Option<&str>) -> Vec<String> {
    if body.all_day {
        return vec![
            format!("DTSTART;VALUE=DATE:{}", ical_date(body.start_time)),
            format!("DTEND;VALUE=DATE:{}", ical_date(body.end_time)),
        ];
    }

    if let Some(tzid) = timezone
        && let Ok(tz) = tzid.parse::<chrono_tz::Tz>()
    {
        let name = tz.name();
        let start = tz.from_utc_datetime(&body.start_time.naive_utc());
        let end = tz.from_utc_datetime(&body.end_time.naive_utc());
        return vec![
            format!("DTSTART;TZID={}:{}", name, start.format("%Y%m%dT%H%M%S")),
            format!("DTEND;TZID={}:{}", name, end.format("%Y%m%dT%H%M%S")),
        ];
    }

    vec![
        format!("DTSTART:{}", ical_datetime(body.start_time)),
        format!("DTEND:{}", ical_datetime(body.end_time)),
    ]
}

/// The properties the JSON payload owns. Every other property in a
/// stored VEVENT (ATTENDEE, VALARM, CATEGORIES, EXDATE/RDATE, X-*, …)
/// is preserved verbatim by [`merge_event_ical`].
fn event_property_lines(
    uid: &str,
    recurrence_id: Option<DateTime<Utc>>,
    body: &SaveEventRequest,
    include_times: bool,
    timezone: Option<&str>,
) -> Vec<String> {
    let mut lines = vec![
        // Raw: the UID is the object's identity and came from the
        // server's own parser. Escaping it here would change the key
        // the upsert routes on if the UID ever contained `,` or `;`.
        format!("UID:{}", uid),
        format!("DTSTAMP:{}", ical_datetime(Utc::now())),
    ];

    if include_times {
        lines.extend(timed_event_lines(body, timezone));
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
        // RFC 5545 §3.8.4.4: the value type must match the recurring
        // event's DTSTART, so an all-day series uses DATE here too.
        if body.all_day {
            lines.push(format!(
                "RECURRENCE-ID;VALUE=DATE:{}",
                ical_date(recurrence_id)
            ));
        } else {
            lines.push(format!("RECURRENCE-ID:{}", ical_datetime(recurrence_id)));
        }
    }

    lines
}

/// Render one VEVENT as a standalone VCALENDAR body.
///
/// `recurrence_id` is set only when rewriting an exception override; it
/// preserves the master/exception split the storage layer routes on.
fn build_event_ical(
    uid: &str,
    recurrence_id: Option<DateTime<Utc>>,
    body: &SaveEventRequest,
    timezone: Option<&str>,
) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//OxiCloud//NONSGML Calendar//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
        "BEGIN:VEVENT".to_string(),
    ];
    lines.extend(event_property_lines(
        uid,
        recurrence_id,
        body,
        true,
        timezone,
    ));
    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());
    lines.push(String::new());
    lines
        .iter()
        .map(|line| fold_ical_line(line))
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn starts_with_ci(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// Undo RFC 5545 §3.1 line folding inside one component block.
fn unfold_ical_lines(block: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in block.split('\n') {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if (line.starts_with(' ') || line.starts_with('\t')) && !lines.is_empty() {
            if let Some(last) = lines.last_mut() {
                last.push_str(&line[1..]);
            }
            continue;
        }
        lines.push(line.to_string());
    }
    lines
}

/// IANA zone carried by DTSTART's `TZID` parameter, if any. The SPA
/// uses it to expand recurrence in wall-clock time; DATE values and
/// UTC DTSTARTs have no zone.
fn ical_dtstart_timezone(ical_data: &str) -> Option<String> {
    // Read only the VEVENT block: a VTIMEZONE's STANDARD/DAYLIGHT
    // sub-components also carry DTSTART properties.
    let components = CalendarEvent::split_components(ical_data);
    let block = components.vevents.first()?;

    for line in unfold_ical_lines(block) {
        let line = line.trim_start();
        let Some((head, value)) = line.split_once(':') else {
            continue;
        };
        let Some((name, params)) = head.split_once(';') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("DTSTART") {
            continue;
        }
        // A `Z` suffix means the value is already UTC and the domain
        // parser ignores TZID; DATE values are zone-free. Neither
        // should make the SPA expand in wall-clock time.
        let value = value.trim();
        if value.ends_with('Z') {
            continue;
        }
        let mut timezone = None;
        let mut date_value = false;
        for param in params.split(';') {
            let Some((key, param_value)) = param.split_once('=') else {
                continue;
            };
            if key.eq_ignore_ascii_case("TZID") {
                timezone = Some(param_value.trim().trim_matches('"').to_string());
            } else if key.eq_ignore_ascii_case("VALUE") && param_value.eq_ignore_ascii_case("DATE")
            {
                date_value = true;
            }
        }
        if !date_value && let Some(timezone) = timezone.filter(|zone| !zone.is_empty()) {
            return Some(timezone);
        }
    }
    None
}

/// Property names at VEVENT depth that the JSON payload replaces.
/// Nested components such as VALARM are never reached by the caller's
/// depth check, so their DESCRIPTION/SUMMARY survive an edit.
fn is_replaced_property(line: &str, body: &SaveEventRequest, keep_existing_times: bool) -> bool {
    let name_end = line.find([':', ';']).unwrap_or(line.len());
    let name = &line[..name_end];
    let is = |property: &str| name.eq_ignore_ascii_case(property);

    is("UID")
        || is("DTSTAMP")
        || is("SUMMARY")
        || is("DESCRIPTION")
        || is("LOCATION")
        || is("RRULE")
        || (!keep_existing_times && (is("DTSTART") || is("DTEND")))
        || (body.rrule.is_none() && (is("EXDATE") || is("RDATE")))
}

fn rewrite_vevent_block(
    block: &str,
    uid: &str,
    body: &SaveEventRequest,
    keep_existing_times: bool,
    timezone: Option<&str>,
) -> Option<String> {
    let lines = unfold_ical_lines(block);
    if !lines
        .first()
        .is_some_and(|line| starts_with_ci(line.trim_start(), "BEGIN:VEVENT"))
    {
        return None;
    }

    let mut out: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut closed = false;

    for line in lines {
        let trimmed = line.trim_start();
        if starts_with_ci(trimmed, "BEGIN:") {
            depth += 1;
            if depth == 1 {
                out.push("BEGIN:VEVENT".to_string());
                out.extend(event_property_lines(
                    uid,
                    None,
                    body,
                    !keep_existing_times,
                    timezone,
                ));
            } else {
                out.push(line);
            }
            continue;
        }
        if starts_with_ci(trimmed, "END:") {
            if depth == 1 {
                out.push("END:VEVENT".to_string());
                depth = 0;
                closed = true;
                continue;
            }
            depth = depth.saturating_sub(1);
            out.push(line);
            continue;
        }
        if depth == 1 && is_replaced_property(trimmed, body, keep_existing_times) {
            continue;
        }
        out.push(line);
    }

    if !closed {
        return None;
    }

    let mut folded = out
        .iter()
        .map(|line| fold_ical_line(line))
        .collect::<Vec<_>>()
        .join("\r\n");
    folded.push_str("\r\n");
    Some(folded)
}

/// Rewrite the JSON-owned properties of an existing VEVENT while
/// keeping every property the SPA does not model. Rebuilding the event
/// from `SaveEventRequest` alone would strip ATTENDEE, VALARM,
/// CATEGORIES, EXDATE/RDATE and X-* on the first edit of an event
/// created by a CalDAV client.
///
/// `keep_existing_times` is set when the payload's start/end/all-day
/// values are unchanged. In that case the original DTSTART/DTEND lines
/// are kept, including their `TZID`/`VALUE=DATE` parameters, so merely
/// renaming a Europe/Paris event does not flatten it to UTC.
fn merge_event_ical(
    existing: &str,
    uid: &str,
    body: &SaveEventRequest,
    keep_existing_times: bool,
    timezone: Option<&str>,
) -> Option<String> {
    let components = CalendarEvent::split_components(existing);
    let block = components.vevents.first()?;
    let merged = rewrite_vevent_block(block, uid, body, keep_existing_times, timezone)?;

    let mut out = String::from(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n",
    );
    for timezone in &components.vtimezones {
        out.push_str(timezone);
    }
    out.push_str(&merged);
    out.push_str("END:VCALENDAR\r\n");
    Some(out)
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
    if !body.all_day
        && let Some(timezone) = &body.timezone
        && timezone.parse::<chrono_tz::Tz>().is_err()
    {
        return Err(AppError::bad_request("Unknown timezone"));
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
        ical_data: build_event_ical(&uid, None, &body, body.timezone.as_deref()),
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
/// underlying write is a `Create`-gated upsert. Properties outside the
/// JSON DTO are preserved from the stored component (see
/// [`merge_event_ical`]); unchanged timing keeps its original
/// `TZID`/`VALUE=DATE` value form.
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

    // Prefer the series' stored zone: a shared Europe/Paris series must
    // not be redefined in the editor's browser zone.
    let existing_timezone = ical_dtstart_timezone(&existing.ical_data);
    let effective_timezone = existing_timezone.as_deref().or(body.timezone.as_deref());
    // Rows created by this SPA before TZIDs were stored are UTC with an
    // `@oxicloud` UID. Their first edit rewrites the same instant in the
    // browser zone, so they stop drifting without a data migration;
    // CalDAV-created UTC rows keep their UTC semantics.
    let add_timezone = existing_timezone.is_none()
        && !existing.all_day
        && body.timezone.is_some()
        && existing.ical_uid.ends_with("@oxicloud");
    // Keep the stored value form when the timing did not change: a
    // TZID-based CalDAV event must not be flattened to UTC by a rename.
    let keep_existing_times = existing.start_time == body.start_time
        && existing.end_time == body.end_time
        && existing.all_day == body.all_day
        && !add_timezone;
    let ical_data = merge_event_ical(
        &existing.ical_data,
        &existing.ical_uid,
        &body,
        keep_existing_times,
        effective_timezone,
    )
    .unwrap_or_else(|| {
        build_event_ical(
            &existing.ical_uid,
            existing.recurrence_id,
            &body,
            effective_timezone,
        )
    });

    let dto = CreateEventICalDto {
        calendar_id,
        ical_data,
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
            timezone: None,
        }
    }

    /// The whole point of generating the component server-side: fields
    /// the structured create path drops must reach the stored ICS, or
    /// CalDAV clients see a summary-only event.
    #[test]
    fn generated_ical_carries_every_field() {
        let ics = build_event_ical("abc@oxicloud", None, &body(), None);
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
        let ics = build_event_ical("abc@oxicloud", None, &event, None);
        assert!(ics.contains("DTSTART;VALUE=DATE:20261105\r\n"));
        assert!(ics.contains("DTEND;VALUE=DATE:20261105\r\n"));
    }

    #[test]
    fn timed_event_uses_tzid_when_timezone_present() {
        let mut event = body();
        event.timezone = Some("Europe/Paris".to_string());
        let ics = build_event_ical("abc@oxicloud", None, &event, event.timezone.as_deref());
        // 2026-11-05 09:00Z is 10:00 CET.
        assert!(ics.contains("DTSTART;TZID=Europe/Paris:20261105T100000\r\n"));
        assert!(ics.contains("DTEND;TZID=Europe/Paris:20261105T110000\r\n"));
        assert!(!ics.contains("DTSTART:20261105T090000Z"));
    }

    /// A wall-class event in the same zone uses the DST offset.
    #[test]
    fn tzid_uses_the_instant_dst_offset() {
        let mut event = body();
        event.timezone = Some("Europe/Paris".to_string());
        event.start_time = DateTime::parse_from_rfc3339("2026-07-05T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        event.end_time = DateTime::parse_from_rfc3339("2026-07-05T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ics = build_event_ical("abc@oxicloud", None, &event, event.timezone.as_deref());
        // July is CEST (UTC+2), unlike the November case above.
        assert!(ics.contains("DTSTART;TZID=Europe/Paris:20260705T110000\r\n"));
        assert!(ics.contains("DTEND;TZID=Europe/Paris:20260705T120000\r\n"));
    }

    #[test]
    fn unknown_timezone_is_rejected() {
        let mut event = body();
        event.timezone = Some("Not/ARealZone".to_string());
        assert!(validate_event(&event).is_err());
    }

    #[test]
    fn tzid_is_extracted_from_stored_dtstart() {
        assert_eq!(
            ical_dtstart_timezone(stored_rich_event()).as_deref(),
            Some("Europe/Paris")
        );
        assert_eq!(
            ical_dtstart_timezone(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:20261105T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            ),
            None
        );
        // A `Z` suffix wins over TZID in the parser, and DATE values
        // have no zone; neither should trigger wall-clock expansion.
        assert_eq!(
            ical_dtstart_timezone(
                "BEGIN:VEVENT\r\nDTSTART;TZID=Europe/Paris:20261105T090000Z\r\nEND:VEVENT\r\n"
            ),
            None
        );
        assert_eq!(
            ical_dtstart_timezone("BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20261105\r\nEND:VEVENT\r\n"),
            None
        );
    }

    #[test]
    fn changed_times_keep_the_series_timezone() {
        let merged = merge_event_ical(
            stored_rich_event(),
            "abc@oxicloud",
            &body(),
            false,
            Some("Europe/Paris"),
        )
        .unwrap();
        assert!(merged.contains("DTSTART;TZID=Europe/Paris:20261105T100000\r\n"));
        assert!(merged.contains("DTEND;TZID=Europe/Paris:20261105T110000\r\n"));
        assert!(!merged.contains("DTSTART:20261105T090000Z"));
    }

    /// An exception override must keep its RECURRENCE-ID or the upsert
    /// writes a second master and the series gains a duplicate.
    #[test]
    fn recurrence_id_is_preserved() {
        let rid = DateTime::parse_from_rfc3339("2026-11-12T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ics = build_event_ical("abc@oxicloud", Some(rid), &body(), None);
        assert!(ics.contains("RECURRENCE-ID:20261112T090000Z\r\n"));
    }

    /// RFC 5545 §3.8.4.4: RECURRENCE-ID's value type matches the
    /// master DTSTART, so an all-day exception must use VALUE=DATE.
    #[test]
    fn all_day_recurrence_id_uses_date_value() {
        let rid = DateTime::parse_from_rfc3339("2026-11-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut event = body();
        event.all_day = true;
        let ics = build_event_ical("abc@oxicloud", Some(rid), &event, None);
        assert!(ics.contains("RECURRENCE-ID;VALUE=DATE:20261112\r\n"));
        assert!(!ics.contains("RECURRENCE-ID:20261112T000000Z"));
    }

    fn stored_rich_event() -> &'static str {
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//OxiCloud//NONSGML Calendar//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:abc@oxicloud\r\n\
         DTSTAMP:20261101T120000Z\r\n\
         DTSTART;TZID=Europe/Paris:20261105T100000\r\n\
         DTEND;TZID=Europe/Paris:20261105T110000\r\n\
         SUMMARY:Old title\r\n\
         DESCRIPTION:Old description\r\n\
         RRULE:FREQ=WEEKLY;BYDAY=TH\r\n\
         EXDATE;TZID=Europe/Paris:20261112T100000\r\n\
         CATEGORIES:Work,Important\r\n\
         ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT:mailto:alice@example.com\r\n\
         X-OXICLOUD-TEST:keep-me\r\n\
         BEGIN:VALARM\r\n\
         ACTION:DISPLAY\r\n\
         DESCRIPTION:Alarm text must survive\r\n\
         TRIGGER:-PT15M\r\n\
         END:VALARM\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    }

    /// The SPA's edit path must not reduce a CalDAV-created event to the
    /// fields the JSON DTO models. Regression pin: update used to call
    /// `build_event_ical`, which dropped ATTENDEE, VALARM, CATEGORIES,
    /// EXDATE and X-* the moment a user renamed the event.
    #[test]
    fn update_keeps_unmanaged_properties_and_value_form() {
        let merged =
            merge_event_ical(stored_rich_event(), "abc@oxicloud", &body(), true, None).unwrap();

        assert!(merged.contains("DTSTART;TZID=Europe/Paris:20261105T100000\r\n"));
        assert!(merged.contains("DTEND;TZID=Europe/Paris:20261105T110000\r\n"));
        assert!(merged.contains("SUMMARY:Team sync\r\n"));
        assert!(!merged.contains("SUMMARY:Old title"));
        assert!(
            merged.contains("ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT:mailto:alice@example.com\r\n")
        );
        assert!(merged.contains("CATEGORIES:Work,Important\r\n"));
        assert!(merged.contains("EXDATE;TZID=Europe/Paris:20261112T100000\r\n"));
        assert!(merged.contains("X-OXICLOUD-TEST:keep-me\r\n"));
        assert!(merged.contains("BEGIN:VALARM\r\n"));
        assert!(merged.contains("DESCRIPTION:Alarm text must survive\r\n"));
    }

    /// A changed time cannot keep the old TZID-qualified value, but the
    /// unmanaged properties still survive the rewrite.
    #[test]
    fn update_with_new_times_replaces_tzid_value() {
        let merged =
            merge_event_ical(stored_rich_event(), "abc@oxicloud", &body(), false, None).unwrap();

        assert!(merged.contains("DTSTART:20261105T090000Z\r\n"));
        assert!(merged.contains("DTEND:20261105T100000Z\r\n"));
        assert!(!merged.contains("DTSTART;TZID=Europe/Paris"));
        assert!(merged.contains("ATTENDEE;CN=Alice"));
        assert!(merged.contains("BEGIN:VALARM"));
    }

    /// Clearing the RRULE removes EXDATE/RDATE (they no longer describe
    /// an existing series) while leaving unrelated properties intact.
    #[test]
    fn clearing_recurrence_drops_exdate_but_keeps_alarms() {
        let mut changed = body();
        changed.rrule = None;
        let merged =
            merge_event_ical(stored_rich_event(), "abc@oxicloud", &changed, true, None).unwrap();

        assert!(!merged.contains("RRULE:"));
        assert!(!merged.contains("EXDATE"));
        assert!(merged.contains("ATTENDEE;CN=Alice"));
        assert!(merged.contains("BEGIN:VALARM"));
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
        let ics = build_event_ical("abc@oxicloud", None, &long, None);
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
