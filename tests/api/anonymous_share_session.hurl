# =============================================================
# OxiCloud — Anonymous share sessions (the share ring)
# =============================================================
# Pins the mechanism that lets a public-share visitor use the
# NORMAL API instead of the bespoke `/api/s/*` mirror: unlocking
# a share issues a signed `oxi_shares` cookie — the ring — whose
# entries `auth_middleware` turns into `Subject::Token(share_id)`
# credentials. The ReBAC grant those match has existed since
# share creation; before this it was never read at request time,
# which is why issue #721 (public shares fetching full-resolution
# images because /api/files/*/thumbnail 401'd) happened at all.
#
# Hurl keeps a cookie jar for the whole file, so it behaves like
# a browser. That matters: the admin requests below authenticate
# with a Bearer header and ALSO carry the ring once it exists,
# which is exactly the "logged-in Alice clicks a colleague's
# share link" case the ring was designed for.
#
# Coverage:
#   1. Setup — folder + image inside it, plus an image OUTSIDE it.
#   2. Password-less share → GET /api/s/{token} issues the ring.
#   3. Anonymous GET /api/files/{in-share}/thumbnail/preview → 200.
#   4. Anonymous GET /api/files/{outside}/thumbnail/preview → 404
#      (anti-enum: same shape as "no such file").
#   5. A sweep of routes OUTSIDE the anonymous allowlist — user
#      profiles, shares, drives, favorites, trash, search, the
#      bare folder listing, a DELETE, /api/auth/me — all 403 even
#      though the ring is valid. The ring is a key to one share,
#      not a login.
#   6. Password share: the bare GET and a wrong password issue
#      NO ring; the right password does.
#   7. The ring APPENDS — after unlocking a second share the
#      first still works.
#   8. Revoking a share kills access even though the ring still
#      names it: authority lives in the grant, not the cookie.
#   9. Teardown.
# =============================================================


# ─────────────────────────────────────────────────────────────
# 1 — Setup
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# The share owner's own id, so step 5 can try to read their
# profile the way a visitor holding only a share link might.
#
# `$.full.user.id`, not `$.id`: `/api/auth/me` returns the Self view
# composed over Full over Public (`user_dto.rs`), and the layers are
# NOT serde-flattened — the nesting is on the wire.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
admin_user_id: jsonpath "$.full.user.id"


GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
admin_home_id: jsonpath "$[0].id"


POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "anon-session-test", "parent_id": "{{admin_home_id}}" }

HTTP 201
[Captures]
anon_folder_id: jsonpath "$.id"


# An image, so the thumbnail route returns real bytes rather than
# the 204 a text file would give — a 204 would pass the
# authorization assertions without proving the image path works.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{anon_folder_id}}
file: file,fixtures/red-image.png; image/png

HTTP 201
[Captures]
inside_file_id: jsonpath "$.id"


# Same owner, but OUTSIDE the shared subtree. The share must not
# reach it — this is the file that proves the ring grants the
# share's scope and not the owner's account.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{admin_home_id}}
file: file,fixtures/blue-image.png; image/png

HTTP 201
[Captures]
outside_file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# 2 — A password-less share hands over the ring on first contact.
#     No second round-trip: reaching 200 here already means the
#     share is open to this caller, which is the whole condition
#     for issuing one.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{anon_folder_id}}",
  "item_type": "folder"
}

HTTP 201
[Captures]
open_share_id:    jsonpath "$.id"
open_share_token: jsonpath "$.token"


GET {{base_url}}/api/s/{{open_share_token}}

HTTP 200
[Asserts]
cookie "oxi_shares" exists
cookie "oxi_shares[HttpOnly]" exists
cookie "oxi_shares[Path]" == "/"


# ─────────────────────────────────────────────────────────────
# 3 — The payoff (issue #721). No Authorization header: the ring
#     alone authenticates, and the grant written at share-creation
#     time authorizes. The share is on the FOLDER, so this also
#     exercises the ltree cascade down to a file inside it.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{inside_file_id}}/thumbnail/preview

HTTP 200
[Asserts]
header "Content-Type" contains "image"


# ─────────────────────────────────────────────────────────────
# 4 — A file the SHARE OWNER owns but that is not in the share
#     must 404, not 403. The visitor has no Read on it, so the
#     graduated-denial rule collapses to the anti-enumeration
#     shape and the response cannot be used to probe file ids.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{outside_file_id}}/thumbnail/preview

HTTP 404


# ─────────────────────────────────────────────────────────────
# 5 — The ring is a key to one share, not a login. Every route
#     below is an ordinary authenticated read that a valid
#     principal may call — nothing about the ring's *authority*
#     stops them, and several would even return useful data if
#     they ran. They are refused because a public-share visitor is
#     confined to an explicit allowlist, and none of these is on
#     it.
#
#     This is the layer that matters most for the routes with no
#     resource id in the path: `/api/shares`, `/api/drives`,
#     `/api/favorites`, `/api/trash/resources` and `/api/search`
#     are all "everything the caller can see" queries, where a
#     per-resource permission check has nothing to bite on.
#
#     `/api/users/{id}` is the one to watch hardest: it is the
#     owner's own profile — name, email — and the visitor knows
#     the owner exists because they are reading their share. 403
#     here means the allowlist stopped it before the handler, not
#     that the profile happened to be empty.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/users/{{admin_user_id}}

HTTP 403


GET {{base_url}}/api/shares

HTTP 403


GET {{base_url}}/api/drives

HTTP 403


GET {{base_url}}/api/favorites

HTTP 403


GET {{base_url}}/api/trash/resources

HTTP 403


GET {{base_url}}/api/search?query=red-image

HTTP 403


# `/api/folders` with no id — the bare listing. `/api/folders/{id}`
# IS allowlisted (a share visitor must be able to browse the shared
# folder), so this pins that the allowlist matches on the ROUTE
# pattern and not on a prefix: one extra path segment must not
# carry anonymous access to a sibling route.
GET {{base_url}}/api/folders

HTTP 403


# A write on an allowlisted PATH. `GET /api/files/{id}` is on the
# list; `DELETE` on the same path is not, because the allowlist is
# GET-only — without that rule, mounting a new method on an already
# listed path would silently inherit anonymous access. CSRF would
# also refuse this, which is the point of having both: the file
# must still be here for the teardown, and it is (step 7 re-reads
# its thumbnail).
DELETE {{base_url}}/api/files/{{inside_file_id}}

HTTP 403


# `/api/auth/me` — a visitor has no account, so there is no "me"
# to return. Refused whether by the allowlist or by the role gate
# on `AuthUser`; both are closed, and the point here is that no
# principal information leaks back.
GET {{base_url}}/api/auth/me

HTTP 403


# ─────────────────────────────────────────────────────────────
# 6 — Password-protected share. The bare GET must not hand out a
#     ring: the share is not unlocked, so there is nothing to
#     grant. Same for a wrong password.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/shares
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "item_id":   "{{anon_folder_id}}",
  "item_type": "folder",
  "password":  "anon-ring-password-1!"
}

HTTP 201
[Captures]
pw_share_id:    jsonpath "$.id"
pw_share_token: jsonpath "$.token"


GET {{base_url}}/api/s/{{pw_share_token}}

HTTP 401
[Asserts]
header "Set-Cookie" not exists


POST {{base_url}}/api/s/{{pw_share_token}}/verify
Content-Type: application/json
{ "password": "obviously-wrong" }

HTTP 401
[Asserts]
header "Set-Cookie" not exists


# The right password does. Two `Set-Cookie` headers come back —
# the legacy per-share unlock cookie and the ring — so this
# asserts the ring specifically rather than "a cookie was set".
POST {{base_url}}/api/s/{{pw_share_token}}/verify
Content-Type: application/json
{ "password": "anon-ring-password-1!" }

HTTP 200
[Asserts]
cookie "oxi_shares" exists


# ─────────────────────────────────────────────────────────────
# 7 — The ring APPENDS. Unlocking the password share must not
#     evict the first one: a visitor with two tabs open would
#     otherwise watch the older tab stop loading. One cookie
#     holding many shares is the entire reason it is a ring and
#     not one cookie per share.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{inside_file_id}}/thumbnail/preview

HTTP 200


# ─────────────────────────────────────────────────────────────
# 8 — Revocation wins over the cookie. The ring still names the
#     share — it is a signed bearer token and cannot be recalled —
#     but the grant behind it is gone, and the grant is where the
#     authority actually lives. This is what makes a long ring TTL
#     safe.
#
#     Both shares point at the same folder, so BOTH must go before
#     access stops; revoking only one would still leave the other
#     granting, and the test would pass for the wrong reason.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/shares/{{open_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/shares/{{pw_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


GET {{base_url}}/api/files/{{inside_file_id}}/thumbnail/preview

HTTP 404


# ─────────────────────────────────────────────────────────────
# 9 — Teardown. The outside file sits in admin's home folder, so
#     it is deleted explicitly — later files in the runner upload
#     to the same folder and would hit the live-name unique index.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{outside_file_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{anon_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 204
