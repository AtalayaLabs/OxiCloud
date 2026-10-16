# =============================================================
# OxiCloud — App-password endpoints: shape + lifecycle contract
# =============================================================
# The three endpoints under /api/auth/app-passwords are exercised
# transitively by every NC-WebDAV Hurl flow — those use create+
# revoke as setup/teardown for the Basic-Auth operations they
# actually test. What they do NOT pin:
#
#   • The response SHAPE of `GET /api/auth/app-passwords`
#     (the list endpoint). NC flows never list — they POST, use,
#     then DELETE by captured id. A Phase-4 migration that
#     renamed/dropped a field in `AppPasswordSummaryDto` would
#     have no Hurl pin surfacing it.
#   • The full response SHAPE of `POST /api/auth/app-passwords`.
#     NC flows capture just `.password` + `.id`; a wire shift on
#     `instructions`, `scopes`, `expires_at`, `username` would
#     slip through.
#   • DELETE 404 (idempotency) and DELETE 400 (bad UUID).
#
# NOT COVERED HERE (intentional, scoped for follow-up):
#   • 403 for an external user creating an app-password. Needs
#     the invite-by-email + SMTP-mock + magic-link-redeem setup
#     (~60 lines, see external_users.hurl for the pattern). Not
#     a Phase-4 migration risk because `profile.ts::createApp
#     Password` is caller-local; the gate is validated by the
#     backend audit tests. Add in its own PR once the external-
#     user setup is reusable fixture-style.
#   • 409 "no username" — requires an internal user with
#     `username IS NULL`, a legitimately rare state that the
#     seed env doesn't easily produce.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login. All subsequent endpoints run as this
#          user, so every row created here belongs to the admin
#          and the teardown at the end is self-contained.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"
admin_username: jsonpath "$.user.full.user.username"


# ─────────────────────────────────────────────────────────────
# Step 2 — Create app-password #1. Pin the FULL response shape:
#          every field the FE wrapper and the generated-SDK type
#          both expect, including the nested `instructions` bag.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "app-passwords hurl #1" }

HTTP 200
[Captures]
ap1_id: jsonpath "$.id"
[Asserts]
# Identity / lifecycle fields.
jsonpath "$.id" exists
jsonpath "$.id" matches "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
jsonpath "$.label" == "app-passwords hurl #1"
# The plain-text secret — returned exactly ONCE. Format pinned by
# the handler's own docstring: `oxicloud-` prefix + 32 chars.
jsonpath "$.password" exists
jsonpath "$.password" matches "^oxicloud-[A-Za-z0-9]{20,}$"
# `username` is what NC Basic Auth expects for this account.
jsonpath "$.username" == "{{admin_username}}"
# `scopes` falls back to the handler's default when the request
# omits the field — pins the three-scope default so a backend
# change to it surfaces here, not downstream.
jsonpath "$.scopes" == "webdav,caldav,carddav"
# Request omitted `expires_in_days` → server stores NULL → wire
# emits the field as JSON null (no `skip_serializing_if` on the
# DTO). `null` is a legitimate "never expires" marker, so pin the
# value explicitly rather than key presence.
jsonpath "$.expires_at" == null
# The `instructions` bag — four hint strings the FE renders. If
# the backend ever drops one, every client reading `.instructions.X`
# gets `undefined` without a type error.
jsonpath "$.instructions.davx5" exists
jsonpath "$.instructions.thunderbird" exists
jsonpath "$.instructions.rclone" exists
jsonpath "$.instructions.curl_example" exists


# ─────────────────────────────────────────────────────────────
# Step 3 — Create app-password #2 WITH custom scopes + a 7-day
#          expiry, so the list endpoint has two differently-
#          shaped rows and the scopes-override path is pinned.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "app-passwords hurl #2", "scopes": "webdav", "expires_in_days": 7 }

HTTP 200
[Captures]
ap2_id: jsonpath "$.id"
[Asserts]
jsonpath "$.scopes" == "webdav"
# Explicit `expires_in_days` → non-null wire field, ISO-8601 string.
jsonpath "$.expires_at" exists
jsonpath "$.expires_at" matches "^[0-9]{4}-[0-9]{2}-[0-9]{2}T"


# ─────────────────────────────────────────────────────────────
# Step 4 — GET list. The whole point of this file: pin the shape
#          of `AppPasswordListResponseDto` + the per-row
#          `AppPasswordSummaryDto` that NC flows never read.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.app_passwords" exists
jsonpath "$.total" exists
# Both rows we just minted are in the list — anchor on label
# because order is implementation-defined and both labels are
# unique within this file.
jsonpath "$.app_passwords[*].label" contains "app-passwords hurl #1"
jsonpath "$.app_passwords[*].label" contains "app-passwords hurl #2"
# Per-row shape: pin every field a Phase-4 FE swap would read.
# `prefix` is the identification signal (first 8 chars of the
# token) — never the full token — so a wire change here can
# silently break the admin "which row is this?" UI.
jsonpath "$.app_passwords[0].id" exists
jsonpath "$.app_passwords[0].label" exists
jsonpath "$.app_passwords[0].prefix" exists
# Shape per `AppPasswordService::list` + the storage-time prefix:
#   stored prefix = `oxicloud-` (9) + first 8 chars of the random body = 17 chars
#   list response then appends `"..."` → exactly 20 chars on the wire.
# Pinned here so a shift in TOKEN_PREFIX, prefix length, or the trailing
# ellipsis surfaces before the admin UI silently renders a wrong label.
jsonpath "$.app_passwords[0].prefix" matches "^oxicloud-[A-Za-z0-9]{8}\\.\\.\\.$"
jsonpath "$.app_passwords[0].scopes" exists
jsonpath "$.app_passwords[0].created_at" exists
jsonpath "$.app_passwords[0].active" == true
# `last_used_at` is nullable (fresh rows have never been used);
# only pin that the key is addressable as a path, not a value.
# `expires_at` is nullable too (ap1 has none; ap2 has one).


# ─────────────────────────────────────────────────────────────
# Step 5 — Revoke #1 → 200 with status/id echo.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/app-passwords/{{ap1_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.status" == "revoked"
jsonpath "$.id" == "{{ap1_id}}"


# ─────────────────────────────────────────────────────────────
# Step 6 — Revoke the same id a second time → still 200.
#
# `AppPasswordPgRepository::revoke` is a SOFT delete
# (`UPDATE ... SET active = FALSE`), and Postgres' rows_affected
# counts WHERE-matched rows (not value-changed rows). So the
# second revoke still matches the (now-inactive) row and returns
# 200 — the delete is UPSERT-idempotent by construction.
#
# Pinned here because a Phase-4 FE might retry on a network blip,
# and a future migration to HARD delete would silently flip this
# to 404, breaking that retry logic.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/app-passwords/{{ap1_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.status" == "revoked"
jsonpath "$.id" == "{{ap1_id}}"


# ─────────────────────────────────────────────────────────────
# Step 7 — Revoke a well-formed but non-existent UUID → 404.
#
# The actual not-found path: service's `get_by_id` fails because
# no row matches `id = $1`, and the error bubbles to a 404. Uses
# the all-zeros UUID — guaranteed never to collide with a real
# row and reads unambiguously as "placeholder, not a real id".
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/app-passwords/00000000-0000-0000-0000-000000000000
Authorization: Bearer {{admin_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 8 — Revoke with a non-UUID id → 400 (bad request).
#
# The handler's own `Uuid::parse_str` rejection path. Pre-
# Phase-3 this would have crashed through to a 500; the typed
# UUID parse keeps it a clean 400.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/app-passwords/not-a-uuid
Authorization: Bearer {{admin_token}}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Teardown — revoke ap2 so the suite's shared DB leaves no
# dangling rows. Per `feedback_hurl_teardown_shared_db` memory:
# Hurl files share state across the whole run, every created
# row has to be revoked or deleted explicitly.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/app-passwords/{{ap2_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
