# =============================================================
# Links — shared-drive membership
# =============================================================
# A drive member gets `Shares/Team`. Deleting that link declines
# it while the drive stays reachable (drive list, @drive);
# POST /api/links restores it. Removing the member deletes link
# and decline, so re-adding links again.
# =============================================================


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"
admin_id: jsonpath "$.user.full.user.id"


POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "dr_bob", "password": "DrBobPassword1!", "email": "dr_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dr_bob", "password": "DrBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"

GET {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}

HTTP 200
[Captures]
bob_home: jsonpath "$[0].id"


POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "Team",
  "owner": { "type": "user", "id": "{{admin_id}}" }
}

HTTP 201
[Captures]
team_id: jsonpath "$.id"
team_root: jsonpath "$.root_folder_id"

POST {{base_url}}/api/drives/{{team_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{bob_id}}" },
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
[Asserts]
jsonpath "$.items[*].resource.name" contains "Shares"

GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Captures]
team_link_id: jsonpath "$.items[?(@.resource.name=='Team')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Team"


# ─────────────────────────────────────────────────────────────
# Deleting the drive link declines it; the drive stays reachable.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{team_link_id}}
Authorization: Bearer {{bob_token}}

HTTP 204


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "Team"


GET {{base_url}}/api/grants/incoming/resources?resource_types=drive
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{team_id}}')].link.declined" == true


GET {{base_url}}/api/drives
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" contains "{{team_id}}"


PROPFIND {{base_url}}/webdav/@drive/{{team_id}}/
Authorization: Bearer {{bob_token}}
Depth: 1

HTTP 207


# ─────────────────────────────────────────────────────────────
# POST /api/links restores it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/links
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "target_id": "{{team_root}}" }

HTTP 201
[Captures]
team_link_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "Team"


GET {{base_url}}/api/grants/incoming/resources?resource_types=drive
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{team_id}}')].link.declined" == false
jsonpath "$.items[?(@.resource.id=='{{team_id}}')].link.link_id" == "{{team_link_id}}"


# ─────────────────────────────────────────────────────────────
# Declined again, then the member is removed: link and decline
# are gone, so re-adding the member links again.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{team_link_id}}
Authorization: Bearer {{bob_token}}

HTTP 204

DELETE {{base_url}}/api/drives/{{team_id}}/members/user/{{bob_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


GET {{base_url}}/api/drives
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" not contains "{{team_id}}"


# `DrMarker` is granted after the removal; once it is linked the removal has been settled.
GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
admin_home: jsonpath "$[0].id"

POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "DrMarker", "parent_id": "{{admin_home}}" }

HTTP 201
[Captures]
marker_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{marker_id}}" },
  "role": "viewer"
}

HTTP 201

GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "DrMarker"
jsonpath "$.items[*].resource.name" not contains "Team"


POST {{base_url}}/api/drives/{{team_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{bob_id}}" },
  "role": "viewer"
}

HTTP 201


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Captures]
team_link_id: jsonpath "$.items[?(@.resource.name=='Team')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Team"


GET {{base_url}}/api/grants/incoming/resources?resource_types=drive
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{team_id}}')].link.declined" == false


# ─────────────────────────────────────────────────────────────
# The member is removed while the link is live: the link row is
# deleted, so re-adding him creates a new one.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/drives/{{team_id}}/members/user/{{bob_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "DrMarker2", "parent_id": "{{admin_home}}" }

HTTP 201
[Captures]
marker2_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{marker2_id}}" },
  "role": "viewer"
}

HTTP 201

GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "DrMarker2"
jsonpath "$.items[*].resource.name" not contains "Team"


POST {{base_url}}/api/drives/{{team_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{bob_id}}" },
  "role": "viewer"
}

HTTP 201


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Team"
jsonpath "$.items[?(@.resource.name=='Team')].resource.id" != "{{team_link_id}}"


# ─────────────────────────────────────────────────────────────
# NextCloud DAV through the drive link: roles and the drive's own quota.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "label": "links 40_drives" }

HTTP 200
[Captures]
dr_nc_password: jsonpath "$.password"


# Viewer member: NC PUT through the link is denied.
PUT {{base_url}}/remote.php/dav/files/dr_bob/Shares/Team/viewer.txt
[BasicAuth]
dr_bob: {{dr_nc_password}}
```
viewer write
```

HTTP 403


POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "NcQuota",
  "owner": { "type": "user", "id": "{{admin_id}}" },
  "quota_bytes": 500
}

HTTP 201
[Captures]
ncq_id: jsonpath "$.id"

POST {{base_url}}/api/drives/{{ncq_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{bob_id}}" },
  "role": "editor"
}

HTTP 201


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "NcQuota"


# The link reports the target drive's quota, not Bob's personal envelope.
PROPFIND {{base_url}}/remote.php/dav/files/dr_bob/Shares/NcQuota/
Depth: 0
[BasicAuth]
dr_bob: {{dr_nc_password}}

HTTP 207
[Asserts]
xpath "number(//*[local-name()='quota-used-bytes'])" == 0
xpath "number(//*[local-name()='quota-available-bytes'])" == 500


# Editor member: NC PUT through the link succeeds.
PUT {{base_url}}/remote.php/dav/files/dr_bob/Shares/NcQuota/quota-probe.txt
[BasicAuth]
dr_bob: {{dr_nc_password}}
```
32-byte-ish payload for nc
```

HTTP 201


# Drive usage is bumped by a detached task; retry until it lands.
PROPFIND {{base_url}}/remote.php/dav/files/dr_bob/Shares/NcQuota/
Depth: 0
[Options]
retry: 10
retry-interval: 200ms
[BasicAuth]
dr_bob: {{dr_nc_password}}

HTTP 207
[Asserts]
xpath "number(//*[local-name()='quota-used-bytes'])" > 0
xpath "number(//*[local-name()='quota-available-bytes'])" < 500


# Chunked upload into a missing folder under the drive link: 409, nothing lands at the drive root.
MKCOL {{base_url}}/remote.php/dav/uploads/dr_bob/drive-missing-parent
[BasicAuth]
dr_bob: {{dr_nc_password}}

HTTP 201


PUT {{base_url}}/remote.php/dav/uploads/dr_bob/drive-missing-parent/00001
[BasicAuth]
dr_bob: {{dr_nc_password}}
`orphan`

HTTP 201


MOVE {{base_url}}/remote.php/dav/uploads/dr_bob/drive-missing-parent/.file
Destination: {{base_url}}/remote.php/dav/files/dr_bob/Shares/NcQuota/Nope/orphan.bin
[BasicAuth]
dr_bob: {{dr_nc_password}}

HTTP 409


PROPFIND {{base_url}}/remote.php/dav/files/dr_bob/Shares/NcQuota/
Depth: 1
[BasicAuth]
dr_bob: {{dr_nc_password}}

HTTP 207
[Asserts]
xpath "count(//*[local-name()='href'][contains(text(), 'orphan.bin')])" == 0
