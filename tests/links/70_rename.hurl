# =============================================================
# Links — owner renames the target
# =============================================================
# rn_alice renames `Reports` → `Reports 2026`. rn_bob's link
# keeps its name, he gets one `link_target_renamed` notification,
# and Shared with me shows both names.
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
{ "username": "rn_alice", "password": "RnAlicePassword1!", "email": "rn_alice@example.com", "role": "user" }

HTTP 201

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "rn_bob", "password": "RnBobPassword1!", "email": "rn_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "rn_alice", "password": "RnAlicePassword1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "rn_bob", "password": "RnBobPassword1!" }

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
{ "name": "Reports", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
reports_id: jsonpath "$.id"

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
[Captures]
link_id: jsonpath "$.items[?(@.resource.name=='Reports')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Reports"


PUT {{base_url}}/api/folders/{{reports_id}}/rename
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Reports 2026" }

HTTP 200


# The notification is sent off the rename's request path; a single match is a scalar.
GET {{base_url}}/api/notifications
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[?(@.kind=='link_target_renamed')].payload.link_name" == "Reports"
jsonpath "$.items[?(@.kind=='link_target_renamed')].payload.old_name" == "Reports"
jsonpath "$.items[?(@.kind=='link_target_renamed')].payload.new_name" == "Reports 2026"
jsonpath "$.items[?(@.kind=='link_target_renamed')].payload.link_id" == "{{link_id}}"
jsonpath "$.items[?(@.kind=='link_target_renamed')].payload.target_id" == "{{reports_id}}"


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Reports"
jsonpath "$.items[*].resource.name" not contains "Reports 2026"


GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{reports_id}}')].resource.name" == "Reports 2026"
jsonpath "$.items[?(@.resource.id=='{{reports_id}}')].link.link_name" == "Reports"
jsonpath "$.items[?(@.resource.id=='{{reports_id}}')].link.link_id" == "{{link_id}}"
