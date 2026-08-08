-- Phase A of the federation-identity rename (see docs/plan/ocm.md § Schema rename).
--
-- Prepares the identity model to accommodate OCM + IndieAuth + OpenID Federation
-- alongside today's single-strategy OIDC surface. Nothing about VALUES changes in
-- this migration; the rename is pure and the new `federation_kind` column is
-- backfilled deterministically from the presence of the existing OIDC columns.
--
-- Value semantics (`federation_issuer` still holds the display-name label today,
-- not the true `iss` URL) are corrected in Phase B — see plan doc. Ship this
-- first so the schema shape is stable before that value migration.
--
-- Rollback: DROP the federation_kind column; the RENAMEd columns keep working
-- with pre-rename code paths because their values are untouched.

ALTER TABLE auth.users RENAME COLUMN oidc_provider TO federation_issuer;
ALTER TABLE auth.users RENAME COLUMN oidc_subject  TO federation_subject;

ALTER TABLE auth.users ADD COLUMN federation_kind TEXT
    CHECK (federation_kind IN ('magic_link', 'ocm', 'oidc'));

-- Backfill: existing rows with a federation_issuer set are all OIDC-linked
-- (only the OIDC flow populated the old oidc_provider column). Anything NULL
-- stays NULL — local / password / magic-link users don't get a kind.
UPDATE auth.users
   SET federation_kind = 'oidc'
 WHERE federation_issuer IS NOT NULL;

-- Swap the anti-duplicate uniqueness. Old index was keyed on
-- (oidc_provider, oidc_subject); new one includes federation_kind so OIDC
-- and OCM (and future IndieAuth / OpenID Federation) principals stay
-- distinct even if their (issuer, subject) tuples collide across kinds.
DROP INDEX IF EXISTS auth.idx_users_oidc;
CREATE UNIQUE INDEX idx_users_federation
    ON auth.users(federation_kind, federation_issuer, federation_subject)
    WHERE federation_kind IS NOT NULL;

COMMENT ON COLUMN auth.users.federation_kind IS
    'Trust chain that owns this identity: oidc | ocm | magic_link. NULL for pure local users. See docs/plan/ocm.md and docs/plan/federated-login.md.';

COMMENT ON COLUMN auth.users.federation_issuer IS
    'Authority that mints the subject id. OIDC: iss URL. OCM: peer domain. Magic-link: NULL. NOTE: legacy rows may still hold the OXICLOUD_OIDC_PROVIDER_NAME display label; Phase B of the federation-identity rename backfills these to real issuer URLs.';

COMMENT ON COLUMN auth.users.federation_subject IS
    'Stable identifier for this user within the federation_issuer. OIDC: sub claim (stable per RFC 7519 §4.1.2). OCM: federated address (protocol has no separate stable id in 1.1). Magic-link: NULL.';
