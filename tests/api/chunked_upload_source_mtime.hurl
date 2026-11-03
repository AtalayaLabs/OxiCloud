# =============================================================
# OxiCloud — the chunked upload preserves the client's source mtime
# =============================================================
# Plan §9, last gap. Three of the four upload paths took
# `source_modified_at`; this one did not, so a client finalising a
# chunked session had its file stamped with the upload moment while the
# same file through any other route kept its real mtime — and no
# response told it so.
#
# The cause was one literal. `upload_file_streaming_with_perms` — the
# variant the chunked path finalises through, for the AuthZ re-check —
# delegated to `upload_file_streaming` with a hardcoded `None` for the
# mtime. Everything below it already worked.
#
# ── Why the field is on `/complete` ───────────────────────────────────
# Not on session creation. `/complete` is where the file row is created;
# a value supplied at session open would have to be carried through the
# session for no gain, and chunked sessions can stay open for hours.
#
# ── Milliseconds, and the sub-second remainder ────────────────────────
# Epoch MILLISECONDS, like the multipart and delta fields and unlike
# `X-OC-Mtime` (seconds, because ownCloud defined it so). The rounding
# rule therefore lives in exactly one place — the server. The seeded
# value has a remainder ≥ 500 so truncation and half-up rounding give
# DIFFERENT answers: 1792055662700 ms is 2026-10-15T09:14:22.700Z, which
# rounds to :23 and truncates to :22. A server that truncated would
# report a file one second older than the client believes, which is
# enough to make the next mtime comparison flap.
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/chunked_upload_source_mtime.hurl
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
# Step 2 – A working folder, so the assertions cannot collide with
#          whatever else the shared database holds.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "hurl-chunked-mtime" }

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 – Session + single chunk. `hello.txt` is 32 bytes and the
#          server's minimum chunk size is 1 MiB, so index 0 carries
#          the whole file.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/uploads
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "filename": "chunked-mtime.txt",
  "folder_id": "{{folder_id}}",
  "content_type": "text/plain",
  "total_size": 32,
  "chunk_size": 1048576
}

HTTP 201
[Captures]
upload_id: jsonpath "$.upload_id"


PATCH {{base_url}}/api/uploads/{{upload_id}}?chunk_index=0
Authorization: Bearer {{token}}
Content-Type: application/octet-stream
file,fixtures/hello.txt;

HTTP 200
[Asserts]
header "Upload-Complete" == "true"


# ─────────────────────────────────────────────────────────────
# Step 4 – Complete WITH the asserted mtime → the stored clock is the
#          client's instant, not now.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/uploads/{{upload_id}}/complete
Authorization: Bearer {{token}}
Content-Type: application/json
{ "source_modified_at": 1792055662700 }

HTTP 201
[Captures]
file_id: jsonpath "$.file_id"


# `?metadata=true` — the bare path downloads the BYTES, so a jsonpath
# assertion on it fails with "HTTP response is not a valid JSON".
GET {{base_url}}/api/files/{{file_id}}?metadata=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
# ⚠ :23, not :22 — the remainder is 700 ms and the server rounds
# half-up. Truncation would report the file a second older than the
# client believes.
jsonpath "$.modified_at" == "2026-10-15T09:14:23Z"


# ─────────────────────────────────────────────────────────────
# Step 5 – Omitted means NOW(), which must still work. The field is
#          optional, and a client that sends nothing is not an error.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/uploads
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "filename": "chunked-mtime-absent.txt",
  "folder_id": "{{folder_id}}",
  "content_type": "text/plain",
  "total_size": 32,
  "chunk_size": 1048576
}

HTTP 201
[Captures]
upload_id_bare: jsonpath "$.upload_id"


PATCH {{base_url}}/api/uploads/{{upload_id_bare}}?chunk_index=0
Authorization: Bearer {{token}}
Content-Type: application/octet-stream
file,fixtures/hello.txt;

HTTP 200


# No body at all — the shape every existing chunked client sends.
POST {{base_url}}/api/uploads/{{upload_id_bare}}/complete
Authorization: Bearer {{token}}

HTTP 201
[Captures]
file_id_bare: jsonpath "$.file_id"


GET {{base_url}}/api/files/{{file_id_bare}}?metadata=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.modified_at" != "2026-10-15T09:14:23Z"


# ─────────────────────────────────────────────────────────────
# Step 6 – The trust bounds, same two as every other path.
#
# A parseable-but-implausible value is the client asserting something
# specific and wrong, so it is a 400 rather than a silent clamp —
# clamping would bake a bad timestamp into the tree. Each attempt needs
# its own session, because a rejected `/complete` leaves the session
# open rather than consuming it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/uploads
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "filename": "chunked-mtime-past.txt",
  "folder_id": "{{folder_id}}",
  "content_type": "text/plain",
  "total_size": 32,
  "chunk_size": 1048576
}

HTTP 201
[Captures]
upload_id_past: jsonpath "$.upload_id"


PATCH {{base_url}}/api/uploads/{{upload_id_past}}?chunk_index=0
Authorization: Bearer {{token}}
Content-Type: application/octet-stream
file,fixtures/hello.txt;

HTTP 200


# 946684799000 = 1999-12-31T23:59:59Z, one second before the floor.
POST {{base_url}}/api/uploads/{{upload_id_past}}/complete
Authorization: Bearer {{token}}
Content-Type: application/json
{ "source_modified_at": 946684799000 }

HTTP 400


POST {{base_url}}/api/uploads
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "filename": "chunked-mtime-future.txt",
  "folder_id": "{{folder_id}}",
  "content_type": "text/plain",
  "total_size": 32,
  "chunk_size": 1048576
}

HTTP 201
[Captures]
upload_id_future: jsonpath "$.upload_id"


PATCH {{base_url}}/api/uploads/{{upload_id_future}}?chunk_index=0
Authorization: Bearer {{token}}
Content-Type: application/octet-stream
file,fixtures/hello.txt;

HTTP 200


# 4102444800000 = 2100-01-01Z, far past the one-hour future tolerance.
POST {{base_url}}/api/uploads/{{upload_id_future}}/complete
Authorization: Bearer {{token}}
Content-Type: application/json
{ "source_modified_at": 4102444800000 }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Teardown – the suite shares one database; leave nothing behind.
# The two rejected sessions are still open, so cancel them — otherwise
# their chunk parts sit on disk until `uploads_cleanup` runs.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/uploads/{{upload_id_past}}
Authorization: Bearer {{token}}

HTTP *

DELETE {{base_url}}/api/uploads/{{upload_id_future}}
Authorization: Bearer {{token}}

HTTP *

DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP *
