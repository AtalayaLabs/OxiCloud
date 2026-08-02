-- ════════════════════════════════════════════════════════════════════════════
-- OPAQUE aPAKE registration + login (Phase 0)
-- ════════════════════════════════════════════════════════════════════════════
-- OPAQUE (RFC 9807) is a zero-knowledge password-authenticated key exchange:
-- the server never sees the user's passphrase, not on registration and not on
-- login. What lives server-side is the OPAQUE "envelope" — an opaque blob the
-- client uploads at registration time that can only be decrypted with the
-- passphrase (via a memory-hard KSF). Login is a two-round exchange that
-- proves possession of the passphrase without transmitting it.
--
-- This migration reserves the storage for the envelope + a small set of
-- lifecycle timestamps and the client-visible pubkey. The columns are added
-- as NULLABLE so existing password accounts keep working — the OPAQUE
-- endpoints ship inert (`OXICLOUD_OPAQUE_MODE=off`) in Phase 0. Later phases
-- populate these columns via a silent-migration hook on legacy login.
--
-- Columns:
--   opaque_envelope        BYTEA        — RegistrationUpload from opaque-ke,
--                                         serialized. Cannot be decrypted by
--                                         the server; only the client with the
--                                         correct passphrase can complete the
--                                         login handshake. This blob also
--                                         embeds the OPAQUE client static
--                                         pubkey used in the 3DH login step —
--                                         extracted at handshake time, not
--                                         denormalised into its own column
--                                         (see docs/plan/opaque.md: the E2EE
--                                         identity pubkey is a separate,
--                                         later concern with its own bridges
--                                         table; conflating the two now would
--                                         bias the E2EE design toward "one
--                                         KEK bridge = OPAQUE" which excludes
--                                         magic-link / OIDC users).
--   opaque_ciphersuite_version SMALLINT — matches
--                                         `AppConfig::opaque.ciphersuite_version`.
--                                         Reserved for a future ciphersuite
--                                         migration; current bind is v1 =
--                                         Ristretto255-SHA512-3DH-Argon2id.
--   opaque_registered_at   TIMESTAMPTZ  — first-ever successful registration.
--                                         Set by /register/finish. Presence
--                                         means "this user has an OPAQUE
--                                         envelope on file."
--   opaque_migrated_at     TIMESTAMPTZ  — first successful login VIA OPAQUE.
--                                         Presence means "legacy password
--                                         endpoint is refused for this user
--                                         (Phase 3+)." Distinct from
--                                         registered_at because the transition
--                                         from "envelope exists" to "OPAQUE is
--                                         mandatory" needs a separate signal.
--
--   force_password_change_at_next_login BOOLEAN — orthogonal but shipped in
--                                         the same migration because the
--                                         admin-set-password flow depends on
--                                         it (see docs/plan/opaque.md §Phase
--                                         0). Set by the admin reset
--                                         endpoint; cleared by change_password.
--                                         Enables the "admin picks a
--                                         temporary password, user must
--                                         replace it on first use" pattern
--                                         that keeps admin capability intact
--                                         through the OPAQUE cutover.
--
-- Indexes:
--   idx_users_opaque_migrated — partial (WHERE opaque_migrated_at IS NOT
--                               NULL). Answers "how many users have finished
--                               migration?" cheaply for the ops dashboard;
--                               would be sparse if unfiltered.

ALTER TABLE auth.users
    ADD COLUMN opaque_envelope BYTEA,
    ADD COLUMN opaque_ciphersuite_version SMALLINT,
    ADD COLUMN opaque_registered_at TIMESTAMPTZ,
    ADD COLUMN opaque_migrated_at TIMESTAMPTZ,
    ADD COLUMN force_password_change_at_next_login BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX idx_users_opaque_migrated
    ON auth.users (opaque_migrated_at)
    WHERE opaque_migrated_at IS NOT NULL;

COMMENT ON COLUMN auth.users.opaque_envelope IS
    'OPAQUE (RFC 9807) RegistrationUpload blob. Server-opaque; the passphrase
     is required client-side to complete the login handshake. NULL = user has
     no OPAQUE registration (Phase 0 default, or account pre-dates the
     silent-migration rollout in Phase 2). E2EE identity pubkeys live in a
     separate table added in the E2EE phase — this blob is auth-scoped only.';

COMMENT ON COLUMN auth.users.opaque_ciphersuite_version IS
    'Version of the OPAQUE ciphersuite this envelope was minted under. Bound
     at registration time; changing the server-side ciphersuite invalidates
     every envelope minted before the flip. Current bind is v1 =
     Ristretto255-SHA512-3DH with Argon2id KSF.';

COMMENT ON COLUMN auth.users.opaque_registered_at IS
    'When this account first minted an OPAQUE envelope. Presence means "the
     endpoints are usable for this user." Distinct from opaque_migrated_at:
     you can be registered but still fall back to legacy password auth during
     the dual-mode window (Phase 2).';

COMMENT ON COLUMN auth.users.opaque_migrated_at IS
    'When this account first completed a successful OPAQUE login. Presence
     means the legacy POST /api/auth/login endpoint is refused for this user
     (Phase 3+). Once set, the admin-reset flow that goes back through legacy
     also nulls this column to re-open the fallback path.';

COMMENT ON COLUMN auth.users.force_password_change_at_next_login IS
    'When TRUE, the next successful login (legacy or OPAQUE) redirects the
     user to the change-password flow before any other action. Set by the
     admin-reset endpoint so admin-picked passwords are always temporary;
     cleared by change_password on success.';
