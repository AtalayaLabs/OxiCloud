# =============================================================
# OxiCloud — WebDAV PATCH data-consistency chain (RFC 5789)
# =============================================================
# `webdav_patch.hurl` covers the PATCH contract itself (ranges, append,
# preconditions, locks). This file chains multiple PATCHes against the
# SAME resource and asserts the server stays consistent afterward —
# the concern behind the review-fix commit that added quota
# enforcement, an ETag re-check, and a `direct_put_max_bytes`
# prefix/suffix accounting bug (see webdav_handler.rs::handle_patch).
#
# Coverage:
#   1. Sequential overlapping-range PATCHes on one file: each step's
#      GET reflects the splice, and the ETag changes every time (no
#      stale-tag reuse across writes).
#   2. Cross-protocol consistency: HEAD and PROPFIND report the same
#      size/ETag as the GET right after the last PATCH.
#   3. Quota rejection (507) leaves the file BYTE-FOR-BYTE unchanged —
#      the ingested blob is discarded before it's ever attached
#      (`upload_ingest::discard_ingested`).
#   4. `direct_put_max_bytes` bounds only the EDIT span, not the whole
#      file: a small edit on a file already bigger than the cap still
#      succeeds, but an edit whose OWN body exceeds the cap still 413s.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login, capture JWT
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


# ═════════════════════════════════════════════════════════════
# Part A — Sequential overlapping PATCHes + cross-protocol check
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# Step 2 — PUT a 20-byte probe: "0123456789ABCDEFGHIJ"
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/patch-consist-chain.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
`0123456789ABCDEFGHIJ`

HTTP 201
[Captures]
etag0: header "ETag"


# ─────────────────────────────────────────────────────────────
# Step 3 — Overwrite bytes 5-9 ("56789") with "XXXXX".
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/webdav/patch-consist-chain.txt
Authorization: Bearer {{token}}
X-Update-Range: bytes=5-9
Content-Type: text/plain
`XXXXX`

HTTP 204
[Captures]
etag1: header "ETag"
[Asserts]
header "ETag" != {{etag0}}


GET {{base_url}}/webdav/patch-consist-chain.txt
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body == "01234XXXXXABCDEFGHIJ"


# ─────────────────────────────────────────────────────────────
# Step 4 — Overwrite bytes 10-14 ("ABCDE") with "YYYYY".
#          Overlaps neither previous edit but chains off it —
#          proves each PATCH sees the result of the last one, not
#          a stale copy.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/webdav/patch-consist-chain.txt
Authorization: Bearer {{token}}
X-Update-Range: bytes=10-14
Content-Type: text/plain
`YYYYY`

HTTP 204
[Captures]
etag2: header "ETag"
[Asserts]
header "ETag" != {{etag1}}


GET {{base_url}}/webdav/patch-consist-chain.txt
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body == "01234XXXXXYYYYYFGHIJ"


# ─────────────────────────────────────────────────────────────
# Step 5 — HEAD reports the same size/ETag as the last GET.
# ─────────────────────────────────────────────────────────────
HEAD {{base_url}}/webdav/patch-consist-chain.txt
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Length" == "20"
# GET/HEAD/PROPFIND quote the ETag (`"<tag>"`) while PUT/PATCH return
# it raw/unquoted (compare webdav_handler.rs's `handle_head` vs
# `handle_patch` response builders) — `contains` tolerates that
# formatting difference instead of asserting byte-for-byte equality.
header "ETag" contains {{etag2}}


# ─────────────────────────────────────────────────────────────
# Step 6 — PROPFIND (named getcontentlength/getetag) agrees with
#          HEAD/GET — no drift between the WebDAV property layer
#          and the plain-file read path.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/patch-consist-chain.txt
Authorization: Bearer {{token}}
Depth: 0
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getcontentlength/>
    <D:getetag/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "number(//*[local-name()='getcontentlength'])" == 20
xpath "string(//*[local-name()='getetag'])" contains {{etag2}}


# Cleanup Part A.
DELETE {{base_url}}/webdav/patch-consist-chain.txt
Authorization: Bearer {{token}}

HTTP 204


# ═════════════════════════════════════════════════════════════
# Part B — Quota rejection leaves the file untouched
# ═════════════════════════════════════════════════════════════
# Dedicated low-quota user so this doesn't cap the shared admin
# account used by the rest of the suite.


# ─────────────────────────────────────────────────────────────
# Step 7 — Provision `patch_quota_owner` with a 50-byte quota.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "username": "patch_quota_owner",
  "password": "PatchQuotaOwnerPwd1!",
  "email":    "patch_quota_owner@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
quota_owner_id: jsonpath "$.user.id"


PUT {{base_url}}/api/admin/users/{{quota_owner_id}}/quota
Authorization: Bearer {{token}}
Content-Type: application/json
{ "quota_bytes": 50 }

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "patch_quota_owner", "password": "PatchQuotaOwnerPwd1!" }

HTTP 200
[Captures]
quota_owner_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 8 — Seed a 10-byte file (well under the 50-byte quota).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/patch-quota-probe.txt
Authorization: Bearer {{quota_owner_token}}
Content-Type: text/plain
`0123456789`

HTTP 201
[Captures]
quota_probe_etag: header "ETag"


# ─────────────────────────────────────────────────────────────
# Step 9 — Append enough bytes to push the file's new total size
#          (110 bytes) well past the 50-byte quota → 507. The
#          ingested blob is discarded before commit — the file
#          must come back completely unchanged.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/webdav/patch-quota-probe.txt
Authorization: Bearer {{quota_owner_token}}
X-Update-Range: append
Content-Type: text/plain
`this-is-a-100-byte-ish-payload-that-blows-past-the-fifty-byte-quota-set-for-this-throwaway-user-abc`

HTTP 507


GET {{base_url}}/webdav/patch-quota-probe.txt
Authorization: Bearer {{quota_owner_token}}

HTTP 200
[Asserts]
body == "0123456789"
header "ETag" contains {{quota_probe_etag}}


# Cleanup Part B.
DELETE {{base_url}}/webdav/patch-quota-probe.txt
Authorization: Bearer {{quota_owner_token}}

HTTP 204

DELETE {{base_url}}/api/admin/users/{{quota_owner_id}}
Authorization: Bearer {{token}}

HTTP 200


# ═════════════════════════════════════════════════════════════
# Part C — direct_put_max_bytes bounds the EDIT, not the whole file
# ═════════════════════════════════════════════════════════════
# `OXICLOUD_DIRECT_PUT_MAX_BYTES` (4 MiB) can't be exceeded by a
# direct PUT, so a file bigger than the cap must be seeded through
# the chunk-agnostic multipart upload endpoint instead. Reuses the
# 5 MiB all-zero fixture `run.sh` already generates for the chunk/
# direct-PUT cap tests.


# ─────────────────────────────────────────────────────────────
# Step 10 — Resolve the home folder id, seed a 5 MiB file in it.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_folder_id: jsonpath "$[0].id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{home_folder_id}}
file: file,fixtures/chunk-over-cap-5mb.bin; application/octet-stream

HTTP 201
[Captures]
big_file_name: jsonpath "$.name"


# ─────────────────────────────────────────────────────────────
# Step 11 — A SMALL mid-file edit succeeds even though the file's
#          total size (5 MiB) is already over the 4 MiB cap.
#          Pre-fix, the cap comparison counted prefix+suffix+edit
#          against the raw cap and would have wrongly 413'd any
#          edit on a file this size; post-fix only the edit span
#          itself is bounded.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/webdav/{{big_file_name}}
Authorization: Bearer {{token}}
X-Update-Range: bytes=100-104
Content-Type: application/octet-stream
`PATCH`

HTTP 204


GET {{base_url}}/webdav/{{big_file_name}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body contains "PATCH"


# ─────────────────────────────────────────────────────────────
# Step 12 — An edit whose OWN body meets/exceeds the cap still
#          413s — the cap still bites real over-cap edits, this
#          isn't a blanket bypass. Replaces the ENTIRE file (no
#          prefix/suffix at all) with a 5 MiB body.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/webdav/{{big_file_name}}
Authorization: Bearer {{token}}
X-Update-Range: bytes=0-5242879
Content-Type: application/octet-stream
file,fixtures/chunk-over-cap-5mb.bin;

HTTP 413


# Cleanup Part C.
DELETE {{base_url}}/webdav/{{big_file_name}}
Authorization: Bearer {{token}}

HTTP 204
