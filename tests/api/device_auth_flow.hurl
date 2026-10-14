# =============================================================
# OxiCloud — Device-authorization grant (RFC 8628) end-to-end
# =============================================================
# The device-auth endpoints had ZERO Hurl coverage before Phase 4
# because the Rust handlers lacked `#[utoipa::path]` annotations
# and therefore no generated SDK existed — the FE wrapper was the
# only consumer, and even that only exercised the two verify paths.
#
# This file closes the gap by driving two chains that together cover
# all 6 handlers:
#
#   Chain A — client picks up the tokens (the happy path):
#       authorize → poll(pending) → verify(info) → verify(approve) → poll(success)
#
#   Chain B — admin manages the list / revokes (device code is never
#   polled, so the row stays in the DB for list/revoke to see — a
#   successful poll deletes the row, by design, to avoid lingering
#   plaintext tokens in storage):
#       authorize → verify(approve) → list_devices → revoke → revoke(404)
#
# The two chains use separate device codes so chain A's `delete after
# poll` doesn't leave chain B with nothing to list.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — owner login (acts as the human approving the device).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
owner_token: jsonpath "$.access_token"


# ═════════════════════════════════════════════════════════════
# CHAIN A — full authorize → approve → poll(success) handshake.
#            The successful poll DELETES the row from the DB, so
#            this chain cannot be followed by list_devices on the
#            same device_code.
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# A1 — Client initiates the device flow (public, no auth needed).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/authorize
Content-Type: application/json
{
  "client_name": "hurl-device-flow-A",
  "scope":       "webdav,caldav,carddav"
}

HTTP 200
[Captures]
device_code_a: jsonpath "$.device_code"
user_code_a: jsonpath "$.user_code"
[Asserts]
jsonpath "$.verification_uri" exists
jsonpath "$.expires_in" > 0
jsonpath "$.interval" > 0


# ─────────────────────────────────────────────────────────────
# A2 — Client polls /token before approval → RFC 8628
#      `authorization_pending`, HTTP 400 with the error code in
#      the JSON body.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/token
Content-Type: application/json
{
  "device_code": "{{device_code_a}}",
  "grant_type":  "urn:ietf:params:oauth:grant-type:device_code"
}

HTTP 400
[Asserts]
jsonpath "$.error" == "authorization_pending"


# ─────────────────────────────────────────────────────────────
# A3 — Human opens /verify?code=... — the FE's lookup endpoint.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/device/verify?code={{user_code_a}}
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.valid" == true
jsonpath "$.client_name" == "hurl-device-flow-A"
jsonpath "$.scopes" exists


# ─────────────────────────────────────────────────────────────
# A4 — Human approves. The typed response echoes `"approved"`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/verify
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "user_code": "{{user_code_a}}", "action": "approve" }

HTTP 200
[Asserts]
jsonpath "$.status" == "approved"


# ─────────────────────────────────────────────────────────────
# A5 — Client re-polls → tokens issued. The service DELETES the
#      device-code row as part of this response (prevents
#      plaintext tokens lingering in auth.device_codes), so this
#      is the last operation chain A can run against this code.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/token
Content-Type: application/json
{
  "device_code": "{{device_code_a}}",
  "grant_type":  "urn:ietf:params:oauth:grant-type:device_code"
}

HTTP 200
[Asserts]
jsonpath "$.access_token" exists
jsonpath "$.refresh_token" exists
jsonpath "$.token_type" exists
jsonpath "$.expires_in" > 0
jsonpath "$.scope" == "webdav,caldav,carddav"


# ═════════════════════════════════════════════════════════════
# CHAIN B — fresh device code, approved but never polled, so the
#            row stays in the DB for list/revoke to see.
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# B1 — Second authorize, giving us a device code with a
#      recognisable client_name we can filter on in list_devices.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/authorize
Content-Type: application/json
{
  "client_name": "hurl-device-flow-B",
  "scope":       "webdav"
}

HTTP 200
[Captures]
user_code_b: jsonpath "$.user_code"


# ─────────────────────────────────────────────────────────────
# B2 — Human approves. list_user_devices filters by `user_id`
#      which is only set on approve, so without this step the
#      next request would see an empty list.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/verify
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "user_code": "{{user_code_b}}", "action": "approve" }

HTTP 200
[Asserts]
jsonpath "$.status" == "approved"


# ─────────────────────────────────────────────────────────────
# B3 — Owner lists their authorized devices. Capture chain B's
#      row by filtering on the unique client_name.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/device/devices
Authorization: Bearer {{owner_token}}

HTTP 200
[Captures]
device_id_b: jsonpath "$[?(@.client_name=='hurl-device-flow-B')].id"
[Asserts]
jsonpath "$[?(@.client_name=='hurl-device-flow-B')].scopes" == "webdav"


# ─────────────────────────────────────────────────────────────
# B4 — Owner revokes the device. 204 confirms the row is gone
#      from the authorized set — this corresponds to the
#      `delete_by_id` path in `device_auth_service::revoke_device`.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/device/devices/{{device_id_b}}
Authorization: Bearer {{owner_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# B5 — Idempotency check: a second revoke of the same id no
#      longer finds a row to delete. The service maps "not found
#      by this user" to 404 — this is the behaviour the FE's
#      sync clients rely on when retrying a revoke after a
#      flaky connection.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/device/devices/{{device_id_b}}
Authorization: Bearer {{owner_token}}

HTTP 404
