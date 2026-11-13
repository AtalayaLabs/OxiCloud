# =============================================================
# Links — seed while OXICLOUD_LINKS_MODE=off
# =============================================================
# lk_alice grants lk_bob `Pre` before links exist. Nothing is
# linked and POST /api/links is not routed. 10_backfill.hurl
# checks that the reboot in auto links `Pre`.
# =============================================================


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.features.links_mode" == "off"


# Deletion keeps running in off (revoke, expiry), so the settle and sweep jobs are registered.
GET {{base_url}}/api/admin/jobs
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$[*].name" contains "links_settle"
jsonpath "$[*].name" contains "links_expiry_sweep"


# ─────────────────────────────────────────────────────────────
# Users shared by the seed, backfill and NC DAV scenarios.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "lk_alice", "password": "LkAlicePassword1!", "email": "lk_alice@example.com", "role": "user" }

HTTP 201

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "lk_bob", "password": "LkBobPassword1!", "email": "lk_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "lk_carol", "password": "LkCarolPassword1!", "email": "lk_carol@example.com", "role": "user" }

HTTP 201


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


GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home: jsonpath "$[0].id"


POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Pre", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
pre_id: jsonpath "$.id"


POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{pre_id}}" },
  "role": "viewer"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Off: no share folder, no link state, no /api/links.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}

HTTP 200
[Captures]
bob_home: jsonpath "$[0].id"


GET {{base_url}}/api/folders/{{bob_home}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "Shares"


GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.id" contains "{{pre_id}}"
jsonpath "$.items[0].link" not exists


POST {{base_url}}/api/links
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "target_id": "{{pre_id}}" }

HTTP 404
