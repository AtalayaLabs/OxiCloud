use chrono::{DateTime, Utc};
use quick_xml::{
    Reader, Writer,
    events::{BytesEnd, BytesStart, BytesText, Event},
};
/**
 * CalDAV Adapter Module
 *
 * This module provides conversion between CalDAV protocol XML structures and OxiCloud domain objects.
 * It handles parsing CalDAV request XML and generating CalDAV response XML according to RFC 4791.
 */
use std::io::{BufReader, Read, Write};
use uuid::Uuid;

use crate::application::adapters::webdav_adapter::{
    PropFindRequest, PropFindType, QualifiedName, Result, WebDavAdapter, WebDavError,
};
use crate::application::dtos::calendar_dto::{CalendarDto, CalendarEventDto, CalendarTodoDto};

/// Emit a WebDAV `getetag` body as `"…"` with the surrounding quotes written as
/// borrowed pre-escaped `&quot;` text events around the escaped etag body.
///
/// Byte-identical to escaping a `"{etag}"` String — `quick_xml`'s
/// `BytesText::new` escapes a literal `"` → `&quot;`, re-allocating an owned
/// `Cow` — but with 0 heap allocs (the NextCloud ROUND20 §C1 / CardDAV
/// ROUND21 §R4 pattern, applied to the CalDAV emitter it missed). The caller
/// writes the surrounding `<D:getetag>…</D:getetag>` tags. Every `etag` body
/// here is a bare `Uuid` (`calendar.id` / `anchor.id`), so the escaped body is
/// itself a borrow — 0 allocs/row.
fn write_quoted_etag<W: Write>(xml_writer: &mut Writer<W>, etag: &str) -> Result<()> {
    xml_writer.write_event(Event::Text(BytesText::from_escaped("&quot;")))?;
    xml_writer.write_event(Event::Text(BytesText::new(etag)))?;
    xml_writer.write_event(Event::Text(BytesText::from_escaped("&quot;")))?;
    Ok(())
}

/// Parse a CalDAV `time-range` element's `start` / `end` attribute
/// value into a UTC `DateTime`.
///
/// RFC 4791 §9.9 requires iCalendar DATE-TIME format
/// (`YYYYMMDDTHHMMSSZ` — no dashes, no colons). Every real client
/// (Thunderbird, Apple Calendar, DAVx⁵, Gnome Calendar) sends this
/// shape, as does the `python-caldav` library.
///
/// A prior pass parsed the value with `DateTime::parse_from_rfc3339`
/// exclusively, which expects `YYYY-MM-DDTHH:MM:SSZ` and fails on
/// the standard shape — silently returning `None`. The caller then
/// dropped the whole time-range filter and fell through to
/// `list_events`, returning the entire calendar regardless of the
/// window. RFC 3339 is retained as a defensive fallback for the rare
/// client that emits it.
///
/// Returns `None` on any parse failure — callers propagate that as
/// "no time-range filter provided", matching the pre-fix behaviour
/// for missing attributes.
fn parse_caldav_datetime(value: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
        .map(|nd| nd.and_utc())
        .ok()
        .or_else(|| {
            DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        })
}

/// Byte index of the first ASCII-case-insensitive occurrence of
/// `needle` in `hay` at or after `from`. Every stored body OxiCloud
/// itself writes carries uppercase tags, so try the memchr-backed
/// exact `find` first; only genuinely mixed-case foreign bodies pay
/// the manual scan. Either way this replaces the old
/// `to_ascii_uppercase()` of the ENTIRE body — one full-copy String
/// allocation per event per REPORT/GET, done purely to locate two
/// tags.
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    if let Some(i) = hay[from..].find(needle) {
        return Some(from + i);
    }
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if h.len() < n.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Extract the `BEGIN:<kind>` ... `END:<kind>` slice from a stored
/// `ical_data` body (as returned by the storage layer — one full
/// VCALENDAR per row).
///
/// Case-insensitive on the tag names per RFC 5545 §3.1. Includes the
/// BEGIN/END lines themselves. Returns `None` if either tag is
/// missing (malformed body) so callers can fall back safely.
fn extract_component_chunk<'a>(
    ical_data: &'a str,
    begin_tag: &str,
    end_tag: &str,
) -> Option<&'a str> {
    let begin = find_ci(ical_data, begin_tag, 0)?;
    // End marker: the first END tag after `begin`, plus the length
    // of the tag itself, then any immediate CRLF/LF to include
    // the terminator line.
    let rel_end = find_ci(ical_data, end_tag, begin)?;
    let end_tag_end = rel_end + end_tag.len();
    // Include any immediate line terminator so the chunk stays a
    // well-formed line even when the caller concatenates.
    let mut end = end_tag_end;
    if ical_data[end..].starts_with('\r') {
        end += 1;
    }
    if ical_data[end..].starts_with('\n') {
        end += 1;
    }
    Some(&ical_data[begin..end])
}

/// Extract the `BEGIN:VEVENT` ... `END:VEVENT` slice from a
/// stored `ical_data` body — see [`extract_component_chunk`].
pub(crate) fn extract_vevent_chunk(ical_data: &str) -> Option<&str> {
    extract_component_chunk(ical_data, "BEGIN:VEVENT", "END:VEVENT")
}

/// Extract the row's own component chunk (VEVENT or VTODO — #754)
/// from its stored `ical_data`.
pub(crate) fn extract_object_chunk<T: CalendarObjectRow>(row: &T) -> Option<&str> {
    match row.row_component_name() {
        "VTODO" => extract_component_chunk(row.row_ical_data(), "BEGIN:VTODO", "END:VTODO"),
        _ => extract_vevent_chunk(row.row_ical_data()),
    }
}

/// Extract every `BEGIN:VTIMEZONE` … `END:VTIMEZONE` chunk from a
/// stored `ical_data` body. Rows written after the #689 fix embed the
/// PUT body's VTIMEZONE blocks (RRULEs included) in their VCALENDAR
/// shell; older rows carry none and yield an empty Vec.
///
/// Each chunk includes its trailing line terminator, matching
/// [`extract_vevent_chunk`]'s contract. Nested STANDARD/DAYLIGHT
/// observances ride along untouched — only the outer END:VTIMEZONE
/// closes a chunk.
pub(crate) fn extract_vtimezone_chunks(ical_data: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(begin) = find_ci(ical_data, "BEGIN:VTIMEZONE", from) {
        let Some(rel_end) = find_ci(ical_data, "END:VTIMEZONE", begin) else {
            break;
        };
        let end_tag_end = rel_end + "END:VTIMEZONE".len();
        let mut end = end_tag_end;
        if ical_data[end..].starts_with('\r') {
            end += 1;
        }
        if ical_data[end..].starts_with('\n') {
            end += 1;
        }
        out.push(&ical_data[begin..end]);
        from = end;
    }
    out
}

/// The `TZID` property value of a VTIMEZONE chunk — the dedupe key
/// used when bundling multiple rows' shells (each row embeds the same
/// zone definitions from its own PUT).
///
/// Scans for the first line starting with `TZID:` (case-insensitive).
/// A folded TZID line isn't unfolded — IANA tzids are far too short to
/// fold in practice, and the worst case of a miss is emitting the same
/// definition twice, which clients tolerate.
pub(crate) fn vtimezone_tzid(chunk: &str) -> Option<&str> {
    for raw_line in chunk.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("TZID:"))
        {
            let value = line[5..].trim();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Read-side projection the CalDAV multistatus / ICS emitters need
/// from a stored calendar-object row. Implemented by
/// [`CalendarEventDto`] (VEVENT) and [`CalendarTodoDto`] (VTODO —
/// #754) so the grouping, bundling and XML emitters below exist
/// exactly once.
pub trait CalendarObjectRow {
    /// Row id — the ETag anchor.
    fn row_id(&self) -> &str;
    /// iCalendar UID the object resource is addressed by.
    fn row_ical_uid(&self) -> &str;
    /// RECURRENCE-ID — `None` on masters, `Some` on exception overrides.
    fn row_recurrence_id(&self) -> Option<&DateTime<Utc>>;
    /// Full stored iCalendar body (served verbatim).
    fn row_ical_data(&self) -> &str;
    /// `updated_at` — the getlastmodified source.
    fn row_updated_at(&self) -> DateTime<Utc>;
    /// Component name for `getcontenttype` ("VEVENT" / "VTODO").
    fn row_component_name(&self) -> &'static str;
}

impl CalendarObjectRow for CalendarEventDto {
    fn row_id(&self) -> &str {
        &self.id
    }
    fn row_ical_uid(&self) -> &str {
        &self.ical_uid
    }
    fn row_recurrence_id(&self) -> Option<&DateTime<Utc>> {
        self.recurrence_id.as_ref()
    }
    fn row_ical_data(&self) -> &str {
        &self.ical_data
    }
    fn row_updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }
    fn row_component_name(&self) -> &'static str {
        "VEVENT"
    }
}

impl CalendarObjectRow for CalendarTodoDto {
    fn row_id(&self) -> &str {
        &self.id
    }
    fn row_ical_uid(&self) -> &str {
        &self.ical_uid
    }
    fn row_recurrence_id(&self) -> Option<&DateTime<Utc>> {
        self.recurrence_id.as_ref()
    }
    fn row_ical_data(&self) -> &str {
        &self.ical_data
    }
    fn row_updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }
    fn row_component_name(&self) -> &'static str {
        "VTODO"
    }
}

/// Group a slice of calendar-object rows by `ical_uid`, preserving
/// the order of first appearance for the groups themselves, and
/// placing the master (`recurrence_id.is_none()`) first within each
/// group per RFC 5545 §3.6.1 convention. Ties among exceptions
/// preserve the original slice order.
///
/// Used by the read-side emitters to fold master + per-instance
/// override rows into a single calendar-object-resource, matching
/// the "one URL per UID" contract of RFC 4791 §4.1. Generic over the
/// object kind so events and tasks share the one implementation
/// (#754).
pub(crate) fn group_objects_by_uid<'a, T: CalendarObjectRow>(rows: &'a [T]) -> Vec<Vec<&'a T>> {
    // Keys borrow from the DTO slice (which outlives every local) — the
    // old String-keyed map cloned every event's UID (twice for first
    // appearances) on every REPORT / collection PROPFIND / GET.
    let mut order: Vec<&'a str> = Vec::new();
    let mut buckets: std::collections::HashMap<&'a str, Vec<&'a T>> =
        std::collections::HashMap::new();

    for row in rows {
        let key = row.row_ical_uid();
        match buckets.entry(key) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                order.push(key);
                slot.insert(vec![row]);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => slot.get_mut().push(row),
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for uid in order {
        let mut bucket = buckets.remove(uid).unwrap_or_default();
        // Master first (recurrence_id None), exceptions in insertion order.
        bucket.sort_by_key(|r| r.row_recurrence_id().is_some());
        out.push(bucket);
    }
    out
}

/// Build the calendar-object-resource body for a bundle (master +
/// N exception overrides sharing the same UID). Serves each row's
/// stored `ical_data` verbatim, extracting the VEVENT chunk and
/// wrapping the concatenation in a single VCALENDAR shell.
///
/// VTIMEZONE blocks embedded in the rows' shells (#689) are re-emitted
/// ONCE per TZID ahead of the events: every row carries the same zone
/// definitions from its own PUT, so naive concatenation would repeat
/// the block for every event in the bundle. When two rows disagree on
/// a TZID's definition (client refreshed its tzdata between PUTs) the
/// first row's version wins — definitions are only advisory to
/// clients, and the newest full sync rewrites every row anyway.
///
/// This is the fix for the phase-4 read-side gap: the pre-fix
/// emitter regenerated the body from DTO fields, which (a) lost
/// every property outside UID / SUMMARY / DTSTART / DTEND /
/// DESCRIPTION / LOCATION / RRULE (so ATTENDEE, VALARM, CATEGORIES,
/// STATUS, X-* all silently dropped) and (b) never emitted
/// RECURRENCE-ID so exception rows were invisible in the bundled
/// GET body. Serving stored bytes verbatim closes both.
///
/// If any row's `ical_data` is malformed (no VEVENT tag pair),
/// that row is skipped — the bundle survives the rest. An empty
/// input bundle yields a minimal VCALENDAR with no VEVENTs (the
/// caller decides whether to treat that as 404 upstream).
pub(crate) fn bundle_to_calendar_body<T: CalendarObjectRow>(bundle: &[&T]) -> String {
    let mut buf = String::with_capacity(256 + bundle.len() * 320);
    buf.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n");
    let mut seen_tzids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for row in bundle {
        for vtz in extract_vtimezone_chunks(row.row_ical_data()) {
            let key = vtimezone_tzid(vtz).unwrap_or(vtz);
            if seen_tzids.insert(key) {
                buf.push_str(vtz);
                if !buf.ends_with('\n') {
                    buf.push_str("\r\n");
                }
            }
        }
        if let Some(chunk) = extract_object_chunk(*row) {
            // The chunk already carries its own trailing line
            // terminator (see extract_vevent_chunk). Append as-is.
            buf.push_str(chunk);
            // Defensive: guarantee a line separator between VEVENTs
            // even if the extracted chunk didn't include a trailing
            // newline (some stored bodies lack the terminator).
            if !buf.ends_with('\n') {
                buf.push_str("\r\n");
            }
        }
    }
    buf.push_str("END:VCALENDAR\r\n");
    buf
}

/// Returns whether `caller_id` owns `calendar`.
///
/// CalDAV clients (DAVx5, Apple Calendar, Thunderbird) only mount a collection
/// read-write when its `current-user-privilege-set` advertises `<D:write/>`, so
/// this gate decides read-only vs read-write for the caller. `caller_id` and
/// [`CalendarDto::owner_id`] are both the user's UUID rendered via
/// `Uuid::to_string()`, so a direct comparison is exact. Calendars merely shared
/// with the caller (non-owner access) stay read-only for now — this never
/// over-grants write.
fn caller_owns_calendar(calendar: &CalendarDto, caller_id: &str) -> bool {
    !caller_id.is_empty() && calendar.owner_id == caller_id
}

/// CalDAV report type
#[derive(Debug, PartialEq, Clone)]
pub enum CalDavReportType {
    /// Calendar-query report
    CalendarQuery {
        time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
        /// The inner `comp-filter` name (`"VEVENT"` / `"VTODO"`),
        /// uppercased. `None` means the client didn't restrict the
        /// component kind — per RFC 4791 §9.7 an unfiltered query
        /// matches every stored kind (events AND tasks, #754).
        /// Unknown kinds fall back to the events surface (the
        /// pre-comp-filter-awareness behaviour).
        comp: Option<String>,
        props: Vec<QualifiedName>,
    },
    /// Calendar-multiget report
    CalendarMultiget {
        hrefs: Vec<String>,
        props: Vec<QualifiedName>,
    },
    /// Sync-collection report
    SyncCollection {
        sync_token: String,
        props: Vec<QualifiedName>,
    },
}

/// CalDAV adapter for converting between XML and domain objects
pub struct CalDavAdapter;

/// Element for an echoed client property. Known namespaces use the
/// prefixes declared on the multistatus root; any other namespace gets a
/// local `xmlns:U` declaration. The old fallback glued the namespace URI
/// into the ELEMENT NAME (`<http://inf-it.com/ns/dav/:settings/>`), which
/// is invalid XML and made strict clients (InfCloud/CalDavZAP) fail the
/// whole PROPFIND parse.
fn caldav_prop_el(namespace: &str, name: &str) -> BytesStart<'static> {
    let (qname, xmlns) = match namespace {
        "DAV:" => (format!("D:{name}"), None),
        "urn:ietf:params:xml:ns:caldav" => (format!("C:{name}"), None),
        "http://calendarserver.org/ns/" => (format!("CS:{name}"), None),
        other => (format!("U:{name}"), Some(other)),
    };
    let mut el = BytesStart::new(qname);
    if let Some(ns) = xmlns {
        el.push_attribute(("xmlns:U", ns));
    }
    el
}

impl CalDavAdapter {
    /// Parse a REPORT XML request for CalDAV
    pub fn parse_report<R: Read>(reader: R) -> Result<CalDavReportType> {
        let mut xml_reader = Reader::from_reader(BufReader::new(reader));
        xml_reader.config_mut().trim_text(true);

        let mut buffer = Vec::new();
        let mut in_calendar_query = false;
        let mut in_calendar_multiget = false;
        let mut in_sync_collection = false;
        let mut in_prop = false;
        let mut in_filter = false;
        let mut start_time: Option<DateTime<Utc>> = None;
        let mut end_time: Option<DateTime<Utc>> = None;
        let mut props = Vec::new();
        let mut hrefs = Vec::new();
        let mut sync_token = String::new();
        // Inner `comp-filter` name ("VTODO" / "VEVENT"), uppercased.
        // The standard nesting is VCALENDAR → VEVENT/VTODO (RFC 4791
        // §9.7); the calendar-level outer name is skipped, the last
        // inner name wins (sibling comp-filters are an OR the query
        // layer doesn't model — documented behaviour).
        let mut comp_filter_name: Option<String> = None;
        let mut ns_map = std::collections::HashMap::<String, String>::new();

        loop {
            match xml_reader.read_event_into(&mut buffer) {
                Ok(Event::Start(ref e)) => {
                    WebDavAdapter::collect_ns_decls(e, &mut ns_map);
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "calendar-query" || s.ends_with(":calendar-query") => {
                            in_calendar_query = true
                        }
                        s if s == "calendar-multiget" || s.ends_with(":calendar-multiget") => {
                            in_calendar_multiget = true
                        }
                        s if s == "sync-collection" || s.ends_with(":sync-collection") => {
                            in_sync_collection = true
                        }
                        s if s == "prop" || s.ends_with(":prop") => in_prop = true,
                        s if s == "filter" || s.ends_with(":filter") => in_filter = true,
                        s if in_filter && (s == "comp-filter" || s.ends_with(":comp-filter")) => {
                            // Read the component name this filter
                            // restricts to. The outer VCALENDAR level
                            // is structural and skipped; the inner one
                            // (VEVENT / VTODO) routes the query (#754).
                            for attr in e.attributes().flatten() {
                                if std::str::from_utf8(attr.key.as_ref()).unwrap_or("") != "name" {
                                    continue;
                                }
                                let value = attr
                                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                    .unwrap_or_default();
                                if !value.eq_ignore_ascii_case("VCALENDAR") {
                                    comp_filter_name = Some(value.to_ascii_uppercase());
                                }
                            }
                        }
                        s if s == "time-range" || s.ends_with(":time-range") => {
                            for attr in e.attributes().flatten() {
                                let attr_name =
                                    std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                                let attr_value = attr
                                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                    .unwrap_or_default();

                                if attr_name == "start" {
                                    start_time = parse_caldav_datetime(&attr_value);
                                } else if attr_name == "end" {
                                    end_time = parse_caldav_datetime(&attr_value);
                                }
                            }
                        }
                        s if s == "sync-token" || s.ends_with(":sync-token") => {
                            // We'll capture the text in the Text event
                        }
                        s if s == "href" || s.ends_with(":href") => {
                            // We'll capture the text in the Text event
                        }
                        _ if in_prop => {
                            let qname = WebDavAdapter::resolve_name(name_str, &ns_map);
                            props.push(qname);
                        }
                        _ => { /* Ignore other elements */ }
                    }
                }
                Ok(Event::Text(e)) => {
                    let text = e.decode().unwrap_or_default();

                    // Check if we're in sync-token element
                    if in_sync_collection && !in_prop && !in_filter {
                        sync_token = text.to_string();
                    }

                    // Check if we're in href element
                    if (in_calendar_multiget || in_sync_collection) && !in_prop && !in_filter {
                        hrefs.push(text.to_string());
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        // Don't reset report-type flags — they're needed at EOF for decision logic
                        s if s == "prop" || s.ends_with(":prop") => in_prop = false,
                        s if s == "filter" || s.ends_with(":filter") => in_filter = false,
                        s if s == "time-range" || s.ends_with(":time-range") => { /* time-range end, attributes already parsed */
                        }
                        _ => (),
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    WebDavAdapter::collect_ns_decls(e, &mut ns_map);
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    if in_prop {
                        let qname = WebDavAdapter::resolve_name(name_str, &ns_map);
                        props.push(qname);
                    } else if name_str == "time-range" || name_str.ends_with(":time-range") {
                        // Empty-element form: <C:time-range start="..." end="..."/>
                        for attr in e.attributes().flatten() {
                            let attr_name = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            let attr_value = attr
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .unwrap_or_default();

                            if attr_name == "start" {
                                start_time = parse_caldav_datetime(&attr_value);
                            } else if attr_name == "end" {
                                end_time = parse_caldav_datetime(&attr_value);
                            }
                        }
                    } else if in_filter
                        && (name_str == "comp-filter" || name_str.ends_with(":comp-filter"))
                    {
                        // Empty-element form: <C:comp-filter name="VTODO"/>
                        for attr in e.attributes().flatten() {
                            if std::str::from_utf8(attr.key.as_ref()).unwrap_or("") != "name" {
                                continue;
                            }
                            let value = attr
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .unwrap_or_default();
                            if !value.eq_ignore_ascii_case("VCALENDAR") {
                                comp_filter_name = Some(value.to_ascii_uppercase());
                            }
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(WebDavError::XmlError(e)),
                _ => (),
            }

            buffer.clear();
        }

        // Create the appropriate report type based on what we parsed
        let report_type = if in_calendar_query {
            // If both start and end time are present, create a time range
            let time_range = if let (Some(start), Some(end)) = (start_time, end_time) {
                Some((start, end))
            } else {
                None
            };

            CalDavReportType::CalendarQuery {
                time_range,
                comp: comp_filter_name,
                props,
            }
        } else if in_calendar_multiget {
            CalDavReportType::CalendarMultiget { hrefs, props }
        } else if in_sync_collection {
            CalDavReportType::SyncCollection { sync_token, props }
        } else {
            // Default to empty calendar query
            CalDavReportType::CalendarQuery {
                time_range: None,
                comp: None,
                props,
            }
        };

        Ok(report_type)
    }

    /// Generate a PROPFIND response for the root CalDAV resource.
    /// Includes a response for /caldav/ itself with discovery properties
    /// (current-user-principal, calendar-home-set) plus each calendar.
    pub fn generate_root_propfind_response<W: Write>(
        writer: W,
        calendars: &[CalendarDto],
        request: &PropFindRequest,
        base_href: &str,
        username: &str,
        caller_id: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        // Start multistatus response
        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:C", "urn:ietf:params:xml:ns:caldav"),
                ("xmlns:CS", "http://calendarserver.org/ns/"),
            ]),
        ))?;

        // Write the root /caldav/ response with discovery properties
        Self::write_root_response(&mut xml_writer, request, base_href, username)?;

        // Add responses for calendars
        for calendar in calendars {
            Self::write_calendar_response(
                &mut xml_writer,
                calendar,
                request,
                &format!("{}{}/", base_href, calendar.id),
                caller_id,
            )?;
        }

        // End multistatus
        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;

        Ok(())
    }

    /// Generate a PROPFIND response for calendars (without root discovery entry)
    pub fn generate_calendars_propfind_response<W: Write>(
        writer: W,
        calendars: &[CalendarDto],
        request: &PropFindRequest,
        base_href: &str,
        caller_id: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        // Start multistatus response
        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:C", "urn:ietf:params:xml:ns:caldav"),
                ("xmlns:CS", "http://calendarserver.org/ns/"),
            ]),
        ))?;

        // Add responses for calendars
        for calendar in calendars {
            Self::write_calendar_response(
                &mut xml_writer,
                calendar,
                request,
                &format!("{}{}/", base_href, calendar.id),
                caller_id,
            )?;
        }

        // End multistatus
        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;

        Ok(())
    }

    /// Generate a PROPFIND response for a user principal resource.
    pub fn generate_principal_propfind_response<W: Write>(
        writer: W,
        request: &PropFindRequest,
        username: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:C", "urn:ietf:params:xml:ns:caldav"),
                ("xmlns:CS", "http://calendarserver.org/ns/"),
            ]),
        ))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        // href
        let href = format!(
            "{}/caldav/principals/{}/",
            crate::common::config::server_base_path(),
            username
        );
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

        match &request.prop_find_type {
            PropFindType::AllProp | PropFindType::PropName => {
                Self::write_principal_props(&mut xml_writer, username)?;
            }
            PropFindType::Prop(props) => {
                Self::write_principal_requested_props(&mut xml_writer, username, props)?;
            }
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;

        Ok(())
    }

    /// Write the root /caldav/ response entry with discovery properties.
    fn write_root_response<W: Write>(
        xml_writer: &mut Writer<W>,
        request: &PropFindRequest,
        href: &str,
        username: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

        match &request.prop_find_type {
            PropFindType::AllProp => {
                // Resource type — collection
                xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
                xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;

                // current-user-principal
                xml_writer
                    .write_event(Event::Start(BytesStart::new("D:current-user-principal")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
                xml_writer.write_event(Event::Text(BytesText::new(&format!(
                    "{}/caldav/principals/{}/",
                    crate::common::config::server_base_path(),
                    username
                ))))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:current-user-principal")))?;

                // calendar-home-set
                xml_writer.write_event(Event::Start(BytesStart::new("C:calendar-home-set")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
                xml_writer.write_event(Event::Text(BytesText::new(&format!(
                    "{}/caldav/{}/",
                    crate::common::config::server_base_path(),
                    username
                ))))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("C:calendar-home-set")))?;
            }
            PropFindType::PropName => {
                xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;
                xml_writer
                    .write_event(Event::Empty(BytesStart::new("D:current-user-principal")))?;
                xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar-home-set")))?;
            }
            PropFindType::Prop(props) => {
                Self::write_root_requested_props(xml_writer, username, props)?;
            }
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;

        Ok(())
    }

    /// Write requested properties for the root /caldav/ resource.
    fn write_root_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        username: &str,
        props: &[QualifiedName],
    ) -> Result<()> {
        for prop in props {
            match (prop.namespace.as_str(), prop.name.as_str()) {
                ("DAV:", "resourcetype") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;
                }
                ("DAV:", "current-user-principal") => {
                    xml_writer
                        .write_event(Event::Start(BytesStart::new("D:current-user-principal")))?;
                    xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(&format!(
                        "{}/caldav/principals/{}/",
                        crate::common::config::server_base_path(),
                        username
                    ))))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
                    xml_writer
                        .write_event(Event::End(BytesEnd::new("D:current-user-principal")))?;
                }
                ("urn:ietf:params:xml:ns:caldav", "calendar-home-set") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("C:calendar-home-set")))?;
                    xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(&format!(
                        "{}/caldav/{}/",
                        crate::common::config::server_base_path(),
                        username
                    ))))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("C:calendar-home-set")))?;
                }
                ("DAV:", "displayname") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
                    xml_writer.write_event(Event::Text(BytesText::new("CalDAV Root")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;
                }
                _ => {
                    // Unknown property — write empty
                    xml_writer
                        .write_event(Event::Empty(caldav_prop_el(&prop.namespace, &prop.name)))?;
                }
            }
        }
        Ok(())
    }

    /// Write standard properties for a principal resource.
    fn write_principal_props<W: Write>(xml_writer: &mut Writer<W>, username: &str) -> Result<()> {
        // resourcetype — principal
        xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:principal")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;

        // displayname
        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Text(BytesText::new(username)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;

        // calendar-home-set
        xml_writer.write_event(Event::Start(BytesStart::new("C:calendar-home-set")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&format!(
            "{}/caldav/{}/",
            crate::common::config::server_base_path(),
            username
        ))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("C:calendar-home-set")))?;

        // current-user-principal (self-reference)
        xml_writer.write_event(Event::Start(BytesStart::new("D:current-user-principal")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&format!(
            "{}/caldav/principals/{}/",
            crate::common::config::server_base_path(),
            username
        ))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:current-user-principal")))?;

        Ok(())
    }

    /// Write requested properties for a principal resource.
    fn write_principal_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        username: &str,
        props: &[QualifiedName],
    ) -> Result<()> {
        for prop in props {
            match (prop.namespace.as_str(), prop.name.as_str()) {
                ("DAV:", "resourcetype") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:principal")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;
                }
                ("DAV:", "displayname") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(username)))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;
                }
                ("DAV:", "current-user-principal") => {
                    xml_writer
                        .write_event(Event::Start(BytesStart::new("D:current-user-principal")))?;
                    xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(&format!(
                        "{}/caldav/principals/{}/",
                        crate::common::config::server_base_path(),
                        username
                    ))))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
                    xml_writer
                        .write_event(Event::End(BytesEnd::new("D:current-user-principal")))?;
                }
                ("urn:ietf:params:xml:ns:caldav", "calendar-home-set") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("C:calendar-home-set")))?;
                    xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(&format!(
                        "{}/caldav/{}/",
                        crate::common::config::server_base_path(),
                        username
                    ))))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("C:calendar-home-set")))?;
                }
                ("urn:ietf:params:xml:ns:caldav", "calendar-user-address-set") => {
                    xml_writer.write_event(Event::Start(BytesStart::new(
                        "C:calendar-user-address-set",
                    )))?;
                    xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(&format!(
                        "{}/caldav/principals/{}/",
                        crate::common::config::server_base_path(),
                        username
                    ))))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
                    xml_writer
                        .write_event(Event::End(BytesEnd::new("C:calendar-user-address-set")))?;
                }
                _ => {
                    xml_writer
                        .write_event(Event::Empty(caldav_prop_el(&prop.namespace, &prop.name)))?;
                }
            }
        }
        Ok(())
    }

    /// Write calendar properties as a response
    fn write_calendar_response<W: Write>(
        xml_writer: &mut Writer<W>,
        calendar: &CalendarDto,
        request: &PropFindRequest,
        href: &str,
        caller_id: &str,
    ) -> Result<()> {
        // Start response element
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        // Write href
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        // Write propstat
        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;

        // Start prop
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

        // Write properties based on request type
        match &request.prop_find_type {
            PropFindType::AllProp => {
                // Write all standard properties for a calendar
                Self::write_calendar_standard_props(xml_writer, calendar, caller_id)?;
            }
            PropFindType::PropName => {
                // Write only property names (empty elements)
                Self::write_calendar_prop_names(xml_writer)?;
            }
            PropFindType::Prop(props) => {
                // Write requested properties
                Self::write_calendar_requested_props(xml_writer, calendar, props, caller_id)?;
            }
        }

        // End prop
        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

        // Write status
        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

        // End propstat
        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;

        // End response
        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;

        Ok(())
    }

    /// Write standard calendar properties
    fn write_calendar_standard_props<W: Write>(
        xml_writer: &mut Writer<W>,
        calendar: &CalendarDto,
        caller_id: &str,
    ) -> Result<()> {
        // Common WebDAV properties

        // Resource type (collection + calendar)
        xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;

        // Display name
        xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&calendar.name)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;

        // Last modified (stack render, benches/ROUND14.md §A5)
        xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
        Self::write_lastmodified_text(xml_writer, calendar.updated_at)?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;

        // ETag (borrowed pre-escaped quotes, §C1/§R4 — was format! + escape)
        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
        write_quoted_etag(xml_writer, &calendar.id)?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;

        // Content type for calendar collection
        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Text(BytesText::new(
            "text/calendar; component=VCALENDAR",
        )))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

        // CalDAV specific properties

        // Supported calendar component set — VEVENT + VTODO (#754)
        Self::write_supported_component_set(xml_writer)?;

        // Calendar timezone (empty for UTC)
        xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar-timezone")))?;

        // Calendar color
        if let Some(color) = &calendar.color {
            xml_writer.write_event(Event::Start(BytesStart::new("CS:calendar-color")))?;
            xml_writer.write_event(Event::Text(BytesText::new(color)))?;
            xml_writer.write_event(Event::End(BytesEnd::new("CS:calendar-color")))?;
        }

        // Support calendar-access (RFC4791)
        xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar-access")))?;

        // Current user privilege set
        xml_writer.write_event(Event::Start(BytesStart::new(
            "D:current-user-privilege-set",
        )))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:privilege")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:read")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:privilege")))?;

        // Advertise write only when the caller owns the calendar. Clients
        // (DAVx5, Apple Calendar, Thunderbird) mount the collection read-only
        // unless this privilege is present.
        if caller_owns_calendar(calendar, caller_id) {
            xml_writer.write_event(Event::Start(BytesStart::new("D:privilege")))?;
            xml_writer.write_event(Event::Empty(BytesStart::new("D:write")))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:privilege")))?;
        }

        xml_writer.write_event(Event::End(BytesEnd::new("D:current-user-privilege-set")))?;

        // Calendar description if present
        if let Some(desc) = &calendar.description {
            xml_writer.write_event(Event::Start(BytesStart::new("C:calendar-description")))?;
            xml_writer.write_event(Event::Text(BytesText::new(desc)))?;
            xml_writer.write_event(Event::End(BytesEnd::new("C:calendar-description")))?;
        }

        // Custom properties
        for (name, value) in &calendar.custom_properties {
            // Skip properties that start with _ - they're internal
            if !name.starts_with('_') {
                xml_writer.write_event(Event::Start(BytesStart::new(format!("CS:{}", name))))?;
                xml_writer.write_event(Event::Text(BytesText::new(value)))?;
                xml_writer.write_event(Event::End(BytesEnd::new(format!("CS:{}", name))))?;
            }
        }

        Ok(())
    }

    /// Write `C:supported-calendar-component-set` advertising every
    /// component kind OxiCloud stores: VEVENT + VTODO (#754). Tasks
    /// clients (DAVx⁵ + Tasks.org / jtx Board) only enable task sync
    /// on a collection when VTODO is advertised here. MKCALENDAR does
    /// not accept a client-chosen set — every OxiCloud calendar holds
    /// both kinds.
    fn write_supported_component_set<W: Write>(xml_writer: &mut Writer<W>) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new(
            "C:supported-calendar-component-set",
        )))?;
        for name in ["VEVENT", "VTODO"] {
            xml_writer.write_event(Event::Empty(
                BytesStart::new("C:comp").with_attributes([("name", name)]),
            ))?;
        }
        xml_writer.write_event(Event::End(BytesEnd::new(
            "C:supported-calendar-component-set",
        )))?;
        Ok(())
    }

    /// Write calendar property names
    fn write_calendar_prop_names<W: Write>(xml_writer: &mut Writer<W>) -> Result<()> {
        // Common WebDAV property names
        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:displayname")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getlastmodified")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getetag")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("D:getcontenttype")))?;

        // CalDAV specific property names
        xml_writer.write_event(Event::Empty(BytesStart::new(
            "C:supported-calendar-component-set",
        )))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar-timezone")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("CS:calendar-color")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar-access")))?;
        xml_writer.write_event(Event::Empty(BytesStart::new(
            "D:current-user-privilege-set",
        )))?;
        xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar-description")))?;

        Ok(())
    }

    /// Write requested calendar properties
    fn write_calendar_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        calendar: &CalendarDto,
        props: &[QualifiedName],
        caller_id: &str,
    ) -> Result<()> {
        for prop in props {
            match (prop.namespace.as_str(), prop.name.as_str()) {
                // DAV namespace properties
                ("DAV:", "resourcetype") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:resourcetype")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:collection")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:resourcetype")))?;
                }
                ("DAV:", "displayname") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:displayname")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(&calendar.name)))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:displayname")))?;
                }
                ("DAV:", "getlastmodified") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
                    // Stack render (benches/ROUND14.md §A5).
                    Self::write_lastmodified_text(xml_writer, calendar.updated_at)?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;
                }
                ("DAV:", "getetag") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
                    write_quoted_etag(xml_writer, &calendar.id)?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;
                }
                ("DAV:", "getcontenttype") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(
                        "text/calendar; component=VCALENDAR",
                    )))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;
                }
                ("DAV:", "current-user-privilege-set") => {
                    xml_writer.write_event(Event::Start(BytesStart::new(
                        "D:current-user-privilege-set",
                    )))?;
                    xml_writer.write_event(Event::Start(BytesStart::new("D:privilege")))?;
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:read")))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:privilege")))?;

                    // Advertise write only when the caller owns the calendar.
                    if caller_owns_calendar(calendar, caller_id) {
                        xml_writer.write_event(Event::Start(BytesStart::new("D:privilege")))?;
                        xml_writer.write_event(Event::Empty(BytesStart::new("D:write")))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("D:privilege")))?;
                    }

                    xml_writer
                        .write_event(Event::End(BytesEnd::new("D:current-user-privilege-set")))?;
                }

                // CalDAV namespace properties
                ("urn:ietf:params:xml:ns:caldav", "supported-calendar-component-set") => {
                    Self::write_supported_component_set(xml_writer)?;
                }
                ("urn:ietf:params:xml:ns:caldav", "calendar-timezone") => {
                    xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar-timezone")))?;
                }
                ("urn:ietf:params:xml:ns:caldav", "calendar-access") => {
                    xml_writer.write_event(Event::Empty(BytesStart::new("C:calendar-access")))?;
                }
                ("urn:ietf:params:xml:ns:caldav", "calendar-description") => {
                    if let Some(desc) = &calendar.description {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("C:calendar-description")))?;
                        xml_writer.write_event(Event::Text(BytesText::new(desc)))?;
                        xml_writer
                            .write_event(Event::End(BytesEnd::new("C:calendar-description")))?;
                    } else {
                        xml_writer
                            .write_event(Event::Empty(BytesStart::new("C:calendar-description")))?;
                    }
                }

                // CalendarServer namespace properties
                ("http://calendarserver.org/ns/", "calendar-color") => {
                    if let Some(color) = &calendar.color {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("CS:calendar-color")))?;
                        xml_writer.write_event(Event::Text(BytesText::new(color)))?;
                        xml_writer.write_event(Event::End(BytesEnd::new("CS:calendar-color")))?;
                    } else {
                        xml_writer
                            .write_event(Event::Empty(BytesStart::new("CS:calendar-color")))?;
                    }
                }

                // Custom properties from the calendar
                _ => {
                    // Check if it's a custom property
                    if let Some(value) = calendar.custom_properties.get(&prop.name) {
                        let prop_el = caldav_prop_el(&prop.namespace, &prop.name);
                        let prop_end = prop_el.to_end().into_owned();
                        xml_writer.write_event(Event::Start(prop_el))?;
                        xml_writer.write_event(Event::Text(BytesText::new(value)))?;
                        xml_writer.write_event(Event::End(prop_end))?;
                    } else {
                        // Property not found, write empty element
                        xml_writer.write_event(Event::Empty(caldav_prop_el(
                            &prop.namespace,
                            &prop.name,
                        )))?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Generate PROPFIND response for a single calendar collection + its events
    pub fn generate_calendar_collection_propfind<W: Write>(
        writer: W,
        calendar: &CalendarDto,
        events: &[CalendarEventDto],
        request: &PropFindRequest,
        base_href: &str,
        depth: &str,
        caller_id: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:C", "urn:ietf:params:xml:ns:caldav"),
                ("xmlns:CS", "http://calendarserver.org/ns/"),
            ]),
        ))?;

        // Write the calendar collection itself
        Self::write_calendar_response(&mut xml_writer, calendar, request, base_href, caller_id)?;

        // If depth > 0, include event resources — see
        // `write_collection_event_page`, which the streaming emitter
        // reuses page by page.
        if depth != "0" {
            Self::write_collection_event_page(&mut xml_writer, events, base_href)?;
        }

        Self::write_caldav_multistatus_end(&mut xml_writer)?;
        Ok(())
    }

    /// Multistatus opening + the calendar collection's own
    /// `D:response` — the head of a depth-1 collection PROPFIND. The
    /// streaming emitter calls this once, then
    /// [`Self::write_collection_event_page`] per hydrated UID page,
    /// then [`Self::write_caldav_multistatus_end`].
    pub fn write_collection_head<W: Write>(
        xml_writer: &mut Writer<W>,
        calendar: &CalendarDto,
        request: &PropFindRequest,
        base_href: &str,
        caller_id: &str,
    ) -> Result<()> {
        Self::write_caldav_multistatus_start(xml_writer)?;
        Self::write_calendar_response(xml_writer, calendar, request, base_href, caller_id)
    }

    /// One depth-1 collection page: event resources folded per UID so a
    /// recurring master + per-instance exception overrides share ONE
    /// `D:response` (RFC 4791 §4.1 + RFC 5545 §3.6.1) — emitting one
    /// response per DB row made clients dedupe the shared href and the
    /// exception appeared to vanish. Callers guarantee same-UID rows
    /// arrive within a single page.
    /// Emit an RFC 2822 `getlastmodified` text node with the allocation-free
    /// stack renderer (byte-identical to `chrono::to_rfc2822` — the parity
    /// gate lives in `common::fmt`), falling back to chrono only for
    /// out-of-4-digit-year timestamps. Mirrors the CardDAV emitter; replaces
    /// the per-event `updated_at.to_rfc2822()` heap `String`
    /// (benches/ROUND14.md §A5).
    fn write_lastmodified_text<W: Write>(
        xml_writer: &mut Writer<W>,
        ts: DateTime<Utc>,
    ) -> Result<()> {
        let mut buf = [0u8; 31];
        match crate::common::fmt::rfc2822_utc(&mut buf, ts.timestamp()) {
            Some(s) => xml_writer.write_event(Event::Text(BytesText::new(s)))?,
            None => xml_writer.write_event(Event::Text(BytesText::new(&ts.to_rfc2822())))?,
        }
        Ok(())
    }

    pub fn write_collection_event_page<W: Write, T: CalendarObjectRow>(
        xml_writer: &mut Writer<W>,
        rows: &[T],
        base_href: &str,
    ) -> Result<()> {
        // Reused per-event href buffer (cleared each iteration) so a whole
        // PROPFIND page allocates the href storage once instead of per event
        // (benches/ROUND14.md §A6). The etag no longer needs a buffer — it is
        // emitted via `write_quoted_etag` (borrowed pre-escaped quotes).
        let mut event_href = String::with_capacity(base_href.len() + 48);
        for bundle in group_objects_by_uid(rows) {
            // The master (sorted first by group_objects_by_uid)
            // supplies the ETag anchor + getlastmodified. If
            // the bundle is all exceptions (no master row),
            // fall back to the first exception.
            let anchor = match bundle.first() {
                Some(e) => *e,
                None => continue,
            };
            event_href.clear();
            let _ = std::fmt::Write::write_fmt(
                &mut event_href,
                format_args!("{}{}.ics", base_href, anchor.row_ical_uid()),
            );

            xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;
            xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
            xml_writer.write_event(Event::Text(BytesText::new(&event_href)))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

            xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;
            xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

            // resourcetype (empty for non-collection)
            xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;

            // getetag — anchor row's id (borrowed pre-escaped quotes, §C1/§R4)
            xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
            write_quoted_etag(xml_writer, anchor.row_id())?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;

            // getcontenttype — `component=VEVENT` / `component=VTODO` by kind
            xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
            xml_writer.write_event(Event::Text(BytesText::new(&format!(
                "text/calendar; component={}",
                anchor.row_component_name()
            ))))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

            // getlastmodified — anchor row's updated_at (stack render, §A5)
            xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
            Self::write_lastmodified_text(xml_writer, anchor.row_updated_at())?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;

            xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

            xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
            xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

            xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        }
        Ok(())
    }

    /// Write the CalDAV `<D:multistatus>` opening tag (DAV + CalDAV +
    /// CalendarServer namespaces). Streaming emitters call this once,
    /// then [`Self::write_report_page`] per hydrated UID page, then
    /// [`Self::write_caldav_multistatus_end`].
    pub fn write_caldav_multistatus_start<W: Write>(xml_writer: &mut Writer<W>) -> Result<()> {
        xml_writer.write_event(Event::Start(
            BytesStart::new("D:multistatus").with_attributes([
                ("xmlns:D", "DAV:"),
                ("xmlns:C", "urn:ietf:params:xml:ns:caldav"),
                ("xmlns:CS", "http://calendarserver.org/ns/"),
            ]),
        ))?;
        Ok(())
    }

    /// Close the multistatus opened by
    /// [`Self::write_caldav_multistatus_start`].
    pub fn write_caldav_multistatus_end<W: Write>(xml_writer: &mut Writer<W>) -> Result<()> {
        xml_writer.write_event(Event::End(BytesEnd::new("D:multistatus")))?;
        Ok(())
    }

    /// One REPORT page: group rows per UID and emit one
    /// `D:response` per bundle. Callers guarantee same-UID rows arrive
    /// within a single page (the uid-keyset pager does). Generic over
    /// the object kind (VEVENT / VTODO — #754).
    pub fn write_report_page<W: Write, T: CalendarObjectRow>(
        xml_writer: &mut Writer<W>,
        rows: &[T],
        request: &CalDavReportType,
        base_href: &str,
    ) -> Result<()> {
        let props = match request {
            CalDavReportType::CalendarQuery { props, .. } => props,
            CalDavReportType::CalendarMultiget { props, .. } => props,
            CalDavReportType::SyncCollection { props, .. } => props,
        };
        // Reused per-event href buffer for the whole REPORT page
        // (benches/ROUND14.md §A6). The etag is emitted via `write_quoted_etag`
        // (borrowed pre-escaped quotes) and no longer needs a buffer.
        let mut href = String::with_capacity(base_href.len() + 48);
        for bundle in group_objects_by_uid(rows) {
            let anchor = match bundle.first() {
                Some(e) => *e,
                None => continue,
            };
            href.clear();
            let _ = std::fmt::Write::write_fmt(
                &mut href,
                format_args!("{}{}.ics", base_href, anchor.row_ical_uid()),
            );
            Self::write_event_response(xml_writer, &bundle, props, &href)?;
        }
        Ok(())
    }

    /// Generate a response for calendar objects (events or tasks).
    pub fn generate_calendar_events_response<W: Write, T: CalendarObjectRow>(
        writer: W,
        rows: &[T],
        request: &CalDavReportType,
        base_href: &str,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        Self::write_caldav_multistatus_start(&mut xml_writer)?;

        // Responses folded per UID so a recurring master + exception
        // overrides share ONE D:response (RFC 4791 §4.1) — see
        // `write_report_page`, which the streaming emitters reuse
        // page by page.
        Self::write_report_page(&mut xml_writer, rows, request, base_href)?;

        Self::write_caldav_multistatus_end(&mut xml_writer)?;

        Ok(())
    }

    /// Write a bundle (master + exception overrides sharing a
    /// UID) as one D:response. The bundle is emitted at one
    /// href (base + uid.ics); ETag + getlastmodified anchor on
    /// the first bundle entry (which `group_objects_by_uid` puts
    /// the master at); calendar-data contains every component.
    /// Generic over the object kind (VEVENT / VTODO — #754).
    fn write_event_response<W: Write, T: CalendarObjectRow>(
        xml_writer: &mut Writer<W>,
        bundle: &[&T],
        props: &[QualifiedName],
        href: &str,
    ) -> Result<()> {
        let anchor = bundle
            .first()
            .copied()
            .expect("write_event_response: bundle must be non-empty (caller guards)");

        // Start response element
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        // Write href
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        // Write propstat
        xml_writer.write_event(Event::Start(BytesStart::new("D:propstat")))?;

        // Start prop
        xml_writer.write_event(Event::Start(BytesStart::new("D:prop")))?;

        // If no specific props requested, return all common ones
        if props.is_empty() {
            Self::write_event_standard_props(xml_writer, anchor, bundle)?;
        } else {
            // Write specifically requested properties
            Self::write_event_requested_props(xml_writer, anchor, bundle, props)?;
        }

        // End prop
        xml_writer.write_event(Event::End(BytesEnd::new("D:prop")))?;

        // Write status
        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

        // End propstat
        xml_writer.write_event(Event::End(BytesEnd::new("D:propstat")))?;

        // End response
        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;

        Ok(())
    }

    /// Write standard event properties for a UID bundle.
    /// `anchor` supplies metadata (ETag, updated_at); `bundle`
    /// supplies the full calendar-data payload (master + all
    /// exceptions concatenated into one VCALENDAR).
    fn write_event_standard_props<W: Write, T: CalendarObjectRow>(
        xml_writer: &mut Writer<W>,
        anchor: &T,
        bundle: &[&T],
    ) -> Result<()> {
        // Common WebDAV properties

        // Resource type (empty for non-collection)
        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;

        // ETag anchored on the master (or first exception in a master-less
        // bundle — pathological state today). Borrowed pre-escaped quotes
        // (§C1/§R4), 0 allocs/event.
        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
        write_quoted_etag(xml_writer, anchor.row_id())?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;

        // Content type — `component=VEVENT` / `component=VTODO` by row kind.
        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&format!(
            "text/calendar; component={}",
            anchor.row_component_name()
        ))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

        // Last modified (stack render, benches/ROUND14.md §A5)
        xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
        Self::write_lastmodified_text(xml_writer, anchor.row_updated_at())?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;

        // CalDAV calendar-data — the whole bundle emitted as one
        // VCALENDAR by extracting each row's stored component chunk
        // verbatim. Every property (ATTENDEE / VALARM / CATEGORIES
        // / STATUS / PRIORITY / X-* / RECURRENCE-ID on exception rows)
        // survives because we no longer regenerate from DTO
        // fields.
        xml_writer.write_event(Event::Start(BytesStart::new("C:calendar-data")))?;
        let ical_data = bundle_to_calendar_body(bundle);
        xml_writer.write_event(Event::Text(BytesText::new(&ical_data)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("C:calendar-data")))?;

        Ok(())
    }

    /// Write requested event properties
    fn write_event_requested_props<W: Write, T: CalendarObjectRow>(
        xml_writer: &mut Writer<W>,
        anchor: &T,
        bundle: &[&T],
        props: &[QualifiedName],
    ) -> Result<()> {
        for prop in props {
            match (prop.namespace.as_str(), prop.name.as_str()) {
                // DAV namespace properties
                ("DAV:", "resourcetype") => {
                    xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;
                }
                ("DAV:", "getetag") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
                    write_quoted_etag(xml_writer, anchor.row_id())?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;
                }
                ("DAV:", "getcontenttype") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(&format!(
                        "text/calendar; component={}",
                        anchor.row_component_name()
                    ))))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;
                }
                ("DAV:", "getlastmodified") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
                    // Stack render (benches/ROUND14.md §A5).
                    Self::write_lastmodified_text(xml_writer, anchor.row_updated_at())?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;
                }

                // CalDAV namespace properties — calendar-data is
                // the whole bundle, master + exceptions in one
                // VCALENDAR served from stored ical_data.
                ("urn:ietf:params:xml:ns:caldav", "calendar-data") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("C:calendar-data")))?;
                    let ical_data = bundle_to_calendar_body(bundle);
                    xml_writer.write_event(Event::Text(BytesText::new(&ical_data)))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("C:calendar-data")))?;
                }

                // Property not supported
                _ => {
                    // Write empty element
                    xml_writer
                        .write_event(Event::Empty(caldav_prop_el(&prop.namespace, &prop.name)))?;
                }
            }
        }

        Ok(())
    }

    /// Parse a MKCALENDAR XML request
    pub fn parse_mkcalendar<R: Read>(
        reader: R,
    ) -> Result<(String, Option<String>, Option<String>)> {
        let mut xml_reader = Reader::from_reader(BufReader::new(reader));
        xml_reader.config_mut().trim_text(true);

        let mut buffer = Vec::new();
        let mut in_mkcalendar = false;
        let mut in_set = false;
        let mut in_prop = false;
        let mut in_displayname = false;
        let mut in_description = false;
        let mut in_calendar_color = false;

        let mut displayname = String::new();
        let mut description = None;
        let mut color = None;

        loop {
            match xml_reader.read_event_into(&mut buffer) {
                Ok(Event::Start(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "mkcalendar" || s.ends_with(":mkcalendar") => {
                            in_mkcalendar = true
                        }
                        s if in_mkcalendar && (s == "set" || s.ends_with(":set")) => in_set = true,
                        s if in_set && (s == "prop" || s.ends_with(":prop")) => in_prop = true,
                        s if in_prop && (s == "displayname" || s.ends_with(":displayname")) => {
                            in_displayname = true
                        }
                        s if in_prop
                            && (s == "calendar-description"
                                || s.ends_with(":calendar-description")) =>
                        {
                            in_description = true
                        }
                        s if in_prop
                            && (s == "calendar-color" || s.ends_with(":calendar-color")) =>
                        {
                            in_calendar_color = true
                        }
                        _ => (),
                    }
                }
                Ok(Event::Text(e)) => {
                    let text = e.decode().unwrap_or_default();

                    if in_displayname {
                        displayname = text.to_string();
                    } else if in_description {
                        description = Some(text.to_string());
                    } else if in_calendar_color {
                        color = Some(text.to_string());
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    let name_str = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match name_str {
                        s if s == "mkcalendar" || s.ends_with(":mkcalendar") => {
                            in_mkcalendar = false
                        }
                        s if s == "set" || s.ends_with(":set") => in_set = false,
                        s if s == "prop" || s.ends_with(":prop") => in_prop = false,
                        s if s == "displayname" || s.ends_with(":displayname") => {
                            in_displayname = false
                        }
                        s if s == "calendar-description"
                            || s.ends_with(":calendar-description") =>
                        {
                            in_description = false
                        }
                        s if s == "calendar-color" || s.ends_with(":calendar-color") => {
                            in_calendar_color = false
                        }
                        _ => (),
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(WebDavError::XmlError(e)),
                _ => (),
            }

            buffer.clear();
        }

        // If no displayname specified, generate a default one based on UUID
        if displayname.is_empty() {
            displayname = format!("Calendar {}", Uuid::new_v4());
        }

        Ok((displayname, description, color))
    }
}

// ─────────────────────────────────────────────────────────────
// Bench support
// ─────────────────────────────────────────────────────────────

/// Thin public wrappers over the `pub(crate)` read-side helpers so
/// `examples/bench_caldav_parse.rs` can measure them. Gated behind the
/// `bench` feature — adds nothing to prod builds.
#[cfg(feature = "bench")]
pub mod bench {
    use super::*;

    pub fn extract_vevent_chunk(ical_data: &str) -> Option<&str> {
        super::extract_vevent_chunk(ical_data)
    }

    pub fn group_objects_by_uid(events: &[CalendarEventDto]) -> Vec<Vec<&CalendarEventDto>> {
        super::group_objects_by_uid(events)
    }
}

// ─────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod bundle_helper_tests {
    use super::*;

    /// One DTO builder for all tests in this module — carries
    /// enough state (uid, recurrence_id, ical_data) for both the
    /// grouping tests and the bundle-body tests.
    fn dto(uid: &str, is_exception: bool, ical: &str) -> CalendarEventDto {
        use chrono::Utc;
        CalendarEventDto {
            id: "row-".to_string() + uid,
            calendar_id: "cal".to_string(),
            summary: "s".to_string(),
            description: None,
            location: None,
            start_time: Utc::now(),
            end_time: Utc::now(),
            all_day: false,
            rrule: None,
            ical_uid: uid.to_string(),
            recurrence_id: if is_exception { Some(Utc::now()) } else { None },
            ical_data: ical.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    // ── extract_vevent_chunk ──────────────────────────────────

    #[test]
    fn extract_vevent_finds_the_block_inside_vcalendar() {
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VEVENT\r
UID:x\r
DTSTART:20260101T090000Z\r
END:VEVENT\r
END:VCALENDAR\r
";
        let chunk = extract_vevent_chunk(body).expect("VEVENT present");
        assert!(chunk.starts_with("BEGIN:VEVENT"));
        assert!(chunk.contains("UID:x"));
        assert!(chunk.trim_end().ends_with("END:VEVENT"));
    }

    #[test]
    fn extract_vevent_case_insensitive_tags() {
        // RFC 5545 §3.1: component names are case-insensitive on
        // read. Real client output is nearly always uppercase but
        // a lowercase or mixed-case tag mustn't confuse the
        // splitter.
        let body = "begin:vcalendar\nbegin:vevent\nuid:x\nend:vevent\nend:vcalendar\n";
        let chunk = extract_vevent_chunk(body).expect("case-insensitive lookup");
        assert!(chunk.to_ascii_lowercase().contains("uid:x"));
    }

    #[test]
    fn extract_vevent_missing_returns_none() {
        // A body with only VTIMEZONE (no VEVENT) → None. Caller
        // uses this to skip malformed rows without crashing the
        // bundle emitter.
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nEND:VTIMEZONE\r\nEND:VCALENDAR\r\n";
        assert!(extract_vevent_chunk(body).is_none());
    }

    #[test]
    fn extract_vevent_includes_trailing_line_terminator() {
        // The chunk should end with CRLF so bundle concatenation
        // produces valid line-separated iCalendar body.
        let body = "BEGIN:VEVENT\r\nUID:x\r\nEND:VEVENT\r\n";
        let chunk = extract_vevent_chunk(body).unwrap();
        assert!(
            chunk.ends_with("\r\n"),
            "chunk must retain trailing CRLF for safe concatenation, got {:?}",
            chunk
        );
    }

    // ── group_objects_by_uid ───────────────────────────────────

    #[test]
    fn group_places_master_first_within_each_uid() {
        // Mixed order: exception first, then master, then a
        // second exception. Result: [master, exception1, exception2].
        let ex1 = dto("u1", true, "");
        let master = dto("u1", false, "");
        let ex2 = dto("u1", true, "");
        let events = vec![ex1, master, ex2];

        let grouped = group_objects_by_uid(&events);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].len(), 3);
        assert!(
            grouped[0][0].recurrence_id.is_none(),
            "master (recurrence_id None) must be first per RFC 5545 §3.6.1 convention"
        );
        assert!(grouped[0][1].recurrence_id.is_some());
        assert!(grouped[0][2].recurrence_id.is_some());
    }

    #[test]
    fn group_preserves_uid_order_of_first_appearance() {
        // If the input has UIDs in order [A, B, A], the output's
        // group order is [A, B] — first-appearance wins.
        let a1 = dto("A", false, "");
        let b = dto("B", false, "");
        let a2 = dto("A", true, "");
        let events = vec![a1, b, a2];

        let grouped = group_objects_by_uid(&events);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0][0].ical_uid, "A");
        assert_eq!(grouped[0].len(), 2);
        assert_eq!(grouped[1][0].ical_uid, "B");
        assert_eq!(grouped[1].len(), 1);
    }

    #[test]
    fn group_empty_input_yields_empty_output() {
        let events: Vec<CalendarEventDto> = vec![];
        assert!(group_objects_by_uid(&events).is_empty());
    }

    // ── bundle_to_calendar_body ───────────────────────────────

    #[test]
    fn bundle_body_wraps_all_vevents_in_one_vcalendar() {
        let master = dto(
            "u",
            false,
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:u\r\nSUMMARY:Master\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let exception = dto(
            "u",
            true,
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:u\r\nSUMMARY:Override\r\nRECURRENCE-ID:20260103T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let bundle: Vec<&CalendarEventDto> = vec![&master, &exception];

        let body = bundle_to_calendar_body(&bundle);
        assert!(body.starts_with("BEGIN:VCALENDAR"));
        assert!(body.trim_end().ends_with("END:VCALENDAR"));
        assert_eq!(
            body.matches("BEGIN:VEVENT").count(),
            2,
            "bundle must produce one VEVENT per bundle member"
        );
        assert!(body.contains("SUMMARY:Master"));
        assert!(body.contains("SUMMARY:Override"));
        assert!(
            body.contains("RECURRENCE-ID:20260103T090000Z"),
            "exception RECURRENCE-ID must survive verbatim from stored ical_data"
        );
        assert!(
            body.contains("RRULE:FREQ=DAILY;COUNT=3"),
            "master RRULE must survive verbatim from stored ical_data"
        );
    }

    #[test]
    fn bundle_body_skips_rows_with_malformed_ical_data() {
        // Real world defense: a row whose stored ical_data is
        // corrupt (no VEVENT tag) shouldn't kill the bundle.
        // Emit the good rows; skip the bad one.
        let good = dto(
            "u",
            false,
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u\r\nSUMMARY:OK\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let bad = dto("u", true, "not-an-ical-body");
        let bundle: Vec<&CalendarEventDto> = vec![&good, &bad];

        let body = bundle_to_calendar_body(&bundle);
        assert_eq!(body.matches("BEGIN:VEVENT").count(), 1);
        assert!(body.contains("SUMMARY:OK"));
    }

    #[test]
    fn bundle_body_of_single_row_still_wraps_in_vcalendar() {
        // A non-recurring event is a bundle of one — output shape
        // must remain a valid VCALENDAR body.
        let single = dto(
            "u",
            false,
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u\r\nSUMMARY:Lone\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let bundle: Vec<&CalendarEventDto> = vec![&single];
        let body = bundle_to_calendar_body(&bundle);
        assert!(body.starts_with("BEGIN:VCALENDAR"));
        assert!(body.contains("SUMMARY:Lone"));
        assert_eq!(body.matches("BEGIN:VEVENT").count(), 1);
    }

    // ── VTIMEZONE preservation & dedupe (#689) ────────────────

    /// A stored row written after the #689 fix: the PUT body's
    /// VTIMEZONE (RRULEs included) embedded ahead of the VEVENT.
    const AUCKLAND_VTZ_ROW: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\nBEGIN:VTIMEZONE\r\nTZID:Pacific/Auckland\r\nBEGIN:DAYLIGHT\r\nTZNAME:NZDT\r\nTZOFFSETFROM:+1200\r\nTZOFFSETTO:+1300\r\nDTSTART:19700927T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=9;BYDAY=-1SU\r\nEND:DAYLIGHT\r\nBEGIN:STANDARD\r\nTZNAME:NZST\r\nTZOFFSETFROM:+1300\r\nTZOFFSETTO:+1200\r\nDTSTART:19700405T030000\r\nRRULE:FREQ=YEARLY;BYMONTH=4;BYDAY=1SU\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:u\r\nDTSTART;TZID=Pacific/Auckland:20260115T120000\r\nSUMMARY:s\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn extract_vtimezone_finds_block_with_observances_and_tzid() {
        let chunks = extract_vtimezone_chunks(AUCKLAND_VTZ_ROW);
        assert_eq!(chunks.len(), 1);
        let chunk = chunks[0];
        assert!(chunk.starts_with("BEGIN:VTIMEZONE"));
        assert!(chunk.trim_end().ends_with("END:VTIMEZONE"));
        // Nested observances and their RRULEs ride along byte-exact.
        assert!(chunk.contains("BEGIN:DAYLIGHT"));
        assert!(chunk.contains("BEGIN:STANDARD"));
        assert!(chunk.contains("RRULE:FREQ=YEARLY;BYMONTH=9;BYDAY=-1SU"));
        assert_eq!(vtimezone_tzid(chunk), Some("Pacific/Auckland"));
    }

    #[test]
    fn extract_vtimezone_empty_when_row_predates_the_fix() {
        // Rows written before #689 carry no VTIMEZONE in their shell —
        // extraction must yield nothing, not choke.
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(extract_vtimezone_chunks(body).is_empty());
    }

    #[test]
    fn bundle_dedupes_vtimezone_by_tzid_across_rows() {
        // Master + exception rows each embed the SAME zone definition
        // from their own PUT — the bundled body must emit it once,
        // ahead of the VEVENTs, with RRULEs intact.
        let master = dto("u", false, AUCKLAND_VTZ_ROW);
        let exception = dto("u", true, AUCKLAND_VTZ_ROW);
        let bundle: Vec<&CalendarEventDto> = vec![&master, &exception];

        let body = bundle_to_calendar_body(&bundle);
        assert_eq!(
            body.matches("BEGIN:VTIMEZONE").count(),
            1,
            "same TZID embedded in two rows must be emitted once"
        );
        assert!(body.contains("RRULE:FREQ=YEARLY;BYMONTH=9;BYDAY=-1SU"));
        assert!(body.contains("RRULE:FREQ=YEARLY;BYMONTH=4;BYDAY=1SU"));
        assert_eq!(body.matches("BEGIN:VEVENT").count(), 2);
        assert!(
            body.find("BEGIN:VTIMEZONE").unwrap() < body.find("BEGIN:VEVENT").unwrap(),
            "VTIMEZONE must precede the first VEVENT in the bundle"
        );
    }

    #[test]
    fn bundle_keeps_distinct_tzids() {
        // Two rows anchored in different zones → both definitions.
        let paris_row = "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Paris\r\nBEGIN:STANDARD\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nDTSTART:19701025T030000\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:v\r\nSUMMARY:p\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let a = dto("u", false, AUCKLAND_VTZ_ROW);
        let b = dto("v", false, paris_row);
        let bundle: Vec<&CalendarEventDto> = vec![&a, &b];

        let body = bundle_to_calendar_body(&bundle);
        assert_eq!(body.matches("BEGIN:VTIMEZONE").count(), 2);
        assert!(body.contains("TZID:Pacific/Auckland"));
        assert!(body.contains("TZID:Europe/Paris"));
    }

    // ── VTODO rows (#754) ─────────────────────────────────────

    /// One CalendarTodoDto builder for the todo-side tests — the
    /// VTODO twin of `dto()` above.
    fn todo_dto(uid: &str, is_exception: bool, ical: &str) -> CalendarTodoDto {
        use chrono::Utc;
        CalendarTodoDto {
            id: "todo-".to_string() + uid,
            calendar_id: "cal".to_string(),
            summary: Some("t".to_string()),
            description: None,
            location: None,
            status: Some("NEEDS-ACTION".to_string()),
            percent_complete: None,
            priority: Some(1),
            start_time: None,
            due_time: Some(Utc::now()),
            completed_at: None,
            all_day: false,
            rrule: None,
            ical_uid: uid.to_string(),
            recurrence_id: if is_exception { Some(Utc::now()) } else { None },
            ical_data: ical.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn extract_object_chunk_pulls_vtodo_for_todo_rows() {
        let row = todo_dto(
            "t",
            false,
            "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:t\r\nSUMMARY:Buy milk\r\nPRIORITY:1\r\nEND:VTODO\r\nEND:VCALENDAR\r\n",
        );
        let chunk = extract_object_chunk(&row).expect("VTODO present");
        assert!(chunk.starts_with("BEGIN:VTODO"));
        assert!(chunk.contains("PRIORITY:1"));
        assert!(chunk.trim_end().ends_with("END:VTODO"));
    }

    #[test]
    fn bundle_body_of_todo_rows_serves_vtodo_verbatim() {
        // The bundle emitter must serve VTODO chunks from todo rows
        // with every property (PRIORITY included) intact — the
        // "blob authoritative, columns index-only" contract.
        let master = todo_dto(
            "t",
            false,
            "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:t\r\nSUMMARY:Weekly chore\r\nPRIORITY:2\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nEND:VTODO\r\nEND:VCALENDAR\r\n",
        );
        let exception = todo_dto(
            "t",
            true,
            "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:t\r\nSUMMARY:Weekly chore — moved\r\nRECURRENCE-ID:20260928T090000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n",
        );
        let bundle: Vec<&CalendarTodoDto> = vec![&master, &exception];

        let body = bundle_to_calendar_body(&bundle);
        assert!(body.starts_with("BEGIN:VCALENDAR"));
        assert_eq!(body.matches("BEGIN:VTODO").count(), 2);
        assert!(body.contains("PRIORITY:2"));
        assert!(body.contains("RRULE:FREQ=WEEKLY;COUNT=4"));
        assert!(body.contains("RECURRENCE-ID:20260928T090000Z"));
        // No phantom VEVENT from a todo-only bundle.
        assert!(!body.contains("BEGIN:VEVENT"));
    }

    #[test]
    fn group_objects_by_uid_works_for_todo_rows() {
        let ex = todo_dto("t", true, "");
        let master = todo_dto("t", false, "");
        let rows = vec![ex, master];
        let grouped = group_objects_by_uid(&rows);
        assert_eq!(grouped.len(), 1);
        assert!(
            grouped[0][0].recurrence_id.is_none(),
            "master must sort first within the UID bundle"
        );
    }
}

#[cfg(test)]
mod time_range_parser_tests {
    use super::*;

    // ── parse_caldav_datetime ─────────────────────────────────

    #[test]
    fn ical_date_time_utc_form_parses() {
        // Standard shape per RFC 4791 §9.9 / RFC 5545 §3.3.5 —
        // what every real CalDAV client sends.
        let parsed = parse_caldav_datetime("20260103T090000Z").expect("iCal DATE-TIME must parse");
        assert_eq!(parsed.to_rfc3339(), "2026-01-03T09:00:00+00:00");
    }

    #[test]
    fn rfc3339_form_parses_as_fallback() {
        // Defensive fallback for the rare client that emits
        // dashes+colons. Retained so behaviour is a superset of
        // the pre-fix parser (which accepted only this shape).
        let parsed = parse_caldav_datetime("2026-01-03T09:00:00Z").expect("RFC 3339 fallback");
        assert_eq!(parsed.to_rfc3339(), "2026-01-03T09:00:00+00:00");
    }

    #[test]
    fn ical_and_rfc3339_agree_on_same_instant() {
        // Sanity: the two accepted forms represent the same
        // instant when they describe the same wall time.
        let a = parse_caldav_datetime("20260103T090000Z").unwrap();
        let b = parse_caldav_datetime("2026-01-03T09:00:00Z").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn empty_string_returns_none() {
        assert!(parse_caldav_datetime("").is_none());
    }

    #[test]
    fn malformed_returns_none() {
        // Neither iCal nor RFC 3339 shape — parser must reject
        // without panicking. The caller treats None as "no
        // time-range attribute provided", falling through to the
        // unfiltered event listing (same as the pre-fix
        // behaviour on unparseable input — but at least now we
        // reach that branch by intent, not by silent parse loss).
        assert!(parse_caldav_datetime("not-a-datetime").is_none());
        assert!(parse_caldav_datetime("20260103").is_none()); // date only, no time
        assert!(parse_caldav_datetime("20260103T090000").is_none()); // missing Z
    }

    // ── parse_report — end-to-end integration ─────────────────

    #[test]
    fn calendar_query_with_ical_time_range_captures_both_bounds() {
        // The end-to-end regression: a calendar-query REPORT
        // with iCal DATE-TIME `time-range` attributes MUST
        // surface both bounds as Some in `CalDavReportType::
        // CalendarQuery { time_range, .. }`. Pre-fix this test
        // would have seen `time_range = None` because
        // parse_from_rfc3339 rejected `20260101T093000Z`.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/><C:calendar-data/></D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="20260101T093000Z" end="20260101T120000Z"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#;

        let report = CalDavAdapter::parse_report(xml.as_bytes()).expect("REPORT parses");

        match report {
            CalDavReportType::CalendarQuery { time_range, .. } => {
                let (start, end) = time_range
                    .expect("iCal DATE-TIME time-range must parse as Some; got None (regression)");
                assert_eq!(start.to_rfc3339(), "2026-01-01T09:30:00+00:00");
                assert_eq!(end.to_rfc3339(), "2026-01-01T12:00:00+00:00");
            }
            other => panic!("Expected CalendarQuery, got {:?}", other),
        }
    }

    #[test]
    fn calendar_query_without_time_range_has_none() {
        // Baseline: a filter-less calendar-query still produces
        // CalendarQuery with time_range=None. Guards against a
        // fix that overreaches and starts inventing time bounds.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/><C:calendar-data/></D:prop>
</C:calendar-query>"#;

        let report = CalDavAdapter::parse_report(xml.as_bytes()).expect("REPORT parses");
        match report {
            CalDavReportType::CalendarQuery { time_range, .. } => {
                assert!(time_range.is_none());
            }
            other => panic!("Expected CalendarQuery, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod comp_filter_parser_tests {
    //! `comp-filter` name capture on calendar-query (#754): the inner
    //! component name (VEVENT / VTODO) routes the query to the right
    //! object table; the structural VCALENDAR level is skipped.
    use super::*;

    fn parse(xml: &str) -> CalDavReportType {
        CalDavAdapter::parse_report(std::io::Cursor::new(xml)).expect("report must parse")
    }

    fn comp_of(report: &CalDavReportType) -> Option<String> {
        match report {
            CalDavReportType::CalendarQuery { comp, .. } => comp.clone(),
            other => panic!("Expected CalendarQuery, got {:?}", other),
        }
    }

    #[test]
    fn vtodo_comp_filter_is_captured() {
        // The Tasks.org / DAVx⁵ shape: only VTODO components wanted.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/><C:calendar-data/></D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VTODO">
        <C:time-range start="20260901T000000Z" end="20261001T000000Z"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#;
        let report = parse(xml);
        assert_eq!(comp_of(&report).as_deref(), Some("VTODO"));
        // The time-range still parses alongside the comp name.
        match report {
            CalDavReportType::CalendarQuery { time_range, .. } => {
                assert!(time_range.is_some());
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn vevent_comp_filter_is_captured() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/></D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT"/>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#;
        assert_eq!(comp_of(&parse(xml)).as_deref(), Some("VEVENT"));
    }

    #[test]
    fn filter_less_query_has_no_comp_restriction() {
        // No <C:filter> at all → every stored kind matches (#754).
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/></D:prop>
</C:calendar-query>"#;
        assert_eq!(comp_of(&parse(xml)), None);
    }

    #[test]
    fn bare_vcalendar_comp_filter_is_not_a_kind_restriction() {
        // A filter that only names the structural level carries no
        // component restriction either.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/></D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR"/>
  </C:filter>
</C:calendar-query>"#;
        assert_eq!(comp_of(&parse(xml)), None);
    }

    #[test]
    fn comp_filter_name_is_case_normalized() {
        // RFC 5545 component names are case-insensitive; the router
        // compares against the uppercase form.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/></D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="vtodo"/>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#;
        assert_eq!(comp_of(&parse(xml)).as_deref(), Some("VTODO"));
    }
}

#[cfg(test)]
mod prop_el_tests {
    use super::caldav_prop_el;

    /// Known namespaces map to the prefixes declared on the multistatus
    /// root — no inline declaration needed.
    #[test]
    fn known_namespaces_use_root_prefixes() {
        for (ns, name, expected) in [
            ("DAV:", "displayname", "D:displayname"),
            (
                "urn:ietf:params:xml:ns:caldav",
                "calendar-home-set",
                "C:calendar-home-set",
            ),
            ("http://calendarserver.org/ns/", "getctag", "CS:getctag"),
        ] {
            let el = caldav_prop_el(ns, name);
            assert_eq!(el.name().as_ref(), expected.as_bytes());
            assert!(el.try_get_attribute("xmlns:U").unwrap().is_none());
        }
    }

    /// A foreign namespace must yield a legal XML name plus an inline
    /// namespace declaration. The old code glued the URI into the element
    /// name (`<http://inf-it.com/ns/dav/:settings/>`), which is invalid
    /// XML and made strict clients (InfCloud/CalDavZAP) abort the whole
    /// PROPFIND parse.
    #[test]
    fn foreign_namespace_gets_inline_declaration() {
        let el = caldav_prop_el("http://inf-it.com/ns/dav/", "settings");
        assert_eq!(el.name().as_ref(), b"U:settings");
        let attr = el
            .try_get_attribute("xmlns:U")
            .unwrap()
            .expect("xmlns:U declared");
        assert_eq!(attr.value.as_ref(), b"http://inf-it.com/ns/dav/");
    }
}
