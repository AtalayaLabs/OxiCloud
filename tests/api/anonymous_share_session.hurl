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
# ── READ THIS BEFORE ADDING A REQUEST ────────────────────────
# Hurl keeps ONE cookie jar for the whole file, and
# `POST /api/auth/login` sets `oxicloud_access` (HttpOnly,
# `Path=/`) alongside the Bearer token it returns. So every entry
# after a login is cookie-authenticated as that user whether or
# not it carries an `Authorization` header — dropping the header
# does NOT make a request anonymous.
#
# This file is therefore split into explicit sessions separated
# by `POST /api/auth/logout`, which clears the three auth cookies
# and leaves `oxi_shares` (a different name) untouched. The first
# entry of every anonymous section is a canary asserting we really
# are logged out; without it a jar leak silently turns the whole
# section into a test of the admin's own permissions, which passes
# for entirely the wrong reason.
#
# Coverage:
#   1. Setup as admin — folder + image inside it, an image
#      OUTSIDE it, and both shares (they must be minted while
#      still authenticated).
#   2. Still logged in: unlocking a share must NOT downgrade a
#      real session — admin keeps reading their own files.
#   3. Logout → genuinely anonymous. Canary, then the payoff:
#      `GET /api/files/{in-share}` and its `/thumbnail/preview`
#      → 200.
#   4. A file OUTSIDE the share → 404 on BOTH (anti-enum: the same
#      shape as "no such file"). Every file request is asserted
#      through both routes, because they must agree — one opened
#      without the other is how a visitor gains a capability
#      nobody meant to grant.
#   4b. The folder routes, as a matrix: the share root and a CHILD
#      of it resolve (the cascade descends), while the PARENT and a
#      SIBLING 404 (it neither climbs nor wanders sideways). All
#      four routes are asserted on every folder — the ZIP most of
#      all, since it is the one that would stream the owner's whole
#      tree if it were the route that got it wrong.
#   5. A sweep of routes outside the anonymous allowlist — user
#      profiles, shares, drives, favorites, trash, search, the
#      bare folder listing, a DELETE, /api/auth/me — all 403.
#      The ring is a key to one share, not a login.
#   6. Password share: the bare GET and a wrong password issue
#      NO ring; the right password does.
#   7. The ring APPENDS — after unlocking a second share the
#      first still works.
#   8. Revoking the shares kills access even though the ring
#      still names them: authority lives in the grant, not the
#      cookie.
#   9. Teardown.
# =============================================================


# ─────────────────────────────────────────────────────────────
# 1 — Setup (admin session #1)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# The share owner's own id, so the sweep can try to read their
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


# Same owner, but OUTSIDE the shared subtree — it sits in the
# PARENT of the shared folder, so it is only reachable by walking
# UP. The ltree cascade grants downward only; this is the file
# that proves the ring carries the share's scope and not the
# owner's account.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{admin_home_id}}
file: file,fixtures/blue-image.png; image/png

HTTP 201
[Captures]
outside_file_id: jsonpath "$.id"


# A CHILD of the shared folder. Without this the suite never proves
# the cascade DESCENDS — every other assertion targets the share
# root itself, so a cascade broken downward would still pass.
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "anon-session-child", "parent_id": "{{anon_folder_id}}" }

HTTP 201
[Captures]
child_folder_id: jsonpath "$.id"


# A SIBLING of the shared folder: same parent, same drive, neither
# ancestor nor descendant. The parent case above pins that the
# cascade does not climb; this pins that it does not wander
# sideways. Same SQL predicate, different shape of mistake — and it
# is the one that would survive someone "simplifying" the check
# into a same-drive test.
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "anon-session-sibling", "parent_id": "{{admin_home_id}}" }

HTTP 201
[Captures]
sibling_folder_id: jsonpath "$.id"


# Both shares are minted now, while authenticated — after the
# logout below there is no one left who may create them.
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
[Asserts]
jsonpath "$.has_password" == true


# ─────────────────────────────────────────────────────────────
# 2 — A password-less share hands over the ring on first contact:
#     reaching 200 here already means the share is open to this
#     caller, which is the whole condition for issuing one. No
#     second round-trip.
#
#     Done while still logged in on purpose. `require_any` puts
#     the user's own subject first, so gaining a ring must cost
#     the user nothing — the assertion below is that admin still
#     reads a file the ring does NOT cover.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/s/{{open_share_token}}

HTTP 200
[Asserts]
cookie "oxi_shares" exists
cookie "oxi_shares[HttpOnly]" exists
cookie "oxi_shares[Path]" == "/"


# Admin now holds BOTH credentials — their session cookie and the
# ring. The outside file is theirs and stays readable: clicking a
# share link must never downgrade a real session.

GET {{base_url}}/api/files/{{outside_file_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


GET {{base_url}}/api/files/{{outside_file_id}}/thumbnail/preview
Authorization: Bearer {{admin_token}}

HTTP 200


# The OWNER's view of the shared folder, captured here so the
# anonymous view further down can be compared against it rather
# than asserted in isolation. Both fields below are withheld from a
# token caller, and a test that only checked the anonymous side
# would also pass if the fields had simply stopped working.
GET {{base_url}}/api/folders/{{anon_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.path" != ""
jsonpath "$.is_shared" == true
jsonpath "$.created_by" exists


# ─────────────────────────────────────────────────────────────
# 3 — End the admin session. This clears `oxicloud_access`,
#     `oxicloud_refresh` and `oxicloud_csrf`; `oxi_shares` has a
#     different name and survives, which is exactly the state a
#     share visitor is in.
#
#     Bearer is CSRF-exempt (`csrf.rs`), and the refresh token the
#     handler needs comes from the cookie the jar already holds.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/logout
Authorization: Bearer {{admin_token}}

HTTP 200


# CANARY. Everything below depends on being anonymous, and the
# failure mode is silent: a surviving session cookie would make
# the next assertions test the admin's own permissions instead.
# 403 here means the principal is a share visitor — `AuthUser`'s
# role gate refused it — and not a user.
GET {{base_url}}/api/auth/me

HTTP 403


# ─────────────────────────────────────────────────────────────
# 3.1 — The payoff (issue #721). No session, no Authorization
#     header: the ring alone authenticates, and the grant written
#     at share-creation time authorizes. The share is on the
#     FOLDER, so this also exercises the ltree cascade down to a
#     file inside it.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{inside_file_id}}

HTTP 200


GET {{base_url}}/api/files/{{inside_file_id}}/thumbnail/preview

HTTP 200
[Asserts]
header "Content-Type" contains "image"


# ─────────────────────────────────────────────────────────────
# 4 — A file the SHARE OWNER owns but that is not in the share
#     must 404, not 403: the visitor has no Read on it, so the
#     graduated-denial rule collapses to the anti-enumeration
#     shape and the response cannot be used to probe file ids.
#
#     Both routes are asserted because they must agree. They take
#     different paths to the same answer — the download route
#     authorizes then reads a DTO, the thumbnail route authorizes
#     then resolves a derived blob — and a divergence between them
#     is exactly the shape of bug worth catching: one route opened
#     without the other is how a share visitor ends up with a
#     capability nobody meant to grant.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{outside_file_id}}

HTTP 404


GET {{base_url}}/api/files/{{outside_file_id}}/thumbnail/preview

HTTP 404


# ─────────────────────────────────────────────────────────────
# 4b — The folder routes. These are what make the share BROWSABLE
#      with the ordinary components instead of the bespoke
#      `/api/s/{token}/contents` mirror.
#
#      Each carries a disclosure the owner's view has and the
#      visitor's must not — compare against the owner assertions
#      captured in step 2.
# ─────────────────────────────────────────────────────────────

# The shared folder itself. `path` is blanked because it names the
# folder's position in the OWNER's tree, including ancestors above
# the share root. `is_shared` is blanked because it is a
# subject-less EXISTS over `role_grants` — computed honestly it
# would be `true` here (the share's own token grant is such a row),
# which is precisely the signal a visitor must not receive.
GET {{base_url}}/api/folders/{{anon_folder_id}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{anon_folder_id}}"
jsonpath "$.path" == ""
jsonpath "$.is_shared" == false
jsonpath "$.is_favorite" == false
# The owner's stable identifiers. A visitor cannot resolve them to a
# name, but the same id across two unrelated public links correlates
# them to one person — see `FolderDto::redacted_for_token`.
jsonpath "$.created_by" not exists
jsonpath "$.updated_by" not exists


# The listing. Both children are visible, and `is_shared` must be
# false on every row or the visitor learns which items the owner
# has ALSO published elsewhere.
#
# `$.items[N].resource.*` — `ResourceContentDto` is `#[serde(untagged)]`,
# so the file/folder fields sit directly under `resource` with no
# wrapper key; `resource_type` alongside it is the discriminator.
# Rows are asserted by index rather than by filter because the
# default `order_by=name` puts folders first — pinning the order is
# deliberate, so a change to it fails loudly instead of quietly
# skipping the row it was meant to check.
GET {{base_url}}/api/folders/{{anon_folder_id}}/resources

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource_type" == "folder"
jsonpath "$.items[0].resource.id" == "{{child_folder_id}}"
jsonpath "$.items[0].resource.is_shared" == false
jsonpath "$.items[0].resource.is_favorite" == false
jsonpath "$.items[1].resource_type" == "file"
jsonpath "$.items[1].resource.id" == "{{inside_file_id}}"
jsonpath "$.items[1].resource.is_shared" == false
jsonpath "$.items[1].resource.is_favorite" == false
# Redaction applies per row, to files as well as folders.
jsonpath "$.items[0].resource.created_by" not exists
jsonpath "$.items[1].resource.created_by" not exists


# The breadcrumb. The walk truncates at the share root — a visitor
# must not learn the names of the folders above it — and reports
# `token`, not `direct_share`.
#
# The distinction is not cosmetic: `direct_share` makes the FE render
# "Shared with you by X" and link the root chip to `/shared-with-me`,
# neither of which exists for a visitor. `subject` and `drive` must be
# absent — naming the sharer would publish the owner's identity to an
# anonymous caller, and the drive is not the visitor's to know.
GET {{base_url}}/api/folders/{{anon_folder_id}}/ancestors

HTTP 200
[Asserts]
jsonpath "$.access_source.kind" == "token"
jsonpath "$.access_source.subject" not exists
jsonpath "$.access_source.drive" not exists
jsonpath "$.ancestors" count == 1
jsonpath "$.ancestors[0].id" == "{{anon_folder_id}}"


# The ZIP of the shared subtree — the same bytes the legacy
# `/api/s/{token}/zip` endpoint has always served.
GET {{base_url}}/api/folders/{{anon_folder_id}}/download

HTTP 200
[Asserts]
header "Content-Type" contains "zip"


# ── The cascade DESCENDS: a child of the share root ──────────
# Without these, a cascade broken downward would pass every other
# assertion in this file — they all target the share root itself.
GET {{base_url}}/api/folders/{{child_folder_id}}

HTTP 200
[Asserts]
jsonpath "$.path" == ""
jsonpath "$.is_shared" == false


GET {{base_url}}/api/folders/{{child_folder_id}}/resources

HTTP 200


# Two crumbs, not the full chain from the drive root: the walk stops
# at the share boundary. This is the assertion that proves truncation
# rather than merely observing a short chain — the child genuinely has
# ancestors above the share root, and none of them may appear.
GET {{base_url}}/api/folders/{{child_folder_id}}/ancestors

HTTP 200
[Asserts]
jsonpath "$.access_source.kind" == "token"
jsonpath "$.ancestors" count == 2
jsonpath "$.ancestors[0].id" == "{{anon_folder_id}}"
jsonpath "$.ancestors[1].id" == "{{child_folder_id}}"


GET {{base_url}}/api/folders/{{child_folder_id}}/download

HTTP 200
[Asserts]
header "Content-Type" contains "zip"


# ── The cascade does NOT climb: the parent of the share root ──
# The ltree cascade grants downward only, so walking UP must 404 —
# the folder counterpart of the outside-file assertions above.
# Every route, because the zip is the one that would stream the
# owner's entire tree if it were the route that got it wrong.
GET {{base_url}}/api/folders/{{admin_home_id}}

HTTP 404


GET {{base_url}}/api/folders/{{admin_home_id}}/resources

HTTP 404


GET {{base_url}}/api/folders/{{admin_home_id}}/ancestors

HTTP 404


GET {{base_url}}/api/folders/{{admin_home_id}}/download

HTTP 404


# ── The cascade does NOT wander sideways: a sibling ───────────
# Same parent, same drive, neither ancestor nor descendant. The
# parent block above would also fail if the check were loosened to
# "same drive"; this one fails if it were loosened to "same parent",
# which is the likelier slip.
GET {{base_url}}/api/folders/{{sibling_folder_id}}

HTTP 404


GET {{base_url}}/api/folders/{{sibling_folder_id}}/resources

HTTP 404


GET {{base_url}}/api/folders/{{sibling_folder_id}}/ancestors

HTTP 404


GET {{base_url}}/api/folders/{{sibling_folder_id}}/download

HTTP 404


# ─────────────────────────────────────────────────────────────
# 5 — The ring is a key to one share, not a login. Every route
#     below is an ordinary authenticated read that a valid
#     principal may call — nothing about the ring's *authority*
#     stops them, and several would return useful data if they
#     ran. They are refused because a public-share visitor is
#     confined to an explicit allowlist, and none is on it.
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
#     means the allowlist stopped it before the handler, not that
#     the profile happened to be empty.
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
# IS allowlisted (a share visitor must be able to browse the
# shared folder), so this pins that the allowlist matches on the
# ROUTE pattern and not on a prefix: one extra path segment must
# not carry anonymous access to a sibling route.
GET {{base_url}}/api/folders

HTTP 403


# A write on an allowlisted PATH. `GET /api/files/{id}` is on the
# list; `DELETE` on the same path is not, because the allowlist is
# GET-only — without that rule, mounting a new method on an
# already listed path would silently inherit anonymous access.
# CSRF would also refuse this, which is the point of having both.
# The file must survive for the teardown, and it does — step 7
# re-reads its thumbnail.
DELETE {{base_url}}/api/files/{{inside_file_id}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# 5b — The surfaces the allowlist does NOT guard.
#
#      Everything above is refused by `anonymous_allowlist_layer`,
#      which only wraps the protected `/api/*` router. These four
#      groups sit outside it and each refuses for its own reason —
#      which is exactly why they need asserting: a change to the
#      allowlist would not affect any of them, so nothing else in
#      this file would notice if one opened up.
# ─────────────────────────────────────────────────────────────

# Other authenticated `/api/*` reads. These ARE inside the router,
# so the allowlist refuses them — listed because they are the
# directory-style queries an audit asks about first.
GET {{base_url}}/api/groups/search?q=a

HTTP 403


GET {{base_url}}/api/address-books

HTTP 403


# The DAV surfaces. `auth_middleware` emits the Basic-auth
# challenge BEFORE it ever looks at the ring, so a ring can never
# authenticate WebDAV/CalDAV/CardDAV — the ordering is the
# guarantee, not a check. 401 + WWW-Authenticate, not 403.
GET {{base_url}}/webdav/

HTTP 401
[Asserts]
header "WWW-Authenticate" contains "Basic"


GET {{base_url}}/caldav/

HTTP 401


GET {{base_url}}/carddav/

HTTP 401


# Session machinery a visitor must not reach.
#
# `refresh` is public and rate-limited — no middleware to consult,
# it simply finds no refresh token and answers 401 itself.
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{}

HTTP 401


# App passwords live in the `/api/auth` nest, which has its OWN
# middleware stack and no allowlist layer. The refusal comes from
# `require_role` inside the `AuthUser` extractor instead — the
# second, independent gate, on a route the first never sees.
# Minting a DAV credential is the last thing a share link should
# be able to do.
POST {{base_url}}/api/auth/app-passwords
Content-Type: application/json
{ "name": "visitor" }

HTTP 403


# `POST /api/rt/ticket` IS inside the protected router, so here the
# allowlist is what refuses (unlisted, and POST besides).
POST {{base_url}}/api/rt/ticket
Content-Type: application/json
{}

HTTP 403


# `GET /api/rt/ws` is the one that needed its own defence: it is
# registered OUTSIDE the protected router (a browser cannot set
# `Authorization` on `new WebSocket()`, so the upgrade carries its
# credential in a subprotocol and self-authenticates). The
# allowlist never sees it.
#
# A visitor holds only the `oxi_shares` cookie — no ticket, no
# bearer — so `authenticate_upgrade` ends at `no_credentials`. The
# handler ALSO refuses an anonymous-role bearer outright: without
# that, closing `/api/rt/ticket` would not close this door, and a
# scripted client could hold one socket per share visitor. That
# path is unreachable today (the ring is a cookie of a different
# `typ`, never a bearer), which is why this asserts the door rather
# than the specific key.
#
# The handshake headers are REQUIRED to make this a real test.
# `WebSocketUpgrade` is an extractor, so a plain GET is rejected
# with 400 before the handler body — and a 400 would only prove
# axum parses headers, not that the socket is closed to visitors.
# With a valid handshake the extractor passes and the auth check
# is what answers. `Sec-WebSocket-Key` is the RFC 6455 example
# value; nothing verifies it on a rejected upgrade.
GET {{base_url}}/api/rt/ws
Connection: Upgrade
Upgrade: websocket
Sec-WebSocket-Version: 13
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==

HTTP 401


# ─────────────────────────────────────────────────────────────
# 6 — Password-protected share. The bare GET must not hand out a
#     ring: the share is not unlocked, so there is nothing to
#     grant. Same for a wrong password.
# ─────────────────────────────────────────────────────────────
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
# 7 — The ring APPENDS. Unlocking the second share must not evict
#     the first: a visitor with two tabs open would otherwise
#     watch the older one stop loading. One cookie holding many
#     shares is the entire reason it is a ring and not one cookie
#     per share.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{inside_file_id}}

HTTP 200


GET {{base_url}}/api/files/{{inside_file_id}}/thumbnail/preview

HTTP 200


# ─────────────────────────────────────────────────────────────
# 8 — Revocation wins over the cookie. The ring still names both
#     shares — it is a signed bearer token and cannot be recalled
#     — but the grants behind them are gone, and the grant is
#     where the authority actually lives. This is what makes a
#     long ring TTL safe.
#
#     Both shares point at the same folder, so BOTH must go
#     before access stops; revoking one would leave the other
#     granting and the test would pass for the wrong reason.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


DELETE {{base_url}}/api/shares/{{open_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/shares/{{pw_share_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# Back to the visitor's seat — the revocation check is only
# meaningful without a session behind it.
POST {{base_url}}/api/auth/logout
Authorization: Bearer {{admin_token}}

HTTP 200


GET {{base_url}}/api/auth/me

HTTP 403

GET {{base_url}}/api/files/{{inside_file_id}}

HTTP 404


# The revoked-link check that matters. The grant row is removed by
# the `trg_cleanup_role_grants_token` trigger on `storage.shares`
# — in the DATABASE, so it never passes through
# `PgAclEngine::clear_role` and never fires the cascade-cache flush
# that every other File/Folder revocation gets.
#
# A 200 here means the engine is still serving a cached decision
# this visitor seeded a few requests ago. That window is the full
# 30 s cache TTL, and the whole run takes under a second, so
# without the explicit flush in `delete_shared_link` this assertion
# fails every time — as it did.
GET {{base_url}}/api/files/{{inside_file_id}}/thumbnail/preview

HTTP 404


# ─────────────────────────────────────────────────────────────
# 9 — Teardown. The outside file sits in admin's home folder, so
#     it is deleted explicitly — later files in the runner upload
#     to the same folder and would hit the live-name unique index.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


DELETE {{base_url}}/api/files/{{outside_file_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# The sibling also lives in admin's home folder, so it needs its own
# delete. The child does not — it goes with the shared folder below.
DELETE {{base_url}}/api/folders/{{sibling_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{anon_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 204
