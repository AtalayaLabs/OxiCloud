# =============================================================
# OxiCloud — NC WebDAV quota properties (RFC 4331)
# =============================================================
# The NextCloud-compatible WebDAV surface
# (`interfaces/nextcloud/webdav_handler.rs`) previously had NO
# `d:quota-used-bytes`/`d:quota-available-bytes` support at all. This
# pins the new coverage added alongside the native surface's
# drive-awareness fix (`AppState::resolve_webdav_quota`):
#
#   1. Personal-drive PROPFIND (no drive marker in the Basic Auth
#      username) reports the caller's account envelope.
#   2. A SHARED drive with its own finite quota reports THAT quota —
#      not the owner's personal envelope — when addressed via the
#      multi-drive POC's `{user}~{root_folder_id}` Basic Auth marker
#      (see `basic_auth_middleware.rs` — the marker is the drive's
#      ROOT FOLDER id, not the drive's own id).
#   3. quota-used-bytes on the shared drive increases after a PUT.
#
# This file always emits the full property set regardless of the
# PROPFIND request body (see `handle_propfind`'s doc comment — "the
# NC response always emits the full property set"), so no XML body
# is needed to request the props; a bare PROPFIND suffices.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login + mint admin's own NC app password (for
#          the personal-envelope case).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "nc_webdav_quota_properties personal" }

HTTP 200
[Captures]
admin_nc_username: jsonpath "$.username"
admin_nc_password: jsonpath "$.password"


# ─────────────────────────────────────────────────────────────
# Step 2 — Personal-drive PROPFIND (no `~` marker) surfaces the
#          account envelope. `>= 0` / `> 0` rather than exact
#          numbers since admin's envelope already has content
#          from earlier tests in the suite.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
Depth: 0
[BasicAuth]
{{admin_nc_username}}: {{admin_nc_password}}

HTTP 207
[Asserts]
xpath "number(//*[local-name()='quota-used-bytes'])" >= 0
xpath "number(//*[local-name()='quota-available-bytes'])" > 0


# ─────────────────────────────────────────────────────────────
# Step 3 — Fresh user + a 500-byte shared drive owned by them.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "ncq_owner",
  "password": "NcqOwnerPwd1!",
  "email":    "ncq_owner@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ncq_owner", "password": "NcqOwnerPwd1!" }

HTTP 200
[Captures]
ncq_owner_jwt: jsonpath "$.access_token"
ncq_owner_id: jsonpath "$.user.full.user.id"

POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{ncq_owner_jwt}}
Content-Type: application/json
{ "label": "nc_webdav_quota_properties shared" }

HTTP 200
[Captures]
ncq_nc_username: jsonpath "$.username"
ncq_nc_password: jsonpath "$.password"


POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "ncq-shared",
  "owner": { "type": "user", "id": "{{ncq_owner_id}}" },
  "quota_bytes": 500
}

HTTP 201
[Captures]
ncq_root_folder_id: jsonpath "$.root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 4 — PROPFIND the shared drive via the multi-drive POC's
#          `{user}~{root_folder_id}` Basic Auth marker. Brand-new
#          drive → used == 0, available == quota exactly.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/remote.php/dav/files/{{ncq_nc_username}}~{{ncq_root_folder_id}}/
Depth: 0
[Options]
variable: ncq_composite_user={{ncq_nc_username}}~{{ncq_root_folder_id}}
[BasicAuth]
{{ncq_composite_user}}: {{ncq_nc_password}}

HTTP 207
[Asserts]
xpath "number(//*[local-name()='quota-used-bytes'])" == 0
xpath "number(//*[local-name()='quota-available-bytes'])" == 500


# ─────────────────────────────────────────────────────────────
# Step 5 — PUT a file into the shared drive; quota-used-bytes
#          must reflect it, quota-available-bytes must shrink.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{ncq_nc_username}}~{{ncq_root_folder_id}}/quota-probe.txt
Content-Type: text/plain
[Options]
variable: ncq_composite_user={{ncq_nc_username}}~{{ncq_root_folder_id}}
[BasicAuth]
{{ncq_composite_user}}: {{ncq_nc_password}}
```
32-byte-ish payload for nc
```

HTTP 201


# The drive-usage bump is fire-and-forget on a tokio task (see
# `file_upload_service.rs::maybe_update_storage_usage`), so retry
# until `used_bytes` catches up — same shape as `drive_quota.hurl`
# Step 5.
PROPFIND {{base_url}}/remote.php/dav/files/{{ncq_nc_username}}~{{ncq_root_folder_id}}/
Depth: 0
[Options]
variable: ncq_composite_user={{ncq_nc_username}}~{{ncq_root_folder_id}}
retry: 10
retry-interval: 200ms
[BasicAuth]
{{ncq_composite_user}}: {{ncq_nc_password}}

HTTP 207
[Asserts]
xpath "number(//*[local-name()='quota-used-bytes'])" > 0
xpath "number(//*[local-name()='quota-available-bytes'])" < 500


# No further cleanup needed — `tests/api/storage_cleanup_check.sh`
# drains/deletes every non-admin-default drive at suite end.
