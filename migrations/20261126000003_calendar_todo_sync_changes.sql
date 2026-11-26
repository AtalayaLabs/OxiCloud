-- RFC 6578 incremental sync-collection: log VTODO changes too.
--
-- `caldav.calendar_todos` (#754, `20261029000000_caldav_vtodo.sql`) shares
-- the calendar collection with `caldav.calendar_events`, so a task change
-- must advance the same per-calendar change log
-- (`20261126000001_calendar_sync_changes.sql`). Without these triggers an
-- incremental sync-collection never reports a created, edited or deleted
-- task: the client keeps a stale copy forever.
--
-- One log for both kinds: a row only records (calendar, member, UID,
-- kind of change). The sync service hydrates every touched UID from BOTH
-- object tables, so the log needs no event/task discriminator.
--
-- INSERT / DELETE reuse the event functions — they read only `id`,
-- `calendar_id` and `ical_uid`, which `calendar_todos` has with the same
-- names and types. Same `pg_trigger_depth() > 1` guard and footgun as
-- documented there: a whole-calendar delete cascades to its tasks at
-- depth 2 and is deliberately not logged per task.
--
-- UPDATE needs its own function: the DAV-observable column set differs
-- (no `end_time`; `due_time`, `completed_at`, `status`, …). `ical_data`
-- is authoritative and served verbatim, so it alone would suffice — the
-- filter-index columns are compared too so that a future write path that
-- touches only those still advances the log.

-- ── UPDATE ────────────────────────────────────────────────────────────
CREATE OR REPLACE FUNCTION caldav.log_calendar_todo_sync_changes_upd()
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
     WHERE (o.summary, o.description, o.location, o.status, o.percent_complete,
            o.priority, o.start_time, o.due_time, o.completed_at, o.all_day,
            o.rrule, o.ical_uid, o.ical_data, o.recurrence_id)
           IS DISTINCT FROM
           (n.summary, n.description, n.location, n.status, n.percent_complete,
            n.priority, n.start_time, n.due_time, n.completed_at, n.all_day,
            n.rrule, n.ical_uid, n.ical_data, n.recurrence_id);

    RETURN NULL;
END;
$$;

CREATE TRIGGER calendar_todos_log_sync_changes_ins
    AFTER INSERT ON caldav.calendar_todos
    REFERENCING NEW TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION caldav.log_calendar_sync_changes_ins();

CREATE TRIGGER calendar_todos_log_sync_changes_del
    AFTER DELETE ON caldav.calendar_todos
    REFERENCING OLD TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION caldav.log_calendar_sync_changes_del();

CREATE TRIGGER calendar_todos_log_sync_changes_upd
    AFTER UPDATE ON caldav.calendar_todos
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION caldav.log_calendar_todo_sync_changes_upd();
