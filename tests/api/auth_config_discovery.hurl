# =============================================================
# OxiCloud – `/api/config` + `/api/status` wire-shape pins
# =============================================================
# Pins the public `/api/config` shape (auth-method matrix,
# `initialized` — pure capability discovery, no runtime state)
# and the authenticated `/api/status` surface. Both are products
# of the `feat/registration` consolidation that retired three
# unauthenticated bootstrap endpoints — `/api/auth/oidc/providers`,
# `/api/auth/status`, and the entire `/api/config.server_status`
# block — in favour of a single pre-auth `/api/config` call plus
# one auth-only `/api/status` for every field that leaks either
# infrastructure topology (migration/rotation/holder) or
# operator-authored content (banner bodies).
#
# Shape (sub-objects per auth concern, cross-cutting policy at
# the top of `auth`):
#   auth: {
#     min_password_length: int,
#     opaque_mode: string,
#     require_verified_email: bool,
#     registration: { enabled: bool },
#     password:     { enabled: bool },
#     magic_link:   { enabled: bool },
#     oidc: {
#       enabled: bool,
#       auto_redirect: bool,
#       providers: [
#         { issuer: string, provider_name: string,
#           authorize_endpoint: string }
#       ]
#     }
#   }
#
# A regression that drops any sub-object OR collapses one back
# to a flat scalar (`auth.registration_enabled` instead of
# `auth.registration.enabled`) breaks every consumer. Pins the
# shape here so a `cargo run --bin generate-openapi` that forgets
# to re-register a schema OR a handler refactor that flattens
# the DTO can't ship silently.
#
# Run:
#   hurl --variables-file tests/api/hurl.vars \
#        --test tests/api/auth_config_discovery.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Every sub-object MUST be present with the right shape.
#
# `exists` + the appropriate type assertion is the load-bearing
# pin on each field. Default values are instance-specific; the
# existence + type pin catches 99% of regressions without
# coupling to env settings.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.auth" exists

# Top-level `initialized` — ported from the retired
# `/api/auth/status.initialized`. The login page gates its
# setup-wizard vs login-form fork on this; a regression that
# drops the field would push every visitor into the setup
# wizard on a configured deployment (the FE fallback defaults
# to `true` but only if the field is MISSING — a `false` on
# the wire would be respected).
jsonpath "$.initialized" exists
jsonpath "$.initialized" isBoolean
jsonpath "$.initialized" == true

# Top-level auth scalars — kept flat because they're cross-
# cutting or truly one-value policy knobs.
jsonpath "$.auth.min_password_length" exists
jsonpath "$.auth.min_password_length" isInteger
jsonpath "$.auth.opaque_mode" exists
jsonpath "$.auth.opaque_mode" isString
jsonpath "$.auth.require_verified_email" exists
jsonpath "$.auth.require_verified_email" isBoolean

# Registration sub-object. Default-true on a fresh test instance
# (no env override, no admin flip) — the FE gates the "Create
# one" button on this.
jsonpath "$.auth.registration" exists
jsonpath "$.auth.registration.enabled" exists
jsonpath "$.auth.registration.enabled" isBoolean
jsonpath "$.auth.registration.enabled" == true

# Password + magic-link sub-objects. The login page renders a
# form per enabled method; a stripped sub-object collapses the
# gate to `undefined` → form hidden on a server that explicitly
# enabled the method (the bug this pin would catch).
jsonpath "$.auth.password" exists
jsonpath "$.auth.password.enabled" exists
jsonpath "$.auth.password.enabled" isBoolean

jsonpath "$.auth.magic_link" exists
jsonpath "$.auth.magic_link.enabled" exists
jsonpath "$.auth.magic_link.enabled" isBoolean

# OIDC sub-object: enablement + auto-redirect policy + provider
# list. The array shape is forward-compatible for the planned
# multi-provider deployments (one deployment federating multiple
# IdPs) — today it's always 0 or 1 elements, but the FE loops
# over it either way.
jsonpath "$.auth.oidc" exists
jsonpath "$.auth.oidc.enabled" exists
jsonpath "$.auth.oidc.enabled" isBoolean
jsonpath "$.auth.oidc.auto_redirect" exists
jsonpath "$.auth.oidc.auto_redirect" isBoolean
jsonpath "$.auth.oidc.providers" exists
jsonpath "$.auth.oidc.providers" isCollection


# ─────────────────────────────────────────────────────────────
# Step 2 — The registration flag is server-controlled, not
# query-overridable.
#
# A regression where a request-side param could flip the flag
# would let any visitor force themselves back into the register
# flow on a disabled instance — the backend `/api/auth/register`
# gate would still refuse the POST, but the UI would offer the
# form that the operator deliberately hid. Mirror of the
# shared_worker_bus_discovery.hurl guard; cheap insurance against
# a future "configurable from the URL" refactor.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config?registration=false

HTTP 200
[Asserts]
jsonpath "$.auth.registration.enabled" == true


# ─────────────────────────────────────────────────────────────
# Step 3 — `/api/config` MUST NOT carry `server_status` at all.
#
# The entire runtime-status surface (readonly + migration +
# rotation + holder + banners) is authenticated-only — it lives
# on `GET /api/status` and the `X-Server-Status` response header
# stamped by `server_status_middleware` on every authenticated
# response. Admin-authored banner bodies carry whatever text the
# operator wrote, so even the "banners" field was a leak on an
# unauthenticated endpoint; dropping the whole `server_status`
# block closes every axis in one cut.
#
# A regression that re-adds the field (even just `readonly`) would
# surface here. The signal path for anonymous visitors hitting a
# maintenance-mode server is the auth endpoint rejecting their
# login with a reason, not a pre-login banner.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.server_status" not exists


# ─────────────────────────────────────────────────────────────
# Step 4 — Two retired paths return 401 at the public probe.
#
# `/api/auth/oidc/providers` — provider list + method matrix
#   moved to `/api/config.auth.*`.
# `/api/auth/status` — `initialized` moved to `/api/config`;
#   the other three fields the DTO carried (`admin_count`,
#   `registration_allowed`, `faces_enabled`) were redundant
#   with `/api/config` or the admin panel.
#
# A regression that silently re-registers either old handler
# would bring back the 200 JSON body — the pin catches it.
# Expected status is 401 (not 404) because the global auth
# middleware guards the whole `/api/auth/*` namespace and fires
# before Axum reaches the fallback for an unregistered path.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/providers

HTTP 401


GET {{base_url}}/api/auth/status

HTTP 401


# ─────────────────────────────────────────────────────────────
# Step 5 — `/api/status` is the AUTHENTICATED runtime snapshot.
#
# Carries the full `HeaderPayload` (readonly + migration +
# rotation + holder + banners_version + banners). An anonymous
# probe MUST hit 401 — the whole point of this endpoint is to
# gate infrastructure topology behind auth. A regression that
# accidentally registered it on the public router would respond
# 200 with the full body, defeating the privacy split.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status

HTTP 401


# ─────────────────────────────────────────────────────────────
# Step 6 — `X-Server-Status` header presence invariant, PUBLIC side.
#
# `/api/config` is on the PUBLIC router and sits OUTSIDE the
# `create_api_routes` layer that stamps `X-Server-Status`.
# The FE's `updateFromHeader` treats header ABSENCE as the signal
# "this response went around the status middleware" — a precondition
# for `apiFetch` not clobbering the live-status store on an
# unrelated public-router response (banner-wipe bug fixed on
# 2025-11, see `middleware/server_status.rs::server_status_middleware`
# comment).
#
# Pin: no header on `/api/config`. A regression that layered the
# middleware onto the public router would break the FE invariant
# silently; this pin catches it.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
header "x-server-status" not exists


# ─────────────────────────────────────────────────────────────
# Step 7 — `X-Server-Status` header presence invariant,
# AUTHENTICATED side.
#
# Every response that goes through `create_api_routes` MUST
# carry the header — even when nothing is happening server-side
# (payload is `{"readonly":false}`, ~20 bytes). The "always
# stamp" rule replaced an earlier "early-return on empty state"
# optimization that caused the banner-wipe bug: a middleware
# response with nothing-happening looked identical to an auth
# response that never saw the middleware. The invariant is
# "header present ⇔ this response went through the status
# middleware", and this pin locks in the "even on idle state"
# half.
#
# `/api/status` is a natural authenticated probe target —
# protected router, no side-effects. A regression re-introducing
# the early-return would surface here immediately.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
header "x-server-status" exists
header "x-server-status" contains "readonly"
