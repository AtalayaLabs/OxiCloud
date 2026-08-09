-- Session origin — how the row was minted.
--
-- Populated at session-mint time by each login handler (legacy password,
-- OPAQUE aPAKE, magic-link redemption, OIDC callback, RFC 8628 device
-- authorization). Refresh copies the parent session's origin (a refresh
-- doesn't change how the user originally authenticated). Existing rows
-- predating this column default to `unknown`.
--
-- Purpose: gives admins a first-class filter on the sessions panel
-- ("show me only the OIDC sessions", "spot the magic-link ones during
-- a suspected phishing wave") without them having to infer from
-- adjacent fields (`oidc_id_token IS NOT NULL` etc.). Also drives
-- correlation with audit lines that already carry the same enum.
--
-- Stored as `text` rather than a PG ENUM: enums lock the schema (adding
-- a new variant needs a migration + release coordination), whereas a
-- checked text column can gain values by editing the constraint. The
-- Rust `SessionOrigin` enum uses `#[serde(rename_all = "snake_case")]`
-- so wire values match column values one-to-one.
--
-- No index — origin is a display column read alongside the row by PK;
-- filtering happens client-side in the admin panel (page size caps at
-- 100, so scanning is fine).
ALTER TABLE auth.sessions
    ADD COLUMN IF NOT EXISTS origin TEXT NOT NULL DEFAULT 'unknown';

-- Enforce the known values at the storage layer so a rogue INSERT
-- can't smuggle an arbitrary string that would then confuse the
-- serde-typed enum deserialize on read. Adding a new variant is a
-- one-line ALTER + Rust enum change.
ALTER TABLE auth.sessions
    ADD CONSTRAINT sessions_origin_known
    CHECK (origin IN ('password', 'opaque', 'magic_link', 'oidc', 'device', 'unknown'));

COMMENT ON COLUMN auth.sessions.origin IS
    'How this session was minted: password | opaque | magic_link | oidc | device | unknown. Set at INSERT time by the login handler; carried over on refresh.';
