# =============================================================
# OxiCloud — WebDAV quota properties (RFC 4331)
# =============================================================
# `DAV:quota-available-bytes` / `DAV:quota-used-bytes` are resolved once
# per PROPFIND request via `AppState::resolve_webdav_quota` — see
# webdav_handler.rs / webdav_adapter.rs::write_quota_props.
# Unlimited accounts/drives (quota <= 0 or unset) omit
# quota-available-bytes entirely per RFC 4331 §3, rather than reporting
# a sentinel value.
#
# Coverage:
#   1. Named-prop PROPFIND for both properties on the WebDAV root → 207,
#      both present with numeric values.
#   2. allprop PROPFIND also includes both properties.
#   3. quota-used-bytes increases by (at least) the size of a file
#      just uploaded through WebDAV.
#   4. A SHARED drive with its own finite quota reports THAT quota on
#      `/webdav/@drive/<id>/`, distinct from the caller's personal
#      envelope — the drive-awareness fix (previously `resolve_quota`
#      ignored `drive_id` entirely and always reported the envelope).
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
# Step 2 — Named-prop PROPFIND for the two quota properties.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{token}}
Depth: 0
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:quota-available-bytes/>
    <D:quota-used-bytes/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='propstat'][1]/*[local-name()='status'])" contains "200 OK"
xpath "number(//*[local-name()='quota-used-bytes'])" >= 0
xpath "number(//*[local-name()='quota-available-bytes'])" > 0


# ─────────────────────────────────────────────────────────────
# Step 3 — allprop PROPFIND also surfaces both properties.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{token}}
Depth: 0
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:allprop/>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "number(//*[local-name()='quota-used-bytes'])" >= 0
xpath "number(//*[local-name()='quota-available-bytes'])" > 0
[Captures]
used_before: xpath "number(//*[local-name()='quota-used-bytes'])"


# ─────────────────────────────────────────────────────────────
# Step 4 — Upload a file, then confirm quota-used-bytes reflects it.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/quota-probe.txt
Authorization: Bearer {{token}}
Content-Type: text/plain
```
quota accounting probe payload
```

HTTP 201


PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{token}}
Depth: 0
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:quota-used-bytes/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "number(//*[local-name()='quota-used-bytes'])" >= {{used_before}}


# ─────────────────────────────────────────────────────────────
# Cleanup
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/webdav/quota-probe.txt
Authorization: Bearer {{token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 5 — Shared drive with its own finite quota reports THAT
#          quota, not the owner's personal envelope.
#
#          Provision a fresh user + a 500-byte shared drive owned
#          by them, then PROPFIND `/webdav/@drive/<id>/` (see
#          `webdav_drive_root.hurl` for the URL-scheme contract).
#          A brand-new drive has `used_bytes == 0`, so
#          quota-available-bytes must equal the quota exactly —
#          a value that cannot coincide with the personal envelope
#          asserted above (that account already had files on it
#          from earlier steps).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "username": "wq_owner",
  "password": "WqOwnerPwd1!",
  "email":    "wq_owner@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "wq_owner", "password": "WqOwnerPwd1!" }

HTTP 200
[Captures]
wq_owner_token: jsonpath "$.access_token"
wq_owner_id: jsonpath "$.user.full.user.id"


POST {{base_url}}/api/drives
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "wq-shared",
  "owner": { "type": "user", "id": "{{wq_owner_id}}" },
  "quota_bytes": 500
}

HTTP 201
[Captures]
wq_drive_id: jsonpath "$.id"


PROPFIND {{base_url}}/webdav/@drive/{{wq_drive_id}}/
Authorization: Bearer {{wq_owner_token}}
Depth: 0
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:quota-available-bytes/>
    <D:quota-used-bytes/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "number(//*[local-name()='quota-used-bytes'])" == 0
xpath "number(//*[local-name()='quota-available-bytes'])" == 500


# Upload a 32-byte file into the shared drive; quota-used-bytes must
# reflect it and quota-available-bytes must shrink accordingly —
# confirms the shared-drive branch reads `storage.drives` live, not
# a cached/stale value.
PUT {{base_url}}/webdav/@drive/{{wq_drive_id}}/quota-probe.txt
Authorization: Bearer {{wq_owner_token}}
Content-Type: text/plain
```
32-byte-ish payload for drv
```

HTTP 201


# The drive-usage bump is fire-and-forget on a tokio task (see
# `file_upload_service.rs::maybe_update_storage_usage`), so the SQL
# UPDATE may not have landed yet when the PUT above returned. Retry
# the PROPFIND until `used_bytes` catches up — same shape as
# `drive_quota.hurl` Step 5.
PROPFIND {{base_url}}/webdav/@drive/{{wq_drive_id}}/
Authorization: Bearer {{wq_owner_token}}
Depth: 0
Content-Type: application/xml; charset=utf-8
[Options]
retry: 10
retry-interval: 200ms
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:quota-available-bytes/>
    <D:quota-used-bytes/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "number(//*[local-name()='quota-used-bytes'])" > 0
xpath "number(//*[local-name()='quota-available-bytes'])" < 500


# No further cleanup needed — `tests/api/storage_cleanup_check.sh`
# drains/deletes every non-admin-default drive at suite end.
