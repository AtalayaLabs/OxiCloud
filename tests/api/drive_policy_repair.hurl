# ═════════════════════════════════════════════════════════════
# Drive policy repair — `drive_policies_consistency?repair=true&drive=…`
#
# Pins the one property the whole feature turns on: repair deletes the
# non-compliant shares of the drive it was GIVEN, and leaves every other
# drive alone. A repair that reached past its drive would revoke access
# nobody asked it to touch, and there is no undo.
#
# Two drives, identical policy breach on each, repair aimed at one:
#
#   drive A (target)  — public link, forbidden by policy → must be GONE
#   drive B (control) — public link, forbidden by policy → must SURVIVE
#
# The control is what makes this a test of scoping rather than a test of
# deletion. Without it, "the link is gone" passes just as well for a job
# that deleted every link on the instance.
#
# Own file rather than more steps on `drive_policy_defaults.hurl`: this
# one DELETES shares, and a destructive step sharing a file with tests
# that assert on their own shares surviving is a trap for whoever adds
# the next step.
#
# State touched, all restored at the end: two shared drives, and the
# SHARED drive-kind default — which is instance-wide, so leaving it
# tightened would make every later file in the suite start getting 405s
# on link creation.
# ═════════════════════════════════════════════════════════════

# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"
admin_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Two shared drives, links created while links are still
#          ALLOWED. Enforcement is creation-time, so the breach has
#          to be manufactured in this order: create the links first,
#          then tighten the policy under them. That is exactly the
#          situation the scan exists for.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind":  "shared",
  "name":  "dpr_target",
  "owner": { "type": "user", "id": "{{admin_user_id}}" }
}

HTTP 201
[Captures]
target_id: jsonpath "$.id"
target_root: jsonpath "$.root_folder_id"


POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind":  "shared",
  "name":  "dpr_control",
  "owner": { "type": "user", "id": "{{admin_user_id}}" }
}

HTTP 201
[Captures]
control_id: jsonpath "$.id"
control_root: jsonpath "$.root_folder_id"


POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{target_root}}",
  "item_type": "folder",
  "item_name": "dpr_target_link"
}

HTTP 201
[Captures]
target_share_id: jsonpath "$.id"


POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{control_root}}",
  "item_type": "folder",
  "item_name": "dpr_control_link"
}

HTTP 201
[Captures]
control_share_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Tighten the shared default so BOTH links are now in
#          breach. Set on the default rather than per drive, so the
#          two drives are in genuinely identical positions and the
#          only difference left between them is which one repair is
#          pointed at.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_public_links": true }

HTTP 200
[Asserts]
jsonpath "$.forbid_public_links" == true


# Both links still work — tightening a policy never revokes what
# already exists. This is the premise of the whole scan, so it is
# asserted rather than assumed.
GET {{base_url}}/api/shares/{{target_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


GET {{base_url}}/api/shares/{{control_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 4 — A plain scan REPORTS and changes nothing.
#
# Run before the repair so the "still there" assertions below mean
# something: they prove the scan did not delete, which is what makes
# the later deletion attributable to `repair=true` alone.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
# Zero removals on a plain run, stated explicitly: the counters are
# always present precisely so "removed nothing" is distinguishable
# from "this run could not remove anything".
jsonpath "$.outcome.extra.extra_stats.links_removed" == 0
jsonpath "$.outcome.extra.extra_stats.grants_removed" == 0


GET {{base_url}}/api/shares/{{target_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 5 — `repair=true` WITHOUT `drive` is REFUSED.
#
# The single most important negative case here, and the reason the
# parameter is required rather than defaulted: the admin panel puts a
# repair checkbox beside a Run button, so a default of "every drive"
# would turn one misclick into an estate-wide revocation with no undo.
# Omitting `drive` must therefore fail, not fall back — and the links
# are checked afterwards to prove the run really did nothing before it
# gave up.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger?repair=true
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.outcome" == "err"
jsonpath "$.outcome.message" contains "requires `drive`"
# The refusal has to teach the deliberate form, or an admin who wanted
# the whole estate is left guessing.
jsonpath "$.outcome.message" contains "*"


GET {{base_url}}/api/shares/{{target_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


GET {{base_url}}/api/shares/{{control_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 6 — An unknown drive is refused, and by name-ambiguity too.
#
# `dpr_target` is a unique name, so it RESOLVES — names are accepted
# for the admin's convenience. A name shared by several drives does
# not, and the message names the candidates instead of picking one;
# that path is not reachable here without creating a duplicate, so
# only the unique-name and unknown-name arms are pinned.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger?repair=true&drive=dpr_no_such_drive
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.outcome" == "err"
jsonpath "$.outcome.message" contains "no drive is named"


# ─────────────────────────────────────────────────────────────
# Step 7 — Repair the TARGET drive, by id.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger?repair=true&drive={{target_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
# Exactly the one link on the target drive. A greater number would
# mean the scope leaked into the control drive.
jsonpath "$.outcome.extra.extra_stats.links_removed" == 1


# ─────────────────────────────────────────────────────────────
# Step 8 — The property under test.
#
# The target's link is gone; the control's is untouched. Both halves
# matter: the first proves repair acted, the second proves it stayed
# inside the drive it was given.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/shares/{{target_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 404


GET {{base_url}}/api/shares/{{control_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 9 — Re-running repair on the now-clean drive is a no-op.
#
# Idempotence, and the reason it is worth a step: an admin who clicks
# twice, or a scheduled run that overlaps a manual one, must not see
# a failure or a second phantom removal.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger?repair=true&drive={{target_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.extra_stats.links_removed" == 0


# ─────────────────────────────────────────────────────────────
# Step 10 — Repair by NAME, on the control drive.
#
# Same operation reached the other way, which is what makes the name
# arm of the resolver load-bearing rather than decorative. This also
# clears the control link, so teardown can delete both drives.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger?repair=true&drive=dpr_control
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.extra_stats.links_removed" == 1


GET {{base_url}}/api/shares/{{control_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 11 — `drive=*` is accepted: the explicit every-drive form.
#
# Deliberately run AFTER the default has been relaxed, so nothing on
# the instance is in breach and the wildcard has nothing to delete.
# That is the whole point of the ordering: this step must prove `*`
# parses and runs estate-wide, and it must not be capable of deleting
# a share belonging to some other test's fixtures — which a wildcard
# repair under a tightened default absolutely could.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/drive-policies/defaults/shared
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{}

HTTP 200
[Asserts]
jsonpath "$.forbid_public_links" == false


POST {{base_url}}/api/admin/jobs/drive_policies_consistency/trigger?repair=true&drive=*
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
# Nothing is in breach, so nothing is removed. Accepting the wildcard
# is what is under test, not deleting with it.
jsonpath "$.outcome.extra.extra_stats.links_removed" == 0
jsonpath "$.outcome.extra.extra_stats.grants_removed" == 0


# ─────────────────────────────────────────────────────────────
# Step 12 — Teardown.
#
# The shared default was already restored in step 11 (the wildcard step
# needed a clean instance to be safe), so only the drives remain. It is
# instance-wide state: a `true` left behind would make every later file
# that creates a public link start failing with 405.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/drives/{{target_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/drives/{{control_id}}
Authorization: Bearer {{admin_token}}

HTTP 204
