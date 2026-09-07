-- Session liveness tracking — per-request `last_seen_at` stamp on
-- `auth.sessions`, moved by the in-process `LastSeenTracker`
-- (see `src/application/services/last_seen_tracker.rs`) via a
-- batched UPDATE every 30 s.
--
-- Distinct from `created_at`: that column moves on session ROTATION
-- (every silent refresh), so its resolution is capped at the
-- access-token TTL (default 3600 s). `last_seen_at` moves on every
-- authenticated request, so the "active in the last N min" query
-- underlying `oxicloud_sessions_active` / `_active_users` gauges is
-- accurate to the flusher's 30 s cadence regardless of token TTL.
--
-- See `docs/plan/sessions.md` for the full design (why DashMap +
-- periodic flush, why partial index, why `NOW()` default).

ALTER TABLE auth.sessions
    ADD COLUMN last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

-- Partial index — the only reads on this column are the gauge
-- queries in `session_liveness_gauges.rs`, and they always filter
-- `revoked = FALSE`. Indexing only unrevoked rows keeps the write
-- cost of the 30 s batched UPDATE flat: rotated / revoked rows
-- fall out of the index automatically when `revoked` flips to TRUE
-- (partial-index maintenance drops them, no re-scan). A full
-- b-tree on the column would double index size for zero read
-- benefit — every gauge query would skip the revoked half anyway.
CREATE INDEX idx_sessions_last_seen_at ON auth.sessions(last_seen_at)
    WHERE revoked = FALSE;
