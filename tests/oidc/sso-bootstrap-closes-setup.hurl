# Phase C of run-sso-only.sh: same DB as Phase B (admin bootstrapped via
# OIDC, no setup flag), password login now on.
#
# #112: /api/setup only checked the setup flag, so after an OIDC
# bootstrap anyone could still mint a password Owner here.

GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.auth.password.enabled" == true
jsonpath "$.initialized" == true


POST {{base_url}}/api/setup
Content-Type: application/json
{
  "username": "late-owner",
  "email": "late-owner@example.test",
  "password": "late-owner-pass-123"
}

HTTP 403
[Asserts]
jsonpath "$.error_type" == "SystemAlreadyInitialized"
