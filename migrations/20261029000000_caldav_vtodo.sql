-- ════════════════════════════════════════════════════════════════════════════
-- caldav.calendar_todos — VTODO (task) storage for CalDAV
-- ════════════════════════════════════════════════════════════════════════════
-- Motivation: AtalayaLabs/OxiCloud#754 — the CalDAV surface only accepted
-- VEVENT. Tasks clients (DAVx⁵ + Tasks.org / jtx Board, Thunderbird,
-- Apple Reminders) sync VTODO components over the same calendar
-- collections; without a store for them, a VTODO PUT failed with
-- HTTP 400 ("No VEVENT components found").
--
-- Design: a SEPARATE table rather than generalising caldav.calendar_events
-- (adding nullable columns + a component_type discriminator there). The
-- events table is untouched, so upgrades from any previous version are
-- lossless — this migration is a pure CREATE TABLE applied automatically
-- on startup; rolling back to an older binary simply leaves the new table
-- unused. The master/exception model mirrors calendar_events
-- (RECURRENCE-ID routing, RFC 5545 §3.8.4.4) because VTODOs can recur too.
--
-- Column philosophy (same as events): the full iCalendar body lives in
-- `ical_data` and is served verbatim on GET/REPORT — every VTODO property
-- (CATEGORIES, CLASS, RELATED-TO, ATTENDEE, X-*, VALARM, …) round-trips
-- byte-exact regardless of whether it has a column. Columns exist ONLY as
-- the server-side filter index (time-range queries per RFC 4791 §9.9 and
-- future REST surfaces); nothing is regenerated from them.
-- ════════════════════════════════════════════════════════════════════════════

BEGIN;

CREATE TABLE IF NOT EXISTS caldav.calendar_todos (
    id UUID PRIMARY KEY,
    calendar_id UUID NOT NULL REFERENCES caldav.calendars(id) ON DELETE CASCADE,
    -- SUMMARY is OPTIONAL on a VTODO (RFC 5545 §3.6.2 mandates only
    -- UID + DTSTAMP), unlike VEVENT where OxiCloud requires it.
    summary TEXT,
    description TEXT,
    location TEXT,
    -- RFC 5545 §3.8.1.11: NEEDS-ACTION / IN-PROCESS / COMPLETED /
    -- CANCELLED. Free-form VARCHAR so client extensions round-trip.
    status VARCHAR(32),
    -- RFC 5545 §3.8.1.8: integer 0..100.
    percent_complete SMALLINT,
    -- RFC 5545 §3.8.1.9: integer 0..9 (0 = undefined).
    priority SMALLINT,
    -- All three instants are optional on a VTODO. `start_time` =
    -- DTSTART, `due_time` = DUE, `completed_at` = COMPLETED. Stored as
    -- UTC instants; TZID-anchored values convert via the IANA tz
    -- database at ingest (#689) while the original wall-clock form
    -- stays in `ical_data`.
    start_time TIMESTAMP WITH TIME ZONE,
    due_time TIMESTAMP WITH TIME ZONE,
    completed_at TIMESTAMP WITH TIME ZONE,
    all_day BOOLEAN NOT NULL DEFAULT FALSE,
    rrule TEXT,
    ical_uid VARCHAR(255) NOT NULL,
    ical_data TEXT, -- Full iCalendar body for round-trip fidelity (served verbatim)
    -- RFC 5545 §3.8.4.4 RECURRENCE-ID — same master/exception model as
    -- calendar_events (NULL = master, non-NULL = per-instance override).
    recurrence_id TIMESTAMP WITH TIME ZONE NULL,
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT calendar_todos_percent_complete_range
        CHECK (percent_complete IS NULL OR (percent_complete >= 0 AND percent_complete <= 100)),
    CONSTRAINT calendar_todos_priority_range
        CHECK (priority IS NULL OR (priority >= 0 AND priority <= 9))
);

-- Partial unique index: at most one master row per (calendar_id, ical_uid).
CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_todos_master_unique
    ON caldav.calendar_todos (calendar_id, ical_uid)
    WHERE recurrence_id IS NULL;

-- Partial unique index: at most one exception override per
-- (calendar_id, ical_uid, recurrence_id).
CREATE UNIQUE INDEX IF NOT EXISTS idx_calendar_todos_exception_unique
    ON caldav.calendar_todos (calendar_id, ical_uid, recurrence_id)
    WHERE recurrence_id IS NOT NULL;

-- Read-path index for the "master + all exceptions" bundle query.
CREATE INDEX IF NOT EXISTS idx_calendar_todos_uid_lookup
    ON caldav.calendar_todos (calendar_id, ical_uid);

CREATE INDEX IF NOT EXISTS idx_calendar_todos_calendar_id
    ON caldav.calendar_todos(calendar_id);

-- Time-range queries key off DUE for tasks (RFC 4791 §9.9 VTODO rules).
CREATE INDEX IF NOT EXISTS idx_calendar_todos_due_time
    ON caldav.calendar_todos (calendar_id, due_time);

CREATE INDEX IF NOT EXISTS idx_calendar_todos_status
    ON caldav.calendar_todos (calendar_id, status);

-- GIN trigram index for ILIKE substring search (parity with events).
CREATE INDEX IF NOT EXISTS idx_calendar_todos_summary_trgm
    ON caldav.calendar_todos USING gin (summary gin_trgm_ops);

COMMENT ON TABLE caldav.calendar_todos IS
    'CalDAV VTODO (task) components (#754). ical_data is authoritative and '
    'served verbatim; columns are the server-side filter index only.';

COMMIT;
