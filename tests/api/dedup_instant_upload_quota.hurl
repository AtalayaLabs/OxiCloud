# =============================================================
# OxiCloud — instant-upload (dedup fast-path) per-drive quota
# =============================================================
# Pins the per-drive quota check on the `POST /api/files/by-hash`
# endpoint (behind the SPA's `instantUploadOwned` fast-path).
#
# ── The regression this guards ──
# Pre-fix, `create_file_from_owned_blob_with_perms`
# (`src/application/services/file_upload_service.rs:256`) only
# enforced the user envelope quota (`check_storage_quota`) and
# NOT the per-drive quota (`check_drive_quota_by_folder`). Every
# other upload path already enforced BOTH caps:
#   - `file_handler.rs::upload_file_impl` (multipart)        line 462
#   - `chunked_upload_handler.rs::finish_upload`              line 236
#   - `delta_upload_service.rs::commit_delta_upload`          line 393
#   - `nextcloud/webdav_handler.rs::put_file`                 line ~1000
# The instant-upload gap let dedup-hit uploads blow a shared-
# drive quota without rejection: a tenant well under user
# envelope (or admin with NULL envelope) saw `drives.used_bytes`
# compound past `quota_bytes` with no 507 ever fired.
#
# Reported on 2026-11-01: an admin, Owner of a shared drive with
# `quota_bytes = 300 MB`, had `used_bytes = 400+ MB` and could
# still drop files via the SPA; the SPA's console reported
# "Download terminated, data duplicated" — i.e. the dedup fast
# path fired.
#
# ── The pin ──
# Create a shared drive with a tight 50-byte quota. Upload the
# 32-byte `hello.txt` normally (fits). Then call `/by-hash` to
# materialise a SECOND reference to the same content in the same
# drive — would project to 32 + 32 = 64 > 50, must 507.
#
# A regression that drops the second quota call (step 256 of
# `file_upload_service.rs`) would admit this second upload, and
# this file trips.
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
# Step 2 — Register the drive owner (a plain user with a high
#          envelope so the per-DRIVE cap is the only one that
#          could fire — this is the whole point of the pin).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "iuq_owner",
  "email":    "iuq_owner@example.com",
  "password": "IuqOwner1!"
}

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "iuq_owner", "password": "IuqOwner1!" }

HTTP 200
[Captures]
owner_token:   jsonpath "$.access_token"
owner_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Admin creates a shared drive with a 50-byte quota,
#          owned by `iuq_owner`. 50 bytes picked specifically so
#          one 32-byte file fits (step 4) but a second 32-byte
#          reference (step 6) projects to 64 > 50 and must 507.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "iuq-tight",
  "owner": { "type": "user", "id": "{{owner_user_id}}" },
  "quota_bytes": 50
}

HTTP 201
[Captures]
drive_id:    jsonpath "$.id"
drive_root:  jsonpath "$.root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Multipart upload: 32-byte `hello.txt`. Fits under the
#          50-byte cap (32 ≤ 50). The multipart path's drive-
#          quota check (`file_handler.rs:462`) is already covered
#          by `drive_quota.hurl` — this upload just creates the
#          blob + reference we need the fast-path to find.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{drive_root}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
first_file_id:   jsonpath "$.id"
first_file_hash: jsonpath "$.content_hash"


# ─────────────────────────────────────────────────────────────
# Step 5 — Confirm the hash is owned by `iuq_owner`. The SPA
#          performs this check (`/api/dedup/check/{hash}`) before
#          firing `instantUploadOwned`, so the pin follows the
#          real caller sequence — a regression that silently
#          changed `/by-hash`'s ownership probe would surface
#          here, not just at the quota step.
#
# The response shape (`HashCheckResponse` in dedup_handler.rs:52)
# is `{hash, existing_size, count, truncated, siblings}` — a 200
# is itself the "owned" signal (404 is the explicit "not
# visible" answer, see the doc comment on the type). Asserting
# `existing_size == 32` is both the ownership signal and a size
# pin against the fixture; a drift between the stored blob size
# and `hello.txt`'s on-disk bytes would surface here instead of
# being discovered through an opaque 507 in step 6.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/{{first_file_hash}}
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.existing_size" == 32
jsonpath "$.hash"          == "{{first_file_hash}}"


# ─────────────────────────────────────────────────────────────
# Step 6 — THE PIN: `POST /api/files/by-hash` for a second
#          reference to the same 32-byte content in the same
#          drive. Projects `used_bytes + file.size = 32 + 32 =
#          64 > 50` ⇒ must 507 QuotaExceeded.
#
# Pre-fix, this request succeeded (201 with the new file row)
# because the service skipped the per-drive check. Post-fix,
# `create_file_from_owned_blob_with_perms` now runs
# `check_drive_quota_by_folder(folder_uuid, metadata.size)`
# right after the user-envelope check at line 258 (same
# comparison, same bytes, same ErrorKind — consistent with
# every other upload path).
#
# Shape assertions ride the normalized ApiErrorBody pin from
# `error_shape_normalized.hurl` — error_type is PascalCase and
# the message carries the operator-flavoured byte arithmetic.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "name":      "hello-dedup.txt",
  "folder_id": "{{drive_root}}",
  "hash":      "{{first_file_hash}}"
}

HTTP 507
[Asserts]
jsonpath "$.error_type" == "QuotaExceeded"
# The server's own message carries the comparison that fired, so
# an operator reading audit logs sees the exact numbers. The SPA
# switches on `error_type` to render the friendly i18n copy
# ("Not enough space…"), see `lib/utils/errors.ts`.
jsonpath "$.message" contains "Drive quota exceeded"
jsonpath "$.message" contains "50 bytes"


# ─────────────────────────────────────────────────────────────
# Step 7 — Teardown. Delete the file + empty the trash so the
#          shared blob store doesn't accumulate across runs; the
#          drive itself is left for the sweep to GC — admin
#          `/drives/{id}` DELETE requires a drained drive which
#          emptying the trash gives us.
#
# Omitting teardown here leaves `used_bytes = 32` on `drive_id`,
# which the next run's step 4 would see and refuse (32 + 32 = 64
# > 50 fires the regression check EARLY, before step 6 ever
# runs). The `cleanup`-side teardown is what keeps the pin
# idempotent across repeated runs in the shared sandbox DB —
# per `feedback_hurl_teardown_shared_db`.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{first_file_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{owner_token}}

HTTP 200


# `DELETE /api/drives/{id}` hardcodes `caller_is_admin = false` in
# the handler (`drive_handler.rs::delete_drive`) and always goes
# through `authz.require(Manage, Drive)` — admin bypass is NOT
# wired here (deliberate, per the drive-management plan: admin
# intervenes via `/api/admin/drives/*` surfaces, not the user API).
# The drive owner is `iuq_owner` from step 3, so the delete runs
# as them. Anti-enum graduated denial maps missing Manage to
# `NotFound` ⇒ 404, which is what Ed's first run surfaced before
# this comment landed.
DELETE {{base_url}}/api/drives/{{drive_id}}
Authorization: Bearer {{owner_token}}

HTTP 204
