# =============================================================
# OxiCloud — out-of-band job alerting, end to end
# =============================================================
# The whole chain, in the order a real incident takes it:
#
#   a run completes
#     → `jobs.run_findings` gets a row
#     → the notifier diffs this run's kinds against the previous
#       completed run of the same job
#     → the severity threshold admits (or drops) the transition
#     → every configured sink delivers
#     → the mock SMTP sender captures the message
#
# Nothing else covers the middle of that. The unit tests cover the
# diff and the message, and `POST /api/admin/webhook/test` injects an
# alert *past* the diff and the threshold — so the one failure this
# feature cannot afford, a channel that looks configured and stays
# silent, lives precisely in the part neither of them touches.
#
# Driven by `notify_selftest`, a read-only job whose entire output is
# one synthetic finding. Its `severity` / `kind` / `findings`
# parameters are what make the transition reachable from a test.
#
# Requires, from `tests/common/server.env`:
#   * `OXICLOUD_SMTP_MOCK=true`           — in-process capture
#   * `OXICLOUD_JOBS_NOTIFY_EMAIL_TO`     — the sink exists at all
# and the DEFAULT `OXICLOUD_JOBS_NOTIFY_MIN_SEVERITY` (`data_loss`),
# deliberately: the configuration under test is the shipped one.
#
# Self-restoring, and it has to be. Alerts fire on a CHANGE, so this
# file's assertions depend on the baseline the previous run left. The
# last step clears the finding, which puts the job back to "no
# findings reported" — so a second execution of this file sees the
# same appeared → unchanged → cleared sequence as the first. Remove
# that step and the file passes once per database.
#
# For the same reason every run below changes exactly ONE finding
# kind. A run that records a different kind also clears the previous
# one, in the same diff — so a step asserting "no mail" would be
# competing with a resolution alert that is correctly on its way.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 0 — The alerting policy, as the server sees it.
#
# Asserted before anything is triggered, because every assertion
# below depends on it: a `min_severity` other than `data_loss` or a
# missing `email` sink would make the rest of this file fail in a way
# that reads as a broken notifier rather than a changed env file.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/notify/info
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.min_severity"      == "data_loss"
jsonpath "$.sinks"             contains "email"
jsonpath "$.email_recipients"  contains "jobs-alert@oxicloud.local"


# ─────────────────────────────────────────────────────────────
# Step 1 — First run: the finding appears, so an alert is owed.
#
# `notify_failures` is asserted ABSENT rather than zero — the engine
# only adds the key when delivery failed, so its presence is the
# signal. A broken sink would surface here before any mail assertion
# below could explain why.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/notify_selftest/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
first_run_id: jsonpath "$.outcome.extra.run_id"
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.completed"      == true
jsonpath "$.outcome.extra.finding_count"  == 1
jsonpath "$.outcome.extra.scanned_count"  == 1
jsonpath "$.outcome.extra.notify_failures" not exists


# The finding is a row, independent of whether anyone was told —
# the invariant the whole design rests on.
GET {{base_url}}/api/admin/jobs/notify_selftest/runs/{{first_run_id}}/findings
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$..kind"     contains "selftest_finding"
jsonpath "$..severity" contains "data_loss"


# ─────────────────────────────────────────────────────────────
# Step 2 — The mail that run produced.
#
# Read back from the mock sender: a real alert, formatted by
# `EmailNotificationSink`, for a finding a real run recorded.
#
# The subject assertion is the one that matters most. Severity and
# kind lead it so a mailbox rule can sort on them, and `[OxiCloud]`
# prefixes it so the message is identifiable in a shared inbox.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/smtp/test/captured?to=jobs-alert@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
first_alert_at: jsonpath "$.captured_at"
[Asserts]
jsonpath "$.to"      == "jobs-alert@oxicloud.local"
jsonpath "$.subject" == "[OxiCloud] data_loss: selftest_finding (1) in notify_selftest"
# Self-contained body: the panel is not open where mail is read.
jsonpath "$.text_body" contains "notify_selftest"
jsonpath "$.text_body" contains "selftest_finding"
jsonpath "$.text_body" contains "data_loss"
jsonpath "$.text_body" contains "appeared"
jsonpath "$.text_body" contains "{{first_run_id}}"
# An alert is news, not a report of action taken. Nothing in this
# chain repairs anything, and the message must not imply otherwise.
jsonpath "$.text_body" contains "nothing has been repaired or deleted"
# Plain text only. Telegram's Markdown mode rejects the unpaired `_`
# in `selftest_finding` outright, so the shared summary carries no
# markup — and a second HTML representation would be a second thing
# to keep saying the same sentence.
jsonpath "$.html_body" == null


# ─────────────────────────────────────────────────────────────
# Step 3 — Second identical run: SILENCE.
#
# The defect the transition diff exists to prevent. Once detectors
# are scheduled, a standing problem is re-reported every run, and
# fifty-two weekly emails about the same finding is an alarm someone
# mutes — after which the channel is worse than absent, because it is
# one an operator still believes in.
#
# "No new mail" is asserted by capture time: the mock keeps only the
# most recent message per recipient, so an unchanged `captured_at`
# means nothing was delivered in between.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/notify_selftest/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.extra.finding_count"   == 1
jsonpath "$.outcome.extra.notify_failures" not exists


GET {{base_url}}/api/admin/smtp/test/captured?to=jobs-alert@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.captured_at" == "{{first_alert_at}}"


# ─────────────────────────────────────────────────────────────
# Step 4 — The resolution alert.
#
# `findings=0` records nothing, so the kind present in the previous
# completed run reads as cleared. An operator told about a problem is
# owed the news that it ended; without this the only way to learn is
# to go looking.
#
# Ordered here, before the threshold steps, so every run in this file
# changes exactly ONE kind. Recording a different kind would clear
# the previous one in the same diff, and an assertion about "no mail"
# would then be competing with a resolution alert that is correctly
# on its way.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/notify_selftest/trigger?findings=0
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.extra.finding_count"   == 0
jsonpath "$.outcome.extra.notify_failures" not exists


GET {{base_url}}/api/admin/smtp/test/captured?to=jobs-alert@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
cleared_alert_at: jsonpath "$.captured_at"
[Asserts]
jsonpath "$.subject" == "[OxiCloud] resolved: selftest_finding in notify_selftest"
jsonpath "$.text_body" contains "no longer reported"
# A resolution is not a count of zero findings.
jsonpath "$.text_body" not contains "0 data_loss"
# Delivered after Step 2's, which is what proves this is a second
# message rather than the first one still sitting in the cache.
jsonpath "$.captured_at" != "{{first_alert_at}}"


# ─────────────────────────────────────────────────────────────
# Step 5 — A bad severity fails the run.
#
# Fail-fast rather than record-and-drop: a finding whose severity no
# build recognises sits below every threshold, which is the silent
# channel this file exists to rule out — arriving from the job that
# is supposed to detect it.
#
# The envelope is still a 200, like every other trigger outcome; the
# run is what failed. A failed run is also not a baseline — only
# COMPLETED runs are diffed — so this leaves Step 4's empty state
# standing for Step 6, which is itself worth pinning.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/notify_selftest/trigger?severity=critical
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.outcome" == "err"
jsonpath "$.outcome.message" contains "not a severity"


# ─────────────────────────────────────────────────────────────
# Step 6 — A finding below the floor is recorded and NOT delivered.
#
# The threshold is the other half of "a channel that stays silent",
# and the half an operator is most likely to get wrong: the shipped
# default admits only `data_loss`, so a configured, healthy, wired
# channel still says nothing about drift. Recording it while not
# delivering it is the correct behaviour, and both halves are
# asserted.
#
# Reachable as a lone appearance because Step 4 left no findings and
# Step 5 failed without becoming the baseline.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/notify_selftest/trigger?severity=inconsistent&kind=selftest_drift
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
drift_run_id: jsonpath "$.outcome.extra.run_id"
[Asserts]
jsonpath "$.outcome.extra.finding_count"   == 1
jsonpath "$.outcome.extra.notify_failures" not exists


# Recorded…
GET {{base_url}}/api/admin/jobs/notify_selftest/runs/{{drift_run_id}}/findings
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$..kind"     contains "selftest_drift"
jsonpath "$..severity" contains "inconsistent"


# …and not delivered: the newest message is still Step 4's.
GET {{base_url}}/api/admin/smtp/test/captured?to=jobs-alert@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.captured_at" == "{{cleared_alert_at}}"


# ─────────────────────────────────────────────────────────────
# Step 7 — Teardown, and the gate's symmetry.
#
# Clears `selftest_drift`. Its resolution is gated on the way out
# exactly as its appearance was on the way in — an operator only
# gets an all-clear for something they were told about — so no mail
# may arrive here either.
#
# This is also what makes the file re-runnable: the job's latest
# completed run now reports no findings, so a second execution
# replays Step 1's appearance instead of silently finding nothing to
# announce.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/notify_selftest/trigger?findings=0
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.extra.finding_count"   == 0
jsonpath "$.outcome.extra.notify_failures" not exists


GET {{base_url}}/api/admin/smtp/test/captured?to=jobs-alert@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.captured_at" == "{{cleared_alert_at}}"
