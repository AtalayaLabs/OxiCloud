# =============================================================
# OxiCloud — external → internal account upgrade
# =============================================================
# Covers the `POST /api/auth/upgrade-to-internal` endpoint end-to-end:
# admin-creates an external user, external user logs in, calls upgrade,
# lands on an internal account with a personal drive.
#
# Cross-cutting invariants pinned:
#   * `is_external` flip is persisted (not just returned).
#   * `PersonalDriveLifecycleHook.on_upgraded_to_internal` runs — a new
#     default personal drive appears via `/api/drives`.
#   * Idempotency: a second upgrade returns 409 `AlreadyInternal`.
#   * Domain gate mirrors register: an off-allowlist email is refused
#     with 403 `RegistrationDomainNotAllowed`.
#   * `OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS` is
#     `example.com,example.test` in tests/common/server.env.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login (needed to admin-create users + reach
#          the delete endpoint for cleanup at the end).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Admin creates an external user `bob-upgrade` with a
#          temp password so this test can log in as him without
#          going through the magic-link invitation flow (that
#          path is exercised elsewhere in external_users.hurl).
#          The temp password is real — admin_create_user hashes
#          it even for externals — but bob's `is_external=true`
#          means he has no drive yet.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "username":    "bob-upgrade",
  "email":       "bob-upgrade@example.com",
  "password":    "TempExtPass1!",
  "role":        "user",
  "is_external": true
}

HTTP 201
[Captures]
bob_user_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Bob logs in with the temp password. Baseline: he can
#          authenticate. Assert `is_external: true` on the /me
#          response so a later /me post-upgrade proves the flip.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "bob-upgrade", "password": "TempExtPass1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"

GET {{base_url}}/api/auth/me
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.full.user.is_external" == true
jsonpath "$.full.storage_quota_bytes" == 0


# ─────────────────────────────────────────────────────────────
# Step 4 — Bob calls upgrade with a NEW password. Response is
#          the updated UserDto (is_external=false, quota set).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/upgrade-to-internal
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "password": "NewInternalPass1!" }

HTTP 200
[Asserts]
jsonpath "$.full.user.is_external" == false
jsonpath "$.full.storage_quota_bytes" > 0


# ─────────────────────────────────────────────────────────────
# Step 5 — /me confirms the flip persisted (not just returned).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.full.user.is_external" == false
jsonpath "$.full.storage_quota_bytes" > 0


# ─────────────────────────────────────────────────────────────
# Step 6 — Bob's NEW password works. Proves the password hash
#          was persisted (not just held in memory) and the old
#          temp password no longer authenticates. Fetches a
#          fresh token so the rest of the test uses a session
#          whose JWT claims already reflect the upgrade.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "bob-upgrade", "password": "NewInternalPass1!" }

HTTP 200
[Captures]
bob_token_after: jsonpath "$.access_token"

# Old password no longer works.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "bob-upgrade", "password": "TempExtPass1!" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 7 — Drive provisioning. Bob's default personal drive
#          shows up on /api/drives. Before upgrade externals
#          have none; after upgrade the lifecycle hook created
#          exactly one via the atomic CTE.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{bob_token_after}}

HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[0].kind" == "personal"


# ─────────────────────────────────────────────────────────────
# Step 8 — Idempotency: a second upgrade returns 409
#          `AlreadyInternal`. The service pre-checks
#          `is_external`; the entity's `promote_to_internal`
#          has a matching guard.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/upgrade-to-internal
Authorization: Bearer {{bob_token_after}}
Content-Type: application/json
{ "password": "AnotherPass1!" }

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyInternal"


# ─────────────────────────────────────────────────────────────
# Step 9 — Domain gate. Create an external user on a domain
#          OUTSIDE the allowlist, log in, attempt upgrade, get
#          403 `RegistrationDomainNotAllowed`. Rationale
#          documented in the handler: invitations must not
#          become a bypass of the operator's registration
#          policy.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "username":    "carol-offdomain",
  "email":       "carol@offdomain.invalid",
  "password":    "TempExtPass1!",
  "role":        "user",
  "is_external": true
}

HTTP 201
[Captures]
carol_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "carol-offdomain", "password": "TempExtPass1!" }

HTTP 200
[Captures]
carol_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/upgrade-to-internal
Authorization: Bearer {{carol_token}}
Content-Type: application/json
{ "password": "NewInternalPass1!" }

HTTP 403
[Asserts]
jsonpath "$.error_type" == "RegistrationDomainNotAllowed"

# Carol is still external — the refusal didn't half-flip anything.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{carol_token}}

HTTP 200
[Asserts]
jsonpath "$.full.user.is_external" == true


# ─────────────────────────────────────────────────────────────
# Cleanup — admin deletes both test users.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{bob_user_id}}
Authorization: Bearer {{alice_token}}

HTTP *

DELETE {{base_url}}/api/admin/users/{{carol_user_id}}
Authorization: Bearer {{alice_token}}

HTTP *
