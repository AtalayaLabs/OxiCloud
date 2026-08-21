# =============================================================
# OxiCloud – D5 drive policies: `forbid_public_links`
# =============================================================
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/drive_policies.hurl
#
# The model under test (`docs/plan/drive.md` §8):
#   Each drive carries a `policies` JSONB. Five known keys, all
#   default-false. The first key shipped is `forbid_public_links`,
#   which blocks anonymous token-share creation on every resource
#   in the drive. Enforced at `share_service::create_shared_link`;
#   mutated by `PATCH /api/drives/{id}/policies` (OxiCloud-admin
#   only — the carve-out closes the self-policing-soft-cap hole
#   where an owner could disable a policy, share, and re-enable).
#
# Cases:
#   1. Baseline — policy off → POST /api/shares succeeds (201).
#   2. Owner flips `forbid_public_links` via PATCH → 200,
#      response echoes the merged bag.
#   3. Policy on → POST /api/shares refused with
#      OperationNotSupported (405) and the share row is NOT created.
#   4. Owner flips the policy back off → POST /api/shares succeeds
#      again (proves merge semantics; the typed write doesn't
#      clobber unrelated keys).
#
# Self-contained: provisions `dp_owner` so it can run alongside
# the rest of the suite. The user's default Personal drive is
# the test surface — the policy applies equally to personal and
# shared drives (`Owner` bundle includes "edit policies").
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
# Step 2 — Provision `dp_owner`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "dp_owner",
  "password": "DpOwnerPwd1!",
  "email":    "dp_owner@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dp_owner", "password": "DpOwnerPwd1!" }

HTTP 200
[Captures]
owner_token:   jsonpath "$.access_token"
owner_user_id: jsonpath "$.user.full.user.id"


# Provision `dp_intruder` — a second internal user used only to
# exercise the negative side of the policy-PATCH authz gate.
# A separate user (not bob, who's external) keeps internal/external
# semantics out of the assertion.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "dp_intruder",
  "password": "DpIntruderPwd1!",
  "email":    "dp_intruder@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dp_intruder", "password": "DpIntruderPwd1!" }

HTTP 200
[Captures]
intruder_token:   jsonpath "$.access_token"
intruder_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Find the user's default Personal drive + root.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{owner_token}}

HTTP 200
[Captures]
personal_root_id: jsonpath "$[0].id"

GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Captures]
personal_drive_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$[0].kind" == "personal"


# ─────────────────────────────────────────────────────────────
# Step 4 — Seed a file to share.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Case 1: baseline. Policy off → POST /api/shares OK.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/shares
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "item_id":   "{{file_id}}",
  "item_type": "file"
}

HTTP 201
[Captures]
baseline_share_id: jsonpath "$.id"


# Clean up the baseline share so the policy-on case starts fresh.
DELETE {{base_url}}/api/shares/{{baseline_share_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 6 — Case 2: flip `forbid_public_links` on.
#          PATCH returns the merged bag.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_public_links": true
}

HTTP 200
[Asserts]
jsonpath "$.forbid_public_links"        == true
jsonpath "$.forbid_sharing"             == false
jsonpath "$.forbid_external_sharing"    == false
jsonpath "$.forbid_cross_drive_move"    == false


# Authz gate — negative case. The PATCH is OxiCloud-admin only.
# Anything below admin role gets a 404 (anti-enum — same shape as
# "drive does not exist", so a probe can't tell apart "no such
# drive" from "policies are admin-managed").
#
# The most important assertion: even the drive's OWNER can no
# longer mutate policies. Before this change the policies were
# owner-mutable, which made them self-policing soft caps (an
# owner could disable forbid_external_sharing, share, re-enable).
# Mirroring `drives.quota_bytes` and `users.storage_quota_bytes`
# admin-only carve-outs.
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "forbid_public_links": false
}

HTTP 404

# And a non-member also gets 404 (same anti-enum shape).
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{intruder_token}}
Content-Type: application/json
{
  "forbid_public_links": false
}

HTTP 404


# Belt-and-braces: the policy that admin set is unchanged
# (no partial write happened under the failed authz).
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[0].policies.forbid_public_links" == true


# ─────────────────────────────────────────────────────────────
# Step 7 — Case 3: policy on → POST /api/shares refused (405).
#          DomainError::operation_not_supported maps to HTTP 405
#          (Method Not Allowed) per the interface error map.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/shares
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "item_id":   "{{file_id}}",
  "item_type": "file"
}

HTTP 405


# Closing the bypass: `POST /api/grants` with `subject.type=token`
# would otherwise mint an anonymous-link grant — same effect as a
# token share, different surface. `grant_handler` now routes
# Token subjects through `DrivePolicies::refuse_public_links`,
# so the policy gates both surfaces. The token UUID is invented
# (no validation up to this point) — the refusal must fire from
# the policy check, not from a missing-token lookup.
POST {{base_url}}/api/grants
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject":  { "type": "token", "id": "00000000-0000-0000-0000-000000000bad" },
  "resource": { "type": "file",  "id": "{{file_id}}" },
  "role":     "viewer"
}

HTTP 405


# Confirm no share row was created — the listing on this file
# is empty.
GET {{base_url}}/api/shares?item_id={{file_id}}&item_type=file
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$" count == 0


# ─────────────────────────────────────────────────────────────
# Step 8 — Case 4: flip the policy back off → share succeeds.
#          Proves the partial-merge: setting `forbid_public_links`
#          to false doesn't touch unrelated keys (still false here,
#          but the round-trip exercises the merge path).
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_public_links": false
}

HTTP 200
[Asserts]
jsonpath "$.forbid_public_links" == false


POST {{base_url}}/api/shares
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "item_id":   "{{file_id}}",
  "item_type": "file"
}

HTTP 201
[Captures]
final_share_id: jsonpath "$.id"

DELETE {{base_url}}/api/shares/{{final_share_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 9 — `forbid_external_sharing` baseline + early refuse.
#          Owner shares a folder by email — succeeds, lazily
#          provisions the external user. Then toggle the policy
#          on and try a fresh email — refused BEFORE the
#          external user is created (early gate prevents the
#          side-effect leak).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "name": "dp-ext-share", "parent_id": "{{personal_root_id}}" }

HTTP 201
[Captures]
ext_folder_id: jsonpath "$.id"


# Baseline: email grant succeeds with policy off. Captures the
# resolved bob_user_id so the LATE gate can be exercised below.
POST {{base_url}}/api/grants
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject":  { "type": "email", "email": "dp_bob@externalcompany.com" },
  "resource": { "type": "folder", "id": "{{ext_folder_id}}" },
  "role":     "viewer"
}

HTTP 201
[Captures]
bob_user_id: jsonpath "$.grants[0].subject.id"


# Toggle `forbid_external_sharing` on.
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_external_sharing": true
}

HTTP 200
[Asserts]
jsonpath "$.forbid_external_sharing" == true
jsonpath "$.forbid_public_links"     == false


# Early gate: email subject refused before any user row is created.
# The grant.rejected audit line fires with reason=forbid_external_sharing
# stage=early_email.
POST {{base_url}}/api/grants
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject":  { "type": "email", "email": "dp_alice@externalcompany.com" },
  "resource": { "type": "folder", "id": "{{ext_folder_id}}" },
  "role":     "viewer"
}

HTTP 405


# ─────────────────────────────────────────────────────────────
# Step 10 — `forbid_external_sharing` late refuse: even passing
#           an existing external user by id is refused (closes
#           the user-by-id loophole).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "folder", "id": "{{ext_folder_id}}" },
  "role":     "viewer"
}

HTTP 405


# Flip the policy back off — same subject now succeeds, proving
# the refusal was policy-driven and not a permanent block.
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_external_sharing": false
}

HTTP 200
[Asserts]
jsonpath "$.forbid_external_sharing" == false


POST {{base_url}}/api/grants
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "folder", "id": "{{ext_folder_id}}" },
  "role":     "viewer"
}

HTTP 201

# ─────────────────────────────────────────────────────────────
# Step 10b — `forbid_sharing` on a personal drive: per-resource
#            grants on resources inside the drive are refused;
#            drive-level membership stays unaffected (covered by
#            the shared-drive positive control in Step 11 below).
#
# This is the broadest D5 policy — toggling it on locks the drive
# to "drive membership only" sharing semantics (§8: "no fine-
# grained sharing of individual files; access happens through
# drive membership only").
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_sharing": true
}

HTTP 200
[Asserts]
jsonpath "$.forbid_sharing"             == true
jsonpath "$.forbid_external_sharing"    == false
jsonpath "$.forbid_public_links"        == false


# File-grant refused. `grant.rejected reason=forbid_sharing`.
POST {{base_url}}/api/grants
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "file",  "id": "{{file_id}}" },
  "role":     "viewer"
}

HTTP 405


# Folder-grant refused with the same shape.
POST {{base_url}}/api/grants
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "folder", "id": "{{ext_folder_id}}" },
  "role":     "viewer"
}

HTTP 405


# Flip the policy off — the same folder-grant now succeeds, proving
# refusal was policy-driven.
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_sharing": false
}

HTTP 200

POST {{base_url}}/api/grants
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "folder", "id": "{{ext_folder_id}}" },
  "role":     "viewer"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 10c — partial-merge regression guard.
#
# The `PATCH /api/drives/{id}/policies` handler documents that
# omitting a field means "leave it alone", not "set it to false".
# Prior implementation round-tripped the wire body through the
# typed `DrivePolicies` struct (which has `#[serde(default)]`, so
# every omitted field defaults to `false`) and then serialised the
# whole struct into the JSONB `||` merge — silently clobbering
# every unmentioned flag back to `false`. This step exercises
# multi-flag interaction so that regression can't creep back:
#
#   1. Set `forbid_sharing = true`, assert the bag.
#   2. In a SEPARATE PATCH, set only `forbid_public_links = true`.
#   3. Assert `forbid_sharing` STILL reads `true` in the response
#      — proving the merge honoured "leave omitted keys alone".
#
# Reset both back to false at the end so the shared-drive steps
# below start from a clean state.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_sharing": true
}

HTTP 200
[Asserts]
jsonpath "$.forbid_sharing"      == true
jsonpath "$.forbid_public_links" == false

PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_public_links": true
}

HTTP 200
[Asserts]
# The load-bearing assertion — `forbid_sharing` must NOT have been
# clobbered by the omitted-key regression.
jsonpath "$.forbid_sharing"      == true
jsonpath "$.forbid_public_links" == true

# Reset both.
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_sharing":      false,
  "forbid_public_links": false
}

HTTP 200
[Asserts]
jsonpath "$.forbid_sharing"      == false
jsonpath "$.forbid_public_links" == false


# ─────────────────────────────────────────────────────────────
# Step 11 — `forbid_external_sharing` on a SHARED drive, via
#           `POST /api/drives/{id}/members`.
#
# Coverage gap closed: the earlier steps exercise the
# grant_handler path (File/Folder grants in dp_owner's personal
# drive). The drive-membership route bypasses grant_handler and
# calls `DriveManagementService::set_member_role` directly —
# `refuse_if_forbid_external_sharing` enforces the same gate at
# the service layer (`docs/plan/drive.md` §8). This step proves
# the route is gated.
#
# Personal drives refuse `add_member` regardless of policy (§2),
# so a shared drive is required. Admin provisions one with
# dp_owner as direct user-Owner.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "dp-shared",
  "owner": { "type": "user", "id": "{{owner_user_id}}" }
}

HTTP 201
[Captures]
shared_drive_id: jsonpath "$.id"
shared_root_id:  jsonpath "$.root_folder_id"


# Toggle `forbid_external_sharing` on the SHARED drive (dp_owner
# is Owner → carries Manage in the role bundle).
PATCH {{base_url}}/api/drives/{{shared_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_external_sharing": true
}

HTTP 200
[Asserts]
jsonpath "$.forbid_external_sharing" == true


# Adding bob (existing external user from Step 9) as a Viewer
# via the drive-membership route is refused by
# `set_member_role`'s `refuse_if_forbid_external_sharing` —
# `grant.rejected reason=forbid_external_sharing stage=drive_member`.
POST {{base_url}}/api/drives/{{shared_drive_id}}/members
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{bob_user_id}}" },
  "role":    "viewer"
}

HTTP 405


# Flip the policy off — same call succeeds, proving the refusal
# was policy-driven (not a permanent block) and that the gate at
# the service layer can be lifted by the drive owner.
PATCH {{base_url}}/api/drives/{{shared_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_external_sharing": false
}

HTTP 200

POST {{base_url}}/api/drives/{{shared_drive_id}}/members
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{bob_user_id}}" },
  "role":    "viewer"
}

HTTP 201


# Authz gate — non-Owner role on a SHARED drive still can't change
# policies. Add `dp_intruder` as Editor (bundle includes Update on
# resources in the drive but NOT Manage), then have them try to
# flip a policy → 404. Proves the PATCH endpoint requires Manage
# specifically, not just any drive role.
POST {{base_url}}/api/drives/{{shared_drive_id}}/members
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{intruder_user_id}}" },
  "role":    "editor"
}

HTTP 201

PATCH {{base_url}}/api/drives/{{shared_drive_id}}/policies
Authorization: Bearer {{intruder_token}}
Content-Type: application/json
{
  "forbid_external_sharing": true
}

HTTP 404


# Belt-and-braces: dp_intruder's failed PATCH didn't side-effect.
# dp_owner reads the drive's policies (canonical owner view) and
# `forbid_external_sharing` stays at the value the owner last set
# (false — flipped back two requests ago).
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}
[QueryStringParams]

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{shared_drive_id}}')].policies.forbid_external_sharing" == false


# `forbid_sharing` carve-out positive control. The policy locks
# per-resource sharing but leaves drive-level membership working
# (§8 — "access happens through drive membership only"). Toggle
# it on, then add a new drive member: must succeed (201). This is
# the assertion that grant_handler skips the gate for
# `Resource::Drive(_)`.
PATCH {{base_url}}/api/drives/{{shared_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_sharing": true
}

HTTP 200
[Asserts]
jsonpath "$.forbid_sharing" == true

# `dp_owner` is already Owner; bob is Viewer; dp_intruder is
# Editor. Re-grant dp_intruder Editor — UPSERT through
# `set_member_role` — under `forbid_sharing=true`. The carve-out
# means this still works.
POST {{base_url}}/api/drives/{{shared_drive_id}}/members
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{intruder_user_id}}" },
  "role":    "editor"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 11b — `forbid_cross_drive_move` on the SOURCE drive
#            refuses moves to a different drive. dp_owner is
#            Owner of both the personal and shared drives, so
#            authz on both ends passes — the refusal must come
#            from the policy gate, not a permission failure.
#
# The policy lives on the SOURCE drive (the one losing the
# content). It's also fetched into the service via
# `get_drive_id_and_policies_for_file`, so the same call site
# proves the lookup works end-to-end.
#
# Clean up `forbid_sharing` first — it would refuse the per-
# resource-grant-style mutations the move tests don't actually
# do, but the test should isolate one policy at a time.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_cross_drive_move": true
}

HTTP 200
[Asserts]
jsonpath "$.forbid_cross_drive_move" == true


# Attempt to move the file from dp_owner's personal drive into
# the shared drive's root folder. Both Update (file) and Create
# (folder) authz pass — dp_owner is Owner of both drives. The
# gate fires `move.rejected reason=forbid_cross_drive_move`.
PUT {{base_url}}/api/files/{{file_id}}/move
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "folder_id": "{{shared_root_id}}"
}

HTTP 405


# Confirm the file stayed put on the source drive (no partial
# move under the failed gate).
GET {{base_url}}/api/files?folder_id={{personal_root_id}}
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{file_id}}')].folder_id" == "{{personal_root_id}}"


# Flip the policy off — same call now succeeds and the file
# lands in the shared drive's root.
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_cross_drive_move": false
}

HTTP 200

PUT {{base_url}}/api/files/{{file_id}}/move
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "folder_id": "{{shared_root_id}}"
}

HTTP 200


# Move the file back to dp_owner's personal drive so the shared
# drive cleanup's empty-before-delete guard passes.
PUT {{base_url}}/api/files/{{file_id}}/move
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "folder_id": "{{personal_root_id}}"
}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 11c — `forbid_owner_role_change` locks the Owner roster
#            against owner mutation. Only OxiCloud admin can
#            change the Owner set when this policy is on.
#
# Fixture at this point: dp_owner is Owner on the shared drive,
# dp_intruder is Editor (from Step 11), bob is Viewer
# (re-granted earlier). Admin enables the policy; dp_owner is
# refused on every Owner-touching mutation; non-Owner mutations
# still work; admin override always succeeds.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{shared_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_owner_role_change": true
}

HTTP 200
[Asserts]
jsonpath "$.forbid_owner_role_change" == true


# dp_owner attempts to promote dp_intruder Editor → Owner.
# Refused by `refuse_if_forbid_owner_role_change` —
# `drive_membership.rejected reason=forbid_owner_role_change`.
POST {{base_url}}/api/drives/{{shared_drive_id}}/members
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{intruder_user_id}}" },
  "role":    "owner"
}

HTTP 405


# dp_owner can still mutate non-Owner roles. Re-grant bob as
# Viewer (UPSERT) under the policy → 201. Proves the carve-out
# is narrow — only Owner-roster writes are gated.
POST {{base_url}}/api/drives/{{shared_drive_id}}/members
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{bob_user_id}}" },
  "role":    "viewer"
}

HTTP 201


# Admin override: admin promotes dp_intruder to Owner. Same
# call shape, just admin's token — must succeed (admin is the
# tenant operator and the only one who can change the roster).
POST {{base_url}}/api/admin/drives/{{shared_drive_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{intruder_user_id}}" },
  "role":    "owner"
}

HTTP 201


# Now dp_intruder IS an Owner. dp_owner attempts to demote them
# back to Editor — refused, even though dp_owner is also an
# Owner (the policy is roster-wide, not per-owner).
POST {{base_url}}/api/drives/{{shared_drive_id}}/members
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{intruder_user_id}}" },
  "role":    "editor"
}

HTTP 405


# dp_owner attempts to remove dp_intruder entirely — refused
# (the subject IS currently Owner, so removal counts as Owner
# roster mutation).
DELETE {{base_url}}/api/drives/{{shared_drive_id}}/members/user/{{intruder_user_id}}
Authorization: Bearer {{owner_token}}

HTTP 405


# Admin override: admin removes dp_intruder. Cleans up the
# Owner roster back to {dp_owner} so the empty-before-delete
# guard below succeeds.
DELETE {{base_url}}/api/admin/drives/{{shared_drive_id}}/members/user/{{intruder_user_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# Disable the policy so the shared-drive cleanup below isn't
# distorted by lingering owner-lock state.
PATCH {{base_url}}/api/drives/{{shared_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "forbid_owner_role_change": false
}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 11d — `include_in_photo_index` scope opt-in (§15).
#
# Default personal drives are seeded with the flag = true by the
# `PersonalDriveLifecycleHook` + backfill migration
# (20260901000000_default_personal_photo_music_flags.sql). Non-
# default drives (shared, secondary personals) start opted-out
# and only surface in `/api/photos` after an admin flips the
# flag on via PATCH.
#
# Coverage:
#   a. Upload a PNG into dp_owner's default Personal drive →
#      surfaces in `/api/photos` (default-personal auto-opted in).
#   b. Upload a PNG into the shared drive → does NOT surface
#      (flag omitted).
#   c. Admin flips `include_in_photo_index=true` on the shared
#      drive → the shared-drive PNG surfaces in `/api/photos`.
#
# `/api/photos` returns a flat array of PhotoDto — each carries
# the file's `id`. Assertions use `jsonpath "$[*].id" contains
# "…"` to sidestep the single-match filter quirks
# (feedback_hurl_jsonpath_filter_empty).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/blue-image.png; image/png

HTTP 201
[Captures]
personal_photo_id: jsonpath "$.id"


# Baseline — personal-drive photo is visible in the timeline.
GET {{base_url}}/api/photos
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" contains "{{personal_photo_id}}"


# Upload a PNG into the SHARED drive's root. dp_owner is Owner
# on the shared drive from earlier steps, so Create passes.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{shared_root_id}}
file: file,fixtures/red-image.png; image/png

HTTP 201
[Captures]
shared_photo_id: jsonpath "$.id"


# Shared drive is NOT opted-in yet — the shared photo must be
# absent from `/api/photos`. The personal photo stays visible.
GET {{base_url}}/api/photos
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" not contains "{{shared_photo_id}}"
jsonpath "$[*].id" contains "{{personal_photo_id}}"


# Flip `include_in_photo_index=true` on the shared drive.
PATCH {{base_url}}/api/drives/{{shared_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "include_in_photo_index": true
}

HTTP 200
[Asserts]
jsonpath "$.include_in_photo_index" == true


# Shared-drive photo now surfaces in `/api/photos`. Personal
# photo remains visible — no regression on the always-in-scope
# default drive.
GET {{base_url}}/api/photos
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" contains "{{shared_photo_id}}"
jsonpath "$[*].id" contains "{{personal_photo_id}}"


# Cleanup — both photos so the shared-drive delete below finds
# an empty drive. The personal-drive photo cascade-deletes with
# dp_owner in Step 12; we still remove it here so the delete
# path is exercised explicitly (deletes don't affect the flag).
DELETE {{base_url}}/api/files/{{shared_photo_id}}
Authorization: Bearer {{owner_token}}

HTTP 204

DELETE {{base_url}}/api/files/{{personal_photo_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# Cleanup the shared drive: empty (no content was added) → delete
# via DELETE /api/drives/{id}. dp_owner is Owner so the call
# carries Manage; the per-drive empty-before-delete guard passes
# trivially (the drive holds only its root folder).
DELETE {{base_url}}/api/drives/{{shared_drive_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 12 — Final cleanup. Admin deletes bob, dp_intruder, and
#           dp_owner. Each cascade reaps that user's default
#           personal drive + their grant rows.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{bob_user_id}}
Authorization: Bearer {{admin_token}}

HTTP 200

DELETE {{base_url}}/api/admin/users/{{intruder_user_id}}
Authorization: Bearer {{admin_token}}

HTTP 200

DELETE {{base_url}}/api/admin/users/{{owner_user_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
