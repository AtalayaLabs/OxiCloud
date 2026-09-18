# =============================================================
# OxiCloud – First-time admin setup
# =============================================================
# Creates the initial admin account. Only works once: the
# endpoint is disabled after the first admin exists.
#
# Run:
#   hurl --variables-file tests/api/hurl.vars --test tests/api/setup.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 0a – Regression for AtalayaLabs/OxiCloud#677
#
# Setup with a password below the min-length must:
#   * be rejected (400),
#   * NOT lock the operator out.
#
# Pre-fix behaviour: the handler claimed system-initialisation
# BEFORE the service validated the password, so a short password
# left `auth.admin_settings.system_initialized=true` with no user
# row, and every retry bounced with 403 SystemAlreadyInitialized.
# The only recovery was to drop the DB.
#
# The two consecutive short-password attempts below prove the
# rollback path is idempotent — both must return 400, and Step 1
# below must still succeed (rather than 403). That's the whole
# regression proof.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/setup
Content-Type: application/json
{
  "username": "{{username}}",
  "email": "{{email}}",
  "password": "short"
}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 0b – Same, right at the boundary (7 chars, default min = 8).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/setup
Content-Type: application/json
{
  "username": "{{username}}",
  "email": "{{email}}",
  "password": "sevench"
}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 0c – /api/config advertises the enforced value so the SPA
# can gate submit on it. Guards against silent regressions in the
# config-DTO shape (adding an auth block was
# AtalayaLabs/OxiCloud#677 fallout).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config
HTTP 200
[Asserts]
jsonpath "$.auth.min_password_length" >= 8


# ─────────────────────────────────────────────────────────────
# Step 1 – Create the first account: the server OWNER.
#
# The `HTTP 201` here is the load-bearing assertion for the
# rollback: two short-password attempts happened above; if the
# claim was not released, this call would 403 instead.
#
# `role == "owner"`, not "admin". A fresh install has exactly one
# candidate for ownership — whoever runs setup is by definition
# standing the instance up — and setup is guarded once-only, so
# there is nothing to guess. Upgraded installs take the other
# path (`oxicloud user promote-to-owner`), where the earliest
# admin may not be the current maintainer.
#
# This pins the difference, because the whole hierarchy rests on
# it: if setup silently produced an Admin, the instance would
# start ownerless and refuse admin-of-admin operations with no
# indication why. See docs/plan/role-hierarchy-owner.md § 4.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/setup
Content-Type: application/json
{
  "username": "{{username}}",
  "email": "{{email}}",
  "password": "{{password}}"
}

HTTP 201
[Asserts]
jsonpath "$.username" == "{{username}}"
jsonpath "$.email" == "{{email}}"
jsonpath "$.role" == "owner"


# ─────────────────────────────────────────────────────────────
# Step 2 – Verify the endpoint is now disabled (admin exists)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/setup
Content-Type: application/json
{
  "username": "another",
  "email": "another@example.com",
  "password": "irrelevant"
}

HTTP 403
[Asserts]
jsonpath "$.error_type" == "SystemAlreadyInitialized"
