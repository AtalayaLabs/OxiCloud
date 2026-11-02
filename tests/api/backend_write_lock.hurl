# =============================================================
# OxiCloud — Backend write-lock gate (External variant)
# =============================================================
# Pins the end-to-end contract introduced by
# `BackendWriteGate`: an operator engaging the lock refuses user
# writes AND makes backend-writer jobs defer their next tick
# until the lock is released.
#
# The design rationale lives on `BackendWriteLockReason` and
# `JobHandler::is_backend_writer`. This scenario is the operational
# proof that the two line up: a held lock → a backend-writer job
# triggered via the admin API records `deferred: true` with
# `deferred_reason: "backend_write_locked"` instead of running;
# releasing the lock → the next trigger of the same job proceeds
# normally.
#
# Why `backend_reclaim` as the test subject (not rechunk / rotate):
# it's on-demand, mutation is well-isolated (`storage.pending_actions`
# page-by-page), and a trigger with an empty queue completes within
# a few ms. Rotate and rechunk have external-entry preconditions that
# would wash out the one property under test.
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/backend_write_lock.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login
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


# ─────────────────────────────────────────────────────────────
# Step 2 — Baseline: the gate is NOT held.
#
# Guards against a prior scenario leaving the lock engaged. If
# this ever fails, something upstream didn't call the DELETE
# in its teardown.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/storage/write-lock
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.is_held" == false
jsonpath "$.holder" not exists


# ─────────────────────────────────────────────────────────────
# Step 3 — Baseline: triggering `backend_reclaim` with the gate
# free lets the job RUN (outcome.extra does NOT carry `deferred`).
#
# This is the "before" half of the pair — without it, a later
# "deferred when locked" assertion could pass trivially if the
# job always deferred.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/backend_reclaim/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# The job dispatch returned a `JobOutcome`; the `deferred` flag
# must NOT be present. (A normal run returns an Ok outcome whose
# extra may contain tenant-specific stats like `reclaimed` /
# `cancelled` / `parked`, but NOT the `deferred_reason` key.)
jsonpath "$.outcome.extra.deferred_reason" not exists


# ─────────────────────────────────────────────────────────────
# Step 3b — Baseline for the wire exposure: with the gate free,
# `/api/status` reports `readonly: false` and no holder. This is
# the "before" bookend for Step 4b-auth's "after" check — without
# it, step 4b-auth could pass trivially (holder present because
# the state has been held since boot by some leftover).
#
# Public `/api/config` carries NO `server_status` field at all,
# gate held or free — the privacy pin for that shape lives in
# `auth_config_discovery.hurl` (step 3) and isn't re-asserted on
# every state change here.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.readonly" == false
jsonpath "$.holder" not exists


# ─────────────────────────────────────────────────────────────
# Step 4 — Acquire the External lock.
#
# Short expiry — the hurl suite is fast and we don't want the
# lock lingering for 6 h if teardown is skipped. 300s = 5min
# gives plenty of margin for the remaining steps.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/storage/write-lock
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "label": "hurl: backend_write_lock scenario",
  "expires_in_seconds": 300
}

HTTP 200
[Asserts]
jsonpath "$.is_held" == true
jsonpath "$.holder.kind" == "external"
jsonpath "$.holder.label" == "hurl: backend_write_lock scenario"


# ─────────────────────────────────────────────────────────────
# Step 4b-public — Public wire exposure after the gate acquire:
# `/api/config` STILL carries no `server_status` field. The
# gate-held / gate-free distinction is invisible on this
# unauthenticated surface by design — the signal path for a
# visitor who tries to use a maintenance-mode server is the auth
# endpoint rejecting their login with a reason, not any
# information on `/api/config`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.server_status" not exists


# ─────────────────────────────────────────────────────────────
# Step 4b-auth — The authenticated `/api/status` body carries the
# full holder details that step 4b-public deliberately strips.
# This is the "logged-in caller actually sees the maintenance
# banner" half of the invariant — without this pin, step 4b-public
# alone would silently pass on a future refactor that stripped
# holder from EVERY endpoint (not just the public one).
#
# `/api/status` is the authenticated runtime snapshot added in
# the feat/registration privacy split; shape = full HeaderPayload
# including `holder.kind` + `holder.display`. The `display`
# string is server-formatted by `BackendWriteLockReason::display()`
# and INCLUDES the operator-authored label on External holds, so
# the exact "External maintenance in progress" value here proves
# end-to-end that this scenario's acquire with
# `label: "hurl: backend_write_lock scenario"` reached the FE wire
# path. (The admin-only `/api/admin/storage/write-lock` surface in
# step 4c carries the raw label too; this one carries the
# server-formatted display.)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.readonly" == true
jsonpath "$.holder.kind" == "external"
jsonpath "$.holder.display" == "External maintenance in progress"


# ─────────────────────────────────────────────────────────────
# Step 4c — The `X-Server-Status` header surface, which carries what
# the public body deliberately does not: the FULL `display()` including
# the operator label.
#
# `/api/admin/*` is nested inside `create_api_routes`, so unlike
# `/api/config` and `/api/auth/*` it DOES pass through the middleware.
# This is the end-to-end proof the original step 4b was reaching for
# when it asserted the label on `/api/config` — same intent, correct
# surface. It is also the suite's only coverage of the header itself.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/storage/write-lock
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
header "x-server-status" contains "\"readonly\":true"
header "x-server-status" contains "\"kind\":\"external\""
# The operator label reached the FE wire path — only observable here.
header "x-server-status" contains "hurl: backend_write_lock scenario"
# And the admin body still carries the raw typed holder.
jsonpath "$.is_held" == true
jsonpath "$.holder.label" == "hurl: backend_write_lock scenario"


# ─────────────────────────────────────────────────────────────
# Step 5 — While the lock is held, trigger `backend_reclaim`.
# The scheduler prologue MUST defer the tick instead of running
# the handler, and record the deferral on the returned outcome.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/backend_reclaim/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.extra.deferred" == true
jsonpath "$.outcome.extra.deferred_reason" == "backend_write_locked"
jsonpath "$.outcome.extra.deferred_holder" == "external"


# ─────────────────────────────────────────────────────────────
# Step 6 — Double-acquire is refused with 409 and the response
# names the current holder. Prevents an operator-triggered lock
# from silently overwriting a running migration's hold.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/storage/write-lock
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "label": "hurl: should fail — gate already held"
}

HTTP 409


# ─────────────────────────────────────────────────────────────
# Step 7 — Release.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/storage/write-lock
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.is_held" == false


# ─────────────────────────────────────────────────────────────
# Step 7b — Wire exposure after release: `/api/status` reports
# `readonly: false` and no holder again — the matched "after"
# bookend for step 4b-auth's "held" assertion. Without this pin
# a regression that forgot to clear the holder on release would
# leave step 4b-auth's assertion passing on a stuck-high state.
#
# Public `/api/config` carries no `server_status` field in any
# state; the privacy pin lives in `auth_config_discovery.hurl`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.readonly" == false
jsonpath "$.holder" not exists


# ─────────────────────────────────────────────────────────────
# Step 8 — Post-release: `backend_reclaim` runs again, no defer.
# This is the "after" half of the pair proving the gate is the
# only thing that caused the deferral in step 5.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/backend_reclaim/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.extra.deferred_reason" not exists


# ─────────────────────────────────────────────────────────────
# Step 9 — Teardown-redundancy: a second DELETE on a free gate
# is a no-op 200. Idempotent so sibling scenarios don't fail if
# they also release at the end.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/storage/write-lock
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.is_held" == false
