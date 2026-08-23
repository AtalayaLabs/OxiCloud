# =============================================================
# OxiCloud – Copy-folder ref_count regression
# =============================================================
# Regression test for the ref_count drift found on Ed's sandbox
# (2026-08-22) and traced to the copy-folder path. When a folder
# is copied, every file inside gets duplicated as a NEW file row
# pointing at the SAME blob(s) — dedup wins bytes-on-disk, but
# `storage.blobs.ref_count` MUST bump by the number of new refs.
# If it doesn't, dedup GC will reap a blob that a live file row
# still references → dangling reference → user gets 404 on
# download of the copied file.
#
# Covers two paths so the CDC boundary can't hide a regression:
#
#   1. Small file (`refcount-cascade-small.txt`) → single legacy whole-
#      file blob. `storage.blobs.ref_count` counted directly on
#      the file's content hash.
#   2. 2 MB file (`refcount-cascade-cdc.bin`) → FastCDC produces multiple
#      distinct chunks. Whole-file `content_hash` still resolves
#      through the dedup API.
#
# Both fixtures are DEDICATED — unique content so ref_count
# assertions are absolute (== 1, == 2). Do NOT reuse these
# fixtures in other hurl files or absolute assertions here will
# flake.
#
# Deletion is TWO STEPS in OxiCloud:
#   `DELETE /api/folders/{id}`  → moves to trash (ref_count
#                                  unchanged; children still
#                                  reference the blob).
#   `DELETE /api/trash/{id}`    → permanent purge; NOW
#                                  ref_count decrements. If it
#                                  hits 0, blob row is deleted
#                                  synchronously (`exists=false`).
# So the test purges trash after every folder-delete step —
# skipping that would make the assertions wrong regardless of
# whether the copy-side bug is present.
#
# `/api/dedup/check/{hash}` returns `ref_count` only for admin
# callers (regular users get `null` for anti-enumeration). This
# suite requires the admin token; setup.hurl seeds it.
#
# Run:
#   hurl --variables-file tests/api/test.env --test \
#        tests/api/refcount_cascade.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login (admin, required for ref_count in dedup API)
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
# Step 2 — Baseline sweeps: run BOTH consistency tenants that
#          check ref_count invariants and capture their finding
#          counts. Later checkpoints assert equality with these
#          baselines instead of `== 0` — so stale findings from
#          previous tests don't flunk this one; only NEW drift
#          introduced by our copy/delete does.
#
#          Two tenants because there are two counters (see
#          `[[bug_dual_refcount_divergence]]`):
#
#          - `blobs_consistency` — reconciles
#            `storage.blobs.ref_count` against its auditor
#            formula. Catches chunk-level drift.
#          - `manifests_consistency` — reconciles
#            `storage.chunk_manifests.ref_count` against its
#            auditor formula. Catches whole-file drift (the
#            path the FE + `/api/dedup/check` surface reads).
#
#          Trigger returns `outcome.count = stats.finding_count`
#          for recoverable tenants (see
#          `scheduler/recoverable.rs::JobOutcome::ok_with` in
#          the Completed branch). `outcome.outcome == "ok"`
#          means the run walked the whole subject; it does NOT
#          mean zero findings.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/blobs_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Captures]
baseline_blobs_findings: jsonpath "$.outcome.count"
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"


POST {{base_url}}/api/admin/jobs/manifests_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Captures]
baseline_manifests_findings: jsonpath "$.outcome.count"
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"


# =============================================================
# Scenario A — small file (single legacy whole-file blob)
# =============================================================

# ─────────────────────────────────────────────────────────────
# A1 — Create source folder
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-ref-source-small"
}

HTTP 201
[Captures]
src_small_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# A2 — Upload the small fixture. Content is unique to this
#      test, so ref_count starts at exactly 1 (no dedup
#      collision with any other fixture in the suite).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{src_small_id}}
file: file,fixtures/refcount-cascade-small.txt; text/plain

HTTP 201
[Captures]
small_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.content_hash" == "2d8eb13178cff0036a22e0c3c42446061e86579f9d59ea73c8343bccc2df0fd3"


# ─────────────────────────────────────────────────────────────
# A3 — Baseline: exactly one live reference.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/2d8eb13178cff0036a22e0c3c42446061e86579f9d59ea73c8343bccc2df0fd3
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 1


# ─────────────────────────────────────────────────────────────
# A4 — Create target folder + copy source into it. The batch
#      endpoint is the single entry point every FE / WebDAV
#      code path funnels through.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-ref-target-small"
}

HTTP 201
[Captures]
tgt_small_id: jsonpath "$.id"

POST {{base_url}}/api/batch/folders/copy
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "folder_ids": ["{{src_small_id}}"],
  "target_folder_id": "{{tgt_small_id}}"
}

HTTP 200
[Captures]
copy_small_root: jsonpath "$.successful[0].new_root_folder_id"
[Asserts]
jsonpath "$.stats.successful"           == 1
jsonpath "$.stats.failed"               == 0
jsonpath "$.successful[0].files_copied" == 1


# ─────────────────────────────────────────────────────────────
# A5 — COPY-SIDE ASSERTION: ref_count must be exactly 2. The
#      pre-fix bug left this at 1 — a subsequent GC would then
#      reap the blob out from under the copied file.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/2d8eb13178cff0036a22e0c3c42446061e86579f9d59ea73c8343bccc2df0fd3
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 2


# ─────────────────────────────────────────────────────────────
# A5b — Sweep both consistency tenants that check ref_count.
#       Complements the single-hash probe above: if the copy
#       path miscounted some OTHER blob or manifest, the single-
#       hash probe wouldn't catch it. Delta vs the baselines
#       isolates NEW drift from ambient.
#
#       Both tenants required — one counter each; see
#       [[bug_dual_refcount_divergence]] for why the copy path
#       must maintain both symmetrically.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/blobs_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_blobs_findings}}


POST {{base_url}}/api/admin/jobs/manifests_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_manifests_findings}}


# ─────────────────────────────────────────────────────────────
# A6 — Soft-delete the copied folder tree (moves to trash).
#      ref_count is EXPECTED to stay at 2 — trashed files
#      still reference the blob per the `NOT is_trashed` gap
#      the auditor deliberately closed (see
#      `[[project_by_hash_drop_is_trashed_filter]]`). Asserting
#      == 2 here makes the trash-vs-permanent boundary explicit
#      so a future refactor that changed the semantics would
#      surface at THIS line, not several steps downstream.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{copy_small_root}}
Authorization: Bearer {{token}}

HTTP 204

GET {{base_url}}/api/dedup/check/2d8eb13178cff0036a22e0c3c42446061e86579f9d59ea73c8343bccc2df0fd3
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 2


# ─────────────────────────────────────────────────────────────
# A7 — Purge the copy folder permanently.
#      `DELETE /api/trash/{id}` accepts the ORIGINAL resource
#      id as the path param (verified in `trash_handler.rs::
#      delete_permanently`) — no need to GET+filter the trash
#      listing to translate. If soft-delete silently failed,
#      the ref_count assertion two lines below catches it.
#      ref_count must drop to exactly 1 (the source folder
#      still holds its file). Guards the DECREMENT half of
#      the invariant: a double-decrement here would go to 0
#      and the next GC wipes the still-live original's blob.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/{{copy_small_root}}
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/dedup/check/2d8eb13178cff0036a22e0c3c42446061e86579f9d59ea73c8343bccc2df0fd3
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 1


# ─────────────────────────────────────────────────────────────
# A8 — Soft-delete + purge the SOURCE folder (last holder).
#      ref_count hits 0 → blob row deleted synchronously →
#      `exists == false` on the next probe. Mirror pattern to
#      `dedup_blob_cleanup.hurl` step 10, applied to folder-
#      scoped delete.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{src_small_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/trash/{{src_small_id}}
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/dedup/check/2d8eb13178cff0036a22e0c3c42446061e86579f9d59ea73c8343bccc2df0fd3
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists" == false


# ─────────────────────────────────────────────────────────────
# A9 — End-of-Scenario-A sweep on BOTH tenants. Scenario A
#      introduced two file rows (source + copy), then deleted
#      both. Net effect on the DB is zero — so the drift count
#      on each counter must be exactly the baseline.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/blobs_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_blobs_findings}}


POST {{base_url}}/api/admin/jobs/manifests_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_manifests_findings}}


# =============================================================
# Scenario B — 2 MB multi-chunk file (CDC manifest path)
#
# Coverage: this scenario now sweeps BOTH `blobs_consistency`
# (chunk-level ref_counts on `storage.blobs.ref_count`) and
# `manifests_consistency` (whole-file ref_counts on
# `storage.chunk_manifests.ref_count`). Same-shape assertions
# as Scenario A — see the baseline capture block near the top
# of this file and [[bug_dual_refcount_divergence]] for why
# both are needed.
# =============================================================

# ─────────────────────────────────────────────────────────────
# B1 — Source folder for the multi-chunk scenario. Isolated
#      from Scenario A so deletion order can't mask a bug (e.g.
#      a shared blob whose counter goes negative).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-ref-source-cdc"
}

HTTP 201
[Captures]
src_cdc_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# B2 — Upload the 2 MB dedicated fixture. Deterministic per-
#      position content → FastCDC produces multiple distinct
#      chunks (no chunk-level dedup with any other fixture).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{src_cdc_id}}
file: file,fixtures/refcount-cascade-cdc.bin; application/octet-stream

HTTP 201
[Captures]
cdc_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.content_hash" == "fb1e63c28bb792e0f69cd16cd7595989f83c218cf70894e07d1f811ab1dc6f83"


# ─────────────────────────────────────────────────────────────
# B3 — Baseline. If exists=false here, the whole-file hash
#      isn't registered in `storage.blobs` for CDC uploads on
#      this build — swap to a chunk-hash probe or a
#      blobs_consistency-driven assertion. See
#      `docs/plan/recovery.md`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/fb1e63c28bb792e0f69cd16cd7595989f83c218cf70894e07d1f811ab1dc6f83
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 1


# ─────────────────────────────────────────────────────────────
# B4 — Target folder + folder copy.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-ref-target-cdc"
}

HTTP 201
[Captures]
tgt_cdc_id: jsonpath "$.id"

POST {{base_url}}/api/batch/folders/copy
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "folder_ids": ["{{src_cdc_id}}"],
  "target_folder_id": "{{tgt_cdc_id}}"
}

HTTP 200
[Captures]
copy_cdc_root: jsonpath "$.successful[0].new_root_folder_id"
[Asserts]
jsonpath "$.stats.successful"           == 1
jsonpath "$.successful[0].files_copied" == 1


# ─────────────────────────────────────────────────────────────
# B5 — CDC copy-side assertion: ref_count == 2.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/fb1e63c28bb792e0f69cd16cd7595989f83c218cf70894e07d1f811ab1dc6f83
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 2


# ─────────────────────────────────────────────────────────────
# B5b — Full-DB sweep on BOTH tenants after CDC copy. Multi-
#       chunk path exercises the manifest side of the invariant
#       — a bug that skips one chunk out of N would leak that
#       chunk without touching the whole-file assertion above.
#       `blobs_consistency` catches chunk-level drift;
#       `manifests_consistency` catches whole-file drift.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/blobs_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_blobs_findings}}


POST {{base_url}}/api/admin/jobs/manifests_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_manifests_findings}}


# ─────────────────────────────────────────────────────────────
# B6 — Soft-delete copy: ref_count stays at 2.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{copy_cdc_root}}
Authorization: Bearer {{token}}

HTTP 204

GET {{base_url}}/api/dedup/check/fb1e63c28bb792e0f69cd16cd7595989f83c218cf70894e07d1f811ab1dc6f83
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 2


# ─────────────────────────────────────────────────────────────
# B7 — Purge copy from trash directly by folder id: ref_count → 1.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/{{copy_cdc_root}}
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/dedup/check/fb1e63c28bb792e0f69cd16cd7595989f83c218cf70894e07d1f811ab1dc6f83
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 1


# ─────────────────────────────────────────────────────────────
# B8 — Soft-delete + purge SOURCE: ref_count → 0, blob purged.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{src_cdc_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/trash/{{src_cdc_id}}
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/dedup/check/fb1e63c28bb792e0f69cd16cd7595989f83c218cf70894e07d1f811ab1dc6f83
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists" == false


# ─────────────────────────────────────────────────────────────
# B9 — End-of-Scenario-B sweep on BOTH tenants. All Scenario B
#      rows gone; both counter drift counts must be back at
#      baseline.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/blobs_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_blobs_findings}}


POST {{base_url}}/api/admin/jobs/manifests_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_manifests_findings}}


# =============================================================
# Cleanup — soft-delete + purge the two empty target folders so
# the run leaves nothing behind. Same direct-by-id pattern as
# above; no trash-listing filter needed.
# =============================================================
DELETE {{base_url}}/api/folders/{{tgt_small_id}}
Authorization: Bearer {{token}}
HTTP 204

DELETE {{base_url}}/api/trash/{{tgt_small_id}}
Authorization: Bearer {{token}}
HTTP 200


DELETE {{base_url}}/api/folders/{{tgt_cdc_id}}
Authorization: Bearer {{token}}
HTTP 204

DELETE {{base_url}}/api/trash/{{tgt_cdc_id}}
Authorization: Bearer {{token}}
HTTP 200
