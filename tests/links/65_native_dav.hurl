# =============================================================
# Links — native WebDAV
# =============================================================
# nd_bob's link `Shares/Reports` is walked through under /webdav/;
# hrefs stay on the requested path, writes follow his role on the target.
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
{ "username": "nd_alice", "password": "NdAlicePassword1!", "email": "nd_alice@example.com", "role": "user" }

HTTP 201

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "nd_bob", "password": "NdBobPassword1!", "email": "nd_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "nd_carol", "password": "NdCarolPassword1!", "email": "nd_carol@example.com", "role": "user" }

HTTP 201
[Captures]
carol_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "nd_alice", "password": "NdAlicePassword1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "nd_bob", "password": "NdBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "nd_carol", "password": "NdCarolPassword1!" }

HTTP 200
[Captures]
carol_token: jsonpath "$.access_token"


GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home: jsonpath "$[0].id"

GET {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}

HTTP 200
[Captures]
bob_home: jsonpath "$[0].id"


POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Reports", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
reports_id: jsonpath "$.id"

PUT {{base_url}}/webdav/Reports/q1.txt
Authorization: Bearer {{alice_token}}
Content-Type: text/plain
```
q1 figures
```

HTTP 201

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{reports_id}}" },
  "role": "viewer"
}

HTTP 201


GET {{base_url}}/api/folders/{{bob_home}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Captures]
shares_id: jsonpath "$.items[?(@.resource.name=='Shares')].resource.id"

GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Reports"


PROPFIND {{base_url}}/webdav/Shares/Reports/
Authorization: Bearer {{bob_token}}
Depth: 1

HTTP 207
[Asserts]
xpath "//*[local-name()='response']/*[local-name()='href' and text()='/webdav/Shares/Reports/q1.txt']" exists
xpath "//*[local-name()='response'][*[local-name()='href' and text()='/webdav/Shares/Reports/']]//*[local-name()='displayname' and text()='Reports']" exists
xpath "count(//*[local-name()='href'][not(starts-with(text(), '/webdav/Shares/Reports/'))])" == 0


GET {{base_url}}/webdav/Shares/Reports/q1.txt
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
body contains "q1 figures"


# ETags across the link: Alice's upload changes Bob's root and Shares etags, not carol's; the link reports the target's.
PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{bob_token}}
Depth: 0

HTTP 207
[Captures]
bob_root_etag: xpath "string(//*[local-name()='getetag'])"

PROPFIND {{base_url}}/webdav/Shares/
Authorization: Bearer {{bob_token}}
Depth: 0

HTTP 207
[Captures]
bob_shares_etag: xpath "string(//*[local-name()='getetag'])"

PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{carol_token}}
Depth: 0

HTTP 207
[Captures]
carol_root_etag: xpath "string(//*[local-name()='getetag'])"

PROPFIND {{base_url}}/webdav/Reports/
Authorization: Bearer {{alice_token}}
Depth: 0

HTTP 207
[Captures]
reports_etag_before: xpath "string(//*[local-name()='getetag'])"

# Folder etags have one-second resolution; the upload must land in a later second than the captures.
PUT {{base_url}}/webdav/Reports/etag.txt
Authorization: Bearer {{alice_token}}
Content-Type: text/plain
[Options]
delay: 1100
```
moves the etag
```

HTTP 201

PROPFIND {{base_url}}/webdav/Reports/
Authorization: Bearer {{alice_token}}
Depth: 0
[Options]
retry: 20
retry-interval: 500

HTTP 207
[Captures]
reports_etag: xpath "string(//*[local-name()='getetag'])"
[Asserts]
xpath "string(//*[local-name()='getetag'])" != "{{reports_etag_before}}"

# Ancestor etags are bumped by the tree-etag flusher after commit; wait for it.
PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{bob_token}}
Depth: 1
[Options]
retry: 20
retry-interval: 500

HTTP 207
[Asserts]
xpath "string(//*[local-name()='response'][*[local-name()='href' and text()='/webdav/']]//*[local-name()='getetag'])" != "{{bob_root_etag}}"
xpath "string(//*[local-name()='response'][*[local-name()='href' and text()='/webdav/Shares/']]//*[local-name()='getetag'])" != "{{bob_shares_etag}}"

PROPFIND {{base_url}}/webdav/Shares/
Authorization: Bearer {{bob_token}}
Depth: 1

HTTP 207
[Asserts]
xpath "string(//*[local-name()='response'][*[local-name()='href' and text()='/webdav/Shares/Reports/']]//*[local-name()='getetag'])" == "{{reports_etag}}"

PROPFIND {{base_url}}/webdav/Shares/Reports/
Authorization: Bearer {{bob_token}}
Depth: 0

HTTP 207
[Asserts]
xpath "string(//*[local-name()='getetag'])" == "{{reports_etag}}"

PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{carol_token}}
Depth: 0

HTTP 207
[Asserts]
xpath "string(//*[local-name()='getetag'])" == "{{carol_root_etag}}"


# R0: carol reaches Bob's `Shares` folder, but his link there is no address for her.
POST {{base_url}}/api/grants
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{carol_id}}" },
  "resource": { "type": "folder", "id": "{{shares_id}}" },
  "role": "viewer"
}

HTTP 201


GET {{base_url}}/webdav/@shared/{{shares_id}}/Reports/q1.txt
Authorization: Bearer {{carol_token}}

HTTP 404
[Captures]
denied_get: body
[Asserts]
body not contains "Folder"


PROPFIND {{base_url}}/webdav/@shared/{{shares_id}}/Reports/
Authorization: Bearer {{carol_token}}
Depth: 0

HTTP 404
[Captures]
denied_propfind: body


PROPFIND {{base_url}}/webdav/@shared/{{shares_id}}/
Authorization: Bearer {{carol_token}}
Depth: 1

HTTP 207
[Asserts]
xpath "count(//*[local-name()='href'][contains(text(), 'Reports')])" == 0


GET {{base_url}}/webdav/Shares/Reports/missing.txt
Authorization: Bearer {{bob_token}}

HTTP 404


PUT {{base_url}}/webdav/Shares/Reports/new.txt
Authorization: Bearer {{bob_token}}
Content-Type: text/plain
```
viewer write
```

HTTP 403


PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{reports_id}}" },
  "role": "editor"
}

HTTP 200


PUT {{base_url}}/webdav/Shares/Reports/new.txt
Authorization: Bearer {{bob_token}}
Content-Type: text/plain
```
editor write
```

HTTP 201


MKCOL {{base_url}}/webdav/Shares/Reports/Sub
Authorization: Bearer {{bob_token}}

HTTP 201


GET {{base_url}}/api/folders/{{reports_id}}/resources
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "new.txt"
jsonpath "$.items[*].resource.name" contains "Sub"


COPY {{base_url}}/webdav/Shares/Reports/new.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/copied.txt

HTTP 201


GET {{base_url}}/webdav/copied.txt
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
body contains "editor write"


MOVE {{base_url}}/webdav/Shares/Reports/new.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Shares/Reports/Sub/moved.txt

HTTP 201


# Editor has no Delete on the target.
DELETE {{base_url}}/webdav/Shares/Reports/Sub/moved.txt
Authorization: Bearer {{bob_token}}

HTTP 403


GET {{base_url}}/webdav/Shares/Reports/Sub/moved.txt
Authorization: Bearer {{bob_token}}

HTTP 200



# Href identity: Bob renames his link and moves it to his root.
MOVE {{base_url}}/webdav/Shares/Reports
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Shares/Reports2026

HTTP 201


MOVE {{base_url}}/webdav/Shares/Reports2026
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Reports2026

HTTP 201


PUT {{base_url}}/webdav/Reports2026/after.txt
Authorization: Bearer {{bob_token}}
Content-Type: text/plain
```
after rename
```

HTTP 201


PROPFIND {{base_url}}/webdav/Reports2026/
Authorization: Bearer {{bob_token}}
Depth: 1

HTTP 207
[Asserts]
xpath "//*[local-name()='response']/*[local-name()='href' and text()='/webdav/Reports2026/after.txt']" exists
xpath "//*[local-name()='response']/*[local-name()='href' and text()='/webdav/Reports2026/q1.txt']" exists
xpath "//*[local-name()='response'][*[local-name()='href' and text()='/webdav/Reports2026/']]//*[local-name()='displayname' and text()='Reports2026']" exists
xpath "count(//*[local-name()='href'][not(starts-with(text(), '/webdav/Reports2026/'))])" == 0


GET {{base_url}}/webdav/Shares/Reports/q1.txt
Authorization: Bearer {{bob_token}}

HTTP 404


# The path that held the link is now a plain miss; carol saw exactly that before.
GET {{base_url}}/webdav/@shared/{{shares_id}}/Reports/q1.txt
Authorization: Bearer {{carol_token}}

HTTP 404
[Asserts]
body == {{denied_get}}


PROPFIND {{base_url}}/webdav/@shared/{{shares_id}}/Reports/
Authorization: Bearer {{carol_token}}
Depth: 0

HTTP 404
[Asserts]
body == {{denied_propfind}}


# R2: DELETE of the link path removes the link, not the target.
DELETE {{base_url}}/webdav/Reports2026
Authorization: Bearer {{bob_token}}

HTTP 204


GET {{base_url}}/api/folders/{{bob_home}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "Reports2026"


GET {{base_url}}/api/folders/{{reports_id}}/resources
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Sub"
jsonpath "$.items[*].resource.name" contains "q1.txt"
jsonpath "$.items[*].resource.name" contains "after.txt"
