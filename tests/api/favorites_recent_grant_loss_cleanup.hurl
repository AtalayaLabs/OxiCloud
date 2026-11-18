# =============================================================
# OxiCloud — Issue #913 regression pin
# Favorites / Recent cleanup on grant loss
# =============================================================
# Before the fix: Bob favorited (or recently accessed) a file
# the admin shared with him; after the admin revoked the grant,
# Bob's `/api/favorites/resources` listing still returned the
# file's live name, size and content_hash — a metadata leak
# (and the owner could observe accesses by watching the row
# re-appear with updated fields).
#
# The fix is a queue-plus-drain pipeline:
#   1. Trigger `role_grants_enqueue_cleanup` (migration
#      20261118000000) fires on every `storage.role_grants`
#      DELETE and enqueues the affected user(s) into
#      `auth.cleanup_queue`.
#   2. Scheduled `list_cleanup` job drains the queue; per user
#      it runs every registered `ListCleanup` port (favorites +
#      recent today) so rows whose targets the caller can no
#      longer read via SOME grant path get dropped.
#
# This scenario pins THREE invariants that would have caught
# the original bug (and that the fix would regress through
# distinct paths):
#
#   (A) Direct user-grant revoke → favorite row disappears
#       after one drain tick.
#   (B) Direct user-grant revoke → recent-items row
#       disappears after one drain tick.
#   (C) **Negative control**: if Bob still has another grant
#       path on the file (e.g. a second direct Viewer grant
#       the admin never revoked), BOTH rows survive the
#       drain. This catches an over-cascade regression where
#       the sweep deletes on revoke without re-checking
#       alternate paths.
#
# Admin-trigger (`POST /api/admin/jobs/list_cleanup/trigger`)
# is used to run the drain synchronously rather than waiting
# for the scheduled cadence.
#
# Self-contained — creates its own users and files; can run
# in any position relative to other Hurl files.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login + home folder lookup.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"
admin_user_id: jsonpath "$.user.full.user.id"


GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
admin_home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Create Bob (grantee).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "i913-bob",
  "password": "I913BobPassword1!",
  "email": "i913-bob@example.com",
  "role": "user"
}

HTTP 201
[Captures]
bob_user_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "i913-bob", "password": "I913BobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 3 — Admin creates a folder + two files inside. File A
#          is the grant-loss subject; File B stays shared via
#          an alternate grant path for the negative control.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "i913-folder", "parent_id": "{{admin_home_id}}" }

HTTP 201
[Captures]
test_folder_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{test_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_a_id: jsonpath "$.id"


PUT {{base_url}}/api/files/{{file_a_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "i913-file-a.txt" }

HTTP 200


POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{test_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_b_id: jsonpath "$.id"


PUT {{base_url}}/api/files/{{file_b_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "i913-file-b.txt" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 4 — Admin grants Bob Viewer on BOTH files.
#          File B gets a SECOND grant later for the negative
#          control (still has access after one revoke).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "file", "id": "{{file_a_id}}" },
  "role": "viewer"
}

HTTP 201
[Captures]
grant_a_id: jsonpath "$.grants[0].id"


POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "file", "id": "{{file_b_id}}" },
  "role": "viewer"
}

HTTP 201
[Captures]
grant_b_primary_id: jsonpath "$.grants[0].id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Bob favorites File A and marks File B as recent.
#          Both calls must succeed (he currently reads both).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/favorites/file/{{file_a_id}}
Authorization: Bearer {{bob_token}}

HTTP 201


POST {{base_url}}/api/favorites/file/{{file_b_id}}
Authorization: Bearer {{bob_token}}

HTTP 201


# Recent is recorded on explicit access POST (see
# behavior_recent_items memory — not auto-populated by reads).
POST {{base_url}}/api/recent/file/{{file_a_id}}
Authorization: Bearer {{bob_token}}

HTTP 200


POST {{base_url}}/api/recent/file/{{file_b_id}}
Authorization: Bearer {{bob_token}}

HTTP 200


# Baseline: both appear in Bob's favorites + recent listings.
# Hurl's `$.items[*]...` returns a scalar when there's exactly
# one match ("types not consistent" on `includes`); its
# `$.items[?(...)]` returns a single OBJECT when one match
# ("invalid filter input type" on `count`). The `exists` /
# `not exists` idiom against the filtered id path is stable
# across arities (0/1/many matches) because it only tests
# presence, not shape.
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{file_a_id}}')].resource.id" exists
jsonpath "$.items[?(@.resource.id == '{{file_b_id}}')].resource.id" exists


GET {{base_url}}/api/recent/resources
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{file_a_id}}')].resource.id" exists
jsonpath "$.items[?(@.resource.id == '{{file_b_id}}')].resource.id" exists


# ─────────────────────────────────────────────────────────────
# Step 6 — Admin revokes Bob's grant on File A. The trigger
#          (migration 20261118000000) enqueues Bob into
#          `auth.cleanup_queue`. Bob still has File B via its
#          grant — he must stay in the listing for File B.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/grants/{{grant_a_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 7 — Trigger the drain synchronously so the test
#          doesn't wait on the 60s cadence. Envelope matches
#          the `admin_jobs.hurl` shape.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/list_cleanup/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
# `count` is users processed this tick — Bob at minimum (other
# tests may have queued concurrently, so assert "at least").
jsonpath "$.outcome.count" exists
jsonpath "$.outcome.extra.rows_removed" exists
jsonpath "$.outcome.extra.failed"       exists


# ─────────────────────────────────────────────────────────────
# Step 8 — Assertion (A + B + C combined):
#   File A must be GONE from Bob's favorites and recent
#     (invariant A + B — the fix we're pinning).
#   File B must STILL be in Bob's favorites and recent
#     (invariant C — negative control; grant B wasn't revoked).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
# The regression: File A must not appear here. Before the fix
# this assertion failed — favorites still JOIN'd back to
# `storage.files` and returned the row with live metadata the
# caller could no longer read.
jsonpath "$.items[?(@.resource.id == '{{file_a_id}}')].resource.id" not exists
# Negative control: Bob's remaining grant on File B keeps it in
# the listing — the sweep must not over-cascade.
jsonpath "$.items[?(@.resource.id == '{{file_b_id}}')].resource.id" exists


GET {{base_url}}/api/recent/resources
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{file_a_id}}')].resource.id" not exists
jsonpath "$.items[?(@.resource.id == '{{file_b_id}}')].resource.id" exists


# ─────────────────────────────────────────────────────────────
# Step 9 — Belt-and-braces: revoking File B's remaining grant
#          + another drain must clean File B out too.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/grants/{{grant_b_primary_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


POST {{base_url}}/api/admin/jobs/list_cleanup/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.outcome" == "ok"


GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{file_b_id}}')].resource.id" not exists


GET {{base_url}}/api/recent/resources
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id == '{{file_b_id}}')].resource.id" not exists


# ─────────────────────────────────────────────────────────────
# Step 10 — `list_consistency` detector. Belt-and-braces for
#          direct-SQL revokes + pre-trigger rows. Should report
#          zero orphans here (the drain above cleaned up) and
#          `?repair=true` should be a no-op.
#
#          Shape matches `admin_jobs.hurl`: `{ ok, outcome: {
#            outcome, count, extra: { repair_mode, users_scanned,
#            users_with_orphans, repaired, per_list: {...} }}}`.
#          `count` is total orphans found across all users and
#          lists — 0 after the drain cleared Bob's favorites +
#          recent above.
#
#          Note: `users_scanned` includes OTHER users whose
#          favorites/recent rows exist in the shared test DB
#          (test fixture isolation per feedback memory). We
#          assert the keys exist, not absolute values, because
#          other Hurl files leave rows that this detector
#          enumerates too. This is why the earlier steps
#          asserted per-UUID with `?(@.resource.id == 'X')` —
#          same discipline.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/list_consistency/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count" exists
jsonpath "$.outcome.extra.repair_mode" == false
jsonpath "$.outcome.extra.users_scanned" exists
jsonpath "$.outcome.extra.users_with_orphans" exists
jsonpath "$.outcome.extra.repaired" == 0
# `per_list` keys match the `ListCleanup::list_name()` values —
# the port contract pinned at the Hurl layer, so renaming a list
# in Rust without updating the test (or vice versa) fails here.
jsonpath "$.outcome.extra.per_list.favorites" exists
jsonpath "$.outcome.extra.per_list.recent" exists


# Trigger with repair=true. Even with 0 orphans found, the
# detector must honour the flag and report it back on
# `repair_mode: true`. Params go on the URL query string
# (`drive_policies_consistency` uses the same style) — a JSON
# body is not how the admin trigger reads parameters.
POST {{base_url}}/api/admin/jobs/list_consistency/trigger?repair=true
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.repair_mode" == true


# ─────────────────────────────────────────────────────────────
# Step 11 — Teardown (feedback_hurl_teardown_shared_db — the
#          test DB is shared across files, so we must clean up).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{test_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{admin_token}}

HTTP *


DELETE {{base_url}}/api/admin/users/{{bob_user_id}}
Authorization: Bearer {{admin_token}}

HTTP *
