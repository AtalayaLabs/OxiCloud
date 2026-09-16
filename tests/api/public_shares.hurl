# =============================================================
# OxiCloud — Baseline: public-share token surface
# =============================================================
# Pins the legacy tokenized share flow (`/api/shares` to mint,
# `/api/s/{token}/*` to consume) — the only public-facing
# unauthenticated read surface in the product. Any regression
# in scope-enforcement here breaks the share-link feature for
# every external recipient.
#
# Coverage:
#   1. Login + seed: create a folder with a file inside.
#   2. POST /api/shares (folder share, no password)        → 201
#   3. GET  /api/shares                                   (lists ours)
#   4. GET  /api/shares/{id}                              (single fetch)
#   5. GET  /api/s/{token}              (no auth)          → 200
#   6. GET  /api/s/{token}/verify       — not applicable
#      for a password-less share, but the unauthenticated
#      anonymous probe of `/api/s/{token}` already exercises
#      the access path; verify is exercised in the password
#      branch below.
#   7. The browsing endpoints (/contents, /file/{id}, /zip,
#      /download) are DELETED → 404 from the routing table.
#      Browsing now happens through the ordinary API with an
#      anonymous session; see anonymous_share_session.hurl.
#   9. POST /api/shares — password-protected variant
#  10. GET  /api/s/{pw_token}                              → 401 (password required)
#  11. POST /api/s/{pw_token}/verify wrong pw              → 401
#  12. POST /api/s/{pw_token}/verify right pw              → 200
#  13. DELETE /api/shares/{id} (no-password)               → 204
#  14. GET /api/s/{token} after revoke                     → 404 / 410
#  15. Cleanup the password-share + folder.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin login, seed folder + file
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
admin_home_id: jsonpath "$[0].id"


POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "public-share-test", "parent_id": "{{admin_home_id}}" }

HTTP 201
[Captures]
share_folder_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{share_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
shared_file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# 2 — Mint a password-less folder share
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{share_folder_id}}",
  "item_type": "folder"
}

HTTP 201
[Captures]
share_id:    jsonpath "$.id"
share_token: jsonpath "$.token"
[Asserts]
jsonpath "$.has_password" == false
jsonpath "$.token"        matches "^[A-Za-z0-9_-]+$"


# ─────────────────────────────────────────────────────────────
# 3 — The share appears in the owner's listing
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "{{share_id}}"


# ─────────────────────────────────────────────────────────────
# 4 — Single-share fetch
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/shares/{{share_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.id"        == "{{share_id}}"
jsonpath "$.item_id"   == "{{share_folder_id}}"
jsonpath "$.item_type" == "folder"


# ─────────────────────────────────────────────────────────────
# 5 — Public access via the token, NO auth header. This is the
#     security-critical path: any auth check that creeps in
#     here breaks all external recipients.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/s/{{share_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 7 — The browsing endpoints are GONE.
#
#     `/contents`, `/contents/{folder_id}`, `/file/{file_id}`,
#     `/zip`, `/zip/{folder_id}` and `/download` were a second
#     implementation of `/api/folders/*` and `/api/files/*`,
#     reached by a service that executed as the share's OWNER
#     rather than as the caller. They are deleted: a visitor now
#     unlocks a ring at `/api/s/{token}` and uses the ordinary
#     API, authorized by the ReBAC grant the share already holds.
#
#     404 here is the routing table answering, not a handler —
#     which is the point. Browsing coverage moved to
#     `anonymous_share_session.hurl`, where it is exercised
#     against the real endpoints with a real anonymous session.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/s/{{share_token}}/contents

HTTP 404


GET {{base_url}}/api/s/{{share_token}}/file/{{shared_file_id}}

HTTP 404


GET {{base_url}}/api/s/{{share_token}}/zip

HTTP 404


GET {{base_url}}/api/s/{{share_token}}/download

HTTP 404


# ─────────────────────────────────────────────────────────────
# 8b — Direct file share: mint a share on the FILE itself
#      (item_type=file) and access it via /api/s/{token}.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{shared_file_id}}",
  "item_type": "file"
}

HTTP 201
[Captures]
file_share_id:    jsonpath "$.id"
file_share_token: jsonpath "$.token"


GET {{base_url}}/api/s/{{file_share_token}}

HTTP 200
[Asserts]
jsonpath "$.item_type" == "file"


# ─────────────────────────────────────────────────────────────
# 9 — Mint a password-protected share on the same folder.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{share_folder_id}}",
  "item_type": "folder",
  "password":  "secret-share-password-1!"
}

HTTP 201
[Captures]
pw_share_id:    jsonpath "$.id"
pw_share_token: jsonpath "$.token"
[Asserts]
jsonpath "$.has_password" == true


# ─────────────────────────────────────────────────────────────
# 10 — Anonymous probe must report "password required" without
#      leaking the shared item's contents.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/s/{{pw_share_token}}

HTTP 401
[Asserts]
jsonpath "$.requiresPassword" == true


# ─────────────────────────────────────────────────────────────
# 11 — Wrong password → 401 (does NOT issue an unlock cookie).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/s/{{pw_share_token}}/verify
Content-Type: application/json
{ "password": "obviously-wrong" }

HTTP 401


# ─────────────────────────────────────────────────────────────
# 12 — Right password → 200 + Set-Cookie unlock JWT.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/s/{{pw_share_token}}/verify
Content-Type: application/json
{ "password": "secret-share-password-1!" }

HTTP 200
[Asserts]
header "Set-Cookie" exists


# ─────────────────────────────────────────────────────────────
# 13 — Revoke the password-less share
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/shares/{{share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# 14 — After revocation the token must not resolve. Different
#      server versions return 404 vs 410 depending on whether
#      the row was hard-deleted or marked revoked — both are
#      acceptable rejections of the token; what matters is the
#      token does NOT yield a 200.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/s/{{share_token}}

HTTP *
[Asserts]
status >= 400
status <  500


# ─────────────────────────────────────────────────────────────
# 14b — Viewer-laundering regression (post-Drive AuthZ audit,
#       Round 1 HIGH). Before the fix, `POST /api/shares` checked
#       only "does the item exist" — any authenticated user who
#       could name the UUID could mint a public Viewer link,
#       laundering read access into a permanent anonymous URL
#       that survived their own grant revocation. Now the
#       service calls `authz.require(Share, resource)` before
#       minting the token; a caller without `Share`
#       (Viewer/Commenter/Contributor/no-grant-at-all) gets 404
#       (anti-enum) + `authz.denied` audit line. See
#       `docs/plan/authz_audit/admin_membership.md`.
#
#       We test the strongest form: an unrelated user with no
#       grant at all. The intermediate case (Viewer with Read
#       but not Share) is covered by the same code path — Share
#       is bundled only with owner/editor role_grants.
# ─────────────────────────────────────────────────────────────

# Create/lookup the attacker. Idempotent: `HTTP *` accepts either
# 201 (first run) or 409 (subsequent runs). Login below is the real
# precondition.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "sh_mallory", "password": "ShMalloryPassword1!", "email": "sh_mallory@example.com", "role": "user" }

HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "sh_mallory", "password": "ShMalloryPassword1!" }

HTTP 200
[Captures]
mallory_token: jsonpath "$.access_token"


# Step 14b.i — Mallory tries to mint a public share on admin's
#              folder: 404 (anti-enum). No token appears in the
#              response body.
POST {{base_url}}/api/shares
Authorization: Bearer {{mallory_token}}
Content-Type: application/json
{
  "item_id":   "{{share_folder_id}}",
  "item_name": "public-share-test",
  "item_type": "folder"
}

HTTP 404


# Step 14b.ii — Same attempt on admin's file: 404.
POST {{base_url}}/api/shares
Authorization: Bearer {{mallory_token}}
Content-Type: application/json
{
  "item_id":   "{{shared_file_id}}",
  "item_name": "hello.txt",
  "item_type": "file"
}

HTTP 404


# Step 14b.iii — Mallory has no shares — no partial success slipped
#                through. (`GET /api/shares` returns only shares the
#                caller created; response is paginated.)
GET {{base_url}}/api/shares
Authorization: Bearer {{mallory_token}}

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# 15 — Teardown: revoke the password share + the direct
#      file-share, then delete the folder.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/shares/{{pw_share_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/shares/{{file_share_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/folders/{{share_folder_id}}
Authorization: Bearer {{admin_token}}
HTTP 204
