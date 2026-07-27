# =============================================================
# OxiCloud — OPAQUE aPAKE (Phase 0 substrate) — inertness smoke
# =============================================================
# The full OPAQUE handshake is NOT testable in Hurl (every message
# contains session-random OPRF blinding + AKE nonces that can't be
# hardcoded in a .hurl body). Full-flow assertions belong in a Rust
# integration test using `opaque-ke` client-side against a real
# server. That lands with the Phase 1 endpoints.
#
# What THIS file asserts is the substrate-level contract for Phase 0:
#
#   1. The server booted with the OPAQUE substrate loaded — proved
#      transitively by the fact that this suite reached the
#      `--test-report` stage at all. `tests/common/server.env` sets
#      `OXICLOUD_OPAQUE_MODE=migrate` + a persisted `SERVER_SETUP`;
#      a boot failure (bad base64, missing setup, ciphersuite drift)
#      would 500 every request or refuse to bind the port.
#
#   2. The Phase 1 endpoints are not yet routed. An unauthenticated
#      POST to any `/api/*` path returns **401** (not 404) — the
#      `/api` namespace is behind the auth middleware, so a missing
#      route is indistinguishable from "route exists but needs
#      auth". That's deliberate anti-enumeration: attackers can't
#      probe which endpoints exist.
#
#      When Phase 1 ships:
#         - Register endpoints stay 401 unauth (they'll be
#           session-required — anti-enum still applies).
#         - Login KE1 / KE3 will flip to **400** because they'll be
#           public and reject the placeholder payloads below as
#           malformed. That's the natural regression signal: update
#           this file to hit the endpoints with a valid handshake
#           driven from a Rust integration test.
#
#   3. The legacy `POST /api/auth/login` continues to work under
#      Migrate mode. `auth_login.hurl` asserts this thoroughly; we
#      don't duplicate it here.
# =============================================================

# ─────────────────────────────────────────────────────────────
# Case 1 — Register-start endpoint not routed (401 anti-enum).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/opaque/register/start
Content-Type: application/json
{ "registrationRequest": "unused-phase-0" }

HTTP 401

# ─────────────────────────────────────────────────────────────
# Case 2 — Register-finish endpoint not routed (401 anti-enum).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/opaque/register/finish
Content-Type: application/json
{ "registrationRecord": "unused-phase-0", "ciphersuiteVersion": 1 }

HTTP 401

# ─────────────────────────────────────────────────────────────
# NOTE — The Phase 0 "login endpoints 401 anti-enum" cases were
# retired at Phase 1 landing. KE1 / KE3 are now routed (public,
# rate-limited). Cases 7 & 8 below assert the Phase 1 shape:
# KE1 400 `OpaqueMalformedRequest` on bad body, KE3 401
# `InvalidCredentials` on unknown exchange_id.
# ─────────────────────────────────────────────────────────────


# =============================================================
# Phase 1 — Register endpoints (authenticated wire coverage)
# =============================================================
# The register/{start,finish} endpoints are wired behind auth +
# CSRF middleware. Sending an authenticated request with an
# intentionally-malformed body proves:
#
#   1. Auth middleware unlocks the endpoint (401 → 400).
#   2. Bearer auth is CSRF-exempt (no 403 CSRF).
#   3. The handler is REACHABLE and its error-type contract
#      (`OpaqueMalformedRequest`, `OpaqueCiphersuiteMismatch`)
#      is stable.
#
# The FULL crypto handshake with real opaque-ke messages is
# proved separately in the Rust integration test at
# `src/infrastructure/repositories/pg/opaque_pg_repository.rs`
# (`envelope_persists_across_register_and_serves_a_matching_login`).
# That test drives the crypto pipeline end-to-end without HTTP —
# same crypto shape, same PG persistence path the handlers use.
# =============================================================

# ─────────────────────────────────────────────────────────────
# Log in as the seed admin and capture the bearer token. Bearer
# auth bypasses the CSRF check per the CSRF middleware doc, so
# subsequent OPAQUE POSTs don't need an X-CSRF-Token header.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
opaque_access_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Case 5 — Authenticated register/start with garbage base64
#           in `registrationRequest`. Handler reaches the
#           `B64.decode` path and returns 400 with the
#           `OpaqueMalformedRequest` error_type.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/opaque/register/start
Authorization: Bearer {{opaque_access_token}}
Content-Type: application/json
{ "registrationRequest": "not-valid-base64!" }

HTTP 400
[Asserts]
jsonpath "$.error_type" == "OpaqueMalformedRequest"


# ─────────────────────────────────────────────────────────────
# Case 6 — Authenticated register/finish with a ciphersuite
#           version the server does NOT accept. Proves the
#           ciphersuite-mismatch guard (server v1, client says
#           v999) is enforced BEFORE the envelope is decoded,
#           so a client cached against a rotated suite can't
#           silently write an unusable envelope.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/opaque/register/finish
Authorization: Bearer {{opaque_access_token}}
Content-Type: application/json
{ "registrationRecord": "AAAA", "ciphersuiteVersion": 999 }

HTTP 400
[Asserts]
jsonpath "$.error_type" == "OpaqueCiphersuiteMismatch"


# =============================================================
# Phase 1 — Login endpoints (KE1 / KE3, public + rate-limited)
# =============================================================
# The KE1 / KE3 endpoints are public — no session required, no
# CSRF (bearer/basic exempt anyway). Rate-limited by the same
# per-IP budget as legacy `/api/auth/login` so an attacker can't
# double their guessing rate by spraying both endpoints.
#
# The full crypto handshake (real opaque-ke bytes) is proved in
# the Rust integration test (in-process, no HTTP). What Hurl
# covers here is the wire wiring: routing exists, error paths
# fire with the stable error_type contract.

# ─────────────────────────────────────────────────────────────
# Case 7 — KE1 with garbage base64 → 400 OpaqueMalformedRequest.
#           Proves the endpoint is publicly reachable (no auth
#           required — no 401), the JSON body is parsed, and the
#           malformed-base64 error path is stable.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/opaque/login/ke1
Content-Type: application/json
{ "userIdentifier": "{{username}}", "startLoginRequest": "not-valid-base64!" }

HTTP 400
[Asserts]
jsonpath "$.error_type" == "OpaqueMalformedRequest"


# ─────────────────────────────────────────────────────────────
# Case 8 — KE3 with an unknown exchange_id → 401
#           InvalidCredentials. The `exchange_id` handle is
#           single-use and 60s-TTL; unknown ids MUST return the
#           SAME error shape as a wrong-passphrase failure so
#           attackers can't distinguish "id expired" from
#           "wrong password" from "id already consumed".
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/opaque/login/ke3
Content-Type: application/json
{ "exchangeId": "00000000-0000-0000-0000-000000000000", "finishLoginRequest": "AAAA" }

HTTP 401
[Asserts]
jsonpath "$.error_type" == "InvalidCredentials"
