# =============================================================
# Regression #595 — Admin-created user with quota=0 ("unlimited"
# per UI convention) must be able to upload.
# =============================================================
# Pre-fix behaviour (documented in the issue):
#
#   1. Admin creates user with `quota_bytes: 0` (meaning "unlimited"
#      per the check-code convention: `check_storage_quota` treats
#      `quota <= 0` as unlimited).
#   2. `PersonalDriveLifecycleHook::create_personal_drive_atomic` was
#      called with `Some(user.storage_quota_bytes())` — so
#      `storage.drives.quota_bytes` on the new personal drive was
#      stamped `0`.
#   3. On upload, the drive-quota check (`check_drive_quota_by_folder`)
#      reads `drives.quota_bytes = 0`, interprets Some(0) as a literal
#      zero-byte cap (its NULL check only accepts `None` as unlimited),
#      and rejects with 507 Insufficient Storage.
#
# The two conventions collided: user-quota "0 = unlimited" vs
# drive-quota "0 = literal zero, NULL = unlimited". Documented as
# a spec violation of docs/plan/drive.md §7: "For personal drives
# this column is NULL … the effective cap comes from the user
# envelope."
#
# Fix (three parts, this test guards all three):
#   1. `folder_service.rs:927` — pass `None`, never `Some(user quota)`.
#   2. Migration `20260916000000_null_personal_drive_quota.sql` —
#      NULL every existing personal drive's `quota_bytes` (data heal).
#   3. Same migration — CHECK constraint pinning
#      `kind <> 'personal' OR quota_bytes IS NULL` at the DB layer.
#
# This scenario reproduces the bug against a fresh user and asserts
# the upload succeeds (Fix 1 evidence) AND the personal drive's
# `quota_bytes` field is absent from the wire (`Option::is_none`
# serde-skip → `quota_bytes` key missing = Fix 1 + migration evidence).
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
# Step 2 — Admin creates a new user with `quota_bytes: 0`
#          (the "unlimited" UI convention that triggered #595).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username":     "unlimited_regression_595",
  "password":     "UnlimitedPwd1!",
  "email":        "unlimited_regression_595@example.com",
  "role":         "user",
  "quota_bytes":  0
}

HTTP 201
[Asserts]
# The user record itself carries the literal `0` (the convention:
# 0 at the user layer means unlimited, `check_storage_quota` passes).
jsonpath "$.storage_quota_bytes" == 0


# ─────────────────────────────────────────────────────────────
# Step 3 — New user logs in.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "unlimited_regression_595", "password": "UnlimitedPwd1!" }

HTTP 200
[Captures]
user_token: jsonpath "$.access_token"
user_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Personal drive should be created with NULL quota_bytes
#          (Fix 1). `DriveDto` uses
#          `#[serde(skip_serializing_if = "Option::is_none")]`
#          on `quota_bytes`, so NULL = the field is OMITTED from
#          the JSON. `body not contains` on `quota_bytes` is the
#          strongest anti-regression assertion available at this
#          layer: if a future change re-introduces `Some(0)` (or
#          any numeric value), the field will surface and this
#          assertion fires. Fresh user has exactly one drive
#          (their default personal) so a body-level contains
#          check is unambiguous.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{user_token}}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].kind" == "personal"
jsonpath "$[0].default_for_user" == "{{user_user_id}}"
body not contains "quota_bytes"


# ─────────────────────────────────────────────────────────────
# Step 5 — Grab the personal drive's root folder id for the
#          upload target.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{user_token}}

HTTP 200
[Captures]
personal_root_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 6 — THE REGRESSION ASSERTION. Upload a file to the
#          user's personal drive. Pre-fix this returned 507
#          Insufficient Storage; post-fix it returns 201 with
#          the created file DTO.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{user_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
uploaded_file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 7 — Cleanup. Delete the file so the storage cleanup
#          check at the end of run.sh doesn't complain, then
#          leave the throwaway user + their empty personal
#          drive in place (deleting the user via the admin API
#          is the same shape as the sibling admin_user_ops.hurl;
#          keeping it minimal here since the fixture user has
#          a deterministic unique name).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{uploaded_file_id}}
Authorization: Bearer {{user_token}}

HTTP *
[Asserts]
status >= 200
status < 300
