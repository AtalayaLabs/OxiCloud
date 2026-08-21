# =============================================================
# OxiCloud — Baseline: second-user fixture (bob)
# =============================================================
# (Re)creates the `bob` user so the Group O cross-user
# isolation scenarios (and Group C's C4 admin-reads-another-
# user case) have a real second principal to test against.
#
# Run-order: this file runs AFTER `external_users.hurl`, which
# deletes bob at its end, and AFTER `permissions.hurl`, which
# creates+uses bob via the admin API. By the time this file
# runs, bob may or may not exist — the anti-enumeration
# registration endpoint returns 200 either way, so this is
# safe in both states. The login step that follows is the
# actual existence assertion: if bob can log in, downstream
# bob-dependent tests (nc_admin_views_other_user.hurl here
# and test_nc_cross_user_isolation.sh in the webdav suite)
# will work.
#
# Bob's credentials are hardcoded fixtures (not env-driven) so
# downstream test files don't need to coordinate via test.env.
# Bob's password is unique per fixture; no overlap with admin.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Register bob via the email-anti-enumeration flow.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "bob",
  "email": "bob@example.com",
  "password": "BobPassword1!"
}

HTTP 200
[Asserts]
jsonpath "$.message" contains "request received"


# ─────────────────────────────────────────────────────────────
# Step 2 — Confirm bob can log in (proves registration
#          actually landed the account — the anti-enum response
#          alone wouldn't tell us).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "bob", "password": "BobPassword1!" }

HTTP 200
[Asserts]
jsonpath "$.access_token" exists
jsonpath "$.user.full.user.username" == "bob"
jsonpath "$.user.full.user.email"    == "bob@example.com"
