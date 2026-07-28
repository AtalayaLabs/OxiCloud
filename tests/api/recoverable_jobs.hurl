# =============================================================
# OxiCloud — Recoverable-run admin surface
# =============================================================
# Pins the Part 2 (recoverable-run engine) admin endpoints:
#   * POST /api/admin/jobs/{name}/trigger    (RecoverableJobHandler
#                                             path via RecoverableAdapter)
#   * POST /api/admin/jobs/{name}/cancel
#   * GET  /api/admin/jobs/{name}/runs
#   * GET  /api/admin/jobs/{name}/runs/{id}
#
# Uses `drives_consistency` — the first recoverable tenant, on-demand
# only. Verifies:
#   1. Registered job appears in `GET /api/admin/jobs` with no
#      interval (on-demand only).
#   2. Triggering creates a fresh row in `jobs.recoverable_runs`,
#      handler completes, run terminates as Completed.
#   3. History endpoint returns the just-completed run.
#   4. Single-run detail endpoint returns the same row.
#   5. Cancel-on-idle is a no-op with `cancelled: false` (nothing
#      running to cancel).
#   6. Unknown run id → 404 on the single-run endpoint.
#   7. Non-admin caller → 403 from the admin middleware on every
#      recoverable endpoint (no bespoke role check in the handlers).
#
# Drift-finding assertions land alongside the `jobs.run_findings`
# migration — the current build LOGS findings to
# `oxicloud::consistency` without persisting them. Log-tail
# assertions from Hurl are fragile so we defer them.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin login + rjobs_bob (non-admin) provisioning
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# Anti-enum registration.
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "rjobs_bob",
  "email":    "rjobs_bob@example.com",
  "password": "RjobsBobPassword1!"
}

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "rjobs_bob", "password": "RjobsBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 1 — `drives_consistency` is registered on-demand only.
#          Appears in the listing without an `interval_ms`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$[*].name" contains "drives_consistency"
# On-demand → no interval_ms (`skip_serializing_if = Option::is_none`).
jsonpath "$[?(@.name=='drives_consistency')].interval_ms" not exists


# ─────────────────────────────────────────────────────────────
# Step 2 — Trigger the check. Handler dispatches through
#          `RecoverableAdapter` → `run_or_resume`, which INSERTs
#          a fresh `jobs.recoverable_runs` row, runs the scan,
#          marks it Completed. Response envelope:
#            { ok, outcome: { outcome: "ok",
#                             extra: { completed: true, run_id: "..." } } }
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/drives_consistency/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.completed" == true
[Captures]
run_id: jsonpath "$.outcome.extra.run_id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Run history returns at least the just-triggered
#          run, newest first. Response is a JSON array of
#          RunSummary; the top entry must be the run_id we
#          captured above with status='Completed'.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs/drives_consistency/runs
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[0].id"        == "{{run_id}}"
jsonpath "$[0].job_name"  == "drives_consistency"
jsonpath "$[0].status"    == "Completed"


# ─────────────────────────────────────────────────────────────
# Step 4 — Single-run detail. Returns the same row shape as
#          the listing but for one id.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs/drives_consistency/runs/{{run_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.id"       == "{{run_id}}"
jsonpath "$.job_name" == "drives_consistency"
jsonpath "$.status"   == "Completed"
# scanned_count is bumped by the handler's checkpoint call — at
# least 0 (empty drives table) but ordinarily > 0 for any real
# fixture data. Present-ness of the field is what we pin.
jsonpath "$.stats.scanned_count" isNumber


# ─────────────────────────────────────────────────────────────
# Step 5 — Cancel-on-idle is a no-op. No Running row means no
#          Running→CancelRequested flip. Response is 200 with
#          `cancelled: false` (NOT a 404 — the job name is
#          registered, cancel just found nothing to cancel).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/drives_consistency/cancel
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.cancelled" == false
jsonpath "$.reason"    == "no running run for this job"


# ─────────────────────────────────────────────────────────────
# Step 6 — Unknown run id → 404. UUID shape is valid; the id
#          just isn't in `jobs.recoverable_runs`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs/drives_consistency/runs/00000000-0000-0000-0000-000000000000
Authorization: Bearer {{admin_token}}

HTTP 404
[Asserts]
jsonpath "$.error" == "run not found"


# ─────────────────────────────────────────────────────────────
# Step 7 — Non-admin caller is denied on every endpoint by
#          the `/api/admin/*` middleware layer. Handlers have
#          no bespoke role check — reaching them at all means
#          the caller is admin.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/drives_consistency/trigger
Authorization: Bearer {{bob_token}}

HTTP 403


POST {{base_url}}/api/admin/jobs/drives_consistency/cancel
Authorization: Bearer {{bob_token}}

HTTP 403


GET {{base_url}}/api/admin/jobs/drives_consistency/runs
Authorization: Bearer {{bob_token}}

HTTP 403


GET {{base_url}}/api/admin/jobs/drives_consistency/runs/{{run_id}}
Authorization: Bearer {{bob_token}}

HTTP 403
