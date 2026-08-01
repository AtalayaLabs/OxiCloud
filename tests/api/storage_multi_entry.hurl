# =============================================================
# OxiCloud — Multi-entry storage: GET /admin/settings/storage
# =============================================================
# Pins the multi-entry projection on the admin storage endpoint
# (`docs/plan/storage-multi-entry.md` slice 6).
#
# Test env (tests/common/server.env) declares two entries:
#   * local_main — active (first in _ENTRIES, no DB pointer yet)
#   * s3_stub    — declared, never activated (bucket doesn't exist,
#                  its Test button would fail — we don't click it)
#
# Coverage:
#   * `entries[]` returned with both names in declared order.
#   * `is_active=true` on `local_main` (boot fallback picks first
#     when active_backend_name is unset).
#   * `is_active=false` on `s3_stub`.
#   * `active_entry_name` mirrors the active entry.
#   * `migration_readonly=false` on a fresh boot.
#   * `location_hint` populated per backend type (root_dir for
#     Local, bucket for S3).
#   * `encryption_enabled=false` on both (no `_ENCRYPTION_KEY`
#     declared).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin login
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 1 — GET /api/admin/settings/storage returns the entries
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/settings/storage
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# Two entries declared, in _ENTRIES order.
jsonpath "$.entries" count == 2
jsonpath "$.entries[0].name" == "local_main"
jsonpath "$.entries[1].name" == "s3_stub"

# Backend types match the declarations.
jsonpath "$.entries[0].backend" == "local"
jsonpath "$.entries[1].backend" == "s3"

# Active pointer: fresh DB has no `active_backend_name` row, so the
# boot fallback picks the FIRST entry in _ENTRIES.
jsonpath "$.active_entry_name" == "local_main"
jsonpath "$.entries[0].is_active" == true
jsonpath "$.entries[1].is_active" == false

# Read-only mode off on a fresh boot (no in-flight migration, no
# stale flag in DB).
jsonpath "$.migration_readonly" == false

# Location hints — Local uses OXICLOUD_STORAGE_PATH fallback (path
# is per-test-suite; assert the endpoint+bucket string on the S3
# side which is stable, per the test env config).
jsonpath "$.entries[1].location_hint" == "http://127.0.0.1:9999/oxicloud-test-stub"

# Neither entry declared an encryption key.
jsonpath "$.entries[0].encryption_enabled" == false
jsonpath "$.entries[1].encryption_enabled" == false


# ─────────────────────────────────────────────────────────────
# Step 2 — POST /admin/settings/storage/test with entry_name
# ─────────────────────────────────────────────────────────────
# Tests the multi-entry test-DTO path (slice-6 rework). local_main
# points at the real test storage dir, so health-check + round-trip
# should pass. Passing entry_name makes the server ignore the legacy
# `backend` / `s3_*` fields.
POST {{base_url}}/api/admin/settings/storage/test
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "entry_name": "local_main", "backend": "" }

HTTP 200
[Asserts]
jsonpath "$.connected" == true
jsonpath "$.backend_type" == "local"
jsonpath "$.roundtrip_passed" == true
jsonpath "$.phase_reached" == "cleanup_ok"


# ─────────────────────────────────────────────────────────────
# Step 3 — Unknown entry_name returns connected=false with a
#          diagnostic message (inline error — never 4xx).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/settings/storage/test
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "entry_name": "does_not_exist", "backend": "" }

HTTP 200
[Asserts]
jsonpath "$.connected" == false
jsonpath "$.message" contains "does_not_exist"
jsonpath "$.message" contains "OXICLOUD_STORAGE_ENTRIES"
