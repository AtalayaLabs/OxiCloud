# =============================================================
# OxiCloud — admin-panel OIDC: env-locked fields + login offer (#251)
# =============================================================
# #251: with OXICLOUD_OIDC_ENABLED=false in the env (example.env), an
# admin save of `enabled` returned 200 and changed nothing; with the
# env unset, OIDC enabled in the panel was served but /api/config still
# hid it (boot allowlist). `tests/common/server.env` pins only
# OXICLOUD_OIDC_PROVIDER_NAME and lists no `oidc` in AUTH_METHODS.
#
# Prerequisites: setup.hurl must have run (admin user exists).
# =============================================================

POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}
HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


GET {{base_url}}/api/admin/settings/oidc
Authorization: Bearer {{admin_token}}
HTTP 200
[Asserts]
jsonpath "$.enabled" == false
jsonpath "$.env_overrides" contains "provider_name"


# Pin: changing an env-locked field is refused, not silently dropped.
PUT {{base_url}}/api/admin/settings/oidc
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "enabled": false,
  "issuer_url": "",
  "client_id": "",
  "provider_name": "Other"
}
HTTP 409
[Asserts]
jsonpath "$.error_type" == "Conflict"
jsonpath "$.message" contains "OXICLOUD_OIDC_PROVIDER_NAME"


# Pin: OIDC switched on in the panel is offered by /api/config.
PUT {{base_url}}/api/admin/settings/oidc
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "enabled": true,
  "issuer_url": "http://127.0.0.1:9101",
  "client_id": "oxicloud-251",
  "client_secret": "secret-251"
}
HTTP 200


GET {{base_url}}/api/config
HTTP 200
[Asserts]
jsonpath "$.auth.oidc.enabled" == true


# Teardown (and allowed pin): switching it off again goes through.
PUT {{base_url}}/api/admin/settings/oidc
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "enabled": false,
  "issuer_url": "",
  "client_id": ""
}
HTTP 200


GET {{base_url}}/api/config
HTTP 200
[Asserts]
jsonpath "$.auth.oidc.enabled" == false
