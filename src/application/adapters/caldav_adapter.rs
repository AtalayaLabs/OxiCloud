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
use crate::application::dtos::calendar_dto::{CalendarDto, CalendarEventDto};

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

/// Extract the `BEGIN:VEVENT` ... `END:VEVENT` slice from a
/// stored `ical_data` body (as returned by the storage layer —
/// one full VCALENDAR per row).
///
/// Case-insensitive on the tag names per RFC 5545 §3.1. Includes
/// the `BEGIN:VEVENT` and `END:VEVENT` lines themselves. Returns
/// `None` if either tag is missing (malformed body) so callers
/// can fall back safely.
pub(crate) fn extract_vevent_chunk(ical_data: &str) -> Option<&str> {
    // Byte index of the first ASCII-case-insensitive occurrence of
    // `needle` in `hay` at or after `from`. Every stored body OxiCloud
    // itself writes carries uppercase tags, so try the memchr-backed
    // exact `find` first; only genuinely mixed-case foreign bodies pay
    // the manual scan. Either way this replaces the old
    // `to_ascii_uppercase()` of the ENTIRE body — one full-copy String
    // allocation per event per REPORT/GET, done purely to locate two
    // tags.
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

    let begin = find_ci(ical_data, "BEGIN:VEVENT", 0)?;
    // End marker: the first END:VEVENT after `begin`, plus the length
    // of "END:VEVENT" itself, then any immediate CRLF/LF to include
    // the terminator line.
    let rel_end = find_ci(ical_data, "END:VEVENT", begin)?;
    let end_tag_end = rel_end + "END:VEVENT".len();
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

/// Group a slice of events by `ical_uid`, preserving the order of
/// first appearance for the groups themselves, and placing the
/// master (`recurrence_id.is_none()`) first within each group per
/// RFC 5545 §3.6.1 convention. Ties among exceptions preserve the
/// original slice order.
///
/// Used by the read-side emitters to fold master + per-instance
/// override rows into a single calendar-object-resource, matching
/// the "one URL per UID" contract of RFC 4791 §4.1.
pub(crate) fn group_events_by_uid<'a>(
    events: &'a [CalendarEventDto],
) -> Vec<Vec<&'a CalendarEventDto>> {
    // Keys borrow from the DTO slice (which outlives every local) — the
    // old String-keyed map cloned every event's UID (twice for first
    // appearances) on every REPORT / collection PROPFIND / GET.
    let mut order: Vec<&'a str> = Vec::new();
    let mut buckets: std::collections::HashMap<&'a str, Vec<&'a CalendarEventDto>> =
        std::collections::HashMap::new();

    for event in events {
        let key = event.ical_uid.as_str();
        match buckets.entry(key) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                order.push(key);
                slot.insert(vec![event]);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => slot.get_mut().push(event),
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for uid in order {
        let mut bucket = buckets.remove(uid).unwrap_or_default();
        // Master first (recurrence_id None), exceptions in insertion order.
        bucket.sort_by_key(|e| e.recurrence_id.is_some());
        out.push(bucket);
    }
    out
}

/// Build the calendar-object-resource body for a bundle (master +
/// N exception overrides sharing the same UID). Serves each row's
/// stored `ical_data` verbatim, extracting the VEVENT chunk and
/// wrapping the concatenation in a single VCALENDAR shell.
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
pub(crate) fn bundle_to_calendar_body(bundle: &[&CalendarEventDto]) -> String {
    let mut buf = String::with_capacity(256 + bundle.len() * 320);
    buf.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n");
    for event in bundle {
        if let Some(chunk) = extract_vevent_chunk(&event.ical_data) {
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
#[derive(Debug, PartialEq)]
pub enum CalDavReportType {
    /// Calendar-query report
    CalendarQuery {
        time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
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

            CalDavReportType::CalendarQuery { time_range, props }
        } else if in_calendar_multiget {
            CalDavReportType::CalendarMultiget { hrefs, props }
        } else if in_sync_collection {
            CalDavReportType::SyncCollection { sync_token, props }
        } else {
            // Default to empty calendar query
            CalDavReportType::CalendarQuery {
                time_range: None,
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
        let href = format!("/caldav/principals/{}/", username);
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
                    "/caldav/principals/{}/",
                    username
                ))))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
                xml_writer.write_event(Event::End(BytesEnd::new("D:current-user-principal")))?;

                // calendar-home-set
                xml_writer.write_event(Event::Start(BytesStart::new("C:calendar-home-set")))?;
                xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
                xml_writer.write_event(Event::Text(BytesText::new(&format!(
                    "/caldav/{}/",
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
                        "/caldav/principals/{}/",
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
                        "/caldav/{}/",
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
                    let prop_name = if prop.namespace == "http://calendarserver.org/ns/" {
                        format!("CS:{}", prop.name)
                    } else if prop.namespace == "urn:ietf:params:xml:ns:caldav" {
                        format!("C:{}", prop.name)
                    } else if prop.namespace == "DAV:" {
                        format!("D:{}", prop.name)
                    } else {
                        format!("{}:{}", prop.namespace, prop.name)
                    };
                    xml_writer.write_event(Event::Empty(BytesStart::new(&prop_name)))?;
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
            "/caldav/{}/",
            username
        ))))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("C:calendar-home-set")))?;

        // current-user-principal (self-reference)
        xml_writer.write_event(Event::Start(BytesStart::new("D:current-user-principal")))?;
        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(&format!(
            "/caldav/principals/{}/",
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
                        "/caldav/principals/{}/",
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
                        "/caldav/{}/",
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
                        "/caldav/principals/{}/",
                        username
                    ))))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;
                    xml_writer
                        .write_event(Event::End(BytesEnd::new("C:calendar-user-address-set")))?;
                }
                _ => {
                    let prop_name = if prop.namespace == "http://calendarserver.org/ns/" {
                        format!("CS:{}", prop.name)
                    } else if prop.namespace == "urn:ietf:params:xml:ns:caldav" {
                        format!("C:{}", prop.name)
                    } else if prop.namespace == "DAV:" {
                        format!("D:{}", prop.name)
                    } else {
                        format!("{}:{}", prop.namespace, prop.name)
                    };
                    xml_writer.write_event(Event::Empty(BytesStart::new(&prop_name)))?;
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

        // Supported calendar component set
        xml_writer.write_event(Event::Start(BytesStart::new(
            "C:supported-calendar-component-set",
        )))?;
        xml_writer.write_event(Event::Empty(
            BytesStart::new("C:comp").with_attributes([("name", "VEVENT")]),
        ))?;
        xml_writer.write_event(Event::End(BytesEnd::new(
            "C:supported-calendar-component-set",
        )))?;

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
                    xml_writer.write_event(Event::Start(BytesStart::new(
                        "C:supported-calendar-component-set",
                    )))?;
                    xml_writer.write_event(Event::Empty(
                        BytesStart::new("C:comp").with_attributes([("name", "VEVENT")]),
                    ))?;
                    xml_writer.write_event(Event::End(BytesEnd::new(
                        "C:supported-calendar-component-set",
                    )))?;
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
                        let prop_name = if prop.namespace == "http://calendarserver.org/ns/" {
                            format!("CS:{}", prop.name)
                        } else if prop.namespace == "urn:ietf:params:xml:ns:caldav" {
                            format!("C:{}", prop.name)
                        } else if prop.namespace == "DAV:" {
                            format!("D:{}", prop.name)
                        } else {
                            format!("{}:{}", prop.namespace, prop.name)
                        };

                        xml_writer.write_event(Event::Start(BytesStart::new(&prop_name)))?;
                        xml_writer.write_event(Event::Text(BytesText::new(value)))?;
                        xml_writer.write_event(Event::End(BytesEnd::new(&prop_name)))?;
                    } else {
                        // Property not found, write empty element
                        let prop_name = if prop.namespace == "http://calendarserver.org/ns/" {
                            format!("CS:{}", prop.name)
                        } else if prop.namespace == "urn:ietf:params:xml:ns:caldav" {
                            format!("C:{}", prop.name)
                        } else if prop.namespace == "DAV:" {
                            format!("D:{}", prop.name)
                        } else {
                            format!("{}:{}", prop.namespace, prop.name)
                        };

                        xml_writer.write_event(Event::Empty(BytesStart::new(&prop_name)))?;
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

    pub fn write_collection_event_page<W: Write>(
        xml_writer: &mut Writer<W>,
        events: &[CalendarEventDto],
        base_href: &str,
    ) -> Result<()> {
        // Reused per-event href buffer (cleared each iteration) so a whole
        // PROPFIND page allocates the href storage once instead of per event
        // (benches/ROUND14.md §A6). The etag no longer needs a buffer — it is
        // emitted via `write_quoted_etag` (borrowed pre-escaped quotes).
        let mut event_href = String::with_capacity(base_href.len() + 48);
        for bundle in group_events_by_uid(events) {
            // The master (sorted first by group_events_by_uid)
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
                format_args!("{}{}.ics", base_href, anchor.ical_uid),
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
            write_quoted_etag(xml_writer, &anchor.id)?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;

            // getcontenttype
            xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
            xml_writer.write_event(Event::Text(BytesText::new(
                "text/calendar; component=vevent",
            )))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

            // getlastmodified — anchor row's updated_at (stack render, §A5)
            xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
            Self::write_lastmodified_text(xml_writer, anchor.updated_at)?;
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

    /// One REPORT page: group `events` per UID and emit one
    /// `D:response` per bundle. Callers guarantee same-UID rows arrive
    /// within a single page (the uid-keyset pager does).
    pub fn write_report_page<W: Write>(
        xml_writer: &mut Writer<W>,
        events: &[CalendarEventDto],
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
        for bundle in group_events_by_uid(events) {
            let anchor = match bundle.first() {
                Some(e) => *e,
                None => continue,
            };
            href.clear();
            let _ = std::fmt::Write::write_fmt(
                &mut href,
                format_args!("{}{}.ics", base_href, anchor.ical_uid),
            );
            Self::write_event_response(xml_writer, &bundle, props, &href)?;
        }
        Ok(())
    }

    /// Generate a response for calendar events.
    ///
    /// `deleted_hrefs` renders each as an RFC 6578 §3.7
    /// `<D:status>HTTP/1.1 404 Not Found</D:status>` sub-response instead
    /// of a `<D:propstat>` block, and `sync_token` (when `Some`) renders
    /// as a trailing `<D:sync-token>` — both empty/`None` for the
    /// calendar-query / calendar-multiget report types, which don't use
    /// either.
    pub fn generate_calendar_events_response<W: Write>(
        writer: W,
        events: &[CalendarEventDto],
        request: &CalDavReportType,
        base_href: &str,
        deleted_hrefs: &[String],
        sync_token: Option<&str>,
    ) -> Result<()> {
        let mut xml_writer = Writer::new(writer);

        Self::write_caldav_multistatus_start(&mut xml_writer)?;

        // Responses folded per UID so a recurring master + exception
        // overrides share ONE D:response (RFC 4791 §4.1) — see
        // `write_report_page`, which the streaming emitters reuse
        // page by page.
        Self::write_report_page(&mut xml_writer, events, request, base_href)?;

        for href in deleted_hrefs {
            Self::write_deleted_event_response(&mut xml_writer, href)?;
        }

        if let Some(token) = sync_token {
            xml_writer.write_event(Event::Start(BytesStart::new("D:sync-token")))?;
            xml_writer.write_event(Event::Text(BytesText::new(token)))?;
            xml_writer.write_event(Event::End(BytesEnd::new("D:sync-token")))?;
        }

        Self::write_caldav_multistatus_end(&mut xml_writer)?;

        Ok(())
    }

    /// RFC 6578 §3.7 removed-member sub-response: a `<D:response>` whose
    /// `<D:status>` is 404, with no `<D:propstat>` block.
    fn write_deleted_event_response<W: Write>(
        xml_writer: &mut Writer<W>,
        href: &str,
    ) -> Result<()> {
        xml_writer.write_event(Event::Start(BytesStart::new("D:response")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:href")))?;
        xml_writer.write_event(Event::Text(BytesText::new(href)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:href")))?;

        xml_writer.write_event(Event::Start(BytesStart::new("D:status")))?;
        xml_writer.write_event(Event::Text(BytesText::new("HTTP/1.1 404 Not Found")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:status")))?;

        xml_writer.write_event(Event::End(BytesEnd::new("D:response")))?;
        Ok(())
    }

    /// Write a bundle (master + exception overrides sharing a
    /// UID) as one D:response. The bundle is emitted at one
    /// href (base + uid.ics); ETag + getlastmodified anchor on
    /// the first bundle entry (which `group_events_by_uid` puts
    /// the master at); calendar-data contains every VEVENT.
    fn write_event_response<W: Write>(
        xml_writer: &mut Writer<W>,
        bundle: &[&CalendarEventDto],
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
    fn write_event_standard_props<W: Write>(
        xml_writer: &mut Writer<W>,
        anchor: &CalendarEventDto,
        bundle: &[&CalendarEventDto],
    ) -> Result<()> {
        // Common WebDAV properties

        // Resource type (empty for non-collection)
        xml_writer.write_event(Event::Empty(BytesStart::new("D:resourcetype")))?;

        // ETag anchored on the master (or first exception in a master-less
        // bundle — pathological state today). Borrowed pre-escaped quotes
        // (§C1/§R4), 0 allocs/event.
        xml_writer.write_event(Event::Start(BytesStart::new("D:getetag")))?;
        write_quoted_etag(xml_writer, &anchor.id)?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;

        // Content type
        xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
        xml_writer.write_event(Event::Text(BytesText::new(
            "text/calendar; component=VEVENT",
        )))?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;

        // Last modified (stack render, benches/ROUND14.md §A5)
        xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
        Self::write_lastmodified_text(xml_writer, anchor.updated_at)?;
        xml_writer.write_event(Event::End(BytesEnd::new("D:getlastmodified")))?;

        // CalDAV calendar-data — the whole bundle emitted as one
        // VCALENDAR by extracting each row's stored VEVENT chunk
        // verbatim. Every property (ATTENDEE / VALARM / CATEGORIES
        // / STATUS / X-* / RECURRENCE-ID on exception rows)
        // survives because we no longer regenerate from DTO
        // fields.
        xml_writer.write_event(Event::Start(BytesStart::new("C:calendar-data")))?;
        let ical_data = bundle_to_calendar_body(bundle);
        xml_writer.write_event(Event::Text(BytesText::new(&ical_data)))?;
        xml_writer.write_event(Event::End(BytesEnd::new("C:calendar-data")))?;

        Ok(())
    }

    /// Write requested event properties
    fn write_event_requested_props<W: Write>(
        xml_writer: &mut Writer<W>,
        anchor: &CalendarEventDto,
        bundle: &[&CalendarEventDto],
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
                    write_quoted_etag(xml_writer, &anchor.id)?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getetag")))?;
                }
                ("DAV:", "getcontenttype") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getcontenttype")))?;
                    xml_writer.write_event(Event::Text(BytesText::new(
                        "text/calendar; component=VEVENT",
                    )))?;
                    xml_writer.write_event(Event::End(BytesEnd::new("D:getcontenttype")))?;
                }
                ("DAV:", "getlastmodified") => {
                    xml_writer.write_event(Event::Start(BytesStart::new("D:getlastmodified")))?;
                    // Stack render (benches/ROUND14.md §A5).
                    Self::write_lastmodified_text(xml_writer, anchor.updated_at)?;
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
                    let prop_name = if prop.namespace == "http://calendarserver.org/ns/" {
                        format!("CS:{}", prop.name)
                    } else if prop.namespace == "urn:ietf:params:xml:ns:caldav" {
                        format!("C:{}", prop.name)
                    } else if prop.namespace == "DAV:" {
                        format!("D:{}", prop.name)
                    } else {
                        format!("{}:{}", prop.namespace, prop.name)
                    };

                    xml_writer.write_event(Event::Empty(BytesStart::new(&prop_name)))?;
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

    pub fn group_events_by_uid(events: &[CalendarEventDto]) -> Vec<Vec<&CalendarEventDto>> {
        super::group_events_by_uid(events)
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

    // ── group_events_by_uid ───────────────────────────────────

    #[test]
    fn group_places_master_first_within_each_uid() {
        // Mixed order: exception first, then master, then a
        // second exception. Result: [master, exception1, exception2].
        let ex1 = dto("u1", true, "");
        let master = dto("u1", false, "");
        let ex2 = dto("u1", true, "");
        let events = vec![ex1, master, ex2];

        let grouped = group_events_by_uid(&events);
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

        let grouped = group_events_by_uid(&events);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0][0].ical_uid, "A");
        assert_eq!(grouped[0].len(), 2);
        assert_eq!(grouped[1][0].ical_uid, "B");
        assert_eq!(grouped[1].len(), 1);
    }

    #[test]
    fn group_empty_input_yields_empty_output() {
        let events: Vec<CalendarEventDto> = vec![];
        assert!(group_events_by_uid(&events).is_empty());
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
