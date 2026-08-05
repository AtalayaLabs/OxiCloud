-- ════════════════════════════════════════════════════════════════════════════
-- Per-envelope OPAQUE KSF parameters
-- ════════════════════════════════════════════════════════════════════════════
-- Client-side Argon2id key-stretching parameters (memory / iterations / lanes)
-- used at OPAQUE register time, stored per-envelope. Complements the three
-- existing OPAQUE columns (envelope, ciphersuite_version, registered_at)
-- introduced by 20261001000002_auth_opaque.sql.
--
-- ## Why per-envelope
--
-- OPAQUE's KSF runs entirely client-side. The server publishes the CURRENT
-- default via GET /api/auth/opaque/params, and the client feeds those values
-- into `argon2::Argon2` before the OPRF blinding step. The resulting
-- stretched password becomes part of the input the AKE integrity-checks
-- against the envelope. **If the client ever uses a different KSF than the
-- one that was used at register time, the AKE fails and the server returns
-- InvalidCredentials.**
--
-- Consequence: without per-envelope storage, changing
-- OXICLOUD_AUTH_OPAQUE_KSF_* invalidates every existing envelope. Users hit
-- a mass "invalid credentials" lockout and the only recovery is
-- `oxicloud-cli opaque reset` to force re-registration.
--
-- With per-envelope storage: `/api/auth/opaque/login/lookup` returns the
-- envelope's own KSF params alongside hasOpaque=true. The client uses those
-- for the login handshake (matching what was used at register), and NEW
-- registrations pick up the current server defaults. Config changes stop
-- being a lockout vector.
--
-- ## Migration handling for existing envelopes
--
-- Columns are nullable — existing envelopes minted before this migration
-- carry NULL and the client falls back to the server's published /params
-- values for the login handshake. That preserves the pre-migration
-- behaviour (works as long as the server's current config matches what
-- was in effect at register time). After a user re-registers (silent
-- migration on next password change, or explicit CLI reset + re-login),
-- the columns get populated with the client's declared values and future
-- config changes stop affecting that envelope.
--
-- We DO NOT backfill with a hardcoded historical default here — different
-- OxiCloud deployments have carried different defaults over the branch's
-- life, and picking one would be wrong for the others. NULL + fallback to
-- current /params is the safest posture.
--
-- ## Columns
--
--   opaque_ksf_memory_kib   INTEGER   — client-side Argon2id memory cost
--                                       in KiB, as CLIENT-DECLARED at
--                                       register/finish time.
--   opaque_ksf_iterations   INTEGER   — client-side iteration count.
--   opaque_ksf_parallelism  INTEGER   — client-side parallelism lanes.
--
-- All three move as one atomic set (populated together at register_finish,
-- nulled together at clear_registration). Sibling to `opaque_ciphersuite_version`
-- but distinct: ciphersuite version is a schema/algorithm identifier
-- (Ristretto255-SHA512-3DH-Argon2id v1); KSF params tune the Argon2id
-- cost within that ciphersuite.

ALTER TABLE auth.users
    ADD COLUMN opaque_ksf_memory_kib INTEGER,
    ADD COLUMN opaque_ksf_iterations INTEGER,
    ADD COLUMN opaque_ksf_parallelism INTEGER;

COMMENT ON COLUMN auth.users.opaque_ksf_memory_kib IS
    'Argon2id memory cost (KiB) the CLIENT used at OPAQUE register time.
     Baked in per-envelope so future changes to OXICLOUD_AUTH_OPAQUE_KSF_*
     do not invalidate existing envelopes — the /login/lookup endpoint
     returns this value alongside hasOpaque=true, and the client uses it
     on the login handshake. NULL for envelopes minted before per-envelope
     storage landed; those envelopes still work as long as the server''s
     current /params matches what they were registered under.';

COMMENT ON COLUMN auth.users.opaque_ksf_iterations IS
    'Argon2id iteration count the CLIENT used at OPAQUE register time.
     See opaque_ksf_memory_kib for the per-envelope-storage rationale.';

COMMENT ON COLUMN auth.users.opaque_ksf_parallelism IS
    'Argon2id parallelism (lanes) the CLIENT used at OPAQUE register time.
     See opaque_ksf_memory_kib for the per-envelope-storage rationale.';
