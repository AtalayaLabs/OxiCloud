# =============================================================
# OxiCloud — trash restore honours drive quota
# =============================================================
# Pin for the restore-into-over-quota case: a user trashes an
# item while the drive is within budget, the drive subsequently
# exceeds quota (admin shrinks the cap, or the sweep reconciles
# `used_bytes` down after trash and other writes refill the
# drive), then attempts to restore the trashed item. The restore
# must surface HTTP 507 + `error_type: QuotaExceeded` instead of
# compounding the overage — the SPA's `errors.ts` switch renders
# the friendly "Espace insuffisant / Not enough space…" copy.
#
# ── Why this needs a dedicated precheck ──
#
# The quota-reconcile sweep (`storage_usage_service`) filters
# `WHERE NOT is_trashed`, so trashed files eventually DROP OUT
# of `drive.used_bytes`. Trash is "best-effort decrement" via
# the sweep, not an immediate counter adjustment — but once the
# sweep runs, the drive's budget has freed up by exactly the
# trashed-file bytes. If restore then flips `is_trashed = FALSE`
# without checking quota, those bytes re-enter `used_bytes` and
# can push the drive past its cap, even though every upload
# path explicitly checks before writing.
#
# `trash_service::restore_item` now runs the same two-step
# check every upload path uses: user envelope (`check_storage
# _quota`) → per-drive (`check_drive_quota_by_folder`). Both
# must pass; the first failure returns 507 and the restore does
# NOT fire — the row stays trashed, no partial state.
#
# ── Scope: file AND folder restore ──
#
# Case A pins the single-file restore path. Case B pins the
# folder-restore path, which sums the subtree's bytes
# INCLUDING cascade-trashed descendants (files with
# `original_folder_id IS NULL` — see
# `pending_restore_folder_subtree_bytes` on
# `storage_usage_service.rs`). Independently-trashed
# descendants stay in trash on restore and are excluded from
# the quota math, so the pin would misfire if the subtree sum
# accidentally included them.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token:    jsonpath "$.access_token"
admin_user_id:  jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Register the drive owner. External-role irrelevant;
#          the quota check is against the drive the owner holds.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "trq_owner",
  "email":    "trq_owner@example.com",
  "password": "TrqOwner1!"
}

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "trq_owner", "password": "TrqOwner1!" }

HTTP 200
[Captures]
owner_token:   jsonpath "$.access_token"
owner_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Admin creates a shared drive at 10 MiB quota — large
#          enough to fit the 5 MiB upload below, small enough
#          that reducing it to 1 MiB afterwards leaves the
#          existing file (would-be 5 MiB on restore) well past
#          the new cap.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "trq-case-a",
  "owner": { "type": "user", "id": "{{owner_user_id}}" },
  "quota_bytes": 10485760
}

HTTP 201
[Captures]
drive_a_id:   jsonpath "$.id"
drive_a_root: jsonpath "$.root_folder_id"


# =============================================================
# ── Case A: file restore into over-quota drive ──
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 4 — Owner uploads the 5 MiB fixture into the drive root.
#          5 MiB ≤ 10 MiB cap, so the upload succeeds.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{drive_a_root}}
file: file,fixtures/chunk-over-cap-5mb.bin; application/octet-stream

HTTP 201
[Captures]
file_a_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Trash the file. `is_trashed = TRUE`; the sweep will
#          eventually drop its 5 MiB from `drives.used_bytes`
#          but the drive still counts it until the next
#          reconcile.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{file_a_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 6 — Force the quota-reconcile sweep so `drives.used_bytes`
#          drops to 0 (the only live file was just trashed) and
#          the cap-change in Step 7 lands on a clean counter.
#          Mirrors `drive_quota.hurl` step 7 — this is the only
#          deterministic way to see the post-trash counter in a
#          test without polling.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 7 — Admin reduces the drive's quota to 1 MiB. The drive
#          is at 0 used (trashed file excluded by the sweep) and
#          1 MiB cap. Restoring the 5 MiB file would project
#          5242880 > 1048576 → 507.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{drive_a_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 1048576 }

HTTP 200
[Asserts]
jsonpath "$.quota_bytes" == 1048576


# ─────────────────────────────────────────────────────────────
# Step 8 — PIN (file restore): must 507 QuotaExceeded. Pre-fix,
#          this call returned 200 and the drive compounded to
#          500% over quota.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/trash/{{file_a_id}}/restore
Authorization: Bearer {{owner_token}}

HTTP 507
[Asserts]
jsonpath "$.error_type" == "QuotaExceeded"
jsonpath "$.status"     == "507 Insufficient Storage"
# Operator message carries the byte arithmetic so audit logs are
# diagnosable; the SPA switches on `error_type` to render the
# friendly i18n copy.
jsonpath "$.message"    contains "Drive quota exceeded"


# ─────────────────────────────────────────────────────────────
# Step 9 — Teardown case A. Admin raises the drive quota so the
#          permanent-delete path (which doesn't check quota —
#          delete frees budget) can drain the drive cleanly.
#          Then purge the trash entry, delete the drive.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{drive_a_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": null }

HTTP 200


DELETE {{base_url}}/api/trash/{{file_a_id}}
Authorization: Bearer {{owner_token}}

HTTP 200


DELETE {{base_url}}/api/drives/{{drive_a_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# =============================================================
# ── Case B: folder restore into over-quota drive ──
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 10 — New shared drive for case B so the fixtures stay
#           isolated across the two cases.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "trq-case-b",
  "owner": { "type": "user", "id": "{{owner_user_id}}" },
  "quota_bytes": 10485760
}

HTTP 201
[Captures]
drive_b_id:   jsonpath "$.id"
drive_b_root: jsonpath "$.root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 11 — Owner creates a folder in the drive root. Admin
#          created the drive but doesn't hold Create on content
#          they aren't a drive-member of — anti-enum 404 on
#          `authz.require(Create, Folder(parent))`, see
#          `project_drive_external_owner_admin_responsibility`.
#          The drive is `trq_owner`'s; writes run as that user.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "name": "trq-subtree", "parent_id": "{{drive_b_root}}" }

HTTP 201
[Captures]
subtree_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 12 — Upload the 5 MiB fixture INTO the subtree folder.
#           Trashing the folder (step 13) will cascade-trash
#           this file with `original_folder_id = NULL`, which is
#           exactly the predicate
#           `pending_restore_folder_subtree_bytes` sums.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{subtree_id}}
file: file,fixtures/chunk-over-cap-5mb.bin; application/octet-stream

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 13 — Trash the folder. The repo's `move_to_trash`
#           cascades to every descendant, setting their
#           `is_trashed = TRUE` and leaving `original_folder_id`
#           NULL (they went to trash as a side effect, not
#           independently).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{subtree_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 14 — Reconcile + shrink quota, same shape as case A.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200


PATCH {{base_url}}/api/drives/{{drive_b_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 1048576 }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 15 — PIN (folder restore): must 507 QuotaExceeded. The
#           subtree sum (`pending_restore_folder_subtree_bytes`)
#           returns 5 MiB (the one cascade-trashed file); the
#           drive cap is 1 MiB. Pre-fix: 200 OK, drive
#           compounded.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/trash/{{subtree_id}}/restore
Authorization: Bearer {{owner_token}}

HTTP 507
[Asserts]
jsonpath "$.error_type" == "QuotaExceeded"
jsonpath "$.status"     == "507 Insufficient Storage"


# ─────────────────────────────────────────────────────────────
# Step 16 — Teardown case B.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{drive_b_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": null }

HTTP 200


DELETE {{base_url}}/api/trash/{{subtree_id}}
Authorization: Bearer {{owner_token}}

HTTP 200


DELETE {{base_url}}/api/drives/{{drive_b_id}}
Authorization: Bearer {{owner_token}}

HTTP 204
