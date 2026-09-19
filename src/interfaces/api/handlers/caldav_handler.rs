/**
 * CalDAV Handler Module
 *
 * This module implements the CalDAV protocol (RFC 4791) endpoints for OxiCloud.
 * It provides calendar access and management through standard CalDAV methods,
 * allowing clients like Thunderbird, Apple Calendar, and GNOME Calendar to sync.
 *
 * Supported methods:
 * - OPTIONS: Advertise CalDAV capabilities
 * - PROPFIND: List calendars and their properties
 * - REPORT: Query events (calendar-query, calendar-multiget)
 * - MKCALENDAR: Create a new calendar
 * - PUT: Create/update calendar events (.ics)
 * - GET: Retrieve calendar event data
 * - DELETE: Remove calendars or events
 * - PROPPATCH: Modify calendar properties
 */
use axum::{
    Router,
    body::{self, Body},
    http::{HeaderName, Request, StatusCode, header},
    response::Response,
};
use bytes::{Buf, Bytes};
use percent_encoding::percent_decode_str;
use quick_xml::Writer;
use std::fmt::Write;
use std::sync::Arc;

use crate::application::adapters::caldav_adapter::{
    CalDavAdapter, CalDavReportType, CalendarObjectRow, bundle_to_calendar_body,
    extract_object_chunk, extract_vtimezone_chunks, group_objects_by_uid, vtimezone_tzid,
};
use crate::application::adapters::uid_from_multiget_href;
use crate::application::adapters::webdav_adapter::{PropFindRequest, PropFindType};
use crate::application::dtos::calendar_dto::{
    CalendarEventDto, CalendarTodoDto, CreateCalendarDto, CreateEventICalDto, UpdateCalendarDto,
};
use crate::application::ports::calendar_ports::{CalendarObject, CalendarUseCase};
use crate::application::services::calendar_service::CalendarService;
use crate::common::di::AppState;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

const HEADER_DAV: HeaderName = HeaderName::from_static("dav");

/// Maximum allowed request body size for CalDAV XML/iCal endpoints (1 MB).
/// Prevents OOM/DoS via unbounded body buffering.
const MAX_CALDAV_BODY: usize = 1_048_576;

/// Minimum rows per emitted page for the streaming CalDAV emitters.
/// Pages only cut at UID boundaries (the cursor delivers same-UID rows
/// adjacent), so a master + its exception overrides always land in one
/// chunk and peak memory is one page of DTOs + its XML instead of the
/// whole calendar twice.
const CALDAV_STREAM_PAGE_EVENTS: usize = 500;

/// Which stored calendar-object kinds a REPORT targets, derived from
/// the parsed `comp-filter` (#754).
#[derive(Debug, Clone, Copy, PartialEq)]
enum CompTarget {
    /// `<C:comp-filter name="VEVENT">` (or an unknown/unsupported
    /// component name — the pre-comp-filter-awareness behaviour).
    EventsOnly,
    /// `<C:comp-filter name="VTODO">`.
    TodosOnly,
    /// No component-level filter — RFC 4791 §9.7: match every stored
    /// kind. Multiget and sync-collection always take this branch
    /// (hrefs address objects of either kind).
    All,
}

fn comp_target(comp: Option<&str>) -> CompTarget {
    match comp {
        Some("VTODO") => CompTarget::TodosOnly,
        Some("VEVENT") => CompTarget::EventsOnly,
        None => CompTarget::All,
        // Unknown component kinds (VJOURNAL, VFREEBUSY — not stored)
        // fall back to the events surface.
        Some(_) => CompTarget::EventsOnly,
    }
}

/// Generic UID-boundary pager: drains `rows` into pages cut at UID
/// boundaries (the cursor delivers same-UID rows adjacent, so a
/// master + its exception overrides always land in one chunk),
/// renders each page with `render`, and yields the chunks. Shared by
/// the streaming REPORT / collection-PROPFIND / whole-calendar-ICS
/// builders so events and tasks flow through identical plumbing
/// (#754); peak memory stays one page of DTOs + its rendered chunk.
fn paged_uid_chunks<T, S, F>(
    rows: S,
    mut render: F,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>>
where
    S: futures::Stream<Item = Result<T, crate::common::errors::DomainError>> + Unpin,
    F: FnMut(&[T]) -> Result<Vec<u8>, std::io::Error>,
    T: CalendarObjectRow,
{
    async_stream::try_stream! {
        let mut rows = rows;
        let mut page: Vec<T> = Vec::with_capacity(CALDAV_STREAM_PAGE_EVENTS + 32);
        loop {
            use futures::TryStreamExt;
            let next = rows
                .try_next()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let flush = match &next {
                Some(obj) => {
                    page.len() >= CALDAV_STREAM_PAGE_EVENTS
                        && page
                            .last()
                            .is_some_and(|p| p.row_ical_uid() != obj.row_ical_uid())
                }
                None => !page.is_empty(),
            };
            if flush {
                let chunk = render(&page)?;
                page.clear();
                yield Bytes::from(chunk);
            }
            match next {
                Some(obj) => page.push(obj),
                None => break,
            }
        }
    }
}

/// Streamed multistatus REPORT: header chunk, one chunk per hydrated
/// UID page, footer chunk. Byte-compatible with the buffered
/// `generate_calendar_events_response` output (same bundle order:
/// `(MIN(start_time), uid)` = first appearance in the start_time
/// listing). TTFB becomes the first page instead of the full
/// generation; the whole-calendar DTO Vec is never materialised.
///
/// `target` decides which stored kinds stream (#754): events, tasks,
/// or both (events first, then tasks — multistatus member order is
/// not significant).
fn build_streaming_report_response(
    calendar_service: Arc<CalendarService>,
    calendar_id: String,
    report: CalDavReportType,
    base_href: String,
    user_id: uuid::Uuid,
    target: CompTarget,
) -> Response<Body> {
    let stream = async_stream::try_stream! {
        let mut buf = Vec::with_capacity(256);
        {
            let mut w = Writer::new(&mut buf);
            CalDavAdapter::write_caldav_multistatus_start(&mut w)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        yield Bytes::from(buf);

        use futures::TryStreamExt;

        // ONE server-side scan+sort in bundle order streamed through a
        // cursor per kind — the same aggregate work the buffered path
        // paid, but only a page of rows resident. The render closures
        // own their clones of `report` / `base_href` so the pager
        // streams are self-contained ('static).
        if target != CompTarget::TodosOnly {
            let rows = calendar_service
                .stream_events_uid_order(&calendar_id, user_id)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut chunks = Box::pin(paged_uid_chunks(rows, {
                let report = report.clone();
                let base_href = base_href.clone();
                move |page: &[CalendarEventDto]| {
                    let mut chunk = Vec::with_capacity(page.len() * 1024 + 128);
                    let mut w = Writer::new(&mut chunk);
                    CalDavAdapter::write_report_page(&mut w, page, &report, &base_href)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    Ok(chunk)
                }
            }));
            while let Some(chunk) = chunks.try_next().await? {
                yield chunk;
            }
        }

        if target != CompTarget::EventsOnly {
            let rows = calendar_service
                .stream_todos_uid_order(&calendar_id, user_id)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut chunks = Box::pin(paged_uid_chunks(rows, {
                let report = report.clone();
                let base_href = base_href.clone();
                move |page: &[CalendarTodoDto]| {
                    let mut chunk = Vec::with_capacity(page.len() * 1024 + 128);
                    let mut w = Writer::new(&mut chunk);
                    CalDavAdapter::write_report_page(&mut w, page, &report, &base_href)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    Ok(chunk)
                }
            }));
            while let Some(chunk) = chunks.try_next().await? {
                yield chunk;
            }
        }

        let mut buf = Vec::with_capacity(32);
        {
            let mut w = Writer::new(&mut buf);
            CalDavAdapter::write_caldav_multistatus_end(&mut w)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        yield Bytes::from(buf);
    };

    use futures::TryStreamExt;
    let stream = stream
        .map_err(|e: std::io::Error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) });

    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Streamed depth-1 collection PROPFIND: head (multistatus + the
/// calendar's own response), one chunk per hydrated UID page, footer.
#[allow(clippy::too_many_arguments)]
fn build_streaming_collection_propfind(
    calendar_service: Arc<CalendarService>,
    calendar: crate::application::dtos::calendar_dto::CalendarDto,
    propfind_request: PropFindRequest,
    calendar_id: String,
    base_href: String,
    caller_id: String,
    user_id: uuid::Uuid,
) -> Response<Body> {
    let stream = async_stream::try_stream! {
        let mut buf = Vec::with_capacity(2048);
        {
            let mut w = Writer::new(&mut buf);
            CalDavAdapter::write_collection_head(&mut w, &calendar, &propfind_request, &base_href, &caller_id)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        yield Bytes::from(buf);

        // Member resources — events first, then tasks (#754). Both
        // kinds must appear here: DAVx⁵ discovers sync candidates via
        // depth-1 PROPFIND getetag, and a task missing from the
        // listing would never sync.
        {
            use futures::TryStreamExt;

            let event_rows = calendar_service
                .stream_events_uid_order(&calendar_id, user_id)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut event_chunks = Box::pin(paged_uid_chunks(event_rows, {
                let base_href = base_href.clone();
                move |page: &[CalendarEventDto]| {
                    let mut chunk = Vec::with_capacity(page.len() * 512 + 128);
                    let mut w = Writer::new(&mut chunk);
                    CalDavAdapter::write_collection_event_page(&mut w, page, &base_href)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    Ok(chunk)
                }
            }));
            while let Some(chunk) = event_chunks.try_next().await? {
                yield chunk;
            }

            let todo_rows = calendar_service
                .stream_todos_uid_order(&calendar_id, user_id)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut todo_chunks = Box::pin(paged_uid_chunks(todo_rows, {
                let base_href = base_href.clone();
                move |page: &[CalendarTodoDto]| {
                    let mut chunk = Vec::with_capacity(page.len() * 512 + 128);
                    let mut w = Writer::new(&mut chunk);
                    CalDavAdapter::write_collection_event_page(&mut w, page, &base_href)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    Ok(chunk)
                }
            }));
            while let Some(chunk) = todo_chunks.try_next().await? {
                yield chunk;
            }
        }

        let mut buf = Vec::with_capacity(32);
        {
            let mut w = Writer::new(&mut buf);
            CalDavAdapter::write_caldav_multistatus_end(&mut w)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        yield Bytes::from(buf);
    };

    use futures::TryStreamExt;
    let stream = stream
        .map_err(|e: std::io::Error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) });

    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Render one UID page of the whole-calendar `.ics` stream: each
/// row's stored component chunk verbatim, with VTIMEZONE blocks
/// deduped through the stream-wide `seen_tzids` set (#689). Shared by
/// the event and task sections (#754).
fn render_ics_page<T: CalendarObjectRow>(
    page: &[T],
    seen_tzids: &std::sync::Mutex<std::collections::HashSet<String>>,
) -> Vec<u8> {
    let mut chunk = String::with_capacity(page.len() * 384);
    for group in group_objects_by_uid(page) {
        for row in group {
            for vtz in extract_vtimezone_chunks(row.row_ical_data()) {
                let key = vtimezone_tzid(vtz).unwrap_or(vtz);
                // The lock is per-page and uncontended — a poisoned
                // mutex can't occur here (no panics while held).
                if let Ok(mut seen) = seen_tzids.lock()
                    && seen.insert(key.to_string())
                {
                    chunk.push_str(vtz);
                    if !chunk.ends_with('\n') {
                        chunk.push_str("\r\n");
                    }
                }
            }
            if let Some(comp) = extract_object_chunk(row) {
                chunk.push_str(comp);
                if !chunk.ends_with('\n') {
                    chunk.push_str("\r\n");
                }
            }
        }
    }
    chunk.into_bytes()
}

/// Streamed whole-calendar `.ics` GET: VCALENDAR header, one chunk per
/// hydrated UID page (each row's stored component chunk served
/// verbatim — events first, then tasks, #754), `END:VCALENDAR`
/// footer. VTIMEZONE blocks embedded in the rows' shells are re-emitted
/// ONCE per TZID across the WHOLE stream (both kinds share one dedupe
/// set, so a zone referenced by an event and a task alike still
/// appears exactly once).
fn build_streaming_calendar_ics(
    calendar_service: Arc<CalendarService>,
    calendar_id: String,
    calendar_name: String,
    calendar_etag: String,
    user_id: uuid::Uuid,
) -> Response<Body> {
    let stream = async_stream::try_stream! {
        let mut head = String::with_capacity(128);
        let _ = write!(
            head,
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\nX-WR-CALNAME:{}\r\n",
            calendar_name
        );
        yield Bytes::from(head);

        // VTIMEZONE dedupe across the whole stream (#689): rows
        // written after the fix each embed the PUT body's zone
        // definitions in their shell; emit each TZID once, the
        // first time it shows up. Blocks may appear AFTER earlier
        // pages' components — component order inside a VCALENDAR is
        // not significant (RFC 5545 §3.4) and every real client
        // parser (ical4j, libical, vobject) accepts that. Shared
        // across the event and task sections via Arc<Mutex>.
        let seen_tzids = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashSet::<String>::new(),
        ));

        {
            use futures::TryStreamExt;

            let event_rows = calendar_service
                .stream_events_uid_order(&calendar_id, user_id)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut event_chunks = Box::pin(paged_uid_chunks(event_rows, {
                let seen_tzids = seen_tzids.clone();
                move |page: &[CalendarEventDto]| Ok(render_ics_page(page, &seen_tzids))
            }));
            while let Some(chunk) = event_chunks.try_next().await? {
                yield chunk;
            }

            let todo_rows = calendar_service
                .stream_todos_uid_order(&calendar_id, user_id)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut todo_chunks = Box::pin(paged_uid_chunks(todo_rows, {
                let seen_tzids = seen_tzids.clone();
                move |page: &[CalendarTodoDto]| Ok(render_ics_page(page, &seen_tzids))
            }));
            while let Some(chunk) = todo_chunks.try_next().await? {
                yield chunk;
            }
        }

        yield Bytes::from_static(b"END:VCALENDAR\r\n");
    };

    use futures::TryStreamExt;
    let stream = stream
        .map_err(|e: std::io::Error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8")
        .header(header::ETAG, format!("\"{}\"", calendar_etag))
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Creates CalDAV routes with full path prefixes.
///
/// Uses `merge()` instead of `nest()` to avoid Axum's trailing-slash routing gap.
/// Registers `/caldav`, `/caldav/`, and `/caldav/{*path}` explicitly.
pub fn caldav_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/caldav/{*path}", axum::routing::any(handle_caldav_methods))
        .route("/caldav/", axum::routing::any(handle_caldav_methods_root))
        .route("/caldav", axum::routing::any(handle_caldav_methods_root))
}

/// Creates RFC 6764 well-known discovery routes.
/// These are public (no auth) and simply redirect to the CalDAV root.
pub fn well_known_routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/.well-known/caldav",
        axum::routing::any(handle_well_known_caldav),
    )
}

async fn handle_well_known_caldav() -> Response<Body> {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(
            header::LOCATION,
            format!("{}/caldav/", crate::common::config::server_base_path()),
        )
        .body(Body::empty())
        .unwrap()
}

async fn handle_caldav_methods_root(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    handle_caldav_methods_inner(state, req, String::new()).await
}

async fn handle_caldav_methods(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let uri = req.uri().clone();
    let path = extract_caldav_path(uri.path());
    reject_path_traversal(&path)?;
    handle_caldav_methods_inner(state, req, path).await
}

async fn handle_caldav_methods_inner(
    state: Arc<AppState>,
    req: Request<Body>,
    path: String,
) -> Result<Response<Body>, AppError> {
    let method = req.method().clone();

    match method.as_str() {
        "OPTIONS" => handle_options().await,
        "PROPFIND" => handle_propfind(state, req, &path).await,
        "REPORT" => handle_report(state, req, &path).await,
        "MKCALENDAR" => handle_mkcalendar(state, req, &path).await,
        "PUT" => handle_put(state, req, &path).await,
        "GET" => handle_get(state, req, &path).await,
        "DELETE" => handle_delete(state, req, &path).await,
        "PROPPATCH" => handle_proppatch(state, req, &path).await,
        _ => Err(AppError::method_not_allowed(format!(
            "Method not allowed: {}",
            method
        ))),
    }
}

/// Extract the CalDAV path from the full URI path, percent-decoding the result.
fn extract_caldav_path(uri_path: &str) -> String {
    let encoded = if let Some(pos) = uri_path.find("/caldav/") {
        let after = &uri_path[pos + 8..];
        after.trim_end_matches('/')
    } else if uri_path.ends_with("/caldav") {
        ""
    } else {
        uri_path.trim_start_matches('/').trim_end_matches('/')
    };
    percent_decode_str(encoded).decode_utf8_lossy().into_owned()
}

/// Reject paths that contain path-traversal segments (`.` or `..`).
fn reject_path_traversal(path: &str) -> Result<(), AppError> {
    for segment in path.split('/') {
        if segment == ".." || segment == "." {
            return Err(AppError::bad_request(
                "Path must not contain '.' or '..' segments",
            ));
        }
    }
    Ok(())
}

// ─── Helper: strip optional username prefix from CalDAV path ─────────
//
// The `calendar-home-set` discovery property returns `/caldav/{username}/`,
// so standard clients (DAVx5, Apple Calendar, Thunderbird) will prefix all
// subsequent requests with the username segment.  The handlers below expect
// paths of the form `{calendar_id}` or `{calendar_id}/{event}.ics`, so we
// need to detect and strip the leading username when present.
//
// Heuristic: if the first path segment is a valid UUID it is already a
// calendar ID; otherwise treat it as a username and skip it.

fn strip_username_prefix(path: &str) -> &str {
    if let Some(pos) = path.find('/') {
        let first = &path[..pos];
        if uuid::Uuid::parse_str(first).is_ok() {
            // First segment is a UUID → no username prefix
            path
        } else {
            // First segment is not a UUID → treat as username, return the rest
            &path[pos + 1..]
        }
    } else {
        // Single segment (no slash)
        if uuid::Uuid::parse_str(path).is_ok() {
            path
        } else {
            // Single non-UUID segment (bare username) → nothing useful after it
            ""
        }
    }
}

// ─── Helper: extract user from request ───────────────────────────────

/// Delegates to the shared helper — this surface receives a raw `Request`
/// and so never passes through `AuthUser`'s `FromRequestParts` guard.
fn extract_user(req: &Request<Body>) -> Result<AuthUser, AppError> {
    crate::interfaces::middleware::auth::auth_user_from_extensions(req.extensions())
}

fn get_calendar_service(state: &AppState) -> Result<&Arc<CalendarService>, AppError> {
    state.calendar_use_case.as_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "CalDAV service is not configured",
            "NotImplemented",
        )
    })
}

// ─── OPTIONS ─────────────────────────────────────────────────────────

async fn handle_options() -> Result<Response<Body>, AppError> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(HEADER_DAV, "1, 2, calendar-access")
        .header(
            header::ALLOW,
            "OPTIONS, GET, PUT, DELETE, PROPFIND, PROPPATCH, REPORT, MKCALENDAR",
        )
        .body(Body::empty())
        .unwrap())
}

// ─── PROPFIND ────────────────────────────────────────────────────────

async fn handle_propfind(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let depth = req
        .headers()
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1")
        .to_string();

    let user = extract_user(&req)?;
    // Caller UUID (string form) — gates the `<D:write/>` privilege on calendars
    // the caller owns, so clients mount their own calendars read-write.
    let caller_id = user.id.to_string();
    let calendar_service = get_calendar_service(&state)?;

    let body_bytes = body::to_bytes(req.into_body(), MAX_CALDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    // Parse PROPFIND request
    let propfind_request = if body_bytes.is_empty() {
        PropFindRequest {
            prop_find_type: PropFindType::AllProp,
        }
    } else {
        crate::application::adapters::webdav_adapter::WebDavAdapter::parse_propfind(
            body_bytes.reader(),
        )
        .map_err(|e| AppError::bad_request(format!("Failed to parse PROPFIND: {}", e)))?
    };

    if path.is_empty() {
        // Root CalDAV path — return discovery properties + list user's calendars
        // At depth 0, only return root entry; at depth 1+, also include calendars
        let calendars = if depth == "0" {
            vec![]
        } else {
            calendar_service
                .list_my_calendars(user.id)
                .await
                .map_err(AppError::from)?
        };

        let base_href = &format!("{}/caldav/", crate::common::config::server_base_path());
        let mut response_body = Vec::new();
        CalDavAdapter::generate_root_propfind_response(
            &mut response_body,
            &calendars,
            &propfind_request,
            base_href,
            &user.username,
            &caller_id,
        )
        .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

        Ok(Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Body::from(response_body))
            .unwrap())
    } else if path.starts_with("principals/") || path == "principals" {
        // Principal resource — return user principal properties
        let username = path.strip_prefix("principals/").unwrap_or(&user.username);
        let username = if username.is_empty() {
            &user.username
        } else {
            username
        };

        let mut response_body = Vec::new();
        CalDavAdapter::generate_principal_propfind_response(
            &mut response_body,
            &propfind_request,
            username,
        )
        .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

        Ok(Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(Body::from(response_body))
            .unwrap())
    } else {
        // Path could be:
        //   {username}                        — user calendar home (from calendar-home-set)
        //   {calendar_id}                     — calendar collection
        //   {calendar_id}/{event_uid}.ics     — individual event
        //   {username}/{calendar_id}          — calendar under user home
        //   {username}/{calendar_id}/{uid}.ics — event under user home
        //
        // Use strip_username_prefix heuristic: if first segment is a UUID
        // it's a calendar ID, otherwise it's a username prefix.
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        let first_segment = parts[0];
        let first_is_uuid = uuid::Uuid::parse_str(first_segment).is_ok();

        if parts.len() == 1 {
            // Single path segment: UUID means calendar ID, otherwise user home
            let calendar_result = if first_is_uuid {
                calendar_service.get_calendar(first_segment, user.id).await
            } else {
                Err(crate::domain::errors::DomainError::new(
                    crate::domain::errors::ErrorKind::NotFound,
                    "Calendar",
                    "Not a UUID",
                ))
            };

            if let Ok(calendar) = calendar_result {
                // Valid calendar ID — return calendar collection.
                // Depth-1 streams the event listing page by page
                // (whole-calendar responses used to materialise every
                // DTO + the full multistatus in RAM); depth-0 has no
                // event section and keeps the tiny buffered path.
                if depth != "0" {
                    let base_href = format!(
                        "{}/caldav/{}/",
                        crate::common::config::server_base_path(),
                        first_segment
                    );
                    return Ok(build_streaming_collection_propfind(
                        calendar_service.clone(),
                        calendar,
                        propfind_request,
                        first_segment.to_string(),
                        base_href,
                        caller_id.clone(),
                        user.id,
                    ));
                }

                let base_href = &format!(
                    "{}/caldav/{}/",
                    crate::common::config::server_base_path(),
                    first_segment
                );
                let mut response_body = Vec::new();

                CalDavAdapter::generate_calendar_collection_propfind(
                    &mut response_body,
                    &calendar,
                    &[],
                    &propfind_request,
                    base_href,
                    &depth,
                    &caller_id,
                )
                .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

                Ok(Response::builder()
                    .status(StatusCode::MULTI_STATUS)
                    .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                    .body(Body::from(response_body))
                    .unwrap())
            } else if first_is_uuid {
                // Path segment IS a UUID but the calendar isn't
                // accessible to the caller — could be another
                // owner's calendar or genuinely missing. Return
                // 404 (anti-enum, matches every other OxiCloud
                // surface post-D7). The pre-Round-3 fall-through
                // silently listed the caller's OWN calendars,
                // which was misleading (the URL claimed one calendar,
                // response returned unrelated ones) and violated
                // the anti-enumeration contract audited in
                // `docs/plan/authz_audit/caldav_carddav_wopi.md`.
                Err(AppError::not_found("Calendar not found"))
            } else {
                // Not a calendar ID — treat as user calendar home (e.g. /caldav/{username}/)
                // List all calendars for this user
                let calendars = calendar_service
                    .list_my_calendars(user.id)
                    .await
                    .map_err(AppError::from)?;

                let base_href = &format!(
                    "{}/caldav/{}/",
                    crate::common::config::server_base_path(),
                    first_segment
                );
                let mut response_body = Vec::new();

                CalDavAdapter::generate_calendars_propfind_response(
                    &mut response_body,
                    &calendars,
                    &propfind_request,
                    base_href,
                    &caller_id,
                )
                .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

                Ok(Response::builder()
                    .status(StatusCode::MULTI_STATUS)
                    .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                    .body(Body::from(response_body))
                    .unwrap())
            }
        } else {
            // Multi-segment path: {something}/{rest}
            let rest = parts[1];

            // Use UUID heuristic: if first segment is a UUID it's a calendar ID
            let (calendar_id, event_path) = if first_is_uuid {
                // first_segment is a calendar ID, rest is event path
                (first_segment, rest)
            } else {
                // first_segment may be a username, rest could be {calendar_id} or
                // {calendar_id}/{event}.ics
                let sub_parts: Vec<&str> = rest.splitn(2, '/').collect();
                if sub_parts.len() == 1 {
                    // /caldav/{username}/{calendar_id}
                    // Try to get this as a calendar collection
                    let cal = calendar_service
                        .get_calendar(sub_parts[0], user.id)
                        .await
                        .map_err(|e| AppError::not_found(format!("Calendar not found: {}", e)))?;

                    // Same streaming/buffered split as the
                    // single-segment collection branch above.
                    if depth != "0" {
                        let base_href = format!(
                            "{}/caldav/{}/{}/",
                            crate::common::config::server_base_path(),
                            first_segment,
                            sub_parts[0]
                        );
                        return Ok(build_streaming_collection_propfind(
                            calendar_service.clone(),
                            cal,
                            propfind_request,
                            sub_parts[0].to_string(),
                            base_href,
                            caller_id.clone(),
                            user.id,
                        ));
                    }

                    let base_href = &format!(
                        "{}/caldav/{}/{}/",
                        crate::common::config::server_base_path(),
                        first_segment,
                        sub_parts[0]
                    );
                    let mut response_body = Vec::new();

                    CalDavAdapter::generate_calendar_collection_propfind(
                        &mut response_body,
                        &cal,
                        &[],
                        &propfind_request,
                        base_href,
                        &depth,
                        &caller_id,
                    )
                    .map_err(|e| {
                        AppError::internal_error(format!("Failed to generate XML: {}", e))
                    })?;

                    return Ok(Response::builder()
                        .status(StatusCode::MULTI_STATUS)
                        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                        .body(Body::from(response_body))
                        .unwrap());
                } else {
                    // /caldav/{username}/{calendar_id}/{event}.ics
                    (sub_parts[0], sub_parts[1])
                }
            };

            // Individual object .ics — indexed lookup by iCalendar UID.
            // An .ics may hold either component kind; the service
            // resolves which, behind a single Read gate.
            let ical_uid = event_path.trim_end_matches(".ics");

            let object = calendar_service
                .get_object_by_ical_uid(calendar_id, ical_uid, user.id)
                .await
                .map_err(AppError::from)?
                .ok_or_else(|| {
                    AppError::not_found(format!("Calendar object not found: {}", ical_uid))
                })?;

            let base_href = &format!(
                "{}/caldav/{}/",
                crate::common::config::server_base_path(),
                calendar_id
            );
            let report_type = CalDavReportType::CalendarMultiget {
                hrefs: vec![format!("{}{}.ics", base_href, ical_uid)],
                props: vec![],
            };

            // Both variants implement `CalendarObjectRow`, so the
            // serializer is already generic over the kind — the match
            // exists only to pick the concrete slice type.
            let mut response_body = Vec::new();
            let xml = match &object {
                CalendarObject::Event(event) => CalDavAdapter::generate_calendar_events_response(
                    &mut response_body,
                    std::slice::from_ref(event),
                    &report_type,
                    base_href,
                ),
                CalendarObject::Todo(todo) => CalDavAdapter::generate_calendar_events_response(
                    &mut response_body,
                    std::slice::from_ref(todo),
                    &report_type,
                    base_href,
                ),
            };
            xml.map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

            Ok(Response::builder()
                .status(StatusCode::MULTI_STATUS)
                .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(Body::from(response_body))
                .unwrap())
        }
    }
}

// ─── REPORT ──────────────────────────────────────────────────────────

async fn handle_report(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let calendar_service = get_calendar_service(&state)?;

    let body_bytes = body::to_bytes(req.into_body(), MAX_CALDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    let report = CalDavAdapter::parse_report(body_bytes.reader())
        .map_err(|e| AppError::bad_request(format!("Failed to parse REPORT: {}", e)))?;

    let effective_path = strip_username_prefix(path);
    let calendar_id = effective_path.split('/').next().unwrap_or(effective_path);

    if calendar_id.is_empty() {
        return Err(AppError::bad_request("Calendar ID required in path"));
    }

    // Whole-calendar shapes (no-range calendar-query, sync-collection)
    // stream: header + one chunk per hydrated UID page + footer, instead
    // of materialising every DTO AND the full multistatus in RAM with
    // TTFB = complete generation. Bounded shapes (time-range query,
    // multiget) keep the buffered path.
    let target = match &report {
        CalDavReportType::CalendarQuery { comp, .. } => comp_target(comp.as_deref()),
        // Multiget addresses objects by href and sync-collection covers
        // the whole collection — both kinds always (#754).
        _ => CompTarget::All,
    };

    if matches!(
        &report,
        CalDavReportType::CalendarQuery {
            time_range: None,
            ..
        } | CalDavReportType::SyncCollection { .. }
    ) {
        // Surface not-found / authz before committing to a 207 stream.
        calendar_service
            .get_calendar(calendar_id, user.id)
            .await
            .map_err(AppError::from)?;
        let base_href = format!(
            "{}/caldav/{}/",
            crate::common::config::server_base_path(),
            calendar_id
        );
        return Ok(build_streaming_report_response(
            calendar_service.clone(),
            calendar_id.to_string(),
            report,
            base_href,
            user.id,
            target,
        ));
    }

    let mut events: Vec<CalendarEventDto> = Vec::new();
    let mut todos: Vec<CalendarTodoDto> = Vec::new();
    match &report {
        CalDavReportType::CalendarQuery { time_range, .. } => {
            if let Some((start, end)) = time_range {
                if target != CompTarget::TodosOnly {
                    events = calendar_service
                        .get_events_in_range(calendar_id, *start, *end, user.id)
                        .await
                        .map_err(AppError::from)?;
                }
                if target != CompTarget::EventsOnly {
                    todos = calendar_service
                        .get_todos_in_range(calendar_id, *start, *end, user.id)
                        .await
                        .map_err(AppError::from)?;
                }
            } else {
                unreachable!("no-range calendar-query streams above")
            }
        }
        CalDavReportType::CalendarMultiget { hrefs, .. } => {
            // Indexed batch lookup (`ical_uid = ANY(...)`) on BOTH
            // object tables — an .ics href may address an event or a
            // task (#754); a multiget for a handful of objects must
            // not pay for listing the whole calendar and filtering
            // client-side.
            let uids: Vec<String> = hrefs
                .iter()
                .filter_map(|href| uid_from_multiget_href(href, ".ics"))
                .collect();

            events = calendar_service
                .get_events_by_ical_uids(calendar_id, &uids, user.id)
                .await
                .map_err(AppError::from)?;
            todos = calendar_service
                .get_todos_by_ical_uids(calendar_id, &uids, user.id)
                .await
                .map_err(AppError::from)?;
        }
        CalDavReportType::SyncCollection { .. } => {
            unreachable!("sync-collection streams above")
        }
    };

    let base_href = &format!(
        "{}/caldav/{}/",
        crate::common::config::server_base_path(),
        calendar_id
    );
    // One multistatus carrying both kinds: events first, then tasks.
    // `write_report_page` over an empty slice emits nothing, so the
    // kinds not targeted above cost no bytes.
    let mut response_body = Vec::new();
    {
        let mut xml_writer = Writer::new(&mut response_body);
        CalDavAdapter::write_caldav_multistatus_start(&mut xml_writer)
            .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;
        CalDavAdapter::write_report_page(&mut xml_writer, &events, &report, base_href)
            .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;
        CalDavAdapter::write_report_page(&mut xml_writer, &todos, &report, base_href)
            .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;
        CalDavAdapter::write_caldav_multistatus_end(&mut xml_writer)
            .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;
    }

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(response_body))
        .unwrap())
}

// ─── MKCALENDAR ──────────────────────────────────────────────────────

async fn handle_mkcalendar(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let calendar_service = get_calendar_service(&state)?;

    let body_bytes = body::to_bytes(req.into_body(), MAX_CALDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    let (name, description, color) = if body_bytes.is_empty() {
        let name = path
            .split('/')
            .next_back()
            .unwrap_or("New Calendar")
            .to_string();
        (name, None, None)
    } else {
        CalDavAdapter::parse_mkcalendar(body_bytes.reader())
            .map_err(|e| AppError::bad_request(format!("Failed to parse MKCALENDAR: {}", e)))?
    };

    let create_dto = CreateCalendarDto {
        name,
        description,
        color,
        is_public: Some(false),
    };

    // See the comment above create_event_from_ical for why this uses
    // `AppError::from` (kind-aware mapping) instead of `internal_error`.
    calendar_service
        .create_calendar(create_dto, user.id)
        .await
        .map_err(AppError::from)?;

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .unwrap())
}

// ─── PUT (.ics) ──────────────────────────────────────────────────────

async fn handle_put(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let calendar_service = get_calendar_service(&state)?;

    let effective_path = strip_username_prefix(path);
    let parts: Vec<&str> = effective_path.splitn(2, '/').collect();
    if parts.len() < 2 {
        return Err(AppError::bad_request(
            "Path must be {calendar_id}/{uid}.ics",
        ));
    }

    let calendar_id = parts[0];

    let body_bytes = body::to_bytes(req.into_body(), MAX_CALDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    let ical_data = String::from_utf8(body_bytes.to_vec())
        .map_err(|e| AppError::bad_request(format!("Invalid UTF-8 in iCalendar data: {}", e)))?;

    // Route the PUT through `upsert_ical_objects` so a body carrying a
    // master + N per-instance overrides (RFC 5545 §3.8.4.4 — the
    // Thunderbird / Apple Calendar / DAVx⁵ "modify one occurrence"
    // shape) persists each component to its own row instead of the last
    // one clobbering the master — VEVENTs to the events table, VTODOs
    // to the tasks table (#754). See AtalayaLabs/OxiCloud#528.
    //
    // Kind-aware error mapping (`AppError::from(DomainError)`):
    //   * `InvalidInput` → 400 (malformed iCal / missing DTSTART)
    //   * `NotFound`     → 404 (calendar doesn't exist / no perm)
    //   * `AccessDenied` → 403 (caller lacks Write on the calendar)
    //   * anything else  → 500 (genuine server bug)
    let create_dto = CreateEventICalDto {
        calendar_id: calendar_id.to_string(),
        ical_data,
    };

    let result = calendar_service
        .upsert_ical_objects(create_dto, user.id)
        .await
        .map_err(AppError::from)?;

    // The object surface still exposes a single resource per UID, so
    // we return an ETag anchored on the master row when present,
    // otherwise the first exception's id — events first, then todos
    // (#754); a mixed body keys off the event side, matching the
    // pre-#528 header contract for clients that only understand a
    // single ETag per PUT.
    let etag_source = result
        .events
        .iter()
        .find(|e| e.recurrence_id.is_none())
        .or_else(|| result.events.first())
        .map(|e| e.id.to_string())
        .or_else(|| {
            result
                .todos
                .iter()
                .find(|t| t.recurrence_id.is_none())
                .or_else(|| result.todos.first())
                .map(|t| t.id.to_string())
        })
        .unwrap_or_default();

    let status = if result.any_inserted {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    };

    Ok(Response::builder()
        .status(status)
        .header(header::ETAG, format!("\"{}\"", etag_source))
        .body(Body::empty())
        .unwrap())
}

// ─── GET (.ics) ──────────────────────────────────────────────────────

async fn handle_get(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let calendar_service = get_calendar_service(&state)?;

    let effective_path = strip_username_prefix(path);
    let parts: Vec<&str> = effective_path.splitn(2, '/').collect();
    let calendar_id = parts[0];

    if parts.len() < 2 {
        // GET on calendar collection — stream all events, folded
        // per UID so master + exception overrides live in ONE
        // VCALENDAR body per resource (RFC 4791 §4.1 + RFC 5545
        // §3.6.1). Each row's stored `ical_data` VEVENT chunk is
        // served verbatim; VTIMEZONE / VALARM / ATTENDEE /
        // CATEGORIES / X-* survive because the body is never
        // regenerated from DTO fields. Streaming (header + one
        // chunk per hydrated UID page + footer) replaces the old
        // whole-calendar String build.
        let calendar = calendar_service
            .get_calendar(calendar_id, user.id)
            .await
            .map_err(AppError::from)?;

        Ok(build_streaming_calendar_ics(
            calendar_service.clone(),
            calendar_id.to_string(),
            calendar.name,
            calendar.id,
            user.id,
        ))
    } else {
        // GET on individual object resource — fetch ALL rows for
        // this UID (master + any exception overrides) and emit
        // ONE calendar-object-resource containing every component.
        // This is the phase-4 fix: `get_event_by_ical_uid` is
        // master-only; using it here made exceptions invisible
        // to clients and their next-PUT would silently drop the
        // stored exception rows. Events are tried first, then
        // tasks (#754) — an .ics object may be either kind.
        let event_file = parts[1];
        let ical_uid = event_file.trim_end_matches(".ics");

        let bundle = calendar_service
            .get_events_by_ical_uids(calendar_id, &[ical_uid.to_string()], user.id)
            .await
            .map_err(AppError::from)?;

        let todos = if bundle.is_empty() {
            calendar_service
                .get_todos_by_ical_uids(calendar_id, &[ical_uid.to_string()], user.id)
                .await
                .map_err(AppError::from)?
        } else {
            Vec::new()
        };

        if bundle.is_empty() && todos.is_empty() {
            return Err(AppError::not_found(format!(
                "Calendar object not found: {}",
                ical_uid
            )));
        }

        // Group so the master (recurrence_id None) sits first,
        // then flatten for the bundle emitter. ETag anchors on
        // the first row of the first group — that's the master
        // for a recurring object, or the sole row for a
        // non-recurring one. Stable across bundle contents so
        // If-Match on subsequent PUTs keys off the master's id.
        let (etag_source, ical) = if !bundle.is_empty() {
            let grouped = group_objects_by_uid(&bundle);
            let flat: Vec<&_> = grouped.into_iter().flatten().collect();
            (
                flat.first()
                    .map(|e| e.row_id().to_string())
                    .unwrap_or_default(),
                bundle_to_calendar_body(&flat),
            )
        } else {
            let grouped = group_objects_by_uid(&todos);
            let flat: Vec<&_> = grouped.into_iter().flatten().collect();
            (
                flat.first()
                    .map(|t| t.row_id().to_string())
                    .unwrap_or_default(),
                bundle_to_calendar_body(&flat),
            )
        };

        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8")
            .header(header::ETAG, format!("\"{}\"", etag_source))
            .body(Body::from(ical))
            .unwrap())
    }
}

// NOTE: the pre-phase-4 `generate_event_ical` + `write_vevent`
// helpers were removed. They regenerated the response body from
// DTO fields, which (a) silently dropped every property outside
// the DTO surface (ATTENDEE, VALARM, CATEGORIES, STATUS, X-*)
// and (b) never emitted RECURRENCE-ID on exception rows. The
// `bundle_to_calendar_body` path replaces both by serving each
// row's stored `ical_data` verbatim.

// ─── DELETE ──────────────────────────────────────────────────────────

async fn handle_delete(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let calendar_service = get_calendar_service(&state)?;

    let effective_path = strip_username_prefix(path);
    let parts: Vec<&str> = effective_path.splitn(2, '/').collect();
    let calendar_id = parts[0];

    if calendar_id.is_empty() {
        return Err(AppError::bad_request("Calendar ID required"));
    }

    if parts.len() < 2 {
        calendar_service
            .delete_calendar(calendar_id, user.id)
            .await
            .map_err(AppError::from)?;
    } else {
        let event_file = parts[1];
        let ical_uid = event_file.trim_end_matches(".ics");

        // Indexed lookup by iCalendar UID instead of listing the calendar.
        // The service resolves which component kind the .ics holds, so
        // this path no longer repeats the try-event-then-todo dance.
        let object = calendar_service
            .get_object_by_ical_uid(calendar_id, ical_uid, user.id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| {
                AppError::not_found(format!("Calendar object not found: {}", ical_uid))
            })?;

        calendar_service
            .delete_object(&object, user.id)
            .await
            .map_err(AppError::from)?;
    }

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}

// ─── PROPPATCH ───────────────────────────────────────────────────────

async fn handle_proppatch(
    state: Arc<AppState>,
    req: Request<Body>,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let user = extract_user(&req)?;
    let calendar_service = get_calendar_service(&state)?;

    let body_bytes = body::to_bytes(req.into_body(), MAX_CALDAV_BODY)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read request body: {}", e)))?;

    let ops = crate::application::adapters::webdav_adapter::WebDavAdapter::parse_proppatch(
        body_bytes.reader(),
    )
    .map_err(|e| AppError::bad_request(format!("Failed to parse PROPPATCH: {}", e)))?;

    let effective_path = strip_username_prefix(path);
    let calendar_id = effective_path.split('/').next().unwrap_or(effective_path);

    if calendar_id.is_empty() {
        return Err(AppError::bad_request("Calendar ID required"));
    }

    let mut update = UpdateCalendarDto {
        name: None,
        description: None,
        color: None,
        is_public: None,
    };

    for op in &ops {
        if let crate::application::adapters::webdav_adapter::PropPatchOp::Set(prop) = op {
            match prop.name.name.as_str() {
                "displayname" => update.name = Some(prop.value.clone().unwrap_or_default()),
                "calendar-description" => update.description = prop.value.clone(),
                "calendar-color" => update.color = prop.value.clone(),
                _ => {}
            }
        }
    }

    if update.name.is_some() || update.description.is_some() || update.color.is_some() {
        calendar_service
            .update_calendar(calendar_id, update, user.id)
            .await
            .map_err(AppError::from)?;
    }

    let mut results = Vec::new();
    for op in &ops {
        match op {
            crate::application::adapters::webdav_adapter::PropPatchOp::Set(prop) => {
                results.push((&prop.name, true));
            }
            crate::application::adapters::webdav_adapter::PropPatchOp::Remove(name) => {
                results.push((name, true));
            }
        }
    }

    let href = format!(
        "{}/caldav/{}",
        crate::common::config::server_base_path(),
        path
    );
    let mut response_body = Vec::new();
    crate::application::adapters::webdav_adapter::WebDavAdapter::generate_proppatch_response(
        &mut response_body,
        &href,
        &results,
    )
    .map_err(|e| AppError::internal_error(format!("Failed to generate XML: {}", e)))?;

    Ok(Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(response_body))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::strip_username_prefix;

    #[test]
    fn test_strip_username_prefix_uuid_only() {
        let uuid = "ae8ae236-709f-4939-b766-37ad589ac7f2";
        assert_eq!(strip_username_prefix(uuid), uuid);
    }

    #[test]
    fn test_strip_username_prefix_uuid_with_event() {
        let path = "ae8ae236-709f-4939-b766-37ad589ac7f2/event.ics";
        assert_eq!(strip_username_prefix(path), path);
    }

    #[test]
    fn test_strip_username_prefix_username_and_uuid() {
        let path = "timm/ae8ae236-709f-4939-b766-37ad589ac7f2";
        assert_eq!(
            strip_username_prefix(path),
            "ae8ae236-709f-4939-b766-37ad589ac7f2"
        );
    }

    #[test]
    fn test_strip_username_prefix_username_uuid_and_event() {
        let path = "timm/ae8ae236-709f-4939-b766-37ad589ac7f2/event.ics";
        assert_eq!(
            strip_username_prefix(path),
            "ae8ae236-709f-4939-b766-37ad589ac7f2/event.ics"
        );
    }

    #[test]
    fn test_strip_username_prefix_bare_username() {
        assert_eq!(strip_username_prefix("timm"), "");
    }

    #[test]
    fn test_strip_username_prefix_empty() {
        assert_eq!(strip_username_prefix(""), "");
    }

    #[test]
    fn test_strip_username_prefix_email_style_username() {
        let path = "user@example.com/ae8ae236-709f-4939-b766-37ad589ac7f2/event.ics";
        assert_eq!(
            strip_username_prefix(path),
            "ae8ae236-709f-4939-b766-37ad589ac7f2/event.ics"
        );
    }
}
