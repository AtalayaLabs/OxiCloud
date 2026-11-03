# =============================================================
# OxiCloud — Public-link download limits
# =============================================================
# Pins the cap a link owner sets at creation (`max_downloads`):
#
#   1. Landing on the link, metadata, 304 and Range seeks do NOT
#      spend the allowance — only a full-file GET and a folder ZIP
#      do. Charging page opens would exhaust a small cap before the
#      visitor saved anything, and charging every media seek would
#      kill playback halfway through.
#   2. The cap is enforced by ONE conditional UPDATE, so the last
#      slot cannot be handed to two concurrent visitors.
#   3. `PUT /api/shares/{id}` distinguishes absent (leave), `null`
#      (remove) and a number (set). Collapsing absent and null —
#      what a plain `Option<u32>` does — would make "unlimited"
#      unreachable after a cap was ever set.
#   4. A capped link still OPENS after the allowance is spent:
#      download stops, browsing does not.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin session, one loose file + one folder with a file
# inside (two disjoint resources, so a download is attributable
# to exactly one of the two shares through the ring).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
admin_home_id: jsonpath "$[0].id"


POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "share-dl-limit", "parent_id": "{{admin_home_id}}" }

HTTP 201
[Captures]
zip_folder_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{admin_home_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
loose_file_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{zip_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
zip_file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# The three wire shapes: capped file, capped folder, uncapped
# default. The uncapped share is never unlocked, so it cannot
# compete with the other two when `require_any` picks the
# credential a download is charged to.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":       "{{loose_file_id}}",
  "item_type":     "file",
  "max_downloads": 1
}

HTTP 201
[Captures]
file_share_id:    jsonpath "$.id"
file_share_token: jsonpath "$.token"
[Asserts]
jsonpath "$.max_downloads"  == 1
jsonpath "$.download_count" == 0


POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":       "{{zip_folder_id}}",
  "item_type":     "folder",
  "max_downloads": 1
}

HTTP 201
[Captures]
zip_share_id:    jsonpath "$.id"
zip_share_token: jsonpath "$.token"
[Asserts]
jsonpath "$.max_downloads"  == 1
jsonpath "$.download_count" == 0


POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{zip_folder_id}}",
  "item_type": "folder"
}

HTTP 201
[Captures]
uncapped_share_id: jsonpath "$.id"
[Asserts]
jsonpath "$.max_downloads"  == null
jsonpath "$.download_count" == 0


# A cap of zero would be a link that never serves anything and
# looks broken rather than deliberate.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":       "{{zip_folder_id}}",
  "item_type":     "folder",
  "max_downloads": 0
}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Become a visitor. Logout clears the three auth cookies; the ring
# is issued per share and is what authenticates the downloads.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/logout
Authorization: Bearer {{admin_token}}

HTTP 200


GET {{base_url}}/api/s/{{file_share_token}}

HTTP 200
[Asserts]
jsonpath "$.max_downloads"  == 1
jsonpath "$.download_count" == 0


GET {{base_url}}/api/s/{{zip_share_token}}

HTTP 200


# CANARY: 403 (not 401) means a share ring is present and the
# principal is anonymous. A surviving session cookie would make
# every download below an owner download — charged to nobody.
GET {{base_url}}/api/auth/me

HTTP 403


# ─────────────────────────────────────────────────────────────
# Browsing is free: metadata, a 304 revalidation, and a Range seek
# must all leave the counter at zero. The ETag comes from the Range
# response — the metadata branch emits no ETag header, and a full
# GET to obtain one would spend the very slot under test.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{loose_file_id}}
Range: bytes=0-3

HTTP 206
[Captures]
file_etag: header "ETag"


GET {{base_url}}/api/files/{{loose_file_id}}
If-None-Match: {{file_etag}}

HTTP 304


GET {{base_url}}/api/files/{{loose_file_id}}?metadata=true

HTTP 200


GET {{base_url}}/api/s/{{file_share_token}}

HTTP 200
[Asserts]
jsonpath "$.download_count" == 0


# ─────────────────────────────────────────────────────────────
# The allowance: first full download passes, second is refused
# with the same 403 shape any denial uses.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{loose_file_id}}

HTTP 200


GET {{base_url}}/api/files/{{loose_file_id}}

HTTP 403
[Asserts]
jsonpath "$.error_type" == "Access Denied"
jsonpath "$.error" contains "Download limit reached"


# The ZIP is the folder share's single unit of download — one
# archive, not one charge per file inside it.
GET {{base_url}}/api/folders/{{zip_folder_id}}/download?format=zip

HTTP 200


GET {{base_url}}/api/folders/{{zip_folder_id}}/download?format=zip

HTTP 403
[Asserts]
jsonpath "$.error_type" == "Access Denied"


# The link still resolves after its allowance is spent: the page
# renders, and the visitor sees why the buttons are gone.
GET {{base_url}}/api/s/{{file_share_token}}

HTTP 200
[Asserts]
jsonpath "$.max_downloads"  == 1
jsonpath "$.download_count" == 1


# ─────────────────────────────────────────────────────────────
# Owner-side edits. Absent leaves the cap, `null` removes it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


PUT {{base_url}}/api/shares/{{file_share_id}}
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "max_downloads": 3 }

HTTP 200
[Asserts]
jsonpath "$.max_downloads"  == 3
jsonpath "$.download_count" == 1


PUT {{base_url}}/api/shares/{{file_share_id}}
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "password": "share-dl-password-1!" }

HTTP 200
[Asserts]
jsonpath "$.max_downloads" == 3


PUT {{base_url}}/api/shares/{{file_share_id}}
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "max_downloads": null }

HTTP 200
[Asserts]
jsonpath "$.max_downloads" == null


PUT {{base_url}}/api/shares/{{file_share_id}}
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "max_downloads": 0 }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Teardown
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/shares/{{file_share_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/shares/{{zip_share_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/shares/{{uncapped_share_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/files/{{loose_file_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/folders/{{zip_folder_id}}
Authorization: Bearer {{admin_token}}
HTTP 204
