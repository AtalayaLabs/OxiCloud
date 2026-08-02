-- RFC 6578 incremental sync-collection: durable change log for CardDAV
-- contacts.
--
-- Mirrors `caldav.calendar_sync_changes`
-- (`20260911000001_calendar_sync_changes.sql`) — same append-only shape,
-- same trigger population, same retention-sweep lifecycle. Scoped to
-- `carddav.contacts` only: contact groups are out of scope for
-- sync-collection (CardDAV addressbook collections enumerate contacts,
-- not groups).
--
-- `collection_address_book_id` is a plain UUID, deliberately WITHOUT a
-- FK to `carddav.address_books(id)` — same reasoning as the WebDAV and
-- CalDAV tables: deleting a whole address book cascades to delete its
-- contacts in the same statement, and a FK would reject the contact's
-- own tombstone insert once its parent address-book row is already
-- gone. A stale sync-token against a deleted address book 404s at
-- address-book-resolution time before the change-log is ever queried,
-- so the orphaned rows are inert and age out via the retention sweep
-- like any other.

CREATE TABLE IF NOT EXISTS carddav.contact_sync_changes (
    seq                        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    collection_address_book_id UUID NOT NULL,
    member_id                  UUID NOT NULL,
    member_uid                 TEXT NOT NULL,
    change_kind                TEXT NOT NULL CHECK (change_kind IN ('created', 'updated', 'deleted')),
    changed_at                 TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_contact_sync_changes_collection_seq
    ON carddav.contact_sync_changes (collection_address_book_id, seq);

CREATE INDEX IF NOT EXISTS idx_contact_sync_changes_changed_at
    ON carddav.contact_sync_changes (changed_at);

CREATE TABLE IF NOT EXISTS carddav.contact_sync_watermark (
    singleton     BOOLEAN NOT NULL DEFAULT TRUE PRIMARY KEY CHECK (singleton),
    low_water_seq BIGINT NOT NULL DEFAULT 0
);

INSERT INTO carddav.contact_sync_watermark (singleton, low_water_seq)
VALUES (TRUE, 0)
ON CONFLICT (singleton) DO NOTHING;

-- ── INSERT ────────────────────────────────────────────────────────────
CREATE OR REPLACE FUNCTION carddav.log_contact_sync_changes_ins()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO carddav.contact_sync_changes
        (collection_address_book_id, member_id, member_uid, change_kind)
    SELECT address_book_id, id, uid, 'created'
      FROM changed_rows;

    RETURN NULL;
END;
$$;

-- ── DELETE ────────────────────────────────────────────────────────────
-- Depth guard skips the fan-out when a whole address book is deleted
-- (its contacts cascade-delete in the same statement, at depth 2) — no
-- client can ever present a token for that now-404 address book, so
-- per-contact tombstones would be pure waste.
CREATE OR REPLACE FUNCTION carddav.log_contact_sync_changes_del()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO carddav.contact_sync_changes
        (collection_address_book_id, member_id, member_uid, change_kind)
    SELECT address_book_id, id, uid, 'deleted'
      FROM changed_rows;

    RETURN NULL;
END;
$$;

-- ── UPDATE ────────────────────────────────────────────────────────────
-- Any change to a DAV-observable column counts (address_book_id is
-- never reassigned by the application, so there's no move branch to
-- handle).
CREATE OR REPLACE FUNCTION carddav.log_contact_sync_changes_upd()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO carddav.contact_sync_changes
        (collection_address_book_id, member_id, member_uid, change_kind)
    SELECT n.address_book_id, n.id, n.uid, 'updated'
      FROM old_rows o
      JOIN new_rows n USING (id)
     WHERE (o.uid, o.full_name, o.first_name, o.last_name, o.nickname,
            o.organization, o.title, o.notes, o.photo_url, o.birthday,
            o.anniversary, o.email, o.phone, o.address, o.vcard, o.etag)
           IS DISTINCT FROM
           (n.uid, n.full_name, n.first_name, n.last_name, n.nickname,
            n.organization, n.title, n.notes, n.photo_url, n.birthday,
            n.anniversary, n.email, n.phone, n.address, n.vcard, n.etag);

    RETURN NULL;
END;
$$;

CREATE TRIGGER contacts_log_sync_changes_ins
    AFTER INSERT ON carddav.contacts
    REFERENCING NEW TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION carddav.log_contact_sync_changes_ins();

CREATE TRIGGER contacts_log_sync_changes_del
    AFTER DELETE ON carddav.contacts
    REFERENCING OLD TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION carddav.log_contact_sync_changes_del();

CREATE TRIGGER contacts_log_sync_changes_upd
    AFTER UPDATE ON carddav.contacts
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION carddav.log_contact_sync_changes_upd();
