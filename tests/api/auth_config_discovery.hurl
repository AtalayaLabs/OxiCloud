# =============================================================
# OxiCloud – `/api/config.auth.*` discovery wire-shape pins
# =============================================================
# Pins the complete auth-method matrix the login page gates its
# forms + buttons on. This endpoint is the ONLY surface — the
# pre-consolidation `/api/auth/oidc/providers` was retired on
# feat/registration, so every FE caller (login, layout, upgrade,
# profile, nextcloud/login) reads from `/api/config.auth.*`.
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

# Top-level scalars — kept flat because they're cross-cutting
# or truly one-value policy knobs.
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
# Step 3 — `/api/auth/oidc/providers` is RETIRED.
#
# The provider list + method matrix moved to `/api/config.auth.*`.
# A regression that silently re-registers the old handler (because
# someone resurrected it without noticing the retirement) would
# bring back the 200 JSON body — the FE's dead `getOidcProviders`
# wrapper won't catch it on its own, but this pin will.
#
# Expected status is 401 (not 404) because the global auth
# middleware guards the whole `/api/auth/*` namespace and fires
# before Axum reaches the fallback for the unregistered path.
# That's intentional — admins specifically do not want to leak
# "endpoint missing" vs "endpoint protected" to anonymous probes.
# A re-registered handler would respond 200, which the pin
# catches either way. A future middleware refactor that changed
# the anonymous-probe response to 404 would surface here first.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/providers

HTTP 401
