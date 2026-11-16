# =============================================================
# OxiCloud — `%2F` / `%5C` inside a WebDAV path segment (#315)
# =============================================================
# The DAV handlers percent-decoded the WHOLE path before splitting on
# `/`, so `a%2Fb` silently became the nested path `a/b` (MKCOL/PUT
# landed in a sub-folder, MOVE/COPY Destination likewise). Names can't
# hold `/` or `\` (path_service::validate_storage_name), so the fix
# refuses such a segment with 400. On the NC surface axum's `Path`
# decodes the wildcard too, hence the raw-uri guard in
# nextcloud/routes.rs::reject_encoded_separators.
#
# Coverage:
#   1. /webdav PUT with `%2F` and `%5c` in a name → 400.
#   2. /webdav MOVE with `%2F` in the Destination → 400.
#   3. NC /remote.php/dav/files PUT with `%2F` → 400.
#   4. NC MOVE with `%2F` in the Destination → 400.
#   5. `%20` still decodes (PUT 201) — the guard is not over-broad.
# =============================================================


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{token}}
Content-Type: application/json
{ "label": "webdav_encoded_slash hurl test" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# ── Step 1 — /webdav: encoded separators in a name are refused
PUT {{base_url}}/webdav/enc-slash-a%2Fb.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
`x`

HTTP 400


PUT {{base_url}}/webdav/enc-slash-a%5cb.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
`x`

HTTP 400


# ── Step 5 (setup for MOVE) — plain escapes still work
PUT {{base_url}}/webdav/enc-slash%20probe.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
`x`

HTTP 201


# ── Step 2 — /webdav MOVE Destination with `%2F`
MOVE {{base_url}}/webdav/enc-slash%20probe.txt
Authorization: Bearer {{token}}
Destination: {{base_url}}/webdav/enc-slash-x%2Fy.txt

HTTP 400


# ── Step 3 — NC surface: request path with `%2F`
PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/enc-slash-nc-a%2Fb.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`x`

HTTP 400


# ── Step 4 — NC MOVE Destination with `%2F`
MOVE {{base_url}}/remote.php/dav/files/{{nc_username}}/enc-slash%20probe.txt
Destination: {{base_url}}/remote.php/dav/files/{{nc_username}}/enc-slash-x%2Fy.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 400


# ── Cleanup
DELETE {{base_url}}/webdav/enc-slash%20probe.txt
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{token}}
HTTP 200
