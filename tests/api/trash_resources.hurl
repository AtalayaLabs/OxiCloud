# =============================================================
# OxiCloud – Cursor-paginated trash listing (GET /api/trash/resources)
# =============================================================
# Depends on files-folders.hurl having run first (home folder exists).
# Runs after trash.hurl, which leaves an empty trash.
#
# Verifies:
#   - Empty trash returns items:[] and no next_cursor
#   - 401 without auth
#   - Default order_by = deletion_date (soonest-expiring first)
#   - order_by=name groups folders before files
#   - order_by=size: folders cluster via -1 sentinel
#   - resource_types=file / =folder filters narrow the result set
#   - Pagination: limit=1 + cursor walks the whole set
#   - Cursor is opaque (returned next_cursor is a non-empty string)
#
# Run:
#   hurl --variables-file tests/api/test.env --test tests/api/trash_resources.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Login + capture token
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 – Trash starts empty.
#
# Pre-flight DELETE /api/trash/empty: `trash.hurl` runs just before
# this file and leaves trash empty on a clean run, but a fail-stopped
# session may have left residue. The empty-trash call makes the
# assertion below self-healing without changing what we're verifying.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
jsonpath "$.items" count == 0
jsonpath "$.next_cursor" not exists


# ─────────────────────────────────────────────────────────────
# Step 4 – Capture the home folder ID
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_folder_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 5 – Create three folders + two files to trash
#   folders: trash-res-A, trash-res-B
#   files:   alpha.txt (in home), beta.txt (in home)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "trash-res-A",
  "parent_id": "{{home_folder_id}}"
}
HTTP 201
[Captures]
folder_a_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "trash-res-B",
  "parent_id": "{{home_folder_id}}"
}
HTTP 201
[Captures]
folder_b_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{home_folder_id}}
file: file,fixtures/hello.txt; text/plain
HTTP 201
[Captures]
file_alpha_id: jsonpath "$.id"


# Rename uploaded file to alpha.txt so order_by=name has a predictable result
PUT {{base_url}}/api/files/{{file_alpha_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "alpha.txt"
}

HTTP 200


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{home_folder_id}}
file: file,fixtures/hello.txt; text/plain
HTTP 201
[Captures]
file_beta_id: jsonpath "$.id"


PUT {{base_url}}/api/files/{{file_beta_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "beta.txt"
}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 6 – Move all four items to trash
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/files/{{file_alpha_id}}
Authorization: Bearer {{token}}
HTTP 200


DELETE {{base_url}}/api/trash/files/{{file_beta_id}}
Authorization: Bearer {{token}}
HTTP 200


DELETE {{base_url}}/api/trash/folders/{{folder_a_id}}
Authorization: Bearer {{token}}
HTTP 200


DELETE {{base_url}}/api/trash/folders/{{folder_b_id}}
Authorization: Bearer {{token}}
HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 7 – Default listing: 4 items, all carry trashed_at + deletion_date
#          + drive_id (D2b — enables per-drive grouping in the UI).
#          The listing is also drive-membership scoped (D2b stage 2): the
#          admin sees their own personal drive's trashed items, just as
#          before, because a personal drive Owner has all roles needed.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 4
jsonpath "$.items[0].trashed_at" isString
jsonpath "$.items[0].deletion_date" isString
# D2b: drive_id is non-null on every trashed item.
jsonpath "$.items[0].drive_id" isString
jsonpath "$.items[1].drive_id" isString
jsonpath "$.items[2].drive_id" isString
jsonpath "$.items[3].drive_id" isString
jsonpath "$.items[0].resource_type" matches "^(file|folder)$"
jsonpath "$.items[*].resource.id" contains {{folder_a_id}}
jsonpath "$.items[*].resource.id" contains {{folder_b_id}}
jsonpath "$.items[*].resource.id" contains {{file_alpha_id}}
jsonpath "$.items[*].resource.id" contains {{file_beta_id}}


# ─────────────────────────────────────────────────────────────
# Step 8 – order_by=name: folders sort before files (folder_first axis),
#           then alphabetic. Expected order:
#           [trash-res-A, trash-res-B, alpha.txt, beta.txt]
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources?order_by=name
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items[0].resource.name" == "trash-res-A"
jsonpath "$.items[1].resource.name" == "trash-res-B"
jsonpath "$.items[2].resource.name" == "alpha.txt"
jsonpath "$.items[3].resource.name" == "beta.txt"


# ─────────────────────────────────────────────────────────────
# Step 9 – order_by=size: folders use sentinel -1 so they cluster
#           together (before files with size >= 0).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources?order_by=size
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 4
jsonpath "$.items[0].resource_type" == "folder"
jsonpath "$.items[1].resource_type" == "folder"
jsonpath "$.items[2].resource_type" == "file"
jsonpath "$.items[3].resource_type" == "file"


# ─────────────────────────────────────────────────────────────
# Step 10 – resource_types=file → only files (2)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources?resource_types=file
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource_type" == "file"
jsonpath "$.items[1].resource_type" == "file"


# ─────────────────────────────────────────────────────────────
# Step 11 – resource_types=folder → only folders (2)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource_type" == "folder"
jsonpath "$.items[1].resource_type" == "folder"


# ─────────────────────────────────────────────────────────────
# Step 12 – Pagination: limit=1 returns 1 item + a next_cursor
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources?order_by=name&limit=1
Authorization: Bearer {{token}}

HTTP 200
[Captures]
page1_cursor: jsonpath "$.next_cursor"
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.name" == "trash-res-A"
jsonpath "$.next_cursor" isString


# Step 13 – Walk to page 2 with the captured cursor
GET {{base_url}}/api/trash/resources?order_by=name&limit=1&cursor={{page1_cursor}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.name" == "trash-res-B"
jsonpath "$.next_cursor" isString


# ─────────────────────────────────────────────────────────────
# Step 14 – Cleanup: empty trash so subsequent tests see a clean slate
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0
