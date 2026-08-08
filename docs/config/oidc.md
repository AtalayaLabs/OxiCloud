# OIDC / SSO

OxiCloud supports OpenID Connect for single sign-on with providers like **Keycloak**, **Authentik**, **Authelia**, **Google**, and **Azure AD**.

## How It Works

1. User clicks "Sign in with SSO" on the login page
2. Browser redirects to the identity provider (IdP)
3. User authenticates with their existing credentials
4. IdP redirects back to OxiCloud with an auth code
5. OxiCloud exchanges the code for user info and issues its own JWT tokens

## Architecture

OIDC follows the Authorization Code Flow and keeps a clear split between provider communication and local session handling.

- `OidcService` discovers provider metadata, builds authorization URLs, exchanges authorization codes, and validates the token response
- `AuthApplicationService` coordinates user lookup or auto-provisioning and then issues OxiCloud's own access and refresh tokens
- the auth handler exposes the public OIDC endpoints under `/api/auth/oidc/*`

After the browser returns from the IdP, OxiCloud does not reuse the provider token for app requests. It converts the identity into its own JWT session model.

## Configuration

```bash
OXICLOUD_OIDC_ENABLED=true
OXICLOUD_OIDC_ISSUER_URL="https://authentik.example.com/application/o/oxicloud/"
OXICLOUD_OIDC_CLIENT_ID="your-client-id"
OXICLOUD_OIDC_CLIENT_SECRET="your-client-secret"
OXICLOUD_OIDC_REDIRECT_URI="https://oxicloud.example.com/api/auth/oidc/callback"
OXICLOUD_OIDC_SCOPES="openid profile email"
OXICLOUD_OIDC_FRONTEND_URL="https://oxicloud.example.com"
OXICLOUD_OIDC_AUTO_PROVISION=true
OXICLOUD_OIDC_ADMIN_GROUPS="oxicloud-admins"
OXICLOUD_OIDC_PROVIDER_NAME="Authentik"
```

### Variable Reference

| Variable | Default | Description |
|---|---|---|
| `OXICLOUD_OIDC_ENABLED` | `false` | Master switch |
| `OXICLOUD_OIDC_ISSUER_URL` | — | Provider's OIDC issuer URL |
| `OXICLOUD_OIDC_CLIENT_ID` | — | OAuth client ID |
| `OXICLOUD_OIDC_CLIENT_SECRET` | — | OAuth client secret |
| `OXICLOUD_OIDC_REDIRECT_URI` | `http://localhost:8086/api/auth/oidc/callback` | Callback URL registered with the IdP |
| `OXICLOUD_OIDC_SCOPES` | `openid profile email` | Requested scopes |
| `OXICLOUD_OIDC_FRONTEND_URL` | `http://localhost:8086` | Where to redirect the browser after auth |
| `OXICLOUD_OIDC_AUTO_PROVISION` | `true` | Auto-create users on first login |
| `OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH` | `true` | Auto-link existing local users to their OIDC identity when the IdP-returned email (with `email_verified=true`) matches an existing local account. See narrative below. |
| `OXICLOUD_OIDC_ADMIN_GROUPS` | — | OIDC groups that grant admin role |
| `OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN` | `false` | **DEPRECATED** — use `OXICLOUD_AUTH_METHODS=oidc` instead. Emits a boot warning; slated for removal in next major release. |
| `OXICLOUD_OIDC_PROVIDER_NAME` | `SSO` | Label shown on the login button |

::: warning
If `OXICLOUD_OIDC_ENABLED=true` but `issuer_url`, `client_id`, or `client_secret` are empty, OIDC is automatically disabled with an error log.
:::

## API Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/auth/oidc/providers` | Returns OIDC provider info |
| GET | `/api/auth/oidc/authorize` | Authorization URL for redirect to IdP |
| GET | `/api/auth/oidc/callback` | Callback from IdP with auth code |
| POST | `/api/auth/oidc/exchange` | Exchange auth code for JWT tokens |

## Identity Mapping

OIDC users are matched by the pair:

- `federation_issuer` (the id_token `iss` claim, canonical issuer URL)
- `federation_subject` (the id_token `sub` claim)

This allows one external identity to map to one local user record and supports just-in-time provisioning when `OXICLOUD_OIDC_AUTO_PROVISION=true`.

### Auto-linking existing local users

When `OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH=true` (default) and the subject-based lookup misses on an OIDC login, OxiCloud tries to match by the IdP-returned email address instead. If exactly one local user's email matches (under `+alias`-stripping normalization) AND the IdP returned `email_verified=true`, the OIDC identity is auto-linked to that existing user — no admin round-trip, no manual SQL, no self-service flow needed. Great UX for enabling SSO on top of an existing user base.

Refusal cases (fall through to the standard "email already exists" error):

- `email_verified=false` on the IdP claims — audit event `federation.auto_link_refused` with `reason=auto_link_email_not_verified`.
- More than one local user's email normalizes to the same value (rare but possible with `alice@example.com` and `alice+work@example.com`) — refused as `email_ambiguous`.
- The matched user is already linked to a different OIDC identity — refused as `already_linked_elsewhere`.

Security model: safe under OxiCloud's single-IdP configuration (admin explicitly chose and configured the IdP; the `email_verified` gate means the IdP has vouched for the user's ownership of that email). NOT safe for future multi-IdP federation where any WebFinger-discovered IdP is accepted — deferred to that flow.

For users who need explicit consent for every OIDC link (compliance requirements), set `OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH=false`. The self-service link flow (profile page "Connect Single Sign-On" button) remains available regardless.

Full decision tree and safety-check details in [`docs/plan/oidc-account-linking.md`](../plan/oidc-account-linking.md).

## Provider Examples

### Keycloak

```yaml
# docker-compose.yml
services:
  oxicloud:
    environment:
      OXICLOUD_OIDC_ENABLED: "true"
      OXICLOUD_OIDC_ISSUER_URL: "https://keycloak.example.com/realms/your-realm"
      OXICLOUD_OIDC_CLIENT_ID: "oxicloud"
      OXICLOUD_OIDC_CLIENT_SECRET: "your-client-secret"
      OXICLOUD_OIDC_REDIRECT_URI: "https://oxicloud.example.com/api/auth/oidc/callback"
      OXICLOUD_OIDC_FRONTEND_URL: "https://oxicloud.example.com"
      OXICLOUD_OIDC_PROVIDER_NAME: "Keycloak"
```

### Authentik

```bash
OXICLOUD_OIDC_ISSUER_URL="https://authentik.example.com/application/o/oxicloud/"
OXICLOUD_OIDC_PROVIDER_NAME="Authentik"
```

### Google

```bash
OXICLOUD_OIDC_ISSUER_URL="https://accounts.google.com"
OXICLOUD_OIDC_PROVIDER_NAME="Google"
```

## Notes

- Always use **HTTPS** for OIDC connections
- One OIDC provider per instance (single-provider model)
- OIDC users share the same permissions model as local users
- After OIDC auth, the backend issues its own JWT tokens (no IdP token dependency)
- Use the admin settings UI (`/admin.html`) to configure and test OIDC at runtime
