# =============================================================
# OxiCloud — credential scopes: the SCOPE axis, end to end
# =============================================================
# The third axis of the authorization engine. Gate asks what KIND
# of caller this is, Grant asks about THIS resource, and Scope
# asks what the CREDENTIAL was minted to reach — independent of
# who holds it. An admin's app password carries the admin role,
# because a role is read from the user row; only the scope axis
# stops it administering the instance.
#
# Everything here runs as the ADMIN on purpose. A pin that used a
# plain user could not tell "refused because the scope is missing"
# from "refused because the role is too low" — the two denials look
# identical from outside. With an admin, role is satisfied by
# construction, so a 403 can only be the scope axis.
#
# Four properties, and each is load-bearing:
#
#   1. the minted set is NARROWED to the credential kind's ceiling
#      and the narrowing is REPORTED (RFC 6749 §5.1);
#   2. a narrow credential is REFUSED on an area it lacks;
#   3. the same credential SUCCEEDS on the area it holds — a gate
#      that refuses everyone passes a refusal-only pin;
#   4. the pre-migration vocabulary is REJECTED, not ignored.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — The CEILING, and that it is reported.
#
# `security` is asked for and must NOT come back. The app-password
# ceiling grants files + calendar + contacts and nothing else, so
# an app password cannot mint or revoke another one — not because
# anything notices it is an app password, but because its set lacks
# the area those routes require.
#
# The response value is the pin, not just the stored row: RFC 6749
# §3.3 lets a server narrow a requested scope, and §5.1 REQUIRES
# the response to say so when it differs from the request. This
# endpoint used to echo `request.scopes` verbatim, which would have
# told a client it held `security` while it held three areas — a
# difference it would discover only as a 403 much later.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "credential_scopes narrowed", "scopes": "oxicloud:files:rw,oxicloud:security:rw" }

HTTP 200
[Captures]
narrowed_id: jsonpath "$.id"
[Asserts]
# Narrowed, and the narrowing is visible to the client.
jsonpath "$.scopes" == "oxicloud:files:rw"
jsonpath "$.scopes" not contains "security"


# ─────────────────────────────────────────────────────────────
# Step 3 — A credential holding ONLY `files:rw`.
#
# Captured as a password for Basic auth below. `username` is what
# the Basic-auth doors resolve on.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "credential_scopes files only", "scopes": "oxicloud:files:rw" }

HTTP 200
[Captures]
files_only_id: jsonpath "$.id"
files_only_pw: jsonpath "$.password"
files_only_user: jsonpath "$.username"
[Asserts]
jsonpath "$.scopes" == "oxicloud:files:rw"


# ─────────────────────────────────────────────────────────────
# Step 4 — REFUSED on an area the credential lacks.
#
# `/api/auth/app-passwords` declares the `security` area at its
# route, which this credential does not hold. The caller is an
# ADMIN, so the role axis is satisfied and 403 can only be scope.
#
# This is the property that makes the ceiling meaningful: without
# it, an app password could mint a SECOND app password, and the
# ceiling would be one API call away from irrelevant.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/app-passwords
[BasicAuth]
{{files_only_user}}: {{files_only_pw}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 5 — ALLOWED on the area it holds.
#
# Without this pin, step 4 would pass against a gate that refuses
# EVERYTHING — a broken scope axis and a working one are
# indistinguishable from a single refusal.
#
# `/api/folders` rather than `/api/files`: both declare `files:ro`,
# but `GET /api/files` answers 400 when no `folder_id` resolves,
# which is a request-shape failure that would mask the very thing
# this leg exists to prove. The allowed leg must fail for ONE
# reason only — scope — so it has to be an endpoint that is a clean
# 200 with no parameters.
#
# `files:rw` satisfies `files:ro` because the set keeps
# `writable ⊆ readable`: holding write implies holding read.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
[BasicAuth]
{{files_only_user}}: {{files_only_pw}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 6 — The PRE-MIGRATION vocabulary is REJECTED.
#
# `webdav` was a valid scope before the area rename. It is now
# refused rather than ignored, which is a deliberate departure from
# RFC 6749 §3.3's permission to skip unrecognised scope values: a
# partial parse that resolves only the tokens it recognises would
# leave a client believing it asked for something it did not get.
#
# The OIDC collision is the sharper case — a client sending the
# textbook `scope=openid profile email` must fail cleanly, because
# `profile` is reserved by OIDC and would otherwise match an
# OxiCloud area with the OPPOSITE meaning: read-only identity
# claims there, write access to user data here. Hence `oxicloud:`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "credential_scopes legacy vocabulary", "scopes": "webdav,caldav" }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 7 — An unprefixed OIDC scope is refused the same way.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "credential_scopes oidc collision", "scopes": "openid profile email" }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 8 — A RESERVED area is refused distinctly from an unknown
#          one, so a client learns it asked for something that
#          does not exist yet rather than something misspelled.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "credential_scopes reserved area", "scopes": "oxicloud:mail:rw" }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 9 — A credential holding ONLY `files:ro`.
#
# One area, read-only, so the next six requests cover both
# dimensions of the axis: WHICH area, and WHICH access within it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "credential_scopes files read-only", "scopes": "oxicloud:files:ro" }

HTTP 200
[Captures]
ro_id: jsonpath "$.id"
ro_pw: jsonpath "$.password"
ro_user: jsonpath "$.username"
[Asserts]
# `:ro` survives the round trip. If the mint widened it to `:rw` — the
# default for an UNQUALIFIED area — every refusal below would become a
# 200 and the access dimension would be untested.
jsonpath "$.scopes" == "oxicloud:files:ro"


# ─────────────────────────────────────────────────────────────
# Step 10 — READ on the area it holds → allowed.
#
# The control for steps 11-15. Without it, six refusals prove only
# that the credential is broken, not that the axis is working.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
[BasicAuth]
{{ro_user}}: {{ro_pw}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 11 — WRITE on the SAME area → refused.
#
# The access dimension, and the one most easily lost: `files:ro` and
# `files:rw` differ by one bit, `satisfies` is a superset test over
# two masks, and a regression that compared only the readable mask
# would pass every other pin in this file. `POST /api/folders`
# declares `files:rw`.
#
# `scope_by_verb` is what makes this work without a second
# declaration: the same `Condition::scope_by_verb(Scope::Files)` on
# the prefix resolves to ReadOnly for GET and ReadWrite for POST, so
# steps 10 and 11 exercise ONE declaration from both sides.
#
# No body: the gate runs before the handler, so a scope refusal
# cannot depend on the payload. If the gate ever let this through,
# the handler's own 400 would fail this assertion too — which is the
# correct outcome, not a false green.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
[BasicAuth]
{{ro_user}}: {{ro_pw}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 12 — CALENDAR withheld → refused.
#
# `GET /api/calendars` declares `calendar:ro`. Holding `files` grants
# nothing here: areas are independent, not a hierarchy.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/calendars
[BasicAuth]
{{ro_user}}: {{ro_pw}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 13 — CONTACTS withheld → refused.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books
[BasicAuth]
{{ro_user}}: {{ro_pw}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 14 — ADMIN withheld → refused, for an ADMIN.
#
# The sharpest pin in the file. `GET /api/admin/users` requires
# `MinRole::Admin` AND `admin:ro`, and this credential belongs to the
# admin — so the ROLE axis is satisfied and the 403 can only be
# scope.
#
# That is the whole mechanism by which a non-interactive credential
# cannot administer the instance: a role is read from the USER row,
# so an app password inherits the admin role and nothing about it
# looks less privileged. Only the ceiling, which never grants
# `admin` to an app password whatever it asks for, stands in the way.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/users
[BasicAuth]
{{ro_user}}: {{ro_pw}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 15 — SECURITY withheld, on the WRITE side → refused.
#
# Step 4 pinned the read (`GET`, `security:ro`); this is `POST`,
# `security:rw`. Both matter: the read leaks which credentials exist,
# and the write is the escalation — an app password minting a wider
# app password, which would make every ceiling in this file one API
# call from irrelevant.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
[BasicAuth]
{{ro_user}}: {{ro_pw}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 16 — Asking for `admin` leaves the rest intact.
#
# The ceiling REMOVES the area it withholds; it does not reject the
# request or empty the set. RFC 6749 §3.3 prescribes exactly this
# ("the authorization server MAY fully or partially ignore the scope
# requested"), and it is the only behaviour that lets a client ask
# for a superset without knowing each credential kind's ceiling.
#
# So `files:rw + admin:rw` must come back as `files:rw` — not as an
# error, and not as an empty set. The negative assertion is the pin:
# a ceiling implemented as a DENYLIST would keep `admin` the moment
# someone added an area and forgot to list it, and this is what
# would catch that.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "credential_scopes admin dropped", "scopes": "oxicloud:files:rw,oxicloud:admin:rw" }

HTTP 200
[Captures]
dropped_id: jsonpath "$.id"
[Asserts]
jsonpath "$.scopes" == "oxicloud:files:rw"
jsonpath "$.scopes" not contains "admin"


# ═════════════════════════════════════════════════════════════
# CHAIN B — the DEVICE GRANT, because the scope reaches the gate by
#           a different road.
#
# Everything above authenticates with an app password, whose scopes
# come from the `auth.app_passwords.scopes` ROW — an app password is
# not a JWT, so there is no claim to read. The device grant mints a
# real access token carrying the RFC 6749 §3.3 `scope` CLAIM, and
# `CurrentCaller` is built from that instead.
#
# Two sources, one field, and nothing above would notice if the claim
# side were broken: a narrow device token would simply carry every
# scope, because an ABSENT claim grandfathers to FULL. That default
# is deliberate — tokens predating the claim must keep working — and
# it is exactly why the claim path needs its own pins. The failure
# mode is silent and fails OPEN.
#
# The device grant also reaches areas an app password cannot: its
# ceiling admits `account`, because the verify screen shows the user
# what is being granted and an app-password dialog does not.
# ═════════════════════════════════════════════════════════════

# ─────────────────────────────────────────────────────────────
# Step 17 — A device code asking for `files:ro` only.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/authorize
Content-Type: application/json
{
  "client_name": "credential_scopes narrow device",
  "scope":       "oxicloud:files:ro"
}

HTTP 200
[Captures]
dev_code: jsonpath "$.device_code"
dev_user_code: jsonpath "$.user_code"


# ─────────────────────────────────────────────────────────────
# Step 18 — The consent screen shows what will ACTUALLY be granted.
#
# Narrowing happens at the STORE, and the device flow stores at
# `authorize` — BEFORE the human approves. So this screen reads the
# capped set, and consent is truthful by construction rather than by
# the UI remembering to cap it.
#
# Before the ceiling moved to mint time, a client could request
# `security` here, the screen would display it, the user would
# approve it, and the credential would hold it.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/device/verify?code={{dev_user_code}}

HTTP 200
[Asserts]
jsonpath "$.scopes" == "oxicloud:files:ro"


# ─────────────────────────────────────────────────────────────
# Step 19 — The human approves, as the admin.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/verify
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "user_code": "{{dev_user_code}}", "action": "approve" }

HTTP 200
[Asserts]
jsonpath "$.status" == "approved"


# ─────────────────────────────────────────────────────────────
# Step 20 — Poll for the token; the response REPORTS the scope.
#
# RFC 6749 §5.1 requires `scope` in the response whenever it differs
# from the request. It is emitted unconditionally here, so a client
# never has to infer what it was given.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/token
Content-Type: application/json
{
  "device_code": "{{dev_code}}",
  "grant_type":  "urn:ietf:params:oauth:grant-type:device_code"
}

HTTP 200
[Captures]
dev_token: jsonpath "$.access_token"
[Asserts]
jsonpath "$.scope" == "oxicloud:files:ro"


# ─────────────────────────────────────────────────────────────
# Step 21 — The CLAIM is enforced: read allowed.
#
# This is the control. A failure here means the narrow token cannot
# do anything at all, which would make step 22's refusal meaningless.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{dev_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 22 — The CLAIM is enforced: write refused.
#
# The pin that an absent-claim regression would break. If the mint
# stopped writing the claim, or the middleware stopped reading it,
# this token would carry EVERY scope and answer 2xx — a fail-open
# that no other assertion in this file can see, because every other
# caller here is an app password reading its scopes from a row.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{dev_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 23 — And an area the token never asked for.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/calendars
Authorization: Bearer {{dev_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Teardown — revoke both credentials. The DB is shared across
# scenarios, so a leaked app password is a live credential another
# run could authenticate with.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/app-passwords/{{narrowed_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


DELETE {{base_url}}/api/auth/app-passwords/{{files_only_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


DELETE {{base_url}}/api/auth/app-passwords/{{ro_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


DELETE {{base_url}}/api/auth/app-passwords/{{dropped_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
