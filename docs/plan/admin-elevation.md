# Admin session elevation (sudo mode)

Branch: not started. Depends on `docs/plan/scope-gates.md` for
`Scope::Elevated`, but can ship standalone with a bespoke layer first.

Status: **DRAFT**

## Goal

Holding the admin role is not enough to **mutate** from the admin panel.
A destructive admin action requires the session to have recently proved
possession of a strong credential — a short-lived elevation, like
GitHub's sudo mode or Django's admin re-auth.

Three separate facts, today conflated into one:

| Fact | Question | Where it lives |
|---|---|---|
| Authentication | is this session valid? | `auth_middleware` |
| Authorization | does this account hold the admin role? | `require_admin` |
| **Elevation** | did *this session* recently re-prove itself? | **missing** |

The threat is a session that is valid and admin but not *presently*
operated by the admin: an unlocked laptop, a stolen access token, an XSS
payload riding an existing session, a shared browser profile. None of
those survive a re-auth prompt; all of them currently reach
`DELETE /api/admin/users/{id}`.

## Design

### Where the state lives — the session row

```sql
ALTER TABLE auth.sessions ADD COLUMN elevated_at TIMESTAMPTZ;
```

Not a JWT claim. The access token is short-lived and rotating, so a
claim would have to be re-minted on elevate, and — decisively — a claim
cannot be **revoked**. Elevation must drop instantly on password change
or role change, which only a row can do. This also matches the
`scope-gates.md` live-read rule: session-bound scope reads the session.

The per-request lookup is affordable precisely because this gates
*mutations* only, which are cold. `dpop_jkt` lives in the claim for the
opposite reason (hot path, `docs/plan/dpop.md` Gate 9) — the asymmetry
is deliberate, not an inconsistency.

`JwtClaims.sid` already carries `auth.sessions.id`, so the lookup key is
in hand; it needs surfacing onto `CurrentUser` (see `scope-gates.md`).

### What it gates — by HTTP method, not by enumeration

```
non-safe method (POST/PUT/PATCH/DELETE) under /api/admin/*  →  elevation required
GET/HEAD under /api/admin/*                                 →  admin role only
```

Method-based on purpose: a new admin mutation endpoint is covered the
day it is written, with nobody remembering anything. An enumerated list
of protected routes is the failure mode this whole line of work exists
to stop (see `scope-gates.md` § Why — the hole came from three layers
each assuming another held the gate).

**Reads stay unelevated.** Forcing a re-auth to merely *look* at the
panel is what makes operators disable the feature, and it buys little:
the reads are already admin-gated.

Escape hatch: a small allowlist for non-safe admin routes that are
operational rather than destructive (e.g. triggering a job run). Keep it
short and justify each entry in a comment — same shape as the allowlist
inside `require_no_password_change_pending_layer`. Start with **none**
and add only on a concrete complaint; deny-by-default means the list
grows from evidence, not from anticipation.

### Elevating — `POST /api/auth/elevate`

One endpoint, a branch per auth method, mirroring
`OXICLOUD_AUTH_METHODS`:

- **Password / OPAQUE** — re-submit the password. Reuses the login
  verification path; MUST go through the same rate limit and lockout as
  login (`LoginLockoutService`), or elevation becomes an unthrottled
  password oracle against a known-valid account.
- **OIDC** — no local password exists. Round-trip the IdP with
  `prompt=login` (or `max_age=0`) and stamp `elevated_at` on callback.
- **Magic link** — issue a link that elevates instead of minting a
  session. This is also the natural home for the
  `elevated_via_magic_link_at` idea in `docs/plan/password-reset`-era
  notes: same column, same TTL.
- **App password / Nextcloud / device-code sessions** — CANNOT elevate.
  No interactive channel exists. Admin mutations over those credentials
  are simply refused, which is acceptable because the admin API is not
  exposed over DAV/NC — but it must be stated, not discovered.

On success: `UPDATE auth.sessions SET elevated_at = NOW() WHERE id = $sid`.

### TTL and invalidation

- `OXICLOUD_ADMIN_ELEVATION_TTL_SECS`, default **900** (15 min).
- Elevation **survives** refresh-token rotation — same session row, and
  re-prompting every 15 minutes of token churn would be unusable.
- Elevation **drops** on: password change, role change (any direction),
  session revoke, logout. Each is an explicit `elevated_at = NULL`, not
  an inferred expiry.
- Elevation is per session: a second browser elevates separately. Two
  tabs on one session share it (they share the credential — expected).

### Refusal shape

```
403  error_type: "ElevationRequired"
```

A distinct `error_type`, not a generic 403: the SPA must switch on it
(`project_error_type_contract` — FE switches on `error_type`, never on
the message). Body also carries the TTL so the UI can say how long the
elevation lasts.

Audit (AGENTS.md § Audit logging):

- `authz.elevation_required`, `reason = "not_elevated" | "elevation_expired"`
- `auth.elevation_granted`, with the method used
- `auth.elevation_rejected`, `reason = "bad_password" | "locked_out" | "method_unavailable"`

### SPA flow — reuse the interceptor

`lib/api/client.ts` already does 401 → refresh → retry
(`project_fetch_interceptor`). Add a sibling branch: 403 +
`error_type === "ElevationRequired"` → open a re-auth modal → `POST
/api/auth/elevate` → **retry the original request once**. One retry
only; a second failure surfaces the error.

This means an admin's click is not lost to the prompt, which is the
difference between a feature operators keep and one they switch off.

## Rollout — the OIDC trap

Defaulting this ON in one step would **lock OIDC-only deployments out of
their own admin panel** until the `prompt=login` path exists. So:

- **Phase 1** — column, layer, `POST /api/auth/elevate` with the
  password/OPAQUE branch, SPA modal. Config defaults ON for deployments
  where a password method is available.
- **Phase 2** — OIDC re-auth branch. Only then does the default flip ON
  for `disable_password_login` deployments.
- Boot-time check: admin elevation required AND no elevation method
  available for the configured auth methods → **warn loudly** at
  startup, naming the lockout. Follow the
  `project_auth_methods_oidc_strict` precedent: warn now, panic next
  major. Do not silently disable the gate (AGENTS.md § fail-fast: a
  silent skip of a security control is the worst option).

## Test plan

- Unit: the decision half (`decide_elevated`) — never elevated, within
  TTL, expired by one second, `elevated_at` cleared. Pure, no live
  session store, same split as `decide_internal_user` /
  `decide_live_role`.
- Unit: method classification — `GET /api/admin/users` needs no
  elevation, `DELETE /api/admin/users/{id}` does. This is the pin that
  catches someone "simplifying" the layer onto the whole nest.
- Hurl (`tests/api/admin_elevation.hurl`, new):
  - admin GET on an admin route → 200 unelevated;
  - admin DELETE/POST → 403 `ElevationRequired`;
  - `POST /api/auth/elevate` with the WRONG password → 403, and the
    attempt counts against the login lockout;
  - with the right password → 200, then the same mutation → succeeds;
  - after `elevated_at` is cleared (password change) → 403 again;
  - a non-admin calling `/api/auth/elevate` → succeeds (elevation is not
    a privilege) but the admin mutation still 403s on the ROLE, with
    `error_type` naming the role failure, not elevation. Pins that the
    two checks stay independent.
- Each pin asserts the refusal AND the allowed path, per AGENTS.md — a
  gate that refuses everyone must not pass.

## Non-goals

- Not step-up MFA/TOTP. Elevation re-proves the *existing* credential;
  adding a second factor is a separate feature that would plug into the
  same `POST /api/auth/elevate` branch list.
- Not applied outside `/api/admin/*`. Per-resource destructive actions
  (deleting your own files) are grant questions, already answered by
  `authz.require`, and trash makes them recoverable.
- Does not change `require_admin` — elevation is layered beside the role
  check, never instead of it.
