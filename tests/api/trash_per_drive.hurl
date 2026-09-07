# =============================================================
# OxiCloud — Per-drive trash empty (D2b stage 4 follow-up)
# =============================================================
# Pins the contract on `DELETE /api/trash/drive/{drive_id}` — the
# per-drive variant of `DELETE /api/trash/empty`. Scope of coverage:
#
#   1. Owner of a drive CAN empty that drive's trash → 204.
#   2. Idempotent: calling again on an empty drive still returns 204.
#   3. Scope: emptying drive A leaves drive B's trash intact.
#   4. Viewer of a shared drive → 404 (Viewer's bundle has no Delete).
#   5. Editor of a shared drive → 404 (Editor's bundle has no Delete).
#   6. Non-member of a shared drive → 404 (anti-enum).
#   7. Unknown drive UUID → 404 (same shape as no-role case; can't
#      enumerate drive existence through this endpoint).
#
# The owner gate has three layers in the implementation (filter,
# membership check, UI hide); cases 4-6 protect the first two. Test 3
# (scope) is the load-bearing assertion against a regression that
# silently merges scopes.
#
# Self-contained: provisions its own users (`tpd_*` prefix) so it
# survives running alongside the rest of the API test suite.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Provision `tpd_owner` (will own the shared drive and a
#          personal-drive trash item that must NOT be touched).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "tpd_owner",
  "password": "TpdOwnerPwd1!",
  "email":    "tpd_owner@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
owner_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "tpd_owner", "password": "TpdOwnerPwd1!" }

HTTP 200
[Captures]
owner_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 3 — Capture the owner's default-personal drive + its root.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].default_for_user" == "{{owner_user_id}}"
[Captures]
personal_drive_id: jsonpath "$[0].id"
personal_root_id:  jsonpath "$[0].root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Admin creates a shared drive owned directly by `tpd_owner`.
#          Direct-user-owner keeps the test self-contained (no group
#          plumbing needed); the membership-API path is exercised
#          separately by `drives_membership.hurl`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "tpd-shared",
  "owner": { "type": "user", "id": "{{owner_user_id}}" }
}

HTTP 201
[Captures]
shared_drive_id: jsonpath "$.id"
shared_root_id:  jsonpath "$.root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Seed one file in each drive, then trash both. We end up
#          with two trash entries the owner can see: one per drive.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
personal_file_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{shared_root_id}}
file: file,fixtures/hello-copy.txt; text/plain

HTTP 201
[Captures]
shared_file_id: jsonpath "$.id"


DELETE {{base_url}}/api/files/{{personal_file_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{shared_file_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# Both files are now trashed; trash listing carries one row per drive.
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].drive_id" contains "{{personal_drive_id}}"
jsonpath "$.items[*].drive_id" contains "{{shared_drive_id}}"


# ─────────────────────────────────────────────────────────────
# Step 6 — Test 1 + Test 3: Owner empties the shared drive's trash;
#          the personal drive's trash item is untouched (scope check).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/drive/{{shared_drive_id}}
Authorization: Bearer {{owner_token}}

HTTP 200


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].drive_id" contains "{{personal_drive_id}}"
jsonpath "$.items[*].drive_id" not contains "{{shared_drive_id}}"


# ─────────────────────────────────────────────────────────────
# Step 7 — Test 2: idempotent on an already-empty drive.
#          No trash items left in the shared drive, but the owner
#          still holds Delete on it, so the response is 200.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/drive/{{shared_drive_id}}
Authorization: Bearer {{owner_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 8 — Test 7: unknown drive id → 404.
#          The endpoint refuses with the same shape it uses for "no
#          Delete on this drive" so callers can't enumerate which
#          drive UUIDs exist via this endpoint.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/drive/00000000-0000-0000-0000-000000000000
Authorization: Bearer {{owner_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 9 — Provision a Viewer of the shared drive (`tpd_viewer`),
#          then assert the per-drive empty refuses for Viewer /
#          Editor / non-member callers. Graduated denial (see
#          [[project_authz_require_graduated_denial]]): the Viewer
#          and Editor tests get 403 because they hold Read on the
#          drive; the non-member fallback keeps the 404 anti-enum
#          shape (no Read = no existence oracle).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "tpd_viewer",
  "password": "TpdViewerPwd1!",
  "email":    "tpd_viewer@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
viewer_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "tpd_viewer", "password": "TpdViewerPwd1!" }

HTTP 200
[Captures]
viewer_token: jsonpath "$.access_token"


# Owner grants Viewer role on the shared drive.
POST {{base_url}}/api/drives/{{shared_drive_id}}/members
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{viewer_user_id}}" },
  "role": "viewer"
}

HTTP 201


# Seed a trash item in the shared drive so the negative tests can't
# pass via the "drive happens to be empty" trivial path. The owner
# trashes a new file; the Viewer/Editor/non-member attempts that
# follow must still refuse — the scope check is on permission, not
# on whether work would be done.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{shared_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
canary_file_id: jsonpath "$.id"

DELETE {{base_url}}/api/files/{{canary_file_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# Test 4 — Viewer cannot empty the drive's trash. Viewer has Read
#          on the drive → graduated denial returns 403.
DELETE {{base_url}}/api/trash/drive/{{shared_drive_id}}
Authorization: Bearer {{viewer_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 10 — Test 5: Editor cannot either.
#           Promote tpd_viewer to Editor; same refusal. Confirms
#           `Delete` isn't in the Editor bundle. Editor has Read →
#           graduated denial returns 403.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{shared_drive_id}}/members/user/{{viewer_user_id}}
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "role": "editor" }

HTTP 200


DELETE {{base_url}}/api/trash/drive/{{shared_drive_id}}
Authorization: Bearer {{viewer_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 11 — Test 6: non-member of the drive cannot empty its trash.
#           Fresh user with no grant on the shared drive whatsoever.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "tpd_outsider",
  "password": "TpdOutsiderPwd1!",
  "email":    "tpd_outsider@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "tpd_outsider", "password": "TpdOutsiderPwd1!" }

HTTP 200
[Captures]
outsider_token: jsonpath "$.access_token"


DELETE {{base_url}}/api/trash/drive/{{shared_drive_id}}
Authorization: Bearer {{outsider_token}}

HTTP 404


# Trash row is still there — none of the negative attempts purged it.
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].drive_id" contains "{{shared_drive_id}}"


# ─────────────────────────────────────────────────────────────
# Step 11b — Regression pin for AuthZ audit #10 (2026-07-12).
#            `POST /api/trash/{id}/restore` and `DELETE /api/trash/{id}`
#            once did `err_str.contains("not found")` to decide "already
#            gone" vs real failure — an authz denial (which returns a
#            `NotFound`-shaped DomainError to preserve anti-enum on the
#            listing side) matched the substring and got synthesised
#            into a 200 `{"success": true}` response. Response lied;
#            no mutation happened.
#
#            Post-fix: both handlers route through
#            `AppError::from(e).into_response()`, so authz denials
#            surface as the graduated 403 / 404 shape and body is
#            never a success envelope.
#
#            The Editor (from Step 10 promotion) holds Read on the
#            canary — graduated denial returns 403 with a
#            `AccessDenied`-shape body, NOT a success envelope. If a
#            future refactor reintroduces the substring hack this
#            assertion trips before it lands in prod.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{viewer_token}}

HTTP 200
[Captures]
# The shared drive's trash holds exactly one item at this point (the
# canary owner trashed after Step 9), so `$.items[0]` is unambiguous
# — no filter needed. `TrashResourceItemDto` wraps the underlying
# resource in `.resource` (untagged File | Folder | Drive enum) and
# the trash key equals the original resource id (see
# `storage.trash_items` view), so `.resource.id` is exactly what
# `POST /api/trash/{id}/restore` and `DELETE /api/trash/{id}` accept.
# The `[?(...)]` + `nth 0` shape (see the sibling
# feedback_hurl_jsonpath_filter_empty memory) collapses on a single
# match and returns a scalar hurl can't index, so we avoid it here.
canary_trash_id: jsonpath "$.items[0].resource.id"


POST {{base_url}}/api/trash/{{canary_trash_id}}/restore
Authorization: Bearer {{viewer_token}}

HTTP 403
[Asserts]
body not contains "\"success\":true"


DELETE {{base_url}}/api/trash/{{canary_trash_id}}
Authorization: Bearer {{viewer_token}}

HTTP 403
[Asserts]
body not contains "\"success\":true"


# The canary is still there — the two Editor attempts didn't mutate.
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
# Owner sees TWO trash items at this point — the shared drive's
# canary (from Step 9) plus their personal drive's leftover from
# Step 4 (owner emptied only the shared drive's trash at Step 6).
# `contains` avoids depending on the sort order between them.
jsonpath "$.items[*].resource.id" contains "{{canary_trash_id}}"


# ─────────────────────────────────────────────────────────────
# Step 12 — Cleanup: drop the canary, then the shared drive itself
#           (D3b's delete-drive guard refuses non-empty drives, so
#           clearing trash + the live tree first is required).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/drive/{{shared_drive_id}}
Authorization: Bearer {{owner_token}}

HTTP 200


DELETE {{base_url}}/api/drives/{{shared_drive_id}}
Authorization: Bearer {{owner_token}}

HTTP 204
