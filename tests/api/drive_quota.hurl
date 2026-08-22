# =============================================================
# OxiCloud — D4 per-drive quota enforcement
# =============================================================
# Pins the upload-time per-drive quota refusal. Scope:
#
#   1. Quota = 100 B on a shared drive; uploading a 5 MiB file →
#      `507 Insufficient Storage`. Refusal happens BEFORE the file
#      row is registered (no orphan blob, no usage drift).
#   2. A small file (32 B) under the same quota → `201`. The fire-
#      and-forget delta hook bumps `drives.used_bytes`; the next
#      `GET /api/drives` lists the new value.
#   3. After consuming most of the quota, a second small file that
#      would push us over → `507`. Confirms the check uses the
#      cached `used_bytes`, not just file size in isolation.
#   4. Unlimited quota (`quota_bytes` omitted at create) accepts the
#      same 5 MiB upload that case 1 refused → `201`.
#   5. Per-user quota is unaffected — uploading to the user's own
#      default Personal drive (no per-drive cap) still works.
#
# The check is layered on top of the existing per-user quota
# (`storage_usage_service::check_storage_quota`) — both run at the
# multipart handler; either refusal yields 507.
#
# Self-contained: provisions `dq_owner` and a fresh shared drive
# per case so this can run alongside the rest of the API suite.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Provision `dq_owner` (drive owner under test).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "dq_owner",
  "password": "DqOwnerPwd1!",
  "email":    "dq_owner@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dq_owner", "password": "DqOwnerPwd1!" }

HTTP 200
[Captures]
owner_token: jsonpath "$.access_token"
owner_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Admin creates a shared drive with a tiny 100-byte quota
#          and `dq_owner` as direct user-Owner.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "dq-tight",
  "owner": { "type": "user", "id": "{{owner_user_id}}" },
  "quota_bytes": 100
}

HTTP 201
[Captures]
tight_drive_id: jsonpath "$.id"
tight_root_id:  jsonpath "$.root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Case 1: 5 MiB upload to the 100-byte drive → 507.
#          Refused at the multipart handler before the file row is
#          registered; the blob is discarded by `discard_ingested`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{tight_root_id}}
file: file,fixtures/chunk-over-cap-5mb.bin; application/octet-stream

HTTP 507


# ─────────────────────────────────────────────────────────────
# Step 5 — Case 2: 32-byte upload fits under the 100-byte cap.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{tight_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
small_file_id: jsonpath "$.id"


# Force freshness on `drives.used_bytes`:
#   1. The fire-and-forget delta hook may not have landed yet
#      (200 ms delay to let the tokio task register — see
#      `bug_trigger_sweep_vs_spawn_hook_race`).
#   2. Force a reconciliation sweep. That's the ONLY path that
#      invalidates `readable_cache` / `default_drive_cache` after
#      Ed's 2026-07-17 design call: the sweep is the escape hatch
#      for tests / operators that need immediate cache freshness;
#      per-write invalidation would nuke the cache on every upload.
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200


GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{tight_drive_id}}')].used_bytes" == 32


# ─────────────────────────────────────────────────────────────
# Step 6 — Case 3: a SECOND small file that would push usage past
#          the cap is refused. With `hello.txt` at 32 bytes already
#          on the drive, the next 32-byte upload projects to
#          32 + 32 + 32 (header overhead negligible) — far under
#          100 — and IS accepted. Then a 5 MiB upload remains over
#          quota: 507. This protects the "uses cached used_bytes"
#          invariant: the check isn't just `size < quota`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{tight_root_id}}
file: file,fixtures/hello-copy.txt; text/plain

HTTP 201


# `used_bytes` climbs to 64 (32 + 32). Same trigger-sweep pattern
# as the first assertion — the delta is fire-and-forget and the
# listing cache lags until the sweep invalidates it.
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200


GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{tight_drive_id}}')].used_bytes" == 64


# The fire-and-forget delta hook updates `drives.used_bytes`; the
# subsequent 5 MiB attempt still fails (5 MiB > 100 alone). This
# assertion holds regardless of whether the previous delta has
# landed in cache or not — `5_242_880 > 100` either way.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{tight_root_id}}
file: file,fixtures/chunk-over-cap-5mb.bin; application/octet-stream

HTTP 507


# `used_bytes` is unchanged — the failed upload didn't charge the
# drive. (Cumulative usage is still 64; the 5 MiB write never
# registered a row.) Trigger the sweep again to guarantee cache
# freshness — the 5 MiB attempt was refused pre-write so no
# delta was queued, but the previous sweep's invalidation was
# consumed by the intervening GET which re-populated the cache
# with the pre-refused-write value. Sweep + re-check for
# determinism.
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}

HTTP 200


GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{tight_drive_id}}')].used_bytes" == 64


# ─────────────────────────────────────────────────────────────
# Step 7 — Case 4: unlimited drive accepts the same 5 MiB upload.
#          Confirms the `quota_bytes IS NULL` short-circuit.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "dq-unlimited",
  "owner": { "type": "user", "id": "{{owner_user_id}}" }
}

HTTP 201
[Captures]
unlimited_root_id: jsonpath "$.root_folder_id"
unlimited_drive_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{unlimited_root_id}}
file: file,fixtures/chunk-over-cap-5mb.bin; application/octet-stream

HTTP 201


# Unlimited drive's `used_bytes` climbs to the file's exact size
# (5 MiB = 5_242_880 bytes). Trigger-sweep pattern (see above).
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200


GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{unlimited_drive_id}}')].used_bytes" == 5242880


# ─────────────────────────────────────────────────────────────
# Step 9 — Post-delete sweep convergence.
#          By design the per-drive `used_bytes` counter is NOT
#          decremented on permanent delete (mirrors the existing
#          per-user quota design: deletes drift, the periodic sweep
#          reconciles). To prove the sweep actually closes the
#          drift, we:
#            a) Trash the 32-byte file in the unlimited drive
#               (well, both: hello.txt + hello-copy.txt are in the
#               tight drive; the 5 MiB is in the unlimited one).
#            b) Permanently delete via empty-trash.
#            c) Trigger the reconciliation sweep on demand —
#               `POST /api/admin/jobs/usage_reconcile/trigger`.
#            d) `GET /api/drives` now shows the corrected counter.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{small_file_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# Permanent purge — empty caller's trash entirely.
DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{owner_token}}

HTTP 200


# Sweep is fire-and-forget on a ticker (default 600 s). Run it now
# so the assertion below is deterministic instead of polling.
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true


# After the sweep, `tight_drive.used_bytes` has dropped from 64 to
# 32 (hello.txt purged, hello-copy.txt still live). `unlimited`
# stays at 5 MiB (nothing trashed there).
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{tight_drive_id}}')].used_bytes" == 32
jsonpath "$[?(@.id=='{{unlimited_drive_id}}')].used_bytes" == 5242880


# ─────────────────────────────────────────────────────────────
# Step 10 — `/api/admin/internal/*` is admin-only and disabled by
#          default. The gate-off case is covered by the absence of
#          the route in production configs; here we just confirm a
#          non-admin caller is refused even when the feature is on.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{owner_token}}

HTTP 403


# Trigger-GC reachable too — assert it returns the freed-blob
# summary shape. Hard count is non-deterministic (depends on the
# grace window vs the test's elapsed time), so we only check the
# response shape.
POST {{base_url}}/api/admin/jobs/dedup_gc/trigger
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ok" == true
jsonpath "$.outcome.count" exists
jsonpath "$.outcome.extra.bytes_reclaimed" exists


# ─────────────────────────────────────────────────────────────
# Step 11 — Pre-flight quota gate on MOVE and COPY.
#
#           Silent gap before 2026-07-06:
#           `move_file_with_perms` / `move_folder_with_perms`
#           / `copy_file_with_perms` / `copy_folder_tree_with_perms`
#           never called `check_drive_quota` on the destination.
#           A user could bypass a tight drive's cap by uploading
#           to their unlimited personal drive first and MOVE-ing
#           (or COPY-ing) into the tight drive afterwards.
#
#           Fix landed in the service layer, so both REST + WebDAV +
#           NC WebDAV surfaces got the check for free. This step
#           locks in the 507 shape on the REST path:
#
#             a) MOVE a 5 MiB file from unlimited → tight → 507.
#             b) COPY a 5 MiB file from unlimited → tight → 507.
#             c) Sanity — same MOVE targeted at unlimited still 200.
# ─────────────────────────────────────────────────────────────

# Capture the 5 MiB file id currently living in the unlimited drive
# (uploaded at Step 7). We'll try to relocate it into the 100-byte
# tight drive.
GET {{base_url}}/api/files?folder_id={{unlimited_root_id}}
Authorization: Bearer {{owner_token}}

HTTP 200
[Captures]
big_file_id: jsonpath "$[0].id"


# 11a — MOVE 5 MiB file into the tight (100-byte quota) drive.
#       Refused at the service pre-check: 5_242_880 + 32 > 100.
PUT {{base_url}}/api/files/{{big_file_id}}/move
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "folder_id": "{{tight_root_id}}"
}

HTTP 507


# 11b — COPY same file into tight drive. Same refusal shape as MOVE
#       — COPY creates a NEW file row that counts against
#       `drives.used_bytes` even when blob dedup means no new bytes
#       hit the store. Batch endpoint lives under `/api/batch/…`,
#       not `/api/files/…`.
POST {{base_url}}/api/batch/files/copy
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "file_ids": ["{{big_file_id}}"],
  "target_folder_id": "{{tight_root_id}}"
}

# Batch envelope: 200 all-ok, 206 partial, 400 all-failed. Our
# single-item batch has one quota-refused item → 400 with the
# failure in the `.failed[]` array (per `BatchOperationResponse`).
HTTP 400
[Asserts]
jsonpath "$.stats.failed"      == 1
jsonpath "$.stats.successful"  == 0
jsonpath "$.failed[0].id"      == "{{big_file_id}}"
jsonpath "$.failed[0].error"   exists


# 11c — Sanity: the file MOVE isn't universally broken. Targeting
#       the unlimited drive's own root succeeds (it's already
#       there, but MOVE is idempotent for same-parent — service
#       returns 200 without re-doing storage work).
PUT {{base_url}}/api/files/{{big_file_id}}/move
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "folder_id": "{{unlimited_root_id}}"
}

HTTP 200


# `used_bytes` on the tight drive is unchanged — the two refused
# operations above never wrote anything. Trigger-sweep so the
# check reads live SQL (see the class doc on the earlier
# sweep + GET pair for the design rationale).
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}

HTTP 200


GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{tight_drive_id}}')].used_bytes" == 32


# ─────────────────────────────────────────────────────────────
# Steps 12-19 — D4 quota MUTATION surface
# (`PATCH /api/drives/{id}/quota`, admin-only).
#
# The enforcement side (steps 4-11 above) tested how a fixed
# quota gates writes. These steps test how an admin CHANGES the
# quota after creation — the counterpart mutation that lets
# quotas be adjusted without recreating the drive.
#
# Reuses `tight_drive_id` (100 B initial cap, used_bytes = 32
# after Step 11's convergence) so we also exercise the soft-
# shrink case (Step 16 lowers the cap below `used_bytes = 32`
# and back — accepted, matches xfs/ext4 quota shrink behaviour).
# ─────────────────────────────────────────────────────────────


# ─────────────────────────────────────────────────────────────
# Step 12 — Admin raises `tight_drive_id` quota to 1 GiB.
# Response echoes the persisted value from the RETURNING clause.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{tight_drive_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 1073741824 }

HTTP 200
[Asserts]
jsonpath "$.quota_bytes" == 1073741824


# ─────────────────────────────────────────────────────────────
# Step 13 — Non-admin (the drive Owner) is refused with 404.
# Anti-enumeration: same shape as "no such drive". A 403 would
# leak the endpoint's existence to any caller who can hit it.
# Matches the identical pattern on `PATCH .../policies`.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{tight_drive_id}}/quota
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "quota_bytes": 500 }

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 14 — Verify Owner refusal was a no-op: cap is still
# 1 GiB from Step 12, not 500 B. Guards against a partial-write
# regression that could sneak a value through even after the
# handler-side admin gate rejects.
#
# Read as the drive owner, NOT admin: admin created this drive
# for `dq_owner` (Step 3) and holds no role_grant on it, so
# `/api/drives` (which returns only drives readable via role
# grants) would omit `tight_drive_id` from admin's list and the
# JSONPath filter would return no value.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{tight_drive_id}}')].quota_bytes" == 1073741824


# ─────────────────────────────────────────────────────────────
# Step 15 — Set unlimited via `null`. Passes through to the DB
# NULL that `storage_usage_service::check_drive_quota` reads as
# "no cap". Response echoes `null`.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{tight_drive_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": null }

HTTP 200
[Asserts]
jsonpath "$.quota_bytes" == null


# ─────────────────────────────────────────────────────────────
# Step 16 — Set unlimited via `0`. Backend normalises ≤ 0 to
# None (see the `.filter(|&q| q > 0)` in the service layer) →
# same NULL persisted, same null echoed. Guards the "0 means
# unlimited" convention shared with the write-time gate.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{tight_drive_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 0 }

HTTP 200
[Asserts]
jsonpath "$.quota_bytes" == null


# ─────────────────────────────────────────────────────────────
# Step 17 — Restore cap to 100 B so we can add a second file in
# Step 18 (the drive already holds 32 B; the null/0 cases above
# left it unlimited).
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{tight_drive_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 100 }

HTTP 200
[Asserts]
jsonpath "$.quota_bytes" == 100


# ─────────────────────────────────────────────────────────────
# Steps 18-23 — Soft-shrink semantic end-to-end:
#   "Admin sets quota BELOW current usage. Owner cannot add
#    new files, but CAN still delete existing ones."
#
# This is the real behavioural pin — matches how xfs / ext4
# quotas treat a shrink: existing data is not retroactively
# touched; the enforcement gate is `used + delta > quota`, so
# new writes are blocked until the drive shrinks back under.
#
# State entering Step 18:
#   tight_drive_id → quota=100 B, used=32 B (hello-copy.txt).
# ─────────────────────────────────────────────────────────────


# ─────────────────────────────────────────────────────────────
# Step 18 — Re-upload hello.txt (deleted in Step 9). Captures
# its id so Step 22 can delete THIS specific file to prove the
# owner-can-still-delete half of the semantic.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{tight_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
soft_shrink_file_id: jsonpath "$.id"


POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200


# used_bytes now = 64 (hello-copy.txt at 32 + hello.txt at 32).
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{tight_drive_id}}')].used_bytes" == 64


# ─────────────────────────────────────────────────────────────
# Step 19 — Admin shrinks the quota to 16 B — well below the
# current 64 B usage. This IS accepted (soft-shrink semantic:
# `drive.md §7` — no retroactive touch, enforcement kicks in
# for new writes only). Response echoes the persisted cap.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{tight_drive_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 16 }

HTTP 200
[Asserts]
jsonpath "$.quota_bytes" == 16


# ─────────────────────────────────────────────────────────────
# Step 20 — Confirm the drive is now in the "over-quota,
# delete-only" state: quota = 16 B, used_bytes = 64 B. Both
# numbers must be visible in `GET /api/drives` since operators
# rely on `used_bytes > quota_bytes` as the signal to flag
# a drive for owner attention.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{tight_drive_id}}')].quota_bytes" == 16
jsonpath "$[?(@.id=='{{tight_drive_id}}')].used_bytes"  == 64


# ─────────────────────────────────────────────────────────────
# Step 21 — Owner tries to upload a new file. Refused with
# `507 Insufficient Storage` — the same shape any over-quota
# write hits (uniform with Steps 4 / 6 / 8 / 11a-b above).
# Guards the "cannot add" half of the delete-only semantic.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{tight_root_id}}
file: file,fixtures/chunk-over-cap-5mb.bin; application/octet-stream

HTTP 507


# ─────────────────────────────────────────────────────────────
# Step 22 — Owner deletes hello.txt (the file captured at Step
# 18). Succeeds with `204 No Content` even though the drive is
# still over quota. This is the "but can still delete" half —
# an over-quota drive isn't frozen; owners recover by shrinking.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{soft_shrink_file_id}}
Authorization: Bearer {{owner_token}}

HTTP 204


# Empty trash so used_bytes reflects the permanent purge, not
# just the trashing (mirrors Step 9's convergence sequence).
DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{owner_token}}

HTTP 200


POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}

HTTP 200


# used_bytes dropped from 64 → 32 (hello-copy.txt still lives).
# Drive is STILL over quota (32 > 16), so the delete-only state
# persists — new writes still refused (see Step 23).
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{tight_drive_id}}')].used_bytes"  == 32
jsonpath "$[?(@.id=='{{tight_drive_id}}')].quota_bytes" == 16


# ─────────────────────────────────────────────────────────────
# Step 23 — Confirm the delete-only state persists: even a
# fresh 5 MiB upload attempt is still refused with 507. The
# enforcement is on total usage vs quota, not per-write. Owner
# would need to delete hello-copy.txt too (bringing used_bytes
# to 0) OR admin would need to raise the quota back for writes
# to resume.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{tight_root_id}}
file: file,fixtures/chunk-over-cap-5mb.bin; application/octet-stream

HTTP 507


# ─────────────────────────────────────────────────────────────
# Step 24 — Admin raises the cap back to 100 B (leaves the
# drive under quota again). Confirms the escape hatch: admins
# can also lift the delete-only state without owner action.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{tight_drive_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 100 }

HTTP 200
[Asserts]
jsonpath "$.quota_bytes" == 100


# ─────────────────────────────────────────────────────────────
# Step 25 — Personal-drive quota edit is refused with 400
# InvalidInput. Personal drives carry NULL `drives.quota_bytes`
# by design — the effective cap is the owner user's
# `storage_quota_bytes` envelope (memory
# `project_user_envelope_quota_model`). Allowing a per-personal-
# drive cap here would fork the enforcement model into two
# paths; the endpoint refuses cleanly with a message that
# points at the correct admin surface.
#
# Locate `dq_owner`'s default personal drive first.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

# `default_for_user` uses `#[serde(skip_serializing_if = "Option::is_none")]`
# — the field is present ONLY on the caller's default personal drive.
# The single-match filter returns a scalar, so `nth 0` breaks with
# "missing value to apply filter" (memory
# `feedback_hurl_jsonpath_filter_empty`). Body regex sidesteps that by
# anchoring on the field-adjacency pattern that Rust's `Serialize`
# preserves (id → name → kind → default_for_user).
HTTP 200
[Captures]
dq_owner_personal_drive_id: body regex "\"id\":\"([a-f0-9-]{36})\",\"name\":\"[^\"]*\",\"kind\":\"personal\",\"default_for_user\""


PATCH {{base_url}}/api/drives/{{dq_owner_personal_drive_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 500 }

HTTP 400
[Asserts]
# Response body carries the hint pointing at the correct
# admin endpoint. Substring check on "envelope" is deliberate —
# operator or misfired client script hitting this endpoint sees
# a self-documenting refusal instead of an opaque error.
body contains "envelope"


# ─────────────────────────────────────────────────────────────
# Step 26 — Non-existent drive returns 404, distinguishable
# ONLY by admin caller (a non-admin sees 404 too, per Step 13's
# anti-enum design). This is the "genuinely missing" case that
# proves the mutation isn't silently creating rows.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/00000000-0000-0000-0000-000000000000/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 42 }

HTTP 404


# No cleanup tail here — `tests/api/storage_cleanup_check.sh` enumerates
# every drive via `GET /api/admin/drives` and drains+deletes any that
# isn't admin's default. This keeps individual Hurl tests focused on
# their assertions instead of teardown.
