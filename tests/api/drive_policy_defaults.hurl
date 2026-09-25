# =============================================================
# OxiCloud – per-drive-kind DEFAULT policies + live inheritance
# =============================================================
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/drive_policy_defaults.hurl
#
# The model under test (`docs/plan/drive-default-policies.md`):
#   `storage.drive_policy_defaults` holds one row per drive kind.
#   `storage.drives.policies` holds ONLY the knobs an admin set on
#   that drive. Effective policy = default(kind) || overrides,
#   resolved at read time — so tightening a default reaches every
#   drive except where someone deliberately overrode that knob,
#   PER KNOB rather than per drive.
#
# Cases:
#   1.  Admin-only: a normal user gets the anti-enum 404, never 403.
#   2.  Unknown kind → 400.
#   3.  `read_only` is refused as a default (operational state, not
#       a posture — a default that froze every drive from one
#       toggle has no legitimate use).
#   4.  `forbid_owner_role_change` is refused on PERSONAL (moot —
#       membership there is immutable regardless) but accepted on
#       SHARED.
#   5.  An unknown knob is refused rather than stored-and-ignored.
#   6.  A new drive INHERITS the shared default.
#   7.  A per-drive override wins for that knob AND the drive still
#       inherits the knobs it did not override.
#   8.  Changing the default moves the inheriting drive and leaves
#       the overriding one alone.
#   9.  The two kinds are independent — setting one never moves the
#       other.
#   10. `?dry_run=true` reports impact WITHOUT writing.
#
# Self-contained: provisions `dpd_user`, and restores both kind
# defaults to their seeded values in teardown. Leaving a tightened
# default behind would silently change every later file in the
# suite — the shares tests in particular.
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
# Needed as the `owner` subject on drive creation — the id lives on
# the login payload, under the three-layer UserDto split.
admin_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Provision a non-admin, for the authorization case.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "dpd_user",
  "password": "DpdUserPwd1!",
  "email":    "dpd_user@example.com",
  "role":     "user"
}

HTTP 201


# The id comes from the LOGIN response, not from creation — the
# admin create endpoint does not surface one in a usable shape.
# `$.user.full.user.id` is the three-layer UserDto split.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dpd_user", "password": "DpdUserPwd1!" }

HTTP 200
[Captures]
user_token:   jsonpath "$.access_token"
dpd_user_id:  jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Capture the seeded defaults so teardown can restore
#          them exactly. Personal ships with the two index flags
#          on; shared ships empty.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.forbid_public_links" == false


GET {{base_url}}/api/admin/drive-policies/defaults/personal
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# The seeded personal default — moved here from the hardcoded
# INSERT literal, so upgrading changed no drive's behaviour.
jsonpath "$.include_in_photo_index" == true
jsonpath "$.include_in_music_index" == true


# ─────────────────────────────────────────────────────────────
# Step 4 — A non-admin gets 403.
#
# 403, not the anti-enum 404 the per-drive policy endpoint uses.
# The two differ for a reason: `PATCH /api/drives/{id}/policies`
# names a specific drive whose existence must not leak, so it
# answers as if the drive were absent. This route names no
# resource — the admin API's existence is not a secret, and every
# other `/api/admin` route answers the same way via the
# `require_admin` layer (audit event `authz.admin_denied`).
#
# Both verbs, because admin is a property of the whole subtree
# rather than of reads.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{user_token}}

HTTP 403


PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{user_token}}
Content-Type: application/json
{ "forbid_public_links": true }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 5 — Input validation. Each of these is refused rather than
#          stored and ignored: a setting silently dropped is one
#          the admin believes they made.
# ─────────────────────────────────────────────────────────────

# Unknown drive kind.
GET {{base_url}}/api/admin/drive-policies/defaults/vault
Authorization: Bearer {{admin_token}}

HTTP 400


# `read_only` is not defaultable on either kind.
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "read_only": true }

HTTP 400


PUT {{base_url}}/api/admin/drive-policies/defaults/personal
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "read_only": true }

HTTP 400


# Unknown knob.
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_telepathy": true }

HTTP 400


# `forbid_owner_role_change` is moot on a personal drive — refused
# there, accepted on shared. The asymmetry is the point.
PUT {{base_url}}/api/admin/drive-policies/defaults/personal
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_owner_role_change": true }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 6 — Set a shared default, then create a drive under it.
#          The new drive carries NO overrides, so every knob
#          resolves through the default.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_public_links": true, "max_public_link_days": 30 }

HTTP 200
[Asserts]
jsonpath "$.forbid_public_links" == true
jsonpath "$.max_public_link_days" == 30


POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind":  "shared",
  "name":  "dpd_inheritor",
  "owner": { "type": "user", "id": "{{admin_user_id}}" }
}

HTTP 201
[Captures]
inheritor_id: jsonpath "$.id"
inheritor_root: jsonpath "$.root_folder_id"
[Asserts]
# The drive's OWN bag is empty — it decided nothing…
jsonpath "$.policies.forbid_public_links" == true


# …yet the policy is IN FORCE, because it resolves through the
# default. Asserted by behaviour rather than by reading the bag
# back: enforcement is what actually matters, and there is no
# `GET /api/drives/{id}` to read from anyway.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{inheritor_root}}",
  "item_type": "folder",
  "item_name": "dpd_inherited_refusal"
}

HTTP 405


# ─────────────────────────────────────────────────────────────
# Step 7 — A per-drive override wins for THAT knob only.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind":  "shared",
  "name":  "dpd_overrider",
  "owner": { "type": "user", "id": "{{admin_user_id}}" }
}

HTTP 201
[Captures]
overrider_id: jsonpath "$.id"
overrider_root: jsonpath "$.root_folder_id"


PATCH {{base_url}}/api/drives/{{overrider_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_public_links": false }

HTTP 200
[Asserts]
# The override wins…
jsonpath "$.forbid_public_links" == false
# …and the knob it did NOT override still follows the default.
# Per-knob inheritance, not per-drive: overriding one setting must
# not detach the drive from every other one.
jsonpath "$.max_public_link_days" == 30


# Behavioural proof of both halves at once. This drive's override
# permits links (the inheritor above was refused outright), so a
# refusal HERE can only come from the 30-day cap it inherited —
# a knob it never set. Year-3015 expiry, comfortably over.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":    "{{overrider_root}}",
  "item_type":  "folder",
  "item_name":  "dpd_over_cap",
  "expires_at": 33000000000
}

HTTP 405


# ─────────────────────────────────────────────────────────────
# Step 8 — Changing the default moves the inheritor and leaves
#          the overrider alone. This is the case that makes a
#          default worth having.
# ─────────────────────────────────────────────────────────────
# Generous cap first, so the SAME request can be shown succeeding
# and then failing. A fixed far-future expiry keeps the test from
# rotting; "within the cap" cannot be expressed as a literal
# relative to now.
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_public_links": false, "max_public_link_days": 3650 }

HTTP 200


# 2027-01-01 — comfortably inside a 3650-day cap.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":    "{{overrider_root}}",
  "item_type":  "folder",
  "item_name":  "dpd_within_cap",
  "expires_at": 1798761600
}

HTTP 201
[Captures]
within_cap_share_id: jsonpath "$.id"


# Tighten the DEFAULT — a knob this drive never set.
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_public_links": false, "max_public_link_days": 7 }

HTTP 200


# The identical request is now refused. Same drive, same body,
# same override — only the inherited knob moved. That contrast is
# live inheritance working.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":    "{{overrider_root}}",
  "item_type":  "folder",
  "item_name":  "dpd_now_over_cap",
  "expires_at": 1798761600
}

HTTP 405


# The override itself is untouched by the default change.
PATCH {{base_url}}/api/drives/{{overrider_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200
[Asserts]
jsonpath "$.forbid_public_links" == false
jsonpath "$.max_public_link_days" == 7


DELETE {{base_url}}/api/shares/{{within_cap_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 9 — The two kinds are independent settings. Moving the
#          shared default must not touch personal.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/drive-policies/defaults/personal
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.forbid_public_links" == false
jsonpath "$.include_in_photo_index" == true


# ─────────────────────────────────────────────────────────────
# Step 10 — `?dry_run=true` reports impact and writes NOTHING.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/drive-policies/defaults/shared?dry_run=true
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_public_links": true, "max_public_link_days": 1 }

HTTP 200
[Asserts]
# The overriding drive stays laxer than the candidate default, so
# it is counted — this is the number the UI shows before saving.
jsonpath "$.drives_weaker_than_default" >= 1
jsonpath "$.by_knob" isCollection


# Nothing was written: the live default still carries the value
# from Step 8, not the previewed 1.
GET {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.max_public_link_days" == 7


# ─────────────────────────────────────────────────────────────
# Step 11 — The two public-link knobs, exercised directly.
#
# Set PER DRIVE rather than via the default, so these assertions
# are about the gates themselves and not about inheritance
# (covered above). Both share one enforcement site alongside
# `forbid_public_links`, and all three refuse with 405.
# ─────────────────────────────────────────────────────────────

# Clear the shared default so only the per-drive values are in
# play — otherwise the inherited 7-day cap from Step 8 would be
# doing the refusing and these assertions would prove nothing.
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200


# ── require_public_link_password ─────────────────────────────
PATCH {{base_url}}/api/drives/{{overrider_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "require_public_link_password": true }

HTTP 200
[Asserts]
jsonpath "$.require_public_link_password" == true


# No password → refused.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{overrider_root}}",
  "item_type": "folder",
  "item_name": "dpd_no_password"
}

HTTP 405


# An EMPTY password is not a password — the gate treats "" the
# same as absent, so a client sending a blank field cannot slip
# past a policy that exists to require a secret.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{overrider_root}}",
  "item_type": "folder",
  "item_name": "dpd_blank_password",
  "password":  ""
}

HTTP 405


# With a password → allowed.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{overrider_root}}",
  "item_type": "folder",
  "item_name": "dpd_with_password",
  "password":  "LinkPwd1!"
}

HTTP 201
[Captures]
pwd_share_id: jsonpath "$.id"


DELETE {{base_url}}/api/shares/{{pwd_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# Turn it back off so the cap case below is isolated.
PATCH {{base_url}}/api/drives/{{overrider_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "require_public_link_password": false }

HTTP 200


# ── max_public_link_days ─────────────────────────────────────
PATCH {{base_url}}/api/drives/{{overrider_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "max_public_link_days": 3650 }

HTTP 200
[Asserts]
jsonpath "$.max_public_link_days" == 3650


# 2027-01-01, inside a 3650-day cap → allowed.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":    "{{overrider_root}}",
  "item_type":  "folder",
  "item_name":  "dpd_cap_ok",
  "expires_at": 1798761600
}

HTTP 201
[Captures]
cap_ok_share_id: jsonpath "$.id"


DELETE {{base_url}}/api/shares/{{cap_ok_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# NO expiry under a cap → refused. A link that never expires is
# the laxest value there is, so "unset" cannot mean "exempt" —
# this is the case most likely to be implemented backwards.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{overrider_root}}",
  "item_type": "folder",
  "item_name": "dpd_no_expiry_under_cap"
}

HTTP 405


# Tighten to 1 day — the same 2027 request is now over.
PATCH {{base_url}}/api/drives/{{overrider_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "max_public_link_days": 1 }

HTTP 200


POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":    "{{overrider_root}}",
  "item_type":  "folder",
  "item_name":  "dpd_cap_exceeded",
  "expires_at": 1798761600
}

HTTP 405


# Clear the cap: no cap means no cap, so the same request that
# just failed now succeeds. Proves the refusal came from the
# policy rather than from anything about the request.
PATCH {{base_url}}/api/drives/{{overrider_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "max_public_link_days": null }

HTTP 200


POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":    "{{overrider_root}}",
  "item_type":  "folder",
  "item_name":  "dpd_cap_cleared",
  "expires_at": 1798761600
}

HTTP 201
[Captures]
cap_cleared_share_id: jsonpath "$.id"


DELETE {{base_url}}/api/shares/{{cap_cleared_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 12 — Tighten a policy UNDER an existing share, and prove
#           the scan reports it.
#
# This is the case the whole feature exists for. Policy gates run
# at CREATION time only, so tightening one never touches a link
# that already exists — the link keeps working and the compliance
# scan is the only thing that makes it visible. Both halves are
# asserted here: the finding appears, AND the link still resolves.
# ─────────────────────────────────────────────────────────────

# `inheritor_root` was captured at creation — there is no
# `GET /api/drives/{id}` to re-read it from.
#
# Relax the default so the link can be created at all.
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200


POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{inheritor_root}}",
  "item_type": "folder",
  "item_name": "dpd_legacy_link"
}

HTTP 201
[Captures]
legacy_share_id: jsonpath "$.id"


# Now tighten. The link above predates this and is untouched.
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_public_links": true }

HTTP 200


# A NEW link is refused — 405, the same shape every policy
# refusal uses.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{inheritor_root}}",
  "item_type": "folder",
  "item_name": "dpd_should_fail"
}

HTTP 405


# …but the pre-existing one still works. Discovery only: the scan
# reports, it never revokes.
GET {{base_url}}/api/shares/{{legacy_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


# Run the scan.
POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200


# Give the run a moment to finish; it is a handful of rows.
GET {{base_url}}/api/admin/jobs/drive_policies_consistency/runs?limit=1
Authorization: Bearer {{admin_token}}
[Options]
retry: 10
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$[0].status" == "Completed"
[Captures]
scan_run_id: jsonpath "$[0].id"


# The finding names the share. `body contains` rather than a
# filtered jsonpath count: a filter matching exactly one element
# collapses to a scalar and `count` then fails on the input type.
GET {{base_url}}/api/admin/jobs/drive_policies_consistency/runs/{{scan_run_id}}/findings?limit=500
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "share_violates_drive_policy"
body contains "forbid_public_links"
body contains "{{legacy_share_id}}"


# Cleanup this step's link before the teardown deletes the drive.
DELETE {{base_url}}/api/shares/{{legacy_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 13 — Every share-facing policy, one at a time, against a
#           compliant control.
#
# Step 12 proved ONE knob raises ONE finding. This walks the rest
# and — more importantly — pins what the scan must NOT say. A
# report that flags everything is as useless as one that flags
# nothing, so a deliberately compliant link runs alongside and is
# asserted absent from every result.
#
# Shares are created FIRST, under a permissive policy, then the
# policy tightens underneath them. That ordering is the whole
# point: gates run at creation time, so these links could never
# be made after the tightening, and the scan is the only thing
# that can see them.
# ─────────────────────────────────────────────────────────────

# Dedicated drive so the control link's compliance depends on
# this step's policy alone.
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind":  "shared",
  "name":  "dpd_scan",
  "owner": { "type": "user", "id": "{{admin_user_id}}" }
}

HTTP 201
[Captures]
scan_drive_id:   jsonpath "$.id"
scan_drive_root: jsonpath "$.root_folder_id"


# Nothing restricted yet — both links below must be creatable.
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200


# VIOLATOR: no password, no expiry. Laxest possible link.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{scan_drive_root}}",
  "item_type": "folder",
  "item_name": "dpd_scan_bad"
}

HTTP 201
[Captures]
bad_share_id: jsonpath "$.id"


# FAR: password set, but expiry in 2100 — compliant on the
# password knob and over the cap on the other. Exercises the
# `Some(expiry) > now + cap` comparison, which is different code
# from the null case above and is where date arithmetic can go
# wrong.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":    "{{scan_drive_root}}",
  "item_type":  "folder",
  "item_name":  "dpd_scan_far",
  "password":   "ScanPwd1!",
  "expires_at": 4102444800
}

HTTP 201
[Captures]
far_share_id: jsonpath "$.id"


# A user grant — NOT a public link. `storage.shares` holds only
# anonymous links, so without the grant pass this share is
# invisible to the scan no matter what the policy says.
POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{dpd_user_id}}" },
  "resource": { "type": "folder", "id": "{{scan_drive_root}}" },
  "role": "viewer"
}

HTTP 201


# CONTROL: password set, expiry 2027 — compliant with everything
# Step 13 is about to turn on. Its absence from the findings is
# what proves the scan discriminates.
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":    "{{scan_drive_root}}",
  "item_type":  "folder",
  "item_name":  "dpd_scan_good",
  "password":   "ScanPwd1!",
  "expires_at": 1798761600
}

HTTP 201
[Captures]
good_share_id: jsonpath "$.id"


# Tighten BOTH link knobs. Neither forbids links outright, so the
# control stays legitimate and only the violator should surface.
PATCH {{base_url}}/api/drives/{{scan_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "require_public_link_password": true, "max_public_link_days": 3650 }

HTTP 200


POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200


GET {{base_url}}/api/admin/jobs/drive_policies_consistency/runs?limit=1
Authorization: Bearer {{admin_token}}
[Options]
retry: 10
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$[0].status" == "Completed"
[Captures]
scan2_run_id: jsonpath "$[0].id"


GET {{base_url}}/api/admin/jobs/drive_policies_consistency/runs/{{scan2_run_id}}/findings?limit=500
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# Both knobs fire on the violator: it has no password, and no
# expiry at all — which is the LAXEST value, not an exemption.
body contains "share_missing_required_password"
body contains "share_outlives_policy_cap"
body contains "{{bad_share_id}}"
# The 2100 link is over the cap via the `Some(expiry)` comparison,
# not the null shortcut — a different branch, reached only by a
# link that HAS an expiry and still outlives the policy.
body contains "{{far_share_id}}"
# …and the compliant link appears nowhere. This is the assertion
# that would catch a scan flagging by policy-is-set rather than by
# policy-is-violated.
body not contains "{{good_share_id}}"


# Now forbid links outright. The control was compliant a moment
# ago and is not any more — nothing about it changed, only the
# policy above it.
PATCH {{base_url}}/api/drives/{{scan_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_public_links": true }

HTTP 200


POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200


GET {{base_url}}/api/admin/jobs/drive_policies_consistency/runs?limit=1
Authorization: Bearer {{admin_token}}
[Options]
retry: 10
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$[0].status" == "Completed"
[Captures]
scan3_run_id: jsonpath "$[0].id"


GET {{base_url}}/api/admin/jobs/drive_policies_consistency/runs/{{scan3_run_id}}/findings?limit=500
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "share_violates_drive_policy"
body contains "{{bad_share_id}}"
# The previously-clean link is now reported too.
body contains "{{good_share_id}}"


# ── forbid_sharing ───────────────────────────────────────────
#
# The broader rule. Two things have never been exercised until
# here: the link check attributing `forbid_sharing` rather than
# `forbid_public_links` (that arm is only reached when the
# narrower knob is OFF), and the grant pass at all.
PATCH {{base_url}}/api/drives/{{scan_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_public_links": false, "forbid_sharing": true }

HTTP 200


POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200


GET {{base_url}}/api/admin/jobs/drive_policies_consistency/runs?limit=1
Authorization: Bearer {{admin_token}}
[Options]
retry: 10
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$[0].status" == "Completed"
[Captures]
scan4_run_id: jsonpath "$[0].id"


GET {{base_url}}/api/admin/jobs/drive_policies_consistency/runs/{{scan4_run_id}}/findings?limit=500
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# The user grant — invisible to any scan that reads only
# `storage.shares`. This is the hole the grant pass closes.
body contains "grant_violates_drive_policy"
body contains "dpd_user"
# And the links are attributed to the knob that actually applies,
# now that the narrower one is off.
body contains "forbid_sharing"


# Both links still resolve. Discovery only — the scan has now
# named them twice and revoked neither.
GET {{base_url}}/api/shares/{{good_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


GET {{base_url}}/api/shares/{{bad_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


DELETE {{base_url}}/api/shares/{{good_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/shares/{{bad_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/shares/{{far_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/drives/{{scan_drive_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 14 — Teardown.
#
# Restore BOTH defaults to their seeded values first. A tightened
# default left behind would silently change every later file in
# the suite — the share tests especially, which create public
# links and would start getting 405s.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200
[Asserts]
jsonpath "$.forbid_public_links" == false
jsonpath "$.max_public_link_days" == null


PUT {{base_url}}/api/admin/drive-policies/defaults/personal
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "include_in_photo_index": true, "include_in_music_index": true }

HTTP 200
[Asserts]
jsonpath "$.include_in_photo_index" == true


DELETE {{base_url}}/api/drives/{{inheritor_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/drives/{{overrider_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/admin/users/{{dpd_user_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
