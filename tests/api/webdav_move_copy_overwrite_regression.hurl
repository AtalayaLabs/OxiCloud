# =============================================================
# OxiCloud — WebDAV MOVE/COPY must not delete before it can succeed (#906)
# =============================================================
# `Overwrite: T` deleted the destination before resolving the source, so a
# failing MOVE/COPY had already lost it. Each pin checks the destination's
# bytes afterwards (the status was 404 before too). Fixtures: `regress-906/`.
# =============================================================


# ── Setup ────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{token}}
Content-Type: application/json
{ "label": "webdav_move_copy_overwrite_regression" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


DELETE {{base_url}}/webdav/regress-906/
Authorization: Bearer {{token}}

HTTP *


MKCOL {{base_url}}/webdav/regress-906/
Authorization: Bearer {{token}}

HTTP 201


PUT {{base_url}}/webdav/regress-906/keep.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
`keep`

HTTP 201


MKCOL {{base_url}}/webdav/regress-906/f/
Authorization: Bearer {{token}}

HTTP 201


MKCOL {{base_url}}/webdav/regress-906/f/sub/
Authorization: Bearer {{token}}

HTTP 201


PUT {{base_url}}/webdav/regress-906/f/inner.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
`inner`

HTTP 201


# ── Native: missing source must not cost the destination ────
COPY {{base_url}}/webdav/regress-906/nope.txt
Authorization: Bearer {{token}}
Destination: {{base_url}}/webdav/regress-906/keep.txt
Overwrite: T

HTTP 404

MOVE {{base_url}}/webdav/regress-906/nope.txt
Authorization: Bearer {{token}}
Destination: {{base_url}}/webdav/regress-906/keep.txt
Overwrite: T

HTTP 404

GET {{base_url}}/webdav/regress-906/keep.txt
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body == "keep"


# ── Native: onto own parent (the issue's folder wipe) ───────
# Overwriting `f/` deletes `f/sub/` with it; must be refused up front.
MOVE {{base_url}}/webdav/regress-906/f/sub/
Authorization: Bearer {{token}}
Destination: {{base_url}}/webdav/regress-906/f/
Overwrite: T

HTTP 409

COPY {{base_url}}/webdav/regress-906/f/sub/
Authorization: Bearer {{token}}
Destination: {{base_url}}/webdav/regress-906/f/
Overwrite: T

HTTP 409

# ── Native: into own subtree ────────────────────────────────
MOVE {{base_url}}/webdav/regress-906/f/
Authorization: Bearer {{token}}
Destination: {{base_url}}/webdav/regress-906/f/sub/f/
Overwrite: T

HTTP 409

GET {{base_url}}/webdav/regress-906/f/inner.txt
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body == "inner"


# ── Native: to itself (trailing slash differs) ──────────────
COPY {{base_url}}/webdav/regress-906/f
Authorization: Bearer {{token}}
Destination: {{base_url}}/webdav/regress-906/f/
Overwrite: T

HTTP 403

GET {{base_url}}/webdav/regress-906/f/inner.txt
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body == "inner"


# ── Native allowed path: a real overwrite still works ───────
PUT {{base_url}}/webdav/regress-906/src.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
`new`

HTTP 201

MOVE {{base_url}}/webdav/regress-906/src.txt
Authorization: Bearer {{token}}
Destination: {{base_url}}/webdav/regress-906/keep.txt
Overwrite: T

HTTP 204

GET {{base_url}}/webdav/regress-906/keep.txt
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body == "new"


# ── NextCloud DAV: missing source ───────────────────────────
MOVE {{base_url}}/remote.php/dav/files/{{username}}/regress-906/nope.txt
Destination: {{base_url}}/remote.php/dav/files/{{username}}/regress-906/keep.txt
Overwrite: T
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 404

GET {{base_url}}/remote.php/dav/files/{{username}}/regress-906/keep.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 200
[Asserts]
body == "new"


# ── NextCloud DAV: folder onto itself (issue repro: wiped) ──
MOVE {{base_url}}/remote.php/dav/files/{{username}}/regress-906/f
Destination: {{base_url}}/remote.php/dav/files/{{username}}/regress-906/f/
Overwrite: T
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 403

# ── NextCloud DAV: onto own parent ──────────────────────────
MOVE {{base_url}}/remote.php/dav/files/{{username}}/regress-906/f/sub
Destination: {{base_url}}/remote.php/dav/files/{{username}}/regress-906/f
Overwrite: T
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 409

GET {{base_url}}/remote.php/dav/files/{{username}}/regress-906/f/inner.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 200
[Asserts]
body == "inner"


# ── Teardown ─────────────────────────────────────────────────
DELETE {{base_url}}/webdav/regress-906/
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{token}}

HTTP 200
