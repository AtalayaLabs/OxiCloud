-- Persist the OIDC session identifier (`sid` claim from the id_token) so
-- the Back-Channel Logout endpoint can revoke a specific device without
-- wiping every other OxiCloud session the user has open.
--
-- OIDC Back-Channel Logout 1.0 requires the logout_token to carry `sub`
-- and/or `sid`. Preferring `sid` (per-session) over `sub` (all sessions)
-- matters when a user is logged in from a laptop AND a phone through the
-- same IdP: logging out on the laptop should not evict the phone.
--
-- Nullable because:
--   * non-OIDC sessions (password / magic-link) don't have a sid;
--   * OIDC IdPs are free to omit the `sid` claim from id_tokens — Keycloak
--     only emits it when "Backchannel Logout Session Required" is enabled
--     on the client. When it's missing we fall back to sub-based revocation
--     (all sessions for that OIDC subject).
--
-- Indexed for the O(1) revoke-by-sid lookup path called from the BCL handler.
ALTER TABLE auth.sessions
    ADD COLUMN IF NOT EXISTS oidc_sid TEXT;

CREATE INDEX IF NOT EXISTS idx_sessions_oidc_sid
    ON auth.sessions(oidc_sid)
    WHERE oidc_sid IS NOT NULL AND NOT revoked;

COMMENT ON COLUMN auth.sessions.oidc_sid IS
    'OIDC session identifier (sid claim) from the id_token. Used by the backchannel-logout endpoint to revoke a single device.';
