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
# Step 5 — Rename via profile PATCH. New name is mixed-case; server
# must store it as lowercase. Same rule as registration, applied on
# the mutation path.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me
Authorization: Bearer {{mixed_token}}
Content-Type: application/json
{ "username": "RenamedTarget" }

HTTP 200
[Asserts]
# The response echoes the stored (canonical) form.
jsonpath "$.full.user.username" == "renamedtarget"


# Old (pre-rename) username no longer resolves — login fails 401.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "MixedCaseUser", "password": "MixedCasePassword1!" }

HTTP 401


# New (post-rename) mixed-case login succeeds.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "RENAMEDTARGET", "password": "MixedCasePassword1!" }

HTTP 200
[Asserts]
jsonpath "$.user.full.user.username" == "renamedtarget"


# ─────────────────────────────────────────────────────────────
# Step 6 — NextCloud Basic Auth accepts every case variant.
# The middleware lowercases the decoded username on the auth path,
# and `extract_url_user` lowercases the URL segment. A cached
# client URL like `.../dav/files/RenamedTarget` continues to work
# indefinitely across the migration.
#
# Uses PROPFIND `Depth: 0` on `/remote.php/dav/files/{user}/` — a
# well-formed request that touches Basic Auth + URL parse + chroot
# resolve in one hop. 207 Multi-Status is the expected success shape.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/remote.php/dav/files/renamedtarget/
Depth: 0
[BasicAuth]
renamedtarget: MixedCasePassword1!

HTTP 207


PROPFIND {{base_url}}/remote.php/dav/files/RenamedTarget/
Depth: 0
[BasicAuth]
RenamedTarget: MixedCasePassword1!

HTTP 207


PROPFIND {{base_url}}/remote.php/dav/files/RENAMEDTARGET/
Depth: 0
[BasicAuth]
RENAMEDTARGET: MixedCasePassword1!

HTTP 207


# Mixed case in URL, lowercase in Basic Auth — still works because
# both surfaces normalise before comparison.
PROPFIND {{base_url}}/remote.php/dav/files/RenamedTarget/
Depth: 0
[BasicAuth]
renamedtarget: MixedCasePassword1!

HTTP 207
