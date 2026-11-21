-- Two IPs per session: where it was created, and where it is used.
--
-- Keeping both matters because **the mismatch is the signal**. A device
-- authorised from a home address and now being used from somewhere else
-- is the shape of a stolen refresh token, and no single column can
-- express it. Neither can a connected-device list that shows only where
-- the human clicked "approve".
--
-- `ip_address` is renamed rather than joined by a second
-- creation-IP column. It was stamped once, at session creation, on every
-- path — the login IP for a browser or DAV client, and for the device
-- flow the APPROVER's browser IP, since the device itself never reaches
-- that endpoint. So it already WAS the "created from" value and only the
-- name said otherwise; adding `ip_approved` beside it (as this feature
-- was first sketched) would have duplicated it under a better name while
-- leaving the misleading one in place.
--
-- The names mirror the timestamps they pair with, which is why they are
-- `created_ip` / `last_seen_ip` rather than `ip_created` / `ip_seen`:
--
--     created_at    created_ip
--     last_seen_at  last_seen_ip
--
-- so each row of the admin table reads as "when, and from where".
--
-- The rename is wire-visible: the column surfaces on
-- `GET /api/admin/sessions`. That surface is admin-only and consumed by
-- this project's own panel — it is NOT part of the NextCloud-compatible
-- API, where a field rename would cost adopters.

ALTER TABLE auth.sessions
    RENAME COLUMN ip_address TO created_ip;

-- NULLABLE, and NOT backfilled from `created_ip`: a session that has
-- made no request since this migration has no observed IP, and NULL says
-- so honestly. Copying `created_ip` would assert an observation that
-- never happened — and would erase the mismatch signal on exactly the
-- sessions where it matters most, the long-lived ones predating this.
ALTER TABLE auth.sessions
    ADD COLUMN last_seen_ip TEXT;

COMMENT ON COLUMN auth.sessions.created_ip IS
    'IP the session was CREATED from — the login IP, or for origin=device the approver''s browser IP (the device never reaches that endpoint). Stamped once, never updated.';

COMMENT ON COLUMN auth.sessions.last_seen_ip IS
    'IP the session was last USED from, written by LastSeenTracker on its batched flush. Accurate to within the flush interval, not instantaneous. NULL until the session makes a request. A mismatch against created_ip is a credential-theft signal.';

-- No index on either. Both are read only as part of a session row
-- already being fetched by id or by user_id, never filtered on.
