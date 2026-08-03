-- Persist the OIDC ID token on the session so it can be used as
-- `id_token_hint` in the RP-initiated logout URL sent back to the FE.
--
-- Without this, OxiCloud logout only clears the local session; the IdP
-- SSO cookie stays alive and — under the `auto_redirect_if_standalone_oidc`
-- posture — the very next `/login` visit silently re-authenticates the
-- user via the IdP session. Shared-computer scenario: a user can't
-- actually log out.
--
-- Nullable because the column only applies to OIDC-issued sessions;
-- password / magic-link sessions leave it NULL. Stored as-is (unencrypted)
-- because ID tokens are short-lived JWTs whose PII payload (email, name)
-- is already present in cleartext in auth.users — no new exposure.
ALTER TABLE auth.sessions
    ADD COLUMN IF NOT EXISTS oidc_id_token TEXT;

COMMENT ON COLUMN auth.sessions.oidc_id_token IS
    'ID token from the OIDC login exchange, used as id_token_hint for RP-initiated logout. NULL for non-OIDC sessions.';
