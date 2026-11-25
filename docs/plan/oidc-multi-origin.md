# OIDC on more than one origin (#117)

**Status**: DRAFT — waiting for a maintainer yes on § Decisions to confirm.

In one line: **an operator can list several public origins for OIDC; a login
started on one of them comes back to that same origin, and a request from any
other origin falls back to the primary one — never to the request's own Host.**

## Problem

One instance, two ways in — `https://cloud.example.com` through a reverse
proxy and `http://192.168.1.10:8086` on the LAN. OIDC works on only one of
them, because every URL the flow emits is a single configured value:

| URL | Today | Where |
|---|---|---|
| `redirect_uri` on `/authorize` | `OidcConfig.redirect_uri` | `oidc_service.rs::get_authorize_url` |
| `redirect_uri` on the token request | same value (RFC 6749 §4.1.3: MUST be identical) | `oidc_service.rs::exchange_code` |
| Post-callback redirect (`/login?oidc_code=…`, `/profile?linked=1`, every `login_error`) | `OidcConfig.frontend_url` | `auth_handler.rs::oidc_callback` (6 branches) |
| `post_logout_redirect_uri` | `config.base_url()` + `/login` | `auth_handler.rs::logout` |

Auth cookies are host-only, so finishing the flow on the "wrong" origin is
not a cosmetic issue: the session lands on an origin the user is not on, and
the LAN login fails whenever the proxy is down — the exact case in the issue.

The browser side already follows whatever origin it is on:
`/api/config.auth.oidc.authorize_endpoint` is a relative path
(`config_handler.rs`), so only the server needs to change.

## Non-goals

- Deriving the redirect URI from the `Host` header with no list. That is
  an open-redirect / code-interception risk with `X-Forwarded-Host`, and
  frenzeldk asked for an allowlist in the issue.
- Several **IdPs** or client IDs. One issuer, one client, several callback
  URLs registered on it (Authentik, Keycloak, Authelia all support that).
- A general multi-origin server: share links, WOPI, e-mail links and
  `OXICLOUD_BASE_URL` stay single-origin. Separate issue if wanted.
- Admin-panel editing of the list. `callback_url` is derived, not stored,
  today; the list is env/TOML only, like `OXICLOUD_OIDC_REDIRECT_URI`.

## Design

### Config

`OXICLOUD_OIDC_REDIRECT_URI` accepts a comma-separated list. One value
behaves exactly as today, so no existing deployment changes.

```env
OXICLOUD_OIDC_REDIRECT_URI=https://cloud.example.com/api/auth/oidc/callback,http://192.168.1.10:8086/api/auth/oidc/callback
```

- First entry = **primary**: used when the request's origin matches no entry,
  and the one shown first in the admin panel.
- Each entry is parsed at startup with `url::Url` (already a dependency);
  an entry that does not parse, is not `http(s)`, or does not end in
  `/api/auth/oidc/callback` (after `base_path`) **fails startup** — a typo
  must not silently fall back to the primary.
- Per entry we keep the origin (`scheme://host[:port]`) and the frontend
  base (the entry minus `/api/auth/oidc/callback`).
- `OXICLOUD_OIDC_FRONTEND_URL` keeps its meaning for the primary entry only;
  other entries use their own frontend base. With a single entry nothing
  changes.

`OidcConfig.redirect_uri: String` becomes `redirect_uris: Vec<OidcOrigin>`
with `fn primary()` and `fn for_origin(&str) -> &OidcOrigin` (exact origin
match, falls back to `primary()`).

### Flow

1. **Authorize** (`oidc_authorize`, `oidc_link_start`, NC Login Flow v2
   `prepare_oidc_authorize_for_nextcloud` — all three doors).
   The handler resolves the external origin from the request and passes it
   in; the service picks `for_origin(origin)` and stores the chosen entry in
   `PendingOidcFlow` next to the PKCE verifier and nonce.
2. **Callback** — `exchange_code` uses the `redirect_uri` **stored in the
   flow**, not the config's. Every redirect in `oidc_callback` uses the
   flow's frontend base. On the error paths where no flow exists (unknown /
   expired `state`), use the primary.
3. **Logout** — `post_logout_redirect_uri` = request origin's entry
   frontend base + `/login`, primary when unmatched.

Origin resolution: the `external_scheme_host` helper in
`middleware/dpop.rs` already does `X-Forwarded-Proto/Host` → `Host` → default.
Move it to a shared spot (`interfaces/api/request_origin.rs` or next to
`client_ip_from_parts`) and use it from both, rather than writing a second
copy.

### Why a forged Host header is harmless

The request origin only **selects** among operator-listed entries; it never
becomes a URL. A forged `Host`/`X-Forwarded-Host` can at most send the user
to another origin the operator registered — and the IdP rejects any
`redirect_uri` it does not have registered anyway. PKCE + `state` binding is
unchanged, and the token request always reuses the flow's own URI.

### Admin panel

`OidcSettingsDto.callback_url: String` → add `callback_urls: Vec<String>`
(all entries) and keep `callback_url` = primary for one release so older
SPAs keep working. The OIDC settings page lists every URL to copy into the
IdP. The #251 env-lock check treats the list as one locked field.

### Docs

`docs/config/oidc.md`, `docs/config/env.md`, `example.env`,
`oxicloud.example.toml` (TOML: array or comma string — follow how the
other list-valued `OXICLOUD_*` keys parse). Note that `OXICLOUD_COOKIE_SECURE=true`
still blocks login on a plain-`http` LAN origin; that is existing behaviour,
not changed here.

## Tests

| Layer | What |
|---|---|
| Rust unit (`config.rs`) | parse 1 / 2 entries; bad URL, wrong scheme, wrong path → startup error; `for_origin` exact match, port matters, unmatched → primary. |
| Rust unit (service) | authorize with origin B stores B's URI; callback uses the stored URI for the token request even after config reload. |
| Hurl `tests/oidc/oidc.hurl` | fake IdP already registers `:8087` and `:8090` callbacks. Set two entries; `/authorize` with `Host: localhost:8090` → `redirect_uri=…8090…`; unknown `Host: evil.example` → primary URI, not `evil`; callback redirect lands on the matching frontend base. |
| Hurl, regression pin | single-entry config: authorize + callback URLs byte-identical to today. |
| Vitest | admin OIDC page renders every `callback_urls` entry. |

Red-before-green: drop the per-flow URI and use the config primary → the
`:8090` pin must fail on `redirect_uri`.

## Doors checked

REST login, self-service link, NC Login Flow v2, logout. Back-channel logout
has no redirect URI — not affected. WebDAV/CalDAV/CardDAV do not run OIDC.

## Decisions (for the maintainer)

| Question | Proposal |
|---|---|
| Knob shape | Reuse `OXICLOUD_OIDC_REDIRECT_URI` as a list (no new var), vs. a new `OXICLOUD_OIDC_REDIRECT_URIS`. |
| Unmatched origin | Fall back to primary (proposal) vs. refuse with 400. Fallback keeps today's behaviour for every unlisted origin. |
| Startup on bad entry | Fail startup (proposal) vs. log + skip. |
| Scope | OIDC only; share links / WOPI / base URL stay single-origin. |

## Size

~1 PR: config + 3 authorize doors + callback + logout + admin DTO/page +
docs + tests. No migration (the chosen URI lives in the in-memory moka
`pending_oidc_flows`, which already holds the PKCE verifier for 10 min).
