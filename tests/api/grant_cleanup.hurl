# =============================================================
# OxiCloud — Expired-grant purge (GrantCleanupService)
# =============================================================
# Regression coverage for the daily purge that deletes rows from
# `storage.role_grants` whose `expires_at` is more than
# `OXICLOUD_GRANT_CLEANUP_GRACE_DAYS` in the past.
#
# The engine's `check` / `list_grants_*` paths already filter
# expired grants out at read time — this purge is pure garbage
# collection. If the SQL were wrong (e.g. missing
# `expires_at IS NOT NULL`, wrong sign on the interval), the
# assertions here catch it before the daemon runs against real
# data.
#
# Uses the `POST /api/admin/jobs/grant_cleanup/trigger` admin
# endpoint (production surface, always on). `?force=true` collapses
# the grace window to zero for the call so we can plant a past-dated
# grant and immediately observe it purged, without waiting 15+ days.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login admin (Alice), capture home folder id.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"
alice_user_id: jsonpath "$.user.full.user.id"


GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Create a grantee user (mallory) — someone we can
#          grant Alice's resources to without polluting shared
#          state used by other test files.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "username": "gc-mallory",
  "password": "GcMalloryPassword1!",
  "email": "gc-mallory@example.com",
  "role": "user"
}

HTTP 201
[Captures]
mallory_user_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Alice creates two folders: one to hold an expired
#          grant, one to hold a permanent (no-expiry) grant we
#          expect the purge to leave alone.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "gc-expired", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
expired_folder_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "gc-permanent", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
permanent_folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Plant an expired grant. Set `expires_at` in 2020 so
#          any grace window less than several years still
#          catches it. The grant handler silently accepts past-
#          dated `expires_at` — a separate PR would reject them
#          on the create path, but here we exploit the
#          permissive behaviour as a test fixture.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{mallory_user_id}}" },
  "resource": { "type": "folder", "id": "{{expired_folder_id}}" },
  "role": "viewer",
  "expires_at": "2020-01-01T00:00:00Z"
}

HTTP 201
[Captures]
expired_grant_id: jsonpath "$.grants[0].id"


# Confirm the grant IS present in the listing — the engine's
# filter is `expires_at > NOW()`, so the past-dated row is
# already invisible to `check()` but still exists physically
# (and thus in the list endpoint too — verified below).
GET {{base_url}}/api/grants?resource_type=folder&resource_id={{expired_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
# Bare array, filter selector — see memory note on Hurl JSONPath
# quirks: use `$[?(...)]` (single-match returns scalar; no `nth`).
jsonpath "$[?(@.id=='{{expired_grant_id}}')].role" == "viewer"


# ─────────────────────────────────────────────────────────────
# Step 5 — Plant a permanent grant on the other folder (no
#          `expires_at`). The purge MUST leave it alone.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{mallory_user_id}}" },
  "resource": { "type": "folder", "id": "{{permanent_folder_id}}" },
  "role": "viewer"
}

HTTP 201
[Captures]
permanent_grant_id: jsonpath "$.grants[0].id"


# ─────────────────────────────────────────────────────────────
# Step 6 — Trigger the purge with `force=true`. The endpoint
#          collapses the grace window to 0 for this call only
#          — the daemon's configured grace is untouched.
#
#          Expect `grants_deleted >= 1` (the past-dated row),
#          `grace_days == 0`, `forced == true`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/grant_cleanup/trigger?force=true
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.forced" == true
jsonpath "$.outcome.extra.grace_days" == 0
# At least the expired-fixture row we just planted.
jsonpath "$.outcome.count" >= 1


# ─────────────────────────────────────────────────────────────
# Step 7 — The expired grant is gone. The permanent grant
#          survives.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants?resource_type=folder&resource_id={{expired_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
# The list is either empty or contains no row with the expired
# grant's id — the filter must not select anything.
jsonpath "$[*].id" not contains "{{expired_grant_id}}"

GET {{base_url}}/api/grants?resource_type=folder&resource_id={{permanent_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
# Permanent grant untouched.
jsonpath "$[?(@.id=='{{permanent_grant_id}}')].role" == "viewer"


# ─────────────────────────────────────────────────────────────
# Step 8 — Second trigger with `force=true` on a table that no
#          longer has any past-dated grants. Expect
#          `grants_deleted == 0`. This is the regression guard
#          on the WHERE clause — if `expires_at IS NOT NULL`
#          were missing, this would nuke the permanent grant
#          from Step 5 (any row with `NULL < NOW() - 0 days` is
#          false in SQL, so it's already correct; but a
#          mistyped predicate could regress).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/grant_cleanup/trigger?force=true
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.count" == 0


# ─────────────────────────────────────────────────────────────
# Step 9 — Unforced trigger. Grace = configured value (15).
#          No new expired grants planted, so purge is a no-op.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/grant_cleanup/trigger
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.forced" == false
# Response echoes the configured grace (15 days by default).
jsonpath "$.outcome.extra.grace_days" == 15
jsonpath "$.outcome.count" == 0


# Permanent grant still there after the unforced call.
GET {{base_url}}/api/grants?resource_type=folder&resource_id={{permanent_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{permanent_grant_id}}')].role" == "viewer"


# ─────────────────────────────────────────────────────────────
# Cleanup — drop both folders. Cascade removes the remaining
# grant + any children.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{expired_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{permanent_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 204
