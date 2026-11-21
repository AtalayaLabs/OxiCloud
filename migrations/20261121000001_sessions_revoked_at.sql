-- Per-session revocation: WHEN a session was revoked, not just that it
-- was.
--
-- `sessions_valid_from` on the user row (20261121000000) kills every
-- token at once, which is right for "sign out everywhere" and wrong for
-- everything else — `change_password` deliberately keeps the caller's
-- session alive, and `DELETE /api/admin/sessions/{id}` names one row.
-- A single `iat` watermark cannot express "all except this one".
--
-- The per-session form needs a BOUNDED set of revoked session ids on
-- the cached `UserFlags` row, and bounding it requires a timestamp: a
-- revoked `sid` only has to be remembered until any access token
-- bearing it would have expired anyway. With `revoked` as a bare
-- boolean there was nothing to bound by, so the set would have had to
-- fall back to `expires_at` — the REFRESH expiry, seven days by
-- default — putting up to a week of revoked ids inside a hot-path cache
-- entry.
--
-- NULLABLE with no backfill, deliberately. A row revoked before this
-- column existed reads as NULL, falls outside the window, and so is not
-- carried in the set — correct, because any access token it issued
-- expired long ago. Backfilling `NOW()` would instead pull every
-- historically-revoked session into the window at once.

ALTER TABLE auth.sessions
    ADD COLUMN revoked_at TIMESTAMPTZ;

COMMENT ON COLUMN auth.sessions.revoked_at IS
    'When this session was revoked; NULL if it never was (or predates the column). Read as a bounded set of session ids on the cached UserFlags row, so a revoked access token is refused per request without a query.';

-- No new index. The read is
--   WHERE user_id = $1 AND revoked AND revoked_at > $2
-- and `idx_sessions_active (user_id, revoked)` from the initial schema
-- already serves the selective part; `revoked_at` then filters a
-- handful of rows per user. An index on it would be written on every
-- revocation to speed up a scan that never grows.
