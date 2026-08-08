# Federated Login — Autodiscovery Without RP Registration

The goal: a user types `alice@example.com` (or a URL), OxiCloud discovers
their identity provider via a well-known probe on `example.com`, initiates
whatever auth flow that provider speaks, and gets back a stable
`(issuer, subject)` pair. **Zero pre-registration of OxiCloud with every
provider.** Fully-meshed federation — no social-login intermediary.

This is separate from [OCM](./ocm.md) which federates SHARED RESOURCES
between clouds. Federated login federates IDENTITY: authenticating a user
whose home isn't ours.

## Why "no RP registration" matters

Every social-login flow today (Google Sign-In, Sign in with Apple, GitHub
OAuth) requires the RP to register upfront in the provider's admin console:
get a client_id, configure redirect URIs, get approval. This works for
centralized RPs but breaks the mesh: every OxiCloud instance would need to
register with every user's IdP.

The dream is what BrowserID *almost* was — RP discovers identity
provisioning from the user's domain, no bilateral setup, works for anyone
who hosts an identity endpoint.

## Landscape — sorted by fit and status

### Alive and directly-useful

**IndieAuth (W3C, active spec)** — literal answer to the "no registration"
ask. Discovery via HTML `<link rel="authorization_endpoint">` on the
identity URL. The **RP's URL IS its client identifier** — no registration
step. Auth flow returns a `me` URL as canonical identity.
- Ecosystem: strong in IndieWeb (Aperture, Micropub servers, IndieAuth.com).
- Adoption outside IndieWeb: thin.
- Best for: small-mesh federation of self-hosted OIDC-adjacent IdPs.
- Fails for: users whose IdP is Google / Microsoft.

**WebFinger (RFC 7033)** — user-supplied discovery.
`GET https://example.com/.well-known/webfinger?resource=acct:alice@example.com`
returns a JRD listing the identity's endpoints, including
`rel="http://openid.net/specs/connect/1.0/issuer"` → OIDC issuer URL.
- NOT a login mechanism, just a discovery layer.
- Deployed at scale (Mastodon uses it for account discovery).
- Pair with OIDC or IndieAuth for the actual login.

**OIDC Dynamic Client Registration (RFC 7591 + OIDC Registration 1.0)** —
machine-driven RP registration. RP POSTs to `registration_endpoint`
(advertised in OIDC discovery) with its metadata, gets back `client_id` /
`client_secret`. Fully server-to-server, no admin console.
- Support matrix:
  - ✅ Keycloak, Authentik, Zitadel, Kanidm, Ory Hydra
  - ⚠️ Auth0, Okta (with policy configuration)
  - ❌ Google, Microsoft/Entra, Facebook (disabled by policy)
- The pragmatic bridge between "no registration" and "OIDC ecosystem."
  Works with essentially every self-hosted OIDC server.

**OpenID Federation 1.0 (finalized 2024)** — the STANDARDIZED future.
Each entity publishes a signed entity statement at
`/.well-known/openid-federation`; trust chains built through federation
authorities (trust anchors). Designed for RP-IdP federation at scale
without pairwise registration.
- Adoption: EU eIDAS 2.0 wallets, GAIN identity network (banking).
- Complexity: needs a trust-anchor infrastructure to be meaningful.
- Timeline for OxiCloud: watch, don't implement Day 1.

**Solid-OIDC** — Solid project's OIDC extension where `client_id` is a
dereferenceable URL. Same trick as IndieAuth applied to full OIDC.
- Deployed in Solid ecosystem (Inrupt); minimal outside it.
- Reasonable to support as a variant of the IndieAuth strategy if demand
  materialises.

### Dead or dying — do not build against

**Mozilla Persona / BrowserID (2011-2016)** — was almost exactly this
plan. RP fetched `example.com/.well-known/browserid`, got the primary
IdP's signing key, redirected user to primary, primary signed an assertion
`{email, iss, sub}`, RP verified signature. Zero RP registration. Elegant
design. **Killed by mainstream mail providers refusing to run primaries**;
Mozilla's fallback (their own primary verifying via email link) undermined
the security story. Lesson for us: any scheme requiring the user's domain
to host an identity endpoint depends on domain operators playing along —
they mostly don't for public email.

**OpenID 2.0 (2007-2014)** — original OpenID with XRDS discovery,
user-entered URL as identifier, full autodiscovery, no client registration
required. Killed by OIDC (which requires registration). Some legacy
support in Drupal, MediaWiki. Do not build new.

**XRI / i-names / OpenXRI** — attempted "personal domain" identifiers with
`=name` / `@name` prefixes. Never gained traction. Standards body
dissolved. Dead.

**WebID+TLS** — Solid predecessor. Client certificate-based, no client_id
needed at all. Academic circles only. Dead outside research.

**SXIP** — early single-sign-on protocol, pre-OpenID. Dead.

**Self-Issued OP (SIOP) v1** — wallet-based auth where the user's device
is the IdP. Cool idea, too complicated for a browser-only flow, requires
wallet software installed. **v2 is emerging** in EU eIDAS 2.0 context but
not for our use case.

**Verifiable Credentials + SIOP v2** — the OIDC-flavored VC world. Right
now: infrastructure-heavy (trust registries, credential formats), user
must have a wallet. Watch for the eIDAS 2.0 rollout in 2026-2027; not yet
right for us.

### Adjacent but not this problem

**OCM invitation flow** — federates SHARED RESOURCES, not identity. See
[ocm.md](./ocm.md). Different plan.

**OAuth 2.0 device flow (RFC 8628)** — solves "log in on a TV using your
phone." Not autodiscovery.

## OxiCloud strategy — WebFinger + strategy-per-provider

Compose the alive-and-useful pieces:

```
alice@example.com
  ↓
discover(identifier)
  ├─ WebFinger probe on example.com (RFC 7033)
  │    → JRD lists rel="…/openid/1.0/issuer" → OIDC issuer URL
  │    → JRD lists rel="…/indieauth/…" → IndieAuth authorization_endpoint
  │    → both = choose OIDC (richer), fall back to IndieAuth on OIDC failure
  │
  ├─ If WebFinger returns 404 / no useful rel:
  │    HTML fetch of identity URL, parse <link rel="authorization_endpoint">
  │    → IndieAuth flow
  │
  └─ Else: hard-fail with actionable message
       "No autodiscovery signal at example.com. Options:
        (a) ask your admin to enable WebFinger/OIDC on your domain;
        (b) ask this OxiCloud admin to pre-register example.com;
        (c) use a supported IdP account."
```

**Strategy A — OIDC + Dynamic Registration:**

```
Discovered issuer URL → fetch /.well-known/openid-configuration
  ↓
Cache lookup: auth.oidc_dynamic_clients WHERE issuer = ?
  ├─ hit  → reuse cached client_id + client_secret
  └─ miss → POST registration_endpoint {redirect_uris, client_name, …}
            get client_id + client_secret → cache row
  ↓
Normal OIDC authorize / callback / token flow (PKCE, nonce, etc.)
  ↓
id_token → federation_kind='oidc', federation_issuer=iss, federation_subject=sub
```

**Strategy B — IndieAuth (fallback):**

```
Discovered authorization_endpoint URL (and token_endpoint from same discovery)
  ↓
Redirect user with client_id = OxiCloud's own URL, redirect_uri, state, PKCE
  ↓
User consents on their IdP
  ↓
Callback with code → POST to token_endpoint
  ↓
Response has { me: "https://alice.example.com" }
  ↓
Store federation_kind='indieauth',
      federation_issuer=example.com (domain from `me`),
      federation_subject=me URL
```

**Strategy C — pre-registered (existing OIDC config):** the current
single-IdP configured via `OXICLOUD_OIDC_ISSUER_URL` etc. Continues to
work; discovery bypassed for that specific issuer. Handles Google /
Microsoft cases where dynamic registration isn't available — admin
pre-registers, users still get autodiscovery for the "which issuer" step.

## Schema compatibility — the identity triple already covers this

The federation-identity model from
[ocm.md § Identity & auth model](./ocm.md) uses
`(federation_kind, federation_issuer, federation_subject)`. **All the alive
strategies fit cleanly** — no schema surgery per strategy, just one CHECK
constraint update to admit new `federation_kind` values.

| Strategy | federation_kind | federation_issuer | federation_subject |
|---|---|---|---|
| Magic-link (existing) | `magic_link` | NULL | NULL (identity is local `email`) |
| OIDC (existing + dynamic) | `oidc` | id_token `iss` (issuer URL) | id_token `sub` |
| IndieAuth | `indieauth` | domain from `me` URL | full `me` URL |
| OCM (planned) | `ocm` | peer domain | federated address |
| OpenID Federation (future) | `openid_fed` | entity-statement `sub` (issuer) | subject id |

Extending the enum when a strategy lands is a one-line migration:

```sql
ALTER TABLE auth.users DROP CONSTRAINT users_federation_kind_check;
ALTER TABLE auth.users ADD  CONSTRAINT users_federation_kind_check
    CHECK (federation_kind IN ('magic_link','oidc','indieauth','ocm','openid_fed'));
```

BCL revocation, anti-duplicate lookup, session middleware, and the AuthZ
engine all key on the triple — kind-blind. Adding IndieAuth or OpenID
Federation touches ONLY the discovery + auth-flow code, never the identity
storage or authorization layers.

## What we'd add on top

**For Strategy A:**

```sql
CREATE TABLE auth.oidc_dynamic_clients (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    issuer          TEXT NOT NULL UNIQUE,   -- keyed on issuer URL
    client_id       TEXT NOT NULL,
    client_secret   TEXT NOT NULL,          -- encrypted at rest (blob-encryption path)
    registered_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata        JSONB                   -- full registration response, for audit
);
```

Cache of "IdPs we've dynamically registered with." First login per issuer
pays ~500ms for the registration round-trip; subsequent logins hit the
cache.

**For Strategy B:** no new table. Client identifier is our URL, discovery
metadata is small enough to cache in moka with a short TTL.

**For discovery layer (shared):** small in-memory + DB cache of
`(identifier → strategy + endpoint URLs)` with a TTL matching provider
metadata freshness (typical: 1 hour for OIDC discovery per its `Cache-Control`
guidance).

## Trust boundary — policy switch, not a hard-coded stance

Open discovery = we accept logins from ANY IdP on ANY domain. Three modes,
per admin choice:

**Open federation** (default for personal / community deployments) — allow
any WebFinger-discovered IdP. Trust is per-user (user chose to authenticate
via that IdP; we trust their choice). Simplest UX, largest attack surface.
A malicious IdP can mint arbitrary `sub`s and claim any identity WITHIN
ITS OWN DOMAIN (can't impersonate other domains — the `(iss, sub)` key
scopes damage). This is the same trust posture as accepting any email
address from an SMTP server today.

**Allowlist federation** (default for enterprise) — admin-managed
`ocm.trusted_peers` (reuse the OCM peer allowlist, or a sibling table
`auth.trusted_federation_domains`). Only auto-discover for listed
domains. Kills the "just enter your email" UX but bounds the attack
surface.

**Hybrid** — allow-by-default with per-user quotas + rate limits + admin
visibility (`GET /api/admin/federation/discovered_peers` shows every
domain we've ever registered against, with revoke button). Middle ground.

Config knob:

```
OXICLOUD_AUTH_FEDERATION_MODE=open|allowlist|hybrid  # default: hybrid
OXICLOUD_AUTH_FEDERATION_ALLOWLIST=domain1.com,domain2.com   # allowlist mode
```

## Same discovery layer serves invitation

The `discover(identifier) → Strategy` function is not just for login — it
answers the same question at INVITE time. When a local user shares with
`alice@example.com`, the invite flow probes her domain and picks the
strongest available channel:

```
share_with_external(alice@example.com)
  ↓
discover(alice@example.com)
  ├─ OCM signal on example.com  → OCM outbound share (resource stays here,
  │                                 she consumes via her own cloud). Grant
  │                                 tied to `federation_kind='ocm'`.
  ├─ OIDC + dynamic reg supported → mint a magic-link that triggers OIDC
  │                                  auth on redemption. Grant tied to
  │                                  `federation_kind='oidc'` on first login.
  ├─ IndieAuth signal            → magic-link that triggers IndieAuth on
  │                                  redemption. Grant tied to
  │                                  `federation_kind='indieauth'`.
  ├─ OpenID Federation           → same idea, `federation_kind='openid_fed'`.
  └─ Nothing discoverable        → traditional email magic-link, mailbox-
                                    strength trust. Grant tied to
                                    `federation_kind='magic_link'`.
```

**Preference ordering** (strongest → weakest):

| Channel | Trust source | Why higher |
|---|---|---|
| OCM | Peer server + user's own auth on their cloud | Resource federation + strong identity |
| OpenID Federation | Signed entity statement + trust anchor chain | Cryptographic RP-IdP trust |
| OIDC + dynamic reg | User's IdP (potentially MFA, hardware keys) | IdP-mediated identity |
| IndieAuth | User's domain (same domain owns identity + endpoint) | Domain-owner attestation |
| Magic-link | Mailbox possession only | Weakest — no MFA, phishable |

The share dialog UX shows a small badge on the resolved recipient
indicating the channel that will be used: `Federated cloud` (OCM),
`SSO via IdP.example.com` (OIDC), `IndieAuth on example.com`, or
`Email link` (magic-link fallback). Sender sees the trust posture
before sending; receiver's invite email / OCM notification reflects
the same.

**Cache the discovery outcome per identifier for a short TTL** (e.g. 24h
with revalidation) so repeated invites to the same person don't repeat
the probe. Cache lives in `auth.federated_identity_discovery` with
columns `(identifier, discovered_kind, discovered_endpoints, cached_at,
expires_at)`. Invalidated eagerly if the invite fails (endpoint
disappeared, IdP rejected registration) — next attempt re-probes.

**Trust-mode enforcement applies here too.** In `allowlist` mode, if a
recipient's domain isn't on the allowlist, invitation falls back to
magic-link regardless of what discovery finds. In `hybrid`/`open` mode,
discovery is trusted per-user with rate limiting.

**Existing external-invite code paths converge here.** Today's magic-link
invitation flow (external user gets emailed a redemption link) becomes
the "Nothing discoverable" branch. No change to the redemption side —
still creates an `is_external=true` shadow user. New paths simply set a
richer `federation_kind` on that same shadow user, which downstream
enables the stronger auth channel on next login attempt.

## Interactions with existing auth flow

- **AUTH_METHODS** — add `federated` as a new allowlist token, analogous
  to `oidc`. `OXICLOUD_AUTH_METHODS=password,federated` means password
  login OR autodiscovered federated login. Same fail-fast semantics as
  the existing OIDC token.
- **OIDC-master rule** — `federation_kind='oidc'` OR
  `federation_kind='indieauth'` OR `federation_kind='openid_fed'` all
  short-circuit magic-link login for that user. Federation-authenticated
  users authenticate through their federation. See
  [ocm.md § Magic-link on federated addresses](./ocm.md).
- **JIT provisioning** — same code path as current OIDC JIT-provisioning:
  create a shadow user on first successful federated login, `is_external`
  handling depending on whether they should get local storage
  (`OXICLOUD_AUTH_FEDERATION_JIT_GRANTS_STORAGE=false` by default).
- **RP-initiated logout** — reuses the existing `end_session_endpoint`
  discovery + `id_token_hint` flow for OIDC. IndieAuth has no standard
  logout; local session revocation only.
- **Back-channel logout** — reuses the existing BCL endpoint; the
  identity key is now unambiguously the `(iss, sub)` pair (fixed by the
  federation_issuer rename planned in [ocm.md](./ocm.md)).

## Phasing (rough)

**Phase 0 — Prerequisite:** federation-identity schema rename (documented
in [ocm.md § Schema rename](./ocm.md)). Land first, no matter which
federated-login strategy comes next.

**Phase 1 — WebFinger + OIDC + Dynamic Registration.** Covers the
self-hosted OIDC mesh case. ~1-2 weeks. Highest immediate value; runs
on top of the existing OIDC flow.

**Phase 2 — Discovery UX in the login form.** Detect email vs URL,
run discovery on submit, show progress spinner while discovering,
graceful fallback if no signal. ~3-5 days FE.

**Phase 3 — IndieAuth strategy.** Small addition once the discovery
framework exists. ~1 week.

**Phase 3b — Discovery reused at INVITE time.** The invitation flow
runs the same probe and dispatches to OCM / OIDC / IndieAuth /
magic-link based on preference ordering. Recipient badge in the share
dialog shows the chosen channel. Cache table
`auth.federated_identity_discovery`. ~5-7 days including UX.

**Phase 4 — Admin panel for discovered peers.** List, revoke, force
rebind. ~3-5 days.

**Phase 5 — Trust-mode policy switches.** Config knobs +
allowlist-mode enforcement. ~2-3 days.

**Phase 6+ — OpenID Federation 1.0.** Add when trust anchor
infrastructure matures externally or a deployment specifically needs it.

## Open questions

- **Federated JIT and quotas** — federated users creating drives? Or
  strictly grant-only externals? Default to grant-only; per-domain
  policy could allow drive creation for trusted domains only.
- **Discovery-cache invalidation** — when an IdP rotates keys, our
  cached JWKS goes stale within the OIDC service's TTL (already
  handled). Discovered issuer + registered client should also expire
  eventually; add a periodic refresh job.
- **Multiple discovery signals on the same domain** — WebFinger says
  IdP is X, HTML `<link>` says Y. Rule: WebFinger wins (RFC-specified
  discovery is authoritative over convention).
- **Rate limiting** — discovery probes are cheap but can be abused.
  Per-caller-IP + per-target-domain rate limits.
- **Anti-typo** — user types `alice@gogle.com`, we discover Gogle's IdP
  and successfully register with it. That's a working federated login
  to a lookalike domain. Mitigation: display the discovered issuer in
  the consent screen ("You will authenticate at
  `https://accounts.gogle.com`"). Not fixable server-side alone;
  user-side confirmation is the last defense.

## Non-goals

- Persona-style browser-mediated flow (dead)
- Verifiable credentials / SIOP v2 (too early)
- OpenID 2.0 backwards compatibility (deprecated a decade ago)
- Social-login integration (Facebook / GitHub / Twitter buttons) —
  intentionally out of scope; those require explicit RP registration and
  a per-provider adapter, not federated identity

## References

- WebFinger: RFC 7033
- OIDC Discovery 1.0: <https://openid.net/specs/openid-connect-discovery-1_0.html>
- OIDC Dynamic Client Registration: RFC 7591 + <https://openid.net/specs/openid-connect-registration-1_0.html>
- IndieAuth: <https://indieauth.spec.indieweb.org/>
- OpenID Federation 1.0: <https://openid.net/specs/openid-federation-1_0.html>
- Solid-OIDC: <https://solid.github.io/solid-oidc/>
- BrowserID postmortem: <https://wiki.mozilla.org/Identity/Persona_Shutdown_Guidelines_for_Reliers>
