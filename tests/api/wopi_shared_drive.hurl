# =============================================================
# OxiCloud — WOPI PutFile against a shared drive
# =============================================================
# Regression pin for AuthZ audit #18 (2026-07-12).
#
# `wopi_handler.rs::put_file` used to resolve the write's target
# drive via `drive_repo.find_default_for_user(claims_sub_uuid)` —
# ALWAYS the caller's own default personal drive, regardless of
# where the file being edited actually lived. Consequences for a
# shared-drive file:
#
#   - If the file's path happened to collide with a personal-drive
#     path, the write MISROUTED into the caller's personal drive
#     (silent cross-drive data ejection).
#   - Otherwise the parent-folder lookup inside
#     `update_file_streaming_with_perms` missed and the request
#     500'd — a UX brick on shared-drive WOPI editing.
#
# Fix: resolve `drive_id` from the FILE's own parent folder via
# `drive_repo.drive_id_for_folder(file.folder_id)`. Same file →
# same drive → write lands in the shared drive it belongs to.
#
# This test:
#   1. Admin creates a shared drive (D3a shape).
#   2. Admin uploads `hello.txt` to the shared drive's root.
#   3. Admin mints a WOPI edit token.
#   4. Admin PutFile with fresh content → 200.
#      Pre-fix this 500'd because the personal-drive-scoped
#      parent-folder lookup couldn't find a folder named "" in
#      admin's personal drive.
#   5. Admin GetFile → the shared drive holds the new content.
#      Proves the write landed on the correct drive.
#
# Prereqs: `OXICLOUD_WOPI_ENABLED=true`, `OXICLOUD_WOPI_SECRET`
# pinned, mock discovery running (all wired in
# `tests/common/server.env` + run.sh — same as `wopi_authz.hurl`).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token:   jsonpath "$.access_token"
admin_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin creates a shared drive owned by themselves.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "wopi-shared-drive-audit-18",
  "owner": { "type": "user", "id": "{{admin_user_id}}" }
}

HTTP 201
[Captures]
wopi_drive_id:      jsonpath "$.id"
wopi_drive_root_id: jsonpath "$.root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Upload `hello.txt` to the shared drive's root.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{wopi_drive_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
wopi_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.mime_type" == "text/plain"


# ─────────────────────────────────────────────────────────────
# Step 3 — Mint an editor URL. Admin has Update on their own
#          shared drive → `can_write=true` in the token.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/wopi/editor-url?file_id={{wopi_file_id}}&action=edit
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
wopi_edit_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 4 — CheckFileInfo — sanity check the token is redeemable
#          and reports `UserCanWrite=true`. Not the audit-#18
#          pin itself (this verb didn't touch the drive-lookup
#          bug) but a quick "the setup is sound" gate before
#          Step 5.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/wopi/files/{{wopi_file_id}}?access_token={{wopi_edit_token}}

HTTP 200
[Asserts]
jsonpath "$.UserCanWrite" == true


# ─────────────────────────────────────────────────────────────
# Step 5 — PutFile with fresh content → 200.
#
#          PRE-FIX (before #18 close): this 500'd. The handler
#          resolved drive_id via find_default_for_user(admin),
#          got admin's personal drive, then
#          `update_file_streaming_with_perms(path, personal_drive_id)`
#          did a parent-folder-by-path lookup scoped to the
#          personal drive — nothing at the shared-drive path
#          existed there → error → 500 wrapper.
#
#          POST-FIX: drive_id resolves from the file's own
#          parent folder → shared drive → write lands in the
#          correct drive.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/wopi/files/{{wopi_file_id}}/contents?access_token={{wopi_edit_token}}
Content-Type: application/octet-stream
```
audit-#18 shared-drive WOPI PutFile canary
```

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 6 — Round-trip proof: GetFile from the same token returns
#          the NEW content, and it's coming from the shared
#          drive (the only place `wopi_file_id` exists).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/wopi/files/{{wopi_file_id}}/contents?access_token={{wopi_edit_token}}

HTTP 200
[Asserts]
body contains "audit-#18 shared-drive WOPI PutFile canary"


# ─────────────────────────────────────────────────────────────
# Cleanup — delete the file, then delete the shared drive
# (D3b: empty-drive precondition holds since the file is gone).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{wopi_file_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/drives/{{wopi_drive_id}}
Authorization: Bearer {{admin_token}}

HTTP 204
