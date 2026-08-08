# =============================================================
# OxiCloud — NextCloud sync-collection (RFC 6578)
# =============================================================
# Real incremental sync on the NC surface too — same change log
# (`storage.folder_sync_changes`) the plain-WebDAV surface uses, via
# `WebdavSyncCollectionService`. See
# report_handler.rs::handle_sync_collection. Mirrors the plain WebDAV
# surface's webdav_sync_collection.hurl.
#
# Coverage:
#   1. Initial sync-collection (empty sync-token) on a fresh folder
#      containing one file and one subfolder → 207, both members
#      present, a sync-token is returned.
#   2. sync-collection with that token and NO intervening changes →
#      207, empty delta.
#   3. Create a new file, then sync-collection with the prior token →
#      only the new file appears.
#   4. Delete that new file, then sync-collection with the
#      just-issued token → an RFC 6578 §3.7 404 sub-response for it.
#   5. sync-collection with Depth: infinity → 403.
#   6. sync-collection targeting a file (not a collection) → 409.
#   7. sync-collection with a malformed sync-token → 400.
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
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
sync probe
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 2 — Initial sync-collection (empty sync-token).
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
Depth: 1
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
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
# Step 3 — sync-collection with that token and NO intervening
#          changes → empty delta.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
Depth: 1
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
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
body not contains "probe.txt"
body not contains "subfolder"
xpath "string(//*[local-name()='sync-token'])" != ""


# ─────────────────────────────────────────────────────────────
# Step 4 — Create a new file, then sync-collection with the prior
#          token → only the new file appears in the delta.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/new-file.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
new file after first sync
```

HTTP 201


REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
Depth: 1
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
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
body contains "new-file.txt"
body not contains "probe.txt"
body not contains "subfolder"
[Captures]
second_sync_token: xpath "string(//*[local-name()='sync-token'])"


# ─────────────────────────────────────────────────────────────
# Step 5 — Delete that new file, then sync-collection with the
#          just-issued token → RFC 6578 §3.7 404 sub-response.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/new-file.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
Depth: 1
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token>{{second_sync_token}}</d:sync-token>
  <d:prop>
    <d:getetag/>
  </d:prop>
</d:sync-collection>
```

HTTP 207
[Asserts]
body contains "new-file.txt"
xpath "string(//*[local-name()='response'][contains(., 'new-file.txt')]/*[local-name()='status'])" contains "404"


# ─────────────────────────────────────────────────────────────
# Step 6 — Depth: infinity is rejected.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
Depth: infinity
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>
```

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 7 — sync-collection on a file (not a collection) → 409.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/probe.txt
Depth: 1
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>
```

HTTP 409


# ─────────────────────────────────────────────────────────────
# Step 8 — Malformed sync-token → 400.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-sync-test-folder/
Depth: 1
Content-Type: application/xml; charset=utf-8
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token>not-a-real-token</d:sync-token>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>
```

HTTP 400


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
