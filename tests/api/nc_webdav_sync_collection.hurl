# =============================================================
# OxiCloud — NextCloud sync-collection (RFC 6578)
# =============================================================
# The NextCloud-compatible WebDAV surface's REPORT dispatch
# (report_handler.rs::handle_nc_report) previously returned an EMPTY
# multistatus for `<D:sync-collection>` bodies — it only recognized
# `oc:filter-files` and `d:searchrequest`. NC desktop/mobile clients
# commonly poll sync-collection for change detection, so this was a
# real functional gap, not spec trivia. See
# report_handler.rs::handle_sync_collection, which reuses
# WebDavAdapter::parse_sync_collection (surface-agnostic) but writes
# the response with NC's own write_folder_response/write_file_response
# so oc:fileid/favorites aren't silently dropped. Mirrors the plain
# WebDAV surface's webdav_sync_collection.hurl.
#
# NOTE: this is a full re-sync every response, not true incremental
# sync (documented limitation — no change-tracking store backs it).
# This test only asserts the spec-shaped round-trip.
#
# Coverage:
#   1. Initial sync-collection (empty sync-token) on a fresh folder
#      containing one file and one subfolder → 207, both members
#      present, a sync-token is returned.
#   2. sync-collection with a prior (arbitrary) sync-token still
#      returns the full current member list (no incremental filtering).
#   3. sync-collection with Depth: infinity → 403.
#   4. sync-collection targeting a file (not a collection) → 409.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup 1 — JWT login (to mint the app password used for NC Basic Auth).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Setup 2 — Mint an app password for NC Basic Auth.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "nc_webdav_sync_collection hurl test" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 1 — Fresh folder with one file and one subfolder.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


MKCOL {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/subfolder/
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/probe.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}
Content-Type: text/plain
```
sync probe
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 2 — Initial sync-collection (empty sync-token).
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
[BasicAuth]
{{nc_username}}: {{nc_password}}
Depth: 1
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/>
  <d:prop>
    <d:getetag/>
    <d:resourcetype/>
  </d:prop>
</d:sync-collection>
```

HTTP 207
[Asserts]
body contains "probe.txt"
body contains "subfolder"
xpath "string(//*[local-name()='sync-token'])" != ""
[Captures]
first_sync_token: xpath "string(//*[local-name()='sync-token'])"


# ─────────────────────────────────────────────────────────────
# Step 3 — sync-collection with a prior token still returns the
#          full current member list (no incremental filtering).
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
[BasicAuth]
{{nc_username}}: {{nc_password}}
Depth: 1
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token>{{first_sync_token}}</d:sync-token>
  <d:prop>
    <d:getetag/>
  </d:prop>
</d:sync-collection>
```

HTTP 207
[Asserts]
body contains "probe.txt"
body contains "subfolder"


# ─────────────────────────────────────────────────────────────
# Step 4 — Depth: infinity is rejected.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
[BasicAuth]
{{nc_username}}: {{nc_password}}
Depth: infinity
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>
```

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 5 — sync-collection on a file (not a collection) → 409.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/probe.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}
Depth: 1
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>
```

HTTP 409


# ─────────────────────────────────────────────────────────────
# Cleanup
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}
HTTP 200
