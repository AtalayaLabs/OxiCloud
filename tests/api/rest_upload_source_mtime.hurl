# =============================================================
# OxiCloud — the REST upload preserves the browser's source mtime
# =============================================================
# Plan §9. Until this landed, `POST /api/files/upload` had no mtime slot
# at all: a drag-and-drop upload was stamped with the upload moment, so
# a photo library from 2008 arrived dated today. Only the two NextCloud
# surfaces could preserve an mtime, via `X-OC-Mtime`.
#
# The new multipart field is `source_modified_at`, carrying
# **epoch MILLISECONDS** — `File.lastModified` verbatim. The server
# rounds half-up (`millis_to_seconds_rounded`) and then applies the same
# trust bounds as `X-OC-Mtime` (`validate_client_mtime`).
#
# ── Why milliseconds, and why that matters to this test ───────────────
# `X-OC-Mtime` is seconds because ownCloud defined it so. This field is
# ours, so it takes the browser's value unchanged and the rounding rule
# lives in exactly one place — the server. A client that truncated
# locally would be invisible to us.
#
# So the seeded value below has a **sub-second remainder ≥ 500**, which
# is the only way to tell rounding from truncation:
#
#   1792055662700 ms  →  truncated 1792055662  ←  wrong
#                     →  rounded   1792055663  ←  the assertion
#
# A remainder under 500 passes under either implementation and proves
# nothing. This is the same trap the plan flags for the Playwright spec.
#
# ── Field ordering is load-bearing ───────────────────────────────────
# `source_modified_at` must arrive BEFORE the `file` part: the server
# reads multipart fields in order and the `file` field consumes the
# stream. Hurl's `[MultipartFormData]` preserves the written order, so
# the section below is also an ordering assertion — move the `file:`
# line up and the mtime is silently ignored.
#
# Self-contained: own folder, removed in teardown.
# =============================================================


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "rest-mtime"
}

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


# ═════════════════════════════════════════════════════════════
# A — the asserted mtime survives, and is ROUNDED not truncated
# ═════════════════════════════════════════════════════════════
# 1792055662700 ms = 2026-10-15T09:14:22.700Z → rounds to :23.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
source_modified_at: 1792055662700
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Asserts]
# 1792055663 = 2026-10-15T09:14:23Z. Truncation gives :22 — a different
# assertion, which is the whole point of the .700 remainder.
jsonpath "$.modified_at" == "2026-10-15T09:14:23Z"


# Read it back through a listing too — the upload response and the
# listing read through different code paths (one returns the DTO the
# write built, the other re-reads the row), and §2 broke exactly that
# pairing once.
GET {{base_url}}/api/folders/{{folder_id}}/resources?limit=50&resource_types=file
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.modified_at" == "2026-10-15T09:14:23Z"


# ═════════════════════════════════════════════════════════════
# B — omitting the field stamps NOW()
# ═════════════════════════════════════════════════════════════
# The common case for any client that does not send it, and proof the
# field is optional rather than required.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/hello-copy.txt; text/plain

HTTP 201
[Asserts]
# Not the Part A instant — this one was stamped on arrival.
jsonpath "$.modified_at" != "2026-10-15T09:14:23Z"


# ═════════════════════════════════════════════════════════════
# C — the trust bounds apply, and are refused not clamped
# ═════════════════════════════════════════════════════════════
# C1 — before 2000-01-01Z. 946684799000 ms = 1999-12-31T23:59:59Z, one
# second under the floor. Catches zero, negative and uninitialised
# clocks, which would otherwise sort to the start of every timeline.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
source_modified_at: 946684799000
file: file,fixtures/hello.txt; text/plain

HTTP 400


# C2 — far future. 4102444800000 ms = 2100-01-01Z, well past the
# one-hour slack. A fixed constant rather than a computed `now + 2h`,
# which Hurl cannot express; the assertion is unaffected by when the
# suite runs.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
source_modified_at: 4102444800000
file: file,fixtures/hello.txt; text/plain

HTTP 400


# C3 — unparseable. Refused here, unlike `X-OC-Mtime`'s lenient path:
# this field is ours and has no compatibility surface to protect, so
# garbage means a bug in our own client, not an old client to tolerate.
# The asymmetry is deliberate — see nc_webdav_client_mtime.hurl Part D4.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
source_modified_at: not-a-number
file: file,fixtures/hello.txt; text/plain

HTTP 400


# C4 — the refusals created nothing. Validation runs before the write,
# so the folder still holds only the two files from A and B. A build
# that validated afterwards would leave rows behind AND return 400,
# which is the worst of both.
GET {{base_url}}/api/folders/{{folder_id}}/resources?limit=50&resource_types=file
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP 204
