-- Durable deletion intent for backend objects.
--
-- The defect this exists for: GC deletes the `storage.blobs` row and then
-- unlinks the backend object best-effort. When the unlink fails — routine on a
-- remote backend, and the norm during an outage — the bytes are stranded with no
-- row, so `dedup_gc` cannot see them: it is DB-driven, and the row is what it
-- drives from. They are discoverable only by `backend_consistency`'s bucket
-- walk, which is registered on-demand and which nothing schedules. Hence 29
-- orphaned blobs accumulating unnoticed on a live S3 deployment.
--
-- The asymmetry worth internalising: BOTH sides of this system leak bytes on
-- failure, and they differ only in discoverability. A failed *creation* ends with
-- a PG row at ref_count 0, which the existing reclaimer finds — `IngestGuard`
-- registers written chunks precisely so it can. A failed *deletion* ends with no
-- row at all. This table is how the deletion path gets the property the creation
-- path already has.
--
-- See `docs/plan/storage-consistency.md` §2.

CREATE TABLE IF NOT EXISTS storage.pending_actions (
    -- Content hash of the backend object to act on.
    hash            TEXT NOT NULL,

    -- What to do with it. One value today, and deletion is structurally the only
    -- per-blob operation that NEEDS a queue: rotation and migration retry by
    -- re-walking `storage.blobs`, which works only because the row survives the
    -- operation. The CHECK keeps that honest, so a second action has to be added
    -- deliberately by someone who has read whether the drain's semantics
    -- transfer — see the ordering invariant in the plan, which they would not.
    action          TEXT NOT NULL DEFAULT 'deletion'
                         CHECK (action IN ('deletion')),

    -- Which tier released it. Informational: useful in findings and for draining
    -- one tier at a time, deliberately NOT part of identity — the tiers dedup to
    -- shared blobs, so one unlink settles a hash however many tiers released it.
    object          TEXT NOT NULL
                         CHECK (object IN ('blob', 'derived', 'attached')),

    -- Storage entry the object lived on, or NULL for "drain via the active
    -- backend".
    --
    -- Nullable on purpose. Nothing can report the entry name at enqueue time —
    -- `DedupService` holds a hot-swappable stack and no layer says which entry it
    -- was built for — so NOT NULL would mean writing a guess. NULL is only
    -- dangerous if a storage cutover lands while the queue is non-empty, which is
    -- closed operationally by draining as part of the cutover, where
    -- `backend_migration` already freezes writes.
    --
    -- This is the one field the drain cannot re-derive; everything else it
    -- re-verifies against current truth. That is why draining before a cutover is
    -- a requirement rather than a nicety.
    entry_name      TEXT,

    -- For reporting reclaimable space in the backlog view.
    size_bytes      BIGINT NOT NULL DEFAULT 0,

    -- Fairness only — oldest first, so nothing starves. MUST NOT become an
    -- ordering key: `now()` is transaction-start time, so this is neither commit
    -- order nor causal order, and a row can surface behind a point the drain has
    -- already passed. Safe precisely because the queue carries no ordering
    -- semantics (see the plan's dirty-set invariant).
    requested_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Per-item backoff, and the input to parking.
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TIMESTAMPTZ,

    -- Without this a parked entry is a mystery at 3am.
    last_error      TEXT,

    -- Set when attempts are exhausted: stop retrying automatically and wait for
    -- a human. Un-parking is an explicit operator action, never a side effect.
    parked_at       TIMESTAMPTZ,

    -- `action` is in the key, not just `hash`. With `hash` alone the table holds
    -- exactly one action per object, so the first time a second action type
    -- exists, enqueueing it for a hash with a pending deletion would either
    -- clobber that deletion (ON CONFLICT DO UPDATE) or be silently dropped (DO
    -- NOTHING) — a multi-action table with a single-action key. Cheap now,
    -- expensive later.
    --
    -- `object` stays OUT of the key so two tiers releasing the same hash collapse
    -- into one row and one unlink.
    PRIMARY KEY (hash, action)
);

-- No FK to `storage.blobs`: the row being gone is the entire point.

-- The drain's query: oldest unparked first. Partial index because parked rows are
-- never selected by a normal run, and on a healthy instance the whole table is
-- short-lived anyway.
CREATE INDEX IF NOT EXISTS idx_pending_actions_drain
    ON storage.pending_actions (requested_at)
    WHERE parked_at IS NULL;

-- The backlog view (§2c) reports parked entries separately, and an operator
-- looking at them wants the newest failures first.
CREATE INDEX IF NOT EXISTS idx_pending_actions_parked
    ON storage.pending_actions (parked_at DESC)
    WHERE parked_at IS NOT NULL;

COMMENT ON TABLE storage.pending_actions IS
    'Durable intent to act on a backend object whose PG row is already gone. '
    'A row is a HINT, not a command: it means "this hash was last observed at '
    'refcount 0 — check, and if it still is, unlink". The authority on whether '
    'an object should exist is storage.blobs / storage.chunk_manifests, never '
    'this table, which is what makes the drain order-free and idempotent. '
    'Drained by the backend_reclaim job. See docs/plan/storage-consistency.md.';
