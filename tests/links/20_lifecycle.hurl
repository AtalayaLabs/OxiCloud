# =============================================================
# Links — creation, naming, decline, restore, revoke
# =============================================================
# lc_bob receives shares from lc_alice and lc_carol: same-named
# targets get ` (2)`, a target named like his own folder in
# Shares gets ` (2)`, user + group grant on one target give one
# link, a nested grant its own link. Deleting a link declines it,
# POST /api/links restores it, a revoke clears the decline so a
# re-share is linked again.
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
{ "username": "lc_alice", "password": "LcAlicePassword1!", "email": "lc_alice@example.com", "role": "user" }

HTTP 201
[Captures]
alice_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "lc_bob", "password": "LcBobPassword1!", "email": "lc_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "lc_carol", "password": "LcCarolPassword1!", "email": "lc_carol@example.com", "role": "user" }

HTTP 201


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "lc_alice", "password": "LcAlicePassword1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "lc_bob", "password": "LcBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "lc_carol", "password": "LcCarolPassword1!" }

HTTP 200
[Captures]
carol_token: jsonpath "$.access_token"


GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home: jsonpath "$[0].id"

GET {{base_url}}/api/folders
Authorization: Bearer {{carol_token}}

HTTP 200
[Captures]
carol_home: jsonpath "$[0].id"

GET {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}

HTTP 200
[Captures]
bob_home: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# A grant links the target into Shares.
# ─────────────────────────────────────────────────────────────
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
[Captures]
reports_grant_id: jsonpath "$.grants[0].id"


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
reports_link_id: jsonpath "$.items[?(@.resource.name=='Reports')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Reports"


# ─────────────────────────────────────────────────────────────
# A second share named `Reports` lands as `Reports (2)`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{carol_token}}
Content-Type: application/json
{ "name": "Reports", "parent_id": "{{carol_home}}" }

HTTP 201
[Captures]
carol_reports_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{carol_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{carol_reports_id}}" },
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
jsonpath "$.items[*].resource.name" contains "Reports"
jsonpath "$.items[*].resource.name" contains "Reports (2)"


GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{carol_reports_id}}')].link.link_name" == "Reports (2)"


# ─────────────────────────────────────────────────────────────
# Bob's own `Own` in Shares pushes the share of `Own` to `Own (2)`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "name": "Own", "parent_id": "{{shares_id}}" }

HTTP 201

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Own", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
own_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{own_id}}" },
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
jsonpath "$.items[*].resource.name" contains "Own"
jsonpath "$.items[*].resource.name" contains "Own (2)"


# ─────────────────────────────────────────────────────────────
# The same target granted to Bob and to his group → one link.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "lc-team", "description": "links lifecycle" }

HTTP 201
[Captures]
group_id: jsonpath "$.id"

POST {{base_url}}/api/groups/{{group_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "user_id": "{{bob_id}}" }

HTTP 201

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Both", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
both_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{group_id}}" },
  "resource": { "type": "folder", "id": "{{both_id}}" },
  "role": "viewer"
}

HTTP 201

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{both_id}}" },
  "role": "viewer"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# A grant nested under a granted folder gets its own link.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Sub", "parent_id": "{{reports_id}}" }

HTTP 201
[Captures]
sub_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{sub_id}}" },
  "role": "editor"
}

HTTP 201


# `Sub` is granted after `Both`, so once it is linked both `Both` events are settled.
GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Sub"
jsonpath "$.items[*].resource.name" contains "Both"
jsonpath "$.items[*].resource.name" not contains "Both (2)"
jsonpath "$.items[*].resource.name" not contains "Sub (2)"


# ─────────────────────────────────────────────────────────────
# Deleting the link declines it; the target is untouched.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{reports_link_id}}
Authorization: Bearer {{bob_token}}

HTTP 204


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "Reports"
jsonpath "$.items[*].resource.name" contains "Reports (2)"
jsonpath "$.items[*].resource.name" contains "Sub"


GET {{base_url}}/api/folders/{{reports_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.name" == "Reports"


GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{reports_id}}')].link.declined" == true
jsonpath "$.items[?(@.resource.id=='{{reports_id}}')].link.link_id" == null


# ─────────────────────────────────────────────────────────────
# Logging in again does not bring the link back, and a grant on
# a folder in Bob's own drive gives him no link. Bob makes Alice
# owner of his `Mine`; she grants him `Mine/Inner`. `Marker0`,
# granted afterwards, shows when both have been settled.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "lc_bob", "password": "LcBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"

POST {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "name": "Mine", "parent_id": "{{bob_home}}" }

HTTP 201
[Captures]
mine_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "name": "Inner", "parent_id": "{{mine_id}}" }

HTTP 201
[Captures]
inner_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{alice_id}}" },
  "resource": { "type": "folder", "id": "{{mine_id}}" },
  "role": "owner"
}

HTTP 201

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{inner_id}}" },
  "role": "viewer"
}

HTTP 201

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Marker0", "parent_id": "{{alice_home}}" }

HTTP 201
[Captures]
marker0_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{marker0_id}}" },
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
jsonpath "$.items[*].resource.name" contains "Marker0"
jsonpath "$.items[*].resource.name" not contains "Reports"
jsonpath "$.items[*].resource.name" not contains "Inner"


# ─────────────────────────────────────────────────────────────
# POST /api/links restores it under its original name.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/links
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "target_id": "{{reports_id}}" }

HTTP 201
[Captures]
reports_link_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "Reports"


GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{reports_id}}')].link.declined" == false
jsonpath "$.items[?(@.resource.id=='{{reports_id}}')].link.link_id" == "{{reports_link_id}}"


# ─────────────────────────────────────────────────────────────
# Declined again, then revoked: link and decline are gone, so a
# re-share is linked again.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{reports_link_id}}
Authorization: Bearer {{bob_token}}

HTTP 204

DELETE {{base_url}}/api/grants/{{reports_grant_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.id" not contains "{{reports_id}}"


# `Marker` is granted after the revoke; once it is linked the revoke has been settled.
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Marker", "parent_id": "{{alice_home}}" }

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


# `Sub` is still granted directly, so its link survives the revoke of `Reports`.
GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Marker"
jsonpath "$.items[*].resource.name" contains "Sub"
jsonpath "$.items[*].resource.name" not contains "Reports"


POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{reports_id}}" },
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
reports_link_id: jsonpath "$.items[?(@.resource.name=='Reports')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Reports"


GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{reports_id}}')].link.declined" == false
jsonpath "$.items[?(@.resource.id=='{{reports_id}}')].link.link_id" == "{{reports_link_id}}"
