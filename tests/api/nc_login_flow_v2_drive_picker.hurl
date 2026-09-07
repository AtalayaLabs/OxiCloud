# =============================================================
# OxiCloud — NC Login Flow v2 — password path, multi-drive picker
# =============================================================
# Sibling to `nc_login_flow_v2.hurl` (protocol init + poll edge
# cases). Where that file exercises the wire shape, THIS file
# exercises the multi-drive fork — the branch in
# `handle_login_submit` (login_v2_handler.rs) that renders the
# drive picker template when `list_folders_with_perms` returns
# ≥ 2 rows, then defers completion until the user picks.
#
# The OIDC equivalent lives at `tests/oidc/oidc.hurl` Step 12 and
# regression-pins the customer-reported bug where OIDC callback
# skipped the picker. This file pins the SAME multi-drive fork
# for the classic password path so a refactor of the shared
# `resolve_drive_or_complete` helper can't silently regress
# either channel.
#
# Coverage (end-to-end simulation of the NC desktop client's
# browser leg + backchannel):
#
#   A. Admin password login → JWT for creating fixtures.
#   B. Create a shared drive owned by admin so admin has
#      exactly 2 drives (default personal + this shared).
#   C. NC LFv2 initiate → capture flow_token + poll_token.
#   D. Pre-completion poll → 404 baseline.
#   E. Submit login form POST /login/v2/flow/{token} with
#      user + password → picker HTML (200), NOT a redirect,
#      because the user has ≥ 2 drives.
#   F. Poll AGAIN → still 404. Proves the submit did NOT
#      complete the flow — regression against a future change
#      that accidentally shortcuts past the picker.
#   G. Submit picker → POST /login/v2/flow/{token}/drive.
#   H. Post-picker poll → 200 with composite `admin~<uuid>`
#      loginName. This is the load-bearing assertion: the
#      picker choice must round-trip into the app-password's
#      login name so NC uploads land on the chosen drive.
#   I. Poll again → 404 (single-use consumed).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step A — Admin password login for fixture creation.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token:   jsonpath "$.access_token"
admin_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step B — Create a shared drive owned by admin. The admin's
#          default personal drive is already there; this second
#          drive triggers the multi-drive picker branch on the
#          next login (`list_folders_with_perms` returns 2 rows).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "admin-picker-fixture",
  "owner": { "type": "user", "id": "{{admin_user_id}}" }
}

HTTP 201
[Captures]
fixture_drive_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step C — NC LFv2 initiate. Public endpoint at
#          `/index.php/login/v2` (the bare `/login/v2` alias
#          only exists for the poll surface, not initiate —
#          nc_routes.rs:50 vs :79).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/index.php/login/v2

HTTP 200
[Captures]
poll_token: jsonpath "$.poll.token"
# Regex-extract flow_token from the login URL.
# Shape: http://<host>/login/v2/flow/<hex>
flow_token: jsonpath "$.login" regex "/login/v2/flow/([a-f0-9]+)"


# ─────────────────────────────────────────────────────────────
# Step D — Baseline poll. No submission yet, so the flow has
#          no `completed` result. MUST 404.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/poll
[FormParams]
token: {{poll_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step E — Submit the login form. `handle_login_submit`
#          verifies credentials, calls list_folders_with_perms,
#          sees ≥ 2 drives, and returns the picker template
#          (HTTP 200 with HTML body). Pre-picker era this
#          would have been a redirect straight to
#          `/nextcloud/success`; that regression is what this
#          assertion pins.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/flow/{{flow_token}}
[FormParams]
user: {{username}}
password: {{password}}

HTTP 200
[Captures]
# Two drives in the picker — home is `loop.first` (index [1] in
# XPath 1-based), shared is [2]. We submit the shared value in
# Step G so the composite marker actually differs from the
# bare login name, exercising the ~<uuid> path (Step H asserts
# on it). Local-name XPath so DAV/HTML namespace scoping doesn't
# interfere.
shared_folder_id: xpath "string((//input[@name='drive']/@value)[2])"
[Asserts]
# Picker markers — distinguish the picker template from any
# other 200 response.
body contains "Choose a drive"
body contains "name=\"drive\""
# Load-bearing regression: the picker's <form action> must
# target the drive endpoint. A wrong action would ship the user
# into an unrelated flow and only surface at the next request.
body contains "action=\"/login/v2/flow/{{flow_token}}/drive\""
# No `nc://` frontchannel URL should ever appear on the
# response — NC clients pick up credentials via the poll
# endpoint, not via a URL redirect. This mirrors the OIDC path
# fix from tests/oidc/oidc.hurl Step 12F.
body not contains "nc://login"


# ─────────────────────────────────────────────────────────────
# Step F — Poll AGAIN. Still 404 — the picker has been
#          rendered but not submitted, so no `complete_flow`
#          call has run. Guards against a future refactor that
#          accidentally auto-completes the flow at the submit
#          step (e.g. re-introducing the pre-picker shortcut
#          the OIDC arm used to have).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/poll
[FormParams]
token: {{poll_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step G — Submit the picker choice. handle_drive_pick reads
#          `pending_user_id` from the flow (stashed by
#          resolve_drive_or_complete when we rendered the
#          picker), validates the folder is visible, and
#          calls complete_flow(..., Some(folder_id)).
#
#          Response redirects to /nextcloud/success — that's
#          where NC clients that don't use the poll backchannel
#          would land visually. NC clients that DO use the poll
#          (standard) have credentials in-hand by the time this
#          redirect fires, courtesy of `login_flow.complete()`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/flow/{{flow_token}}/drive
[FormParams]
drive: {{shared_folder_id}}

# 3xx redirect to /nextcloud/success. Not `nc://` — that's the
# whole point of the earlier "friendly success page" fix.
HTTP *
[Asserts]
status >= 300
status <  400
header "Location" == "/nextcloud/success"


# ─────────────────────────────────────────────────────────────
# Step H — Post-picker poll. NOW the credentials appear.
#
#          The composite `admin~<folder_id>` login name proves
#          the picker choice round-tripped into the app
#          password's login name (basic_auth_middleware.rs
#          treats the `~<uuid>` suffix as a chroot marker for
#          subsequent WebDAV / NC requests). Without the
#          composite, the sync client would target the home
#          drive regardless of what the user picked.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/poll
[FormParams]
token: {{poll_token}}

HTTP 200
[Asserts]
# Loose host match — the server derives base_url from its bind
# config (which lands on `127.0.0.1` when neither
# OXICLOUD_BASE_URL nor the host env is set), while test.env
# uses `localhost` for its own variable. Both resolve to the
# same address for a client; pin the port, not the host.
jsonpath "$.server"      matches "^https?://[^/]+:8087$"
jsonpath "$.appPassword" isString
# Load-bearing composite-marker assertion. Pre-fix (or if a
# refactor ever drops the picker branch) this would show the
# bare `admin` with no `~`.
jsonpath "$.loginName" matches "^{{username}}~[0-9a-f-]{36}$"
# Belt-and-braces: the exact folder id we picked in Step G is
# what got wired into the login name. Catches a hypothetical
# drive/folder id swap in `handle_drive_pick`.
jsonpath "$.loginName" contains "{{shared_folder_id}}"


# ─────────────────────────────────────────────────────────────
# Step I — Poll again — MUST 404. The completed result is
#          single-use (poll() removes it from the state map
#          on read); a regression that failed to remove would
#          leak credentials to any subsequent poll with the
#          same token, effectively a replay window.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/poll
[FormParams]
token: {{poll_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Teardown — remove the fixture shared drive.
#
# CRITICAL: individual Hurl files inside `tests/api/run.sh`
# share DB state within a single run (postgres restarts once
# per run.sh, not per file). Leaving this drive around inflates
# admin's `list_folders_with_perms` result from 1 to 2, which
# breaks any downstream file that assumes admin has exactly one
# root folder (files-folders.hurl:43, favorites.hurl:59,
# recent.hurl:47, and any future test using `/api/folders`).
# Every hurl that creates a persistent drive/folder MUST clean
# it up here, not rely on the next run.sh invocation to reset.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/drives/{{fixture_drive_id}}
Authorization: Bearer {{admin_token}}

HTTP 204
