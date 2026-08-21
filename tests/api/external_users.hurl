# =============================================================
# OxiCloud — invite-by-email + magic-link redemption (PR 9)
# =============================================================
# End-to-end: Alice shares a folder with bob@externalcompany.com,
# the server lazily provisions bob as an external user, sends the
# invitation through MockEmailSender, and bob redeems the magic
# link to land authenticated on the resource.
#
# Requires `OXICLOUD_SMTP_MOCK=true` in tests/common/server.env so
# the in-process capture endpoint at /api/admin/smtp/test/captured
# is mounted. The .hurl file would error on a real SMTP setup
# because the magic link wouldn't be retrievable.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Alice logs in (admin) and grabs her home folder id.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"
alice_user_id: jsonpath "$.user.full.user.id"

GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Alice creates a folder she's about to share by email.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "ext-share", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
ext_folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Alice shares with bob@externalcompany.com via the new
#           subject.type=email payload. Server lazily provisions
#           bob as an external user.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "email", "email": "bob@externalcompany.com" },
  "resource": { "type": "folder", "id": "{{ext_folder_id}}" },
  "role":     "viewer"
}

HTTP 201
# The response carries the resolved subject as a regular user UUID —
# externals never surface as a distinct subject_type post-PR-9.3a.
# PR N1: POST /api/grants now wraps the array in
# `CreateGrantResponseDto { grants, notification }`.
[Asserts]
jsonpath "$.grants[0].subject.type" == "user"
jsonpath "$.grants[0].resource.id" == "{{ext_folder_id}}"
[Captures]
bob_user_id: jsonpath "$.grants[0].subject.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Alice's /grants/outgoing lists bob as a grantee.
#           The endpoint groups by resource and exposes the
#           subject display string (here: bob's email-as-username).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants/outgoing/resources
Authorization: Bearer {{alice_token}}

HTTP 200
# Hurl's `includes` predicate type-mismatches when JSONPath returns a
# scalar (single-grantee case) instead of an array, so we assert on
# the raw body — robust regardless of result count + ordering.
[Asserts]
body contains "bob@externalcompany.com"
body contains "{{bob_user_id}}"


# ─────────────────────────────────────────────────────────────
# Step 5 — Defense gap #2: bob must NOT appear in the system
#           address book. The contacts handler filters externals
#           via `include_external = false` (PR 6).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books/system/contacts
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
body not contains "bob@externalcompany.com"
body not contains "{{bob_user_id}}"


# ─────────────────────────────────────────────────────────────
# Step 6 — Retrieve the invitation email captured by the mock
#           sender BEFORE issuing any further mail (the mock only
#           remembers the latest message per recipient), then
#           extract the magic-link URL out of the plain-text body.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/smtp/test/captured?to=bob@externalcompany.com
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.to" == "bob@externalcompany.com"
jsonpath "$.subject" contains "shared a folder with you"
jsonpath "$.text_body" matches "/magic/v1/[A-Za-z0-9_-]+"
[Captures]
magic_url: jsonpath "$.text_body" regex "(https?://[^\\s]+/magic/v1/[A-Za-z0-9_-]+)"


# ─────────────────────────────────────────────────────────────
# Step 7 — Re-sharing the same email reuses bob — no second
#           external user gets created. The response carries the
#           same user_id captured in Step 3.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "ext-share-2", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
ext_folder_id_2: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "email", "email": "bob@externalcompany.com" },
  "resource": { "type": "folder", "id": "{{ext_folder_id_2}}" },
  "role":     "viewer"
}

HTTP 201
[Asserts]
jsonpath "$.grants[0].subject.id" == "{{bob_user_id}}"


# ─────────────────────────────────────────────────────────────
# Step 8 — Redeem the magic link. The handler 302s to the SPA
#           hash-route for the shared folder and sets the auth
#           cookies. Hurl follows-mode is OFF by default; we want
#           to inspect the Location header AND the Set-Cookie.
# ─────────────────────────────────────────────────────────────
GET {{magic_url}}

HTTP 302
[Asserts]
# SvelteKit `files/[...path]` accepts a folder ID as a path segment.
# Historical value pre-migration was `/#/files/folder/{id}` (legacy
# vanilla-frontend hash-routing). Kept in sync with the redemption
# handler in src/interfaces/api/handlers/magic_link_handler.rs.
header "Location" == "/files/{{ext_folder_id}}"
[Captures]
bob_access_token: cookie "oxicloud_access"


# ─────────────────────────────────────────────────────────────
# Step 9 — Bob (now carrying the cookie-issued JWT as bearer)
#           can read the shared folder. Without the magic-link
#           grant this would be 404 anti-enumeration.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{ext_folder_id}}
Authorization: Bearer {{bob_access_token}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{ext_folder_id}}"
jsonpath "$.name" == "ext-share"


# ─────────────────────────────────────────────────────────────
# Step 10 — Bob sees the shared folder in his /grants/incoming.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants/incoming/resources
Authorization: Bearer {{bob_access_token}}

HTTP 200
[Asserts]
body contains "{{ext_folder_id}}"


# ─────────────────────────────────────────────────────────────
# Step 11 — External-user lockouts (PR 11.1 + ContactsHandler).
#           Bob (external) must NOT reach the system address book
#           or the per-user profile endpoint. Defense-in-depth on
#           top of the PR 6 service-level filter.
# ─────────────────────────────────────────────────────────────

# 11a — system address book: visible at the catalog level
# (`GET /api/address-books`) for bob? It must NOT list the system entry.
GET {{base_url}}/api/address-books
Authorization: Bearer {{bob_access_token}}

HTTP 200
[Asserts]
body not contains "OxiCloud Users"
body not contains "\"id\":\"system\""

# 11b — system contacts listing: 403 for bob.
GET {{base_url}}/api/address-books/system/contacts
Authorization: Bearer {{bob_access_token}}

HTTP 403

# 11c — /api/users/{id}: bob CAN look up his own profile (self-lookup
#        is the first allow rule) so the SharedWithMe view can show
#        his own avatar in the user menu. After PR 16 externals have
#        NULL username (the field is omitted from JSON when None) —
#        the email field is the identity.
GET {{base_url}}/api/users/{{bob_user_id}}
Authorization: Bearer {{bob_access_token}}

HTTP 200
[Asserts]
# `/api/users/{id}` returns the slim `PublicUserDto` (9 fields) —
# id / email / username / role / image / is_external / given_name /
# family_name / is_online. Admin-visible fields like
# `email_verified_at` moved to `FullUserDto` under the three-layer
# refactor (docs/plan/userdto-refactor.md) and are checked below
# via `/api/admin/users`.
jsonpath "$.id" == "{{bob_user_id}}"
jsonpath "$.is_external" == true
jsonpath "$.email" == "bob@externalcompany.com"
jsonpath "$.username" not exists

# 11d — bob CAN look up Alice (his granter) — shared-grant relationship
#        lets the external recipient resolve the sharer's display name +
#        photo for the SharedWithMe view's owner column.
GET {{base_url}}/api/users/{{alice_user_id}}
Authorization: Bearer {{bob_access_token}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{alice_user_id}}"
jsonpath "$.is_external" == false

# 11c/d/verify — admin (alice) observes email_verified_at on both
# users via `GET /api/admin/users/{id}` — returns `FullUserDto`
# (public identity in `.user` + admin-visible extras at top level).
#
# `email_verified_at` lives on `FullUserDto` (admin+self-visible),
# not on `PublicUserDto` — peer views via `/api/users/{id}` never
# expose it. The admin single-user endpoint is the correct
# observation surface. See `docs/plan/userdto-refactor.md` for the
# three-layer split.
#
# Setup admin auto-verified rationale: `setup_create_admin` stamps
# `email_verified_at = NOW()` — admin fiat counts as verification,
# matching the OIDC-JIT convention. An operator running the first-run
# wizard is authoritative by construction. Without this, flipping
# `OXICLOUD_REQUIRE_VERIFIED_EMAIL=true` on an existing deployment
# would lock the sole admin out of their own instance.
GET {{base_url}}/api/admin/users/{{bob_user_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.user.id" == "{{bob_user_id}}"
jsonpath "$.email_verified_at" exists

GET {{base_url}}/api/admin/users/{{alice_user_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.user.id" == "{{alice_user_id}}"
jsonpath "$.email_verified_at" exists

# 11e — bob CANNOT enumerate unrelated users. A random UUID returns 404
#        (anti-enumeration; same response as "user doesn't exist").
GET {{base_url}}/api/users/00000000-0000-0000-0000-baadbeef1234
Authorization: Bearer {{bob_access_token}}

HTTP 404

# 11f — bob CANNOT create an app password. Externals are
#        magic-link-only; an app password would be a persistent
#        credential bypassing has_login_credential().
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{bob_access_token}}
Content-Type: application/json
{ "label": "rogue" }

HTTP 403

# 11g — bob CANNOT enumerate groups via the share-dialog endpoint.
#        Defence-in-depth on top of the ReBAC layer (externals can't
#        be group members today anyway).
GET {{base_url}}/api/groups/search?q=any
Authorization: Bearer {{bob_access_token}}

HTTP 403

# 11h — bob CANNOT reach the WebDAV protocol surface. He has no home
#        folder, so the protocol has no semantic meaning for him.
#        Layered before the handler so even malformed PROPFIND is
#        rejected upfront.
PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{bob_access_token}}
Depth: 0

HTTP 403

# 11i — bob CANNOT reach the CalDAV surface. No calendar.
PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{bob_access_token}}
Depth: 0

HTTP 403

# 11j — bob CANNOT reach the CardDAV surface. No personal address book.
PROPFIND {{base_url}}/carddav/
Authorization: Bearer {{bob_access_token}}
Depth: 0

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 12 — /api/users/{id} happy path (Alice → Bob).
#           Visibility rule: they share a grant, so Alice sees
#           Bob's profile (with is_external=true). Bob's username
#           is NULL post PR 16 (externals don't carry a handle).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/users/{{bob_user_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{bob_user_id}}"
jsonpath "$.is_external" == true
jsonpath "$.email" == "bob@externalcompany.com"
jsonpath "$.username" not exists


# ─────────────────────────────────────────────────────────────
# Step 13 — /api/users/{id} 404 anti-enumeration for an
#           unrelated UUID (random Uuid that doesn't exist).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/users/00000000-0000-0000-0000-deadbeefcafe
Authorization: Bearer {{alice_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 14 — Second redemption of the same magic-link token is
#           rejected — single-use is enforced by the SQL UPDATE in
#           magic_link_token_pg_repository::mark_used.
# ─────────────────────────────────────────────────────────────
GET {{magic_url}}

HTTP 410


# ─────────────────────────────────────────────────────────────
# Step 15 — Login-via-email (PR 10). Bob has no password (he was
#           lazily provisioned via the invite flow), so he is
#           magic-link-eligible. He requests a fresh sign-in link.
#           Anti-enumeration: the API always returns 200 with the
#           same body regardless of whether an account exists.
# ─────────────────────────────────────────────────────────────

# 15a — bob requests a sign-in link.
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "bob@externalcompany.com" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"

# 15b — Capture the fresh email; extract the NEW magic-link URL.
#       This is a NULL-resource token (login flow), so redemption
#       will land on /shared-with-me rather than a deep-link.
GET {{base_url}}/api/admin/smtp/test/captured?to=bob@externalcompany.com
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.subject" contains "Sign in"
jsonpath "$.text_body" matches "/magic/v1/[A-Za-z0-9_-]+"
[Captures]
login_magic_url: jsonpath "$.text_body" regex "(https?://[^\\s]+/magic/v1/[A-Za-z0-9_-]+)"

# 15c-i — PR 22: the token is browser-bound. Hitting the
#         redemption URL without the matching cookie shows the
#         cross-browser confirmation page (200 + HTML) rather
#         than redeeming. Audit-logs `magic_link.cross_browser_prompt`.
#         The token is NOT marked used on this branch.
GET {{login_magic_url}}

HTTP 200
[Asserts]
header "content-type" startsWith "text/html"
body contains "different browser"

# 15c-ii — Same token, with `?confirm=1` to acknowledge the
#          cross-browser redemption. PR 22 audit-logs
#          `cross_browser_confirmed=true` on the success line.
#          Lands on /shared-with-me since the token has no
#          resource target (external user, NULL resource_kind).
GET {{login_magic_url}}?confirm=1

HTTP 302
[Asserts]
# SvelteKit route (path-based). Historical value pre-migration was
# `/#/sharedwithme` (legacy vanilla-frontend hash-routing). Kept in
# sync with `redirect_target()` in
# src/interfaces/api/handlers/magic_link_handler.rs.
header "Location" == "/shared-with-me"
[Captures]
bob_relogin_token: cookie "oxicloud_access"

# 15d — Bob's new session works: he can read his incoming grants.
GET {{base_url}}/api/grants/incoming/resources
Authorization: Bearer {{bob_relogin_token}}

HTTP 200

# 15e — Unknown email → same uniform 200 (anti-enumeration). No
#        mail is captured under that address.
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "nobody-here@externalcompany.com" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"

GET {{base_url}}/api/admin/smtp/test/captured?to=nobody-here@externalcompany.com
Authorization: Bearer {{alice_token}}

HTTP 404

# 15f — Email maps to an existing internal user with a password
#        (Alice the admin). The test env has
#        `OXICLOUD_AUTH_POLICIES=permit_magic_link_for_password_users`
#        set globally in `tests/common/server.env`, so the `has_password`
#        eligibility check is bypassed and the link IS minted. Under
#        the STRICT default (policy absent), the eligibility ladder
#        would refuse with `reason="has_password"` and no mail would
#        ship — that path is covered by a Rust unit test on
#        `magic_link_eligibility()` because it needs the opposite env
#        which we can't hot-swap mid-run.
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "{{email}}" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"

# With the permit policy, a mail WAS captured. Rate-limit slot burned
# either way (increment fires before eligibility) — Step 16's math
# still holds.
GET {{base_url}}/api/admin/smtp/test/captured?to={{email}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.to" == "{{email}}"


# ─────────────────────────────────────────────────────────────
# Step 16 — Rate-limit caps (PR 12). Test-only thresholds come
#           from tests/common/server.env:
#             OXICLOUD_MAGIC_LINK_INVITE_PER_CALLER_PER_HOUR=3
#             OXICLOUD_MAGIC_LINK_SEND_PER_EMAIL_PER_HOUR=2
#           Alice already burned 2 invite slots earlier (bob's
#           folder + ext-share-2) and 1 send slot in Step 15a.
# ─────────────────────────────────────────────────────────────

# 16a — Alice's 3rd email-invite (3/3) succeeds — right at the
#       cap. Fresh email so resolve_or_create_recipient mints a
#       new external user we'll clean up below.
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "email", "email": "ratelimit-test-1@externalcompany.com" },
  "resource": { "type": "folder", "id": "{{ext_folder_id}}" },
  "role":     "viewer"
}

HTTP 201
[Captures]
rl_user_1_id: jsonpath "$.grants[0].subject.id"

# 16b — 4th invite (4/3) is rejected with 429 + Retry-After. The
#       cap is visible because Alice is authenticated and her own
#       rate-limit state leaks nothing about other accounts.
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "email", "email": "ratelimit-test-2@externalcompany.com" },
  "resource": { "type": "folder", "id": "{{ext_folder_id}}" },
  "role":     "viewer"
}

HTTP 429
[Asserts]
header "retry-after" exists
jsonpath "$.retry_after_secs" >= 1

# 16c — Anonymous /magic-link/send to bob (2/2 — at cap). Returns
#       the same uniform 200 a successful issuance would; the
#       audit log distinguishes the two.
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "bob@externalcompany.com" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"

# 16d — 3rd anonymous send to bob (3/2 — over cap). Anti-enumeration:
#       must NOT return 429, must NOT change the response shape.
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "bob@externalcompany.com" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"

# 16e — Authenticated callers bypass both anti-flood caps. Alice
#       resends to bob with her Bearer token; the per-email and
#       per-IP counters are not consulted (a logged-in user
#       resending should never be throttled). Still returns 200.
POST {{base_url}}/api/auth/magic-link/send
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "email": "bob@externalcompany.com" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"


# ─────────────────────────────────────────────────────────────
# Step 12 — Cleanup. Alice trashes the two test folders and
#           deletes bob + the two rate-limit-test externals via
#           the admin API so the suite's storage-check sweep at
#           run.sh end sees a clean DB.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{ext_folder_id_2}}
Authorization: Bearer {{alice_token}}

HTTP 204

DELETE {{base_url}}/api/folders/{{ext_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

DELETE {{base_url}}/api/admin/users/{{bob_user_id}}
Authorization: Bearer {{alice_token}}

HTTP *

DELETE {{base_url}}/api/admin/users/{{rl_user_1_id}}
Authorization: Bearer {{alice_token}}

HTTP *
