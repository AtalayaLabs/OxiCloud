# =============================================================
# OxiCloud – same-content rewrite must not leak a blob reference
# =============================================================
# Regression test for the `manifest_refcount_mismatch` findings on
# Ed's live S3 deployment (2026-09), traced to one guard in
# `file_blob_write_repository::swap_blob_hash`:
#
#     if old_hash != new_hash
#         && let Err(e) = self.dedup.remove_reference(&old_hash).await
#
# The caller has ALREADY taken a reference on the new content
# (`update_file_content_with_blob`: "swap_blob_hash consumes its
# reference"). When a rewrite stores identical bytes,
# `old_hash == new_hash`, the guard skipped the release, and that
# surplus +1 stood. So **every same-content rewrite leaked exactly
# one reference** — matching the observed `stored: 2, actual: 1`.
#
# Why it matters beyond a wrong number: a blob whose ref_count can
# never fall back to 0 is never reclaimable. The bytes are pinned
# for the life of the deployment. Hence the final assertion here is
# not just "ref_count == 1" but "the blob is actually GONE after the
# file is purged" — that is the user-visible harm, and with the leak
# present the blob survives the purge.
#
# Collaborative editing reaches this constantly: the flush
# short-circuit key `last_flushed_content_hash` is in-memory and
# per-session, so any restart / idle-reap / reconnect loses it and
# the next flush rewrites identical bytes through the leaking path.
# WebDAV PUT of unchanged content does the same, which is what this
# test uses — it is the same `update_file_content_with_blob`
# overwrite-by-path branch, reachable without a collab session.
#
# The fixture content is UNIQUE to this file, so ref_count
# assertions are absolute (== 1) rather than relative. Do NOT reuse
# `fixtures/refcount-samecontent.txt` elsewhere or these assertions
# will flake.
#
#   content: `refcount-samecontent-9f3c1a` (27 bytes, no trailing LF)
#   blake3 : 22fc01d8938dfe4ef0237713e97a6318d2e909fb30c3d6b957d6f11e290e191c
#
# `/api/dedup/check/{hash}` exposes `ref_count` to admin callers
# only (regular users get null for anti-enumeration), so this suite
# needs the admin token that setup.hurl seeds.
#
# Baselines rather than `== 0` on the consistency sweeps: other
# files in the suite share this database, so only NEW drift
# introduced by these steps may fail the test. Both refcount
# tenants are checked because there are two counters — see
# `[[bug_dual_refcount_divergence]]`.
#
# IF THIS FILE FAILS PART-WAY, CLEAN UP BEFORE RE-RUNNING. Folder
# names are unique per parent (`idx_folders_unique_name`), so a
# leftover `hurl-rc-samecontent` makes step 3 fail with a conflict
# that has nothing to do with refcounts. And because the ref_count
# assertions are absolute, a leftover *file* holding this content
# makes step 5 read 2 instead of 1. Delete the folder (and purge it
# from trash) first:
#
#   GET    /api/folders                 → find hurl-rc-samecontent
#   DELETE /api/folders/{id}            → 204
#   GET    /api/trash/resources         → find it
#   DELETE /api/trash/{id}              → 200
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/refcount_same_content_rewrite.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login (admin, required for ref_count visibility)
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
# Step 2 — Baseline both refcount sweeps. `outcome.outcome == "ok"`
#          means the run walked the whole subject; it does NOT mean
#          zero findings, hence capturing the counts.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/blobs_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Captures]
baseline_blobs: jsonpath "$.outcome.count"
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"


POST {{base_url}}/api/admin/jobs/manifests_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Captures]
baseline_manifests: jsonpath "$.outcome.count"
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"


# ─────────────────────────────────────────────────────────────
# Step 3 — Folder to hold the fixture
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-rc-samecontent"
}

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Upload the fixture. Unique content ⇒ exactly one
#          reference, and the hash the rest of the file asserts on.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/refcount-samecontent.txt; text/plain

HTTP 201
[Captures]
file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.content_hash" == "22fc01d8938dfe4ef0237713e97a6318d2e909fb30c3d6b957d6f11e290e191c"


# ─────────────────────────────────────────────────────────────
# Step 5 — Exactly one live reference before any rewrite.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/22fc01d8938dfe4ef0237713e97a6318d2e909fb30c3d6b957d6f11e290e191c
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 1


# ─────────────────────────────────────────────────────────────
# Step 6 — Rewrite #1 with IDENTICAL bytes, via the WebDAV PUT
#          overwrite-by-path branch. 204 (not 201) confirms this
#          replaced the existing file rather than creating a second
#          one — which matters, because a second file row would
#          legitimately make ref_count 2 and mask the bug.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/hurl-rc-samecontent/refcount-samecontent.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
file,fixtures/refcount-samecontent.txt;

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 7 — THE REGRESSION ASSERTION. Same file, same bytes, so the
#          reference count must not have moved. Before the fix this
#          read 2.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/22fc01d8938dfe4ef0237713e97a6318d2e909fb30c3d6b957d6f11e290e191c
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 1


# ─────────────────────────────────────────────────────────────
# Step 8 — Rewrite #2 and #3. The leak was one reference PER
#          rewrite, so repeating proves the fix holds rather than
#          merely being off-by-one in a compensating direction.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/hurl-rc-samecontent/refcount-samecontent.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
file,fixtures/refcount-samecontent.txt;

HTTP 204


PUT {{base_url}}/webdav/hurl-rc-samecontent/refcount-samecontent.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
file,fixtures/refcount-samecontent.txt;

HTTP 204


GET {{base_url}}/api/dedup/check/22fc01d8938dfe4ef0237713e97a6318d2e909fb30c3d6b957d6f11e290e191c
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists"    == true
jsonpath "$.ref_count" == 1


# ─────────────────────────────────────────────────────────────
# Step 9 — No new refcount drift from any of it. Equality with the
#          baselines, not `== 0`, so pre-existing findings from
#          other suites cannot flunk this.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/blobs_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_blobs}}


POST {{base_url}}/api/admin/jobs/manifests_consistency/trigger
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ok"              == true
jsonpath "$.outcome.outcome" == "ok"
jsonpath "$.outcome.count"   == {{baseline_manifests}}


# ─────────────────────────────────────────────────────────────
# Step 10 — Trash the file, then purge it. Deletion is two steps:
#           DELETE /api/files moves to trash and leaves the
#           reference intact; only the trash purge decrements.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_file_id: jsonpath "$.items[?(@.resource.id == '{{file_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_file_id}}
Authorization: Bearer {{token}}

# 200, not 204 — the trash purge returns a body, unlike the
# /api/files and /api/folders deletes above.
HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 11 — THE ASSERTION THAT MATTERS MOST. The blob must be gone.
#
#           This is the user-visible harm of the leak rather than a
#           restatement of step 7: a surplus reference means the
#           count can never reach 0, so the blob is never reclaimed
#           and its bytes are pinned for the life of the
#           deployment. With the leak present and three rewrites,
#           ref_count would still be 3 here and `exists` would be
#           true.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/22fc01d8938dfe4ef0237713e97a6318d2e909fb30c3d6b957d6f11e290e191c
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists" == false


# ─────────────────────────────────────────────────────────────
# Step 12 — Teardown: the folder, then purge it too. This suite
#           shares its database with every other hurl file, so
#           leaving rows behind breaks later runs.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_folder_id: jsonpath "$.items[?(@.resource.id == '{{folder_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_folder_id}}
Authorization: Bearer {{token}}

HTTP 200
