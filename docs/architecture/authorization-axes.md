# Authorization axes — gate, scope, grant

Three different questions decide whether a request proceeds. They have
different sources, different mutability, and are enforced in different
layers. Conflating them is the bug this page exists to prevent.

| axis | question | source | client may request? | enforced in |
|---|---|---|---|---|
| **Gate** | who is this **caller**? | `auth.users` + the credential, read live | **never** | router layer |
| **Scope** | what was this **credential** issued for? | frozen on the credential at mint | yes | router / spec |
| **Grant** | may they touch **this object**? | `storage.role_grants` | n/a | service layer |

Effective permission is the **intersection**. A scope never grants
beyond what the user already has — it only narrows. An admin's app
password scoped `files:ro` administers nothing: not because the scope
says "not admin", but because `/api/admin` asks a Gate question and no
scope string can answer it.

Status: **Gate and Grant are enforced today. Scope is not** — the device
flow collects scopes and displays them, but nothing checks them. See
`docs/plan/scope-gates.md`.

## Glossary

Used strictly, because several of these words were doing double duty
before the design settled.

| word | means | does NOT mean |
|---|---|---|
| **axis** | one of the three questions above — Gate / Scope / Grant | a field of a policy — those are *conditions* |
| **condition** | one dimension a request-time policy checks: minimum role, minimum principal, scope, elevation | an axis; a policy spans two axes, so its fields cannot be axes |
| **grant** | may this caller touch THIS resource — service layer, `authz.require` | a policy or a condition |
| **integrity** | signature, DPoP proof, CSRF — is the REQUEST authentic | an axis or a condition (below) |
| **authn phase** | login, refresh — where admission is decided | "session phase": an app password creates no session yet still runs phases on every request |
| **admission** | may this account hold a credential at all, and what for | a per-request check |

Retired as TYPE names: **"gate"** (it named the axis, any caller-kind
check, and a router layer) and **"facet"** (superseded by *condition*).
"Gate axis" and "Gate-axis check" remain correct — Gate IS one of the
three axes.

The planned request-time engine in `docs/plan/scope-gates.md` adds
`Condition` (an operation declares a slice of them), `Policy` (the
compiled result), and two traits split by moment — `RequestPolicy` and
`AdmissionPolicy`. Those are not implemented; the Gate axis is still five
hand-wired layers.

## The Scope axis

A scope is **what a credential was issued for** — the only axis a client
may request, and the only one frozen at mint.

> **Status: designed, collected, enforced nowhere.**
> `auth.app_passwords.scopes` and `auth.device_codes.scopes` both exist,
> both default to `'webdav,caldav,carddav'`, and both are displayed in
> the UI. **No authorization decision reads either.** A credential
> showing "scopes: webdav,caldav,carddav" today is unrestricted.
> `docs/plan/scope-gates.md` is the enforcement plan.

### Bound to the (session, user) pair

Effective permission is `what the credential was issued for ∩ what the
user is currently allowed`. Both halves matter: a scope can only
**narrow**, never grant. An admin's app password scoped `files:ro`
administers nothing — not because the scope says "not admin", but
because `/api/admin` asks a Gate question no scope string can answer.

### Frozen at mint — and why that is safe

Scope is the one axis legitimately baked into a credential, because it
**cannot change under that credential**: narrowing an app password's
scopes means issuing a different one. So there is nothing to invalidate
and no staleness window — unlike role or `is_external`, which are re-read
live per request (§ What a classic API request actually checks).

### Vocabulary: areas, not protocols

Designed as eight resource **areas**, each with `ro` or `rw`, written
`oxicloud:<area>:<ro|rw>` on the wire:

`files` · `calendar` · `contacts` · `photos` · `music` · `sharing` ·
`account` (profile, preferences) · `security` (password, identities,
credentials)

Resource-shaped on purpose. Today's stored values are
*protocol*-shaped — `webdav,caldav,carddav` — which cannot express
"read-only" and does not survive a client reaching the same files over
REST instead of DAV. `files` covers the data wherever it is reached from.

### The ceiling is where credential kind is enforced

Each credential kind has a maximum, as an **allowlist**:
`effective = (requested ?? type_default) ∩ type_maximum`.

| credential | maximum |
|---|---|
| app password | `files,calendar,contacts` — **no `security`, no `account`** |
| device flow | + `account` — **no `security`** |
| interactive session | every area, `security` included |

This is why credential kind is not a request-time condition: an app
password reaching `/api/auth/change-password` is refused because its
scope set lacks `security`, not because anything noticed it was an app
password. Being an allowlist, a credential kind added without thought
about `security` gets it **withheld**.

### Where a scope is visible

OpenAPI's own `security` array, which the spec already carries: 228
operations emit `security: [{"bearerAuth": []}]` today — the structure
with an empty scope list — and 21 emit `security: []` for public routes.
Filling that array is how a scope becomes visible in `openapi.json`, in
Swagger UI, and in the generated TypeScript client, with no new
mechanism.

The planned direction is **engine → spec**: the array is rendered from
the enforced policy rather than read as its source, so the documentation
cannot disagree with enforcement. `generate-openapi` plus CI's spec-drift
check make a stale array a build failure. See
`docs/plan/scope-gates.md` § The spec is rendered FROM the policy table.

### Not to be confused with OIDC client scopes

`config.oidc.scopes` is unrelated: those are the scopes **OxiCloud
requests from an identity provider** at login (`openid`, `profile`,
`email`). They are real and in use. Credential scopes are what
OxiCloud's **own** credentials are limited to, and are the inert ones.
Same word, opposite direction.

## A fourth thing, which is not an axis: integrity

Three checks on the request path answer **"is this request authentic?"**
— a precondition to authorization rather than a part of it:

| check | asks | failure |
|---|---|---|
| JWT signature + expiry | did we issue this token, is it still valid? | `401` |
| DPoP proof (RFC 9449) | does the holder of this bound session possess the key? | `401` + `WWW-Authenticate: DPoP error="invalid_dpop_proof"` |
| CSRF double-submit | did this state-changing request come from our own page? | `403` |

They are not axes because the **operation has no opinion**: DPoP is
applied uniformly to all 8 protected nests and CSRF to 7, and what varies
is `OXICLOUD_DPOP_MODE` — configuration, not routing. A failure means
*this request* cannot be trusted, not that *this caller* is the wrong
kind.

The distinction matters when adding a check: if every operation would
declare the same value, it is integrity and belongs once at the root. If
operations genuinely differ, it is an axis.

## Gate vs grant — the distinction that matters

The two are easy to blur because both can answer "no" to the same
request. The difference is whether a **resource** is named.

|  | gate | grant |
|---|---|---|
| asks | may this KIND of caller reach this SURFACE? | may this caller act on THIS resource? |
| names a resource | no | yes — always |
| example | `/api/admin/*` requires admin; `/webdav` refuses externals | may Bob read file X |
| answer depends on | the caller alone | the (caller, resource) pair |
| scales with | nothing — one check per request | the grant graph, per resource |
| lives in | a router layer | the service layer, `authz.require` |
| if you forget it | a whole surface is open to the wrong kind of caller | one resource leaks |

Two consequences follow, and both are rules rather than preferences:

- **A grant check belongs in the service layer, never a handler**
  (`src/AGENTS.md` § Authorization). Handlers authenticate and pass
  `caller_id` in; the service decides. This keeps every door to a method
  — REST, WebDAV, NextCloud, CalDAV — on one policy.
- **A Gate-axis check belongs in the router**, because there is no resource to
  name and therefore nothing for `authz.require` to be called *about*.
  `require_internal_user_layer` and the admin middleware are gates; they
  are not "authz in the wrong place".

The failure mode when they are confused: a **top-level create** has no
parent resource, so there is nothing to `authz.require` on — and the
grant rule alone yields no check at all. `create_calendar` /
`create_address_book` / `create_playlist` each stamp the caller as owner
and seed themselves a grant, so handler, service and router each read as
though another held the check. That is how external users could own
playlists over REST while the CardDAV door to the same method was gated.
A top-level create is a **gate** question wearing a grant's clothes.

## What a classic API request actually checks

`GET /api/files/{id}` with a Bearer token. Every `⚡` is a cache.

```
┌─ GET /api/files/{id}   Authorization: Bearer <jwt>
│
├─[1] auth_middleware                        ← applied 9x, once per nest
│     │                                        (no root choke point)
│     ├── share-ring cookie? ── no ──→ continue      (anonymous path)
│     │
│     ├── validate_token(jwt)          ⚡ validation_cache  30 s / 50k
│     │      └─ signature + exp                 miss = CPU only, no DB
│     │
│     ├── resolve_live_role(user_id, claim_role, token)      ── GATE ──
│     │      ├── get_user_flags(user_id)  ⚡ user_flags_cache  30 s
│     │      │      └─ MISS → 1 single-row PK lookup (5 cols, no image)
│     │      │                try_get_with coalesces concurrent misses
│     │      └── decide_live_role(flags)
│     │             ├─ not active                 → 401
│     │             ├─ iat < sessions_valid_from  → 401  (revoke-all)
│     │             ├─ sid ∈ revoked_session_ids  → 401  (per-session)
│     │             └─ DB error                   → 503  (never a denial)
│     │
│     ├── CurrentCaller { id, username, email, role ←LIVE, dpop_jkt, token }
│     │        └─→ Arc into request extensions
│     │
│     └── last_seen.touch(sid)         ⚡ DashMap upsert, no I/O
│                └─ batched UNNEST UPDATE every 30 s
│
├─[2] require_dpop_layer                            ── INTEGRITY ──
│        bound session ⇒ valid proof. Uniform across all 8 nests;
│        varies by OXICLOUD_DPOP_MODE, never by route. Failure is
│        401 invalid_dpop_proof — the REQUEST is untrustworthy, not
│        the caller. Same category as CSRF and signature checks.
│
├─[3] route layers — the hand-wired gates                   ── GATE ──
│     ├── require_internal_user_layer  → get_user_flags again (same ⚡)
│     ├── require_no_password_change_pending_layer   (a scope narrowing)
│     └── anonymous_allowlist_layer                  (needs MatchedPath)
│
├─[4] handler — extracts AuthUser / CurrentUserId from extensions
│                NO authz here, by rule
│     ↓
├─[5] service — authz.require(User(caller), Read, File(id))  ── GRANT ──
│     │   ⚡ owner_cache           300 s   (owner is immutable)
│     │   ⚡ drive_role_cache       30 s   (invalidated on membership change)
│     │   ⚡ drive_policies_cache   30 s
│     │   ⚡ cascade_grant_cache    30 s
│     │      └─ MISS → PG recursive grant resolution
│     │   denial → audit `authz.denied`, then 403 (has Read) / 404 (anti-enum)
│     ↓
└─[6] repository — the data query
```

Three things worth reading off the diagram:

- **The gate axis costs one cached read per request.** Role is resolved
  live in step 1 and lands on `CurrentCaller.role`, so it is the *claim*
  that is advisory and the live value that is authoritative. Step 2's
  second `get_user_flags` is a redundant hit on the same cache, because
  `is_external` is not carried on the caller.
- **Five caches, four of them 30 s.** That is the house convention, and
  what makes it safe is `invalidate_*` on change rather than the TTL: a
  tightening change closes the window immediately instead of waiting for
  expiry.
- **A DB failure in a Gate-axis check answers 503, never a denial.** A transient
  fault must not read as "you are not an admin" — see `auth-model.md`.

## How the axes compose — worked, in the planned form

> **Not implemented.** The syntax below is `docs/plan/scope-gates.md`'s
> request-time engine. The *composition* it shows is real today, spread
> across five middleware layers and `authz.require`; what changes is that
> each operation states its requirements in one place.

Three nests, because each shows a different composition.

**`/api/admin` — one condition, inherited.**

```rust
let admin = admin_handler::admin_routes(state)
    .policy([Condition::role(MinRole::Admin)]);   // the whole subtree

    //  inherited:  GET /settings/oidc, PUT /settings/storage,
    //              GET /sessions  →  [role(Admin)]

    // One child tightens. It says nothing about role, so Admin is kept.
    .route("/sessions/{id}", delete(revoke_session)
        .policy([Condition::elevated()]));
    //  → [role(Admin), elevated]
```

**`/api/auth` — the three principals in one nest.** Today this is *two*
routers, mounted differently, because publicness is expressed by which
one a handler was added to. A declared exception collapses them.

```rust
let auth = auth_public_routes().merge(auth_protected_routes());   // ONE nest

    // PUBLIC — no credential at all.
    .route("/login",            post(login).policy([Condition::public()]))
    .route("/refresh",        post(refresh).policy([Condition::public()]))
    .route("/oidc/callback",    get(oidc_callback).policy([Condition::public()]))

    // DEFAULT — authenticated user, nothing declared.
    .route("/me",      get(get_current_user))
    .route("/logout", post(logout))

    // CREDENTIAL OPERATIONS — the `security` area, and a human present.
    .route("/change-password", put(change_password)
        .policy([Condition::scope(Scope::Security, Access::ReadWrite),
                 Condition::elevated()]))
    .route("/sessions/{id}", delete(revoke_own_session)
        .policy([Condition::scope(Scope::Security, Access::ReadWrite)]));
```

`/change-password` needs both and they are independent: the scope says the
credential was *issued for* credential operations, elevation says a human
proved themselves *recently*. An app password fails the first; a week-old
browser session fails the second.

**`/api/files` — a scope floor, tightened per method, and the visitor.**

```rust
// `basic_file_router` in `routes.rs`, nested at /files.
let files = basic_file_router
    .policy([Condition::scope(Scope::Files, Access::ReadOnly)]);  // floor

    // Reads inherit files:ro…
    .route("/{id}/download",         get(download))

    // …except the three a share link may reach. `visitor_allowed()`
    // also admits a User — the owner opens the same route.
    .route("/{id}",                  get(get_file)
        .policy([Condition::visitor_allowed()]))
    .route("/{id}/metadata",         get(get_metadata)
        .policy([Condition::visitor_allowed()]))
    .route("/{id}/thumbnail/{size}", get(get_thumbnail)
        .policy([Condition::visitor_allowed()]))
    //  → [scope(Files, ro), principal(Visitor)]

    // Writes tighten the SAME axis through the chain: ro → rw.
    .route("/{id}",                  put(update_file)
        .policy([Condition::scope(Scope::Files, Access::ReadWrite)]))
    .route("/",                     post(upload_file)
        .policy([Condition::scope(Scope::Files, Access::ReadWrite)]));
    //  → [scope(Files, rw)]
```

Four things these make visible:

- **The same path can carry different policies per method.** `GET
  /files/{id}` admits a visitor; `PUT` on the identical path does not,
  and needs `rw` rather than `ro`. That is only expressible because the
  table is keyed by **operation** (method + path), not by route — which
  is also how `ANONYMOUS_ALLOWLIST` behaves today, with a `GET`-only
  check in the layer.
- **A floor plus per-method tightening needs the chain rule.** The nest
  says `scope(Files, ro)` and a child says `scope(Files, rw)` — the same
  axis twice, legal because it is a CHAIN and the fold takes the
  stricter. The same pair inside ONE declaration is a boot error, since
  there it means someone wrote two things where they meant one.
- **`visitor_allowed()` admits a user as well.** The owner and the share
  visitor reach the same handler, which a two-valued
  allowed/denied condition could not express.
- **Grant still runs underneath.** `visitor_allowed()` says only that a
  share session may *reach* the operation. Whether this ring may read
  *this* file is `authz.require(…, Read, File(id))` in the service — the
  gate/grant split, in one route.

## Where each axis is enforced, and why

- **Gate → router layer.** No resource to name, and a subtree's denial is
  the surface rather than one method. A layer also cannot be forgotten
  per-handler the way an extractor can.
- **Grant → service layer.** Every protocol reaches the same service
  method, so the check must sit where they converge. A handler-level
  check drifts the moment a second door is added.
- **Scope → router plus the OpenAPI spec**, once enforced: the spec is
  where an operation declares what it needs, so the declaration is
  reviewable without reading the handler.

Grant's detail is deliberately not here. `rebac-authorization.md` is the
deep dive; its § What ReBAC does *not* cover says global roles "live
outside ReBAC because they're cross-cutting concerns, not per-resource
permissions" — that sentence describes the Gate axis, and this page is
where it lands.

## Deny by default, ranked

When a check can be forgotten, prefer the form where forgetting fails:

1. **Won't compile** — a capability type the callee cannot fabricate
   (`AdmittedCaller`: a `Session` cannot be built without proof of
   admission).
2. **Won't start** — a startup assertion over a route table.
3. **Won't pass CI** — a test that enumerates routes and fails on an
   undeclared one.
4. **Fails closed at runtime** — the default answer is refusal.
   `CurrentCaller::role_enum()` resolving an unknown value to `Anonymous`
   is this: the least privileged answer, not the most convenient one.
5. **Merely visible** — a comment or a doc. Weakest; use only when the
   four above cannot apply.

A refusal being the default answer is the design, not a bug to work
around.

## See also

- `rebac-authorization.md` — the Grant axis in depth
- `auth-model.md` — how a credential is obtained, and the admission and
  revocation phases
- `docs/plan/scope-gates.md` — the Gate fold and Scope enforcement, both
  planned
- `src/AGENTS.md` § Authorization — the rules a contributor applies
