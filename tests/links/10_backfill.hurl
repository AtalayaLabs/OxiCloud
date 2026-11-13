# =============================================================
# Links — off → auto backfill
# =============================================================
# After the reboot in auto, the backfill links the grant seeded
# by 00_seed_off.hurl: lk_bob gets `Shares/Pre`. The four links
# jobs are registered.
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
jsonpath "$.features.links_mode" == "auto"


GET {{base_url}}/api/admin/jobs
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$[*].name" contains "links_settle"
jsonpath "$[*].name" contains "links_expiry_sweep"
jsonpath "$[*].name" contains "links_backfill"
jsonpath "$[*].name" contains "links_drop"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "lk_bob", "password": "LkBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


GET {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}

HTTP 200
[Captures]
bob_home: jsonpath "$[0].id"


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
pre_link_id: jsonpath "$.items[?(@.resource.name=='Pre')].resource.id"
[Asserts]
jsonpath "$.items[*].resource.name" contains "Pre"


GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.name=='Pre')].link.link_id" == "{{pre_link_id}}"
jsonpath "$.items[?(@.resource.name=='Pre')].link.declined" == false
