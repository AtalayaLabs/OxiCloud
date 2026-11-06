# =============================================================
# OxiCloud — NC write responses carry OC-FileId + X-OC-MTime
# =============================================================
# Regression found by tests/nextcloudcmd (2026-10-22): the NC
# chunked-upload assembly MOVE answered 201 with ETag/OC-ETag but
# no `OC-FileId`. The Nextcloud desktop sync engine (3.16) treats
# that as "Missing File ID from server" and marks every chunked
# upload failed — although the server had committed the file. Plain
# PUTs lacked the header too, and none of the write paths answered
# `X-OC-MTime: accepted`, so the client logged "Server does not
# support X-OC-MTime" while the mtime had in fact been applied.
#
# Both live in `webdav_handler::with_nc_write_headers`, shared by
# PUT, PATCH and `uploads_handler::handle_assemble`. Pinned here:
#   - OC-FileId is present and equals PROPFIND's `oc:id` (the client
#     compares them — a different format is as bad as none);
#   - X-OC-MTime: accepted iff the stored mtime IS the requested one.
#     Creates and overwrites both apply X-OC-Mtime, so both claim it;
#     a write that sent no X-OC-Mtime must not.
# =============================================================

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "nc_write_response_headers hurl test" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# =============================================================
# A. Plain PUT
# =============================================================

# A1 — Create with X-OC-Mtime: OC-FileId in oc:id form, mtime
# accepted.
PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-write-headers-put.txt
X-OC-Mtime: 1600000000
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
plain put body
```

HTTP 201
[Asserts]
header "OC-ETag" exists
header "OC-FileId" matches /^[0-9]{8,}[a-z0-9]+$/
header "X-OC-MTime" == "accepted"
[Captures]
put_file_id: header "OC-FileId"


# A2 — The header names the same object PROPFIND does.
PROPFIND {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-write-headers-put.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:prop><oc:id/><d:getlastmodified/></d:prop>
</d:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='id'])" == "{{put_file_id}}"


# A3 — Overwrite with X-OC-Mtime: same file id, mtime accepted…
PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-write-headers-put.txt
X-OC-Mtime: 1600000000
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
plain put body, v2
```

HTTP 204
[Asserts]
header "OC-FileId" == "{{put_file_id}}"
header "X-OC-MTime" == "accepted"


# A4 — …and actually stored (1600000000 = 13 Sep 2020 12:26:40 UTC).
PROPFIND {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-write-headers-put.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:getlastmodified/></d:prop>
</d:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='getlastmodified'])" contains "13 Sep 2020 12:26:40"


# A5 — Overwrite without X-OC-Mtime: same file id, and the server
# must not claim to have accepted an mtime nobody sent.
PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-write-headers-put.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
plain put body, overwritten
```

HTTP 204
[Asserts]
header "OC-FileId" == "{{put_file_id}}"
header "X-OC-MTime" not exists


# =============================================================
# B. Chunked upload (NC chunking v2) — the path the desktop client
#    failed on.
# =============================================================

MKCOL {{base_url}}/remote.php/dav/uploads/{{nc_username}}/nc-write-headers-session
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


PUT {{base_url}}/remote.php/dav/uploads/{{nc_username}}/nc-write-headers-session/00001
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
first chunk,
```

HTTP 201


PUT {{base_url}}/remote.php/dav/uploads/{{nc_username}}/nc-write-headers-session/00002
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
second chunk
```

HTTP 201


# B1 — Assembly MOVE: the response the client needs OC-FileId on.
# New destination with X-OC-Mtime: accepted, same as A1.
MOVE {{base_url}}/remote.php/dav/uploads/{{nc_username}}/nc-write-headers-session/.file
Destination: {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-write-headers-chunked.txt
X-OC-Mtime: 1600000000
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201
[Asserts]
header "OC-ETag" exists
header "OC-FileId" matches /^[0-9]{8,}[a-z0-9]+$/
header "OC-FileId" != "{{put_file_id}}"
header "X-OC-MTime" == "accepted"
[Captures]
chunked_file_id: header "OC-FileId"


# B2 — Same object as PROPFIND reports.
PROPFIND {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-write-headers-chunked.txt
Depth: 0
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:prop><oc:id/><d:getlastmodified/></d:prop>
</d:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='id'])" == "{{chunked_file_id}}"


# =============================================================
# Teardown
# =============================================================
DELETE {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-write-headers-put.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-write-headers-chunked.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
