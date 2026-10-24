# =============================================================
# OxiCloud — WebDAV / REST listing ETag cross-check
# =============================================================
# Regression test for the Cyberduck finding: GET /webdav/<file>
# and GET /api/folders/<id>/resources used to return DIFFERENT
# ETags for the SAME file whenever `updated_at` had sub-second
# ≥ 0.5.
#
# WHY: the WebDAV path reads `storage.files.updated_at` via SQL
# `EXTRACT(EPOCH FROM x)::bigint` which ROUNDS half-up, while the
# listing path used `row.updated_at.timestamp() as u64` which
# FLOORS. For a file with mtime `.883931` both paths started
# with the same TIMESTAMPTZ but produced seconds values 1 apart,
# giving ETags like `0331184a13fc0ac9-1783489231` (WebDAV) vs
# `0331184a13fc0ac9-1783489230` (listing).
#
# The fix is `datetime_to_etag_seconds` in `timestamp_helpers.rs`
# — a shared round-half-up helper that matches the SQL cast
# behaviour. Both paths now feed `File::compute_etag` the same
# integer. WebDAV sync clients (NC desktop, Cyberduck, Finder)
# keep their caches valid across the fix; only the SPA listing
# cache invalidates once for files whose sub-second was ≥ 0.5.
#
# COVERAGE
#   1. Upload a file via WebDAV (captures the WebDAV ETag).
#   2. GET the parent folder's listing via REST, pull out the
#      same file's ETag, assert byte-identical.
#   3. Delete the probe to leave a clean DB.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login, capture JWT.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Capture the user's home folder id. The listing in
#          step 4 happens against this folder.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_folder_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 3 — PUT a file via WebDAV at the user's root and capture
#          its ETag (served from the File entity → SQL-rounded).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/etag-cross-check.bin
Authorization: Bearer {{token}}
Content-Type: application/octet-stream
`0123456789ABCDEF`

HTTP 201
[Captures]
# Strip the RFC 7232 quotes at capture time. The DAV header is
# `"<tag>"`; the REST JSON `etag` field stores the inner unquoted form.
# Capturing the raw header and leaning on `contains` to absorb the
# difference (what this file did until 2026-10-24) cannot work: the
# actual is the SHORTER unquoted string, and `contains` asks whether the
# actual contains the expected, so a 29-char value can never contain its
# own 31-char quoted form. Normalising here lets the comparison below be
# an exact `==`, which is what a cross-check wants anyway.
#
# The optional quotes in the pattern keep this working either way, so a
# future change to the header's quoting does not silently skip the
# assertion.
webdav_etag: header "ETag" regex "\"?([^\"]+)\"?"


# ─────────────────────────────────────────────────────────────
# Step 4 — Fetch the folder listing via REST. The listing path
#          previously used `.timestamp() as u64` which floors;
#          the fix routes it through `datetime_to_etag_seconds`
#          which rounds half-up, matching WebDAV byte-for-byte.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{home_folder_id}}/resources?limit=200
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
# The listing echoes the file's ETag at `.items[?(name=="etag-cross-check.bin")].resource.etag`.
# jsonpath filter syntax: find the item whose name matches, pluck its etag.
#
# Exact equality, not `contains`: the capture above already stripped the
# header's quotes, and this file exists to prove the two surfaces agree
# BYTE-FOR-BYTE (the Cyberduck bug was a half-second rounding difference,
# which a substring match could mask — `...-1792876242` contains
# `...-179287624`).
jsonpath "$.items[?(@.resource.name=='etag-cross-check.bin')].resource.etag" == "{{webdav_etag}}"


# ─────────────────────────────────────────────────────────────
# Cleanup — remove the probe so a re-run starts clean.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/webdav/etag-cross-check.bin
Authorization: Bearer {{token}}

HTTP 204
