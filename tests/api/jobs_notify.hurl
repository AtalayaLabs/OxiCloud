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
#   * `OXICLOUD_SMTP_MOCK=true`                      — in-process capture
#   * `OXICLOUD_JOBS_NOTIFY_EMAIL_TO`                — the sink exists
#   * `OXICLOUD_JOBS_NOTIFY_MIN_SEVERITY=anomaly`    — every class is
#     reachable; run-health alerts are graded `anomaly` and would be
#     silent at the shipped default. The threshold's own behaviour is
#     covered by the pure unit tests on `NotifyThreshold`, not here.
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
# below depends on it: a different `min_severity` or a missing `email`
# sink would make the rest of this file fail in a way that reads as a
# broken notifier rather than a changed env file.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/notify/info
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.min_severity"      == "anomaly"
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
# Step 6 — A different severity, delivered, and labelled as itself.
#
# The severity travels with the alert rather than being flattened to
# "a finding happened": the subject has to say `inconsistent` so a
# mailbox rule can route drift differently from data loss. Pinned
# because every transport builds its text from one shared summary, so
# a regression here would mislabel every channel at once.
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


# Recorded as a row…
GET {{base_url}}/api/admin/jobs/notify_selftest/runs/{{drift_run_id}}/findings
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$..kind"     contains "selftest_drift"
jsonpath "$..severity" contains "inconsistent"


# …and delivered, carrying its own severity.
GET {{base_url}}/api/admin/smtp/test/captured?to=jobs-alert@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
drift_alert_at: jsonpath "$.captured_at"
[Asserts]
jsonpath "$.subject" == "[OxiCloud] inconsistent: selftest_drift (1) in notify_selftest"
jsonpath "$.captured_at" != "{{cleared_alert_at}}"


# ─────────────────────────────────────────────────────────────
# Step 7 — Its resolution, gated the same way its appearance was.
#
# A cleared alert keeps the severity the finding HAD, so an operator
# only gets an all-clear for something they were actually told about.
# Clearing `selftest_drift` also restores the baseline, which is what
# makes this file re-runnable: the job's latest completed run reports
# no findings again, so a second execution replays Step 1's appearance
# instead of finding nothing to announce.
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
cleared_drift_at: jsonpath "$.captured_at"
[Asserts]
jsonpath "$.subject" == "[OxiCloud] resolved: selftest_drift in notify_selftest"
jsonpath "$.captured_at" != "{{drift_alert_at}}"


# ─────────────────────────────────────────────────────────────
# Step 8 — A run that STOPS is alerted on too.
#
# The defect found on a live sandbox: `backend_consistency` hit a DNS
# failure, retried three times, paused at its cursor with
# `reason=backend_unavailable` — and told nobody on any channel, because
# the notifier was only ever called from the Completed arm. The widest
# possible severity floor and two live sinks made no difference: a pause
# produces no finding, so there was nothing for a threshold to admit.
#
# `stall=2` reproduces it without taking a backend away. Each trigger
# resumes the same run, so this is one run across three requests:
#   attempt 1 → stops, alert (the news)
#   attempt 2 → stops again, same reason, SILENT (not news twice)
#   attempt 3 → completes, all-clear
#
# `findings=0` so the finding diff has nothing to say and every message
# below is unambiguously about the run stopping.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/notify_selftest/trigger?findings=0&stall=2
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
stalled_run_id: jsonpath "$.outcome.extra.run_id"
[Asserts]
# `ok`, not `err`: the run did not fail, it stopped and can be resumed.
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.extra.paused"    == true
jsonpath "$.outcome.extra.retryable" == true
jsonpath "$.outcome.extra.reason"    == "selftest_stall"
jsonpath "$.outcome.extra.detail"    contains "Synthetic stall 1 of 2"
jsonpath "$.outcome.extra.notify_failures" == 0


# The row says why it stopped, in both halves: a stable key to match on
# and prose to read. An operator pause sets neither — that is what makes
# "error_reason present" usable as the alert trigger.
GET {{base_url}}/api/admin/jobs/notify_selftest/runs/{{stalled_run_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.status"        == "Paused"
jsonpath "$.error_reason"  == "selftest_stall"
jsonpath "$.error_message" contains "Synthetic stall 1 of 2"


# The alert itself. `kind` is the reason, so a mailbox rule can sort on
# it; the detail carries the cause, which is the half that says what to
# go and look at.
GET {{base_url}}/api/admin/smtp/test/captured?to=jobs-alert@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
stall_alert_at: jsonpath "$.captured_at"
[Asserts]
# No severity and no "(1)" in the subject, and no "finding(s)" anywhere:
# nothing was found. The first live alert read "1 anomaly finding(s) of
# kind backend_unavailable", which sends an operator looking for a
# finding that does not exist.
jsonpath "$.subject" == "[OxiCloud] stopped: selftest_stall in notify_selftest"
jsonpath "$.text_body" contains "stopped early"
jsonpath "$.text_body" contains "resumes from where it stopped"
jsonpath "$.text_body" not contains "finding(s)"
jsonpath "$.text_body" contains "Synthetic stall 1 of 2"
jsonpath "$.text_body" contains "nothing is wrong with this instance"
jsonpath "$.captured_at" != "{{cleared_drift_at}}"


# ─────────────────────────────────────────────────────────────
# Step 9 — Second attempt, same reason: silence.
#
# The muting risk, and the reason this is de-duplicated per run rather
# than per attempt: a backend down for a day gets auto-resumed
# repeatedly, and a channel that reports the same outage every time is
# one an operator turns off.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/notify_selftest/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# The same run resumed, not a new one — and the args come from the row,
# so the `stall=2` it started with still governs.
jsonpath "$.outcome.extra.run_id" == "{{stalled_run_id}}"
jsonpath "$.outcome.extra.reason" == "selftest_stall"
jsonpath "$.outcome.extra.detail" contains "Synthetic stall 2 of 2"


GET {{base_url}}/api/admin/smtp/test/captured?to=jobs-alert@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.captured_at" == "{{stall_alert_at}}"


# ─────────────────────────────────────────────────────────────
# Step 10 — Third attempt completes: the all-clear.
#
# Whoever was told the sweep had stopped is owed the news that it runs
# again. Driven by the marker the first alert left on this run, which is
# why a run that never stopped says nothing here.
#
# This also returns the job to a terminal state, so the next execution
# of this file starts with no non-terminal run — the exclusivity
# invariant would otherwise make Step 1 resume this one instead of
# starting fresh.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/notify_selftest/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.outcome.extra.run_id"    == "{{stalled_run_id}}"
jsonpath "$.outcome.extra.completed" == true
jsonpath "$.outcome.extra.notify_failures" not exists


GET {{base_url}}/api/admin/smtp/test/captured?to=jobs-alert@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.subject" == "[OxiCloud] recovered: selftest_stall in notify_selftest"
jsonpath "$.text_body" contains "is running again"
jsonpath "$.captured_at" != "{{stall_alert_at}}"


# And the row is clean afterwards. Both error columns are cleared on
# success — on resume and again on completion — because a run that
# recovered is not a run that stopped.
#
# Regression: clearing only `error_message` left `error_reason` behind,
# and the panel keys its verdict off the key — so a job that resumed and
# completed rendered as "stopped" next to "completed".
GET {{base_url}}/api/admin/jobs/notify_selftest/runs/{{stalled_run_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.status"        == "Completed"
jsonpath "$.error_reason"  not exists
jsonpath "$.error_message" not exists
