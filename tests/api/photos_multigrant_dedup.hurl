# =============================================================
# OxiCloud – Photos listing stays deduped when a caller reaches a
# drive through MORE than one grant
# =============================================================
# Regression for the inline `accessible` CTE in `list_media_files`:
# the earlier `JOIN storage.role_grants` shape emitted one row per
# matching grant, so a user with BOTH a direct `user` grant AND a
# `group` grant on the same drive saw every photo in that drive
# twice. Downstream `CROSS JOIN LATERAL` then fanned the duplicate
# into the response and the virtualized Photos grid froze on
# `each_key_duplicate` the first time a shared drive overlapped
# with group membership.
#
# This scenario sets up exactly that overlap, uploads a single
# photo, and asserts the response carries the file exactly once.
# Without the `EXISTS`-shaped CTE it reports count == 2.
#
# §0 of docs/plan/photos-resources-migration.md.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login as admin (alice).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"
# `/api/auth/login` returns the three-layer SelfUserDto — the user id
# lives at `.user.full.user.id`, not `.user.id` (which is the POST
# /api/admin/users response shape).
alice_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Fresh user "mia" who will accumulate two grants on
#          the same drive. Password matches the suite convention
#          (one capital, one digit, one symbol).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "mia", "password": "MiaPassword1!", "email": "mia@example.com", "role": "user" }

HTTP 201
[Captures]
mia_user_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Shared drive owned by alice, with
#          `include_in_photo_index = true` so photos uploaded
#          here show up in `GET /api/photos`. (Shared drives
#          default the flag OFF; a personal drive would have it
#          auto-true at creation but personal drives don't accept
#          outside grants.)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "multigrant-photos-hurl",
  "owner": { "type": "user", "id": "{{alice_user_id}}" }
}

HTTP 201
[Captures]
drive_id: jsonpath "$.id"
root_folder_id: jsonpath "$.root_folder_id"


PATCH {{base_url}}/api/drives/{{drive_id}}/policies
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "include_in_photo_index": true }

HTTP 200
[Asserts]
jsonpath "$.include_in_photo_index" == true


# ─────────────────────────────────────────────────────────────
# Step 4 — Upload one photo. The suite's `oxicloud-logo.jpg`
#          fixture is content-addressed and already used by
#          `photos_etag.hurl`; reusing it keeps blob ref_counts
#          predictable.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{root_folder_id}}
file: file,fixtures/oxicloud-logo.jpg; image/jpeg

HTTP 201
[Captures]
photo_file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Build the overlap:
#            * group "grp-multigrant-photos-hurl" with mia as a
#              direct member;
#            * add mia DIRECTLY as a viewer grant on the drive;
#            * add the group as a viewer grant on the drive.
#          Mia now reaches the drive through two role_grants
#          rows at once — the exact shape that duplicates in the
#          old CTE.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grp-multigrant-photos-hurl", "description": "overlap test" }

HTTP 201
[Captures]
multigrant_group_id: jsonpath "$.id"


POST {{base_url}}/api/groups/{{multigrant_group_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "user_id": "{{mia_user_id}}" }

HTTP 201


# Direct user grant on the drive.
POST {{base_url}}/api/drives/{{drive_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{mia_user_id}}" },
  "role": "viewer"
}

HTTP 201


# Group grant on the same drive — the second row that used to
# duplicate `accessible`.
POST {{base_url}}/api/drives/{{drive_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject": { "type": "group", "id": "{{multigrant_group_id}}" },
  "role": "viewer"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 6 — Mia logs in and reads `/api/photos`. The response
#          must carry the uploaded photo EXACTLY ONCE. The
#          `jsonpath` filter counts occurrences of `photo_file_id`
#          in the top-level array; the inline `JOIN role_grants`
#          CTE returned count == 2 here before the EXISTS fix.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mia", "password": "MiaPassword1!" }

HTTP 200
[Captures]
mia_token: jsonpath "$.access_token"


GET {{base_url}}/api/photos?limit=200
Authorization: Bearer {{mia_token}}

HTTP 200
[Asserts]
# `$[*].id count` would be the direct phrasing but hurl collapses
# single-match projections to a scalar — `count` and `contains`
# then refuse to run on a non-list (see memory
# `hurl-jsonpath-filter-empty-result`). The reliable form is
# root-count + positional access, which the suite uses elsewhere:
# `$ count` reads the array length as the dedup proof (would be
# 2 before the EXISTS fix, since mia has no other visible source
# of photos), and `$[0].id` pins the row that landed is the one
# we uploaded.
jsonpath "$"       count == 1
jsonpath "$[0].id"       == "{{photo_file_id}}"


# ─────────────────────────────────────────────────────────────
# Step 7 — Teardown. Hurl runs share one DB (feedback_hurl_
# teardown_shared_db); leaving the group / grants / drive /
# user around would poison later photo-count assertions.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/drives/{{drive_id}}/members/group/{{multigrant_group_id}}
Authorization: Bearer {{alice_token}}

HTTP *


DELETE {{base_url}}/api/drives/{{drive_id}}/members/user/{{mia_user_id}}
Authorization: Bearer {{alice_token}}

HTTP *


DELETE {{base_url}}/api/groups/{{multigrant_group_id}}
Authorization: Bearer {{alice_token}}

HTTP *


DELETE {{base_url}}/api/admin/users/{{mia_user_id}}
Authorization: Bearer {{alice_token}}

HTTP *


# Drive deletion is admin-only and refuses while content remains;
# trashing the photo first keeps the delete clean.
DELETE {{base_url}}/api/files/{{photo_file_id}}
Authorization: Bearer {{alice_token}}

HTTP *


DELETE {{base_url}}/api/admin/drives/{{drive_id}}
Authorization: Bearer {{alice_token}}

HTTP *
