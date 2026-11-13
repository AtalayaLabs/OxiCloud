# =============================================================
# Links — grant expiry, untimed setup for 25_expiry.hurl
# =============================================================
# run.sh runs this first and hands its captures to 25_expiry.hurl
# with an expiry computed afterwards. ex_bob is in ex-team, which
# holds a live grant on `Kept`; he declines that link here.
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
{ "username": "ex_alice", "password": "ExAlicePassword1!", "email": "ex_alice@example.com", "role": "user" }

HTTP 201

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "ex_bob", "password": "ExBobPassword1!", "email": "ex_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ex_alice", "password": "ExAlicePassword1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ex_bob", "password": "ExBobPassword1!" }

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
{ "name": "Expiring", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
expiring_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Kept", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
kept_id: jsonpath "$.id"


POST {{base_url}}/api/groups
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "ex-team", "description": "links expiry" }

HTTP 201
[Captures]
group_id: jsonpath "$.id"

POST {{base_url}}/api/groups/{{group_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "user_id": "{{bob_id}}" }

HTTP 201

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{group_id}}" },
  "resource": { "type": "folder", "id": "{{kept_id}}" },
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
kept_link_id: jsonpath "$.items[?(@.resource.name=='Kept')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Kept"

DELETE {{base_url}}/api/folders/{{kept_link_id}}
Authorization: Bearer {{bob_token}}

HTTP 204
