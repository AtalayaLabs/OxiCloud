# =============================================================
# OxiCloud – User envelope quota (sum of personal drives)
# =============================================================
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/user_envelope_quota.hurl
#
# The model under test (`docs/plan/drive.md` §7):
#   `auth.users.storage_quota_bytes` caps the SUM of `used_bytes`
#   across the user's PERSONAL drives only. Shared drives never
#   count against any user envelope.
#
# Cases:
#   1. Baseline — fresh user: `/me.storage_used_bytes == 0`.
#   2. Shared-drive upload does NOT touch the envelope —
#      `/me.storage_used_bytes` stays 0 after upload + sweep.
#   3. Personal-drive upload DOES bump the envelope —
#      `/me.storage_used_bytes == file_size` after upload + sweep.
#   4. Sweep self-heals — after trashing the personal file and
#      `trigger-sweep`, `/me.storage_used_bytes` returns to 0.
#
# `POST /api/admin/jobs/usage_reconcile/trigger` is the
# deterministic synchronisation point: it runs the drive-side sweep
# then the user-side sweep (both under the periodic scheduler), so
# both cached counters are authoritative ground-truth by the time
# the assertion fires. Production surface — always on, no gate.
#
# Self-contained: provisions `ue_owner` so it can run alongside
# the rest of the suite.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Provision `ue_owner` (user envelope under test).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "ue_owner",
  "password": "UeOwnerPwd1!",
  "email":    "ue_owner@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ue_owner", "password": "UeOwnerPwd1!" }

HTTP 200
[Captures]
owner_token:   jsonpath "$.access_token"
owner_user_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Fetch the user's default Personal drive root folder.
#          `GET /api/folders` returns root folders for the
#          caller; for a fresh user that's a single entry — the
#          Personal drive's root provisioned by
#          `PersonalDriveLifecycleHook`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{owner_token}}

HTTP 200
[Captures]
personal_root_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$" count == 1


# ─────────────────────────────────────────────────────────────
# Step 4 — Baseline. Fresh user's envelope is zero.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.storage_used_bytes" == 0


# ─────────────────────────────────────────────────────────────
# Step 5 — Admin creates a shared drive with `ue_owner` as
#          direct user-Owner. No per-drive quota (NULL = unlim).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "ue-shared",
  "owner": { "type": "user", "id": "{{owner_user_id}}" }
}

HTTP 201
[Captures]
shared_drive_id: jsonpath "$.id"
shared_root_id:  jsonpath "$.root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 6 — Case 2: upload hello.txt (32 B) to the SHARED drive.
#          The drive's `used_bytes` will move; the user envelope
#          must NOT.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{shared_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201


# Force freshness on `drives.used_bytes`:
#   1. 200 ms delay to let the fire-and-forget tokio task from the
#      upload above land its SQL write (see
#      `bug_trigger_sweep_vs_spawn_hook_race`).
#   2. Trigger the reconciliation sweep — the ONLY path that
#      invalidates `readable_cache` / `default_drive_cache` after
#      Ed's 2026-07-17 design call (per-write invalidation would
#      nuke the cache on every upload, defeating the point). Also
#      acts as the synchronisation point for the user-envelope
#      assertion below — the sweep is the authoritative
#      ground-truth for both drive- and user-side counters.
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200


GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{shared_drive_id}}')].used_bytes" == 32


# Force the user-side sweep to run, authoritative ground-truth.
# If the delta path incorrectly fired the user counter, the sweep
# would still correct it back to 0 (the new SQL excludes shared
# drives) — this also validates the sweep formula.
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}

HTTP 200


# Envelope untouched by the shared upload. Both delta path and
# sweep path agree on `0` for a user with no personal-drive
# content.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.storage_used_bytes" == 0


# ─────────────────────────────────────────────────────────────
# Step 7 — Case 3: upload hello.txt (32 B) to the user's own
#          default Personal drive. The envelope MUST move now.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
personal_file_id: jsonpath "$.id"


# Retry until the user-side delta lands. If the conditional-fire
# logic is broken (delta never fires for personal), retries time
# out at `0` and the test fails — this is the regression catch.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{owner_token}}
[Options]
retry: 10
retry-interval: 200ms

HTTP 200
[Asserts]
jsonpath "$.storage_used_bytes" == 32


# Confirm the sweep agrees with the delta — both code paths must
# give the same number.
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}

HTTP 200

GET {{base_url}}/api/auth/me
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.storage_used_bytes" == 32


# ─────────────────────────────────────────────────────────────
# Step 8 — Case 4: trash + empty the personal file, then sweep.
#          Per-drive (and per-user) counters are NOT decremented
#          on delete (same design as the per-drive quota model);
#          the sweep is the correctness backstop. Asserts it
#          actually closes the drift back to 0.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{personal_file_id}}
Authorization: Bearer {{owner_token}}

HTTP 204

DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{owner_token}}

HTTP 200

POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}

HTTP 200

GET {{base_url}}/api/auth/me
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.storage_used_bytes" == 0


# ─────────────────────────────────────────────────────────────
# Step 9 — Cleanup. Deleting `ue_owner` cascades through
#          `default_for_user` (default Personal drive + root
#          folder + files) and removes the `role_grants` rows
#          tying them to the shared drive. The shared drive
#          itself is owned by admin (the creator) and gets
#          drained by `storage_cleanup_check.sh` later.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{owner_user_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
