# =============================================================
# OxiCloud — Baseline: NC Basic Auth failure modes
# =============================================================
# Group P from BASELINE_TESTS_NC_WEBDAV.md.
#
# Coverage:
#   P1 — no Authorization header → 401 + WWW-Authenticate
#   P2 — wrong password (real user) → 401
#   P3 — N wrong attempts from same IP against a THROWAWAY
#        username trip the per-(account,IP) lockout
#   P4 — per-IP lockout scope (the #323 regression guard):
#        6 wrong attempts from spoofed X-Forwarded-For: IP1
#        lock (admin, IP1), but the SAME correct credential
#        from spoofed X-Forwarded-For: IP2 still succeeds.
#        Depends on OXICLOUD_TRUST_PROXY_CIDR=0.0.0.0/0 in
#        tests/common/server.env so the server honours the
#        X-Forwarded-For header on localhost.
#
# Deliberately NOT covered here:
#   P5 — External user attempts NC Basic Auth. Externals can't
#        mint app passwords in the first place (the upstream gate
#        is asserted in tests/api/external_users.hurl), so the
#        in-middleware belt-and-braces check is unreachable via
#        a black-box HTTP test. Verified by code inspection.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup 1 — JWT login (needed to mint the app password for P4's
#           positive control).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Setup 2 — Mint admin's NC app password.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "nc_auth_failures P4 positive control" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# P1 — PROPFIND without any Authorization header
#      → 401 with `WWW-Authenticate: Basic realm="OxiCloud"`.
#      NC desktop relies on this challenge to know it should
#      offer credentials at all.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/

HTTP 401
[Asserts]
header "WWW-Authenticate" contains "Basic"


# ─────────────────────────────────────────────────────────────
# P2 — Wrong password against a real user
#      → 401 with the same WWW-Authenticate challenge.
#      Anti-enumeration: response shape identical whether the
#      user exists or not.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
[BasicAuth]
{{username}}: definitely-wrong-password

HTTP 401
[Asserts]
header "WWW-Authenticate" contains "Basic"


# ─────────────────────────────────────────────────────────────
# P3 — Lockout trip
#
# Consecutive bad-credential attempts against a throwaway
# username trip the per-(account, IP) lockout. The default
# threshold is 5 (`OXICLOUD_LOCKOUT_MAX_FAILURES = 5`) and
# `tests/common/server.env` does NOT override it — the only
# raised values in that env are the broader login / refresh /
# register rate-limits, which prevent the rate-limiter from
# firing AHEAD of the lockout under sustained test traffic.
# The lockout itself still engages on the 6th attempt.
#
# Once engaged, every subsequent attempt for the same
# (account, IP) pair short-circuits to 401 from the lockout
# check, BEFORE app-password verification runs. The audit log
# fires `WARN account_temporarily_locked` from
# `login_lockout_service.rs` when the cap is hit — useful
# correlation signal during a real run.
#
# A throwaway username (`nc-lockout-probe-…`) is used because
# the lockout is keyed by (username, IP); locking a throwaway
# pair never poisons admin's auth path, so downstream Hurl
# tests in run.sh that authenticate as admin keep working.
#
# Limitation of a black-box HTTP probe: the wire response is
# 401 with the same WWW-Authenticate header whether the 401
# comes from "lockout engaged" or "still just rejecting bad
# creds" — both look the same on the wire. Verification of
# the lockout-engaged branch specifically lives in the unit
# tests (`login_lockout_service.rs::tests`). What this Hurl
# test guards is that 7 consecutive attempts keep returning
# the same 401 shape (no 500s, no header drift), and the
# server-side audit log confirms the lockout engaged at 5.
# ─────────────────────────────────────────────────────────────

PROPFIND {{base_url}}/remote.php/dav/files/nc-lockout-probe-001/
[BasicAuth]
nc-lockout-probe-001: bad-1

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/nc-lockout-probe-001/
[BasicAuth]
nc-lockout-probe-001: bad-2

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/nc-lockout-probe-001/
[BasicAuth]
nc-lockout-probe-001: bad-3

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/nc-lockout-probe-001/
[BasicAuth]
nc-lockout-probe-001: bad-4

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/nc-lockout-probe-001/
[BasicAuth]
nc-lockout-probe-001: bad-5

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/nc-lockout-probe-001/
[BasicAuth]
nc-lockout-probe-001: bad-6

HTTP 401

# 7th attempt: account+IP is now locked at the middleware level.
# Continues to return 401.
PROPFIND {{base_url}}/remote.php/dav/files/nc-lockout-probe-001/
[BasicAuth]
nc-lockout-probe-001: bad-7

HTTP 401
[Asserts]
header "WWW-Authenticate" contains "Basic"


# ─────────────────────────────────────────────────────────────
# P4 — Per-IP lockout scope (#323 regression guard)
#
# Goal: prove that locking out (admin, IP1) does NOT lock out
# (admin, IP2) — the lockout is keyed by *both* parts, not by
# username alone. This was the gap the reporter demonstrated:
# an attacker spoofing X-Forwarded-For could lock a legitimate
# user out from their own IP. The fix scoped the key.
#
# Mechanic:
#   1. Pre-check: admin's app password works from the test's
#      default client IP (127.0.0.1, no X-Forwarded-For).
#   2. 6 wrong attempts with X-Forwarded-For: 10.0.0.1 trip the
#      lockout for (admin, 10.0.0.1).
#   3. CORRECT app password with X-Forwarded-For: 10.0.0.1 →
#      401 (still locked from THIS IP — positive demonstration
#      that the lockout actually engaged, not just chance).
#   4. CORRECT app password with X-Forwarded-For: 10.0.0.2 →
#      207 (NOT locked from this IP — the load-bearing
#      assertion of P4).
#
# Why this doesn't break the rest of the suite: every other
# test runs from the default client IP (127.0.0.1) without
# X-Forwarded-For, so (admin, 127.0.0.1) is untouched. The
# lockouts placed here are on (admin, 10.0.0.1) and
# (admin, 10.0.0.2 — released by the success), neither of
# which any other test touches.
#
# Requires: OXICLOUD_TRUST_PROXY_CIDR=0.0.0.0/0 in
# tests/common/server.env.
# ─────────────────────────────────────────────────────────────

# ── Pre-check: app password works with no X-Forwarded-For ───
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 207


# ── Step 1: burn the lockout for (admin, 10.0.0.1) ──────────

PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
X-Forwarded-For: 10.0.0.1
[BasicAuth]
{{username}}: p4-bad-1

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
X-Forwarded-For: 10.0.0.1
[BasicAuth]
{{username}}: p4-bad-2

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
X-Forwarded-For: 10.0.0.1
[BasicAuth]
{{username}}: p4-bad-3

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
X-Forwarded-For: 10.0.0.1
[BasicAuth]
{{username}}: p4-bad-4

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
X-Forwarded-For: 10.0.0.1
[BasicAuth]
{{username}}: p4-bad-5

HTTP 401

PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
X-Forwarded-For: 10.0.0.1
[BasicAuth]
{{username}}: p4-bad-6

HTTP 401


# ── Step 2: CORRECT app password from IP1 — still 401 ──────
# This is the positive demonstration that the lockout engaged.
# If this were 207, the lockout would not have fired and the
# subsequent IP2 success wouldn't prove anything.
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
X-Forwarded-For: 10.0.0.1
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 401


# ── Step 3: CORRECT app password from IP2 — 207 ────────────
# The load-bearing assertion of P4: a successful auth from a
# DIFFERENT spoofed source IP proves the lockout was scoped
# to (admin, 10.0.0.1) and not to admin alone.
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/
X-Forwarded-For: 10.0.0.2
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 207


# ─────────────────────────────────────────────────────────────
# Teardown — Revoke the app password we minted. Keeps the
#            app-passwords table clean across re-runs.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
