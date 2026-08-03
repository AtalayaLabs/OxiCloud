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

- `oidc_provider`
- `oidc_subject`

This allows one external identity to map to one local user record and supports just-in-time provisioning when `OXICLOUD_OIDC_AUTO_PROVISION=true`.

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
