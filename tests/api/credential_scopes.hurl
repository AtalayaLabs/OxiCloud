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
