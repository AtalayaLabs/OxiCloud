# =============================================================
# OxiCloud — PUT /api/auth/me/image: wire shape + lifecycle
# =============================================================
# The avatar endpoint was the one profile.ts wrapper that had no
# utoipa annotation until `6417b482`; before that there was no
# generated SDK function for it and no Hurl pin at all. The
# wrapper now routes through the SDK + the Vitest layer carries
# two unit pins (null-passthrough + error-shape).
#
# This file pins the WIRE SHAPE the SDK generation reads from
# `openapi.json`: path, method, request body shape, response
# status, round-trip through a `GET /me` to verify the server
# actually persisted the change. A regression that collapses the
# three states (clear / set / invalid) into the same response
# would be caught here, not at any lower layer.
#
# COVERED:
#   • PUT with `{ "image": null }`     → 200, GET /me shows no image
#   • PUT with a tiny data URI         → 200, GET /me reflects it
#   • PUT with `{ "image": null }`     → 200, teardown back to clear
#   • Not currently covered: 400 on an invalid data URI — the
#     backend's `update_user_image` returns a DomainError which
#     surfaces as 400, but the exact validation shape (max bytes,
#     MIME allowlist, URL-scheme filter) lives in the service
#     layer and warrants its own file if we add it; keeping this
#     file focused on the happy wire-shape round-trip.
#
# All operations run as the shared admin — no secondary user
# needed, and the final teardown leaves the admin's avatar in
# whatever state the suite started in (cleared, which is also
# the fresh-DB default).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login. Every subsequent request is bearer-auth
#          as this user.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Clear the image first so the subsequent "set" step
#          has a known-null starting state. `{ "image": null }`
#          is the canonical shape for "no image" per the FE
#          wrapper + Pin 5 in `profile.test.ts`.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/auth/me/image
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "image": null }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 3 — Confirm `GET /me` reports NO stored image after a
#          null-clear. The field name on the wire is `image`
#          (not `image_url` / `avatar` — pinned per `PublicUserDto`
#          at `user_dto.rs:145`). The field has NO
#          `skip_serializing_if = "Option::is_none"`, so an
#          empty state surfaces as JSON `null` on the wire, not
#          an absent key.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# Pin on JSON null — the exact shape the SPA's "no avatar"
# branch keys on (falsy check in Svelte's avatar snippet).
jsonpath "$.full.user.image" == null


# ─────────────────────────────────────────────────────────────
# Step 4 — Store a 1×1 transparent PNG as a data URI. This is
#          the shape the Svelte avatar upload picker produces
#          (via `resizeImageToDataUrl`), so a wire-shape test
#          that accepts this round-trip proves the FE's
#          preferred encoding is viable server-side.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/auth/me/image
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "image": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 5 — GET /me now reflects the stored image. Pin the exact
#          data URI we just PUT so a regression that silently
#          stripped or mutated the value would surface here.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.full.user.image" == "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII="


# ─────────────────────────────────────────────────────────────
# Teardown — clear the image so the admin's row returns to the
#            default state. Per `feedback_hurl_teardown_shared_db`
#            memory: Hurl files share DB state, so every mutation
#            needs an explicit undo.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/auth/me/image
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "image": null }

HTTP 200
