-- ════════════════════════════════════════════════════════════════════════════
-- Collaborative document sessions — Phase A C1
-- ════════════════════════════════════════════════════════════════════════════
--
-- One row per file that has EVER had a collaborative editing session. The
-- CRDT state persists across all clients disconnecting so a client
-- reopening an idle-GC'd doc reconstitutes the same authored history.
--
-- Design: docs/plan/markdown-collab.md § Data model — collab doc sessions.
-- Architecture: docs/architecture/message-bus-and-notifications.md (Topic::Collab
-- reservation on the message bus port; C2 lands the WS binary-frame routing).
--
-- Key invariants:
--   * `file_id` is the primary key AND a FK to `storage.files(id)`.
--     Matches the blob a flush-to-blob write targets — one CRDT session
--     never spans two files. (Note: NOT `storage.file_metadata` — that
--     table holds EXIF/media metadata, keyed on `file_id`, and only
--     exists for files that have been through the media pipeline. The
--     canonical file table is `storage.files`.)
--   * ON DELETE CASCADE handles hard file-delete cleanly — no orphan
--     CRDT state after the file row is removed. Soft-delete
--     (`storage.files.is_trashed = true`) does NOT remove the row, so
--     the collab session survives a trip through the trash — matches
--     the "restore from trash keeps history" UX.
--   * `state` holds a serialised `yrs::Doc` snapshot; `state_vector` is the
--     Yjs state vector for cheap sync-step-1 catch-up on reconnect. Both
--     rotate every N updates via compaction (default N=200 per plan).
--   * `updates_since_snapshot` tracks non-compacted updates layered on top
--     of `state` — the service compacts when this passes threshold.
--   * `last_flushed_content_hash` records the content hash of the last
--     successful flush-to-blob write. The service short-circuits flushes
--     when re-computing the CRDT's text yields the same hash (no wire
--     write for a no-op edit).
--   * `last_activity_at` drives idle-GC — sessions with no attached
--     sockets AND `last_activity_at < now - idle_ttl` get one final flush
--     and their row dropped by the background task (default 30 min).
BEGIN;

CREATE SCHEMA IF NOT EXISTS collab;

CREATE TABLE collab.doc_sessions (
    file_id                    UUID        PRIMARY KEY
                                           REFERENCES storage.files(id) ON DELETE CASCADE,
    state                      BYTEA       NOT NULL,
    state_vector               BYTEA       NOT NULL,
    updates_since_snapshot     INTEGER     NOT NULL DEFAULT 0,
    last_flushed_content_hash  TEXT,
    last_flushed_at            TIMESTAMPTZ,
    last_activity_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at                 TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Idle-GC scans `last_activity_at ASC`. The index makes the "oldest N
-- untouched sessions" query cheap without a full-table sort.
CREATE INDEX idx_collab_doc_sessions_stale
    ON collab.doc_sessions (last_activity_at);

COMMENT ON TABLE  collab.doc_sessions IS
    'CRDT state for collaborative markdown / text editing sessions. See docs/plan/markdown-collab.md § Data model.';
COMMENT ON COLUMN collab.doc_sessions.state IS
    'Serialised yrs::Doc snapshot; opaque bytes on the DB side. Regenerated on compaction.';
COMMENT ON COLUMN collab.doc_sessions.state_vector IS
    'Yjs state vector for cheap sync-step-1 replies. Cached alongside state so reconnect probes skip re-parsing the doc.';
COMMENT ON COLUMN collab.doc_sessions.updates_since_snapshot IS
    'Non-compacted updates layered on top of state. Service compacts (re-serialises as one snapshot) when this passes CollabLimits::snapshot_after_updates.';
COMMENT ON COLUMN collab.doc_sessions.last_flushed_content_hash IS
    'Content hash of the last successful flush-to-blob write. NULL until the first flush. Used to short-circuit no-op flushes.';

COMMIT;
