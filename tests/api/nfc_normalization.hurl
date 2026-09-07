# =============================================================
# OxiCloud — NFC normalization on write (regression pin)
# =============================================================
# Regression pin for AtalayaLabs/OxiCloud#706. The bug: macOS Finder
# and other NFD-emitting clients uploaded folder names in decomposed
# form ("à" = "a" + U+0300 combining grave, 2 codepoints), which
# landed raw in storage.folders.name / storage.files.name. Clients
# doing NFC-normalized lookups (NextCloud desktop, DAVX5, well-
# behaved sync clients — the exact clients the reporter used) then
# failed to descend into or match their own uploads.
#
# The fix normalizes at the repository layer (folder_db_repository,
# file_blob_write_repository) so every write surface — REST, WebDAV,
# NextCloud DAV, batch, chunked, WOPI-fallback — is covered at one
# choke point. WebDAV/NC MKCOL/PUT handlers additionally emit
# `Content-Location` (RFC 7231 §3.1.4.2) when canonicalisation
# actually changed the URL, so well-behaved clients update their
# local index immediately without waiting for the next PROPFIND
# cycle.
#
# This file pins the three critical write paths from the reporter's
# scenario:
#   1. REST `POST /api/folders`      — the browser-side upload path
#   2. WebDAV `MKCOL`                — davfs / macOS Finder
#   3. NextCloud `MKCOL`             — NC desktop client / DAVX5
#
# For each: post NFD, expect the DB row to hold NFC, and for the
# DAV surfaces expect `Content-Location` pointing at the canonical
# NFC URL, plus a follow-up NFC-URL lookup that succeeds (proving
# the two clients-and-server sides agree on the canonical form
# after the fix).
#
# Byte encoding conventions used below:
#   * NFD `à` = U+0061 U+0300 → UTF-8 `61 CC 80` → URL `a%CC%80`
#   * NFC `à` = U+00E0        → UTF-8 `C3 A0`    → URL `%C3%A0`
# JSON bodies use `̀` (JSON-standard Unicode escape, always
# interpreted by the server's JSON parser). Assertions use Hurl's
# `\u{HHHH}` escape for the expected NFC codepoint.
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
# Step 2 — Resolve admin's home folder id (the default parent
# for REST folder create when no parent_id is supplied, but we
# pass it explicitly so this test doesn't depend on the auto-
# resolve fallback path).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 3 — REST `POST /api/folders` with an NFD name in the
# JSON body. The name field carries "nfc-rest-à" — that
# is the 11-byte NFD form ("nfc-rest-a" + U+0300). Post-fix,
# the repo NFC-normalizes at bind time, so the returned name
# must be the 10-byte NFC form "nfc-rest-\u{00e0}".
#
# Pre-fix: the response would echo the NFD input verbatim
# (`nfc-rest-à`), the DB would store 11 bytes, and a
# subsequent PROPFIND from an NFC-normalizing client would
# miss. That's the class of bug #706 reports.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "name": "nfc-rest-à",
  "parent_id": "{{home_id}}"
}

HTTP 201
[Captures]
rest_folder_id: jsonpath "$.id"
[Asserts]
# The stored (and returned) name must be NFC. If this fails, the
# repo-level normalize call was skipped or bypassed by a new code
# path — see folder_db_repository::create_folder.
jsonpath "$.name" == "nfc-rest-\u{00e0}"


# ─────────────────────────────────────────────────────────────
# Step 4 — Follow-up GET verifies the DB persisted NFC (not just
# that the create response happened to canonicalise before
# echoing). Reads from the same row a client's PROPFIND would.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{rest_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.name" == "nfc-rest-\u{00e0}"


# ─────────────────────────────────────────────────────────────
# Step 5 — Cleanup the REST-created folder before moving on to
# the DAV surfaces. Ed's memory feedback_hurl_teardown_shared_db:
# hurl files share the DB across the suite; each file must clean
# up what it created.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{rest_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP *


# ─────────────────────────────────────────────────────────────
# Step 6 — WebDAV `MKCOL` with an NFD folder name in the URL
# path. Sends `nfc-dav-a%CC%80/` — the URL-encoded NFD form
# ("nfc-dav-a" + %CC%80 for U+0300). Post-fix, the handler
# canonicalises and emits `Content-Location` pointing at the
# NFC URL. Naive clients ignore the header (status stays 201);
# well-behaved clients update their local index to the
# canonical URL immediately.
#
# Bare `/webdav/<name>/` (no `@drive/<selector>/` prefix) maps
# to the caller's default drive contents — matches the shape
# `webdav_drive_root.hurl` Step 4 documents. Simpler than the
# picker, and covers the same code path (`handle_mkcol` runs
# either way; the last URL segment is what the normalize
# operates on).
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/webdav/nfc-dav-a%CC%80/
Authorization: Bearer {{admin_token}}

HTTP 201
[Asserts]
# The canonical URL substitutes the NFC form (%C3%A0) for the
# NFD segment in the request. If Content-Location is missing or
# still contains %CC%80, either the handler skipped the
# normalize-and-diff or repo-level canonicalization didn't fire.
header "Content-Location" contains "%C3%A0"
header "Content-Location" not contains "%CC%80"


# ─────────────────────────────────────────────────────────────
# Step 7 — PROPFIND on the CANONICAL (NFC) URL confirms the
# folder is reachable there. This is what a well-behaved sync
# client does on its next cycle after consuming Content-Location
# — and what the reporter's macOS/Android clients were doing
# already, hence their failure to find their own NFD uploads.
# Post-fix, this must return a matching 207.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/nfc-dav-%C3%A0/
Authorization: Bearer {{admin_token}}
Depth: 0
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/><D:resourcetype/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# The response body must reference the canonical URL exactly.
body contains "nfc-dav-%C3%A0"


# ─────────────────────────────────────────────────────────────
# Step 8 — PROPFIND on the ORIGINAL (NFD) URL returns 404. The
# server does not maintain a legacy-NFD-alias for post-fix rows
# — the canonical URL is the only one that resolves. This pins
# the intended one-way behaviour (write NFD → stored NFC →
# only NFC URL matches), which is exactly what NFC-normalizing
# clients want.
#
# (Pre-existing NFD rows in the DB, deliberately not touched by
# this fix per operator decision, remain reachable via their
# NFD URL. That's a separate scenario — historic content, not
# the write-time regression this file covers.)
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/nfc-dav-a%CC%80/
Authorization: Bearer {{admin_token}}
Depth: 0
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:resourcetype/></D:prop>
</D:propfind>
```

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 9 — Cleanup the WebDAV-created folder.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/webdav/nfc-dav-%C3%A0/
Authorization: Bearer {{admin_token}}

HTTP *


# ─────────────────────────────────────────────────────────────
# Step 10 — Mint an app-password for NextCloud Basic Auth.
#
# NextCloud DAV endpoints (`/remote.php/dav/…`) never accept a
# plain JWT — they use HTTP Basic Auth with an app-password,
# same pattern as every existing `nc_*.hurl` test. Mint one
# here so steps 11-13 below can authenticate.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "label": "nfc_normalization hurl test" }

HTTP 200
[Captures]
nc_user:    jsonpath "$.username"
nc_pw:      jsonpath "$.password"
nc_pw_id:   jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 11 — NextCloud `MKCOL` with NFD name in the URL path.
# Same shape as WebDAV MKCOL but on the /remote.php/dav/files/
# surface — this is the code path macOS-based NC desktop
# clients hit. Same Content-Location contract.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/remote.php/dav/files/{{username}}/nfc-nc-a%CC%80/
[BasicAuth]
{{nc_user}}: {{nc_pw}}

HTTP 201
[Asserts]
header "Content-Location" contains "%C3%A0"
header "Content-Location" not contains "%CC%80"


# ─────────────────────────────────────────────────────────────
# Step 12 — PROPFIND via NextCloud DAV on the canonical URL.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/nfc-nc-%C3%A0/
Depth: 0
Content-Type: application/xml
[BasicAuth]
{{nc_user}}: {{nc_pw}}
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/><D:resourcetype/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body contains "nfc-nc-%C3%A0"


# ─────────────────────────────────────────────────────────────
# Step 13 — Cleanup the NC-created folder + retire the
# app-password so this test file leaves no side effects
# behind (per feedback_hurl_teardown_shared_db).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/remote.php/dav/files/{{username}}/nfc-nc-%C3%A0/
[BasicAuth]
{{nc_user}}: {{nc_pw}}

HTTP *


DELETE {{base_url}}/api/auth/app-passwords/{{nc_pw_id}}
Authorization: Bearer {{admin_token}}

HTTP *
