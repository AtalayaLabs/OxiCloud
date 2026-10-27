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
# `/api/config`'s body carries `server_status.readonly: false` and no
# holder. This is the "before" bookend for Step 4b's "after" check —
# without it, step 4b could pass trivially.
#
# (This comment used to describe an "X-Server-Status hot-path
# early-return (readonly:false + no rotation → no header)". That rule
# was removed from `server_status_middleware`: it now stamps on every
# response inside its layer, because the FE needs header absence to mean
# "outside the middleware stack" rather than "nothing happening". A
# stale `hot-path returns without a header` comment still sits at the
# top of that middleware and is what this test was written against.)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.server_status.readonly" == false
jsonpath "$.server_status.holder" not exists


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
# Step 4b — Public wire exposure: with the gate held, `/api/config`
# surfaces the holder in its body, and every response inside the
# `create_api_routes` layer additionally carries the
# `X-Server-Status` header (not "EVERY API response" as this comment
# previously claimed — the public and `/api/auth/*` trees sit outside
# that layer by design). Both consumed by the FE's
# global server-status store to render the maintenance banner
# across the whole app — not just the admin tab. Public-safe
# projection: `kind` + `display` only (admin_id / expires_at /
# raw label text / source-target names stay on the admin-only
# `/api/admin/storage/write-lock` surface).
#
# The `display` field is server-formatted by
# `BackendWriteLockReason::display()` and includes the operator
# label on External holds, so `contains "hurl: backend_write_lock"`
# proves end-to-end that this scenario's acquire reached the FE
# wire path.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
# No header assertion HERE: `/api/config` is on the PUBLIC router and
# `server_status_middleware` is layered inside `create_api_routes`, so
# this response never passes through it. That is deliberate — the
# middleware relies on header ABSENCE meaning "response path outside the
# middleware stack". The header is asserted in step 4c below, on an
# in-layer endpoint.
jsonpath "$.server_status.readonly" == true
jsonpath "$.server_status.holder.kind" == "external"
# The PUBLIC projection strips the operator label: `build_header_payload`
# calls `reason.public_display()`, which for External always returns the
# generic string, while the header path calls `reason.display()` and
# keeps "Maintenance: {label}". This assertion asked for the label here
# and got "External maintenance in progress" the first time the file ran
# (2026-10-24) — the privacy control working as designed. Pinning the
# generic form is the useful assertion: it fails if the label ever starts
# leaking to anonymous callers.
jsonpath "$.server_status.holder.display" == "External maintenance in progress"


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
# Step 7b — Public wire exposure after release: the `/api/config`
# body reports `server_status.readonly: false` + no holder, and
# the global banner on every open client disappears. (The parenthetical
# here used to say the `X-Server-Status` header is "omitted on the
# hot-path early-return when readonly is false AND rotation is
# inactive". That early-return no longer exists — inside its layer the
# middleware now stamps `{"readonly":false}` unconditionally, and it is
# that explicit false, not the header's absence, that resets the FE
# store.)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.server_status.readonly" == false
jsonpath "$.server_status.holder" not exists


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
