-- RFC 6578 incremental sync-collection: durable change log for CalDAV
-- calendar events.
--
-- Mirrors `storage.folder_sync_changes`
-- (`20260911000000_folder_sync_changes.sql`) — same append-only shape,
-- same trigger population, same retention-sweep lifecycle. Simpler than
-- the WebDAV table because a `caldav.calendar_events` row never moves
-- between calendars (no move branch) and is never soft-deleted (no
-- trash/restore branch): only created / updated / deleted.
--
-- `collection_calendar_id` is a plain UUID, deliberately WITHOUT a FK to
-- `caldav.calendars(id)` — same reasoning as the WebDAV table: deleting
-- a whole calendar cascades to delete its events in the same statement,
-- and a FK would reject the event's own tombstone insert once its
-- parent calendar row is already gone. A stale sync-token against a
-- deleted calendar 404s at calendar-resolution time before the
-- change-log is ever queried, so the orphaned rows are inert and age
-- out via the retention sweep like any other.

CREATE TABLE IF NOT EXISTS caldav.calendar_sync_changes (
    seq                    BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    collection_calendar_id UUID NOT NULL,
    member_id              UUID NOT NULL,
    member_ical_uid        TEXT NOT NULL,
    change_kind            TEXT NOT NULL CHECK (change_kind IN ('created', 'updated', 'deleted')),
    changed_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_calendar_sync_changes_collection_seq
    ON caldav.calendar_sync_changes (collection_calendar_id, seq);

CREATE INDEX IF NOT EXISTS idx_calendar_sync_changes_changed_at
    ON caldav.calendar_sync_changes (changed_at);

CREATE TABLE IF NOT EXISTS caldav.calendar_sync_watermark (
    singleton     BOOLEAN NOT NULL DEFAULT TRUE PRIMARY KEY CHECK (singleton),
    low_water_seq BIGINT NOT NULL DEFAULT 0
);

INSERT INTO caldav.calendar_sync_watermark (singleton, low_water_seq)
VALUES (TRUE, 0)
ON CONFLICT (singleton) DO NOTHING;

-- ── INSERT ────────────────────────────────────────────────────────────
CREATE OR REPLACE FUNCTION caldav.log_calendar_sync_changes_ins()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO caldav.calendar_sync_changes
        (collection_calendar_id, member_id, member_ical_uid, change_kind)
    SELECT calendar_id, id, ical_uid, 'created'
      FROM changed_rows;

    RETURN NULL;
END;
$$;

-- ── DELETE ────────────────────────────────────────────────────────────
-- Depth guard skips the fan-out when a whole calendar is deleted (its
-- events cascade-delete in the same statement, at depth 2) — no client
-- can ever present a token for that now-404 calendar, so per-event
-- tombstones would be pure waste.
CREATE OR REPLACE FUNCTION caldav.log_calendar_sync_changes_del()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO caldav.calendar_sync_changes
        (collection_calendar_id, member_id, member_ical_uid, change_kind)
    SELECT calendar_id, id, ical_uid, 'deleted'
      FROM changed_rows;

    RETURN NULL;
END;
$$;

-- ── UPDATE ────────────────────────────────────────────────────────────
-- Any change to a DAV-observable column counts (calendar_id is never
-- reassigned by the application, so there's no move branch to handle).
CREATE OR REPLACE FUNCTION caldav.log_calendar_sync_changes_upd()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO caldav.calendar_sync_changes
        (collection_calendar_id, member_id, member_ical_uid, change_kind)
    SELECT n.calendar_id, n.id, n.ical_uid, 'updated'
      FROM old_rows o
      JOIN new_rows n USING (id)
     WHERE (o.summary, o.description, o.location, o.start_time, o.end_time,
            o.all_day, o.rrule, o.ical_uid, o.ical_data)
           IS DISTINCT FROM
           (n.summary, n.description, n.location, n.start_time, n.end_time,
            n.all_day, n.rrule, n.ical_uid, n.ical_data);

    RETURN NULL;
END;
$$;

CREATE TRIGGER calendar_events_log_sync_changes_ins
    AFTER INSERT ON caldav.calendar_events
    REFERENCING NEW TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION caldav.log_calendar_sync_changes_ins();

CREATE TRIGGER calendar_events_log_sync_changes_del
    AFTER DELETE ON caldav.calendar_events
    REFERENCING OLD TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION caldav.log_calendar_sync_changes_del();

CREATE TRIGGER calendar_events_log_sync_changes_upd
    AFTER UPDATE ON caldav.calendar_events
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION caldav.log_calendar_sync_changes_upd();
