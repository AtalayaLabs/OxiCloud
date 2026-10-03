-- Split a stopped run's "why" into a key and a message.
--
-- `error_message` already held the whole story: context plus the full
-- error chain, e.g. "deep verify read: Transient Backend: Failed to get
-- blob 08b3…: network I/O error: … dns error …". Good for a human,
-- useless for anything that has to MATCH on it.
--
-- The short key did exist — but only as a literal inside a `tracing`
-- call (`reason = "backend_unavailable"`), so it could not be read back,
-- filtered, grouped, or de-duplicated against. Every transient cause
-- also logged the same literal, which meant a DNS failure and a timeout
-- were indistinguishable downstream.
--
-- So: `error_reason` is the stable vocabulary (`backend_unavailable`,
-- `backend_timeout`, `job_failed`, `server_restart`) and `error_message`
-- stays the prose. Out-of-band alerting keys off the former — a run that
-- stopped with it set is worth telling an operator about, and an operator
-- pause sets neither, which is what keeps a human stopping a job from
-- paging anyone.
--
-- Nullable with no backfill, deliberately: rows written before this
-- migration have prose but no key, and inventing one by pattern-matching
-- old message text would manufacture a vocabulary that was never
-- recorded. A NULL reason on an old row reads correctly as "we did not
-- capture one".

ALTER TABLE jobs.recoverable_runs
    ADD COLUMN IF NOT EXISTS error_reason TEXT;

COMMENT ON COLUMN jobs.recoverable_runs.error_reason IS
    'Stable machine-readable key for why the run stopped '
    '(backend_unavailable, backend_timeout, job_failed, server_restart). '
    'NULL on an operator pause and on any run that completed normally. '
    'Fixed vocabulary — log filters, mailbox rules and alert '
    'de-duplication match on it, so values are never reworded.';

COMMENT ON COLUMN jobs.recoverable_runs.error_message IS
    'Human-readable detail for why the run stopped: context plus the '
    'full error chain. Prose, free to be reworded; match on '
    'error_reason instead.';
