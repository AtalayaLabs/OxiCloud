# =============================================================
# OxiCloud – Music (playlist) + Round-3 AuthZ end-to-end scenario
# =============================================================
# Verifies the full playlist REST surface post-Round-3:
#
#   * `POST /api/playlists` seeds an Owner grant on
#     `Resource::Playlist(uuid)` so the caller can see it via the
#     unified engine (list, get) on the very next request.
#   * `GET /api/playlists` returns the union of owned + shared
#     playlists via `authz.list_incoming_grants`; the pre-Round-3
#     owner-only + separate shared query pair is gone.
#   * Cross-user reads (`GET /api/playlists/{id}`) return the 404
#     anti-enum shape (was 403 in the bespoke
#     `user_has_access` era).
#   * Sharing works through BOTH surfaces post-migration:
#       - Generic `POST /api/grants` with `resource.type = "playlist"`
#         (first-class ReBAC variant added in this PR)
#       - Legacy `POST /api/playlists/{id}/share` (bool `can_write`)
#         still routes through the same `role_grants` table via
#         `authz.set_role`, so both flows converge on the unified
#         engine.
#   * `GET /api/playlists/{id}/shares` reads `list_grants_on_resource`
#     and hides the Owner self-grant.
#   * Revoke through either surface drops the playlist from the
#     recipient's listing.
#   * Viewer role blocks writes: `Update`/`Delete`/`Share` all 404 for
#     a Viewer, matching the anti-enum shape.
#
# The `playlist_id` is captured from the POST response body. Fresh CI
# database via `tests/api/run.sh`, so admin has no prior playlists —
# the JSONPath capture from `GET /api/playlists` is unambiguous.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Alice (admin) logs in.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"
alice_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 2 – Alice creates a playlist. The response body carries the
# server-assigned UUID and `owner_id == alice_user_id`. The service
# also seeds an Owner role_grant on `Resource::Playlist(uuid)` —
# proven by Step 4 which lists playlists via
# `authz.list_incoming_grants` and expects this one to surface.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/playlists
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "name": "round3-playlist",
  "description": "Music AuthZ migration coverage"
}

HTTP 201
[Captures]
playlist_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "round3-playlist"
jsonpath "$.owner_id" == "{{alice_user_id}}"


# ─────────────────────────────────────────────────────────────
# Step 3 – Alice GETs the playlist she just created. This is the
# fast-path validation of the Owner grant seeded at create time:
# without it, `authz.require(Read)` would return NotFound and this
# would 404.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists/{{playlist_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{playlist_id}}"


# ─────────────────────────────────────────────────────────────
# Step 4 – Alice lists playlists — hers appears exactly once.
# The service reads `list_incoming_grants(Alice)` and filters to
# `Resource::Playlist`, so this exercises the same code path as
# CalDAV's `list_my_calendars`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" contains "{{playlist_id}}"


# ─────────────────────────────────────────────────────────────
# Step 5 – Provision Bob. Idempotent: `HTTP *` accepts 201 first
# run, 409 subsequent runs. Login is the real precondition.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "username": "music_bob",
  "password": "MusicBobPassword1!",
  "email":    "music_bob@example.com",
  "role":     "user"
}

HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "music_bob",
  "password": "MusicBobPassword1!"
}

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"
bob_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 6 – Cross-user GET on Alice's playlist → 404. Before Round 3
# this was the bespoke `user_has_access` denial which returned 403;
# post-migration `authz.require(Read)` denies with `NotFound` for
# anti-enumeration parity with files/folders/drives.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists/{{playlist_id}}
Authorization: Bearer {{bob_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 7 – Bob's playlist listing does NOT include Alice's. The
# `list_incoming_grants(Bob)` call sees no grant on that playlist,
# so nothing surfaces.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$..id" not contains "{{playlist_id}}"


# ─────────────────────────────────────────────────────────────
# Step 8 – Alice shares the playlist with Bob as Viewer via the
# generic ReBAC grant endpoint. `resource.type = "playlist"` is a
# first-class variant added by this PR; before Round 3, this
# request would 400 (Unsupported resource type).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "playlist", "id": "{{playlist_id}}" },
  "role":     "viewer"
}

HTTP 201
[Captures]
share_grant_id: jsonpath "$.grants[0].id"
[Asserts]
jsonpath "$.grants[0].role" == "viewer"
jsonpath "$.grants[0].resource.type" == "playlist"
jsonpath "$.grants[0].resource.id" == "{{playlist_id}}"


# ─────────────────────────────────────────────────────────────
# Step 9 – Bob GET now succeeds. `authz.require(Read)` sees the
# Viewer role_grant row and grants access.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists/{{playlist_id}}
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{playlist_id}}"


# ─────────────────────────────────────────────────────────────
# Step 10 – Bob's listing now surfaces Alice's playlist — proving
# the owned + shared union in `list_playlists`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists?include_shared=true
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" contains "{{playlist_id}}"


# ─────────────────────────────────────────────────────────────
# Step 11 – Bob cannot rename the playlist. Viewer's bundle is
# Read-only (no Update). Bob has Read → graduated denial returns
# 403 (see [[project_authz_require_graduated_denial]]).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/playlists/{{playlist_id}}
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "name": "hijacked" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 12 – Bob cannot delete the playlist. Viewer's bundle
# excludes Delete → 403 (Read granted).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/playlists/{{playlist_id}}
Authorization: Bearer {{bob_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 13 – Bob cannot re-share the playlist. Viewer's bundle
# excludes Share → 403 (Read granted). The legacy /share endpoint
# routes through `authz.require(Share)`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/playlists/{{playlist_id}}/share
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "user_id": "{{alice_user_id}}", "can_write": true }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 14 – Alice lists shares via the legacy endpoint. The
# service reads `list_grants_on_resource` and drops the Owner
# self-grant, so exactly one row surfaces: Bob as Viewer
# (can_write=false).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists/{{playlist_id}}/shares
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[*].user_id" contains "{{bob_user_id}}"
jsonpath "$[?(@.user_id == '{{bob_user_id}}')].can_write" == false
jsonpath "$[*].user_id" not contains "{{alice_user_id}}"


# ─────────────────────────────────────────────────────────────
# Step 14b – Same query, unified endpoint. `GET /api/grants?
# resource_type=playlist&resource_id=…` requires `Share` on the
# resource (same gate as the legacy /shares endpoint) and returns
# the raw `role_grants` rows — including the Owner self-grant that
# the legacy DTO hides. Confirms `ResourceTypeDto::Playlist` is
# admitted at the wire boundary and that both surfaces read the
# same underlying data.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants?resource_type=playlist&resource_id={{playlist_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[*].subject.id" contains "{{bob_user_id}}"
jsonpath "$[*].subject.id" contains "{{alice_user_id}}"
jsonpath "$[?(@.subject.id == '{{bob_user_id}}')].role" == "viewer"
jsonpath "$[?(@.subject.id == '{{alice_user_id}}')].role" == "owner"
jsonpath "$[?(@.subject.id == '{{bob_user_id}}')].resource.type" == "playlist"


# ─────────────────────────────────────────────────────────────
# Step 14c – Bob (Viewer only) is denied on the unified list
# endpoint: `Share` is required, Viewer's bundle excludes it.
# Bob has Read → graduated denial returns 403 (see
# [[project_authz_require_graduated_denial]]).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants?resource_type=playlist&resource_id={{playlist_id}}
Authorization: Bearer {{bob_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 15 – Alice revokes the ReBAC grant. `DELETE /api/grants/{id}`
# deletes the single `role_grants` row keyed by grant_id.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/grants/{{share_grant_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 16 – Bob's GET goes back to 404, and his listing drops the
# playlist. The `role_grants` row is gone → `list_incoming_grants`
# doesn't surface it, `require(Read)` denies.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists/{{playlist_id}}
Authorization: Bearer {{bob_token}}

HTTP 404


GET {{base_url}}/api/playlists?include_shared=true
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$..id" not contains "{{playlist_id}}"


# ─────────────────────────────────────────────────────────────
# Step 17 – Alice re-shares Bob as Editor via the LEGACY endpoint.
# `can_write=true` maps to `Role::Editor` inside
# `music_service::share_playlist` — proving the legacy surface
# and `/api/grants` now converge on the same `role_grants` table.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/playlists/{{playlist_id}}/share
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "user_id": "{{bob_user_id}}", "can_write": true }

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 18 – Editor CAN update (Editor's bundle includes Update).
# Confirms the can_write=true → Editor mapping actually takes
# effect at the engine level.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/playlists/{{playlist_id}}
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "description": "renamed by editor bob" }

HTTP 200
[Asserts]
jsonpath "$.description" == "renamed by editor bob"


# ─────────────────────────────────────────────────────────────
# Step 19 – Editor still cannot Share (Share stays Owner-only).
# Bob has Read (Editor bundle) → graduated denial returns 403.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/playlists/{{playlist_id}}/share
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "user_id": "{{alice_user_id}}", "can_write": false }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 20 – `/shares` now reports Bob as Editor (can_write=true).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists/{{playlist_id}}/shares
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[*].user_id" contains "{{bob_user_id}}"
jsonpath "$[?(@.user_id == '{{bob_user_id}}')].can_write" == true


# ─────────────────────────────────────────────────────────────
# Step 21 – Alice removes the legacy-endpoint share.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/playlists/{{playlist_id}}/share/{{bob_user_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 22 – Post-remove listing is empty (Owner self-grant is
# still hidden).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists/{{playlist_id}}/shares
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$..user_id" not contains "{{bob_user_id}}"


# ─────────────────────────────────────────────────────────────
# Step 23 – Cleanup: Alice deletes the playlist. The service
# runs `authz.require(Delete)` (owner passes via the seeded Owner
# grant), then `revoke_all_for_resource` wipes any stray grants.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/playlists/{{playlist_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 24 – GET returns 404 after delete (nothing to enum).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/playlists/{{playlist_id}}
Authorization: Bearer {{alice_token}}

HTTP 404
