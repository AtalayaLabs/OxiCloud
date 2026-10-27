# =============================================================
# OxiCloud — a folder's getlastmodified is the SAME by REPORT and PROPFIND
# =============================================================
# Plan §2's last open gap, closed 2026-10-27.
#
# For a collection, `D:getlastmodified` means the SUBTREE — the newest
# content change anywhere beneath the folder — not the folder row's own
# mtime. Otherwise renaming a folder announces a content change and every
# sync client re-walks it. See `docs/architecture/dates.md` § Collections.
#
# PROPFIND got that right. The NC search REPORT did not: its renderer had
# no subtree value to use, so `folder_dto_from_search` fell back to the
# folder's own `modified_at`. Same folder, same property, two answers
# depending on how the client asked.
#
# ── The invariant this pins ───────────────────────────────────────────
# Not a literal timestamp — that would be a clock race. The assertion is
# an EQUALITY between the two methods on the same folder, which is the
# actual contract and cannot pass by coincidence once the two diverge.
#
# To make them diverge if the fix regresses, the folder is RENAMED after
# its child is created. A rename bumps the folder row's `modified_at` and
# must NOT bump the subtree rollup — so after it:
#
#   tree_modified_at  = when the child landed   ← both must report this
#   modified_at       = when the rename happened
#
# Before the fix, REPORT reported the rename instant and PROPFIND the
# child instant, so the equality below failed. The ETag is derived from
# the same rollup, so it is asserted equal too — it is the load-bearing
# half, the one sync clients actually act on.
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/nc_report_folder_tree_clock.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — JWT, then an NC app password (DAV uses Basic Auth).
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
{ "label": "nc_report_folder_tree_clock" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 1 — A folder with a child, so the subtree clock has a value
#          distinct from anything the folder row will carry.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/remote.php/dav/files/{{username}}/treeclock-before
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP *


PUT {{base_url}}/remote.php/dav/files/{{username}}/treeclock-before/child.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
file,fixtures/hello.txt;

HTTP *


# ─────────────────────────────────────────────────────────────
# Step 2 — Rename the folder.
#
# This is what separates the two clocks. The rename touches the folder
# ROW (`modified_at`) and must leave the subtree rollup alone, because
# no content beneath it changed. A renderer that falls back to
# `modified_at` now reports a different instant from one that uses the
# rollup — which is exactly the divergence being guarded.
# ─────────────────────────────────────────────────────────────
MOVE {{base_url}}/remote.php/dav/files/{{username}}/treeclock-before
Destination: {{base_url}}/remote.php/dav/files/{{username}}/treeclock-probe
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP *


# ─────────────────────────────────────────────────────────────
# Step 3 — PROPFIND the folder. This path has always been right;
#          capture its answer as the reference.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/treeclock-probe
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getlastmodified/>
    <D:getetag/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Captures]
propfind_mtime: xpath "string((//*[local-name()='getlastmodified'])[1])"
propfind_etag: xpath "string((//*[local-name()='getetag'])[1])"
[Asserts]
# Guard the capture itself: an empty string would make the equality
# below pass vacuously.
xpath "string((//*[local-name()='getlastmodified'])[1])" != ""


# ─────────────────────────────────────────────────────────────
# Step 4 — ⚠ The same folder via the NC search REPORT.
#
# `d:searchrequest` with a `<d:literal>` is the shape the NC desktop
# client sends. The folder matches on name.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{username}}/
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<d:searchrequest xmlns:d="DAV:">
  <d:basicsearch>
    <d:select><d:prop><d:getlastmodified/><d:getetag/></d:prop></d:select>
    <d:from><d:scope><d:href>/remote.php/dav/files/{{username}}/</d:href><d:depth>infinity</d:depth></d:scope></d:from>
    <d:where><d:like><d:prop><d:displayname/></d:prop><d:literal>%treeclock-probe%</d:literal></d:like></d:where>
    <d:limit><d:nresults>20</d:nresults></d:limit>
  </d:basicsearch>
</d:searchrequest>
```

HTTP 207
[Asserts]
# The folder is in the results at all — otherwise everything below is
# vacuous.
xpath "string(//*[local-name()='response'][contains(string(*[local-name()='href']), 'treeclock-probe')]/*[local-name()='href'])" contains "treeclock-probe"

# ⚠ THE INVARIANT. The subtree clock REPORT reports must equal the one
# PROPFIND reports, for the same folder. Before the fix this read the
# rename instant from step 2 while PROPFIND read the child instant from
# step 1, so the two differed.
xpath "string(//*[local-name()='response'][contains(string(*[local-name()='href']), 'treeclock-probe')]//*[local-name()='getlastmodified'])" == "{{propfind_mtime}}"

# And the ETag, which is the load-bearing half — it is derived from the
# same rollup, and it is what a sync client actually acts on. If these
# two ever disagree, a client alternating between search and listing
# re-downloads the subtree.
xpath "string(//*[local-name()='response'][contains(string(*[local-name()='href']), 'treeclock-probe')]//*[local-name()='getetag'])" == "{{propfind_etag}}"


# ─────────────────────────────────────────────────────────────
# Teardown – the suite shares one database; leave nothing behind.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/remote.php/dav/files/{{username}}/treeclock-probe
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP *


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
