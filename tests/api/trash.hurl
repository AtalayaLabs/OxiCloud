# =============================================================
# OxiCloud – Trash API end-to-end scenario
# =============================================================
# Depends on files-folders.hurl having run first (home folder
# with test1 and test2-renamed must exist).
#
# Run:
#   hurl --variables-file tests/api/test.env --test tests/api/trash.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Login and capture the JWT token
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
[Asserts]
jsonpath "$.access_token" isString


# ─────────────────────────────────────────────────────────────
# Step 2 – Trash is empty at the start.
#
# Pre-flight DELETE /api/trash/empty first so residue from a prior
# session (fail-stopped test that never reached its teardown) can't
# flunk the `count == 0` assertion that follows. The endpoint is
# idempotent — a succeeded 200 on an already-empty trash is the
# usual case. This mirrors step 14 below, which uses the same
# endpoint for the test's own cleanup.
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


# ─────────────────────────────────────────────────────────────
# Step 3 – Capture the home folder ID
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_folder_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 4 – Create "to-delete" folder inside the home folder
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "to-delete",
  "parent_id": "{{home_folder_id}}"
}

HTTP 201
[Captures]
to_delete_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "to-delete"
jsonpath "$.parent_id" == {{home_folder_id}}


# ─────────────────────────────────────────────────────────────
# Step 5 – Upload hello.txt into "to-delete"
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{to_delete_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Asserts]
jsonpath "$.name" == "hello.txt"
jsonpath "$.folder_id" == {{to_delete_id}}


# ─────────────────────────────────────────────────────────────
# Step 6 – Delete "to-delete" folder (moves it to trash)
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{to_delete_id}}
Authorization: Bearer {{token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 7 – Trash contains exactly the "to-delete" folder.
#           In the soft-delete trash model, the trash entry id
#           equals the original resource id (see
#           `storage.trash_items` view + `row_to_trashed_item`),
#           so `resource.id` is what `POST /api/trash/{id}/restore`
#           and `DELETE /api/trash/{id}` accept.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_id: jsonpath "$.items[0].resource.id"
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource_type" == "folder"
jsonpath "$.items[0].resource.name" == "to-delete"
jsonpath "$.items[0].resource.id" == {{to_delete_id}}


# ─────────────────────────────────────────────────────────────
# Step 8 – "to-delete" is no longer listed in the home folder
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{home_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "to-delete"


# ─────────────────────────────────────────────────────────────
# Step 9 – Restore "to-delete" from trash
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/trash/{{trash_id}}/restore
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.success" == true


# ─────────────────────────────────────────────────────────────
# Step 10 – "to-delete" folder is back in the home folder
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{home_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "to-delete"


# ─────────────────────────────────────────────────────────────
# Step 11 – hello.txt is still inside the restored folder
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files?folder_id={{to_delete_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].name" == "hello.txt"


# ─────────────────────────────────────────────────────────────
# Step 12 – Trash is empty after restore
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 13 – Delete "to-delete" folder again (moves to trash)
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{to_delete_id}}
Authorization: Bearer {{token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 14 – Empty the trash
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.success" == true


# ─────────────────────────────────────────────────────────────
# Step 15 – Trash is empty after purge
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 16 – "to-delete" folder no longer exists in home folder
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{home_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "to-delete"
