# =============================================================
# OxiCloud — vCard parameter value encoding (RFC 6868)
# =============================================================
# Contact `type` fields (EMAIL/TEL/ADR) are free text, not a fixed
# enum — a value containing a comma, semicolon, colon, or DQUOTE
# previously broke the generated vCard's grammar outright (no quoting
# or escaping was applied at all). See
# application/adapters/param_encoding.rs::render_param_value, wired
# into carddav_adapter.rs::contact_to_vcard and
# contact_service.rs::generate_vcard.
#
# Coverage:
#   1. A plain type value ("work") is emitted unquoted, unchanged.
#   2. A type value containing a comma and an embedded DQUOTE is
#      emitted quoted and RFC 6868-encoded (embedded DQUOTE → `^'`),
#      producing a well-formed content line instead of broken syntax.
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
# Step 2 — Fresh address book.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/address-books
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "param-encoding-test", "description": "", "is_public": false }

HTTP 201
[Captures]
book_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Contact with an EMAIL type needing RFC 6868 encoding:
#          contains a comma (grammar-triggers quoting) AND an
#          embedded DQUOTE (needs caret-escaping even once quoted).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/address-books/{{book_id}}/contacts
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "first_name": "Special",
  "last_name": "Case",
  "full_name": "Special Case",
  "email": [
    { "email": "special@example.com", "type": "Work, \"Primary\"", "is_primary": true }
  ],
  "phone": [
    { "number": "+1-555-0199", "type": "mobile", "is_primary": true }
  ]
}

HTTP 201
[Captures]
contact_id: jsonpath "$.id"
contact_uid: jsonpath "$.uid"


# ─────────────────────────────────────────────────────────────
# Step 4 — Fetch the generated vCard via CardDAV GET.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/carddav/{{book_id}}/{{contact_uid}}.vcf
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body startsWith "BEGIN:VCARD"
# Plain type value ("mobile") stays bare, unquoted.
body contains "TEL;TYPE=MOBILE:+1-555-0199"
# Comma + embedded DQUOTE forces quoting and RFC 6868 caret-encoding.
body contains "EMAIL;TYPE=\"WORK, ^'PRIMARY^'\":special@example.com"


# ─────────────────────────────────────────────────────────────
# Cleanup
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books/{{book_id}}/contacts/{{contact_id}}
Authorization: Bearer {{token}}

HTTP 200
[Captures]
contact_etag: header "ETag"


DELETE {{base_url}}/api/address-books/{{book_id}}/contacts/{{contact_id}}
Authorization: Bearer {{token}}
If-Match: {{contact_etag}}

HTTP 204


DELETE {{base_url}}/api/address-books/{{book_id}}
Authorization: Bearer {{token}}

HTTP 204
