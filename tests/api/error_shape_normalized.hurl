# =============================================================
# OxiCloud — error-response wire shape (post-normalization)
# =============================================================
# Pins the uniform `ApiErrorBody` body shape every REST handler
# returns on 4xx/5xx after `chore/wire-apierrorbody-responses`:
#
#   {
#     "status":     "<numeric http as string>",
#     "message":    "<human message>",
#     "error":      "<same string as message, legacy field>",
#     "error_type": "<PascalCase discriminator>"
#   }
#
# Also pins the sibling `OAuthErrorResponse` shape used by federated
# endpoints (OIDC Back-Channel Logout) — RFC 6749 §5.2 requires
# exactly `{error, error_description}` with no sibling fields some
# IdPs would reject.
#
# Each row below covers ONE class of regression this PR could
# reintroduce. Deleting any assertion silently restores the drift
# — the shapes won't visibly change on the wire until an SDK consumer
# notices.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin + a plain user for anti-enum / RBAC pins.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Row 1 — BAD_REQUEST path
#
# Favorites rejects an unknown `item_type` path parameter with
# `AppError::bad_request(...)`. Simpler trigger than the admin-job
# surface (whose registered-job set depends on which startup jobs the
# test env wired) and the same code path through
# `AppError::IntoResponse` + the handler-emitted `BadRequest` literal.
#
# The pin covers:
#   * The four required ApiErrorBody fields all appear.
#   * `.error` and `.message` carry the SAME string (back-compat
#     duplication). A regression that dropped one or stopped
#     mirroring would trip here.
#   * `error_type` is PascalCase `BadRequest` from the handler-
#     emitted literal (NOT the generic `InvalidInput` the
#     ErrorKind::InvalidInput → wire_name path would produce).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/favorites/not_a_valid_item_type/00000000-0000-0000-0000-000000000000
Authorization: Bearer {{admin_token}}

HTTP 400
[Captures]
# Back-compat duplication: both field names must carry the SAME string.
# The invariant is enforced by `AppError::IntoResponse` at
# `interfaces/errors.rs:239` (one `client_message` bound into both
# `error` and `message` fields), so capturing one and asserting the
# other matches catches a drift. Phase 2c of
# docs/plan/normalize-errors.md will drop `.error` after a deprecation
# window; this capture-based pin is also what the Phase-2c PR will
# search-and-replace to find every consumer.
dup_message: jsonpath "$.message"
[Asserts]
jsonpath "$.status"     == "400 Bad Request"
jsonpath "$.message"    contains "Item type must be"
jsonpath "$.error"      == "{{dup_message}}"
jsonpath "$.error_type" == "BadRequest"


# ─────────────────────────────────────────────────────────────
# Row 2 — NOT_FOUND with folded context
#
# Previously the handler emitted `{"error": "job not registered",
# "name": "<name>"}` — multi-key payload. Normalization folded the
# `name` into the message string (pending Phase 2's `params`
# carrier). Pin that:
#   * The job name DOES appear in the message (operators still see
#     which job they tried to trigger).
#   * `error_type: NotFound` (ErrorKind::NotFound → wire_name).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/nonexistent-job-name/trigger
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ }

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"
jsonpath "$.message"    contains "nonexistent-job-name"
jsonpath "$.status"     == "404 Not Found"


# ─────────────────────────────────────────────────────────────
# Row 3 — RBAC path (403 FORBIDDEN) stays shape-stable
#
# A plain user hitting the admin trigger endpoint — the middleware
# layer rejects before the handler. This exercises the
# `From<DomainError> for AppError` fallback (not a direct
# `AppError::forbidden` call). Confirms the shape is uniform
# whichever construction path produced the error.
# ─────────────────────────────────────────────────────────────
# Register a plain user — anti-enum uniform 200 whether created or not.
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "err_shape_bob",
  "email":    "err_shape_bob@example.com",
  "password": "ErrShapeBob1!"
}

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "err_shape_bob", "password": "ErrShapeBob1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


# Bob is not an admin → the admin middleware returns 403 before the
# handler runs.
POST {{base_url}}/api/admin/jobs/thumbnails_derive_from_originals/trigger
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ }

HTTP 403
[Asserts]
jsonpath "$.error_type" exists
jsonpath "$.message"    exists
jsonpath "$.error"      exists
jsonpath "$.status"     == "403 Forbidden"


# ─────────────────────────────────────────────────────────────
# Row 4 — Recent: bad item_type → normalized bad_request
#
# `recent_handler.rs:50` — the first drift site I migrated manually.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/recent/not_a_valid_item_type/00000000-0000-0000-0000-000000000000
Authorization: Bearer {{bob_token}}

HTTP 400
[Asserts]
jsonpath "$.error_type" == "BadRequest"
jsonpath "$.message"    contains "Item type must be"


# ─────────────────────────────────────────────────────────────
# Row 6 — UNAUTHORIZED (invalid bearer)
#
# Hit a protected endpoint with a deliberately invalid Bearer so the
# auth middleware rejects via `AuthError::InvalidToken` → `AppError::
# unauthorized`. Pins that the shape stays uniform on the pre-handler
# auth middleware path too — pre-normalization `AuthError::IntoResponse`
# emitted a bare `{"error": "..."}` body; the fix funnels it through
# AppError so middleware rejections also carry the full ApiErrorBody.
#
# NOTE: a bare request (no Authorization header) would NOT 401 here —
# Hurl carries the session cookie set by the admin login above, and
# cookie-auth is a valid credential path. Explicit garbage Bearer
# wins over the cookie because Bearer is tried first.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer deliberately.invalid.token.for.pin

HTTP 401
[Asserts]
jsonpath "$.error_type" exists
jsonpath "$.status"     == "401 Unauthorized"
jsonpath "$.error"      exists
jsonpath "$.message"    exists


# ─────────────────────────────────────────────────────────────
# Row 7 — 404 NotFound anti-enum
#
# GET on an unknown user id returns 404 for both "user doesn't exist"
# and "caller has no visibility" (anti-enum). The shape is uniform
# whichever path the service took.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/users/00000000-0000-0000-0000-000000000000
Authorization: Bearer {{bob_token}}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"
jsonpath "$.status"     == "404 Not Found"


# ─────────────────────────────────────────────────────────────
# Row 8 — Field duplication invariant across every row above
#
# Repeated on a cheap-to-trigger 404 so the per-row cost of
# re-asserting the duplication guarantee stays bounded. If Phase 2c
# drops `.error`, THIS row goes along with every other row's
# `$.error exists` assertion in one coordinated change — searching
# the repo for `jsonpath "$.error"` surfaces every consumer.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/00000000-0000-0000-0000-000000000000
Authorization: Bearer {{bob_token}}

HTTP 404
[Captures]
dup_message_r8: jsonpath "$.message"
[Asserts]
jsonpath "$.error_type" == "NotFound"
jsonpath "$.error"      == "{{dup_message_r8}}"


# ─────────────────────────────────────────────────────────────
# Row 9 — OIDC Back-Channel Logout → OAuthErrorResponse body
#
# This endpoint intentionally does NOT emit ApiErrorBody: it talks
# to federated IdPs that parse RFC 6749 §5.2 (`{error,
# error_description}`), and some reject responses carrying sibling
# fields. The handler's two error paths (400 bad token, 503 OIDC
# off) stay on the OAuth shape; the utoipa::path advertises
# `body = OAuthErrorResponse` instead of ApiErrorBody.
#
# The pin asserts:
#   * `error` is a lowercase_snake OAuth error code.
#   * `error_description` carries the human breadcrumb.
#   * No `status` / `message` / `error_type` siblings — those are
#     ApiErrorBody fields that must NOT leak into the OAuth shape.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/oidc/backchannel-logout
Content-Type: application/x-www-form-urlencoded
`logout_token=not.a.valid.jwt`

# Either 400 (OIDC configured, token invalid) or 503 (OIDC off on
# this test deployment). Both carry the OAuth shape; the pin
# tolerates whichever the environment resolves to.
HTTP *
[Asserts]
# OAuth 2.0 error code — lowercase_snake, from a known enumeration.
jsonpath "$.error" matches "^(invalid_request|authentication_not_configured)$"
jsonpath "$.error_description" exists
# ApiErrorBody fields must NOT appear here.
jsonpath "$.status"     not exists
jsonpath "$.message"    not exists
jsonpath "$.error_type" not exists
