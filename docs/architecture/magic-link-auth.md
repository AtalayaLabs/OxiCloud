# Magic-Link External Authentication

OxiCloud supports sharing resources with people who do not yet have an account on the instance, via per-email invitations and per-email sign-in links. Recipients are provisioned lazily as **external users** and authenticate exclusively through one-time URLs delivered by email, until they later set a password or link an OIDC identity.

This page is the architectural overview of the magic-link mechanism specifically. For the overall identity / login / registration model — what credential slots a user has, which login paths are available, anti-enumeration behaviour — see the canonical [Authentication model](/architecture/auth-model) page. For configuration knobs, see [Environment Variables](/config/env). For how grants are evaluated, see [ReBAC Authorization](/architecture/rebac-authorization).

## Why this exists

Two scenarios are not covered by username/password or OIDC:

1. **Sharing with someone who is not yet an OxiCloud user.** The sharer should be able to type an email address into the share modal and let the server handle the rest.
2. **A pre-existing external user has lost their bookmark.** They never had a password — the only way to get back in is a fresh magic link to the address on file.

Both are handled by the same magic-link primitive: a single-use, time-limited token issued out of band (by email) that exchanges for a session on redemption.

## Two flows

```
Invitation flow                           Login-via-email flow
───────────────                           ────────────────────
Alice fills share modal                   Bob hits /login, types his email
       │                                         │
POST /api/grants                          POST /api/auth/magic-link/send
{ subject.type: "email" }                 { email: "bob@example.com" }
       │                                         │
resolve_or_create_recipient               find user by email
  ├─ found  → reuse                       (no creation here)
  └─ new    → User::new_external                 │
       │                                         │
mint token (resource_type/id set)         mint token (NULL resource)
       │                                         │
queue invitation email                    queue sign-in email
       │                                         │
return GrantDto (201)                     return uniform 200
       │                                         │
       └──────────────┬──────────────────────────┘
                      │
              recipient clicks /magic/v1/{token}
                      │
               validate, mark used, issue cookies
                      │
       ┌──────────────┴──────────────────────────┐
       ↓                                         ↓
Redirect to /#/files/folder/{id}        Redirect to /#/sharedwithme
(resource target)                       (no resource target)
```

The same redemption endpoint serves both — the only difference is the landing redirect, which is decided by whether `magic_link_tokens.resource_id IS NULL`.

## Identity model for external users

A user is **magic-link-eligible** if and only if they have no other authentication method configured. The single source of truth is `User::has_login_credential()`:

| State                          | `password_hash`              | `oidc_subject` | Eligible? |
|--------------------------------|------------------------------|----------------|-----------|
| External, freshly invited      | `__EXTERNAL_NO_PASSWORD__`   | NULL           | yes       |
| External who set a password    | real Argon2 hash             | NULL           | no        |
| External who linked OIDC       | `__OIDC_NO_PASSWORD__`       | set            | no        |
| Internal, password             | real Argon2 hash             | NULL           | no        |
| Internal, OIDC-only            | `__OIDC_NO_PASSWORD__`       | set            | no        |

The placeholder strings (`__EXTERNAL_NO_PASSWORD__`, `__OIDC_NO_PASSWORD__`) are an acknowledged smell. A future refactor introduces an `auth.user_auth_methods` side-table with one row per `(user_id, method_type)`; the migration touches the body of `has_login_credential()` only.

The eligibility rule rules out one specific bypass: an internal user with a password cannot be signed in via a magic link sent to their mailbox. Mailbox ownership is not a substitute for the password — that distinction matters when mailboxes are easier to compromise than passwords (mail-forwarding rules, shared aliases, etc.).

### Username and display

- External users get `username = normalised_email`. Login forms accept username OR email; lookup tries `username` first, falls back to `email`.
- The `auth.users.username` column was widened from 32 to 254 chars (RFC 5321 maximum) when this work landed.
- `auth.users.given_name` and `auth.users.family_name` are `TEXT NULL` — populated from OIDC claims at JIT provisioning; external users get NULL initially and can fill them in later.
- Home folder name (`"My Folder - alice"`) is **not** renamed when username changes — it was display text at creation; the folder is semantically owned by `user_id`.

### Email normalisation

Every email crossing the boundary into the DB or a rate-limit key goes through `domain::services::email_normalize::normalize_email`:

1. Trim whitespace.
2. Split on the **last** `@`.
3. Lowercase the local part.
4. Punycode-encode the domain via `idna::domain_to_ascii`.

So `Alice@Example.COM`, `  alice@example.com  `, and `alice@münchen.de` all map to a stable ASCII form before storage or comparison. Gmail's `+tag` and `.` insensitivities are deliberately **not** special-cased — addresses are treated as opaque strings post-normalisation.

## Token lifecycle

```
                    ┌─────────┐
   (insert) ─────►  │ pending │  ─── redeem ──►  ┌──────┐
                    │         │                  │ used │
                    └────┬────┘                  └──────┘
                         │
                  (sweeper, TTL)
                         ↓
                    ┌─────────┐
                    │ expired │
                    └─────────┘
```

`auth.magic_link_tokens` mirrors `auth.device_codes` exactly: PostgreSQL ENUM status, 32-byte CSPRNG token in base64url, single-use via `UPDATE … WHERE status = 'pending'`, partial index on `expires_at WHERE pending`, and a background sweeper that promotes pending-and-overdue rows to expired.

Salient properties:

- **Single-use** — second redemption attempt rejected as "link already used".
- **TTL-enforced** — `expires_at < NOW()` → "link expired". TTL is `OXICLOUD_MAGIC_LINK_TTL_HOURS`, default 24.
- **Token in path, not query** — `GET /magic/v1/{token}` so the secret stays out of `Referer` headers.
- **302 immediately on success** — the URL is replaced in the address bar before the user can navigate away or screenshot it.
- **Optional resource target** — `resource_type` + `resource_id` columns, with a `CHECK ((resource_type IS NULL) = (resource_id IS NULL))` constraint to make the two-or-neither rule explicit.
- **Rows persist past `used` / `expired`** — the sweeper transitions status, it does not immediately DELETE. This is what makes the self-service resend (next section) possible: the token in the URL keeps working as a recipient-discovery key well after the credential it carried has stopped working.

## Self-service resend

The 410-Gone landing page for a stale link is **not a dead end**. The page server-side branches on whether the token row is recoverable:

- Row exists, status is `expired` **or** `used`, owning user is still active → the page renders a one-click form: *"Send a fresh link to a…@example.com"* (POST to `/magic/v1/{token}/resend`).
- Anything else (unknown token, pending, deactivated account, plumbing missing) → the page falls back to the existing generic "no longer valid" message. The two responses are deliberately indistinguishable to the caller — the rich page only differs when the row already proves the caller has legitimate context.

### Why no PII in the URL

An earlier sketch carried the recipient's email as `?r={base64(email)}` so the page could greet the user by address. We dropped it: a token is short-lived but a URL persists in browser history forever (and syncs to Chrome / Firefox cloud profiles), the address would leak via any future external Referer, and reverse-proxy access logs would gain a PII field they don't have today. Since the row already carries `user_id → users.email`, the server can recover the address on demand and the URL stays clean.

### Why both `expired` and `used` qualify

`used` covers the "I clicked the link on my phone, now I want to sign in on my laptop" case — the original link is dead by single-use design, but the recipient is real and the row still holds the recipient pointer. Offering resend on `used` is harmless (the new mail goes to the registered email, not the caller; rate limits are the same) and avoids a confusing dead-end for the most common second-device path.

### Endpoint shape

`POST /magic/v1/{token}/resend` mirrors `POST /api/auth/magic-link/send` in every operational respect:

1. **Per-source-IP rate limit** (`OXICLOUD_MAGIC_LINK_SEND_PER_IP_PER_HOUR`, default 200/h) — runs first, unconditionally. Burns budget even when the token doesn't resolve so the endpoint can't be used to spread probes thin across many tokens.
2. **Token resolution** — `MagicLinkInviteService::lookup_resend_recipient(token)`. Returns `Some(ResendRecipientHint)` only for `expired` / `used` rows whose owning user is active. `None` in every other case (pending, unknown, deactivated, repo absent).
3. **Per-target-email rate limit** (`OXICLOUD_MAGIC_LINK_SEND_PER_EMAIL_PER_HOUR`, default 5/h) — keyed on the recipient we just resolved. Caps actual mail volume to the recipient regardless of how many IPs hammer the endpoint; effectively a per-token-recipient ceiling without a schema-level counter.
4. **Fresh challenge + send** — generate a per-request challenge (cookie + token-row mirror, see PR 22), dispatch through `send_login_link`. The new token has the standard short login TTL, not the longer invite TTL — the recipient just clicked, so a slow second click is almost certainly someone else with mailbox access.
5. **Uniform response** — every outcome (rate-limited, no-account, account deactivated, SMTP-failed, succeeded) renders the same "Check your inbox" HTML page. The real outcome is in the audit channel via `auth.magic_link_send` events.

The handler is a sibling under the same `/magic/v1/*` router, **no CSRF middleware** (the page that triggers it is the 410 response itself — same-origin, plain HTML form, no JS, no third-party referrer realistically able to forge the POST against a per-token URL).

### What the per-token ceiling looks like in practice

A single recovered URL × per-target-email cap (5/h) × 24h = **120 magic-link mails per day maximum** to that recipient from that specific URL. Annoying but bounded; the recipient's inbox cannot be flooded into uselessness from one stable handle. A per-token counter column (`resend_count` with a hard maximum, e.g. 3) would tighten the bound further; it's deferred until real abuse is observed.

## User-profile visibility rule (`GET /api/users/{id}`)

The endpoint is the cornerstone of the share modal's "who is this person" rendering. Its visibility rule is intentionally narrow, evaluated in this order:

1. **Self** — caller asks for their own profile.
2. **Shared-grant relationship** — caller and target share at least one access grant in either direction. Applies to internal AND external callers; this is what lets a recipient resolve the granter's name/photo in the SharedWithMe view.
3. **External lockout** — if the caller is external and rule 2 did not match, stop and return 404.
4. **Directory exposure** — if the target is internal AND `OXICLOUD_EXPOSE_SYSTEM_USERS=true`, return the target.
5. **Admin** — admins can always look up any user.
6. **404** — otherwise. Same response as "user does not exist" (anti-enumeration).

A per-caller sliding-window rate limit (60 req/minute) guards against an attacker iterating UUIDs against rule 2 with a stale JWT. The visibility rule alone is sufficient defence-in-principle; the rate limit makes the attack uneconomical.

## Audit events

Every denial or rejection in the magic-link path emits a structured event on the `audit` tracing target. Operators tail `target=audit` for compliance and incident response.

| Event                              | Reasons (subset)                                                                                  | Where it fires                                          |
|------------------------------------|---------------------------------------------------------------------------------------------------|----------------------------------------------------------|
| `authz.denied`                     | permission missing                                                                                | `AuthorizationEngine::require`                           |
| `auth.login`                       | `user_not_found`, `bad_password`, `account_deactivated`                                           | `AuthApplicationService::login`                          |
| `auth.magic_link_send`             | `sent`, `no_account`, `has_credential`, `account_deactivated`, `malformed_email`, `rate_limited_ip`, `rate_limited_email`, `internal_error` | `MagicLinkInviteService::send_login_link`, `auth_handler::send_magic_link`, `magic_link_handler::resend_magic_link` |
| `auth.magic_link_redeem`           | `redeemed`, `token_not_found`, `token_used`, `token_expired`, `account_deactivated`               | `MagicLinkInviteService::redeem`                         |
| `user_profile.rejected`            | `external_no_relationship`, `target_external_hidden`, `target_hidden`                              | `AuthApplicationService::get_user_profile`               |
| `grants.email_invite`              | `rate_limited`                                                                                    | `grant_handler::create_grant`                            |
| `authz.external_user_blocked`      | `internal_only_surface`                                                                            | `require_internal_user_layer` (CalDAV / CardDAV / WebDAV) |
| `auth.nc_basic_rejected`           | `external_user`                                                                                    | `basic_auth_middleware` (NC Basic-Auth surface)          |
| `auth.app_password_create_rejected`| `external_user`                                                                                    | `create_app_password`                                    |
| `groups.search_rejected`           | `external_user`                                                                                    | `search_groups`                                          |

The convention (see CLAUDE.md § Authorization) is: any branch that denies or rejects a request **must** emit an audit event before returning the user-facing response. Anti-enumeration is preserved at the API surface (uniform response shape, 404 not 403), and the true reason is recorded only in the audit channel.

## Rate limits

Three caps protect the magic-link surface. Each is a moka sliding-window counter; the keys differ.

| Cap                                          | Keyed on                       | Default   | Env var                                              | Visible on hit? |
|----------------------------------------------|--------------------------------|-----------|------------------------------------------------------|------------------|
| Per-sharer email-invite                      | `caller_id`                    | 50 / hour | `OXICLOUD_MAGIC_LINK_INVITE_PER_CALLER_PER_HOUR`     | yes (429 + `Retry-After`) |
| Per-target-email send                        | normalised email               | 5 / hour  | `OXICLOUD_MAGIC_LINK_SEND_PER_EMAIL_PER_HOUR`        | no (uniform 200) |
| Per-source-IP send (backstop)                | trusted client IP              | 200 / hour | `OXICLOUD_MAGIC_LINK_SEND_PER_IP_PER_HOUR`           | no (uniform 200) |

The two send caps are shared between `POST /api/auth/magic-link/send` and `POST /magic/v1/{token}/resend` — they're the same moka counters, so an attacker can't double their budget by alternating endpoints.

Two distinct visibility regimes:

- **Authenticated callers** see 429 when they hit a cap, because their own rate-limit state leaks nothing about other accounts. The invite cap is in this regime.
- **Anonymous callers** never see 429 on the send endpoint — the status itself would become an enumeration oracle ("this email has been probed recently → probably has an account"). The two send caps are silently absorbed: the response stays the uniform 200, and the real reason is recorded in the audit channel.

An authenticated caller resending to themselves bypasses both send caps. The bypass signal is "presence (not validity) of an Authorization header or access cookie" — a stale-cookie holder gets a 401 from any other endpoint they touch, so the worst-case bypass is narrow.

The per-IP backstop respects `OXICLOUD_TRUST_PROXY_CIDR` for client IP resolution: behind a reverse proxy, the upstream IP from `X-Forwarded-For` (leftmost) is used; without a configured trusted CIDR, the proxy's IP would be used and the backstop would effectively become a single bucket.

## Defence-in-depth boundary protections

External users are a new principal kind, and several pre-existing surfaces would over-share once they appeared. The protections fall in two layers — service-level filters that every surface inherits, and route-level layers that close protocol surfaces with no semantic meaning for externals.

### Service-layer filters (every surface inherits these)

1. **Subject groups reject external members.** `subject_group_service.rs::add_member` short-circuits if the candidate user has `is_external = TRUE`. Otherwise an admin could add `alice@example.com` to "Engineering", which later receives a grant on internal-only resources — silent privilege escalation. Mirrors the no-external-admins enforcement.
2. **System contacts hide externals by default.** `auth_service.list_users` and `auth_service.search_users` take `include_external: bool`, defaulting to `false`. The share modal autocomplete (via `/api/address-books/system/contacts`) therefore never surfaces external users to internal callers, and external users never see internal users at the address book layer.
3. **External users are excluded from the Internal virtual group.** `pg_acl_engine.rs::expand_user` no longer inserts `INTERNAL_GROUP_ID` for users with `is_external = TRUE`. The group's name finally honours its semantics; every grant addressed to "all internal users" is now genuinely internal-only.

### Route-level lockouts (close protocol surfaces upfront)

External users have no calendar, no address book, no home folder, and (by design) no persistent credential. The protocol surfaces that assume those things are closed to them at the middleware layer — before any handler runs:

4. **`/caldav/*`, `/carddav/*`, `/webdav/*`** are wrapped with `require_internal_user_layer` in `main.rs`. The layer runs after `auth_middleware`, reads the populated `CurrentCaller` from request extensions, calls `require_internal_user` once per request, and 403s + audit-logs on rejection. PROPFIND / REPORT / OPTIONS — every DAV verb is closed.
5. **NextCloud `/remote.php/*` and `/ocs/*`** are gated inside `basic_auth_middleware`: after a successful app-password match, a follow-up lookup checks `is_external` and returns 401 if true. This is belt-and-braces — externals can't create app passwords in the first place (next item) — but it covers users who later flip to `is_external` after creating one.
6. **`POST /api/auth/app-passwords` is closed.** App passwords are persistent credentials; the magic-link-eligibility rule (`has_login_credential`) assumes externals have **no other credential configured**. Letting an external mint an app password would break that invariant and would also be the only way to authenticate them on the NC surface. 403 + audit on rejection.
7. **`GET /api/groups/search` is closed.** Group names aren't strictly secret, but externals have no legitimate use for the share-dialog autocomplete (they can't be added to groups anyway).

Pre-existing safeguards from the user-lifecycle work continue to apply: the DB CHECK constraints `users_external_not_admin` and `users_external_no_storage`, and the `PersonalDriveLifecycleHook` short-circuit that skips drive provisioning for externals (they get no default drive, so `DriveRepository::home_root_folder_id_for(external_user_id)` returns `Ok(None)`).

### Why protocol-level instead of handler-level

The route layer is one `require_internal_user_layer` per nest rather than one check per handler. Three reasons:

- **Coverage.** Every DAV verb (and every NC OCS endpoint) is gated in one place. New handlers added under the same nest inherit the protection automatically.
- **Cost.** The layer hits the DB once per request (already cached in moka under the hood); a per-handler check would do the same work without the reuse.
- **Auditability.** A single audit event (`authz.external_user_blocked` with the request `path`) covers the whole subtree. Operators can grep one `event=` value across all DAV traffic.

## Kill switches and feature scoping

| Knob                                   | What it does                                                                                          |
|----------------------------------------|--------------------------------------------------------------------------------------------------------|
| `OXICLOUD_ALLOW_EXTERNAL_USERS=false`  | Coarse off-switch. `POST /api/grants` rejects email-typed subjects for unknown emails; send endpoint returns the uniform stub without issuing a token. Pre-existing externals continue to function. |
| `OXICLOUD_EXTERNAL_EMAIL_DOMAINS=…`    | Fine-grained allowlist of accepted domains for new external users. Empty = no restriction. Exact-match (case-insensitive) on the post-`@` part — `partner.com` does NOT match `eng.partner.com`. |
| `OXICLOUD_SMTP_*` unconfigured         | The whole magic-link feature is unavailable. Endpoints that depend on it return `503 Service Unavailable` with a clear message. |
| `OXICLOUD_MAGIC_LINK_TTL_HOURS`        | Token lifetime. Default 24 hours. Shortening it raises the resend rate; lengthening it raises the window for token theft. |

The send endpoint **does** return 503 (not the uniform 200) when SMTP is entirely unconfigured: the absence of the feature is visible from any other `/api/auth/magic-link/*` route anyway, so hiding the 503 leaks nothing the attacker could not learn elsewhere.

## What is deliberately out of scope

These are intentionally deferred. Each has a clear future trigger; none block the present design.

- **`auth.user_auth_methods` side-table.** Replaces the placeholder-string smell. `has_login_credential()` is the single migration point.
- **Email-locale routing.** v1 ships English-only invitation templates. A future PR adds recipient-locale detection (Accept-Language at send time, or stored preference) and a template engine.
- **MX-record validation at share time.** Regex is the only pre-send check; bad domains surface via SMTP bounce.
- **Dormant external user sweeper.** Purges users with no `last_login_at` for 13+ months. The GDPR-purge variant in `UserLifecycleHook::on_user_deleted` is its hook entry point.
- **`OXICLOUD_EXTERNAL_USERS_CAN_RESHARE=false`.** Forbids externals from being a grant's `granted_by`. Today an external with `Permission::Share` can mint more externals — a soft policy worth tightening but not load-bearing.
- **Differentiated session TTL for externals.** Uniform across all users today. Future env: `OXICLOUD_EXTERNAL_REFRESH_TOKEN_EXPIRY_DAYS`.
- **`session_kind` on sessions emitted from magic-link.** Enables scoped sessions ("magic-link session can only access granted resources, not the user's own folders"). External users have no own folders so the practical exposure is small.
- **Admin-list-users that includes externals.** Today `list_users` filters externals by default. A future admin UI for managing externals (rename, deactivate, view their grants) will need an `include_external` query param.
- **Open Cloud Mesh (OCM) federation.** A separate path for external identity; `ExternalIdentityLifecycleHook::on_user_created` accommodates a `source` discriminator (`magic_link` / `oidc` / `ocm`).
- **WebAuthn / passkey enrolment.** Distinct future feature; magic-link is the bootstrap.
- **Bounce tracking.** No webhook listener for SES-style bounce notifications. A future `on_email_bounce` event would surface "this user's email is dead" in admin UI.

## Related documents

- [User lifecycle](/architecture/user-lifecycle) — the hook framework that fires on user creation and the deletion modes.
- [ReBAC Authorization](/architecture/rebac-authorization) — how grants are evaluated against `auth.users` rows (including external ones).
- [Share Integration](/architecture/share-integration) — how the public-share-link flow relates to the email-invite flow (both create `role_grants` rows; only the former lives in `storage.shares`).
- [Environment Variables](/config/env) — the full set of `OXICLOUD_*` knobs.
