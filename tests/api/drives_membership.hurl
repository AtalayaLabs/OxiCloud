# =============================================================
# OxiCloud — D2/D3a drive membership + create-drive end-to-end
# =============================================================
# D2 coverage (steps 1-12, personal-drive-only world):
#   1. `GET /api/drives` exposes `caller_role` on every row.
#   2. `GET /api/drives/{id}/members` lists role grants on a drive
#      (one Owner row for the lifecycle-hook-provisioned default).
#   3. Personal-drive guard refuses every membership mutation
#      (POST / PATCH / DELETE on `/api/drives/{id}/members*`) with
#      405 — personal drives are single-user single-owner.
#   4. The same guard fires on the generic `/api/grants` write paths
#      (POST / PUT / DELETE) when `resource.type='drive'` and the
#      drive is personal — verifies the `DriveManagementService`
#      delegation that closes the bypass.
#   5. Drive grants surface in `GET /api/grants/incoming/resources`
#      with `resource_types=drive` filter (previously hard-skipped).
#   6. Anti-enum: an unrelated user gets the same `404` for a drive
#      they can't read, whether or not it exists.
#
# D3a coverage (steps 13-22, unlocked by `POST /api/drives`):
#   - admin-only authz gate on create
#   - kind=personal returns 501 (placeholder for the future PR)
#   - Owner subject = user → single Owner shared drive
#   - Owner subject = group with members → group-mediated Owner
#   - Owner subject = empty group → 400 (no orphan Owner)
#   - Token subject refused
#   - Last-owner protection on the new shared drive
#   - Group-mediated Owner: caller_role resolves the strongest role
#     (MIN over direct + group grants)
#   - Editor cascade through the drive precheck (Bob gets Editor on
#     a shared drive via the membership API and sees the drive)
#   - Role demotion: PATCH Bob from Editor to Viewer reflects in his
#     listing
#
# Per-role mutation matrix coverage (steps 23-29):
#   - Owner CAN rename the drive (positive symmetry)
#   - Owner CAN edit owners / editors / viewers (grant Owner, promote
#     and demote across all role boundaries)
#   - Viewer CANNOT POST / PATCH / DELETE members → 404
#   - Editor CANNOT POST / PATCH / DELETE members → 404
#   - Editor CAN modify drive content (positive role-bundle check)
#   - Viewer CAN read drive content (positive role-bundle check)
#   - Non-member sees 404 on every member-mutation verb AND on
#     GET /members (anti-enum: no existence leak)
#
# **Known gap** surfaced by Step 26d: today's folder rename uses
# `Permission::Update`, which is in Editor's bundle. The plan
# (`drive.md §6`) says drive rename should be Owner-only. If/when
# tightening: change the folder service to require `Manage` (or a
# new `RenameDrive` permission) for folders that are drive roots.
#
# Self-contained: provisions its own users so it can run after
# drives_foundation without aliasing state.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — admin login
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"
admin_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Admin's GET /api/drives now includes `caller_role`.
#          Personal-drive owner role is seeded by
#          PersonalDriveLifecycleHook on user creation.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$[0].kind"             == "personal"
jsonpath "$[0].default_for_user" == "{{admin_user_id}}"
jsonpath "$[0].caller_role"      == "owner"

[Captures]
admin_drive_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 3 — GET /api/drives/{id}/members returns the lifecycle-
#          seeded Owner row and only that.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives/{{admin_drive_id}}/members
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$"                       count == 1
jsonpath "$[0].subject.type"       == "user"
jsonpath "$[0].subject.id"         == "{{admin_user_id}}"
jsonpath "$[0].resource.type"      == "drive"
jsonpath "$[0].resource.id"        == "{{admin_drive_id}}"
jsonpath "$[0].role"               == "owner"


# ─────────────────────────────────────────────────────────────
# Step 4 — Create a fresh user (mbr_alice) so we have a second
#          subject the personal-drive guard can refuse on.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "mbr_alice", "password": "MbrAlicePassword1!", "email": "mbr_alice@example.com", "role": "user" }

HTTP 201
[Captures]
alice_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mbr_alice", "password": "MbrAlicePassword1!" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

# Alice's GET /api/drives surfaces her own default with caller_role=owner.
GET {{base_url}}/api/drives
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$"                     count == 1
jsonpath "$[0].caller_role"      == "owner"
jsonpath "$[0].default_for_user" == "{{alice_user_id}}"

[Captures]
alice_drive_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Personal-drive guard via the dedicated endpoint:
#          POST /api/drives/{id}/members refuses with 405.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives/{{admin_drive_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{alice_user_id}}" },
  "role": "editor"
}

HTTP 405


# ─────────────────────────────────────────────────────────────
# Step 6 — PATCH on a personal drive's members → 405 (even for
#          the owner row itself; personal drives' membership
#          is structurally immutable).
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{admin_drive_id}}/members/user/{{admin_user_id}}
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "role": "editor" }

HTTP 405


# ─────────────────────────────────────────────────────────────
# Step 7 — DELETE on a personal drive's owner → 405. (Verifies
#          the guard fires BEFORE the last-owner check; the order
#          matters for the right error.)
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/drives/{{admin_drive_id}}/members/user/{{admin_user_id}}
Authorization: Bearer {{admin_token}}

HTTP 405


# ─────────────────────────────────────────────────────────────
# Step 8 — Generic /api/grants delegation. POST /api/grants with
#          resource.type='drive' on a personal drive must hit the
#          same guard, otherwise the membership rules are bypassable.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "resource": { "type": "drive", "id": "{{admin_drive_id}}" },
  "subject":  { "type": "user",  "id": "{{alice_user_id}}" },
  "role": "editor"
}

HTTP 405


# ─────────────────────────────────────────────────────────────
# Step 9 — PUT /api/grants/role (silent admin update) on a drive
#          resource — also delegated. Same guard fires.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "resource": { "type": "drive", "id": "{{admin_drive_id}}" },
  "subject":  { "type": "user",  "id": "{{admin_user_id}}" },
  "role": "editor"
}

HTTP 405


# ─────────────────────────────────────────────────────────────
# Step 10 — Drive grants in /api/grants/incoming/resources.
#           Default UI calls pass `resource_types=file,folder` so
#           drives don't appear; explicit `resource_types=drive`
#           must surface the admin's Owner grant on their own drive.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants/incoming/resources?resource_types=drive
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.items"                count >= 1
jsonpath "$.items[0].resource_type" == "drive"


# ─────────────────────────────────────────────────────────────
# Step 11 — Anti-enum: Alice queries the members of admin's
#           personal drive. She has no Read on it → 404, same
#           shape as "drive doesn't exist". Operators see the
#           real reason in the audit log; she sees nothing.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives/{{admin_drive_id}}/members
Authorization: Bearer {{alice_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 12 — The same anti-enum shape for a UUID that doesn't
#           exist at all. Indistinguishable from step 11 to the
#           caller — the canonical no-leak response.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives/00000000-0000-0000-0000-000000000000/members
Authorization: Bearer {{alice_token}}

HTTP 404


# =============================================================
# D3a — POST /api/drives (create shared drive)
# =============================================================
# Below covers the create-shared-drive endpoint + the role-bundle
# tests that were deferred until shared-drive creation was wirable:
#
#  - admin-only authz gate
#  - kind=personal returns 501 (placeholder)
#  - Owner subject = user → single Owner shared drive
#  - Owner subject = group with members → group-mediated Owner
#  - Owner subject = empty group → 400 (no orphan Owner)
#  - Token subject refused
#  - Editor cascade: drive Owner can mutate content in the new drive
#  - Last-owner protection on removal
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 13 — Non-admin caller refused with 403.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "should-not-exist",
  "owner": { "type": "user", "id": "{{alice_user_id}}" }
}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 14 — kind=personal returns 501 (wire-shape placeholder).
#           The body is accepted as valid JSON; the rejection is
#           explicit at the service layer.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "personal",
  "name": "side-private",
  "owner": { "type": "user", "id": "{{alice_user_id}}" }
}

HTTP 501


# ─────────────────────────────────────────────────────────────
# Step 15 — Create a shared drive with a single user owner
#           (Alice). The new drive lands with kind=shared,
#           default_for_user=NULL, and Alice as the sole Owner.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "alice-shared",
  "owner": { "type": "user", "id": "{{alice_user_id}}" }
}

HTTP 201
[Asserts]
jsonpath "$.kind"             == "shared"
jsonpath "$.name"             == "alice-shared"
jsonpath "$.default_for_user" not exists
jsonpath "$.used_bytes"       == 0

[Captures]
alice_shared_drive_id: jsonpath "$.id"


# Alice now sees TWO drives — her default Personal + the new shared.
# Caller_role on the shared drive is "owner" (her user grant).
GET {{base_url}}/api/drives
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$"                                count == 2
jsonpath "$[*].id"                          contains {{alice_shared_drive_id}}


# ─────────────────────────────────────────────────────────────
# Step 16 — Last-owner protection: Alice is the sole Owner of
#           the new shared drive. Removing her grant must refuse.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/drives/{{alice_shared_drive_id}}/members/user/{{alice_user_id}}
Authorization: Bearer {{alice_token}}

HTTP 400


# And the demotion form: PATCH her role to editor → same refusal.
PATCH {{base_url}}/api/drives/{{alice_shared_drive_id}}/members/user/{{alice_user_id}}
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "role": "editor" }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 17 — Empty group is refused (would orphan the drive's Owner).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "empty-grp-for-drive" }

HTTP 201
[Captures]
empty_group_id: jsonpath "$.id"


POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "should-not-exist",
  "owner": { "type": "group", "id": "{{empty_group_id}}" }
}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 18 — Empty name is refused (basic validation).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "   ",
  "owner": { "type": "user", "id": "{{alice_user_id}}" }
}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 19 — Token subject is refused (drives can't be owned by
#           share-link tokens).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "should-not-exist",
  "owner": { "type": "token", "id": "00000000-0000-0000-0000-000000000099" }
}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 20 — Group-mediated Owner: create a group, add Alice, then
#           create a shared drive with the group as Owner. Alice
#           should see the new drive in her listing with
#           caller_role="owner" (resolved through the group).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "drive-grp-with-alice" }

HTTP 201
[Captures]
alice_group_id: jsonpath "$.id"


POST {{base_url}}/api/groups/{{alice_group_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "user_id": "{{alice_user_id}}" }

HTTP 201


POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "team-drive",
  "owner": { "type": "group", "id": "{{alice_group_id}}" }
}

HTTP 201
[Captures]
team_drive_id: jsonpath "$.id"
team_root_folder_id: jsonpath "$.root_folder_id"


# Alice's drive listing now includes the team drive with caller_role=owner.
# `MIN(role)` over (direct grants + group-mediated grants) resolves Owner.
GET {{base_url}}/api/drives
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" contains {{team_drive_id}}


# ─────────────────────────────────────────────────────────────
# Step 21 — Editor cascade through the drive precheck. Add a fresh
#           user (mbr_bob) as Editor on the team drive; he should
#           be able to read the drive root and create folders in it
#           via the drive's Editor permission bundle, without any
#           per-folder grant.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "mbr_bob", "password": "MbrBobPassword1!", "email": "mbr_bob@example.com", "role": "user" }

HTTP 201
[Captures]
bob_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mbr_bob", "password": "MbrBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


# Bob has no role on the team drive → drive doesn't appear in his listing.
GET {{base_url}}/api/drives
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" not contains {{team_drive_id}}


# Admin (well — Alice as drive Owner; admin would also work) grants Bob Editor.
POST {{base_url}}/api/drives/{{team_drive_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{bob_user_id}}" },
  "role": "editor"
}

HTTP 201


# Bob now sees the drive with caller_role=editor.
GET {{base_url}}/api/drives
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id"          contains {{team_drive_id}}


# ─────────────────────────────────────────────────────────────
# Step 21b — Upload gate by role (post-Drive AuthZ audit Round 2).
#            Bob is Editor on team_drive; `POST /api/files/upload`
#            targeting team_root_folder_id should succeed. This is
#            the REST-side counterpart of the WebDAV/NC PUT chain
#            hardened by `update_file_streaming_with_perms`. If
#            this fails, the whole role-bundle → Permission::Create
#            wiring is broken.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{bob_token}}
[MultipartFormData]
folder_id: {{team_root_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
bob_editor_upload_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 22 — Higher role wins: Bob now ALSO gets a Viewer direct
#           grant (would lower his bundle). The collapsed caller_role
#           must remain Editor (the stronger of his two grants).
# ─────────────────────────────────────────────────────────────
# Demote Bob to Viewer via PATCH — first ensure he was editor before
# (already confirmed via the GET above).
PATCH {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{bob_user_id}}
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "role": "viewer" }

HTTP 200
[Asserts]
jsonpath "$.role" == "viewer"


# Bob's listing now reflects the demotion.
GET {{base_url}}/api/drives
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" contains {{team_drive_id}}


# ─────────────────────────────────────────────────────────────
# Step 22b — Viewer CANNOT upload into a shared drive.
#            Post-Drive AuthZ audit Round 2: the create branch of
#            `update_file_streaming_with_perms` requires
#            `Permission::Create` on the parent folder — bundled
#            with `owner`/`editor`/`contributor` role_grants only,
#            NOT with `viewer`. `POST /api/files/upload` shares the
#            same `save_file_with_blob` gate. Bob has Read on the
#            drive (viewer role cascades) → graduated denial returns
#            403 (see [[project_authz_require_graduated_denial]]).
#            Also verify the batch / overwrite paths refuse — the
#            whole chain from drive-membership to file write is
#            exercised here.
# ─────────────────────────────────────────────────────────────

# 22b.i — Fresh file: 403.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{bob_token}}
[MultipartFormData]
folder_id: {{team_root_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 403


# 22b.ii — Overwrite attempt on the Editor-era upload: still 403.
#          `save_file_with_blob` catches the duplicate name at the
#          `Create`-permission check before the upsert races (which
#          would otherwise 409).
POST {{base_url}}/api/files/upload
Authorization: Bearer {{bob_token}}
[MultipartFormData]
folder_id: {{team_root_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 403


# 22b.iii — Alice's Editor-era file is untouched.
GET {{base_url}}/api/files/{{bob_editor_upload_id}}
Authorization: Bearer {{alice_token}}

HTTP 200


# =============================================================
# Per-role mutation matrix — what every role can / can't do
# =============================================================
# Setup state at this point:
#   - team_drive owners: alice (via alice_group) — sole Owner role grant
#   - team_drive Viewer: bob (user grant after Step 22 demotion)
#
# Steps 23-29 cover the per-role authorization matrix on member
# management + drive rename + content R/W. Anti-enum: every refusal
# returns 404 (not 403) so an unauthorised caller can't enumerate the
# difference between "drive doesn't exist" and "you can't manage it".


# ─────────────────────────────────────────────────────────────
# Step 23 — Owner CAN rename the drive.
#           Drive name lives on its root folder per drive.md §6,
#           renamed via PUT /api/folders/<root_folder_id>/rename.
#           Alice's Owner role (via her group) carries Update, so
#           the engine drive precheck grants the rename.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{team_root_folder_id}}/rename
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "team-drive-renamed" }

HTTP 200
[Asserts]
jsonpath "$.name" == "team-drive-renamed"


# The new name surfaces on the drive listing too — drive.name is
# sourced from the root folder per DriveDto::From<DriveWithRootName>.
GET {{base_url}}/api/drives
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
# Filter expressions in Hurl: `[?(...)]` collapses to a scalar when there's
# exactly one match — list-style predicates like `includes` / `contains` then
# fail with a type mismatch. So we assert string equality instead.
jsonpath "$[?(@.id=='{{team_drive_id}}')].name" == "team-drive-renamed"


# ─────────────────────────────────────────────────────────────
# Step 24 — Owner CAN edit owners, editors, and viewers.
#           Grant Carol Owner, promote Bob to Owner, then demote
#           Bob back to Viewer (the role he needs for Step 25).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "mbr_carol", "password": "MbrCarolPassword1!", "email": "mbr_carol@example.com", "role": "user" }

HTTP 201
[Captures]
carol_user_id: jsonpath "$.user.id"


# 24a — Owner grants Carol Owner role (Owner-creates-Owner).
POST {{base_url}}/api/drives/{{team_drive_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{carol_user_id}}" },
  "role": "owner"
}

HTTP 201
[Asserts]
jsonpath "$.role" == "owner"


# 24b — Owner promotes Bob (Viewer) to Owner.
PATCH {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{bob_user_id}}
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "role": "owner" }

HTTP 200
[Asserts]
jsonpath "$.role" == "owner"


# 24c — Owner demotes Bob back to Viewer (last-owner protection
#       allows it: Carol + Alice-via-group remain as Owners).
PATCH {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{bob_user_id}}
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "role": "viewer" }

HTTP 200
[Asserts]
jsonpath "$.role" == "viewer"


# ─────────────────────────────────────────────────────────────
# Step 25 — Viewer CANNOT edit drive members.
#           Bob is Viewer (has Read on the drive) → graduated denial
#           returns 403 on every member-mutation verb.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives/{{team_drive_id}}/members
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{alice_user_id}}" },
  "role": "editor"
}

HTTP 403


PATCH {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{carol_user_id}}
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "role": "viewer" }

HTTP 403


DELETE {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{carol_user_id}}
Authorization: Bearer {{bob_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 26 — Editor CANNOT edit drive members + CANNOT rename
#           the drive (rename = PUT on the drive's root folder).
#           Promote Bob to Editor first (Owner-driven).
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{bob_user_id}}
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "role": "editor" }

HTTP 200


# 26a — Editor POST /api/drives/{id}/members → 403 (Editor has Read).
POST {{base_url}}/api/drives/{{team_drive_id}}/members
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{alice_user_id}}" },
  "role": "viewer"
}

HTTP 403


# 26b — Editor PATCH a member → 403.
PATCH {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{carol_user_id}}
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "role": "viewer" }

HTTP 403


# 26c — Editor DELETE a member → 403.
DELETE {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{carol_user_id}}
Authorization: Bearer {{bob_token}}

HTTP 403


# 26d — Editor renames the drive (root folder) → 403.
#       Folder rename normally requires `Permission::Update` (which
#       Editor has on every folder in the drive via the engine's drive
#       precheck). The folder service promotes the requirement to
#       `Permission::Manage` when the target folder has `parent_id IS
#       NULL` — i.e. it's a drive root — so the drive-rename surface is
#       Owner-only per drive.md §6, without changing the public folder
#       endpoint shape. Editor has Read → graduated denial → 403.
PUT {{base_url}}/api/folders/{{team_root_folder_id}}/rename
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "name": "team-drive-editor-renamed" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 27 — Editor CAN modify content in the drive (positive).
#           Confirms the Editor bundle isn't accidentally too
#           restrictive — they can create folders under the drive
#           root via the drive precheck.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "name": "editor-created-folder",
  "parent_id": "{{team_root_folder_id}}"
}

HTTP 201
[Captures]
editor_created_folder_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "editor-created-folder"


# ─────────────────────────────────────────────────────────────
# Step 28 — Viewer CAN read content in the drive (positive).
#           Demote Bob back to Viewer, then confirm he can still
#           list the drive's root folder.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{bob_user_id}}
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "role": "viewer" }

HTTP 200


GET {{base_url}}/api/folders/{{team_root_folder_id}}
Authorization: Bearer {{bob_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 29 — A user with NO role on the drive cannot edit members.
#           Provision a fresh user (mbr_dave) with no grants on
#           the team drive; every member-mutation verb → 404.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "mbr_dave", "password": "MbrDavePassword1!", "email": "mbr_dave@example.com", "role": "user" }

HTTP 201
[Captures]
dave_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mbr_dave", "password": "MbrDavePassword1!" }

HTTP 200
[Captures]
dave_token: jsonpath "$.access_token"


# 29a — Non-member POST → 404 (the drive itself appears not to exist).
POST {{base_url}}/api/drives/{{team_drive_id}}/members
Authorization: Bearer {{dave_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{alice_user_id}}" },
  "role": "viewer"
}

HTTP 404


# 29b — Non-member PATCH → 404.
PATCH {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{carol_user_id}}
Authorization: Bearer {{dave_token}}
Content-Type: application/json
{ "role": "viewer" }

HTTP 404


# 29c — Non-member DELETE → 404.
DELETE {{base_url}}/api/drives/{{team_drive_id}}/members/user/{{carol_user_id}}
Authorization: Bearer {{dave_token}}

HTTP 404


# 29d — Non-member GET members → 404 too (anti-enum: no member-list leak).
GET {{base_url}}/api/drives/{{team_drive_id}}/members
Authorization: Bearer {{dave_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 30 — Drive delete (D3b).
#           - Non-Owner → 403 (Bob is Viewer post-Step 28, has Read).
#           - Owner on non-empty drive → 409 (the editor-created-folder
#             from Step 27 is still live).
#           - Owner after the folder is trashed → 204.
#           Personal-drive refusal (default_for_user IS NOT NULL) is
#           covered separately — `mbr_dave` keeps his default drive,
#           we exercise its 405 below.
# ─────────────────────────────────────────────────────────────

# 30a — Viewer (Bob) cannot delete the drive → 403 (has Read).
DELETE {{base_url}}/api/drives/{{team_drive_id}}
Authorization: Bearer {{bob_token}}

HTTP 403


# 30b — Owner (Alice) on a non-empty drive → 409 with the canonical
#       "drive_not_empty" reason in the audit log.
DELETE {{base_url}}/api/drives/{{team_drive_id}}
Authorization: Bearer {{alice_token}}

HTTP 409


# 30c — Clear the lingering content (the Editor-created folder from
#       Step 27 and the Editor-era file from Step 21b). Delete via
#       the regular endpoints so rows land in trash, not the live
#       tree; `is_empty` excludes trashed rows so a populated trash
#       bin is allowed.
DELETE {{base_url}}/api/folders/{{editor_created_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{bob_editor_upload_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# 30d — Owner on an empty drive → 204.
DELETE {{base_url}}/api/drives/{{team_drive_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# 30e — Drive is gone; subsequent reads return 404.
GET {{base_url}}/api/drives/{{team_drive_id}}/members
Authorization: Bearer {{alice_token}}

HTTP 404


# 30f — Default Personal drive — Dave's home — cannot be deleted.
#       Look up the drive id via the picker listing. Dave is a fresh
#       user and only has his default personal drive, so `$[0].id`
#       is unambiguous. (Avoiding the `[?(...)]` filter — Hurl
#       collapses single-match results to a scalar, which breaks
#       `nth` / list-style assertions; see memory.)
GET {{base_url}}/api/drives
Authorization: Bearer {{dave_token}}

HTTP 200
[Captures]
dave_default_drive_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$[0].default_for_user" == "{{dave_user_id}}"

DELETE {{base_url}}/api/drives/{{dave_default_drive_id}}
Authorization: Bearer {{dave_token}}

HTTP 405
