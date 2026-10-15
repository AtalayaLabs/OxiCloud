# =============================================================
# OxiCloud — DTO shape regression guards (Phase 3 enum tightening)
# =============================================================
# Pins the three request-shape tightenings `oasdiff breaking` flagged
# as REAL breaking changes (as opposed to the 79 "server already
# emitted exactly these values" cosmetic flags):
#
#   1. PUT /api/admin/settings/storage     — `backend` was `String`,
#      now `StorageBackendKind` enum.
#   2. PUT /api/admin/users/{id}/role      — `role` was `String`,
#      now `UserRoleDto` enum.
#   3. PATCH /api/auth/me/profile          — `ui_preferences` was
#      `any`, now `object | null`.
#
# Each case sends a value that was ACCEPTED by the pre-Phase-3 DTO
# but is REJECTED by the post-Phase-3 schema. The expected status is
# 422 (Unprocessable Entity — serde deser failure at axum's extractor
# layer), replacing the pre-change 400 the service used to return for
# the same semantic rejection.
#
# These are belt-and-braces: no live FE call site today sends values
# that would trip these guards, but a future script hitting the endpoint
# directly could. Pinning the behaviour keeps the regression visible
# the next time someone edits one of these three fields.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — owner login (the /api/setup account) so we can hit admin.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
owner_token: jsonpath "$.access_token"
owner_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Guard 1 — PUT /admin/settings/storage with `backend` outside
#           the enum set → 422. Pre-Phase-3 this would have been
#           accepted as `String` and resolved later by the service.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/settings/storage
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "backend": "mystery-backend",
  "s3_endpoint_url": null,
  "s3_bucket": null,
  "s3_region": null,
  "s3_access_key": null,
  "s3_secret_key": null,
  "s3_force_path_style": null
}

HTTP 422


# ─────────────────────────────────────────────────────────────
# Guard 2 — PUT /admin/users/{id}/role with a non-enum role →
#           422. Pre-Phase-3 the service would have returned 400
#           `"Unknown role"`; this fires one step earlier (axum
#           extractor) with 422.
# ─────────────────────────────────────────────────────────────
# A target user — anyone that isn't the caller. The owner can't
# change its own role, so we need a second account first.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "username": "guard-target",
  "password": "GuardPass1!",
  "email":    "guard-target@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
guard_target_id: jsonpath "$.user.id"


PUT {{base_url}}/api/admin/users/{{guard_target_id}}/role
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "role": "superadmin" }

HTTP 422


# ─────────────────────────────────────────────────────────────
# Guard 3 — PATCH /auth/me/profile with `ui_preferences` as a
#           non-object → 400 from the handler's "must be object"
#           check.
#
#           Note: oasdiff flagged the schema change `any → object | null`
#           as a breaking change, but on inspection the Rust type is
#           still `Option<serde_json::Value>` (anything-goes at the
#           serde layer). The `#[schema(value_type = Option<Object>)]`
#           hint tightens the OpenAPI documentation; the handler itself
#           has always rejected non-objects with 400 `InvalidInput`.
#           So this guard pins existing behaviour rather than a new
#           tightening — call-sites that worked before still work.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "ui_preferences": [1, 2, 3] }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Cleanup — remove the guard target so this file re-runs clean.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{guard_target_id}}
Authorization: Bearer {{owner_token}}

HTTP 200
