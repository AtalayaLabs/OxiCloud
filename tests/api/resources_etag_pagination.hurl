# =============================================================
# §2b — ETag stability across pagination boundaries
# =============================================================
# `resources_etag_contract.hurl` proves single-page ETag + mutation
# invalidation on the five `/resources` roots. This file covers the
# PAGINATION-BOUNDARY case: when a mutation lands on page 1, page 1's
# ETag MUST invalidate — but every page STRICTLY AFTER the boundary
# cursor MUST stay stable because keyset cursors (not offsets) select
# the same remaining row set regardless of what happens upstream.
#
# Why it matters: the SPA cache in `frontend/src/lib/api/etagCache.ts`
# keys entries by the full URL (query string included), so page 1 and
# page 2 are separate cache entries. If an upload invalidated page 2
# just because it changed page 1, every user doing cursor-based scroll
# would re-download every subsequent page on every upload — defeating
# the point of §2b for the exact workload the Photos timeline hits.
#
# Scenario on /api/folders/{id}/resources with order_by=modified_at.
# The server's default direction on this axis is DESC (newest first,
# verified in folder_db_repository.rs 'ORDER BY modified_at DESC');
# no `reverse` param is needed. The Photos-style read order falls out
# of that default.
#   1. Create a dedicated folder and upload three files in order.
#      With newest-first the gallery sees them [3rd, 2nd, 1st].
#   2. GET page 1 (limit=2) → captures etag_p1 + cursor_c1 +
#      the IDs that actually came back at positions 0 and 1.
#      (IDs are captured — the test does NOT hardcode an ordering
#      that could drift if the server's modified_at tie-break
#      changes; it asserts what the server itself just said.)
#   3. GET page 2 (cursor_c1) → captures etag_p2 and the one
#      remaining ID.
#   4. Sanity: refetch both pages with `If-None-Match` → both 304.
#   5. Upload a fourth file so newest-first becomes [4th, 3rd, 2nd, 1st].
#      The insert lands on page 1 (the new file is now the newest).
#   6. Refetch page 1 → 200 with a FRESH ETag. Position 0 is the new
#      file; position 1 is the previous position-0 file.
#   7. Refetch page 2 with the ORIGINAL cursor_c1 → 304. The keyset
#      predicate still selects everything strictly older than the
#      row cursor_c1 was issued at, which is still just the oldest
#      file.
#
# Teardown deletes the work folder and purges from trash.
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/resources_etag_pagination.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login.
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
# Step 2 — Home folder id.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Dedicated work folder (isolates row count from siblings).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-etag-pagination",
  "parent_id": "{{home_id}}"
}

HTTP 201
[Captures]
work_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Upload three distinct fixtures sequentially. Each
#          upload stamps a fresh `modified_at`, so the order
#          below becomes the oldest → newest axis.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{work_id}}
file: file,fixtures/dedup-test.jpg; image/jpeg

HTTP 201
[Captures]
file_oldest_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{work_id}}
file: file,fixtures/dedup-test-2.jpg; image/jpeg

HTTP 201
[Captures]
file_middle_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{work_id}}
file: file,fixtures/oxicloud-logo.jpg; image/jpeg

HTTP 201
[Captures]
file_newest_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Page 1 (limit=2, newest-first). Capture etag_p1,
#          cursor_c1, and the two IDs the server actually
#          returned. Expected composition from upload order:
#            position 0 = file_newest_id   (uploaded 3rd)
#            position 1 = file_middle_id   (uploaded 2nd)
#          A `next_cursor` MUST be present — file_oldest_id is
#          on page 2.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{work_id}}/resources?order_by=modified_at&limit=2
Authorization: Bearer {{token}}

HTTP 200
[Captures]
etag_p1: header "ETag"
cursor_c1: jsonpath "$.next_cursor"
[Asserts]
header "ETag" exists
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.id" == "{{file_newest_id}}"
jsonpath "$.items[1].resource.id" == "{{file_middle_id}}"
jsonpath "$.next_cursor" exists


# ─────────────────────────────────────────────────────────────
# Step 6 — Page 2. Content = [file_oldest_id]. No further cursor.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{work_id}}/resources?order_by=modified_at&limit=2&cursor={{cursor_c1}}
Authorization: Bearer {{token}}

HTTP 200
[Captures]
etag_p2: header "ETag"
[Asserts]
header "ETag" exists
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{file_oldest_id}}"
jsonpath "$.next_cursor" not exists


# ─────────────────────────────────────────────────────────────
# Step 7 — Baseline revalidation on both pages. Confirms the
#          hash is stable across identical reads before we
#          introduce the upstream mutation.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{work_id}}/resources?order_by=modified_at&limit=2
Authorization: Bearer {{token}}
If-None-Match: {{etag_p1}}

HTTP 304


GET {{base_url}}/api/folders/{{work_id}}/resources?order_by=modified_at&limit=2&cursor={{cursor_c1}}
Authorization: Bearer {{token}}
If-None-Match: {{etag_p2}}

HTTP 304


# ─────────────────────────────────────────────────────────────
# Step 8 — Upstream mutation: upload a fourth file into the
#          same folder. Newest-first ordering now puts the row
#          sequence [new, newest, middle, oldest]. The insert
#          lands on page 1.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{work_id}}
file: file,fixtures/refcount-samecontent.txt; text/plain

HTTP 201
[Captures]
file_new_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 9 — Page 1 ETag MUST invalidate — composition changed
#          to [file_new_id, file_newest_id], and the fresh_signal
#          `max(modified_at)` on the returned rows now covers
#          file_new_id.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{work_id}}/resources?order_by=modified_at&limit=2
Authorization: Bearer {{token}}
If-None-Match: {{etag_p1}}

HTTP 200
[Asserts]
header "ETag" exists
header "ETag" != "{{etag_p1}}"
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.id" == "{{file_new_id}}"
jsonpath "$.items[1].resource.id" == "{{file_newest_id}}"


# ─────────────────────────────────────────────────────────────
# Step 10 — THE KEY ASSERTION. Page 2's ETag MUST stay stable:
#           the keyset cursor cursor_c1 was issued when page 1
#           ended at file_middle_id, and the predicate "past
#           file_middle_id in newest-first order" still selects
#           exactly [file_oldest_id] — the new row is NEWER
#           than file_middle_id, so it falls on page 1 and
#           never crosses the cursor boundary.
#
#           If this returns 200, keyset stability is broken and
#           every SPA scroll past the first page re-downloads
#           on every unrelated upload — the specific regression
#           this test guards.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{work_id}}/resources?order_by=modified_at&limit=2&cursor={{cursor_c1}}
Authorization: Bearer {{token}}
If-None-Match: {{etag_p2}}

HTTP 304


# ─────────────────────────────────────────────────────────────
# Teardown — purge the work folder.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{work_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_work_id: jsonpath "$.items[?(@.resource.id == '{{work_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_work_id}}
Authorization: Bearer {{token}}

HTTP 200
