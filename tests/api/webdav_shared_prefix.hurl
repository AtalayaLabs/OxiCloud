# =============================================================
# OxiCloud — native WebDAV /webdav/@shared/<uuid> address
# =============================================================
# Any readable folder or file is reachable by UUID; the UUID segment
# itself is an address (MOVE/DELETE/PROPPATCH/MKCOL on it -> 403).
# Self-contained: own users shp_alice and shp_bob.
# =============================================================

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "shp_alice", "password": "ShpAlicePass1!", "email": "shp_alice@example.com", "role": "user" }

HTTP 201
[Captures]
alice_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "shp_bob", "password": "ShpBobPass1!", "email": "shp_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "shp_alice", "password": "ShpAlicePass1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "shp_bob", "password": "ShpBobPass1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"

GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home_id: jsonpath "$[0].id"

# ── Alice's content ─────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Reports", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
reports_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Private", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
alice_private_id: jsonpath "$.id"

PUT {{base_url}}/webdav/Reports/q1.txt
Authorization: Bearer {{alice_token}}
Content-Type: text/plain
```
quarter one
```

HTTP 201

POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{alice_home_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
loose_id: jsonpath "$.id"

PUT {{base_url}}/api/files/{{loose_id}}/rename
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "loose.txt" }

HTTP 200

# ── Grants: viewer on the folder and on the loose file ──────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_user_id}}" },
  "resource": { "type": "folder", "id": "{{reports_id}}" },
  "role": "viewer"
}

HTTP 201
[Captures]
reports_grant_id: jsonpath "$.grants[0].id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "file", "id": "{{loose_id}}" },
  "role": "viewer"
}

HTTP 201

# ── Bob reads through the address ───────────────────────────
PROPFIND {{base_url}}/webdav/@shared/{{reports_id}}/
Authorization: Bearer {{bob_token}}
Depth: 1

HTTP 207
[Asserts]
body contains "/webdav/@shared/{{reports_id}}/"
body contains "q1.txt"
body not contains "/webdav/Reports/"

GET {{base_url}}/webdav/@shared/{{reports_id}}/q1.txt
Authorization: Bearer {{bob_token}}

HTTP 200

GET {{base_url}}/webdav/@shared/{{loose_id}}
Authorization: Bearer {{bob_token}}

HTTP 200

GET {{base_url}}/webdav/@shared/{{loose_id}}/x
Authorization: Bearer {{bob_token}}

HTTP 404

# ── The UUID segment is an address ──────────────────────────
MOVE {{base_url}}/webdav/@shared/{{reports_id}}
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@shared/{{reports_id}}-x

HTTP 403

DELETE {{base_url}}/webdav/@shared/{{reports_id}}
Authorization: Bearer {{bob_token}}

HTTP 403

PROPPATCH {{base_url}}/webdav/@shared/{{reports_id}}
Authorization: Bearer {{bob_token}}
Content-Type: application/xml
```
<?xml version="1.0"?>
<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:x"><D:set><D:prop><Z:p>v</Z:p></D:prop></D:set></D:propertyupdate>
```

HTTP 403

MKCOL {{base_url}}/webdav/@shared/{{reports_id}}
Authorization: Bearer {{bob_token}}

HTTP 403

# Below the segment: ordinary authorisation (viewer cannot create).
MKCOL {{base_url}}/webdav/@shared/{{reports_id}}/sub
Authorization: Bearer {{bob_token}}

HTTP 403

# Destination side of MOVE and COPY, and COPY source.
PUT {{base_url}}/webdav/shp_own.txt
Authorization: Bearer {{bob_token}}
Content-Type: text/plain
```
mine
```

HTTP 201

MOVE {{base_url}}/webdav/shp_own.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@shared/{{reports_id}}

HTTP 403

COPY {{base_url}}/webdav/shp_own.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@shared/{{reports_id}}

HTTP 403

COPY {{base_url}}/webdav/@shared/{{reports_id}}
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/shp_copy

HTTP 403

# Dot segments cannot climb out of the shared folder.
GET {{base_url}}/webdav/@shared/{{reports_id}}/../Private/x
Authorization: Bearer {{bob_token}}
[Options]
path-as-is: true

# Raw and encoded dot segments are refused before the handler.
HTTP 400

GET {{base_url}}/webdav/@shared/{{reports_id}}/%2e%2e/Private/x
Authorization: Bearer {{bob_token}}
[Options]
path-as-is: true

HTTP 400

# ── Unreadable and non-existent are indistinguishable ───────
GET {{base_url}}/webdav/@shared/{{alice_private_id}}/
Authorization: Bearer {{bob_token}}

HTTP 404

GET {{base_url}}/webdav/@shared/00000000-0000-0000-0000-000000000000/
Authorization: Bearer {{bob_token}}

HTTP 404

GET {{base_url}}/webdav/@shared/not-a-uuid/
Authorization: Bearer {{bob_token}}

HTTP 404

# ── Listing the pseudo-root ─────────────────────────────────
PROPFIND {{base_url}}/webdav/@shared/
Authorization: Bearer {{bob_token}}
Depth: 1

HTTP 207
[Asserts]
body contains "{{reports_id}}"
body contains "{{loose_id}}"
body contains "<D:displayname>Reports</D:displayname>"
body not contains "{{alice_private_id}}"

PROPFIND {{base_url}}/webdav/@shared/
Authorization: Bearer {{bob_token}}
Depth: 0

HTTP 207
[Asserts]
xpath "count(//*[local-name()='response'])" == 1

# No trailing slash: member hrefs keep the separator.
PROPFIND {{base_url}}/webdav/@shared
Authorization: Bearer {{bob_token}}
Depth: 1

HTTP 207
[Asserts]
body contains "/webdav/@shared/{{reports_id}}/"
body contains "/webdav/@shared/{{loose_id}}<"

MKCOL {{base_url}}/webdav/@shared/
Authorization: Bearer {{bob_token}}

HTTP 405

# ── Revoke ends access ──────────────────────────────────────
DELETE {{base_url}}/api/grants/{{reports_grant_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

GET {{base_url}}/webdav/@shared/{{reports_id}}/q1.txt
Authorization: Bearer {{bob_token}}

HTTP 404
