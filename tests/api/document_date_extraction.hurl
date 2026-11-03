# =============================================================
# OxiCloud – a document's own creation date, end to end
#
# `storage.content_metadata.created_at` means "the date the bytes
# claim", whatever the format — not "the EXIF capture date". This
# scenario is the proof for the non-photo half of that sentence:
# upload a document carrying a known creation date, read it back over
# `nc:creation_time`.
#
# Four formats, four places the date hides:
#
#   .docx / .xlsx / .pptx → `docProps/core.xml` → `dcterms:created`
#   .odt                  → `meta.xml`          → `meta:creation-date`
#   .pdf                  → the trailer's `/Info` → `/CreationDate`
#
# Each fixture claims a DIFFERENT instant, so reading the wrong
# document's property shows up as the wrong number instead of passing
# by luck. The PDF is the deliberate exception: it claims the same
# instant as the .docx but writes it `11:22+01'00'`, so the offset
# arithmetic is observable. Invert the sign and it reports 12:22 — an
# hour late for every European document, and plausible enough to ship.
#
# The OOXML fixtures also carry a LATER `dcterms:modified` beside the
# `dcterms:created`, under a nearly identical name. Reading that one
# instead yields a date that tracks edits, which is exactly the drift
# `nc:creation_time` refuses to report (see §0 of
# docs/plan/normalize-file-dates.md, and the `== "0"` assertion in
# nc_webdav_client_mtime.hurl for the no-claim case).
#
# Why `nc:creation_time` and not a REST field: it is the property this
# column exists to feed, and it is the only surface that exposes a
# claimed creation date today. `PhotoResourceItemDto.captured_at`
# would not do — a `.docx` is not a photo and never reaches /api/photos.
#
# Fixtures are COMMITTED (under 1.1 KB each) and built by
# `scripts/helpers/make-document-date-fixtures.py`. The same instants
# are pinned from the parser side by
# `document_date_extractor::the_committed_fixtures_claim_the_dates_the_hurl_scenario_expects`,
# so a regenerated fixture fails there first and cheaper.
#
# Extraction runs in a background task (`spawn_extraction_background`),
# hence the retries — the PROPFIND would otherwise race the upload.
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/document_date_extraction.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — JWT login, then mint an NC app password.
#
# NC DAV authenticates with Basic Auth, not the JWT bearer token, and
# `nc_username`/`nc_password` are CAPTURED here rather than configured:
# they are not in `tests/api/test.env` and cannot be, being minted per
# run. Same shape as `nc_webdav_client_mtime.hurl`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "document_date_extraction" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 1 – .docx → dcterms:created 2019-03-04T10:22:00Z
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/doc-date.docx
Content-Type: application/vnd.openxmlformats-officedocument.wordprocessingml.document
[BasicAuth]
{{nc_username}}: {{nc_password}}
file,fixtures/doc-date.docx;

HTTP *


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/doc-date.docx
Depth: 0
Content-Type: application/xml; charset=utf-8
[Options]
retry: 20
retry-interval: 250ms
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:nc="http://nextcloud.org/ns">
  <D:prop>
    <nc:creation_time/>
    <nc:upload_time/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# 2019-03-04T10:22:00Z. Integer Unix seconds, because NC clients parse
# with `(int)$value` and an RFC 3339 string silently coerces to 0.
xpath "string(//*[local-name()='creation_time'])" == "1551694920"
# NOT the `dcterms:modified` sitting next to it in the same entry.
xpath "string(//*[local-name()='creation_time'])" != "1730534400"
# NOT 0 — that is the "claims nothing" answer, and this document claims
# something. A `0` here means extraction never ran or never looked.
xpath "string(//*[local-name()='creation_time'])" != "0"
# And distinct from the receipt instant, which is now.
xpath "string(//*[local-name()='upload_time'])" != "1551694920"


# ─────────────────────────────────────────────────────────────
# Step 2 – .xlsx → 2020-06-15T08:30:00Z
#
# Same container and same entry as the .docx; a separate variant in
# `DocumentFormat` only because their TEXT lives in different places.
# Pinned separately so a dispatch that handled Word and forgot Excel
# cannot hide behind step 1.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/doc-date.xlsx
Content-Type: application/vnd.openxmlformats-officedocument.spreadsheetml.sheet
[BasicAuth]
{{nc_username}}: {{nc_password}}
file,fixtures/doc-date.xlsx;

HTTP *


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/doc-date.xlsx
Depth: 0
Content-Type: application/xml; charset=utf-8
[Options]
retry: 20
retry-interval: 250ms
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:nc="http://nextcloud.org/ns">
  <D:prop><nc:creation_time/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='creation_time'])" == "1592209800"


# ─────────────────────────────────────────────────────────────
# Step 3 – .pptx → 2022-01-20T16:05:00Z
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/doc-date.pptx
Content-Type: application/vnd.openxmlformats-officedocument.presentationml.presentation
[BasicAuth]
{{nc_username}}: {{nc_password}}
file,fixtures/doc-date.pptx;

HTTP *


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/doc-date.pptx
Depth: 0
Content-Type: application/xml; charset=utf-8
[Options]
retry: 20
retry-interval: 250ms
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:nc="http://nextcloud.org/ns">
  <D:prop><nc:creation_time/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='creation_time'])" == "1642694700"


# ─────────────────────────────────────────────────────────────
# Step 4 – .odt → meta:creation-date 2021-09-09T14:45:00
#
# ⚠ No timezone in the document, which is normal for ODF. It is read
# as UTC, never as the server's local time: these rows are keyed by
# CONTENT and shared between instances, so a server-local reading
# would make the same bytes claim different dates in two deployments.
# A CI box on UTC would never notice; a machine on CEST would.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/doc-date.odt
Content-Type: application/vnd.oasis.opendocument.text
[BasicAuth]
{{nc_username}}: {{nc_password}}
file,fixtures/doc-date.odt;

HTTP *


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/doc-date.odt
Depth: 0
Content-Type: application/xml; charset=utf-8
[Options]
retry: 20
retry-interval: 250ms
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:nc="http://nextcloud.org/ns">
  <D:prop><nc:creation_time/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='creation_time'])" == "1631198700"


# ─────────────────────────────────────────────────────────────
# Step 5 – .pdf → /CreationDate (D:20190304112200+01'00')
#
# ⚠ The offset case, and the only assertion here that can fail by an
# exact hour. 11:22 at +01:00 IS 10:22Z, so this must equal the
# .docx's instant — the offset is SUBTRACTED to reach UTC. Adding it
# gives 1551702120 (12:22Z), which is the classic sign error.
#
# This also exercises the trailer-window scan rather than a full PDF
# parse: `/Info` is uncompressed here, which is what Word,
# LibreOffice, Ghostscript and most scanners emit. A PDF 1.5+ document
# hiding `/Info` in a compressed object stream reports 0 — a
# documented blind spot, because `lopdf` is not in the tree and adding
# a PDF object parser to read one date is its own decision.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/doc-date.pdf
Content-Type: application/pdf
[BasicAuth]
{{nc_username}}: {{nc_password}}
file,fixtures/doc-date.pdf;

HTTP *


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/doc-date.pdf
Depth: 0
Content-Type: application/xml; charset=utf-8
[Options]
retry: 20
retry-interval: 250ms
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:nc="http://nextcloud.org/ns">
  <D:prop><nc:creation_time/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='creation_time'])" == "1551694920"
# ⚠ The sign error, pinned by value: +01:00 added instead of
# subtracted would report 12:22Z.
xpath "string(//*[local-name()='creation_time'])" != "1551702120"


# ═════════════════════════════════════════════════════════════
# Part B — the REST surface: /api/files/{id}/metadata
# ═════════════════════════════════════════════════════════════
# Part A read the date through `nc:creation_time`, which is a
# NextCloud-compat integer. This part reads the SAME value through the
# REST endpoint a browser client would use, where it is an RFC 3339
# string on a typed response.
#
# Three things are pinned here that Part A cannot reach:
#
#  1. The field is `created_at`. It was `captured_at` while EXIF was the
#     only writer — photo vocabulary for a value that now covers every
#     format the extractor reads, and a `.docx` is not *captured*.
#  2. It is NOT confused with `FileDto.created_at`, which is when this
#     instance received the bytes. Same spelling, different meaning, two
#     different responses: "Added" for arrival, "Created" for the claim
#     (docs/architecture/dates.md § Vocabulary).
#  3. The response is a declared schema. It used to be annotated
#     `body = Object`, so the generated SDK carried no type and the SPA
#     kept a hand-written interface with nothing checking it — which is
#     how it went on declaring `captured_at` after the rename.
#
# A document fills `created_at` and nothing else, so the media-shaped
# fields must come back null rather than absent or zero.
#
# Reuses the `jwt` captured in Setup — REST takes the bearer token,
# where the DAV half above needs the Basic Auth app password.
# ─────────────────────────────────────────────────────────────
# Upload through REST rather than looking up the DAV copy by name.
#
# Two reasons: the multipart response hands back the id directly, so no
# JSONPath filter over a listing (hurl's filter support is quirky enough
# that a non-matching expression yields nothing rather than failing) —
# and it exercises the OTHER upload path. Part A went through DAV, so
# between them both ingest routes are shown to run the extractor.
POST {{base_url}}/api/folders
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "name": "hurl-doc-dates" }

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{jwt}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/doc-date.docx; application/vnd.openxmlformats-officedocument.wordprocessingml.document

HTTP 201
[Captures]
docx_id: jsonpath "$.id"
docx_added: jsonpath "$.created_at"


GET {{base_url}}/api/files/{{docx_id}}/metadata
Authorization: Bearer {{jwt}}
[Options]
retry: 20
retry-interval: 250ms

HTTP 200
[Asserts]
# The claimed creation date, as RFC 3339 — 2019-03-04T10:22:00Z, the same
# instant Part A read as 1551694920.
jsonpath "$.created_at" == "2019-03-04T10:22:00Z"
# ⚠ The old name must be gone, not merely deprecated. Leaving both would
# let the SPA keep reading the stale one indefinitely.
jsonpath "$.captured_at" not exists
# A document fills this field and no other: present-and-null, so a client
# can tell "we looked" from "no such field".
jsonpath "$.camera_make" == null
jsonpath "$.width" == null
jsonpath "$.height" == null
# And `message` is absent when there IS something to report.
jsonpath "$.message" not exists
# ⚠ Not the file's own `created_at`, captured from the upload response
# above. `FileDto.created_at` is when this instance received the bytes —
# moments ago — while this one is 2019. Same spelling, different
# question: "Added" for arrival, "Created" for the claim. Pinned as an
# inequality as well as by value, so the two cannot quietly converge on
# one column without a named failure.
jsonpath "$.created_at" != "{{docx_added}}"


# The PDF through the same surface, for the offset arithmetic.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{jwt}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/doc-date.pdf; application/pdf

HTTP 201
[Captures]
pdf_id: jsonpath "$.id"


GET {{base_url}}/api/files/{{pdf_id}}/metadata
Authorization: Bearer {{jwt}}
[Options]
retry: 20
retry-interval: 250ms

HTTP 200
[Asserts]
# `D:20190304112200+01'00'` is 10:22Z. Reported as 11:22 or 12:22 would
# mean the offset was ignored or added instead of subtracted.
jsonpath "$.created_at" == "2019-03-04T10:22:00Z"


# ─────────────────────────────────────────────────────────────
# A PDF whose /Info is inside a COMPRESSED OBJECT STREAM.
#
# The window scan cannot see this one — `grep CreationDate` on the file
# finds nothing, because the key is deflated inside an `/ObjStm`. It is
# read by following the trailer's `/Info` reference through the xref
# stream, which needs a real PDF object model.
#
# Reported `0` until 2026-10 and was documented as a blind spot on the
# grounds that a parser would be a new dependency. It was not: lopdf was
# already compiled in as `pdf-extract`'s own dependency, just not
# importable.
#
# A distinct instant from the plain PDF above, so reading the wrong
# fixture cannot satisfy this.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{jwt}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/doc-date-objstm.pdf; application/pdf

HTTP 201
[Captures]
objstm_id: jsonpath "$.id"


GET {{base_url}}/api/files/{{objstm_id}}/metadata
Authorization: Bearer {{jwt}}
[Options]
retry: 20
retry-interval: 250ms

HTTP 200
[Asserts]
# D:20231107084500Z
jsonpath "$.created_at" == "2023-11-07T08:45:00Z"
# ⚠ Not the plain PDF's instant — that would mean the wrong fixture was
# read, and the compressed path proves nothing.
jsonpath "$.created_at" != "2019-03-04T10:22:00Z"


# ─────────────────────────────────────────────────────────────
# Teardown – the suite shares one database; leave nothing behind.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{jwt}}

HTTP *

DELETE {{base_url}}/remote.php/dav/files/{{username}}/doc-date.docx
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP *

DELETE {{base_url}}/remote.php/dav/files/{{username}}/doc-date.xlsx
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP *

DELETE {{base_url}}/remote.php/dav/files/{{username}}/doc-date.pptx
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP *

DELETE {{base_url}}/remote.php/dav/files/{{username}}/doc-date.odt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP *

DELETE {{base_url}}/remote.php/dav/files/{{username}}/doc-date.pdf
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP *


# The app password minted in Setup. Revoked last, after every request
# that authenticates with it.
DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
