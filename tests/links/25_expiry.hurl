# =============================================================
# Links — grant expiry and renewal (timed part)
# =============================================================
# Needs the captures of 25_expiry_setup.hurl and `expires_soon`
# (run.sh: 12 s after the setup finished). A grant expiring then
# is linked, hidden, and deleted by the sweep, which frees its
# name; moving `expires_at` forward links it again. A second,
# expiring grant on the declined `Kept` leaves the decline alone,
# and the sweep does not bring the link back.
# =============================================================


POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{kept_id}}" },
  "role": "viewer",
  "expires_at": "{{expires_soon}}"
}

HTTP 201

POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{expiring_id}}" },
  "role": "viewer",
  "expires_at": "{{expires_soon}}"
}

HTTP 201


# Linked before the expiry; the retry budget (8 s) stays inside it. `Kept` was granted
# first, so its event is settled too: the decline stands.
GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 16
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Expiring"
jsonpath "$.items[*].resource.name" not contains "Kept"


# ─────────────────────────────────────────────────────────────
# Expired: hidden, then deleted by the sweep, freeing the name.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 40
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "Expiring"


POST {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}
Content-Type: application/json
[Options]
retry: 30
retry-interval: 500
{ "name": "Expiring", "parent_id": "{{shares_id}}" }

HTTP 201


# The sweep has settled both expired pairs: `Kept` stays declined.
GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "Kept"

GET {{base_url}}/api/grants/incoming/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{kept_id}}')].link.declined" == true


# ─────────────────────────────────────────────────────────────
# Renewed: linked again, next to Bob's own `Expiring`.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{bob_id}}" },
  "resource": { "type": "folder", "id": "{{expiring_id}}" },
  "role": "viewer",
  "expires_at": "2099-01-01T00:00:00Z"
}

HTTP 200


GET {{base_url}}/api/folders/{{shares_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}
[Options]
retry: 20
retry-interval: 500

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "Expiring"
jsonpath "$.items[*].resource.name" contains "Expiring (2)"
