# =============================================================
# OxiCloud – Recent items API end-to-end scenario
# =============================================================
# Depends on files-folders.hurl having run first:
#   - home folder exists with test1 and test2-renamed
#   - test2-renamed contains hello-renamed.txt
#
# Run:
#   hurl --variables-file tests/api/test.env --test tests/api/recent.hurl
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
# Step 2 – Discover the file ID of hello-renamed.txt
#           Folders are ORDER BY name: test1 ($[0]), test2-renamed ($[1])
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_folder_id: jsonpath "$[0].id"


GET {{base_url}}/api/folders/{{home_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Captures]
test2_id: jsonpath "$.items[1].resource.id"
[Asserts]
jsonpath "$.items[1].resource.name" == "test2-renamed"


GET {{base_url}}/api/files?folder_id={{test2_id}}
Authorization: Bearer {{token}}

HTTP 200
[Captures]
file_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$[0].name" == "hello-renamed.txt"


# Defensive clear before the explicit-POST assertions: earlier
# scenarios in the runner (files-folders.hurl) auto-record every
# file they upload / GET through the service-layer
# `ResourceAccessHook`, so Recent already has rows by the time we
# arrive here. Clearing first lets step 4 assert `count == 1`
# against a known-empty baseline.
DELETE {{base_url}}/api/recent/clear
Authorization: Bearer {{token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 3 – Record access to hello-renamed.txt
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/recent/file/{{file_id}}
Authorization: Bearer {{token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 4 – Recent list contains hello-renamed.txt
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource_type" == "file"
jsonpath "$.items[0].resource.name" == "hello-renamed.txt"


# ─────────────────────────────────────────────────────────────
# Step 5 – Clear all recent items
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/recent/clear
Authorization: Bearer {{token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 6 – Recent list is empty after clear
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 7 – Auto-recording on upload
#           The backend `ResourceAccessHook` fires on a successful
#           authorised upload, so the new file lands in Recent
#           without the client POSTing /api/recent/file/{id}.
#           This is the SvelteKit-era contract: the legacy
#           vanilla-JS frontend did the POST itself; the new shell
#           relies on the service-layer hook instead.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{home_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
auto_uploaded_id: jsonpath "$.id"


GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{auto_uploaded_id}}"


# ─────────────────────────────────────────────────────────────
# Step 8 – Auto-recording on GET (file download)
#           Clear first, then download the file content and
#           assert it reappears in Recent. The per-(user, file)
#           60 s throttle inside the recording hook means the
#           cleared row may re-record on the very next GET only
#           because we just emptied the table — moka stores the
#           throttle entry independently of the DB row, but the
#           upsert is idempotent and harmless either way.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/recent/clear
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/files/{{auto_uploaded_id}}
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{auto_uploaded_id}}"


# ─────────────────────────────────────────────────────────────
# Step 9 – Cleanup so the test is idempotent across runs.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{auto_uploaded_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/recent/clear
Authorization: Bearer {{token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 10 — Cross-tenant regression (post-Drive AuthZ audit,
#           Round 1 HIGH). Before this fix, `POST /api/recent/…`
#           accepted any UUID and the listing endpoint JOINed back
#           to storage.files/folders (name/mime/size/drive_id) — a
#           metadata oracle over the whole tenant. Now the write
#           path calls `authz.require(Read, …)`; unauthorised
#           callers get 404 (anti-enum) + `authz.denied` audit line.
#           See `docs/plan/authz_audit/rest_storage.md`.
# ─────────────────────────────────────────────────────────────

# Re-discover a folder id so the attacker has TWO targets to probe
# (file + folder). Same test1 folder as favorites.hurl.
GET {{base_url}}/api/folders/{{home_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Captures]
test1_id: jsonpath "$.items[0].resource.id"


# Create/lookup the attacker. Idempotent: `HTTP *` accepts either
# 201 (first run) or 409 (subsequent runs). Login below is the real
# precondition.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{token}}
Content-Type: application/json
{ "username": "rec_mallory", "password": "RecMalloryPassword1!", "email": "rec_mallory@example.com", "role": "user" }

HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "rec_mallory", "password": "RecMalloryPassword1!" }

HTTP 200
[Captures]
mallory_token: jsonpath "$.access_token"


# Step 10a — Record admin's file into mallory's recent: 404.
POST {{base_url}}/api/recent/file/{{file_id}}
Authorization: Bearer {{mallory_token}}

HTTP 404


# Step 10b — Same for admin's folder: 404.
POST {{base_url}}/api/recent/folder/{{test1_id}}
Authorization: Bearer {{mallory_token}}

HTTP 404


# Step 10c — Mallory's recent list stays empty.
GET {{base_url}}/api/recent/resources
Authorization: Bearer {{mallory_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 11 — §7: an unknown order_by is a 400, not a silent default.
# ─────────────────────────────────────────────────────────────
# `order_by` is a typed `RecentOrderBy` enum, so serde rejects anything
# outside the six variants at wire-decode time. It was a bare String
# re-validated at three layers, each falling back to `accessed_at`:
# `build_recent_cursor`'s `_` arm doubled as the default AND the
# unknown case, and the repository had `(_, false)` / `(_, true)` arms
# plus a `_` in its cursor-field match.
#
# The fallbacks disagreed with each other for any axis the caller named
# that one of them did not honour, pairing a keyset predicate on one
# column with an ORDER BY on another — the row-skipping hazard §2 fixed
# on the folder listing. All are gone; the matches are exhaustive.
GET {{base_url}}/api/recent/resources?order_by=definitely-not-an-axis
Authorization: Bearer {{mallory_token}}

HTTP 400
[Asserts]
# serde's derived error names the accepted variants.
body contains "accessed_at"


# A valid non-default axis still works, so the 400 above is the enum
# rejecting an unknown value rather than the parameter being broken.
GET {{base_url}}/api/recent/resources?order_by=owner
Authorization: Bearer {{mallory_token}}

HTTP 200
