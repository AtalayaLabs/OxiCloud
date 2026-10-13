# =============================================================
# OxiCloud — NextCloud HTTP PATCH partial content updates (RFC 5789)
# =============================================================
# The NextCloud-compatible WebDAV surface (/remote.php/dav/…) had no
# PATCH dispatch arm at all (fell through to 405), unlike the plain-file
# surface (see api/handlers/webdav_handler.rs::handle_patch). See
# nextcloud/webdav_handler.rs::handle_patch, which reuses the plain
# surface's `parse_update_range` and `upload_ingest::
# ingest_range_patch_to_cas` — both surface-agnostic.
#
# Coverage:
#   1. PATCH an explicit byte range (`X-Update-Range: bytes=<start>-<end>`)
#      → 204, Content-Range header, and the resulting content reflects
#      the patched span with the untouched prefix/suffix intact.
#   2. PATCH with `X-Update-Range: append` → 204, content grows.
#   3. PATCH with a Content-Range header → 400 (must use X-Update-Range).
#   4. PATCH without X-Update-Range → 400.
#   5. PATCH on a nonexistent file → 404.
#   6. PATCH on a directory → 409 (not 404 — the NC surface previously
#      had no folder-existence check and returned 404 for both a missing
#      file AND an existing directory; the fix commit added an explicit
#      check so the two cases are distinguishable again, matching the
#      plain-surface behavior).
#
# Hurl gotcha: headers MUST come before section blocks like
# `[BasicAuth]` in a request — a header line placed after `[BasicAuth]`
# is parsed as the START OF A NEW REQUEST instead (see
# `nc_multidrive_move_regression.hurl`'s note on the same gotcha).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup 1 — JWT login (to mint the app password used for NC Basic Auth).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Setup 2 — Mint an app password for NC Basic Auth.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "nc_webdav_patch hurl test" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 1 — Seed a 10-byte probe file: "0123456789".
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`0123456789`

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 2 — PATCH bytes 3-5 ("345") with "XYZ".
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe.txt
X-Update-Range: bytes=3-5
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`XYZ`

HTTP 204
[Asserts]
header "Content-Range" == "bytes 3-9/10"


GET {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 200
[Asserts]
body == "012XYZ6789"
# NC GET surface emits `Last-Modified` as RFC 1123 HTTP-date (RFC 7231
# §7.1.1.1). Alternates ` GMT` and ` +0000` are both accepted by sync
# clients in practice; our emitter uses ` +0000`. A regression to RFC
# 3339 (`2026-10-11T22:13:20Z`) would break NC desktop / Finder /
# Cyberduck here.
header "Last-Modified" matches "^[A-Z][a-z]{2}, [0-9]{1,2} [A-Z][a-z]{2} [0-9]{4} [0-9]{2}:[0-9]{2}:[0-9]{2} (GMT|\\+0000)$"


# ─────────────────────────────────────────────────────────────
# Step 3 — PATCH append.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe.txt
X-Update-Range: append
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`END`

HTTP 204
[Asserts]
header "Content-Range" == "bytes 10-12/13"


GET {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 200
[Asserts]
body == "012XYZ6789END"


# ─────────────────────────────────────────────────────────────
# Step 4 — Content-Range header on PATCH is rejected.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe.txt
X-Update-Range: bytes=0-2
Content-Range: bytes 0-2/13
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`abc`

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 5 — Missing X-Update-Range header.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`abc`

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 6 — PATCH on a nonexistent file → 404.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-does-not-exist.txt
X-Update-Range: append
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`abc`

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 7 — PATCH on a directory → 409 Conflict (not 404).
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe-dir/
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


PATCH {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe-dir/
X-Update-Range: bytes=0-2
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`NOP`

HTTP 409


DELETE {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe-dir/
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Cleanup
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-patch-probe.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}
HTTP 200
