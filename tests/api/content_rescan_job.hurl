# =============================================================
# OxiCloud — the `content_rescan` job's wire surface
# =============================================================
# The job itself is covered by unit tests (the decision table, the
# container-probe rules) and a DB integration test (the fan-out, the
# one-read-per-content guarantee). What neither reaches is the HTTP
# surface an operator actually touches: that it is registered at all,
# that it dispatches DETACHED, that a bad parameter is refused, and
# that a bad `rederive` mode fails the RUN rather than quietly doing
# less work than was asked for.
#
# ── Why every run here is dry_run or scoped to nothing ───────────
# `content_rescan` rewrites `mime_type` and `category_order` across
# the whole corpus by default. The suite shares one database, so a
# real run here would retype fixtures belonging to other scenarios —
# `upload_mime_sniffing`, the Photos probes, the EXIF/ID3 files — and
# break them for reasons having nothing to do with this test. So the
# triggers below either set `dry_run=true` or scope `mime_prefix` to
# a type nothing in the corpus has. Mutation is the integration
# test's job, where the database is the test's own.
#
# ── Detached dispatch is the point of step 2 ─────────────────────
# This job is in `is_detached_job`, so the trigger returns 202 with
# `dispatched: true` INSTEAD of the usual 200 outcome envelope that
# `recoverable_jobs.hurl` asserts. A regression that dropped it from
# that list would make the HTTP request block for a full-corpus
# read — which is the exact defect this job was built to remove from
# the two endpoints it replaced.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 1 — The job is registered. If DI ever stops wiring it,
#          every assertion below would 404 and the failure
#          would read as "the endpoint is broken" rather than
#          "the job is gone".
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection


# ─────────────────────────────────────────────────────────────
# Step 2 — Trigger, DETACHED. 202 + `detached: true`, not the
#          200 outcome envelope every inline job returns.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/content_rescan/trigger?dry_run=true&mime_prefix=application/x-oxicloud-nothing
Authorization: Bearer {{admin_token}}

HTTP 202
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.dispatched" == true
jsonpath "$.detached" == true
jsonpath "$.name" == "content_rescan"


# ─────────────────────────────────────────────────────────────
# Step 3 — The detached run reaches Completed. Retries because
#          the trigger returned before the handler had run,
#          which is the whole point of step 2.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs/content_rescan/runs
Authorization: Bearer {{admin_token}}
[Options]
retry: 30
retry-interval: 300

HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[0].job_name" == "content_rescan"
jsonpath "$[0].status" == "Completed"


# ─────────────────────────────────────────────────────────────
# Step 4 — An undeclared parameter is refused BEFORE dispatch,
#          with a message naming what the job accepts. 400, not
#          a 202 that silently ignores the typo.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/content_rescan/trigger?rederrive=always
Authorization: Bearer {{admin_token}}

HTTP 400
[Asserts]
jsonpath "$.error" exists


# ─────────────────────────────────────────────────────────────
# Step 5 — A declared parameter with an unknown VALUE cannot be
#          caught at the HTTP boundary — `rederive` is a string,
#          so `alwyas` parses fine — and this job is detached, so
#          the trigger still returns 202. The rejection has to
#          land on the RUN.
#
#          This is the behaviour that matters most on this page.
#          Asking for `always` and silently getting `changed`
#          would make a requested full backfill do nothing and
#          report success — and `always` is the only mode that
#          repairs a fixed extractor, since an extractor fix does
#          not change the sniffed type.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/content_rescan/trigger?rederive=alwyas
Authorization: Bearer {{admin_token}}

HTTP 202
[Asserts]
jsonpath "$.dispatched" == true


GET {{base_url}}/api/admin/jobs/content_rescan/runs
Authorization: Bearer {{admin_token}}
[Options]
retry: 30
retry-interval: 300

HTTP 200
[Asserts]
jsonpath "$[0].status" == "Failed"
# The error must NAME the typo, so an operator sees what they
# actually sent rather than being told "invalid parameter".
jsonpath "$[0].error_message" contains "alwyas"
