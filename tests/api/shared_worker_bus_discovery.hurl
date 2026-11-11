# =============================================================
# OxiCloud – `shared_worker_bus` discovery via /api/config
# =============================================================
# Pins the Gate-3 server-flag wire shape so a careless edit to
# `FeaturesDto` can't silently drop the field. The FE factory
# gates on `features.shared_worker_bus === true` before attempting
# the SharedWorker handshake, so a missing field reads as
# `undefined` and pins every user to per-tab mode — which is now a
# REGRESSION rather than the default it once was: the flag ships
# enabled (opt-out) since the observation window closed. The
# failure would be invisible on the wire and show up only as the
# admin dashboard reporting one WebSocket per tab instead of one
# per browser, so pin the shape here.
#
# Run:
#   hurl --variables-file tests/api/hurl.vars \
#        --test tests/api/shared_worker_bus_discovery.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — /api/config MUST include `features.shared_worker_bus`.
#
# Default is `true` (opt-out). The field must be present AND
# boolean — a missing field reads as `undefined` on the FE and
# lands on per-tab, i.e. exactly what a dropped field and a
# deliberate opt-out look like from the client. Only the field's
# presence distinguishes "the operator chose per-tab" from "the
# server stopped telling us".
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.features.shared_worker_bus" exists
jsonpath "$.features.shared_worker_bus" isBoolean
# The harness runs a baseline config with no override, so this
# asserts the compiled-in default itself — which is the point: it
# is the only test that fails if someone flips
# `FeaturesConfig::enable_shared_worker_bus` back to `false`
# without meaning to. An instance that sets the env explicitly
# would need its own scenario keyed off that value.
jsonpath "$.features.shared_worker_bus" == true

# The sibling flag `message_bus` must also still be present —
# the SharedWorker is useless without it, and regressing one
# without the other would leave the FE in a wedge state
# (handshake succeeds, no WS endpoint exists).
jsonpath "$.features.message_bus" exists
jsonpath "$.features.message_bus" isBoolean


# ─────────────────────────────────────────────────────────────
# Step 2 — The field is server-controlled, not query-overridable.
#
# A regression where a request-side param could flip the flag
# would let any user override the operator's choice for their own
# tab — and the direction that matters flipped with the default.
# The probe therefore asks the server to turn the feature OFF: on
# an opt-out default, `?shared_worker_bus=true` would return
# `true` whether or not the param was honoured, so it proves
# nothing. Asking for `false` and still getting `true` is what
# shows the flag is server-controlled.
#
# Not a security hole today (no such param exists), but pinning
# the shape stops an "obvious" future refactor introducing one.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config?shared_worker_bus=false

HTTP 200
[Asserts]
jsonpath "$.features.shared_worker_bus" == true
