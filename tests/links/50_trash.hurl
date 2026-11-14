# =============================================================
# Links — trashed target
# =============================================================
# Trashing the target hides tr_bob's link; a membership event on
# the same pair while it is trashed does not delete it; restoring
# the target shows the link again.
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
{ "username": "tr_alice", "password": "TrAlicePassword1!", "email": "tr_alice@example.com", "role": "user" }

HTTP 201

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "tr_bob", "password": "TrBobPassword1!", "email": "tr_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "tr_alice", "password": "TrAlicePassword1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "tr_bob", "password": "TrBobPassword1!" }

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


# ─────────────────────────────────────────────────────────────
# `Trashy` granted to Bob and to an empty group → Bob is linked.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "tr-team", "description": "links trash" }

HTTP 201
[Captures]
group_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Trashy", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
trashy_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{trashy_id}}" },
  "role": "viewer"
}

HTTP 201

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{group_id}}" },
  "resource": { "type": "folder", "id": "{{trashy_id}}" },
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
trashy_link_id: jsonpath "$.items[?(@.resource.name=='Trashy')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Trashy"


# ─────────────────────────────────────────────────────────────
# Alice trashes `Trashy` → the link is hidden.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{trashy_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "Trashy"

PROPFIND {{base_url}}/webdav/Shares/
Authorization: Bearer {{bob_token}}
Depth: 1

HTTP 207
[Asserts]
xpath "count(//*[local-name()='href'][contains(text(), 'Trashy')])" == 0

GET {{base_url}}/api/folders/{{trashy_link_id}}
Authorization: Bearer {{bob_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Bob joins the group holding a grant on `Trashy`; `TrMarker`,
# granted afterwards, shows when that event has been settled.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups/{{group_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "user_id": "{{bob_id}}" }

HTTP 201

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "TrMarker", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
marker_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
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
jsonpath "$.items[*].resource.name" contains "TrMarker"
jsonpath "$.items[*].resource.name" not contains "Trashy"


# ─────────────────────────────────────────────────────────────
# Alice restores `Trashy` → the same link is visible again.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/trash/{{trashy_id}}/restore
Authorization: Bearer {{alice_token}}

HTTP 200


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.name=='Trashy')].resource.id" == "{{trashy_link_id}}"
jsonpath "$.items[*].resource.name" not contains "Trashy (2)"

GET {{base_url}}/api/folders/{{trashy_link_id}}
Authorization: Bearer {{bob_token}}

HTTP 200
