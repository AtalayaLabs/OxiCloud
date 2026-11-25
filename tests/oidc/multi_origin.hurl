# OIDC on more than one origin (#117).
#
# server-with-oidc.env lists two callback entries for the SAME server:
# `localhost:8087` (primary) and `127.0.0.1:8087`. A login started on
# 127.0.0.1 must send that origin's redirect_uri, reuse it on the token
# exchange (else the IdP refuses the code) and land back on 127.0.0.1.
# An unlisted Host must fall back to the primary, never echo the Host.

# Authorize on the second origin → its own redirect_uri.
GET http://127.0.0.1:8087/api/auth/oidc/authorize
[Options]
location: false
HTTP 307
[Captures]
idp_url_lan: header "Location"
[Asserts]
header "Location" contains "redirect_uri=http%3A%2F%2F127.0.0.1%3A8087%2Fapi%2Fauth%2Foidc%2Fcallback"

# Full chain: the callback exchanges the code with the flow's URI and
# redirects to the matching frontend base. Regression: using the primary
# on the token request → IdP rejects → login_error=callback_denied.
GET {{idp_url_lan}}
[Options]
location: true
location-trusted: true
HTTP 200
[Captures]
lan_oidc_code: url regex "oidc_code=([a-f0-9]+)"
[Asserts]
url matches "^http://127\\.0\\.0\\.1:8087/login\\?oidc_code=[a-f0-9]+$"

# Primary origin unchanged (single-entry behaviour).
GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: false
HTTP 307
[Asserts]
header "Location" contains "redirect_uri=http%3A%2F%2Flocalhost%3A8087%2Fapi%2Fauth%2Foidc%2Fcallback"

# Forged Host selects nothing → primary, never evil.example.
GET {{base_url}}/api/auth/oidc/authorize
Host: evil.example
[Options]
location: false
HTTP 307
[Asserts]
header "Location" contains "redirect_uri=http%3A%2F%2Flocalhost%3A8087%2Fapi%2Fauth%2Foidc%2Fcallback"
header "Location" not contains "evil.example"

# X-Forwarded-Host picks a listed entry like a reverse proxy would.
GET {{base_url}}/api/auth/oidc/authorize
X-Forwarded-Host: 127.0.0.1:8087
[Options]
location: false
HTTP 307
[Asserts]
header "Location" contains "redirect_uri=http%3A%2F%2F127.0.0.1%3A8087%2Fapi%2Fauth%2Foidc%2Fcallback"

# Exchange the LAN login's one-time code for a real session (fresh cookies).
POST {{base_url}}/api/auth/oidc/exchange
Content-Type: application/json
{ "code": "{{lan_oidc_code}}" }
HTTP 200
[Captures]
lan_csrf: cookie "oxicloud_csrf"

# RP-initiated logout from the LAN origin: the IdP end-session URL must carry
# THIS origin's post_logout_redirect_uri, not the primary's. Regression: logout
# hard-coded base_url/login, so a non-primary origin was sent to the primary's
# login and the IdP (which only allows registered URIs) could refuse it.
POST {{base_url}}/api/auth/logout
X-Forwarded-Host: 127.0.0.1:8087
X-CSRF-Token: {{lan_csrf}}
Content-Type: application/json
{}
HTTP 200
[Asserts]
jsonpath "$.post_logout_url" contains "post_logout_redirect_uri=http%3A%2F%2F127.0.0.1%3A8087%2Flogin"
jsonpath "$.post_logout_url" not contains "localhost%3A8087%2Flogin"
