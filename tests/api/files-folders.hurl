# =============================================================
# OxiCloud – Files & Folders API end-to-end scenario
# =============================================================
# Run:
#   hurl --variables-file tests/api/test.env --test tests/api/files-folders.hurl
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
admin_user_id: jsonpath "$.user.full.user.id"
[Asserts]
jsonpath "$.access_token" isString
jsonpath "$.token_type" == "Bearer"
jsonpath "$.user.full.user.id" isString


# ─────────────────────────────────────────────────────────────
# Step 2 – List root folders
#           A fresh admin account has exactly one root folder
#           (the home folder created at setup time).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_folder_id:   jsonpath "$[0].id"
home_folder_name: jsonpath "$[0].name"
[Asserts]
jsonpath "$" isCollection
jsonpath "$" count == 1
jsonpath "$[0].id" isString
jsonpath "$[0].name" isString
jsonpath "$[0].parent_id" == null


# ─────────────────────────────────────────────────────────────
# Step 3 – Browse the home folder: sub-folders must be empty
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{home_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 4 – Browse the home folder: files must be empty
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files?folder_id={{home_folder_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" count == 0


# ─────────────────────────────────────────────────────────────
# Step 5 – Creating a folder named "/" must be rejected (400)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "/",
  "parent_id": "{{home_folder_id}}"
}

HTTP 400
[Asserts]
jsonpath "$.error_type" == "Invalid Input"


# ─────────────────────────────────────────────────────────────
# Step 6 – Create folder test1 inside the home folder
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "test1",
  "parent_id": "{{home_folder_id}}"
}

HTTP 201
[Captures]
test1_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" isString
jsonpath "$.name" == "test1"
jsonpath "$.parent_id" == {{home_folder_id}}
# D0 §14 provenance — self-creation: both fields stamp the caller.
jsonpath "$.created_by" == "{{admin_user_id}}"
jsonpath "$.updated_by" == "{{admin_user_id}}"
# Caller-flag contract — fresh folder: not favorited, no grants on it.
# Wire contract is "always present bool" — never null, never absent.
jsonpath "$.is_favorite" == false
jsonpath "$.is_shared" == false


# ─────────────────────────────────────────────────────────────
# Step 7 – Create folder test2 inside test1
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "test2",
  "parent_id": "{{test1_id}}"
}

HTTP 201
[Captures]
test2_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" isString
jsonpath "$.name" == "test2"
jsonpath "$.parent_id" == {{test1_id}}


# ─────────────────────────────────────────────────────────────
# Step 8 – Home folder now has exactly one sub-folder (test1)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{home_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == {{test1_id}}
jsonpath "$.items[0].resource.name" == "test1"


# ─────────────────────────────────────────────────────────────
# Step 9 – test1 has exactly one sub-folder (test2)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{test1_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == {{test2_id}}
jsonpath "$.items[0].resource.name" == "test2"
jsonpath "$.items[0].resource.parent_id" == {{test1_id}}


# ─────────────────────────────────────────────────────────────
# Step 10 – Upload hello.txt into test2
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{test2_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" isString
jsonpath "$.name" == "hello.txt"
jsonpath "$.folder_id" == {{test2_id}}
jsonpath "$.size" == 32
jsonpath "$.mime_type" == "text/plain"
# D0 §14 provenance — uploader's id stamps both fields on a fresh upload.
jsonpath "$.created_by" == "{{admin_user_id}}"
# Caller-flag contract — fresh upload: not favorited, no grants on it.
jsonpath "$.is_favorite" == false
jsonpath "$.is_shared" == false
jsonpath "$.updated_by" == "{{admin_user_id}}"


# ─────────────────────────────────────────────────────────────
# Step 11 – test2 contains exactly hello.txt; verify mime and icon
#            icon_class is only available in the full FileDto
#            returned by the listing endpoint
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files?folder_id={{test2_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].id" == {{file_id}}
jsonpath "$[0].name" == "hello.txt"
jsonpath "$[0].mime_type" == "text/plain"
jsonpath "$[0].icon_class" == "fas fa-file-alt"


# ─────────────────────────────────────────────────────────────
# Step 12 – Move test2 from test1 into the home folder
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{test2_id}}/move
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "parent_id": "{{home_folder_id}}"
}

HTTP 200
[Asserts]
jsonpath "$.id" == {{test2_id}}
jsonpath "$.name" == "test2"
jsonpath "$.parent_id" == {{home_folder_id}}


# ─────────────────────────────────────────────────────────────
# Step 13 – test1 is now empty (test2 was its only child)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{test1_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 14 – Home folder now has two sub-folders: test1 and test2
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{home_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[*].resource.id" contains {{test1_id}}
jsonpath "$.items[*].resource.id" contains {{test2_id}}


# ─────────────────────────────────────────────────────────────
# Step 15 – Rename test2 to test2-renamed
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{test2_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "test2-renamed"
}

HTTP 200
[Asserts]
jsonpath "$.id" == {{test2_id}}
jsonpath "$.name" == "test2-renamed"
jsonpath "$.parent_id" == {{home_folder_id}}


# ─────────────────────────────────────────────────────────────
# Step 16 – Rename hello.txt to hello-renamed.txt
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{file_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hello-renamed.txt"
}

HTTP 200
[Asserts]
jsonpath "$.id" == {{file_id}}
jsonpath "$.name" == "hello-renamed.txt"
jsonpath "$.folder_id" == {{test2_id}}


# ─────────────────────────────────────────────────────────────
# Step 17 – Renaming a file to "." must be rejected (400)
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{file_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "."
}

HTTP 400
[Asserts]
jsonpath "$.error_type" == "Invalid Input"


# ─────────────────────────────────────────────────────────────
# Step 18 – Upload oxicloud-logo.jpg into the home folder
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{home_folder_id}}
file: file,fixtures/oxicloud-logo.jpg; image/jpeg

HTTP 201
[Captures]
logo_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "oxicloud-logo.jpg"
jsonpath "$.mime_type" == "image/jpeg"
jsonpath "$.folder_id" == {{home_folder_id}}


# ─────────────────────────────────────────────────────────────
# Step 19 – Home folder file listing: verify icon class for JPEG
#            (icon_class is only present in the listing response)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files?folder_id={{home_folder_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].name" == "oxicloud-logo.jpg"
jsonpath "$[0].mime_type" == "image/jpeg"
jsonpath "$[0].icon_class" == "fas fa-file-image"


# ─────────────────────────────────────────────────────────────
# Step 20 – Thumbnail icon size exists (200 with image bytes)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{logo_id}}/thumbnail/icon
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Type" startsWith "image/"


# ─────────────────────────────────────────────────────────────
# Step 21 – Thumbnail preview size exists
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{logo_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Type" startsWith "image/"


# ─────────────────────────────────────────────────────────────
# Step 22 – Thumbnail large size exists
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{logo_id}}/thumbnail/large
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Type" startsWith "image/"
