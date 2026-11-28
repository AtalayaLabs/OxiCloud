-- RFC 6578 sync-collection: make the change logs safe against commit-order
-- skew.
--
-- The bug: `seq` is an IDENTITY value assigned when a change row is
-- INSERTed, but the row only becomes visible when its transaction COMMITs.
-- A slow writer can take seq 100, a fast writer seq 101 and commit first;
-- a client that polls in between is handed a token at 101 and its next
-- poll asks for `seq > 101`. When the slow writer commits, seq 100 lies
-- below every token already handed out and is never delivered: the client
-- silently diverges from the server. The same hole exists on an initial
-- sync whose token is minted while a change is still in flight.
--
-- The fix: every row records the top-level transaction id that wrote it
-- (`xid`, the 64-bit epoch-extended form, stored as BIGINT so it can be
-- aggregated on PostgreSQL 13), and every token carries the snapshot it was
-- minted in (`xmin:xmax:xip`, the `pg_snapshot` text form). The rows a token
-- has not covered are exactly those written by a transaction its snapshot
-- could not see:
--
--     xid >= token.xmax  OR  xid = ANY(token.xip)
--
-- and a poll bounds itself to what its own new snapshot sees, so every
-- change is delivered exactly once — no gap, no re-report, whatever order
-- transactions commit in.
--
-- Retention: the watermark tables also record the highest purged xid and
-- the xid of the transaction that purged. A purged row with xid >= a
-- token's xmin may be one that token never delivered, so such a token is
-- expired (full resync), never answered with a silent gap — but only if the
-- token's snapshot did not see the purge: a purged row was committed before
-- the purge, so a token minted after it has seen every purged row. Without
-- that, any long-running transaction (holding xmin down) expired every
-- token on arrival after a purge, and clients resynced in a loop.
--
-- Existing rows get the migration transaction's xid. Tokens minted before
-- this migration do not parse in the new format and are answered with
-- DAV:valid-sync-token, so their clients resync.

ALTER TABLE storage.folder_sync_changes
    ADD COLUMN IF NOT EXISTS xid BIGINT NOT NULL
        DEFAULT (pg_current_xact_id()::text::bigint);
ALTER TABLE caldav.calendar_sync_changes
    ADD COLUMN IF NOT EXISTS xid BIGINT NOT NULL
        DEFAULT (pg_current_xact_id()::text::bigint);
ALTER TABLE carddav.contact_sync_changes
    ADD COLUMN IF NOT EXISTS xid BIGINT NOT NULL
        DEFAULT (pg_current_xact_id()::text::bigint);

-- Serves the delta query, which selects by xid within one collection.
CREATE INDEX IF NOT EXISTS idx_folder_sync_changes_collection_xid
    ON storage.folder_sync_changes (collection_folder_id, xid);
CREATE INDEX IF NOT EXISTS idx_calendar_sync_changes_collection_xid
    ON caldav.calendar_sync_changes (collection_calendar_id, xid);
CREATE INDEX IF NOT EXISTS idx_contact_sync_changes_collection_xid
    ON carddav.contact_sync_changes (collection_address_book_id, xid);

-- Highest xid retention has purged for the collection, and the xid of the
-- latest purging transaction. NULL = none.
ALTER TABLE storage.folder_sync_watermark
    ADD COLUMN IF NOT EXISTS low_water_xid BIGINT,
    ADD COLUMN IF NOT EXISTS purged_by_xid BIGINT;
ALTER TABLE caldav.calendar_sync_watermark
    ADD COLUMN IF NOT EXISTS low_water_xid BIGINT,
    ADD COLUMN IF NOT EXISTS purged_by_xid BIGINT;
ALTER TABLE carddav.contact_sync_watermark
    ADD COLUMN IF NOT EXISTS low_water_xid BIGINT,
    ADD COLUMN IF NOT EXISTS purged_by_xid BIGINT;
