-- True revocation for every token already issued to a user.
--
-- `revoke_all_user_sessions` sets `auth.sessions.revoked = TRUE`, and
-- that row holds the REFRESH token — so revoking has meant "cannot
-- renew", not "stop now". The auth middleware reads `claims.sid` only
-- to feed the last-seen tracker and never asks whether the session was
-- revoked, by design: no I/O on that path. So an access token kept
-- working for its remaining lifetime (default 1 h) after an admin
-- revoked every session, after a password change, and after a
-- deactivation.
--
-- This column is the cutoff. Any token whose `iat` precedes it is
-- rejected, which costs one timestamp comparison against the
-- already-cached `UserFlags` row — no new query on the hot path. See
-- `docs/plan/authn-pipeline.md` § True revocation, at no per-call cost.
--
-- NULLABLE, with no default, on purpose: NULL means "nothing revoked",
-- so deploying this migration invalidates nothing. A `DEFAULT NOW()`
-- would log out every user on the instance the moment it ran.

ALTER TABLE auth.users
    ADD COLUMN sessions_valid_from TIMESTAMPTZ;

COMMENT ON COLUMN auth.users.sessions_valid_from IS
    'Tokens issued before this instant are rejected. NULL = nothing revoked. Set by revoke-all, password change and deactivation; compared against the JWT iat using the cached UserFlags row, so it adds no per-request query.';

-- No index. The column is only ever read as part of the single-row
-- `get_user_flags` lookup by primary key, never filtered on.
