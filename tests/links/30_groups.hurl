# =============================================================
# Links — group grants and membership
# =============================================================
# A group grant links for every member; joining links, leaving
# keeps a link still readable through a direct grant, revoking
# that grant unlinks; nesting a group links for its members;
# deleting a group unlinks its members.
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
{ "username": "gr_alice", "password": "GrAlicePassword1!", "email": "gr_alice@example.com", "role": "user" }

HTTP 201

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "gr_bob", "password": "GrBobPassword1!", "email": "gr_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "gr_carol", "password": "GrCarolPassword1!", "email": "gr_carol@example.com", "role": "user" }

HTTP 201
[Captures]
carol_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "gr_alice", "password": "GrAlicePassword1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "gr_bob", "password": "GrBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "gr_carol", "password": "GrCarolPassword1!" }

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

GET {{base_url}}/api/folders
Authorization: Bearer {{carol_token}}

HTTP 200
[Captures]
carol_home: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Group G (Carol) gets a grant on `GroupDocs` → Carol is linked.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "gr-team", "description": "links groups" }

HTTP 201
[Captures]
group_id: jsonpath "$.id"

POST {{base_url}}/api/groups/{{group_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "user_id": "{{carol_id}}" }

HTTP 201


POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "GroupDocs", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
docs_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{group_id}}" },
  "resource": { "type": "folder", "id": "{{docs_id}}" },
  "role": "viewer"
}

HTTP 201


GET {{base_url}}/api/folders/{{carol_home}}/resources?resource_types=folder
Authorization: Bearer {{carol_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Captures]
carol_shares_id: jsonpath "$.items[?(@.resource.name=='Shares')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Shares"

GET {{base_url}}/api/folders/{{carol_shares_id}}/resources?resource_types=folder
Authorization: Bearer {{carol_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "GroupDocs"


# ─────────────────────────────────────────────────────────────
# Bob joins G → linked.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups/{{group_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "user_id": "{{bob_id}}" }

HTTP 201


GET {{base_url}}/api/folders/{{bob_home}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Captures]
bob_shares_id: jsonpath "$.items[?(@.resource.name=='Shares')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Shares"

GET {{base_url}}/api/folders/{{bob_shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "GroupDocs"


# ─────────────────────────────────────────────────────────────
# Bob is also granted directly, then leaves G → the link stays.
# `Marker1` is granted after the removal; once it is linked the
# removal has been settled.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{docs_id}}" },
  "role": "viewer"
}

HTTP 201
[Captures]
direct_grant_id: jsonpath "$.grants[0].id"


DELETE {{base_url}}/api/groups/{{group_id}}/members/user/{{bob_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Marker1", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
marker1_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{marker1_id}}" },
  "role": "viewer"
}

HTTP 201


GET {{base_url}}/api/folders/{{bob_shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Marker1"
jsonpath "$.items[*].resource.name" contains "GroupDocs"


# ─────────────────────────────────────────────────────────────
# The direct grant revoked → Bob's link is gone, Carol's stays.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/grants/{{direct_grant_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


GET {{base_url}}/api/folders/{{bob_shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "GroupDocs"


GET {{base_url}}/api/folders/{{carol_shares_id}}/resources?resource_types=folder
Authorization: Bearer {{carol_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "GroupDocs"


# ─────────────────────────────────────────────────────────────
# G added into group Outer, which holds a grant → Carol is linked.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "gr-outer", "description": "links groups, outer" }

HTTP 201
[Captures]
outer_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "OuterDocs", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
outer_docs_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{outer_id}}" },
  "resource": { "type": "folder", "id": "{{outer_docs_id}}" },
  "role": "viewer"
}

HTTP 201

POST {{base_url}}/api/groups/{{outer_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "group_id": "{{group_id}}" }

HTTP 201


GET {{base_url}}/api/folders/{{carol_shares_id}}/resources?resource_types=folder
Authorization: Bearer {{carol_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "OuterDocs"


# ─────────────────────────────────────────────────────────────
# G deleted while Carol is a member → her links through G are gone.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/groups/{{group_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


GET {{base_url}}/api/folders/{{carol_shares_id}}/resources?resource_types=folder
Authorization: Bearer {{carol_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "GroupDocs"
jsonpath "$.items[*].resource.name" not contains "OuterDocs"
