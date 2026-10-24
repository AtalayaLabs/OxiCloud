# =============================================================
# OxiCloud — usernames are case-insensitive (silent lowercase on ingest)
# =============================================================
# Pin for `docs/plan/username-lowercase.md`. The plan makes usernames
# case-insensitive by canonicalising to lowercase in
# `User::validate_username`. This file covers the wire surface:
#
#   1. Registration with a mixed-case username lands as lowercase.
#   2. Login by the ORIGINAL mixed-case string succeeds (server
#      normalises on lookup).
#   3. Login by ALL-CAPS of the same name also succeeds.
#   4. Login by the canonical lowercase form succeeds.
#   5. Profile rename to a mixed-case name lands as lowercase.
#   6. NC Basic Auth accepts every case variant of the same account.
#
# Character-set + boundary rules (no leading dot, no `@`, etc.) are
# NOT re-tested here — that's the Rust unit-test surface. This file
# only pins the end-to-end normalization behaviour.
#
# Runs after `setup.hurl` (admin exists). Uses a self-contained user
# to avoid interfering with other scenarios.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Register with a mixed-case username. Expect the server
# to silently store the lowercase form.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "MixedCaseUser",
  "email":    "mixedcase@example.com",
  "password": "MixedCasePassword1!"
}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 2 — Log in with the ORIGINAL mixed-case string. Server
# should normalise on lookup and accept.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "MixedCaseUser", "password": "MixedCasePassword1!" }

HTTP 200
[Asserts]
jsonpath "$.access_token" isString
# The `/auth/me` response inside the login reply exposes the canonical
# stored username. Post-migration, it MUST be lowercase regardless of
# what the caller typed at registration.
jsonpath "$.user.full.user.username" == "mixedcaseuser"


# ─────────────────────────────────────────────────────────────
# Step 3 — Log in with ALL-CAPS. Same account, different casing.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "MIXEDCASEUSER", "password": "MixedCasePassword1!" }

HTTP 200
[Asserts]
jsonpath "$.user.full.user.username" == "mixedcaseuser"


# ─────────────────────────────────────────────────────────────
# Step 4 — Log in with the canonical lowercase form. Same account.
# Capture the token for the profile-rename step below.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mixedcaseuser", "password": "MixedCasePassword1!" }

HTTP 200
[Captures]
mixed_token: jsonpath "$.access_token"
[Asserts]
jsonpath "$.user.full.user.username" == "mixedcaseuser"


# ─────────────────────────────────────────────────────────────
# Step 5 — A rename attempt is REFUSED. Username is claim-once.
#
# This step asserted the opposite until 2026-10-24: it PATCHed
# `/api/auth/me` expecting 200 and a lowercased `renamedtarget`,
# which is wrong twice over. `/api/auth/me` is GET-only (PATCH has
# lived at `/me/profile` since 2026-06-03, so the old path returned
# 405), and `update_profile` rejects a username on a user that
# already has one with 409 / `reason = "username_immutable"`. The
# file was never wired into run.sh, so neither error ever surfaced.
#
# `docs/plan/username-lowercase.md` is the source of the confusion —
# it lists `update_profile()` under "User-driven rename" and
# specifies this very test as "rename to `NewName`, assert `newname`
# stored". The implementation deliberately went the other way, and
# the reason is load-bearing: the DAV and NextCloud surfaces bake
# the username into path segments (`/remote.php/dav/files/{user}/`),
# so a rename silently breaks every cached client URL. Immutability
# wins; the plan's rename clause is obsolete.
#
# Pinning the refusal is worth more than deleting the step: it keeps
# claim-once from being relaxed by accident, which would be a
# NextCloud-compatibility regression rather than a mere API change.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{mixed_token}}
Content-Type: application/json
{ "username": "RenamedTarget" }

HTTP 409
[Asserts]
# There is NO fine-grained machine flag to assert here, and that is the
# finding rather than an oversight in this test.
#
# `error_type` is only `ErrorKind::as_str()` — "Already Exists" — which
# `update_profile` returns for BOTH 409s: username-immutable AND
# username-taken-by-another-user. Asserting it would pass whichever
# rejection fired, so it pins nothing. REST `AppError` has no
# `error_code` field at all (the `error_code` constants in
# `message_bus_ports` are the RT/RPC surface only).
#
# So this pins the message substring instead — not because matching
# English is good, but because `frontend/src/lib/api/endpoints/
# profile.ts:43` ALREADY depends on it:
#
#     const key = msg.includes('already claimed') ? … : …
#
# to split the two 409s into `profile.username_immutable_error` vs
# `profile.username_taken_error`. Rewording the server message
# therefore makes the SPA show the wrong error with no other signal.
# This assertion is the only thing that would catch that.
#
# Replace both with `jsonpath "$.error_code" == "username_immutable"`
# once REST errors carry a code — see the note in
# `docs/plan/username-lowercase.md`.
jsonpath "$.error_type" == "Already Exists"
jsonpath "$.message" contains "already claimed"


# The original username still resolves, case-insensitively — nothing
# was renamed. This is the assertion the old version had inverted.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "MixedCaseUser", "password": "MixedCasePassword1!" }

HTTP 200
[Asserts]
jsonpath "$.user.full.user.username" == "mixedcaseuser"


# ─────────────────────────────────────────────────────────────
# Step 6 — NextCloud Basic Auth accepts every case variant.
# The middleware lowercases the decoded username on the auth path
# (`basic_auth_middleware.rs:108`), and `extract_url_user` lowercases
# the URL segment. A cached client URL like
# `.../dav/files/MixedCaseUser` keeps working across the migration.
#
# Uses PROPFIND `Depth: 0` on `/remote.php/dav/files/{user}/` — a
# well-formed request that touches Basic Auth + URL parse + chroot
# resolve in one hop. 207 Multi-Status is the expected success shape.
#
# The credential MUST be an app password. The original version of this
# step passed the ACCOUNT password and got 401 on every variant: the NC
# surface verifies via `app_passwords.verify_basic_auth()` and never
# accepts the login password. Minting one here matches every other NC
# scenario in the suite (see `nc_avatar_preview.hurl`).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{mixed_token}}
Content-Type: application/json
{ "label": "lowercase_usernames hurl test" }

HTTP 200
[Captures]
nc_password: jsonpath "$.password"
[Asserts]
# The mint echoes the canonical stored username — lowercase, even though
# the account was registered as `MixedCaseUser`. On-topic for this file:
# it is the same canonicalisation seen from a third surface.
jsonpath "$.username" == "mixedcaseuser"


# Canonical lowercase in both URL and credential — the baseline.
PROPFIND {{base_url}}/remote.php/dav/files/mixedcaseuser/
Depth: 0
[BasicAuth]
mixedcaseuser: {{nc_password}}

HTTP 207


PROPFIND {{base_url}}/remote.php/dav/files/MixedCaseUser/
Depth: 0
[BasicAuth]
MixedCaseUser: {{nc_password}}

HTTP 207


PROPFIND {{base_url}}/remote.php/dav/files/MIXEDCASEUSER/
Depth: 0
[BasicAuth]
MIXEDCASEUSER: {{nc_password}}

HTTP 207


# Mixed case in URL, lowercase in Basic Auth — still works because
# both surfaces normalise before comparison.
PROPFIND {{base_url}}/remote.php/dav/files/MixedCaseUser/
Depth: 0
[BasicAuth]
mixedcaseuser: {{nc_password}}

HTTP 207
