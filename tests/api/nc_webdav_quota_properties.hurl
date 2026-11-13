# =============================================================
# OxiCloud — NC WebDAV quota properties (RFC 4331)
# =============================================================
# The NextCloud-compatible WebDAV surface
# (`interfaces/nextcloud/webdav_handler.rs`) previously had NO
# `d:quota-used-bytes`/`d:quota-available-bytes` support at all. This
# pins the new coverage added alongside the native surface's
# drive-awareness fix (`AppState::resolve_webdav_quota`):
#
#   Personal-drive PROPFIND reports the caller's account envelope.
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


# No further cleanup needed — `tests/api/storage_cleanup_check.sh`
# drains/deletes every non-admin-default drive at suite end.
