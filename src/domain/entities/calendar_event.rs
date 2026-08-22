use chrono::{DateTime, Duration, TimeZone, Utc};
/**
 * Calendar Event Entity
 *
 * This module defines the CalendarEvent entity, which represents an event or
 * appointment in a calendar, following the iCalendar (RFC 5545) specification.
 *
 * Calendar events have properties like summary, description, location, start/end times,
 * and can include recurrence rules for repeating events. Each event belongs to a
 * specific calendar and stores its complete iCalendar representation.
 */
use uuid::Uuid;

use crate::common::errors::{DomainError, ErrorKind, Result};

// Re-export entity errors from the centralized module
pub use super::entity_errors::CalendarEventError;

/**
 * CalendarEvent entity.
 *
 * Represents a calendar event or appointment that can be synced via CalDAV.
 * Follows the iCalendar format (RFC 5545) for compatibility with CalDAV clients.
 */
/// Owned decomposition of a [`CalendarEvent`] (see
/// [`CalendarEvent::into_parts`]).
pub struct CalendarEventParts {
    pub id: Uuid,
    pub calendar_id: Uuid,
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub all_day: bool,
    pub rrule: Option<String>,
    pub recurrence_id: Option<DateTime<Utc>>,
    pub ical_uid: String,
    pub ical_data: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct CalendarEvent {
    /// Unique identifier for the event
    id: Uuid,

    /// ID of the calendar this event belongs to
    calendar_id: Uuid,

    /// Short summary/title of the event
    summary: String,

    /// Detailed description of the event (optional)
    description: Option<String>,

    /// Location of the event (optional)
    location: Option<String>,

    /// Start time of the event
    start_time: DateTime<Utc>,

    /// End time of the event
    end_time: DateTime<Utc>,

    /// Whether this is an all-day event
    all_day: bool,

    /// Recurrence rule in iCalendar RRULE format (optional)
    rrule: Option<String>,

    /// RECURRENCE-ID (RFC 5545 §3.8.4.4) — non-NULL on exception
    /// instances of a recurring event, NULL on the master.
    ///
    /// When a client (Thunderbird, Apple Calendar, Gnome Calendar, …)
    /// modifies a SINGLE occurrence of a recurring event, it sends
    /// a separate VEVENT that shares the master's UID and carries
    /// a `RECURRENCE-ID` identifying which occurrence is being
    /// overridden. That per-instance override lives as its own row
    /// in `caldav.calendar_events`; the master row keeps NULL here.
    ///
    /// Lookup key is `(calendar_id, ical_uid, recurrence_id)` —
    /// enforced at the DB layer by two partial unique indexes:
    ///
    ///   * `(calendar_id, ical_uid) WHERE recurrence_id IS NULL` —
    ///     at most one master per UID per calendar.
    ///   * `(calendar_id, ical_uid, recurrence_id) WHERE
    ///     recurrence_id IS NOT NULL` — at most one override for a
    ///     given (master, instance) pair.
    ///
    /// See AtalayaLabs/OxiCloud#528 for the ticket that motivated
    /// this field, and `docs/plan/` (future) for the full model.
    recurrence_id: Option<DateTime<Utc>>,

    /// Unique identifier in iCalendar format (used for CalDAV sync)
    ical_uid: String,

    /// Complete iCalendar data (VEVENT component)
    ical_data: String,

    /// Time when the event was created
    created_at: DateTime<Utc>,

    /// Time when the event was last modified
    updated_at: DateTime<Utc>,
}

impl CalendarEvent {
    /**
     * Creates a new calendar event with the given properties.
     *
     * @param calendar_id ID of the calendar this event belongs to
     * @param summary Short summary/title of the event
     * @param description Detailed description of the event (optional)
     * @param location Location of the event (optional)
     * @param start_time Start time of the event
     * @param end_time End time of the event
     * @param all_day Whether this is an all-day event
     * @param rrule Recurrence rule in iCalendar RRULE format (optional)
     * @param ical_data Complete iCalendar data (VEVENT component)
     * @return Result containing the new CalendarEvent or a domain error
     */
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        calendar_id: Uuid,
        summary: String,
        description: Option<String>,
        location: Option<String>,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        all_day: bool,
        rrule: Option<String>,
        ical_data: String,
    ) -> Result<Self> {
        // Validate inputs
        if summary.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "Event summary cannot be empty",
            ));
        }

        if end_time < start_time {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "End time cannot be before start time",
            ));
        }

        // Validate RRULE if provided (basic validation)
        if let Some(ref rule) = rrule
            && !rule.starts_with("FREQ=")
        {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "Recurrence rule must start with FREQ=",
            ));
        }

        // Validate iCalendar data (basic validation)
        if !ical_data.contains("BEGIN:VEVENT") || !ical_data.contains("END:VEVENT") {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "iCalendar data must contain a VEVENT component",
            ));
        }

        let now = Utc::now();

        Ok(Self {
            id: Uuid::new_v4(),
            calendar_id,
            summary,
            description,
            location,
            start_time,
            end_time,
            all_day,
            rrule,
            recurrence_id: None,
            ical_uid: Uuid::new_v4().to_string(),
            ical_data,
            created_at: now,
            updated_at: now,
        })
    }

    /**
     * Creates a calendar event with specific ID and timestamps.
     * Typically used when reconstructing from storage.
     *
     * @param id Unique identifier for the event
     * @param calendar_id ID of the calendar this event belongs to
     * @param summary Short summary/title of the event
     * @param description Detailed description of the event (optional)
     * @param location Location of the event (optional)
     * @param start_time Start time of the event
     * @param end_time End time of the event
     * @param all_day Whether this is an all-day event
     * @param rrule Recurrence rule in iCalendar RRULE format (optional)
     * @param ical_uid Unique identifier in iCalendar format
     * @param ical_data Complete iCalendar data (VEVENT component)
     * @param created_at Time when the event was created
     * @param updated_at Time when the event was last modified
     * @return Result containing the new CalendarEvent or a domain error
     */
    #[allow(clippy::too_many_arguments)]
    pub fn with_id(
        id: Uuid,
        calendar_id: Uuid,
        summary: String,
        description: Option<String>,
        location: Option<String>,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        all_day: bool,
        rrule: Option<String>,
        ical_uid: String,
        ical_data: String,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self> {
        // Basic validation
        if summary.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "Event summary cannot be empty",
            ));
        }

        if end_time < start_time {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "End time cannot be before start time",
            ));
        }

        Ok(Self {
            id,
            calendar_id,
            summary,
            description,
            location,
            start_time,
            end_time,
            all_day,
            rrule,
            recurrence_id: None,
            ical_uid,
            ical_data,
            created_at,
            updated_at,
        })
    }

    /**
     * Creates a calendar event from an iCalendar VEVENT component.
     * Parses the iCalendar data to extract event properties.
     *
     * @param calendar_id ID of the calendar this event belongs to
     * @param ical_data Complete iCalendar data (VEVENT component)
     * @return Result containing the new CalendarEvent or a domain error
     */
    pub fn from_ical(calendar_id: Uuid, ical_data: String) -> Result<Self> {
        // Parse the body ONCE and read every property from the parsed
        // component. The previous shape funnelled each of the 8 property
        // lookups below through `extract_ical_property[_with_params]`,
        // which re-ran the full `IcalParser` (line unfolding + component
        // tree build) per property — 8 complete parses per VEVENT on
        // every CalDAV PUT / import. A missing-or-unparseable body maps
        // to the same "Missing SUMMARY" error the old first lookup
        // produced, preserving error parity.
        let event = Self::parse_first_vevent(&ical_data);

        // Extract required fields from the parsed component
        let summary = event
            .as_ref()
            .and_then(|e| Self::prop_value(e, "SUMMARY"))
            .ok_or_else(|| {
                DomainError::new(
                    ErrorKind::InvalidInput,
                    "CalendarEvent",
                    "Missing SUMMARY in iCalendar data",
                )
            })?;
        let event = event.expect("prop_value returned Some, so the parse succeeded");

        // DTSTART / DTEND: use the params-aware extractor so we can
        // detect `VALUE=DATE` (all-day) from the property parameters
        // rather than scanning the raw property line. The pre-parser-
        // rewrite substring scan couldn't see param-carrying lines at
        // all — see #528.
        // DTSTART carries the value AND the all-day flag: a `VALUE=DATE`
        // parameter (RFC 5545 §3.3.4) means date-only. Strict — only "DATE"
        // (case-insensitive) counts; "DATE-TIME" and anything else is timed.
        // The flag drives both the DTSTART and the DTEND datetime parse below.
        let (dtstart_value, all_day) =
            Self::prop_value_and_is_date(&event, "DTSTART").ok_or_else(|| {
                DomainError::new(
                    ErrorKind::InvalidInput,
                    "CalendarEvent",
                    "Missing DTSTART in iCalendar data",
                )
            })?;

        // DTEND needs only its value (the all-day flag comes from DTSTART).
        let dtend_value = Self::prop_value(&event, "DTEND").ok_or_else(|| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "Missing DTEND in iCalendar data",
            )
        })?;

        let start_time = Self::parse_ical_datetime(&dtstart_value, all_day).map_err(|e| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                format!("Invalid DTSTART: {}", e),
            )
        })?;

        let end_time = Self::parse_ical_datetime(&dtend_value, all_day).map_err(|e| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                format!("Invalid DTEND: {}", e),
            )
        })?;

        // Extract optional fields
        let description = Self::prop_value(&event, "DESCRIPTION");
        let location = Self::prop_value(&event, "LOCATION");
        let rrule = Self::prop_value(&event, "RRULE");

        // Extract UID or generate a new one
        let ical_uid =
            Self::prop_value(&event, "UID").unwrap_or_else(|| Uuid::new_v4().to_string());

        // RECURRENCE-ID (RFC 5545 §3.8.4.4). When present, this VEVENT
        // is an override for a specific occurrence of a recurring
        // master with the same UID. The parameter tells us whether the
        // value is a date (all-day master) or datetime (timed master).
        // A parse failure here downgrades to `None` — the VEVENT still
        // gets stored, just as a plain event (worst case a client sync
        // treats it as a new master, which the DB uniqueness will
        // refuse; better a persistence error than a silent split).
        let recurrence_id = match Self::prop_value_and_is_date(&event, "RECURRENCE-ID") {
            Some((value, is_date)) => Self::parse_ical_datetime(&value, is_date).ok(),
            None => None,
        };

        let now = Utc::now();

        Ok(Self {
            id: Uuid::new_v4(),
            calendar_id,
            summary,
            description,
            location,
            start_time,
            end_time,
            all_day,
            rrule,
            recurrence_id,
            ical_uid,
            ical_data,
            created_at: now,
            updated_at: now,
        })
    }

    // Getters

    /// Returns the event's unique identifier
    pub fn id(&self) -> &Uuid {
        &self.id
    }

    /// Returns the ID of the calendar this event belongs to
    pub fn calendar_id(&self) -> &Uuid {
        &self.calendar_id
    }

    /// Returns the event's summary/title
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Returns the event's description, if any
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Returns the event's location, if any
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    /// Returns the event's start time
    pub fn start_time(&self) -> &DateTime<Utc> {
        &self.start_time
    }

    /// Returns the event's end time
    pub fn end_time(&self) -> &DateTime<Utc> {
        &self.end_time
    }

    /// Returns whether this is an all-day event
    pub fn all_day(&self) -> bool {
        self.all_day
    }

    /// Returns the event's recurrence rule, if any
    pub fn rrule(&self) -> Option<&str> {
        self.rrule.as_deref()
    }

    /// Returns the event's iCalendar UID
    /// Returns the RECURRENCE-ID for this event, if any. `None` on
    /// masters and standalone (non-recurring) events; `Some` on
    /// exception overrides that target a specific occurrence of a
    /// recurring master with the same `ical_uid`.
    pub fn recurrence_id(&self) -> Option<&DateTime<Utc>> {
        self.recurrence_id.as_ref()
    }

    /// Set the RECURRENCE-ID on this event. Used by the repository
    /// layer when reconstructing an entity from a stored row (the
    /// column is read straight into the field — no re-parse of the
    /// ical_data body). Passing `None` clears the marker, promoting
    /// an exception back to a plain event.
    pub fn set_recurrence_id(&mut self, recurrence_id: Option<DateTime<Utc>>) {
        self.recurrence_id = recurrence_id;
        self.updated_at = Utc::now();
    }

    pub fn ical_uid(&self) -> &str {
        &self.ical_uid
    }

    /// Returns the complete iCalendar data for the event
    pub fn ical_data(&self) -> &str {
        &self.ical_data
    }

    /// Returns the time when the event was created
    pub fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    /// Returns the time when the event was last modified
    pub fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    /// Decompose into owned parts for DTO conversion — the `File`/`Folder`/
    /// `Contact` pattern. Moving the owned `String`s (most importantly the
    /// unbounded `ical_data` blob, ~11 KB with attendees/VALARMs) replaces
    /// the per-event deep copies `CalendarEventDto::from` used to make via
    /// getters (benches/ROUND11.md §19).
    pub fn into_parts(self) -> CalendarEventParts {
        CalendarEventParts {
            id: self.id,
            calendar_id: self.calendar_id,
            summary: self.summary,
            description: self.description,
            location: self.location,
            start_time: self.start_time,
            end_time: self.end_time,
            all_day: self.all_day,
            rrule: self.rrule,
            recurrence_id: self.recurrence_id,
            ical_uid: self.ical_uid,
            ical_data: self.ical_data,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    /// Returns the duration of the event
    pub fn duration(&self) -> Duration {
        self.end_time - self.start_time
    }

    // Setters and Mutators

    /**
     * Updates the event's summary/title.
     *
     * @param summary New summary/title for the event
     * @return Result indicating success or containing a domain error
     */
    pub fn update_summary(&mut self, summary: String) -> Result<()> {
        if summary.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "Event summary cannot be empty",
            ));
        }

        // Clone the summary before updating the struct
        let summary_clone = summary.clone();
        self.summary = summary;
        self.updated_at = Utc::now();

        // Update iCalendar data using the cloned value
        self.update_ical_property("SUMMARY", &summary_clone);

        Ok(())
    }

    /**
     * Updates the event's description.
     *
     * @param description New description for the event
     */
    pub fn update_description(&mut self, description: Option<String>) {
        self.description = description.clone();
        self.updated_at = Utc::now();

        // Update iCalendar data
        match description {
            Some(desc) => self.update_ical_property("DESCRIPTION", &desc),
            None => self.remove_ical_property("DESCRIPTION"),
        }
    }

    /**
     * Updates the event's location.
     *
     * @param location New location for the event
     */
    pub fn update_location(&mut self, location: Option<String>) {
        self.location = location.clone();
        self.updated_at = Utc::now();

        // Update iCalendar data
        match location {
            Some(loc) => self.update_ical_property("LOCATION", &loc),
            None => self.remove_ical_property("LOCATION"),
        }
    }

    /**
     * Updates the event's start and end times.
     *
     * @param start_time New start time for the event
     * @param end_time New end time for the event
     * @return Result indicating success or containing a domain error
     */
    pub fn update_time_range(
        &mut self,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
    ) -> Result<()> {
        if end_time < start_time {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "End time cannot be before start time",
            ));
        }

        self.start_time = start_time;
        self.end_time = end_time;
        self.updated_at = Utc::now();

        // Update iCalendar data. Timed events stack-render the compact UTC
        // stamp via `fmt::compact_ical_utc` (the ROUND19 §V2 pattern: drops
        // chrono's `%Y%m%dT%H%M%SZ` strftime interpreter — ~3 → 0 allocs each),
        // with chrono kept as the out-of-range fallback. All-day keeps its
        // `%Y%m%d` + literal-suffix form. Byte-identical output either way.
        let (mut sbuf, mut ebuf) = ([0u8; 16], [0u8; 16]);
        let (start_owned, end_owned);
        let start_str: &str = if self.all_day {
            start_owned = format!("{}T000000Z", start_time.format("%Y%m%d"));
            &start_owned
        } else if let Some(s) =
            crate::common::fmt::compact_ical_utc(&mut sbuf, start_time.timestamp())
        {
            s
        } else {
            start_owned = format!("{}", start_time.format("%Y%m%dT%H%M%SZ"));
            &start_owned
        };
        let end_str: &str = if self.all_day {
            end_owned = format!("{}T000000Z", end_time.format("%Y%m%d"));
            &end_owned
        } else if let Some(e) =
            crate::common::fmt::compact_ical_utc(&mut ebuf, end_time.timestamp())
        {
            e
        } else {
            end_owned = format!("{}", end_time.format("%Y%m%dT%H%M%SZ"));
            &end_owned
        };

        self.update_ical_property("DTSTART", start_str);
        self.update_ical_property("DTEND", end_str);

        Ok(())
    }

    /**
     * Updates whether this is an all-day event.
     *
     * @param all_day Whether this is an all-day event
     */
    pub fn update_all_day(&mut self, all_day: bool) {
        self.all_day = all_day;
        self.updated_at = Utc::now();

        // Update iCalendar data. Timed events stack-render the compact UTC
        // stamp via `fmt::compact_ical_utc` (drops chrono's `%Y%m%dT%H%M%SZ`
        // strftime interpreter — ~3 → 0 allocs each), chrono fallback out of
        // range. All-day keeps its `VALUE=DATE:` + `%Y%m%d` form. Byte-identical.
        let (mut sbuf, mut ebuf) = ([0u8; 16], [0u8; 16]);
        let (start_owned, end_owned);
        let start_str: &str = if all_day {
            start_owned = format!("VALUE=DATE:{}", self.start_time.format("%Y%m%d"));
            &start_owned
        } else if let Some(s) =
            crate::common::fmt::compact_ical_utc(&mut sbuf, self.start_time.timestamp())
        {
            s
        } else {
            start_owned = format!("{}", self.start_time.format("%Y%m%dT%H%M%SZ"));
            &start_owned
        };
        let end_str: &str = if all_day {
            end_owned = format!("VALUE=DATE:{}", self.end_time.format("%Y%m%d"));
            &end_owned
        } else if let Some(e) =
            crate::common::fmt::compact_ical_utc(&mut ebuf, self.end_time.timestamp())
        {
            e
        } else {
            end_owned = format!("{}", self.end_time.format("%Y%m%dT%H%M%SZ"));
            &end_owned
        };

        self.update_ical_property("DTSTART", start_str);
        self.update_ical_property("DTEND", end_str);
    }

    /**
     * Updates the event's recurrence rule.
     *
     * @param rrule New recurrence rule for the event
     * @return Result indicating success or containing a domain error
     */
    pub fn update_rrule(&mut self, rrule: Option<String>) -> Result<()> {
        // Validate RRULE if provided (basic validation)
        if let Some(ref rule) = rrule
            && !rule.starts_with("FREQ=")
        {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "Recurrence rule must start with FREQ=",
            ));
        }

        self.rrule = rrule.clone();
        self.updated_at = Utc::now();

        // Update iCalendar data
        match rrule {
            Some(rule) => self.update_ical_property("RRULE", &rule),
            None => self.remove_ical_property("RRULE"),
        }

        Ok(())
    }

    /**
     * Updates the complete iCalendar data for the event.
     * Also updates the event properties based on the new iCalendar data.
     *
     * @param ical_data New iCalendar data for the event
     * @return Result indicating success or containing a domain error
     */
    pub fn update_ical_data(&mut self, ical_data: String) -> Result<()> {
        // Validate iCalendar data (basic validation)
        if !ical_data.contains("BEGIN:VEVENT") || !ical_data.contains("END:VEVENT") {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "iCalendar data must contain a VEVENT component",
            ));
        }

        // Parse the body ONCE and update every property from the parsed
        // component (same 8-parses→1 collapse as `from_ical`). An
        // unparseable body behaves exactly like the old per-property
        // lookups all returning `None`: optional fields clear, required
        // fields keep their previous values.
        let event = Self::parse_first_vevent(&ical_data);

        if let Some(summary) = event.as_ref().and_then(|e| Self::prop_value(e, "SUMMARY")) {
            self.summary = summary;
        }

        self.description = event
            .as_ref()
            .and_then(|e| Self::prop_value(e, "DESCRIPTION"));
        self.location = event.as_ref().and_then(|e| Self::prop_value(e, "LOCATION"));

        // Extract DTSTART with parameters — needed for the all-day
        // detection below AND for the DTSTART/DTEND datetime parsers
        // (they need to know whether the value is a date or a datetime).
        let dtstart_pair = event
            .as_ref()
            .and_then(|e| Self::prop_value_and_is_date(e, "DTSTART"));
        let all_day = dtstart_pair
            .as_ref()
            .map(|(_v, is_date)| *is_date)
            .unwrap_or(false);
        self.all_day = all_day;

        if let Some((value, _is_date)) = &dtstart_pair
            && let Ok(start_time) = Self::parse_ical_datetime(value, all_day)
        {
            self.start_time = start_time;
        }

        if let Some(value) = event.as_ref().and_then(|e| Self::prop_value(e, "DTEND"))
            && let Ok(end_time) = Self::parse_ical_datetime(&value, all_day)
        {
            self.end_time = end_time;
        }

        self.rrule = event.as_ref().and_then(|e| Self::prop_value(e, "RRULE"));

        if let Some(uid) = event.as_ref().and_then(|e| Self::prop_value(e, "UID")) {
            self.ical_uid = uid;
        }

        self.ical_data = ical_data;
        self.updated_at = Utc::now();

        Ok(())
    }

    /**
     * Checks if this event belongs to the specified calendar.
     *
     * @param calendar_id ID of the calendar to check against
     * @return true if the event belongs to the calendar, false otherwise
     */
    pub fn belongs_to_calendar(&self, calendar_id: &Uuid) -> bool {
        self.calendar_id == *calendar_id
    }

    /**
     * Checks if this event occurs within the specified time range.
     *
     * @param start Start of the time range to check
     * @param end End of the time range to check
     * @return true if the event occurs within the range, false otherwise
     */
    pub fn occurs_in_range(&self, start: &DateTime<Utc>, end: &DateTime<Utc>) -> bool {
        // Basic case: event directly overlaps with range
        if self.start_time <= *end && self.end_time >= *start {
            return true;
        }

        // If event has recurrence, check if any recurrence occurs in range
        // Note: A full implementation would need a proper recurrence rule parser
        if let Some(rrule) = &self.rrule {
            // Simplified check for demonstration
            // A real implementation would need to generate recurrence instances
            // and check if any fall within the range

            // For now, we'll just check if the recurrence hasn't ended
            // or if it ended after the start of our range
            if let Some(until_pos) = rrule.find("UNTIL=") {
                let until_start = until_pos + 6; // "UNTIL=" is 6 chars
                let until_str = if let Some(until_end) = rrule[until_start..].find(';') {
                    &rrule[until_start..until_start + until_end]
                } else {
                    // UNTIL is the last part of the rule
                    &rrule[until_start..]
                };
                // RFC 5545 §3.3.10 — UNTIL is either a DATE (`YYYYMMDD`,
                // 8 chars) or a DATE-TIME (`YYYYMMDDTHHMMSSZ`, 16 chars,
                // trailing Z). Distinguish by shape: exactly 8 chars ⇒
                // date-only. Everything else is treated as datetime and
                // parsed accordingly.
                let is_date_only = until_str.len() == 8;
                if let Ok(until_date) = Self::parse_ical_datetime(until_str, is_date_only) {
                    return until_date >= *start;
                }
            } else {
                // No UNTIL specified, so recurrence continues indefinitely
                return true;
            }
        }

        false
    }

    // Helper methods for iCalendar operations

    /**
     * Extracts a property value from iCalendar data.
     *
     * Backed by the `ical` crate's RFC 5545 parser (see `Cargo.toml`
     * doc-comment on the dep). The pre-2026-07-14 hand-rolled scan
     * looked for `\n<NAME>:` and refused any parameter-carrying
     * property (`DTSTART;VALUE=DATE:20260101`,
     * `RECURRENCE-ID;VALUE=DATE:...`, `ATTENDEE;CN=…;PARTSTAT=…:…`) —
     * see AtalayaLabs/OxiCloud#528.
     *
     * The current implementation reads the first VEVENT from the raw
     * body via `IcalParser` and returns the named property's `value`
     * (parameters discarded — use `extract_ical_property_with_params`
     * for callers that care about `VALUE=DATE`, `TZID`, etc.).
     *
     * Returns `None` when the property is missing, has an empty value,
     * or the body isn't parseable as iCalendar. Whole-body parse
     * failures collapse to `None` rather than surface — same behaviour
     * as the pre-rewrite hand-rolled scan, which just returned `None`
     * on any mismatch. If callers need to distinguish "missing" from
     * "unparseable body", they should use `parse_first_vevent` directly.
     *
     * @param ical_data The iCalendar data to search in
     * @param property_name The name of the property to extract
     * @return Option containing the property value if found
     */
    #[cfg(test)]
    fn extract_ical_property(ical_data: &str, property_name: &str) -> Option<String> {
        Self::prop_value(&Self::parse_first_vevent(ical_data)?, property_name)
    }

    /// Test-only sibling of [`Self::prop_with_params`] that parses the
    /// raw body first. Production callers (`from_ical`,
    /// `update_ical_data`) parse ONCE and use the by-reference helpers.
    #[cfg(test)]
    fn extract_ical_property_with_params(
        ical_data: &str,
        property_name: &str,
    ) -> Option<(String, std::collections::HashMap<String, Vec<String>>)> {
        Self::prop_with_params(&Self::parse_first_vevent(ical_data)?, property_name)
    }

    /// Read a property's trimmed value from an already-parsed VEVENT.
    ///
    /// Value-only lookups skip the parameter-map build entirely; use
    /// [`Self::prop_with_params`] for DTSTART / DTEND / RECURRENCE-ID
    /// which need `VALUE=DATE` detection.
    ///
    /// Returns `None` when the property is missing or its value is
    /// empty after trimming — the same rules the old per-property
    /// full-parse extractors applied.
    fn prop_value(
        event: &ical::parser::ical::component::IcalEvent,
        property_name: &str,
    ) -> Option<String> {
        let prop = event
            .properties
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(property_name))?;
        let trimmed = prop.value.as_deref()?.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.to_string())
    }

    /// Read a property's trimmed value plus whether it carries a
    /// case-insensitive `VALUE=DATE` parameter (the all-day / date-only
    /// marker) — the ONLY thing `from_ical` / `update_ical_data` ever asked the
    /// parameter map for. Scans `prop.params` directly, so DTSTART / DTEND /
    /// RECURRENCE-ID no longer build a throwaway
    /// `HashMap<String, Vec<String>>` (uppercased keys + cloned value Vecs) per
    /// event on every CalDAV PUT / iCal import (benches/ROUND20.md §A1).
    ///
    /// `.rev().find(...)` preserves the old map's last-insert-wins semantics for
    /// the (pathological) duplicate-`VALUE` case, so the flag is byte-identical.
    fn prop_value_and_is_date(
        event: &ical::parser::ical::component::IcalEvent,
        property_name: &str,
    ) -> Option<(String, bool)> {
        let prop = event
            .properties
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(property_name))?;
        let trimmed = prop.value.as_deref()?.trim();
        if trimmed.is_empty() {
            return None;
        }
        let is_date = prop
            .params
            .as_ref()
            .and_then(|list| {
                list.iter()
                    .rev()
                    .find(|(n, _)| n.eq_ignore_ascii_case("VALUE"))
            })
            .map(|(_, vs)| vs.iter().any(|v| v.eq_ignore_ascii_case("DATE")))
            .unwrap_or(false);
        Some((trimmed.to_string(), is_date))
    }

    /// Read a property's trimmed value AND parameter map from an
    /// already-parsed VEVENT. The map is keyed by parameter name
    /// (`"VALUE"`, `"TZID"`, `"CN"`, …) whose value is the list of
    /// parameter values (parameters can be multi-valued —
    /// `MEMBER="mailto:a@x","mailto:b@x"` — hence the `Vec<String>`
    /// per key).
    ///
    /// Retained only for the `#[cfg(test)]` `extract_ical_property_with_params`
    /// wrapper; production parses once and uses [`Self::prop_value_and_is_date`]
    /// / [`Self::prop_value`].
    #[cfg(test)]
    fn prop_with_params(
        event: &ical::parser::ical::component::IcalEvent,
        property_name: &str,
    ) -> Option<(String, std::collections::HashMap<String, Vec<String>>)> {
        let prop = event
            .properties
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(property_name))?;
        let trimmed = prop.value.as_deref()?.trim();
        if trimmed.is_empty() {
            return None;
        }
        let mut params: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        if let Some(param_list) = &prop.params {
            for (name, values) in param_list {
                // RFC 5545 property parameter names are ASCII case-insensitive.
                // Normalise to UPPER so callers key on a canonical form.
                params.insert(name.to_ascii_uppercase(), values.clone());
            }
        }
        Some((trimmed.to_string(), params))
    }

    /// Parse a VCALENDAR body containing one or more VEVENT components
    /// (typically a master + one or more per-instance exception
    /// overrides in the same PUT — RFC 5545 §3.6.1), returning one
    /// `CalendarEvent` per VEVENT.
    ///
    /// Splitting is done on the raw text so each returned entity's
    /// `ical_data` remains a valid standalone iCalendar body (the GET
    /// path serves it verbatim). Line-folding (§3.1) is preserved
    /// because we forward every line as-is inside the extracted block;
    /// the ical-crate parser inside `from_ical` unfolds when reading.
    ///
    /// Nested VALARM / VTODO sub-components inside a VEVENT are
    /// carried through unchanged — the scanner only splits on
    /// `BEGIN:VEVENT` / `END:VEVENT` at the outer level.
    ///
    /// Returns `InvalidInput` if the body contains zero VEVENTs — a
    /// PUT with no events isn't a state we accept on the CalDAV surface.
    pub fn parse_all_events(calendar_id: Uuid, ical_data: &str) -> Result<Vec<Self>> {
        let blocks = Self::split_vevents(ical_data);

        if blocks.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "CalendarEvent",
                "No VEVENT components found in iCalendar body",
            ));
        }

        let mut out = Vec::with_capacity(blocks.len());
        for block in blocks {
            // Wrap each VEVENT in a fresh VCALENDAR shell so the
            // stored `ical_data` per row is self-describing (RFC 5545
            // §3.4 mandates VERSION + PRODID on any exported body).
            let wrapped = format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n{}END:VCALENDAR\r\n",
                block,
            );
            out.push(Self::from_ical(calendar_id, wrapped)?);
        }
        Ok(out)
    }

    /// Extract each `BEGIN:VEVENT` … `END:VEVENT` block from the raw
    /// body as its own String (CRLF-terminated). Component tags are
    /// matched case-insensitively per RFC 5545 §3.1. Anything outside
    /// a VEVENT (VTIMEZONE / VTODO / VJOURNAL / calendar-level
    /// properties) is discarded — those aren't ours to persist.
    fn split_vevents(ical_data: &str) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut in_event = false;
        let mut current = String::new();

        // Allocation-free case-insensitive prefix test. `to_ascii_uppercase`
        // maps ASCII bytes in place and leaves multi-byte chars untouched,
        // so "first N bytes uppercased equal TAG" ⇔ "first N bytes
        // ASCII-case-insensitively equal TAG"; `get(..N)` returning `None`
        // (char straddling the boundary) implies the prefix can't be the
        // all-ASCII tag. The old per-line `to_ascii_uppercase()` allocated
        // a String for every line of every uploaded body.
        fn starts_with_ci(line: &str, tag: &str) -> bool {
            line.get(..tag.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(tag))
        }

        for raw_line in ical_data.split('\n') {
            let line = raw_line.trim_end_matches('\r');
            // Match the tag ignoring case, allowing surrounding
            // whitespace (some clients emit a leading space on folded
            // continuations — the raw-line scan sees those but they
            // won't start with BEGIN/END so they slot through as
            // in-event content, which is correct).
            let tag_area = line.trim_start();

            if starts_with_ci(tag_area, "BEGIN:VEVENT") {
                in_event = true;
                current.clear();
            }

            if in_event {
                current.push_str(line);
                current.push_str("\r\n");
            }

            if in_event && starts_with_ci(tag_area, "END:VEVENT") {
                blocks.push(std::mem::take(&mut current));
                in_event = false;
            }
        }

        blocks
    }

    /// Parse the raw iCalendar body and return the first VEVENT
    /// component's properties. Returns `None` on any parse failure or
    /// if the body carries zero events (e.g. a `VCALENDAR` with only
    /// VTODOs — not our concern for the events surface).
    ///
    /// Delegated to the `ical` crate's `IcalParser`, which handles
    /// line-folding, escaped characters, and RFC 5545 parameter syntax.
    fn parse_first_vevent(ical_data: &str) -> Option<ical::parser::ical::component::IcalEvent> {
        use std::io::BufReader;
        let reader = BufReader::new(ical_data.as_bytes());
        let parser = ical::IcalParser::new(reader);
        for cal in parser {
            let Ok(cal) = cal else { continue };
            if let Some(event) = cal.events.into_iter().next() {
                return Some(event);
            }
        }
        None
    }

    /**
     * Parses an iCalendar datetime string into a DateTime object.
     *
     * @param value The property value (already stripped of parameters
     *              by the ical-crate-backed extractor).
     * @param is_date_only True when the source line carried
     *                     `VALUE=DATE` (all-day event) — caller derives
     *                     this from `extract_ical_property_with_params`.
     * @return Result containing the parsed DateTime or an error
     */
    fn parse_ical_datetime(
        value: &str,
        is_date_only: bool,
    ) -> std::result::Result<DateTime<Utc>, String> {
        // All-day form — YYYYMMDD, 8 chars, no time component. Caller
        // signalled this via the `VALUE=DATE` parameter on the source
        // property. Pre-2026-07-14 this was detected by scanning the
        // raw property line for the substring `VALUE=DATE`, which
        // failed because `extract_ical_property` refused to return
        // param-carrying lines at all (see #528).
        if is_date_only {
            if value.len() != 8 {
                return Err(format!(
                    "Invalid all-day date format: expected YYYYMMDD (8 chars), got {} chars",
                    value.len()
                ));
            }

            let year = value[0..4]
                .parse::<i32>()
                .map_err(|_| "Invalid year".to_string())?;
            let month = value[4..6]
                .parse::<u32>()
                .map_err(|_| "Invalid month".to_string())?;
            let day = value[6..8]
                .parse::<u32>()
                .map_err(|_| "Invalid day".to_string())?;

            return match chrono::NaiveDate::from_ymd_opt(year, month, day) {
                Some(date) => Ok(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap())),
                None => Err("Invalid date components".to_string()),
            };
        }

        // Standard UTC form: YYYYMMDDTHHMMSSZ, 16 chars, trailing 'Z'.
        // Floating-time (no 'Z', RFC 5545 §3.3.5) is what calendar apps emit
        // for events without a timezone — DAVx5 sends it from Fossify
        // Calendar, and rejecting it failed the whole event sync with a 400
        // (#682). Accept it and interpret the wall-clock time as UTC.
        // TZID-anchored forms remain unsupported — future work when we
        // tackle VTIMEZONE properly.
        let has_utc_suffix = value.len() == 16 && value.ends_with('Z');
        let is_floating = value.len() == 15;
        if !has_utc_suffix && !is_floating {
            return Err(format!(
                "Invalid datetime format: expected YYYYMMDDTHHMMSSZ, got {:?}",
                value
            ));
        }

        let year = value[0..4]
            .parse::<i32>()
            .map_err(|_| "Invalid year".to_string())?;
        let month = value[4..6]
            .parse::<u32>()
            .map_err(|_| "Invalid month".to_string())?;
        let day = value[6..8]
            .parse::<u32>()
            .map_err(|_| "Invalid day".to_string())?;

        let hour = value[9..11]
            .parse::<u32>()
            .map_err(|_| "Invalid hour".to_string())?;
        let minute = value[11..13]
            .parse::<u32>()
            .map_err(|_| "Invalid minute".to_string())?;
        let second = value[13..15]
            .parse::<u32>()
            .map_err(|_| "Invalid second".to_string())?;

        match chrono::NaiveDate::from_ymd_opt(year, month, day) {
            Some(date) => match date.and_hms_opt(hour, minute, second) {
                Some(datetime) => Ok(Utc.from_utc_datetime(&datetime)),
                None => Err("Invalid time components".to_string()),
            },
            None => Err("Invalid date components".to_string()),
        }
    }

    /**
     * Updates an iCalendar property in the event's iCalendar data.
     *
     * @param property_name The name of the property to update
     * @param value The new value for the property
     */
    /// Write `\n{name}:` into `buf` and return it as `&str`, or `None` if the
    /// name is too long to fit (unreachable for RFC 5545 property names, the
    /// longest of which — `LAST-MODIFIED`, `RECURRENCE-ID` — are 13 bytes).
    ///
    /// The bare-LF form is deliberate: `\n{name}:` is a suffix of the CRLF form
    /// `\r\n{name}:`, so a single search for it matches a property line whether
    /// the body is LF- or CRLF-terminated and returns the LF offset either way
    /// — behaviour-identical to the old `find("\n..").or(find("\r\n.."))` (the
    /// CRLF needle could never match where the LF one didn't). Built on the
    /// stack: no per-call heap needle.
    fn line_needle<'a>(buf: &'a mut [u8; 64], name: &str) -> Option<&'a str> {
        let n = name.len();
        if n + 2 > buf.len() {
            return None;
        }
        buf[0] = b'\n';
        buf[1..1 + n].copy_from_slice(name.as_bytes());
        buf[1 + n] = b':';
        // `name` is valid UTF-8 and only ASCII bytes were added around it.
        std::str::from_utf8(&buf[..n + 2]).ok()
    }

    fn update_ical_property(&mut self, property_name: &str, value: &str) {
        let mut buf = [0u8; 64];
        let needle_owned;
        let needle: &str = match Self::line_needle(&mut buf, property_name) {
            Some(n) => n,
            None => {
                needle_owned = format!("\n{property_name}:");
                &needle_owned
            }
        };

        if let Some(pos) = self.ical_data.find(needle) {
            // Value spans from just after `\n{NAME}:` to the next LF (or EOF).
            let value_start = pos + needle.len();
            let value_end = self.ical_data[value_start..]
                .find('\n')
                .map_or(self.ical_data.len(), |p| value_start + p);

            // In place: reuses the body's own buffer (growing it once only when
            // the new value is longer) instead of allocating a whole fresh body
            // String per property, as the old `format!("{}{}{}")` did — on a
            // multi-field edit that was one full-body (up to ~11 KB) allocation
            // per changed property.
            self.ical_data.replace_range(value_start..value_end, value);
        } else {
            // Property absent: insert `{NAME}:{value}\n` before END:VEVENT.
            let end_pos = self
                .ical_data
                .find("END:VEVENT")
                .unwrap_or(self.ical_data.len());

            // Insert the four pieces at one point in reverse order so the result
            // is `{NAME}:{value}\n` before END:VEVENT — byte-identical to the old
            // `format!("{}{}:{}\n{}")` — without allocating a fresh body String
            // (nor a value-sized fragment). One `reserve` caps it at a single
            // grow; the shifted tail is just the trailing END:VEVENT/VCALENDAR.
            self.ical_data
                .reserve(property_name.len() + value.len() + 2);
            self.ical_data.insert(end_pos, '\n');
            self.ical_data.insert_str(end_pos, value);
            self.ical_data.insert(end_pos, ':');
            self.ical_data.insert_str(end_pos, property_name);
        }
    }

    /**
     * Removes an iCalendar property from the event's iCalendar data.
     *
     * @param property_name The name of the property to remove
     */
    fn remove_ical_property(&mut self, property_name: &str) {
        let mut buf = [0u8; 64];
        let needle_owned;
        let needle: &str = match Self::line_needle(&mut buf, property_name) {
            Some(n) => n,
            None => {
                needle_owned = format!("\n{property_name}:");
                &needle_owned
            }
        };

        if let Some(pos) = self.ical_data.find(needle) {
            // Delete from the property's leading LF (`pos`) through the end of
            // its value line (exclusive of the next line's LF) — byte-identical
            // to the old `format!("{}{}", &data[..pos], &data[value_end..])`,
            // but in place, with no fresh body String.
            let value_end = self.ical_data[pos + 1..]
                .find('\n')
                .map_or(self.ical_data.len(), |p| pos + 1 + p);
            self.ical_data.replace_range(pos..value_end, "");
        }
    }
}

#[cfg(test)]
mod ical_parser_tests {
    //! Regression tests for the `ical`-crate-backed property extractor.
    //!
    //! Every shape here failed under the pre-2026-07-14 hand-rolled
    //! `find("\n<NAME>:")` scan (see AtalayaLabs/OxiCloud#528). Fixtures
    //! are RFC 5545-shaped; when we bundle real client bodies from
    //! Thunderbird / DAVx⁵ / Gnome Calendar the mapping will follow the
    //! same style — each case declares which shape it exercises.
    //!
    //! Fixture sources / attributions:
    //!   * RFC 5545 §3.6.1 (VEVENT baseline) — timed event example
    //!   * RFC 5545 §3.8.2.4 (DTSTART DATE form) — all-day event
    //!   * RFC 5545 §3.8.4.4 (RECURRENCE-ID) — exception instance
    //!   * Shape adapted from Radicale test fixtures — RRULE + UNTIL
    //!     with a DATE-form UNTIL for an all-day recurring event
    //!
    //! Everything is spec-shaped and byte-small; no network / no
    //! external files. Real client bodies can be added later under
    //! `tests/fixtures/ical/` and loaded via `include_str!`.

    use super::*;

    /// Simple timed VEVENT. Baseline sanity — this shape worked pre-
    /// rewrite (no property parameters), so it's the regression floor.
    const TIMED_EVENT: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//OxiCloud test//EN\r
BEGIN:VEVENT\r
UID:timed-1@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART:20260101T120000Z\r
DTEND:20260101T130000Z\r
SUMMARY:Timed baseline\r
END:VEVENT\r
END:VCALENDAR\r
";

    /// All-day VEVENT — the exact shape #528 flagged. Property line
    /// carries `;VALUE=DATE:` which the old scan refused; the crate-
    /// backed extractor now parses it and the all-day flag is derived
    /// from the `VALUE` parameter.
    const ALL_DAY_EVENT: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//OxiCloud test//EN\r
BEGIN:VEVENT\r
UID:allday-1@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART;VALUE=DATE:20260201\r
DTEND;VALUE=DATE:20260202\r
SUMMARY:All-day event\r
END:VEVENT\r
END:VCALENDAR\r
";

    /// Timed recurring master with a modified single occurrence
    /// (RECURRENCE-ID identifies which instance). The exception VEVENT
    /// shares the master's UID and adds `RECURRENCE-ID:` to pinpoint
    /// the overridden date. This is the #528 shape — parser must not
    /// choke on the presence of RECURRENCE-ID even though we don't
    /// route it into the domain yet (that's phase 2).
    const RECURRING_WITH_EXCEPTION: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//OxiCloud test//EN\r
BEGIN:VEVENT\r
UID:daily-1@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART:20260101T090000Z\r
DTEND:20260101T100000Z\r
SUMMARY:Daily standup\r
RRULE:FREQ=DAILY;COUNT=10\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:daily-1@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART:20260103T110000Z\r
DTEND:20260103T120000Z\r
SUMMARY:Daily standup — rescheduled\r
RECURRENCE-ID:20260103T090000Z\r
END:VEVENT\r
END:VCALENDAR\r
";

    /// All-day recurring with an all-day exception — the most-broken
    /// case in #528 (RECURRENCE-ID;VALUE=DATE:...). Parser must accept
    /// the parameter on both DTSTART and RECURRENCE-ID.
    const ALL_DAY_RECURRING_WITH_EXCEPTION: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//OxiCloud test//EN\r
BEGIN:VEVENT\r
UID:weekly-allday@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART;VALUE=DATE:20260105\r
DTEND;VALUE=DATE:20260106\r
SUMMARY:Weekly all-day\r
RRULE:FREQ=WEEKLY;COUNT=4\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:weekly-allday@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART;VALUE=DATE:20260113\r
DTEND;VALUE=DATE:20260114\r
SUMMARY:Weekly all-day — rescheduled\r
RECURRENCE-ID;VALUE=DATE:20260112\r
END:VEVENT\r
END:VCALENDAR\r
";

    /// Floating-time VEVENT — DTSTART/DTEND without the UTC 'Z' suffix
    /// (RFC 5545 §3.3.5 "form #2": local time, no timezone reference).
    /// This is what DAVx5 syncs from calendar apps for events created
    /// without a timezone (e.g. Fossify Calendar); rejecting it failed
    /// the entire event upload with a 400 (#682).
    const FLOATING_TIME_EVENT: &str = "BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud test//EN
BEGIN:VEVENT
UID:floating-1@oxicloud.test
DTSTAMP:20260101T100000Z
DTSTART:20260831T154000
DTEND:20260831T164000
SUMMARY:Floating time event
END:VEVENT
END:VCALENDAR
";

    fn parse_ok(body: &str) -> CalendarEvent {
        CalendarEvent::from_ical(Uuid::new_v4(), body.to_string())
            .expect("expected successful parse")
    }

    #[test]
    fn timed_event_parses_and_is_not_all_day() {
        let ev = parse_ok(TIMED_EVENT);
        assert_eq!(ev.summary(), "Timed baseline");
        assert!(!ev.all_day());
    }

    #[test]
    fn floating_time_event_parses_as_utc_wall_clock() {
        // Regression (#682): '20260831T154000' used to be rejected with
        // "Invalid datetime format: expected YYYYMMDDTHHMMSSZ" and the
        // whole DAVx5 sync failed with HTTP 400.
        let ev = parse_ok(FLOATING_TIME_EVENT);
        assert_eq!(ev.summary(), "Floating time event");
        assert!(!ev.all_day());
        assert_eq!(ev.start_time().to_rfc3339(), "2026-08-31T15:40:00+00:00");
        assert_eq!(ev.end_time().to_rfc3339(), "2026-08-31T16:40:00+00:00");
    }

    #[test]
    fn all_day_event_parses_and_flags_as_all_day() {
        // Regression: DTSTART;VALUE=DATE:20260201 used to fail
        // property-extraction ("Missing DTSTART") because the raw
        // scan required a colon directly after the property name.
        let ev = parse_ok(ALL_DAY_EVENT);
        assert!(ev.all_day(), "VALUE=DATE parameter should flag all-day");
        assert_eq!(
            ev.start_time().date_naive().to_string(),
            "2026-02-01",
            "DTSTART value should parse the YYYYMMDD payload"
        );
    }

    #[test]
    fn recurring_with_exception_still_returns_the_master() {
        // The crate parses BOTH events from the VCALENDAR body; our
        // `parse_first_vevent` returns the first, which is the master.
        // Exception routing is phase 2 — this test locks the current
        // "first event wins" behavior so phase 2 knows what it's
        // extending.
        let ev = parse_ok(RECURRING_WITH_EXCEPTION);
        assert_eq!(ev.summary(), "Daily standup");
        assert_eq!(ev.ical_uid(), "daily-1@oxicloud.test");
        assert_eq!(ev.rrule(), Some("FREQ=DAILY;COUNT=10"));
    }

    #[test]
    fn all_day_recurring_with_exception_master_parses() {
        // The #528 shape end-to-end: parameterised DTSTART on both the
        // master and the exception, plus a parameterised RECURRENCE-ID.
        // Pre-rewrite this was a 400 (post the error-mapping fix) or 500
        // (before it); post-rewrite the master parses cleanly and the
        // all_day flag is set from the master's DTSTART parameters.
        let ev = parse_ok(ALL_DAY_RECURRING_WITH_EXCEPTION);
        assert!(ev.all_day());
        assert_eq!(ev.ical_uid(), "weekly-allday@oxicloud.test");
    }

    #[test]
    fn missing_dtstart_still_returns_a_useful_error() {
        // Preserve the pre-rewrite error contract for the genuinely-
        // missing case. `dav_error_mapping.hurl` asserts this shape.
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//OxiCloud test//EN\r
BEGIN:VEVENT\r
UID:missing-dtstart@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTEND:20260101T130000Z\r
SUMMARY:No DTSTART\r
END:VEVENT\r
END:VCALENDAR\r
";
        let err = CalendarEvent::from_ical(Uuid::new_v4(), body.to_string())
            .expect_err("expected InvalidInput for missing DTSTART");
        assert_eq!(err.kind, ErrorKind::InvalidInput);
        assert!(
            err.message.contains("DTSTART"),
            "message should mention DTSTART, got: {}",
            err.message
        );
    }

    #[test]
    fn extract_property_with_params_returns_parameter_map() {
        // Direct test of the params-aware extractor. Confirms
        // parameter names are normalised to uppercase and preserved
        // as a list (RFC 5545 §3.2 — parameters can carry multiple
        // comma-separated values).
        let (value, params) =
            CalendarEvent::extract_ical_property_with_params(ALL_DAY_EVENT, "DTSTART")
                .expect("DTSTART must extract");
        assert_eq!(value, "20260201");
        let vals = params.get("VALUE").expect("VALUE param must be present");
        assert_eq!(vals, &vec!["DATE".to_string()]);
    }

    #[test]
    fn extract_property_case_insensitive_property_name() {
        // Property names are ASCII case-insensitive per RFC 5545 §3.1.
        // The lookup must accept "dtstart" as well as "DTSTART".
        let v = CalendarEvent::extract_ical_property(TIMED_EVENT, "dtstart");
        assert_eq!(v.as_deref(), Some("20260101T120000Z"));
    }

    // ─────────────────────────────────────────────────────────────
    // Phase 2 — RECURRENCE-ID extraction into the entity
    // ─────────────────────────────────────────────────────────────

    #[test]
    fn master_event_has_no_recurrence_id() {
        // A plain VEVENT (no RECURRENCE-ID line) should carry a NULL
        // recurrence_id — that's what marks it as a master in the DB.
        let ev = parse_ok(TIMED_EVENT);
        assert!(
            ev.recurrence_id().is_none(),
            "master should have recurrence_id = None"
        );
    }

    #[test]
    fn recurring_master_has_no_recurrence_id_even_with_rrule() {
        // The presence of RRULE on the master does not by itself
        // populate recurrence_id — only RECURRENCE-ID does. The
        // exception-instance VEVENT in the same VCALENDAR carries
        // RECURRENCE-ID; `parse_first_vevent` returns the master, so
        // we get `None` here. Phase 3 will introduce a `parse_all_events`
        // helper to surface the exceptions.
        let ev = parse_ok(RECURRING_WITH_EXCEPTION);
        assert!(
            ev.recurrence_id().is_none(),
            "master with RRULE should still have recurrence_id = None"
        );
        assert_eq!(ev.rrule(), Some("FREQ=DAILY;COUNT=10"));
    }

    #[test]
    fn timed_exception_populates_recurrence_id() {
        // A standalone exception-override VEVENT (as sent by a client
        // that's already synced the master and is now modifying one
        // instance) parses with recurrence_id = the RECURRENCE-ID's
        // timestamp. This is the phase-2 half of #528 — the value is
        // preserved through the domain model; phase 3 will use it to
        // route inserts to their own row.
        let exception = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//OxiCloud test//EN\r
BEGIN:VEVENT\r
UID:daily-1@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART:20260103T110000Z\r
DTEND:20260103T120000Z\r
SUMMARY:Daily standup — rescheduled\r
RECURRENCE-ID:20260103T090000Z\r
END:VEVENT\r
END:VCALENDAR\r
";
        let ev = parse_ok(exception);
        let rid = ev
            .recurrence_id()
            .expect("exception must have recurrence_id set");
        assert_eq!(
            rid.to_rfc3339(),
            "2026-01-03T09:00:00+00:00",
            "RECURRENCE-ID must parse to the timed override timestamp"
        );
    }

    #[test]
    fn all_day_exception_populates_recurrence_id_at_midnight() {
        // RECURRENCE-ID;VALUE=DATE:20260112 — the exact shape #528
        // flagged. Domain normalises the DATE form to midnight UTC on
        // the given day so the field's type stays `DateTime<Utc>`.
        let exception = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//OxiCloud test//EN\r
BEGIN:VEVENT\r
UID:weekly-allday@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART;VALUE=DATE:20260113\r
DTEND;VALUE=DATE:20260114\r
SUMMARY:Weekly all-day — rescheduled\r
RECURRENCE-ID;VALUE=DATE:20260112\r
END:VEVENT\r
END:VCALENDAR\r
";
        let ev = parse_ok(exception);
        let rid = ev
            .recurrence_id()
            .expect("all-day exception must have recurrence_id set");
        assert_eq!(
            rid.to_rfc3339(),
            "2026-01-12T00:00:00+00:00",
            "all-day RECURRENCE-ID must normalise to 00:00:00 UTC of the target date"
        );
    }

    // ─────────────────────────────────────────────────────────────
    // Phase 3 — parse_all_events (multi-VEVENT splitter)
    // ─────────────────────────────────────────────────────────────

    /// Timed daily recurring master + one timed exception override,
    /// both inside a single VCALENDAR wrapper — the shape a CalDAV
    /// client PUTs when it modifies one occurrence.
    const MASTER_PLUS_TIMED_EXCEPTION: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//OxiCloud test//EN\r
BEGIN:VEVENT\r
UID:daily-1@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART:20260101T090000Z\r
DTEND:20260101T093000Z\r
SUMMARY:Daily standup\r
RRULE:FREQ=DAILY;COUNT=10\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:daily-1@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART:20260103T110000Z\r
DTEND:20260103T120000Z\r
SUMMARY:Daily standup — rescheduled\r
RECURRENCE-ID:20260103T090000Z\r
END:VEVENT\r
END:VCALENDAR\r
";

    #[test]
    fn parse_all_events_splits_master_and_exception() {
        // Both VEVENTs must come back: master with recurrence_id=None,
        // exception with recurrence_id=Some. UIDs match (that's what
        // ties the exception to its master); it's the recurrence_id
        // marker that distinguishes them.
        let cal_id = Uuid::new_v4();
        let events = CalendarEvent::parse_all_events(cal_id, MASTER_PLUS_TIMED_EXCEPTION)
            .expect("both VEVENTs must parse");

        assert_eq!(events.len(), 2, "expected master + exception");
        assert_eq!(events[0].ical_uid(), "daily-1@oxicloud.test");
        assert_eq!(events[1].ical_uid(), "daily-1@oxicloud.test");

        assert!(
            events[0].recurrence_id().is_none(),
            "first row must be the master (recurrence_id None)"
        );
        let rid = events[1]
            .recurrence_id()
            .expect("second row must be the exception override");
        assert_eq!(rid.to_rfc3339(), "2026-01-03T09:00:00+00:00");

        assert_eq!(events[0].rrule(), Some("FREQ=DAILY;COUNT=10"));
        assert!(
            events[1].rrule().is_none(),
            "exception overrides do NOT carry RRULE"
        );

        // Each event's stored ical_data must be a self-contained
        // VCALENDAR body so the GET path can serve it verbatim.
        for e in &events {
            assert!(e.ical_data().starts_with("BEGIN:VCALENDAR"));
            assert!(e.ical_data().trim_end().ends_with("END:VCALENDAR"));
        }
    }

    #[test]
    fn parse_all_events_lone_master_returns_single_event() {
        // No RECURRENCE-ID exception in the body → one row, master.
        let cal_id = Uuid::new_v4();
        let events =
            CalendarEvent::parse_all_events(cal_id, TIMED_EVENT).expect("plain event must parse");
        assert_eq!(events.len(), 1);
        assert!(events[0].recurrence_id().is_none());
    }

    #[test]
    fn parse_all_events_all_day_master_plus_all_day_exception() {
        // The #528 shape: DATE-form DTSTART on both, DATE-form
        // RECURRENCE-ID on the exception. Pre-parser-rewrite this
        // silently 500'd because the param-carrying property lines
        // were invisible to the substring scanner.
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//OxiCloud test//EN\r
BEGIN:VEVENT\r
UID:weekly-allday@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART;VALUE=DATE:20260105\r
DTEND;VALUE=DATE:20260106\r
SUMMARY:Weekly review\r
RRULE:FREQ=WEEKLY;COUNT=4\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:weekly-allday@oxicloud.test\r
DTSTAMP:20260101T100000Z\r
DTSTART;VALUE=DATE:20260113\r
DTEND;VALUE=DATE:20260114\r
SUMMARY:Weekly review — rescheduled\r
RECURRENCE-ID;VALUE=DATE:20260112\r
END:VEVENT\r
END:VCALENDAR\r
";
        let cal_id = Uuid::new_v4();
        let events = CalendarEvent::parse_all_events(cal_id, body).expect("both must parse");
        assert_eq!(events.len(), 2);
        assert!(events[0].all_day());
        assert!(events[1].all_day());
        assert!(events[0].recurrence_id().is_none());
        let rid = events[1].recurrence_id().unwrap();
        assert_eq!(rid.to_rfc3339(), "2026-01-12T00:00:00+00:00");
    }

    #[test]
    fn parse_all_events_zero_vevents_is_invalid_input() {
        // A VCALENDAR with only calendar-level properties (no events)
        // is not a state the CalDAV surface accepts on PUT.
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//test//EN\r
END:VCALENDAR\r
";
        let err =
            CalendarEvent::parse_all_events(Uuid::new_v4(), body).expect_err("must reject empty");
        assert_eq!(err.kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn parse_all_events_vtodo_is_ignored() {
        // A body carrying only VTODOs (no VEVENTs) is treated as
        // "zero events" — we don't persist tasks in the events table.
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//test//EN\r
BEGIN:VTODO\r
UID:task-1@x\r
SUMMARY:buy milk\r
END:VTODO\r
END:VCALENDAR\r
";
        let err = CalendarEvent::parse_all_events(Uuid::new_v4(), body)
            .expect_err("VTODO-only body must be rejected");
        assert_eq!(err.kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn parse_all_events_preserves_valarm_inside_vevent() {
        // VALARM lives INSIDE a VEVENT. The splitter must NOT be
        // fooled by BEGIN:VALARM into thinking a new outer component
        // has started — the whole VALARM block must ride along inside
        // the parent VEVENT's stored ical_data.
        let body = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//test//EN\r
BEGIN:VEVENT\r
UID:with-alarm@x\r
DTSTAMP:20260101T100000Z\r
DTSTART:20260101T090000Z\r
DTEND:20260101T093000Z\r
SUMMARY:Standup with alarm\r
BEGIN:VALARM\r
ACTION:DISPLAY\r
TRIGGER:-PT15M\r
DESCRIPTION:Standup soon\r
END:VALARM\r
END:VEVENT\r
END:VCALENDAR\r
";
        let events = CalendarEvent::parse_all_events(Uuid::new_v4(), body)
            .expect("VEVENT with VALARM must parse");
        assert_eq!(events.len(), 1);
        let stored = events[0].ical_data();
        assert!(
            stored.contains("BEGIN:VALARM"),
            "VALARM must survive the split into stored ical_data"
        );
        assert!(
            stored.contains("END:VALARM"),
            "matching END:VALARM must survive too"
        );
    }

    #[test]
    fn set_recurrence_id_setter_round_trips() {
        // Repository rehydration path: `with_id` initialises
        // recurrence_id to None; the repo calls `set_recurrence_id`
        // with the DB column value. Prove both branches survive the
        // setter cleanly.
        let mut ev = parse_ok(TIMED_EVENT);
        assert!(ev.recurrence_id().is_none());

        let target = Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap();
        ev.set_recurrence_id(Some(target));
        assert_eq!(ev.recurrence_id(), Some(&target));

        ev.set_recurrence_id(None);
        assert!(ev.recurrence_id().is_none());
    }
}
