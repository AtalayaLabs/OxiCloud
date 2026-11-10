# Global CSS is available before authentication; unknown theme URLs never become HTML.
GET {{base_url}}/themes/active.css
HTTP 200
[Captures]
initial_etag: header "ETag"
[Asserts]
header "Content-Type" == "text/css; charset=utf-8"
header "Cache-Control" == "no-cache"
header "X-Content-Type-Options" == "nosniff"

HEAD {{base_url}}/themes/active.css
HTTP 200
[Asserts]
header "ETag" == "{{initial_etag}}"

GET {{base_url}}/themes/active.css
If-None-Match: {{initial_etag}}
HTTP 304

GET {{base_url}}/themes/unknown.css
HTTP 404

GET {{base_url}}/themes
HTTP 404

GET {{base_url}}/api/admin/settings/appearance
HTTP 401

# Refresh mutates the in-memory catalogue and must remain authenticated.
POST {{base_url}}/api/admin/settings/appearance/reload
HTTP 401

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }
HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"

GET {{base_url}}/api/admin/settings/appearance
Authorization: Bearer {{admin_token}}
HTTP 200
[Asserts]
jsonpath "$.available_themes" includes "default"
jsonpath "$.available_themes" includes "hurl-sober"
jsonpath "$.active_theme_id" == "default"

# Admin scope must refuse an ordinary active internal user while allowing
# the owner requests above and below (a denial-only gate is insufficient).
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "theme-regular", "email": "theme-regular@example.com", "password": "ThemeRegular1!", "role": "user" }
HTTP 201
[Captures]
regular_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "theme-regular", "password": "ThemeRegular1!" }
HTTP 200
[Captures]
regular_token: jsonpath "$.access_token"

GET {{base_url}}/api/admin/settings/appearance
Authorization: Bearer {{regular_token}}
HTTP 403

PUT {{base_url}}/api/admin/settings/appearance
Authorization: Bearer {{regular_token}}
Content-Type: application/json
{ "theme_id": "default" }
HTTP 403

POST {{base_url}}/api/admin/settings/appearance/reload
Authorization: Bearer {{regular_token}}
HTTP 403

DELETE {{base_url}}/api/admin/users/{{regular_id}}
Authorization: Bearer {{admin_token}}
HTTP 200

POST {{base_url}}/api/admin/settings/appearance/reload
Authorization: Bearer {{admin_token}}
HTTP 200
[Asserts]
jsonpath "$.available_themes" includes "hurl-sober"

# Wire regression pin: invalid selections must retain the InvalidInput envelope.
PUT {{base_url}}/api/admin/settings/appearance
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "theme_id": "../escape" }
HTTP 400
[Asserts]
jsonpath "$.error_type" == "InvalidInput"

PUT {{base_url}}/api/admin/settings/appearance
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "theme_id": "hurl-sober" }
HTTP 200
[Asserts]
jsonpath "$.selected_theme_id" == "hurl-sober"
jsonpath "$.active_theme_id" == "hurl-sober"

GET {{base_url}}/themes/active.css
HTTP 200
[Asserts]
header "ETag" != "{{initial_etag}}"
body contains "#123456"
# Operator CSS is no longer restricted to token declarations.
body contains ".theme-smoke { border-radius: 2rem !important; }"

PUT {{base_url}}/api/admin/settings/appearance
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "theme_id": "default" }
HTTP 200
[Asserts]
jsonpath "$.active_theme_id" == "default"
