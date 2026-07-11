# =============================================================
# OxiCloud — WebDAV sync-collection (RFC 6578) for the file surface
# =============================================================
# The plain-file WebDAV surface previously had no REPORT support at
# all — sync-collection/sync-token were CalDAV/CardDAV-only. See
# webdav_adapter.rs::parse_sync_collection /
# generate_sync_collection_response and
# webdav_handler.rs::handle_report.
#
# NOTE: this is a full re-sync every response, not true incremental
# sync (documented limitation — no change-tracking store backs it,
# matching the existing CalDAV/CardDAV sync-collection REPORTs). This
# test only asserts the spec-shaped round-trip: REPORT works, returns
# every current member plus a sync-token, and Depth: infinity is
# rejected.
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
# Step 4 — sync-collection with a prior token still returns the
#          full current member list (no incremental filtering).
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
body contains "probe.txt"
body contains "subfolder"


# ─────────────────────────────────────────────────────────────
# Step 5 — Depth: infinity is rejected.
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
# Step 6 — sync-collection on a file (not a collection) → 409.
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
# Cleanup
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/webdav/sync-test-folder/
Authorization: Bearer {{token}}

HTTP 204
