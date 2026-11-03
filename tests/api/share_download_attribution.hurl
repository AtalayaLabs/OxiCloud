# =============================================================
# OxiCloud — which link pays for a download
# =============================================================
# A browser can hold several share links at once (`oxi_shares`), and the
# ring is ordered by last unlock — re-opening a link moves it to the back.
# Before this fix the server charged whichever link happened to authorize
# the request, so with two links on the same file an exhausted link 1
# refused every download through link 2, and link 2's counter never moved.
#
# The share page now names its link (`?share=<share id>`) on every
# content request: previews, downloads and ZIPs. The server charges
# that link, provided the caller holds it and it grants Read on the
# resource. Requests without the parameter keep the old fallback.
# =============================================================


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


POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{admin_home_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
shared_file_id: jsonpath "$.id"


# link A has room; link B has exactly one download. If the charge
# lands on A, B never moves and the second download through B
# wrongly succeeds.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":       "{{shared_file_id}}",
  "item_type":     "file",
  "max_downloads": 3
}

HTTP 201
[Captures]
share_a_id:    jsonpath "$.id"
share_a_token: jsonpath "$.token"


POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":       "{{shared_file_id}}",
  "item_type":     "file",
  "max_downloads": 1
}

HTTP 201
[Captures]
share_b_id:    jsonpath "$.id"
share_b_token: jsonpath "$.token"


POST {{base_url}}/api/auth/logout
Authorization: Bearer {{admin_token}}

HTTP 200


GET {{base_url}}/api/auth/me

HTTP 401


# Both links are unlocked in the SAME cookie jar. Ring order is
# [A, B] — the order the old bug used to pick its victim from.
GET {{base_url}}/api/s/{{share_a_token}}

HTTP 200

GET {{base_url}}/api/s/{{share_b_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Naming B charges B, even though A authorizes the request first.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{shared_file_id}}?share={{share_b_id}}

HTTP 200


GET {{base_url}}/api/s/{{share_b_token}}

HTTP 200
[Asserts]
jsonpath "$.download_count" == 1
jsonpath "$.max_downloads"  == 1


GET {{base_url}}/api/s/{{share_a_token}}

HTTP 200
[Asserts]
jsonpath "$.download_count" == 0


# B is spent. Naming it must refuse rather than silently spend A.
GET {{base_url}}/api/files/{{shared_file_id}}?share={{share_b_id}}

HTTP 403
[Asserts]
jsonpath "$.error_type" == "AccessDenied"
jsonpath "$.error" contains "Download limit reached"


# Naming A charges A.
GET {{base_url}}/api/files/{{shared_file_id}}?share={{share_a_id}}

HTTP 200


GET {{base_url}}/api/s/{{share_a_token}}

HTTP 200
[Asserts]
jsonpath "$.download_count" == 1


# A request with no `share` falls back to the first link in the ring
# that grants Read. The `/api/s/` hit above re-unlocked A, and a
# re-unlock moves a link to the BACK of the ring, so the fallback now
# reaches B first — and B is spent. Omitting `share` therefore cannot
# buy a download the caps did not allow.
GET {{base_url}}/api/files/{{shared_file_id}}

HTTP 403
[Asserts]
jsonpath "$.error_type" == "AccessDenied"


GET {{base_url}}/api/s/{{share_a_token}}

HTTP 200
[Asserts]
jsonpath "$.download_count" == 1


# ─────────────────────────────────────────────────────────────
# Teardown
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


DELETE {{base_url}}/api/shares/{{share_a_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/shares/{{share_b_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/files/{{shared_file_id}}
Authorization: Bearer {{admin_token}}
HTTP 204
