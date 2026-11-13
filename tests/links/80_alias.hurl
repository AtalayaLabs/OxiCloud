# =============================================================
# Links — MOVE/COPY across two aliases of one resource
# =============================================================
# al_bob holds owner grants on al_alice's `Team` and on `Team/Docs`,
# so `Shares/Team/Docs` and `Shares/Docs` name the same folder.
# A MOVE/COPY whose source and destination overlap through the
# links is 403 and nothing is deleted.
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
{ "username": "al_alice", "password": "AlAlicePassword1!", "email": "al_alice@example.com", "role": "user" }

HTTP 201

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "al_bob", "password": "AlBobPassword1!", "email": "al_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "al_alice", "password": "AlAlicePassword1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "al_bob", "password": "AlBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


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
{ "name": "Team", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
team_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Docs", "parent_id": "{{team_id}}" }

HTTP 201
[Captures]
docs_id: jsonpath "$.id"

PUT {{base_url}}/webdav/Team/Docs/a.txt
Authorization: Bearer {{alice_token}}
Content-Type: text/plain
```
alias content
```

HTTP 201

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{team_id}}" },
  "role": "owner"
}

HTTP 201

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{docs_id}}" },
  "role": "owner"
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
jsonpath "$.items[*].resource.name" contains "Team"
jsonpath "$.items[*].resource.name" contains "Docs"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "label": "links 80_alias" }

HTTP 200
[Captures]
nc_password: jsonpath "$.password"


# ─────────────────────────────────────────────────────────────
# NextCloud DAV
# ─────────────────────────────────────────────────────────────
MOVE {{base_url}}/remote.php/dav/files/al_bob/Shares/Team/Docs/a.txt
Destination: {{base_url}}/remote.php/dav/files/al_bob/Shares/Docs/a.txt
Overwrite: T
[BasicAuth]
al_bob: {{nc_password}}

HTTP 403


MOVE {{base_url}}/remote.php/dav/files/al_bob/Shares/Docs
Destination: {{base_url}}/remote.php/dav/files/al_bob/Shares/Team/Docs
Overwrite: T
[BasicAuth]
al_bob: {{nc_password}}

HTTP 403


MOVE {{base_url}}/remote.php/dav/files/al_bob/Shares/Team/Docs/a.txt
Destination: {{base_url}}/remote.php/dav/files/al_bob/Shares/Docs
Overwrite: T
[BasicAuth]
al_bob: {{nc_password}}

HTTP 403


GET {{base_url}}/remote.php/dav/files/al_bob/Shares/Docs/a.txt
[BasicAuth]
al_bob: {{nc_password}}

HTTP 200
[Asserts]
body contains "alias content"


# ─────────────────────────────────────────────────────────────
# Native WebDAV
# ─────────────────────────────────────────────────────────────
MOVE {{base_url}}/webdav/Shares/Team/Docs/a.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Shares/Docs/a.txt
Overwrite: T

HTTP 403


MOVE {{base_url}}/webdav/Shares/Docs
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Shares/Team/Docs
Overwrite: T

HTTP 403


MOVE {{base_url}}/webdav/Shares/Team
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Shares/Docs/Team
Overwrite: T

HTTP 403


MOVE {{base_url}}/webdav/Shares/Team/Docs/a.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Shares/Docs
Overwrite: T

HTTP 403


COPY {{base_url}}/webdav/Shares/Team/Docs/a.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Shares/Docs/a.txt
Overwrite: T

HTTP 403


COPY {{base_url}}/webdav/@shared/{{docs_id}}/a.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Shares/Docs/a.txt
Overwrite: T

HTTP 403


COPY {{base_url}}/webdav/Shares/Docs
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Shares/Team/Docs
Overwrite: T

HTTP 403


# ─────────────────────────────────────────────────────────────
# An empty path segment in the Destination names the same resource.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/webdav/Mine
Authorization: Bearer {{bob_token}}

HTTP 201

PUT {{base_url}}/webdav/Mine/b.txt
Authorization: Bearer {{bob_token}}
Content-Type: text/plain
```
own content
```

HTTP 201


MOVE {{base_url}}/webdav/Mine/b.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Mine//b.txt
Overwrite: T

HTTP 403


COPY {{base_url}}/webdav/Mine/b.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/Mine//b.txt
Overwrite: T

HTTP 403


MOVE {{base_url}}/remote.php/dav/files/al_bob/Mine/b.txt
Destination: {{base_url}}/remote.php/dav/files/al_bob/Mine//b.txt
Overwrite: T
[BasicAuth]
al_bob: {{nc_password}}

HTTP 403


GET {{base_url}}/webdav/Mine/b.txt
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
body contains "own content"


COPY {{base_url}}/webdav/Shares/Docs/a.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@shared/{{team_id}}/Docs//a.txt
Overwrite: T

HTTP 403


MOVE {{base_url}}/webdav/Shares/Docs/a.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@shared/{{team_id}}/Docs//a.txt
Overwrite: T

HTTP 403


MOVE {{base_url}}/remote.php/dav/files/al_bob/Shares/Docs/a.txt
Destination: {{base_url}}/remote.php/dav/files/al_bob/Shares/Team/Docs//a.txt
Overwrite: T
[BasicAuth]
al_bob: {{nc_password}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Nothing was deleted: the file, its folder and both links stand.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/webdav/Team/Docs/a.txt
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
body contains "alias content"


GET {{base_url}}/api/folders/{{team_id}}/resources
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Docs"


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Team"
jsonpath "$.items[*].resource.name" contains "Docs"
