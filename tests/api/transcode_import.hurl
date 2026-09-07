# =============================================================
# OxiCloud – transcode_import: registration, contract, empty-tree run
#
# ## What this covers, and what it deliberately does not
#
# It covers the job's SURFACE: that it is registered, that it declares
# the metadata the admin panel switches on, and that a run against an
# already-drained tree completes cleanly with zeroed counters rather
# than erroring or reporting phantom work.
#
# It does NOT cover the re-keying — the part that matters most, where
# `{file_id}.webp` is resolved through `storage.files` to a content
# hash, and several entries naming the same content collapse into one
# row. That needs `.transcoded/webp/` entries on disk before the run,
# and nothing reachable over HTTP can create them: since the write path
# moved to the derived tier, only callers with no content hash
# (external mounts) still write there, and hurl cannot place files in
# the server's storage directory either.
#
# So the migration itself is validated by a snapshot restore against a
# populated `.transcoded/`, the same way the thumbnail migration was —
# an exercise that found three bugs the test suite did not. This file
# guards the contract that surrounds it; it is not a substitute.
#
# The empty-tree assertions are still worth having. A drained tree is
# the steady state of every install after the first boot, so this is
# what the job does on the overwhelming majority of its runs, and
# "does nothing, quietly, forever" is a real property — the thumbnail
# equivalent warned on every boot for exactly this case until it was
# caught by eye.
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/transcode_import.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Login
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 – The job is registered, and declares itself correctly.
#
# The admin panel keys its read-only badge and its repair toggle off
# these three fields, so a job that stops declaring them degrades the
# UI silently: `mutates` wrong means a destructive job renders as safe,
# and a missing `repair_description` removes the only way to reach the
# deletion from the interface.
#
# `always` because a plain run inserts rows and writes blobs — the
# repair flag adds deletion on top, which is why the two are
# independent rather than one implying the other.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/jobs
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$[*].name" contains "transcode_import"
jsonpath "$[?(@.name=='transcode_import')].mutates" == "always"
jsonpath "$[?(@.name=='transcode_import')].recoverable" == true


# ─────────────────────────────────────────────────────────────
# Step 3 – A discovery-only run over a drained tree.
#
# No `?repair=true`: the default must import without deleting, which is
# what the daily-tick and the no-silent-auto-repair rule both depend
# on. Counters are asserted to exact zeros rather than "exists" — a
# non-zero here would mean the job found work in a directory the API
# cannot have put anything into, which is the shape a re-keying bug
# would take.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/transcode_import/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.completed" == true
jsonpath "$.outcome.extra.extra_stats.imported" == 0
jsonpath "$.outcome.extra.extra_stats.negatives" == 0
jsonpath "$.outcome.extra.extra_stats.file_gone" == 0
jsonpath "$.outcome.extra.extra_stats.deleted" == 0
jsonpath "$.outcome.extra.extra_stats.failed" == 0
jsonpath "$.outcome.extra.extra_stats.unverified" == 0
# A clean run raises no findings. `file_gone` entries are recorded as
# anomalies, so a non-zero count here would contradict the zero above.
jsonpath "$.outcome.extra.finding_count" == 0


# ─────────────────────────────────────────────────────────────
# Step 4 – The same run under `?repair=true`.
#
# On a drained tree this must be indistinguishable from the run above:
# nothing to import, nothing to delete, and — the part worth pinning —
# no complaint about the directory being absent. The thumbnail teardown
# warned `could not be removed / No such file or directory` on every
# boot after its migration finished, warning about success forever.
# Absence is the end state, not a failure, and the job has to treat it
# that way because this is what every run after the first looks like.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/transcode_import/trigger?repair=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.extra_stats.imported" == 0
jsonpath "$.outcome.extra.extra_stats.negatives" == 0
jsonpath "$.outcome.extra.extra_stats.deleted" == 0
jsonpath "$.outcome.extra.extra_stats.failed" == 0
jsonpath "$.outcome.extra.finding_count" == 0


# ─────────────────────────────────────────────────────────────
# Step 5 – Idempotence.
#
# The job is registered as a daily tick AND named in
# OXICLOUD_STARTUP_JOBS, so on a long-lived install it runs
# unattended, repeatedly, forever. Re-running must stay a no-op —
# anything that accumulated across runs would accumulate unattended.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/transcode_import/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.extra_stats.imported" == 0
jsonpath "$.outcome.extra.extra_stats.negatives" == 0
jsonpath "$.outcome.extra.finding_count" == 0
