-- Drop the dead `is_public` flag from caldav.calendars.
--
-- The column has been unreachable since the domain `Calendar` entity
-- never carried the field: both `create_calendar` and `update_calendar`
-- bound a literal `false`, each with the comment "is_public doesn't
-- exist as a field". So every row is FALSE, `list_public_calendars`
-- could only ever return the empty set, and the twelve
-- `calendar.is_public || has_calendar_perm(...)` read gates had a left
-- operand that was constant-false.
--
-- Nothing is lost: there is no row this drop can change the meaning of.
-- Calendar sharing is unaffected — it runs on `storage.role_grants`
-- (`Resource::Calendar`) and never consulted this column.
--
-- The count is reported rather than assumed: if some path this audit
-- missed ever did set the flag, the operator sees it in the migration
-- log instead of losing it silently.
DO $$
DECLARE
    public_rows BIGINT;
BEGIN
    SELECT count(*) INTO public_rows
    FROM caldav.calendars
    WHERE is_public;

    IF public_rows > 0 THEN
        RAISE WARNING
            'dropping caldav.calendars.is_public: % row(s) had it set. Expected 0 — the write paths bound a literal false. Those calendars are now reachable only through their grants.',
            public_rows;
    END IF;
END
$$;

ALTER TABLE caldav.calendars DROP COLUMN IF EXISTS is_public;
