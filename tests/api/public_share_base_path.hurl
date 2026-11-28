# A share link must still grant its scoped anonymous session access when the
# whole API is nested beneath OXICLOUD_BASE_PATH. The inner request reproduces
# #922: Axum's MatchedPath includes `/oxi`, while the allowlist stores the
# prefix-free route pattern.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"

GET {{base_url}}/api/auth/me
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
owner_id: jsonpath "$.full.user.id"

GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
home_id: jsonpath "$[0].id"

POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "base-path-share-test", "parent_id": "{{home_id}}" }

HTTP 201
[Captures]
shared_folder_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{shared_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
shared_file_id: jsonpath "$.id"

POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id": "{{shared_folder_id}}",
  "item_type": "folder"
}

HTTP 201
[Captures]
share_token: jsonpath "$.token"

# Hurl keeps the login cookies for the whole file. Clear the authenticated
# session before creating the anonymous share session, while preserving the
# separately named oxi_shares cookie that the public link will set.
POST {{base_url}}/api/auth/logout
Authorization: Bearer {{admin_token}}

HTTP 200

GET {{base_url}}/api/auth/me

HTTP 401

# The response installs the anonymous share cookie. A successful scoped read
# proves the prefixed MatchedPath is normalized before the allowlist check.
GET {{base_url}}/api/s/{{share_token}}

HTTP 200
[Asserts]
cookie "oxi_shares" exists

GET {{base_url}}/api/folders/{{shared_folder_id}}/resources

HTTP 200
[Asserts]
body contains "{{shared_file_id}}"

# Prefix normalization must not widen the anonymous allowlist.
GET {{base_url}}/api/users/{{owner_id}}

HTTP 403
