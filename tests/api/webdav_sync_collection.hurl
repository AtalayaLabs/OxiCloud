# =============================================================
# OxiCloud — WebDAV sync-collection (RFC 6578) for the file surface
# =============================================================
# Real incremental sync, backed by `storage.folder_sync_changes`
# (see migrations/20260911000000_folder_sync_changes.sql,
# WebdavSyncCollectionService, webdav_handler.rs::handle_report).
#
# Coverage:
#   1. Initial sync-collection (empty sync-token) on a fresh folder
#      containing one file and one subfolder → 207, both members
#      present, a sync-token is returned.
#   2. sync-collection with that token and NO intervening changes →
#      207, empty delta (neither prior member reappears).
#   3. Create a new file, then sync-collection with the prior token →
#      only the new file appears, not the untouched members.
#   4. Delete that new file, then sync-collection with the
#      just-issued token → an RFC 6578 §3.7 404 sub-response for it.
#   5. sync-collection with Depth: infinity → 403.
#   6. sync-collection targeting a file (not a collection) → 409.
#   7. sync-collection with a malformed sync-token → 400.
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


# ─────────────────────────────────────────────────────────────
# Step 2 — Fresh folder with one file and one subfolder.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/webdav/sync-test-folder/
Authorization: Bearer {{token}}

HTTP 201


MKCOL {{base_url}}/webdav/sync-test-folder/subfolder/
Authorization: Bearer {{token}}

HTTP 201


PUT {{base_url}}/webdav/sync-test-folder/probe.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
```
sync probe
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 3 — Initial sync-collection (empty sync-token).
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/webdav/sync-test-folder/
Authorization: Bearer {{token}}
Depth: 1
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token/>
  <D:prop>
    <D:getetag/>
    <D:resourcetype/>
  </D:prop>
</D:sync-collection>
```

HTTP 207
[Asserts]
body contains "probe.txt"
body contains "subfolder"
xpath "string(//*[local-name()='sync-token'])" != ""
[Captures]
first_sync_token: xpath "string(//*[local-name()='sync-token'])"


# ─────────────────────────────────────────────────────────────
# Step 4 — sync-collection with that token and NO intervening
#          changes → empty delta (real incremental sync, not a
#          full re-sync).
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/webdav/sync-test-folder/
Authorization: Bearer {{token}}
Depth: 1
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token>{{first_sync_token}}</D:sync-token>
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:sync-collection>
```

HTTP 207
[Asserts]
body not contains "probe.txt"
body not contains "subfolder"
xpath "string(//*[local-name()='sync-token'])" != ""


# ─────────────────────────────────────────────────────────────
# Step 5 — Create a new file, then sync-collection with the prior
#          token → only the new file appears in the delta.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/sync-test-folder/new-file.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
```
new file after first sync
```

HTTP 201


REPORT {{base_url}}/webdav/sync-test-folder/
Authorization: Bearer {{token}}
Depth: 1
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token>{{first_sync_token}}</D:sync-token>
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:sync-collection>
```

HTTP 207
[Asserts]
body contains "new-file.txt"
body not contains "probe.txt"
body not contains "subfolder"
[Captures]
second_sync_token: xpath "string(//*[local-name()='sync-token'])"


# ─────────────────────────────────────────────────────────────
# Step 6 — Delete that new file, then sync-collection with the
#          just-issued token → RFC 6578 §3.7 404 sub-response.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/webdav/sync-test-folder/new-file.txt
Authorization: Bearer {{token}}

HTTP 204


REPORT {{base_url}}/webdav/sync-test-folder/
Authorization: Bearer {{token}}
Depth: 1
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token>{{second_sync_token}}</D:sync-token>
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:sync-collection>
```

HTTP 207
[Asserts]
body contains "new-file.txt"
xpath "string(//*[local-name()='response'][contains(., 'new-file.txt')]/*[local-name()='status'])" contains "404"


# ─────────────────────────────────────────────────────────────
# Step 7 — Depth: infinity is rejected.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/webdav/sync-test-folder/
Authorization: Bearer {{token}}
Depth: infinity
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token/>
  <D:prop><D:getetag/></D:prop>
</D:sync-collection>
```

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 8 — sync-collection on a file (not a collection) → 409.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/webdav/sync-test-folder/probe.txt
Authorization: Bearer {{token}}
Depth: 1
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token/>
  <D:prop><D:getetag/></D:prop>
</D:sync-collection>
```

HTTP 409


# ─────────────────────────────────────────────────────────────
# Step 9 — Malformed sync-token → 400.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/webdav/sync-test-folder/
Authorization: Bearer {{token}}
Depth: 1
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token>not-a-real-token</D:sync-token>
  <D:prop><D:getetag/></D:prop>
</D:sync-collection>
```

HTTP 400


# ─────────────────────────────────────────────────────────────
# Cleanup
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/webdav/sync-test-folder/
Authorization: Bearer {{token}}

HTTP 204
