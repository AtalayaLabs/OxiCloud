# =============================================================
# OxiCloud – `registration.enabled` discovery via /api/config
# =============================================================
# Pins the public shape the login page gates its "Create one"
# button on. The server already enforces registration on-off at
# `POST /api/auth/register`
# (`admin_settings_service::get_registration_enabled`:
#  env `OXICLOUD_DISABLE_REGISTRATION` > DB setting >
#  default-true), but without a /api/config field the login UI
# can't ASK — a user on a disabled instance would still see the
# register link, click it, fill the form, and only hit the 403
# after submit. The field here is what lets the FE hide the
# link up front.
#
# Shape is `auth.registration: { enabled: bool }` — a sub-object
# so future registration-side knobs (verified-email requirement,
# per-domain allowlist mirror) land as siblings without widening
# `AuthDto` itself. A regression that drops the sub-object OR
# collapses it back to a flat `auth.registration_enabled` would
# break the FE path that reads `serverConfig.auth.registration.
# enabled`.
#
# Run:
#   hurl --variables-file tests/api/hurl.vars \
#        --test tests/api/registration_discovery.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — /api/config MUST carry `auth.registration.enabled`.
#
# `exists` + `isBoolean` are the load-bearing pins; a missing
# field would be read as `undefined` by the FE and the register
# link would stay visible on a disabled instance. The value
# itself defaults to `true` on this test instance (no env
# override, no DB flip) and is the only knob the login page
# cares about — any new sibling on the registration sub-object
# lands as an additional assertion here, not a replacement.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.auth" exists
jsonpath "$.auth.registration" exists
jsonpath "$.auth.registration.enabled" exists
jsonpath "$.auth.registration.enabled" isBoolean
# Default on this test instance is `true` — pinning the value
# protects against an "obvious" default-flip refactor and
# documents what a fresh deployment sees. A test instance that
# sets `OXICLOUD_DISABLE_REGISTRATION=true` belongs in a
# dedicated hurl file (not here) so the two cases don't
# compete over the same wire snapshot.
jsonpath "$.auth.registration.enabled" == true

# Sibling fields of `auth.registration` that the FE already
# reads must still be present — the login page's $derived
# `passwordLoginEnabled` consults `getOidcProviders()` today
# but `min_password_length` + `opaque_mode` on /api/config
# are what the register and change-password forms gate submit
# on, so regressing them in the same payload would mask real
# damage behind this one new field.
jsonpath "$.auth.min_password_length" exists
jsonpath "$.auth.min_password_length" isInteger
jsonpath "$.auth.opaque_mode" exists
jsonpath "$.auth.opaque_mode" isString


# ─────────────────────────────────────────────────────────────
# Step 2 — The field is server-controlled, not query-overridable.
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
