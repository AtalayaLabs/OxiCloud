# OIDC Account Linking — Self-Service Link / Unlink

Logged-in local users self-serve the wiring of an OIDC identity to their
account (no admin round-trip / manual SQL). Companion: unlink for users
who want to detach the OIDC identity while keeping their local login.

Design context: builds on the federation-identity rename
([ocm.md § Schema rename](./ocm.md)) — a linked identity is
`(federation_kind='oidc', federation_issuer=<iss URL>, federation_subject=<sub>)`
on `auth.users`.

## UX flow — auto-link on first OIDC login (majority case)

Handles the case where a user already has a local OxiCloud account and
tries "Sign in with SSO" (or is auto-redirected under the standalone-OIDC
policy) for the first time. Without auto-link, today's flow refuses with
"A user with email X already exists — contact admin to link your OIDC
identity" — forcing an admin round-trip or the self-service link flow
below. Auto-link removes that friction for the common case:

**Trigger:** OIDC login callback lookup misses on `(iss, sub)` AND on
the legacy-label fallback (Phase B), but the IdP-returned email matches
an existing OxiCloud user.

**Decision tree** (all checks under normalized-email comparison):

```
Look up user by normalize(claims.email):
  ├─ exactly 1 match + email_verified=true + user not already linked
  │    → AUTO-LINK: UPDATE federation_kind='oidc', issuer=iss, subject=sub
  │    → emit `federation.auto_linked` audit event
  │    → proceed with login as this user
  ├─ 1 match + email_verified=false
  │    → refuse (`auto_link_email_not_verified`)
  ├─ 1 match + already linked to a DIFFERENT identity
  │    → refuse (`already_linked_elsewhere`)
  ├─ >1 match (ambiguous under +alias normalization)
  │    → refuse (`email_ambiguous`)
  └─ 0 matches
       → existing JIT-provisioning branch (creates a fresh user)
```

**Refusals fall through to the current "contact admin" error page**
(same shape as before this feature). Users can then self-serve via the
link flow below, or the admin can intervene.

### Security model — why auto-link is safe here

The classic account-takeover attack: attacker creates a rogue IdP
account with the victim's email → OIDC login → auto-link → hijacks the
victim's OxiCloud account.

Mitigation is the industry-standard **`email_verified=true` gate**: the
IdP itself has verified the user controls the email, so the attacker
can't just claim any email in their own IdP account.

Safe in OxiCloud's current single-IdP model because:
- Admin explicitly configures ONE trusted IdP (`OXICLOUD_OIDC_ISSUER_URL`)
- Same trust chain as JIT auto-provisioning today (which we already
  gate on `email_verified` when `OXICLOUD_REQUIRE_VERIFIED_EMAIL=true`)
- The IdP is chosen by the admin, not the user

**Explicitly NOT safe** for the future multi-IdP federated login
(`docs/plan/federated-login.md` — any WebFinger-discovered IdP is
accepted). That flow needs different rules: allowlisted-IdP-only
auto-link, or no auto-link at all. Deferred until that lands.

### Config knob

```
OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH=true   # default TRUE
```

Ships enabled — it's the good UX. Admins with compliance requirements
that mandate explicit consent for every link opt out. `false` restores
the "refuse with contact admin" behavior; self-service link flow (below)
remains available.

Documented as a deployment-config knob in **THREE places** — all must
be updated when this env var lands (per the project convention that
every env var appears in every config-reference surface):

- `example.env` — commented entry under the OIDC block with the
  default value + one-sentence explanation
- `docs/config/env.md` — table row in the OIDC section
- `docs/config/oidc.md` — narrative paragraph explaining the
  auto-link decision tree and security model (short form of the
  section above)

## UX flow — link

1. User logged in via password/OPAQUE lands on `/profile`
2. Sees a **"Connect Single Sign-On"** card, visible when
   `oidc.enabled && !user.federation_kind`
3. Clicks button → SPA `POST /api/auth/oidc/link/start` (authenticated)
   → backend mints a state token, stores a `pending_oidc_flow` entry with
   `intent = Link { user_id }`, returns `{ authorize_url }`
4. Full-page navigation to the IdP → user authenticates as themselves
5. IdP redirects back to `/api/auth/oidc/callback?code=&state=`
6. Callback recognises the `Link` intent (via state cache lookup) →
   exchanges code → validates id_token → runs safety checks (below) →
   UPDATE user row
7. Redirect to `/profile?linked=1` — SPA shows a toast and strips the
   query param

## UX flow — unlink

1. User (currently OIDC-linked AND with alternative auth wired) sees a
   **"Disconnect Single Sign-On"** card on `/profile`
2. Clicks button → SPA `POST /api/auth/oidc/unlink`
3. Backend refuses if the user has no other auth method (see below)
4. On success: profile refreshes, `federation_kind`/`issuer`/`subject`
   become `null`, "Connect SSO" card takes over the space

## Safety checks — link callback

Ran BEFORE the UPDATE. Any refusal returns
`/profile?link_error=<stable-key>` with the reason logged internally.

| Check | Refuse reason | Rationale |
|---|---|---|
| Session valid (state's `user_id` matches an active session) | `session_expired` | Cookie invalidated during the IdP round-trip; treat as auth failure |
| IdP-returned email matches OxiCloud email after normalization | `email_mismatch` | Prevents "link Bob's identity to my account, then Bob logs in via OIDC and lands here" |
| IdP provided an email at all | `email_not_provided` | Without email we can't verify identity ownership — refuse |
| Identity `(kind, iss, sub)` not already linked to a DIFFERENT user | `already_linked_elsewhere` | Prevents linking the same OIDC identity to two OxiCloud accounts |
| Current user isn't linked to a DIFFERENT identity | `already_linked` | User must unlink first — no silent identity swap |
| Same identity as currently-linked → idempotent success | (no error) | Repeat link is a no-op success |

Optional deferred: step-up auth (require fresh password/OPAQUE
verification within the last N minutes before starting link). Guards
against session-theft → link-attack. Add if we care.

## Email normalization

`common::text::normalize_email_for_link`:

```rust
pub fn normalize_email_for_link(email: &str) -> String {
    let lower = email.trim().to_ascii_lowercase();
    let Some((local, domain)) = lower.split_once('@') else {
        return lower;
    };
    // Strip +alias sub-addressing (Gmail / Outlook / Fastmail / etc.):
    // alice+github@example.com → alice@example.com
    let local_base = local.split_once('+').map(|(b, _)| b).unwrap_or(local);
    format!("{}@{}", local_base, domain)
}
```

**NOT** doing dot-stripping (Gmail-only, causes false positives on other
providers). NOT doing Unicode normalization (email addresses compare as
ASCII-normalized already).

Behavior matrix:

| OxiCloud email | IdP email | Match? |
|---|---|---|
| `alice@example.com` | `alice@example.com` | ✅ |
| `alice@example.com` | `Alice@example.com` | ✅ (case) |
| `alice@example.com` | `alice+oidc@example.com` | ✅ (alias) |
| `alice+work@example.com` | `alice@example.com` | ✅ (alias both) |
| `alice@example.com` | `bob@example.com` | ❌ |
| `alice@example.com` | (missing) | ❌ (`email_not_provided`) |

Legitimate-but-refused cases (documented, admin unlinks+relinks):
- User changed email on IdP but not on OxiCloud
- User's IdP email uses a different domain than OxiCloud email

## Unlink refusal — retain a working direct login

`POST /api/auth/oidc/unlink` refuses when the user has **no other
credential** to log in with:

```
if !user.has_password() && !user.opaque_registered() {
    return AccessDenied("cannot_unlink_no_alternative_auth");
}
```

Rationale: OIDC-only account unlinking creates a passwordless account
with no OIDC either → the user can't log in AT ALL. Magic-link isn't a
safe fallback since (a) it's gated by SMTP wiring and (b) the
OIDC-master rule wouldn't refuse it AFTER unlink but does BEFORE, so
users could be surprised by inconsistent behavior. Refusing at the API
layer forces the user to add a password first (via profile change-
password card) before unlinking.

`opaque_registered` counts as an alternative because an OPAQUE envelope
IS a login credential.

## Wire changes — endpoints

| Method | Path | Auth | Body | Returns |
|---|---|---|---|---|
| `POST` | `/api/auth/oidc/link/start` | Bearer/cookie | `{}` | `{ authorize_url: "..." }` |
| `POST` | `/api/auth/oidc/unlink` | Bearer/cookie | `{}` | `200 OK` or `403` |
| `GET` | `/api/auth/oidc/callback` | Public | — | Extended: `?intent=link` cases redirect to `/profile?...` |

## Pending-flow cache extension

Existing `AuthApplicationService::pending_oidc_flows: Cache<String, PendingOidcFlow>`
gains an `intent` field:

```rust
enum FlowIntent {
    Login,
    Link { user_id: Uuid },
}

struct PendingOidcFlow {
    pkce_verifier: String,
    nonce: String,
    nc_flow_token: Option<String>,
    intent: FlowIntent,
}
```

Default `Login` preserves existing behavior. `Link` is set by
`prepare_oidc_link(user_id)`. The callback dispatches on the intent
variant.

## Repo methods

```rust
async fn link_federation_identity(
    &self, user_id: Uuid,
    kind: &str, issuer: &str, subject: &str,
) -> Result<(), UserRepositoryError>;
// Returns AlreadyExists on UNIQUE(kind, issuer, subject) violation —
// app service translates to `already_linked_elsewhere`.

async fn unlink_federation_identity(
    &self, user_id: Uuid,
) -> Result<(), UserRepositoryError>;
// UPDATE ... SET federation_kind = NULL, federation_issuer = NULL,
// federation_subject = NULL WHERE id = $1.
```

## Audit events

- `federation.link_started` — user_id, intent (self-service flow only)
- `federation.link_completed` — user_id, kind, issuer, subject
- `federation.link_refused` — user_id, reason (stable enum-shaped key:
  `session_expired`, `email_mismatch`, `email_not_provided`,
  `already_linked_elsewhere`, `already_linked`)
- `federation.auto_linked` — user_id, kind, issuer, subject,
  reason=`email_match_verified` (fired by the auto-link branch on the
  OIDC callback path — NOT by the self-service link flow)
- `federation.auto_link_refused` — user_id (if resolvable), reason
  (`auto_link_disabled`, `auto_link_email_not_verified`,
  `email_ambiguous`, `already_linked_elsewhere`)
- `federation.unlinked` — user_id
- `federation.unlink_refused` — user_id, reason (=`no_alternative_auth`)

Same anti-drift discipline as other structured audit events (per
`feedback_enum_over_string_literals_in_logs`).

## Hurl test coverage

`tests/oidc/link_unlink.hurl` under the OIDC runner:

**Auto-link scenarios** (OIDC login path with email match):

1. **Auto-link happy path** — Alice has a local account
   `alice@example.com`; the fake IdP is set to return that email with
   `email_verified=true`; Alice clicks "Sign in with SSO" → login
   completes → GET `/api/auth/me` shows `federation_kind = "oidc"` +
   correct issuer/subject. Audit line
   `event="federation.auto_linked", reason="email_match_verified"`
   emitted.
2. **Auto-link refused — email_verified=false** — fake IdP returns
   the matching email but with `email_verified=false`; login refuses
   with `auto_link_email_not_verified`.
3. **Auto-link refused — normalized email ambiguity** — two OxiCloud
   users exist (`alice@example.com` AND `alice+work@example.com`);
   fake IdP returns `alice@example.com`; both normalize to the same
   value; login refuses with `email_ambiguous`.
4. **Auto-link disabled by config** — separate suite with
   `OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH=false`; email match no longer
   auto-links; existing "contact admin" refusal returns. (Optional
   Phase-2 test — env-var flip requires a separate server boot.)

**Self-service link scenarios** (`POST /link/start` from an
authenticated session):

5. **Self-service happy path** — Alice logs in via password, POSTs
   `/link/start`, follows the authorize URL, IdP returns matching
   email, callback completes link, redirect to `/profile?linked=1`.
6. **Email mismatch refuse** — Alice starts link, `/control/set-email`
   on the fake IdP flips to `bob@example.com`; callback refuses,
   redirects to `/profile?link_error=email_mismatch`.
7. **+alias normalization link** — Alice's OxiCloud email is
   `alice@example.com`, fake IdP returns `alice+oidc@example.com` —
   link succeeds (both normalize to `alice@example.com`).
8. **Already linked elsewhere** — Alice links; Bob logs in and starts
   link; fake IdP returns Alice's identity (same sub); refused with
   `already_linked_elsewhere`.

**Unlink scenarios:**

9. **Unlink success** — Alice (linked via any prior scenario) POSTs
   `/unlink`; refresh shows `federation_kind = null`; Alice can still
   log in via password.
10. **Unlink refused** — a user with only OIDC (no password, no
    OPAQUE) tries to unlink; refused with `no_alternative_auth`.

## FE changes

`frontend/src/routes/profile/+page.svelte`:

- Import `getOidcProviders` (existing) to know if OIDC is enabled AND
  to resolve the display name.
- "Connect Single Sign-On" card: visible when
  `providers.enabled && !user.federation_kind`. Button → POST
  `/api/auth/oidc/link/start` → `window.location.assign(response.authorize_url)`.
- "Disconnect Single Sign-On" card: visible when
  `user.federation_kind === 'oidc' && (has_password || opaque_registered)`.
  Button → POST `/api/auth/oidc/unlink` → refresh session.
- On mount: read `?linked=1` → success toast; read `?link_error=<key>` →
  error toast with localized message per key; strip query params via
  `history.replaceState`.

## Scope / non-scope

**In scope for the first ship:**
- Link + unlink endpoints + safety checks
- Email normalization + tests
- Hurl coverage for 6 scenarios
- Profile-page UI

**Deferred:**
- Step-up auth before link start
- Admin-mediated link/unlink via `oxicloud federation` (proper for
  "user changed IdP email" recovery scenario)
- OCM link (same shape, different kind)
- Multi-federation (multiple linked identities per user — see
  ocm.md § Future — multi-federation per user)
