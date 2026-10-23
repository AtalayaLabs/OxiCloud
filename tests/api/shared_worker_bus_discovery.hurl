# =============================================================
# OxiCloud – `shared_worker_bus` discovery via /api/config
# =============================================================
# Pins the Gate-3 server-flag wire shape so a careless edit to
# `FeaturesDto` can't silently drop the field. The FE factory
# gates on `features.shared_worker_bus` before attempting the
# SharedWorker handshake; a missing field would default FE
# behaviour to on anyway (per the opt-out semantics), BUT would
# silently defeat an operator who set
# `OXICLOUD_ENABLE_SHARED_WORKER_BUS=false` — their users would
# still land on shared mode. That failure would be invisible
# until a user reported it, so pin the shape here.
#
# Run:
#   hurl --variables-file tests/api/hurl.vars \
#        --test tests/api/shared_worker_bus_discovery.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — /api/config MUST include `features.shared_worker_bus`.
#
# Default is `true` (opt-out). The field must be present AND
# boolean — a missing field would be read as `undefined` by the
# FE and default-engage shared mode, but an operator who opted
# out explicitly would have no way to confirm their setting took
# effect without this field.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.features.shared_worker_bus" exists
jsonpath "$.features.shared_worker_bus" isBoolean
# Default on this test instance is `true` — assertions on a
# specific value are OK here because `hurl.vars` points at a
# baseline config without the opt-out override. A test instance
# that flipped the env would need to adjust this (and documenting
# that case belongs in a dedicated hurl file keyed off a
# different `OXICLOUD_ENABLE_SHARED_WORKER_BUS` env value).
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
# would be an authorization bypass for operators who deliberately
# disabled shared mode (anyone with a browser could re-enable it
# for themselves). This isn't a security hole today (no such
# param exists), but pinning the shape prevents an "obvious"
# future refactor from silently introducing one.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config?shared_worker_bus=false

HTTP 200
[Asserts]
jsonpath "$.features.shared_worker_bus" == true
