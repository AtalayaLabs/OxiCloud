# =============================================================
# OxiCloud — a rename must not change a file's ETag
# =============================================================
# This is the payoff of the whole dates plan, and until now nothing
# asserted it.
#
# `File::compute_etag` is `{blob_hash[..16]}-{content_modified_at}`.
# Before §2 the second input was `updated_at`, which a rename bumps —
# so renaming a file changed its ETag, every sync client saw a
# "modified" resource, and every one of them re-downloaded bytes that
# had not moved. On a large shared drive a bulk rename meant a bulk
# re-transfer.
#
# After §2 a rename touches neither input: the blob is untouched and
# `content_modified_at` is only written by content-mutation paths. The
# ETag must therefore be **byte-identical** across a rename.
#
# ── Why the complement is asserted too ────────────────────────────────
# "ETag did not change" is trivially satisfiable by an ETag that never
# changes at all, which would be a far worse bug — clients would stop
# downloading real edits and silently serve stale content forever. Part
# C proves the ETag still moves when the bytes do, so Part B's
# stability means "correctly stable" rather than "frozen".
#
# ── Why X-OC-Mtime seeds the file ─────────────────────────────────────
# It pins `content_modified_at` to a known past instant, so the ETag
# suffix is deterministic and the test never races the wall clock.
#
# Runs on the NC surface because that is the only one accepting
# `X-OC-Mtime`; the ETag formula itself is shared with plain `/webdav/`
# and the REST listing (see `webdav_etag_cross_check.hurl`, which pins
# that the three surfaces agree — a different property from this one).
#
# Self-contained: deletes its probe file and app password in teardown.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup
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
{ "label": "webdav_etag_rename_stability" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# ═════════════════════════════════════════════════════════════
# Part A — seed with a known content clock, capture the ETag
# ═════════════════════════════════════════════════════════════
PUT {{base_url}}/remote.php/dav/files/{{username}}/etag-rename-probe.txt
Content-Type: text/plain
X-OC-Mtime: 1700086400
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
bytes that will not change during the rename
```

HTTP 201


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/etag-rename-probe.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getetag/>
    <D:getlastmodified/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Captures]
etag_before: xpath "string(//*[local-name()='getetag'])"
[Asserts]
xpath "string(//*[local-name()='getetag'])" exists
# The suffix is the content clock in epoch seconds, so the asserted
# mtime from the PUT above must be the one feeding it. If this drifts,
# the stability assertion below would be comparing the wrong thing.
xpath "string(//*[local-name()='getetag'])" contains "1700086400"
xpath "string(//*[local-name()='getlastmodified'])" matches "^Wed, 15 Nov 2023 22:13:20 (GMT|\\+0000)$"


# ═════════════════════════════════════════════════════════════
# Part B — rename: the ETag must be byte-identical
# ═════════════════════════════════════════════════════════════
MOVE {{base_url}}/remote.php/dav/files/{{username}}/etag-rename-probe.txt
Destination: {{base_url}}/remote.php/dav/files/{{username}}/etag-rename-probe-2.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/etag-rename-probe-2.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# The assertion this file exists for. A regression here means every
# sync client re-downloads every renamed file.
xpath "string(//*[local-name()='getetag'])" == "{{etag_before}}"


# ═════════════════════════════════════════════════════════════
# Part C — a real content change must still move the ETag
# ═════════════════════════════════════════════════════════════
# Without this, an ETag hard-coded to a constant would pass Part B.
# Both inputs move here: the bytes differ, so `blob_hash` changes, and
# no `X-OC-Mtime` is sent, so `content_modified_at` becomes NOW().
PUT {{base_url}}/remote.php/dav/files/{{username}}/etag-rename-probe-2.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
genuinely different bytes
```

HTTP 204


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/etag-rename-probe-2.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='getetag'])" != "{{etag_before}}"
# And specifically no longer pinned to the seeded instant — the content
# clock moved to NOW() with the bytes.
xpath "string(//*[local-name()='getetag'])" not contains "1700086400"


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/remote.php/dav/files/{{username}}/etag-rename-probe-2.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
