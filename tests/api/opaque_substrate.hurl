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
# Case 3 — Login KE1 endpoint not routed (401 anti-enum).
#           Will flip to 400 in Phase 1 (public + malformed body).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/opaque/login/ke1
Content-Type: application/json
{ "userIdentifier": "{{username}}", "startLoginRequest": "unused-phase-0" }

HTTP 401

# ─────────────────────────────────────────────────────────────
# Case 4 — Login KE3 endpoint not routed (401 anti-enum).
#           Will flip to 400 in Phase 1 (public + malformed body).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/opaque/login/ke3
Content-Type: application/json
{ "exchangeId": "unused-phase-0", "finishLoginRequest": "unused-phase-0" }

HTTP 401
