-- ============================================================================
-- Slice 1 of Part 2 (Recoverable-Run Engine) — see
-- docs/plan/job-registry.md#part-2--recoverable-run-engine.
--
-- Introduces the `jobs.*` schema housing state for long-running,
-- restart-tolerant jobs (storage migration, reextract-*, consistency
-- checks). Deliberately distinct from `auth.*` / `storage.*` / `admin.*`
-- because this is JOB-runtime state, not domain data.
--
-- This migration lands the ENGINE table only. Job-specific per-record
-- artifacts (the `jobs.run_findings` generic table used by consistency
-- checks + migration failure logs + reextract failures) land alongside
-- Slice 2 or its first tenant PR — deferred here to keep the review
-- surface tight.
-- ============================================================================

CREATE SCHEMA IF NOT EXISTS jobs;

-- ─── recoverable_runs ────────────────────────────────────────────────────────
-- One row per RUN of a RecoverableJobHandler tenant. Non-terminal
-- rows carry the live cursor + stats; terminal rows are the audit
-- trail (last-run visibility for `GET /api/admin/jobs`, retention
-- pruning deferred).
--
-- `status` values (TEXT, enforced by the partial unique index below
-- plus per-tenant discipline):
--   • 'Running'         — actively executing.
--   • 'Paused'          — cooperatively yielded (cancel poll or
--                         graceful shutdown). Resumable from `cursor`.
--   • 'CancelRequested' — cancel signalled; handler is winding down.
--                         Still non-terminal — a re-trigger must NOT
--                         spawn a parallel run.
--   • 'Completed'       — walked the whole space. Terminal.
--   • 'Failed'          — irrecoverable error. Terminal.
--                         `error_message` is populated.
--
-- Column notes:
--   • `started_at` is FIXED at run start — long-running consistency
--     scans use it as their grace-window reference (see trap #1 in
--     docs/plan/consistency-check.md).
--   • `last_progress_at` bumps on every checkpoint. Doubles as heartbeat
--     for the boot-time crash-recovery sweep (Running rows with stale
--     heartbeats get flipped to Paused on server restart).
--   • `cursor` is opaque bytes per-job (BLAKE3 hash for blob scans,
--     UUID for file scans, ltree path for folder-tree scans). NULL on
--     a fresh run's first checkpoint.
--   • `stats` accumulates per-run counters (scanned_count,
--     migrated_blobs, findings_this_run, …). JSONB shape is per-job;
--     the reserved top-level `count` mirrors JobOutcome::Ok.count.
--   • `params` carries per-run params captured at start (grace_window_secs,
--     source_backend, …). Distinct from stats — params are set once,
--     stats accumulate.

CREATE TABLE jobs.recoverable_runs (
    id                UUID        PRIMARY KEY,
    job_name          TEXT        NOT NULL,
    status            TEXT        NOT NULL,
    started_at        TIMESTAMPTZ NOT NULL,
    last_progress_at  TIMESTAMPTZ NOT NULL,
    completed_at      TIMESTAMPTZ,
    cursor            BYTEA,
    stats             JSONB       NOT NULL DEFAULT '{}'::jsonb,
    params            JSONB       NOT NULL DEFAULT '{}'::jsonb,
    error_message     TEXT
);

-- Exclusivity — the load-bearing invariant.
--
-- "At most one non-terminal run per job_name." Enforced at the DB
-- layer so concurrent triggers or scheduler-vs-operator races cannot
-- create a parallel run. The partial index makes duplicate INSERTs
-- fail with a unique-violation; the trigger-endpoint handler catches
-- that and returns the surviving row instead.
--
-- CancelRequested is INCLUDED — a second trigger during cancel must
-- not spawn a parallel run.
CREATE UNIQUE INDEX one_active_run_per_job
    ON jobs.recoverable_runs (job_name)
    WHERE status IN ('Running', 'Paused', 'CancelRequested');

-- Boot recovery sweep index. On restart, `UPDATE ... SET status='Paused'
-- WHERE status='Running' OR status='CancelRequested'` finds all abandoned
-- rows. Partial index keeps it a fast index-only scan even when the table
-- accumulates terminal (Completed/Failed) rows over time.
CREATE INDEX ON jobs.recoverable_runs (last_progress_at)
    WHERE status = 'Running';

-- "Latest run per job" query — powers `GET /api/admin/jobs` when
-- recoverable jobs appear in the listing (`SELECT DISTINCT ON (job_name)
-- ... ORDER BY job_name, started_at DESC`).
CREATE INDEX ON jobs.recoverable_runs (job_name, started_at DESC);
