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
├─[3] policy gate — ONE lookup, one evaluate                ── GATE ──
│     │   the operation's compiled Policy vs the caller already in
│     │   the extensions. Field reads: no DB round trip, and ONCE
│     │   rather than once per nesting level.
│     └── require_no_password_change_pending_layer   (a scope narrowing,
│         NOT an axis — stays a layer)
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

## Scope APPENDS — a leaf cannot change what its prefix requires

**Read this before adding a route.** It is the one rule that decides where
a route may be mounted, and getting it wrong is silent until someone's
credential is refused.

A declaration on a nest applies to everything beneath it. A declaration
on a route **adds** to that. It never replaces it:

```rust
// /api/drives declares files; the members route adds sharing
.nest("/drives", drives_router.policy([Condition::scope_by_verb(Scope::Files)]))
// …inside drives_router:
.route("/{id}/members", get(list_members)
    .policy([Condition::scope_by_verb(Scope::Sharing)]))
```

`GET /api/drives/{id}/members` then requires **`files:ro` AND
`sharing:ro`** — a credential must carry both. That is deliberate: listing
drive members reads drive metadata *and* reads grants, so requiring both
is honest, and the fold is monotonic — every declaration down the chain
only ever tightens.

Three things follow, and the third is the one that bites:

1. **There is no conflict to resolve.** Two areas in a chain used to be a
   boot error. They are now a conjunction, which is strictly tighter, so
   no precedence rule is needed and no declaration can be weakened from
   below.
2. **The same area twice tightens the ACCESS** by `max` — a `files:ro`
   prefix with a `files:rw` leaf yields `files:rw`.
3. **A MIXED subtree must declare per route, never at its prefix.** A leaf
   cannot shed what a prefix imposed, so a prefix declaration is a
   commitment binding every route beneath it, including ones not written
   yet.

### The worked case: `/api/groups`

The group CRUD is the `admin` control surface. `/api/groups/search` is
recipient lookup for the share picker, which an ordinary user does.
Declaring `admin` on the `/api/groups` nest made `search` require
`["admin:ro", "sharing:ro"]` — the spec showed it, and it would have meant
a share picker demanding admin scope.

So `/api/groups` declares **nothing** at the prefix: the CRUD routes
declare `admin` each, and `search` declares `sharing`. Verbose, and
correct. The prefix form is for subtrees that are homogeneous, like
`/api/calendars`.

### Consequences for the API tree

- **Where you mount a route decides what it requires.** The tree shape is
  part of the policy, not just addressing. Adding `/api/files/share` would
  silently require `files` on an operation that is about grants.
- **Adding a route under an existing prefix inherits silently.** That is
  the desired default — it is what stops a new route being `Undefined` —
  but it means checking that the prefix's area actually fits before
  mounting there, rather than after.
- **A route that fits no existing prefix wants its own**, or a per-route
  declaration. Reaching for a prefix that is nearly right is how an
  operation ends up requiring an area it has nothing to do with.

### Visitors carry exactly `files:ro`

A public-share session (the ring) holds a token for one shared item, with
no account behind it. `ScopeSet::ring()` is therefore `files:ro` and
nothing else — read-only because a `Share` has `max_downloads` and
`download_count` and no write permission at all, and `files`-only because
no other area is reachable without an account.

The compile step enforces it: a visitor-admitting operation that requires
more than `files:ro` is a GENERATION failure, not a 403 discovered when
someone opens a share link. So the seven visitor-reachable GETs may
require `files:ro` and nothing further.

## What each area covers

Declared at PREFIXES, in `interfaces/api/routes.rs`. This table is the
design — which prefix owns which area — and it is deliberately not a
route list:

| area | prefixes that declare it |
|---|---|
| `files` | `/api/files`, `/api/folders`, `/api/uploads`, `/api/batch`, `/api/dedup`, `/api/drives`, `/api/favorites`, `/api/recent`, `/api/trash`, `/api/photos`, `/api/playlists`, `/api/wopi` |
| `sharing` | `/api/shares`, `/api/grants`, `/api/users`, `/api/groups`, `/api/drives`, `/api/playlists` |
| `calendar` | `/api/calendars` |
| `contacts` | `/api/address-books` |
| `faces` | `/api/people` |
| `account` | `/api/notifications`, parts of `/api/auth` |
| `security` | parts of `/api/auth` |
| `admin` | `/api/admin`, parts of `/api/groups` |
| *scopeless* | `/api/config`, `/api/i18n`, `/api/status`, `/api/setup`, `/api/search`, `/api/s`, parts of `/api/auth` |

**`photos`, `music` and `trash` are not areas.** They are views over
files, so a credential holding `files` reaches them — and conversely a
photo-only scope could not exist without being a lie, since the same
bytes are reachable through `/api/files`.

**The authoritative answer is per OPERATION, and it is published.**
Do not read this table as the contract — ask the spec, which is a
projection of the same registry the gate enforces:

```sh
jq -r '.paths | to_entries[] | .key as $p | .value | to_entries[]
       | "\(.key|ascii_upcase)\t\($p)\t\(.value["x-oxicloud-policy"].scope
           // "UNDEFINED" | if type=="array" then join(",") else . end)"' \
  resources/gen/openapi.json | sort -t$'\t' -k2,2 -k1,1 | column -t -s $'\t'
```

Sorted by ROUTE then method, so the verbs of one path sit together and
the `:ro`/`:rw` split is readable down the third column. The `paths`
object holds only method keys, so no filtering is needed.

All 250 operations answer; none is `null`. A `null` would mean
`ScopeReq::Undefined` reached generation, which fails the build.

### The leaves with subtleties

Five operations across four paths require TWO areas, and they are the
append rule made visible — a `sharing` declaration at the route, under a
`files` prefix. Straight from the query above:

```text
GET     /api/drives/{id}/members                      files:ro,sharing:ro
POST    /api/drives/{id}/members                      files:rw,sharing:rw
POST    /api/playlists/{playlist_id}/share            files:rw,sharing:rw
DELETE  /api/playlists/{playlist_id}/share/{user_id}  files:rw,sharing:rw
GET     /api/playlists/{playlist_id}/shares           files:ro,sharing:ro
```

A credential holding only `files:rw` is refused on all five. That is
correct and it is the design: managing who else can reach a thing is a
sharing operation whatever the thing is.

The first two rows also show `scope_by_verb` doing its job — ONE
declaration per area on the route, resolving to `:ro` for the GET and
`:rw` for the POST. Nobody wrote `files:ro,sharing:ro` anywhere; the
method decided it at compile time.

**Three MIXED subtrees declare per route, never at the prefix**, because
a leaf cannot shed what a prefix imposes:

- **`/api/auth`** is the extreme case — it spans `security` (the
  app-password endpoints), `account` (profile) and *scopeless* (login,
  logout, refresh, the OIDC and magic-link flows, OPAQUE). A prefix
  declaration here would impose one of those on all of them; an early
  draft put `security` on the router and 19 operations failed to compile.
- **`/api/groups`** spans `admin` (every mutation) and `sharing`
  (`/search`, so the share dialog can offer groups as recipients).
  Declaring `admin` at the nest made the share picker require admin
  scope. Worked through in § The worked case above.
- **`/api/drives`** and **`/api/playlists`** are `files` prefixes whose
  `members`/`share` leaves append `sharing`, per the table above.

## Two sets are FORCED, not requested

Most credentials get what they ask for, bounded by their kind's ceiling.
Two do not, and both are constants in the code rather than rows in a
table — which is what makes them auditable.

| | set | where | why it cannot be asked for |
|---|---|---|---|
| **Visitor** (share ring) | `files:ro` | `ScopeSet::ring()` | a capability URL is not a credential — there is no row to carry a scope column, so the constant IS the scope |
| **App password** | `files:rw` + `calendar:rw` + `contacts:rw` + `sharing:ro` | `CredentialKind::AppPassword::ceiling()` | the dialog that mints it shows no consent screen, so it may not hold anything a user has not seen named |

The visitor set is enforced at COMPILE time as well as applied at
runtime: a visitor-admitting operation requiring more than `files:ro`
fails generation (§ Visitors carry exactly `files:ro`). The two must stay
equal, and `the_visitor_ring_is_exactly_files_read_only` pins the
constant so a change to one surfaces as a failing test rather than as a
share link that 403s.

**The app-password ceiling is an ALLOWLIST, and the three it withholds
matter individually:**

- **no `security`** — so an app password cannot mint or revoke another
  one. Without this the ceiling is one API call from irrelevant: a
  credential minted for DAVx⁵ could mint itself a wider one.
- **no `admin`** — and this is the one worth understanding. A role is
  read from the USER row, so an admin's app password carries the admin
  role and nothing about it looks less privileged. `MinRole::Admin` is
  satisfied. Only the missing area stops it, which is why
  `credential_scopes.hurl` step 14 asserts a 403 on
  `GET /api/admin/users` *as the admin* — role satisfied by
  construction, so the refusal can only be scope.
- **`sharing` at `:ro`, not withheld** — and this one was got WRONG
  first, which is worth recording. It was excluded on the reasoning that
  `sharing` enumerates the user roster, with the cost described as "the
  NC share dialog's autocomplete". `nc_admin_views_other_user.hurl`
  showed the cost was larger — an NC admin could no longer read another
  user's OCS profile — and that the line had been drawn in the wrong
  place.

  **The ceiling exists to prevent ESCALATION, not to withhold capability
  the human already has.** `security` and `admin` are escalation: minting
  another credential, administering the instance. Reading the roster is
  not — the user behind the credential can do it from the web UI, so
  withholding it took nothing from them and only broke NextCloud
  compatibility, which this project treats as a feature.

  Read-ONLY because all three OCS routes that declare `sharing` are
  GETs, so `:ro` satisfies every door that exists. It is the one area
  whose access differs from the rest, and least privilege is why.

**The ceiling is sized for DAV and NextCloud, which is what an app
password is FOR.** It authenticates sync clients — NC desktop, DAVx⁵,
Thunderbird, rclone — over `/remote.php`, `/ocs`, `/webdav`, `/caldav`
and `/carddav`. Reaching `/api` with one is possible but incidental, and
an app password is itself an anomaly: it exists because those clients
cannot do OIDC. So "a credential that reaches files, calendars, contacts
and the roster" is not a narrowing of its job — it IS its job, and the
109 of 250 REST operations it cannot reach are surfaces it was never
meant for.

What it therefore withholds, each for its own reason:

| area | why withheld |
|---|---|
| `security` | escalation — it is how a credential would mint or revoke another |
| `admin` | escalation — and a role check cannot stop it, since an admin's app password carries the admin role |
| `sharing:rw` | share CREATION; no OCS endpoint needs it, and `:ro` already covers the roster reads that do |
| `account` | the caller's own profile and notifications. NC clients do not need it: the OCS self-probe `cloud/user` is scopeless |
| `faces` | People enumerates recognised individuals, which is a sharper privacy question than the files the faces were found in — and no sync client browses it |

A future major may refuse app passwords on `/api` outright rather than
scope them there; the areas above are the same list either way.

A withheld area is REMOVED from the request, not a reason to reject it
(RFC 6749 §3.3) — `files:rw + admin:rw` is minted as `files:rw`. The
narrowing is then REPORTED, because §5.1 requires it whenever the issued
scope differs from the requested one.

**The device grant's ceiling is the same three plus `account`**, and the
difference is consent: its verify screen shows the user what is being
granted before they approve. That screen reads the STORED set, and the
flow stores at `authorize` — before approval — so it displays the capped
set and consent is truthful by construction rather than by the UI
remembering to cap it.

**An interactive session is the only credential that holds `admin` or
`security`**, and it holds them by the absent-claim default rather than
an explicit set: a session's ceiling is every area, so its token carries
no `scope` claim at all and only a genuinely narrowed credential pays for
the string.

## The message bus: scope per TOPIC

The bus asks the same three questions as a REST request, in the same
order, and the answers live on `Topic` itself (`application/ports/
message_bus_ports.rs`) — `required_perm` for the gate and grant,
`required_scope` for this axis.

**`:ro` is subscribe. `:rw` is subscribe + publish.** That one rule
decides every row, and it decides most of them trivially, because only
`collab:` is client-publishable — every other topic is server-published,
so a client can do nothing but subscribe:

| topic | gate / grant | scope |
|---|---|---|
| `folder:{id}` | Read on the folder | `files:ro` |
| `collab:{id}` | Read on the file | `files:ro` |
| `user:{id}:authz` | identity match | *scopeless* |
| `user:{id}:notifications` | identity match | *scopeless* |
| `job:{name}` | admin role | `admin:ro` |
| `server:status` | any session | *scopeless* |

Keyed on the topic CLASS, never the instance. `job:{name}` and
`folder:{id}` are parameterised, and "may this caller reach THIS folder"
is the grant axis's question, not this one.

### The collab frames are where the rule bites

Binary frames on `collab:{id}` are `[1 byte kind][16 byte file_id][payload]`,
and the kind decides the access:

| frame | meaning | scope |
|---|---|---|
| `0x01` | Yjs update | **`files:rw`** |
| `0x02` | awareness (presence) | `files:ro` |
| `0x03` | sync-step 1/2 | `files:ro` |
| unknown | — | **`files:rw`**, the safe end |

So a `files:ro` credential **opens a document in the editor, sees every
change arrive, and publishes presence — and is refused only when it tries
to type.** The subscribe succeeds; only the update frame is dropped, and
the subscribe ack already told the client `can_write: false`, so the
editor renders read-only rather than appearing to accept edits that
vanish.

The grant axis already split exactly here — `Read` to subscribe, an
`Update` check on `0x01` at frame-apply time — so the scope axis mirrors
a split that existed rather than inventing one.

### Why two topics are scopeless

`user:{id}:authz` and `user:{id}:notifications` are **auto-subscribed by
the server at session open**. An area the credential lacked would
therefore break the whole session instead of narrowing it — an app
password would get a failed connection rather than a working one minus
notifications.

They are also only pokes. The payloads are thin facts ("your permissions
changed", "notification N of kind K arrived") and the client must refetch
`GET /api/notifications` for anything real — which **is** scope-gated at
`account`. **The gate belongs where the data is, not on the signal that
data changed.** Same reasoning that makes avatars scopeless.

Privacy does not depend on the scope axis here: `AuthzCheck::IdentityMatch`
is `caller_id == user_id` with **no admin bypass**, so a caller reaches
only its own `user:{self}` topics whatever its scopes say. Two axes, two
jobs.

### Which refusals collapse, and which do not

The wire has **two** codes, and the split is deliberate — an earlier
draft of this page claimed one, which was wrong:

| code | returned for |
|---|---|
| `no_read` | no grant on the resource, **and** no such resource |
| `topic_forbidden` | unknown topic, identity mismatch, role denial, **scope denial** |

`no_read` is the anti-enumeration collapse: "you may not read it" and
"it does not exist" must be one answer, or a caller can probe for
resources by subscribing.

A scope refusal joins `topic_forbidden` instead, and that is safe to
distinguish because **it is a fact about the CALLER, not the resource**.
The scope check runs before the grant dispatch, so a scope-denied
subscriber learns nothing at all about the topic it named — the answer
would be identical for a folder that exists, one that does not, and one
it could otherwise read. Grouping it with the role denial is right for
the same reason: both say "this credential, not this thing".

The audit line carries the truth either way —
`message_bus.subscribe_denied` with `required_area`, `required_access`
and `granted`, and `message_bus.frame_denied` for a refused frame.

The scope check runs BEFORE the grant dispatch, because it is two mask
comparisons where the grant may hit the authorization engine. A
credential that cannot reach the area at all should not cost a resource
lookup to refuse.

### The scope set reaches the socket by two roads

A WS upgrade has no middleware chain, so the set is carried rather than
resolved:

- **Bearer upgrade** — the handler validates the JWT itself and reads the
  `scope` claim off it.
- **Ticket upgrade** (what browsers use) — `POST /api/rt/ticket` runs
  under the full chain and therefore KNOWS the caller, so the ticket
  records the scope set alongside the presented token, and the upgrade
  redeems both.

Without the ticket carrying it, that path would be a scope hole of the
same shape as the DPoP one it already closes: a narrow credential could
ask for a ticket over REST, where its scopes are checked, and redeem it
into a socket where nothing knew what they were.

Frozen at upgrade, like the token: a scope narrowed afterwards takes
effect when the credential's token expires — the same trade § Frozen at
mint makes for REST.

## One place to declare a route and its policy

**`src/interfaces/api/routes.rs`.** Route, layer and policy are written
together, per verb:

```rust
// the subtree
router.nest("/admin", admin_router.policy([Condition::role(MinRole::Admin)]))

// one verb on one path — the other verbs on it are untouched
.route("/{id}", get(download).policy([Condition::visitor_allowed()]))
.route("/{id}", delete(remove))          // no policy: the safe default
```

A reader of that file sees all three without cross-referencing, and the
path appears exactly once — `.policy()` attaches conditions, and the
`nest`/`route` call supplies the path.

**Per VERB, not per path.** `get(h)` and `delete(h)` here are OxiCloud's
constructors (`interfaces/api/policy_router.rs`), not axum's, because
axum's return a `MethodRouter` that has forgotten which verb it is — so
`.policy()` on one could only declare for the whole path.

That is structural, not tidiness. One path routinely serves a
visitor-readable GET and a write that no visitor may reach:
`/api/files/{id}` is a download and a delete. A path-level
`visitor_allowed()` there hands a share visitor the DELETE. `.policy()`
on a `MethodRouter` is a compile error for that reason.

A declaration leaks in none of the three directions, which
`policy_router`'s non-leak test pins end to end:

| operation | policy | why |
|---|---|---|
| `GET /api/files/{id}` | `Visitor` | declared |
| `DELETE /api/files/{id}` | `User` | same path, other verb |
| `GET /api/files` | `User` | the collection — upward |
| `GET /api/files/{id}/versions` | `User` | a subpath — downward |

A visitor holds a key to one share, not a listing of everything and not
the right to delete it.

**What may NOT be declared there**, and so lives in
`interfaces/api/policy_declarations.rs`: operations with no route to
attach to. A DAV surface has one `any` handler, so its operation is an
`(area, access)` pair; the WS bus has one route, so its operation is the
`rt.*` method; collab has one, so its operation is the frame kind. Nothing
else belongs in that file — and never a copy of a list that exists
elsewhere, since a cross-check test over two copies defends the
duplication instead of removing it.

## How the axes compose — worked, in the planned form

> **Partly implemented.** The `.policy([...])` syntax below exists
> (`interfaces/api/policy_router.rs`), and so does the engine it feeds
> (`application/services/scope_policy.rs` and `scope_table.rs`) — but
> nothing is wired: `routes.rs` does not declare, and the five middleware
> layers still enforce. The *composition* shown is real today, spread
> across those layers and `authz.require`; what changes is that each
> operation states its requirements in one place.
>
> Two corrections to what the examples imply:
>
> - **`.policy()` cannot record the path**, because neither a `Router`
>   nor a `MethodRouter` knows where it will be mounted — axum's whole
>   public introspection surface is `has_routes() -> bool`. So
>   `.policy()` attaches conditions and the path is learned at the
>   `nest` / `route` call, through `GatedRouter`. The path is still
>   written exactly once; the examples below are accurate about that.
> - **`Condition::elevated()` does not exist yet.** It needs
>   `auth.sessions.elevated_at` (`session-elevation.md`), so the two
>   examples using it are the planned form, not today's.

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
  table is keyed by **operation** (method + path), not by route.
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
