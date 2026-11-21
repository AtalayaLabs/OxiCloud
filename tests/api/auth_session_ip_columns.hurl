# =============================================================
# OxiCloud — created_ip vs last_seen_ip
# =============================================================
# A session records two addresses, named to mirror the
# timestamps they pair with:
#
#     created_at    created_ip    where it was made
#     last_seen_at  last_seen_ip  where it is being used
#
# **The mismatch is the signal.** A session authorised from one
# address and now used from another is the shape of a stolen
# refresh token, and neither column shows that alone.
#
# IP spoofing works here because `tests/common/server.env` sets
# `OXICLOUD_TRUST_PROXY_CIDR=0.0.0.0/0`, so `X-Forwarded-For` is
# honoured. Without that line every request would look like it
# came from the loopback peer and this file could assert nothing.
#
# Coverage:
#   1. Fixture user, logged in from a spoofed address
#   2. created_ip records THAT address
#   3. last_seen_ip picks up a DIFFERENT address after use
#   4. created_ip is unchanged by step 3 — the property the
#      whole pair depends on
#   5. Teardown
#
# Not covered: that a refresh INHERITS `created_ip` rather than
# re-capturing it. The rotation happens inside one request, so a
# black-box test cannot distinguish "inherited" from "captured
# from a refresh that came from the same address" without driving
# the refresh from a third address AND waiting out another flush.
# The inheritance is asserted where it is decided, in the
# `refresh_token` unit tests.
# =============================================================


# ─────────────────────────────────────────────────────────────
# 0 — Admin, for fixture setup and for reading the session list.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 1 — Fixture user, so the assertions below can filter the
#     session list to exactly one row rather than guessing which
#     of the admin's sessions is theirs.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "ip-columns-probe",
  "password": "IpColumnsPass1!",
  "email":    "ip-columns-probe@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
fixture_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# 2 — Log in FROM 198.51.100.7 (RFC 5737 documentation range, so
#     it can never collide with a real address in a CI log).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
X-Forwarded-For: 198.51.100.7
Content-Type: application/json
{ "username": "ip-columns-probe", "password": "IpColumnsPass1!" }

HTTP 200
[Captures]
fixture_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 3 — created_ip is the login address.
#
# Also asserts last_seen_ip is still null: the session has made
# no request of its own yet, and NULL says that honestly rather
# than being backfilled from created_ip. A backfill would erase
# the mismatch signal on exactly the sessions where it matters.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/sessions?user_id={{fixture_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.sessions" count == 1
jsonpath "$.sessions[0].created_ip" == "198.51.100.7"
jsonpath "$.sessions[0].last_seen_ip" == null


# ─────────────────────────────────────────────────────────────
# 4 — Use the session FROM a different address.
#
# One ordinary authenticated call. The address is observed by
# the per-request stamp, which is an in-memory DashMap upsert —
# no SQL here, which is the point of the design.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{fixture_token}}
X-Forwarded-For: 203.0.113.9

HTTP 200


# ─────────────────────────────────────────────────────────────
# 5 — last_seen_ip becomes the USED-from address, and created_ip
#     does NOT move.
#
# Retries because the write is batched: `LastSeenTracker` flushes
# every `FLUSH_INTERVAL` (30 s) for every session stamped in the
# window, in one UNNEST'd UPDATE. Polling rather than sleeping
# means this passes as soon as the flush lands instead of always
# paying the worst case — and 40 s of budget covers a 30 s
# interval plus scheduling slack.
#
# The second assertion is the load-bearing one. If `created_ip`
# ever tracked the latest request instead of the session's
# origin, both columns would show 203.0.113.9, the mismatch would
# always be empty, and the signal this pair exists for would be
# silently dead while every other test still passed.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/sessions?user_id={{fixture_id}}
Authorization: Bearer {{admin_token}}
[Options]
retry: 40
retry-interval: 1000ms

HTTP 200
[Asserts]
jsonpath "$.sessions" count == 1
jsonpath "$.sessions[0].last_seen_ip" == "203.0.113.9"
jsonpath "$.sessions[0].created_ip" == "198.51.100.7"


# ─────────────────────────────────────────────────────────────
# 6 — Teardown.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{fixture_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
