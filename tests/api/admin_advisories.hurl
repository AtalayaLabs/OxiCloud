# =============================================================
# OxiCloud — admin dashboard advisory flags
# =============================================================
# Pins the shape and current-env values of the two advisory flags
# surfaced on `GET /api/admin/dashboard`:
#
#   * `storage_cache_recommended`   — AppShell's "Enable the local
#     blob cache" banner, fires when the active backend is remote
#     AND the local-disk cache is disabled.
#   * `notification_sink_missing`   — AppShell's "Configure a
#     notification sink" banner, fires when NEITHER
#     OXICLOUD_JOBS_NOTIFY_EMAIL_TO nor OXICLOUD_WEBHOOK_URL is set.
#
# ## What this scenario pins
#
# The current test env (`tests/common/server.env`) has:
#   - OXICLOUD_JOBS_NOTIFY_EMAIL_TO = jobs-alert@oxicloud.local
#   - OXICLOUD_WEBHOOK_URL          = (unset)
#   - OXICLOUD_STORAGE_CACHE_ENABLED = (unset, defaults to TRUE)
#   - Active backend                 = local_main (first entry)
#
# So the expected values here are BOTH `false` — but read the cache one
# carefully, because its reason changed when the default flipped to
# enabled. `storage_cache_recommended` is `remote && !cache.enabled`,
# and it is now false because the active backend is LOCAL, not because
# the cache is off. The advisory therefore fires only for an operator
# who explicitly disabled the cache while on a remote backend, which is
# a narrower (and more interesting) signal than "hasn't opted in yet".
# The scenario pins:
#   1. Both fields exist on the DTO (regression gate against a
#      schema removal / rename).
#   2. Both are booleans (not strings, not null — strict shape).
#   3. Their values match what the current env should produce.
#      If an operator accidentally unsets EMAIL_TO in server.env or
#      flips the suite's active backend to a remote entry, this
#      scenario breaks loudly and names the field.
#
# ## What this scenario DELIBERATELY does NOT pin
#
# The "banner fires" direction (either flag true). The computation
# depends on BOOT-TIME env vars; the hurl suite runs against a
# single running server and can't change env mid-run. Exercising
# the true branches needs either:
#   - A second server env + a second hurl selector (
#     `storage_cache_recommended=true` when active=azurite +
#     cache off), OR
#   - A Rust unit test for the branch-coverage.
#
# The Rust side carries the true-branch coverage (see the
# `#[test]` module in `admin_handler.rs` for the planned
# `notification_sink_missing_fires_when_no_sink` test).
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/admin_advisories.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Shape + current-env values on the dashboard DTO.
#
# `isBoolean` filter rejects null / string / missing — the schema
# contract is a strict boolean on both fields. Then the equality
# assertions pin the env-driven current value: a change to the
# suite's server.env (unset EMAIL_TO, enable a remote active
# backend) breaks this scenario and names the field.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/dashboard
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# Shape: all three flags present, all booleans.
jsonpath "$.storage_cache_recommended" isBoolean
jsonpath "$.notification_sink_missing" isBoolean
jsonpath "$.opaque_migration_recommended" isBoolean
# Values matching tests/common/server.env:
#   - active backend is local_main (first entry) → not remote → false
jsonpath "$.storage_cache_recommended" == false
#   - OXICLOUD_JOBS_NOTIFY_EMAIL_TO is set → sink NOT missing → false
jsonpath "$.notification_sink_missing" == false
#   - OXICLOUD_AUTH_OPAQUE_MODE=migrate → already migrating → false.
#     The harness happens to be configured the way the banner advises,
#     which makes this the useful direction to pin: flipping the suite
#     back to `off` must surface the advisory rather than leave the
#     field stuck. The password half of the rule is satisfied here too
#     (`OXICLOUD_AUTH_METHODS=password,magic_link`), so a `true` would
#     mean the mode check regressed, not the method check.
jsonpath "$.opaque_migration_recommended" == false
