# =============================================================
# OxiCloud — External file mounts: admin CRUD + authz gate
# =============================================================
# Black-box HTTP coverage for the admin mount endpoints
# (`admin_external_mounts.rs`), which previously had no
# handler- or protocol-level tests — only the repository,
# factory and provider layers underneath were covered.
#
# Endpoints under test (all `/api/admin/*`, admin-gated by the
# middleware layer):
#   * GET    /api/admin/external-mounts        — list
#   * POST   /api/admin/external-mounts        — create
#   * DELETE /api/admin/external-mounts/{id}   — delete
#
# This test pins:
#   * Input validation: missing destination drive → 422;
#     empty name → 400; non-existent host
#     path (invalid provider config) → 400.
#   * Create returns 201 with the full mount view; the mount is
#     then visible in the list with the config echoed back.
#   * Delete returns 204 and removes it from the list; a second
#     delete of the same id → 404 (anti-enum / idempotency).
#   * Non-admin (bob) is denied on list / create / delete → 403
#     (the `/api/admin/*` middleware gate; the handler never runs).
#
# Requires OXICLOUD_ENABLE_EXTERNAL_MOUNTS=true (server.env).
# `/tmp` is used as the host path — always present on the CI
# runner and validated by the local_fs provider factory.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin login + a non-admin (bob) for the deny checks.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# Select the admin's default Personal drive as the destination.
GET {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
drive_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$[0].kind" == "personal"


# Anti-enum registration (200 whether or not it already exists).
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "mount_bob",
  "email":    "mount_bob@example.com",
  "password": "MountBobPassword1!"
}

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mount_bob", "password": "MountBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


# A destination drive is required; reject the old request shape.
POST {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "missing-drive", "host_path": "/tmp" }

HTTP 422


# ─────────────────────────────────────────────────────────────
# Step 1 — Validation: empty name is rejected before any
#          filesystem or DB work happens.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "", "host_path": "/tmp", "drive_id": "{{drive_id}}" }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 2 — Validation: a host path that does not exist fails the
#          provider-config build → 400 (never a 500).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "bad-mount", "host_path": "/no/such/path/oxicloud-does-not-exist", "drive_id": "{{drive_id}}" }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 3 — Create a valid read-only mount → 201 with the full
#          mount view. Capture the id for the list/delete steps.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "hurl-mount-test", "host_path": "/tmp", "read_only": true, "drive_id": "{{drive_id}}" }

HTTP 201
[Captures]
mount_id: jsonpath "$.mount_folder_id"
[Asserts]
jsonpath "$.mount_folder_id" isString
jsonpath "$.name"            == "hurl-mount-test"
jsonpath "$.kind"            == "local_fs"
jsonpath "$.read_only"       == true
jsonpath "$.owner_id"        isString
jsonpath "$.drive_id"        == "{{drive_id}}"
jsonpath "$.mount_path"      isString
jsonpath "$.config.path"     == "/tmp"


# ─────────────────────────────────────────────────────────────
# Step 4 — The mount is now listed, with the config echoed back.
#          This test is the only mount-creator in the suite and
#          runs against a fresh DB, so the list holds exactly the
#          one mount just created.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$"                    isCollection
jsonpath "$"                    count == 1
jsonpath "$[0].mount_folder_id" == "{{mount_id}}"
jsonpath "$[0].name"            == "hurl-mount-test"
jsonpath "$[0].read_only"       == true
jsonpath "$[0].kind"            == "local_fs"
jsonpath "$[0].drive_id"        == "{{drive_id}}"
jsonpath "$[0].config.path"     == "/tmp"


# ─────────────────────────────────────────────────────────────
# Step 5 — Non-admin (bob) is denied on every verb. The
#          `/api/admin/*` middleware layer returns 403 before
#          the handler runs — no hand-rolled check in the handler.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{bob_token}}

HTTP 403


POST {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{ "name": "bob-mount", "host_path": "/tmp", "drive_id": "{{drive_id}}" }

HTTP 403


DELETE {{base_url}}/api/admin/external-mounts/{{mount_id}}
Authorization: Bearer {{bob_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 6 — Admin deletes the mount → 204. The host filesystem
#          content is untouched; only the row + root folder go.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/external-mounts/{{mount_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 7 — It is gone from the list.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$" count == 0


# ─────────────────────────────────────────────────────────────
# Step 8 — Deleting the same id again → 404 (already removed).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/external-mounts/{{mount_id}}
Authorization: Bearer {{admin_token}}

HTTP 404
