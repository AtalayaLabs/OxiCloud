# =============================================================
# OxiCloud – backend_migration against an endpoint that never answers
#
# The regression test asked for by
# `docs/plan/jobs-handling-recoverable-error.md` §Testing: assert that
# an unreachable backend lands the run in **Paused**, that
# `error_message` names the cause, and that it gets there in **bounded
# time rather than hanging**.
#
# ## The failure this pins
#
# A backend that *fails* was always handled — classified, retried,
# paused. A backend that never *answers* was handled by nothing. Pull a
# network on an established connection and there is no RST and no ICMP;
# the socket blocks until the OS abandons retransmission, on the order
# of fifteen minutes. Throughout that window the job is neither running
# nor failed: no error, so no retry, no log line, no pause. It looks
# exactly like a slow migration.
#
# Worse, before the classification fixes the `blob_exists` source probe
# reported such a failure as PERMANENT, which took the
# record-a-finding-and-continue branch — the cursor advanced past the
# blob, and with `failed` still 0 the run could reach `finish_completed`
# and flip the pointer to a target missing everything the outage
# covered. A migration reporting success having silently dropped
# whatever was unreachable at the time.
#
# ## Why `s3_blackhole` and not `s3_stub`
#
# `s3_stub` points at `127.0.0.1:9999`, where nothing listens, so the
# connection is REFUSED instantly. That path was never broken. A test
# built on it would pass with no timeout configured anywhere and pin
# nothing.
#
# `s3_blackhole` points at `192.0.2.1` — TEST-NET-1 (RFC 5737),
# reserved for documentation and guaranteed unrouted. A SYN goes
# unanswered, which is the hang. See `tests/common/server.env`.
#
# ## Why this is safe inside the shared suite
#
# The migration fails at `target.initialize()`, which runs BEFORE
# `migration_readonly` is engaged (`backend_migration_service.rs`, the
# comment on the target-init pause). So this file cannot leave the
# server read-only for whatever runs after it — the reason this shape
# was chosen over a mid-copy failure, which would hold the freeze.
#
# The run is cancelled at the end regardless, so the DB is left with no
# non-terminal `backend_migration` row.
#
# Prerequisites: setup.hurl must have run (admin user exists).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Log in as admin.
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
# Step 2 — Dispatch the migration at the black hole.
#
# `backend_migration` is a DETACHED job: the handler is spawned and the
# call returns 202 immediately, so this response carries no outcome and
# no run_id. Step 3 polls for both.
#
# That is also why the bound is asserted as a polling budget rather
# than with `duration` — the trigger returns in milliseconds no matter
# how long the backend hangs, so timing THIS request would prove
# nothing at all.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/backend_migration/trigger?storage=s3_blackhole
Authorization: Bearer {{admin_token}}

HTTP 202
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.detached" == true


# ─────────────────────────────────────────────────────────────
# Step 3 — Poll until the run reaches Paused.
#
# THE LOAD-BEARING STEP. Runs are newest-first, so `$[0]` is ours.
#
# The retry budget IS the bounded-time assertion: 60 attempts × 2s =
# 120s, after which hurl fails with the last assert error. Against the
# old hanging behaviour the row would sit in `Running` for ~15 minutes
# and this step would exhaust its budget — which is the entire point of
# the file. Every other assertion here would eventually pass even
# unbounded; this one would not.
#
# 120s is deliberately loose: comfortably above the observed ~31s (the
# SDK's own attempts stacked on the 10s connect timeout) and three
# orders of magnitude below the unbounded socket. A slow CI runner must
# not make this flaky, and the failure it guards is nowhere near the
# threshold.
#
# `Paused`, not `Failed`: that distinction is the whole plan. Failed is
# terminal and needs a human; Paused resumes and finishes the migration
# once the backend returns.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs/backend_migration/runs?limit=1
Authorization: Bearer {{admin_token}}
[Options]
retry: 60
retry-interval: 2000

HTTP 200
[Captures]
blackhole_run_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$[0].status" == "Paused"
# The reason must name what went wrong, not merely that something did.
# An operator reading only this string has to be able to tell an
# unreachable backend from a wrong bucket — the first is worth waiting
# out, the second never resolves on its own.
jsonpath "$[0].error_message" contains "Transient Backend"
jsonpath "$[0].error_message" contains "target backend init"
# Absent, not null — the run is not over. A paused row carrying a
# completion timestamp would make every "how long did this take" query
# lie, and would read as finished in the admin panel.
jsonpath "$[0].completed_at" not exists


# ─────────────────────────────────────────────────────────────
# Step 4 — Teardown: cancel the paused run.
#
# Mandatory, not tidiness. `open_or_start` picks up the latest
# non-terminal row for a job name, so a `Paused` row left behind would
# be RESUMED by the next `backend_migration` trigger in the suite —
# silently retargeting that run at the black hole and failing a test
# that has nothing to do with this file. Hurl files share one database.
#
# Cancel is also the path that releases `migration_readonly` for a
# paused row (nothing to release here — this run never engaged it —
# but the call is idempotent).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/backend_migration/cancel
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.cancelled" == true
jsonpath "$.run_id" == "{{blackhole_run_id}}"


# ─────────────────────────────────────────────────────────────
# Step 5 — Confirm the row is terminal, so the next trigger in the
# suite starts fresh instead of resuming ours.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs/backend_migration/runs/{{blackhole_run_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.status" == "Cancelled"
