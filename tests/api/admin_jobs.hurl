# =============================================================
# OxiCloud — JobRegistry admin surface
# =============================================================
# Pins `GET /api/admin/jobs` and `POST /api/admin/jobs/{name}/trigger`
# — the production admin surface for the periodic-job scheduler
# (`docs/plan/job-registry.md` Part 1).
#
# Coverage:
#   * Listing returns the four registered tenants
#     (trash_cleanup, storage_reconcile, dedup_gc, grant_cleanup).
#   * Scheduled jobs report `interval_ms`; on-demand jobs
#     (`dedup_gc`) omit it via `skip_serializing_if=None`.
#   * Triggering a job updates its `last_outcome` in the next list.
#   * Unknown job → 404 (anti-enum on the trigger URL).
#   * Non-admin caller → 403 from the admin middleware layer
#     (no bespoke check in `list_jobs`/`trigger_job` itself).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin login + jobs_bob (re-)provisioning
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
  "username": "jobs_bob",
  "email":    "jobs_bob@example.com",
  "password": "JobsBobPassword1!"
}

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "jobs_bob", "password": "JobsBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin lists jobs. All four Part 1 tenants must appear.
# Scheduled jobs (trash_cleanup, storage_reconcile, grant_cleanup)
# report `interval_ms`; on-demand jobs (`dedup_gc`) omit it via
# serde's `skip_serializing_if = "Option::is_none"`.
#
# JSONPath idiom (per `feedback_hurl_jsonpath_filter_empty`): the
# single-match `[?(...)]` filter unwraps to a scalar so `count`
# fails; `count == 0` also fails on the zero-match case ("no
# value"). Aggregate `$..field` + `contains` is the reliable
# primitive Ed's memory endorses.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# Names — the four scheduler tenants.
jsonpath "$[*].name" contains "trash_cleanup"
jsonpath "$[*].name" contains "storage_reconcile"
jsonpath "$[*].name" contains "dedup_gc"
jsonpath "$[*].name" contains "grant_cleanup"

# Scheduled jobs' interval_ms values, in whatever order:
#   TrashCleanup    → 24 h = 86_400_000 ms
#   GrantCleanup    → 24 h = 86_400_000 ms
#   StorageReconcile → 600 s = 600_000 ms
# Recursive descent collects all interval_ms values across the
# array; `contains` doesn't care about order.
jsonpath "$..interval_ms" contains 86400000
jsonpath "$..interval_ms" contains 600000

# On-demand job (`dedup_gc`) has no interval_ms field, so the
# total count of interval_ms values is 3, not 4. Combined with
# the four-name check above, this pins the on-demand-omission
# behaviour without hitting the single-match filter trap.
jsonpath "$..interval_ms" count == 3

# Every entry carries a `running` bool — same aggregate primitive.
# Count matches the registered-tenant count: 4 Part 1 periodics
# (trash_cleanup, storage_reconcile, dedup_gc, grant_cleanup) + 5
# Part 2 recoverables (drives_consistency, folders_consistency,
# files_consistency, blobs_consistency, backend_consistency —
# wrapped by RecoverableAdapter so they appear here alongside the
# periodics) + 1 coordinator (consistency_batch — a plain
# JobHandler that dispatches every registered `*_consistency`).
# Bump when a new tenant registers.
jsonpath "$..running" count == 10
jsonpath "$[*].name" contains "drives_consistency"
jsonpath "$[*].name" contains "folders_consistency"
jsonpath "$[*].name" contains "files_consistency"
jsonpath "$[*].name" contains "consistency_batch"


# ─────────────────────────────────────────────────────────────
# Step 3 — Trigger `trash_cleanup`. Envelope shape:
#          `{ ok, outcome: { outcome, count, extra } }`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/trash_cleanup/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
# Trash may or may not have expired items; count is a non-negative
# integer either way. `isNumber` on a 1-element JSONPath extract
# needs the filter idiom too — assert exists via the wrapper key.
jsonpath "$.outcome.count" exists
jsonpath "$.outcome.extra.files_purged"   exists
jsonpath "$.outcome.extra.folders_purged" exists


# ─────────────────────────────────────────────────────────────
# Step 4 — Re-list. At least one job (`trash_cleanup`, just
#          triggered above) now has a `last_outcome` populated
#          with `outcome=ok`. Aggregate JSONPath — recursive
#          descent collects every last_outcome.outcome value
#          across the response.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$..last_outcome.outcome" contains "ok"


# ─────────────────────────────────────────────────────────────
# Step 4b — Trigger `folders_consistency`. Second Part 2
#           recoverable tenant. Goes through the same
#           RecoverableAdapter → PgJobStoreProvider path as
#           drives_consistency (opens a run row, walks the
#           `storage.folders` cursor, marks Completed). Success
#           envelope shape identical.
#
#           Captures the run_id so Step 4b-findings can pin the
#           `GET /runs/{id}/findings` endpoint.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/folders_consistency/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
folders_run_id: jsonpath "$.outcome.extra.run_id"
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count" exists
jsonpath "$.outcome.extra.completed" == true
jsonpath "$.outcome.extra.run_id" exists


# ─────────────────────────────────────────────────────────────
# Step 4b-findings — List findings for the run we just kicked.
# The response body is a JSON array (possibly empty on a clean
# test DB — the fresh Hurl DB has no folder-tree drift). Assert:
#   * 200 on a real run_id.
#   * response is an array (isCollection covers both empty + non-
#     empty). Structural shape of individual finding rows is pinned
#     by the drives_consistency_service integration test which seeds
#     drift and asserts kind/severity/detail — not repeated here.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs/folders_consistency/runs/{{folders_run_id}}/findings
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection


# ─────────────────────────────────────────────────────────────
# Step 4b-findings-404 — Findings for a run_id that doesn't
# exist. Endpoint returns 404, not 200 [] — otherwise a broken
# link from the admin UI would look like "no findings" instead
# of "run missing".
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs/folders_consistency/runs/00000000-0000-0000-0000-000000000000/findings
Authorization: Bearer {{admin_token}}

HTTP 404
[Asserts]
jsonpath "$.error" == "run not found"


# ─────────────────────────────────────────────────────────────
# Step 4b2 — Trigger `files_consistency`. Third Part 2 recoverable
#            tenant. Iterates `storage.files` and self-joins folder
#            + blob. Same envelope shape as the earlier consistency
#            tenants.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/files_consistency/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count" exists


# ─────────────────────────────────────────────────────────────
# Step 4c — Trigger `consistency_batch`. Coordinator (plain
#           JobHandler) — snapshots the registry, filters names
#           ending `_consistency`, sequentially triggers each.
#           `outcome.count` = number of children dispatched (5 as
#           of Slice 10: drives + folders + files + blobs +
#           backend). `extra.per_check` carries a per-child outcome
#           map. Batch itself always returns ok — child failures
#           live inside per_check. `?deep=true` propagates as
#           `extra.deep`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/consistency_batch/trigger?deep=true
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count" == 5
jsonpath "$.outcome.extra.deep" == true
jsonpath "$.outcome.extra.ok" == 5
jsonpath "$.outcome.extra.err" == 0
# per_check is keyed by child job name.
jsonpath "$.outcome.extra.per_check.drives_consistency.outcome"  == "ok"
jsonpath "$.outcome.extra.per_check.folders_consistency.outcome" == "ok"
jsonpath "$.outcome.extra.per_check.files_consistency.outcome"   == "ok"
jsonpath "$.outcome.extra.per_check.blobs_consistency.outcome"   == "ok"
jsonpath "$.outcome.extra.per_check.backend_consistency.outcome" == "ok"


# ─────────────────────────────────────────────────────────────
# Step 5 — Trigger a job that doesn't exist. 404 anti-enum on
#          `JobRegistry::trigger` returning `None`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/no_such_job/trigger
Authorization: Bearer {{admin_token}}

HTTP 404
[Asserts]
jsonpath "$.error" == "job not registered"
jsonpath "$.name"  == "no_such_job"


# ─────────────────────────────────────────────────────────────
# Step 6 — Non-admin caller is denied by the `/api/admin/*`
#          middleware layer. The handler itself has no bespoke
#          role check — reaching it at all means the caller is
#          admin (same shape as `dedup_admin_gate.hurl` pins).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs
Authorization: Bearer {{bob_token}}

HTTP 403


POST {{base_url}}/api/admin/jobs/trash_cleanup/trigger
Authorization: Bearer {{bob_token}}

HTTP 403
