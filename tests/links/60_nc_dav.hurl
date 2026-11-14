# =============================================================
# Links — NextCloud-compatible DAV
# =============================================================
# lk_bob's link `Shares/Reports` is a path under
# /remote.php/dav/files/lk_bob/; `@shared` is native-only.
# =============================================================


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "lk_alice", "password": "LkAlicePassword1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "lk_bob", "password": "LkBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"
bob_id: jsonpath "$.user.full.user.id"


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


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "label": "links 60_nc_dav" }

HTTP 200
[Captures]
nc_password: jsonpath "$.password"


PROPFIND {{base_url}}/remote.php/dav/files/lk_bob/Shares/
Depth: 1
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 207
[Asserts]
xpath "//*[local-name()='response']/*[local-name()='href' and contains(text(), '/Shares/Reports/')]" exists


# ETags across the link: Alice's upload changes Bob's root and Shares etags; the link reports the target's.
PROPFIND {{base_url}}/remote.php/dav/files/lk_bob/
Depth: 0
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 207
[Captures]
bob_root_etag: xpath "string(//*[local-name()='getetag'])"

PROPFIND {{base_url}}/remote.php/dav/files/lk_bob/Shares/
Depth: 0
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 207
[Captures]
bob_shares_etag: xpath "string(//*[local-name()='getetag'])"

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
PROPFIND {{base_url}}/remote.php/dav/files/lk_bob/
Depth: 0
[BasicAuth]
lk_bob: {{nc_password}}
[Options]
retry: 20
retry-interval: 500

HTTP 207
[Asserts]
xpath "string(//*[local-name()='getetag'])" != "{{bob_root_etag}}"

PROPFIND {{base_url}}/remote.php/dav/files/lk_bob/Shares/
Depth: 1
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 207
[Asserts]
xpath "string(//*[local-name()='response'][*[local-name()='href' and text()='/remote.php/dav/files/lk_bob/Shares/']]//*[local-name()='getetag'])" != "{{bob_shares_etag}}"
xpath "string(//*[local-name()='response'][*[local-name()='href' and text()='/remote.php/dav/files/lk_bob/Shares/Reports/']]//*[local-name()='getetag'])" == "{{reports_etag}}"

PROPFIND {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/
Depth: 0
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 207
[Asserts]
xpath "string(//*[local-name()='getetag'])" == "{{reports_etag}}"


PROPFIND {{base_url}}/remote.php/dav/files/lk_bob/@shared/
Depth: 1
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 404


GET {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/q1.txt
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 200
[Asserts]
body contains "q1 figures"


PROPFIND {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/
Depth: 1
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 207
[Asserts]
xpath "//*[local-name()='response']/*[local-name()='href' and text()='/remote.php/dav/files/lk_bob/Shares/Reports/q1.txt']" exists
xpath "//*[local-name()='response'][*[local-name()='href' and text()='/remote.php/dav/files/lk_bob/Shares/Reports/']]//*[local-name()='displayname' and text()='Reports']" exists
xpath "count(//*[local-name()='href'][not(starts-with(text(), '/remote.php/dav/files/lk_bob/Shares/Reports/'))])" == 0


PUT {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/new.txt
[BasicAuth]
lk_bob: {{nc_password}}
```
viewer write
```

HTTP 403


# Viewer: overwriting an existing file is 403, not 500.
PUT {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/q1.txt
[BasicAuth]
lk_bob: {{nc_password}}
```
viewer overwrite
```

HTTP 403


PATCH {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/q1.txt
X-Update-Range: bytes=0-2
Content-Type: text/plain
[BasicAuth]
lk_bob: {{nc_password}}
`NOP`

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


PUT {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/new.txt
[BasicAuth]
lk_bob: {{nc_password}}
```
editor write
```

HTTP 201


# Editor: PATCH and overwrite of an existing file succeed.
PATCH {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/q1.txt
X-Update-Range: bytes=0-2
Content-Type: text/plain
[BasicAuth]
lk_bob: {{nc_password}}
`XYZ`

HTTP 204


GET {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/q1.txt
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 200
[Asserts]
body startsWith "XYZfigures"


PUT {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/q1.txt
[BasicAuth]
lk_bob: {{nc_password}}
```
q1 figures
```

HTTP 204


MKCOL {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/Sub
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 201


GET {{base_url}}/api/folders/{{reports_id}}/resources
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "new.txt"
jsonpath "$.items[*].resource.name" contains "Sub"


MOVE {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/new.txt
Destination: {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/Sub/moved.txt
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 201


GET {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/Sub/moved.txt
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 200
[Asserts]
body contains "editor write"


# Editor has no Delete on the target.
DELETE {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/Sub/moved.txt
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 403


GET {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/Sub/moved.txt
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 200


# Chunked upload into the link subtree.
MKCOL {{base_url}}/remote.php/dav/uploads/lk_bob/link-chunked
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 201


PUT {{base_url}}/remote.php/dav/uploads/lk_bob/link-chunked/00001
[BasicAuth]
lk_bob: {{nc_password}}
`chunk one `

HTTP 201


PUT {{base_url}}/remote.php/dav/uploads/lk_bob/link-chunked/00002
[BasicAuth]
lk_bob: {{nc_password}}
`chunk two`

HTTP 201


MOVE {{base_url}}/remote.php/dav/uploads/lk_bob/link-chunked/.file
Destination: {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/chunked.bin
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 201


GET {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/chunked.bin
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 200
[Asserts]
body == "chunk one chunk two"


GET {{base_url}}/webdav/Reports/chunked.bin
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
body == "chunk one chunk two"


# Href identity: Bob renames his link and moves it to his root.
MOVE {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports
Destination: {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports2026
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 201


MOVE {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports2026
Destination: {{base_url}}/remote.php/dav/files/lk_bob/Reports2026
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 201


PUT {{base_url}}/remote.php/dav/files/lk_bob/Reports2026/after.txt
[BasicAuth]
lk_bob: {{nc_password}}
```
after rename
```

HTTP 201


PROPFIND {{base_url}}/remote.php/dav/files/lk_bob/Reports2026/
Depth: 1
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 207
[Asserts]
xpath "//*[local-name()='response']/*[local-name()='href' and text()='/remote.php/dav/files/lk_bob/Reports2026/after.txt']" exists
xpath "//*[local-name()='response']/*[local-name()='href' and text()='/remote.php/dav/files/lk_bob/Reports2026/chunked.bin']" exists
xpath "//*[local-name()='response'][*[local-name()='href' and text()='/remote.php/dav/files/lk_bob/Reports2026/']]//*[local-name()='displayname' and text()='Reports2026']" exists
xpath "count(//*[local-name()='href'][not(starts-with(text(), '/remote.php/dav/files/lk_bob/Reports2026/'))])" == 0


GET {{base_url}}/remote.php/dav/files/lk_bob/Shares/Reports/q1.txt
[BasicAuth]
lk_bob: {{nc_password}}

HTTP 404
