# =============================================================
# OxiCloud — refresh-token grace: a lost response is not theft
# =============================================================
# `docs/plan/refresh-token-grace.md`. Sibling of
# `auth_session_lifecycle.hurl`, which pins the STRICT half of the
# same mechanism (replay after the new token has been used → 403).
# This file pins the two halves that strictness alone got wrong, and
# the one it still gets right:
#
#   1. login
#   2. refresh → rotation. The new access token is deliberately
#      NOT used: this is the client whose rotation response was
#      lost (navigation, tab close, dropped connection) and who
#      therefore still holds only the parent token.
#   3. replay the PARENT → 200, and the response re-delivers the
#      SAME refresh token the rotation minted. Before the grace this
#      was a 403 plus a revoked token family — a spurious logout.
#   4. use the child's access token. One authenticated request is
#      the client demonstrating it received the child.
#   5. replay the PARENT again → 403. The window closed on client
#      PROGRESS, not on a clock: step 4 is the only thing that
#      changed between here and step 3.
#   6. the child is dead too — step 5 was read as theft, so the
#      whole family went. Replay detection is intact, which is the
#      property the grace must not trade away.
#
# Requires `OXICLOUD_REFRESH_TOKEN_GRACE_SECS > 0` (the default is
# 60). At 0 this file fails at step 3 with a 403 — which is the
# correct behaviour for that setting, and the reason the step-3
# assertion names the knob.
#
# Steps 3–5 must run well inside the window. They are three
# round trips against a local server, so there is no realistic
# timing hazard; a 60s default leaves ~59.9s of headroom.
# =============================================================


# ─────────────────────────────────────────────────────────────
# 1 — Login. Captures the parent pair.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
grace_access_v1: jsonpath "$.access_token"
grace_refresh_v1: jsonpath "$.refresh_token"


# ─────────────────────────────────────────────────────────────
# 2 — Rotate. The child pair exists server-side from here on;
#     pretend the response never arrived by never using
#     `grace_access_v2` before step 4.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{ "refresh_token": "{{grace_refresh_v1}}" }

HTTP 200
[Captures]
grace_access_v2: jsonpath "$.access_token"
grace_refresh_v2: jsonpath "$.refresh_token"
[Asserts]
jsonpath "$.refresh_token" != "{{grace_refresh_v1}}"


# ─────────────────────────────────────────────────────────────
# 3 — THE PIN. Replaying the parent re-delivers the child,
#     because the child has never served a request.
#
#     `refresh_token == grace_refresh_v2` is the load-bearing
#     assertion: a mere 200 could also mean "rotated again", which
#     would mint a third token and leave the client's lost one
#     stranded. Re-delivery is what makes the retry idempotent.
#
#     A 403 here means either the grace is disabled
#     (`OXICLOUD_REFRESH_TOKEN_GRACE_SECS=0`) or the DI wiring in
#     `auth_factory.rs` lost `with_refresh_token_grace_secs` — the
#     service defaults to strict precisely so that mistake shows up
#     as a test failure rather than as a silently missing feature.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{ "refresh_token": "{{grace_refresh_v1}}" }

HTTP 200
[Asserts]
jsonpath "$.refresh_token" == "{{grace_refresh_v2}}"
# A fresh access token every time — the graced response is a
# re-delivery of the refresh token, not a replay of the whole
# response, so the access token it carries is newly minted and
# its `sid` points at the child session.
jsonpath "$.access_token" != "{{grace_access_v1}}"


# ─────────────────────────────────────────────────────────────
# 4 — The client finally uses the child. `claims.sid` lands in
#     `LastSeenTracker`, which the grace consults alongside the
#     `last_seen_at` column — the column alone trails by up to one
#     30s flush, which would leave the next step passing for the
#     wrong reason (time, not use).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{grace_access_v2}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 5 — Replay the parent once more → 403. Same request as step 3,
#     opposite outcome, and the only difference is step 4.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{ "refresh_token": "{{grace_refresh_v1}}" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# 6 — Step 5 was read as theft, so the family is gone: the CHILD
#     token no longer works either. This is the half of replay
#     detection the grace must preserve — without this assertion
#     a "grace" that simply never revokes anything would pass.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{ "refresh_token": "{{grace_refresh_v2}}" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Teardown — nothing to clean. No rows are created by a graced
# replay (that is the point), and the sessions this file minted
# are all revoked by step 5's family revoke. The shared-DB rule in
# AGENTS.md is satisfied without a cleanup step: logging in again
# in a later file mints a fresh family.
# ─────────────────────────────────────────────────────────────
