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
# (trash_cleanup, storage_reconcile, dedup_gc, grant_cleanup) + 1
# Part 2 recoverable (drives_consistency, wrapped by RecoverableAdapter
# so it appears here alongside the periodics). Bump when a new
# tenant registers.
jsonpath "$..running" count == 5


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
