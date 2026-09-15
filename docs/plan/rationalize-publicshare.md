# Rationalize public shares — anonymous sessions on the normal API

**Status: IMPLEMENTED.** All five phases shipped; #721 is closed. Triggered by
that issue (public share grids load full-resolution originals) and by the TODO
in `share_handler.rs:571` — *"remove this and use the classic /api/files &
/api/folders get, but with the token as session ?"* — which is precisely what
this did, so the TODO went with the handler.

Net: **867 lines deleted**, the share page 750 → ~390, and the six-endpoint
mirror of `/api/folders/*` + `/api/files/*` gone along with the service behind
it that executed as the share's OWNER.

What the build actually changed from the design below, worth reading before
trusting any section as written:

- **The ring replaced the anonymous access token entirely.** There is no second
  credential and no `auth.sessions` row — see §Session model. `share_id` claims,
  `generate_share_token` and `validate_principal_shape` were all deleted as
  redundant once the ring existed.
- **`ResourceList` needed one read-only prop after all**, not none:
  `allowThumbnailGenerate`. Every *other* mutating affordance there is opt-in
  through its own callback, but the thumbnail `onerror` fallback is not — and it
  fetches the full original, which is #721 again inside the component that fixed
  it. The plan predicted this; the first implementation pass still missed it.
- **Pseudo-scopes are a vendor extension, not a bearer scope.** OpenAPI defines
  scope arrays only for `oauth2`/`openIdConnect` and requires them empty
  elsewhere, so `security(("bearerAuth" = ["share:read"]))` made the document
  non-conformant and Swagger UI dropped it. `x-oxicloud-scope` carries it, and
  both markers are GENERATED from their gates rather than annotated per handler.
- **Two revocation bugs surfaced**, latent until the ring made token grants
  readable at request time: the `trg_cleanup_role_grants_token` trigger deletes
  the grant row in SQL, bypassing `clear_role` and its cache flush, so a revoked
  link kept working for the full 30 s TTL; `set_expiry_for_subject` never flushed
  either.

Three audits (API risk, read-path compatibility, WS/session lifecycle) were run
against a draft of this design. They moved the enforcement point and found four
pre-existing bugs. Both are recorded below.

---

## Context

Public shares are a **parallel implementation of the application**: six bespoke
`/api/s/*` endpoints, a 195-line `share_browse_service`, and a 750-line share page
of which ~700 lines duplicate existing components.

Three costs:

1. **Features stop at the boundary.** Issue #721 is the symptom — no thumbnail
   endpoint is reachable without a session. Every future feature needs another
   bespoke endpoint. There is also **no pagination**: `/contents` is unbounded.
2. **Two authorization paths.** `assert_file_in_share`
   (`share_browse_service.rs:119`) is a hand-rolled containment check returning a
   flat `not_found` — no `authz.require`, no graduated denial, no audit line.
3. **It is a confused deputy.** `share_browse_service.rs:66` reads
   `share.created_by` and lists **as the owner** (`:175-185`). Containment is the
   only thing between a visitor and the owner's entire drive.

The correct mechanism **already exists and is already populated**:
`set_role(user_id, Subject::Token(share.id()), Role::Viewer, resource, expires_at)`
(`share_service.rs:344-354`), backfilled for historic shares by
`20260520000000_rebac_access_grants.sql`. **Those rows are never read at request
time.** `architecture/share-integration.md:5` already documents them as
*"evaluated by the same AuthorizationEngine"* — true of storage, false of
evaluation. This work makes the documentation true.

### Verified: shares are LIVE, not frozen

Both mechanisms resolve membership per request — `is_file_in_subtree`
(`c.lpath <@ r.lpath`) and `folder_cascade_grant_exists` (`gf.lpath @> …`,
`pg_acl_engine.rs:820-853`). Nothing is snapshotted. A file added to a shared
folder is immediately visible, under either.

---

## What the audit changed

**The extractor is not a choke point.** The draft put the guard in `AuthUser` /
`CurrentUserId`. Four authenticated paths never touch them:

| Bypass | Where |
|---|---|
| `require_authenticated` / `require_admin` re-parse the JWT from header **or cookie** themselves | `middleware/admin.rs:86-122`, `:27-79` |
| WebDAV / CalDAV / CardDAV build `AuthUser` by hand from extensions | `webdav_handler.rs:154`, `caldav_handler.rs:421`, `carddav_handler.rs:173` |
| `POST /api/auth/refresh` is mounted **outside** `auth_middleware` | `main.rs:775-780` |
| `GET /api/rt/ws` self-authenticates from a raw `Authorization: Bearer` | `rt_ws.rs:240-255` |

Also: **`OptionalUserId` has zero call sites** — dead code, so "it yields `None`
for anonymous" was a no-op protection. And **`CurrentUser.role` is a `SmolStr`,
not the `UserRole` enum**: adding a variant produces *zero* compile errors in
`src/interfaces/`. The single-`role` safety property still holds (a string cannot
be both `"anonymous"` and `"admin"`), but nothing guides the change — hence
`require_user_role` as the single grep point.

**Rejected: handlers taking `Option<CurrentUser>` plus an assertion.** That
inverts the default from deny to allow — a handler that forgets the call silently
accepts anonymous, across 202 sites. Keeping the rejection *inside* `AuthUser`
leaves those 202 sites untouched and fail-closed. The assertion is still needed,
but only in the six places that build or validate a principal by hand: the three
DAV `extract_user` functions, `require_authenticated`, `authenticate_upgrade`, and
the refresh path. (`Option<CurrentUser>` already exists as `OptionalUserId` — and
nobody ever used it.)

**Therefore the decision moves to `auth_middleware`, where `CurrentUser` is
inserted, as an allowlist.** One choke point covers all four bypasses. Extractor
checks become defence in depth, not the primary control.

---

## Design

### One stored claim: `role`. Scope is derived.

```
role ∈ { anonymous, user, admin }
```

`UserRole` (`user.rs:9-12`) gains `Anonymous`. It stays on `role` — and `role`
stays single-valued — because that is what makes `anonymous` and `admin` mutually
exclusive by construction, with no admin-branch audit to forget.

Scope keeps the OIDC vocabulary at the API and documentation layer, but is
**derived, not stored**, because today the three roles map 1:1 onto the three
capability sets:

```
anonymous → share:read        user → full        admin → full + admin
```

A second signed claim that is always derivable is another thing to validate and
keep in sync, for no information gained. `Scope::for_role(role)` gives the value;
`require_scope(&cu, Scope::ShareRead)` can still be the helper signature, so call
sites are written against the right abstraction and only the *source* changes
later.

It stops being derivable the moment two credentials share a role but differ in
capability — **app passwords** (`role=user`, but their stored `scopes` are
enforced nowhere today: `verify_basic_auth` returns only
`(uuid, username, email, role)`), or a future "upload to share". Introduce the
stored claim then, not now.

### Naming follows `require_*`

15+ `require_*` helpers exist (`require_admin`, `require_internal_user`,
`require_file_perm`, …) and no `ensure_*`. The assertion inspects the **`role`**
claim, so it is **`require_user_role(&cu) -> Result<(), AuthError>`** — named for
the axis it reads, sitting beside `require_admin` as the same kind of check on the
same claim. Returning a `Result` rather than a bool matters: with `?` it cannot be
ignored, which an `is_anonymous()` bool invites.

### The OpenAPI declares the boundary — and is tested against it

utoipa already carries the mechanism, unused: every path declares
`security(("bearerAuth" = []))`, and **that empty array is the scopes list**
(OpenAPI `SecurityRequirement`). `SecurityAddon` (`mod.rs:551`) already registers
the scheme.

OpenAPI has **no field for a minimum role** — OAuth2 has no role concept, so the
spec has nowhere to put one. Pseudo-scopes give both gates one vocabulary:

```rust
security(("bearerAuth" = ["role:admin"])),     // the /api/admin nest
security(("bearerAuth" = ["share:read"])),     // the seven opted-in reads
// empty array stays the implicit default: role:user
```

Declaring only the two *exceptional* cases keeps the diff small — the risk lives
in the exceptions, not the default.

**utoipa's `security` is documentation, not enforcement**, so it can drift from
the real gate. The route-coverage test cross-checks both directions, for both
gates:

- every route declaring `share:read` ⇔ present in the allowlist constant
- every route declaring `role:admin` ⇔ mounted under the `require_admin` nest

That makes `resources/gen/openapi.json` a *tested* description of the
authorization boundary. Today it says "a session is required" for everything from
`GET /api/version` to `PUT /api/admin/users/{id}/role` — true, and useless.

### Enforcement: allowlist as a `route_layer` *(shipped — see note)*

Anonymous sessions reach **only** routes on an explicit allowlist constant
(`middleware/anonymous_allowlist.rs`). Everything else is denied before the
handler runs.

> **Implementation note.** The draft placed this in `auth_middleware`. It
> ships as a `route_layer` instead, because matching must be on axum's
> `MatchedPath` — the route *pattern* (`/api/files/{id}`), never the concrete
> URI — and `MatchedPath` is only populated **after** routing, which
> `.layer()` precedes. An absent `MatchedPath` is a **denial**, so misplacing
> the layer breaks anonymous access loudly rather than leaking quietly.
>
> I also argued once for skipping this layer entirely, on the grounds that
> every protected handler already takes a guarded extractor (verified: they
> do). That was wrong twice — it contradicted the three-independent-layers
> principle used everywhere else here, and it mistook a snapshot for an
> invariant. Positive enumeration is the stronger property precisely because
> it does not depend on the 201st handler being written correctly.

The allowlist, and nothing more:

```
GET /api/folders/{id}                     GET /api/files/{id}
GET /api/folders/{id}/resources           GET /api/files/{id}/thumbnail/{size}
GET /api/folders/{id}/ancestors           GET /api/files/{id}/metadata
GET /api/folders/{id}/download
```

A method-based guard is **not** sufficient on its own and is kept only as a second
layer: `GET /api/wopi/editor-url` mints a WOPI token usable for
`POST /wopi/files/{id}/contents`, and `GET /api/s/{token}` already performs a DB
write (`register_shared_link_access`, `share_handler.rs:243`). GET-that-writes is
normal in this codebase.

### Session model — stateless. **REVISED during implementation.**

> The original design here called for an `auth.sessions` row with a nullable
> `user_id`, a `SessionOrigin::Share` variant, a CHECK pairing the two,
> metrics exclusions and a cleanup predicate. **All of that is dropped.**
> Kept as a record of why, because the reasoning is the interesting part.

Checking what a session row would actually buy killed it:

| a row gives | what we decided |
|---|---|
| refresh-token rotation | anonymous **cannot** refresh |
| per-session revocation | we revoke the **share** |
| liveness metrics | anonymous must be **excluded** |
| admin sessions panel | anonymous must be **excluded** |

Every one is unwanted or something we would immediately suppress — while the
cost is a nullable FK, an `Option<Uuid>` refactor across the `Session` entity
and its callers, a cleanup predicate, metrics exclusions, and a 7+90-day
retention problem for rows nobody reads.

It also avoids a runtime hazard: `Session.user_id` is `Uuid` and the repo
does `row.get("user_id")`, so a NULL column would **panic on load** rather
than fail cleanly.

**So an anonymous session is a signed JWT and nothing else:**

- `role = anonymous` + `share_id` claim. No `auth.sessions` row, no `sid`.
- **Revocation still works, at the granularity that matters.** The engine
  re-checks the token grant on every request, so deleting a share drops its
  grant (cascade trigger) and every outstanding token is denied immediately.
  Verified the hot path allows this: `validate_token` is pure HMAC + expiry,
  and `resolve_live_role` looks up the *user* — already short-circuited for
  anonymous. No session-row read per request.
- Consistent with what already ships: `oxi_share_unlock_{token}` is exactly
  this shape — signed, expiring, non-revocable.
- **`OXICLOUD_SHARE_SESSION_EXPIRY_SECS`, default 4 h.** Separate from
  `access_token_expiry_secs` because with no refresh this value is the ENTIRE
  visit; at 1 h a visitor browsing a large folder stops working mid-browse.
  Going longer costs little — the token is strictly *weaker* than the link
  that produced it, and anyone holding the link can mint another. The one
  real window is a password-protected share, where an outstanding token
  survives a password change until expiry.
- **No refresh.** `POST /api/auth/refresh` is outside `auth_middleware`, so it
  rejects anonymous explicitly. A visitor whose token expires re-opens the
  share link.

Still required, unchanged from the original design:

- **Distinct, share-scoped cookie name** — *not* `oxicloud_access`, which is
  `Path=/`: reusing it means **a logged-in user who clicks a share link has
  their real session overwritten**. Follow the `oxi_share_unlock_{token}`
  precedent (`share_unlock_cookie.rs:79`).
- **Reusing the share owner's uuid as the principal id is disqualified** —
  `handle_session` auto-subscribes on `caller_id` with no authz check
  (`rt_ws.rs:402`, `:412`) and hardcodes `Subject::User(caller_id)` at `:657`.

**Known limitation, deferred not designed around:** one share per session.
`Subject` is single-valued, so two share links in two tabs clobber each other.
`subject_match_set` (`pg_acl_engine.rs:663-675`) already returns a *set* of
ids, so the fix is a `Subject::Tokens(Vec<Uuid>)` variant with no SQL change.

### No WebSocket — and the draft's claim was wrong

`GET /api/rt/ws` accepts `Authorization: Bearer <jwt>` as a co-equal path to the
ticket, checking only `sub_id != nil` and **never reading `claims.role`**
(`rt_ws.rs:240-255`). Blocking `/api/rt/ticket` does not close it. A browser
cannot set that header on `new WebSocket()`, so organic traffic is blocked — but
`curl` → JWT → `websocat` is not, and `/s/{token}/verify` would be an
unauthenticated token-minting oracle.

Required: reject `anonymous` inside `authenticate_upgrade`, **and** re-check at
`handle_session` before the auto-subscribes, **and** rate-limit `/verify`.

### …and the client must never ask for a ticket

A correct server-side denial becomes a client-side retry storm unless the page
never subscribes. `#onTicketFailure` (`client.svelte.ts:487-493`) **does not
inspect the status code** — a permanent 403 is retried on the same backoff as a
transient blip: ~20 `POST /api/rt/ticket` per tab over ~3 minutes, each running
the full auth + DPoP + CSRF chain, against an endpoint that is explicitly
unrate-limited (`rt_ticket_handler.rs:50-54`). Worse, **the circuit breaker
re-arms on every tab focus**: `#handleVisibilityChange` (`:391-394`) calls
`reconnect()`, which resets `#consecutiveFailures = 0` (`:322-327`). Tabbing
back and forth yields a fresh burst each time, so over a session it is
**unbounded** — defeating the breaker in exactly the case its own doc says it
exists for.

Two facts make the fix structural rather than conditional:

- **The connection is lazy.** `#connect()` fires only from inside `subscribe()`
  (`client.svelte.ts:267-271`), and both `#onClose` and `#onTicketFailure`
  reconnect only `if (#subs.size > 0)`. **No subscribe ⇒ no ticket request,
  ever.** `/s/*` issues zero today.
- **The four reused components are bus-free.** `ResourceList`, `PhotoLightbox`,
  `FileViewer` and `FolderBreadcrumb` have no bus coupling at all. It lives in
  exactly two places: `routes/files/[...path]/+page.svelte:471`
  (`useFolderTopic`) and `AppShell.svelte:342` (`useNotifications`) — neither of
  which the share page needs.

So: **`useFolderTopic` must stay in the authenticated page** and not migrate into
whatever folder-view module Phase 3 factors out. Pass it as an optional
`onLiveUpdate?` hook the share page simply does not provide.

Do **not** rely on the existing `serverConfig.features.message_bus` guard: it is
server-global, served from the deliberately public `/api/config`
(`routes.rs:170-178`), and its frontend fallback is **fail-open**
(`serverConfig.svelte.ts:35`) — all wrong properties for a per-session security
flag.

And **keep `session.user` null for anonymous sessions**. `useNotifications` is
unreachable from `/s/*` today by route *prefix*, not by role
(`+layout.svelte:35`, `:258-261`). If a populated `session.user.id` ever let an
anonymous visitor navigate to `/files/…`, `AppShell` mounts, `useNotifications`
subscribes, and the storm starts from there.

---

## What gets deleted

**Endpoints** (`routes.rs:127-153`): `/contents`, `/contents/{folder_id}`,
`/file/{file_id}`, `/download`, `/zip`, `/zip/{folder_id}`.
**Kept**: `GET /api/s/{token}` (payload reshaped) and `POST /api/s/{token}/verify`
(now session-minting). `/api/shares/*` untouched.

**Rust**: `share_browse_service.rs` entire (195 lines) + its `mod` line and four
`di.rs` sites; eight handlers and two helpers in `share_handler.rs` (779 → ~280);
`is_folder_in_subtree` / `is_file_in_subtree` (`folder_db_repository.rs:1420-1490`)
— `share_browse_service` is their only caller. **`OptionalUserId`**
(`auth.rs:85-98`), dead code.

**Frontend**: `getShareContents`, `shareDownloadUrl`, `shareFileUrl`,
`shareZipUrl`, the degenerate `ShareFileEntry`/`ShareListing` types, ~700 of 750
lines of the share page.

---

## Phases

### Phase 0 — Principal plumbing ✅ **DONE** *(no behaviour change; all dormant)*

Eleven commits, each gated and independently reviewable. Nothing mints an
anonymous session yet, so no behaviour changed.

1. ✅ `UserRole::Anonymous` with **two** parsers — `from_stored` (cannot yield
   anonymous; the DB has no such row) and `from_session` (can). Ordering is an
   explicit `rank()`, not a derived `Ord`, so reordering variants cannot
   silently invert every comparison. **No migration** — see the revised session
   model above.
   *Discovered here:* every existing parse site is `Some("admin") => Admin,
   _ => User` — fail-**open**. Adding a variant to an enum whose parsers default
   to `User` would have made `role: "anonymous"` a privilege escalation.
   `CurrentUser::role_enum()` resolves unknowns to `Anonymous` instead.
2. ✅ **`AuthUser` and `CurrentUserId` reject anonymous**, keeping ~200 handlers
   closed with zero edits. `OptionalUserId` **deleted** — zero call sites, so
   teaching it about anonymous would have guarded nothing.
   `require_role(&cu, min)` returns a `Result`, so `?` makes it unignorable.
3. ✅ **Allowlist** as a `route_layer` (see the note above).
4. ✅ **Anonymous short-circuit** in `resolve_live_role` — extracted as
   `anonymous_live_role` so the branch is testable rather than sitting untested
   in the authentication path.
5. ✅ **Fail-open site closed**: `require_internal_user`'s `_ => Ok(())` admitted
   a rowless principal past the only middleware guarding all three DAV
   surfaces. `NotFound` now denies; transient errors deliberately stay open,
   and that distinction is now written down and tested.
6. ✅ **Four bypasses patched.** The three DAV `extract_user` copies were
   **extracted into one** `auth_user_from_extensions` rather than patched
   three times — three copies is three chances to forget, which is how they
   drifted apart originally.
7. ✅ **`Subject` threaded** through the service entry points. Only the five
   read-path methods on `FileRetrievalUseCase` move; the other nine keep
   `caller_id: Uuid` **deliberately**, so the signature records which
   operations a non-user principal may reach.
   Added `Subject::user_id()` / `token_id()` — `Subject::id()` is the trap they
   exist to avoid, since it returns a share id that satisfies no FK.
8. ✅ **`notify_file_accessed` skipped** for callers with no user id — otherwise
   every share download is a guaranteed FK violation on
   `auth.user_recent_files`.
9. ✅ **Token predicate in `fetch_ancestor_walk`**, on `has_folder_grant` only.
   The breadcrumb then truncates at the share root with **no share-specific
   code in the caller**. Also stopped resolving `fetch_grant_by` for token
   callers — it would have published the **owner's username** to anonymous
   visitors.
10. ✅ **`CallerSubject`** — the single place a session (`role`) becomes an
    authorization subject (`Subject`). Refuses incoherent principals rather
    than guessing: `anonymous` with no `share_id` would otherwise fall back to
    `Subject::User(cu.id)` and match a *session* id against user grants.
11. ✅ **Stateless share sessions** — `share_id` JWT claim, no DB row, own TTL.

### Phase 1 — `/s/{token}` mints the session ✅ **DONE**

All seven allowlisted routes swapped (`7ecce01f`, `e56fe672`, `63f53a13`),
each carrying its own disclosure fix. The vertical slice below came first and
is kept because it records why each piece is shaped as it is.

**Vertical slice shipped** (`7ecce01f`, tests `730a4309`). One route swapped —
`GET /api/files/{id}/thumbnail/{size}` — which is enough to close #721 and to
exercise every layer end to end. What landed:

- `auth_middleware` parses `oxi_shares` **once, before any credential arm**, and
  inserts `ShareRing` for every caller — a logged-in Alice who opens a
  colleague's link keeps her identity *and* gains the share. A fourth arm builds
  the `Anonymous` principal when a valid ring exists and no user credential
  does, placed **after** the DAV challenge so a ring can never authenticate
  WebDAV/CalDAV/CardDAV.
- Both unlock paths mint: `POST /api/s/{token}/verify` on a correct password,
  and `GET /api/s/{token}` when the share has none — reaching `Ok` there already
  means the share is open to this caller. `attach_ring_cookie` is shared by
  both; failure is silent, degrading to the legacy endpoints rather than
  denying.
- `/verify` is rate-limited, budgeted from the **login** limits rather than a
  new knob: it is unauthenticated and runs Argon2id per call — a password oracle
  and a memory amplifier, the two properties that earned `/api/auth/login` its
  limiter. The three existing limiters collapsed into one `enforce`.
- `FileManagementUseCase::require_permission` takes `&[Subject]` and **returns
  the credential that granted**; `require_any` likewise. `get_thumbnail` hands
  `granted_by` to its cache-miss fetch — re-deriving the subject there could
  deny what was just allowed.
- The force-password-change layer skips anonymous principals. Without it every
  thumbnail request looked up a `visitor_id` matching no row, missing the flags
  cache and hitting the DB on the hottest GET in the app.

Ring TTL is its own `DEFAULT_TTL_SECS` (8h), deliberately not the access-token
TTL: there is no session for a visitor to refresh, and the share's `expires_at`
is still checked per request, so a long ring cannot outlive the share.

**Superseded from the draft:** `validate_principal_shape` is gone. The ring *is*
the anonymous session, so "holds a ring, is not a user" is what makes a
principal anonymous — the pairing is structural, leaving nothing to validate.

`GET /api/files/{id}` (`download_file`) followed, driven by a finding from the
API test: asserting each file through BOTH routes showed them disagreeing —
the thumbnail gave the authorization answer while the download route gave the
role-gate 403. Two routes over the same resource that do not agree is how a
visitor ends up with a capability nobody meant to grant, so they were brought
into line. Its Phase 2 disclosure fix had to land in the same commit rather
than after it (see below). The external-mount branch denies token callers with
404: the mount layer authenticates to the remote provider as a *user*, and a
visitor has no identity there to borrow.

**Two revocation bugs surfaced** (fixed in `7dcde85e`) — both latent until the
ring made token grants readable at request time:

- `trg_cleanup_role_grants_token` deletes the grant row in the DATABASE, so it
  bypasses `PgAclEngine::clear_role` and its `cascade_grant_cache` flush. A
  revoked link kept working for the full 30 s TTL.
- `set_expiry_for_subject` never flushed either, though `set_role` and
  `clear_role` both do. Shortening a share's expiry to a past instant is a
  revocation.

**Remaining in this phase:** swap the other five allowlisted routes to
`CallerSubjects`. Legacy endpoints stay alive throughout.

### Phase 2 — Disclosure fixes ✅ **DONE**

Shipped alongside each route swap, plus `4283e0d5` (owner identifiers) and
`773888d9` (`AccessSourceKind::Token`). The endpoints "work" without these,
which is exactly why they get skipped.

**A disclosure fix cannot lag its route swap.** `GET /api/files/{id}` proved
the ordering: opening the route first would have handed visitors `path` for a
release. So each remaining swap carries its own fix in the same commit, and
this phase is now a checklist against Phase 1 rather than a stage after it.

- ✅ **`GET /api/files/{id}?metadata=true` no longer returns `path`** to a
  token-granted caller. Keyed on the *credential that granted*, not the
  account: a logged-in user reaching a file only through someone else's share
  is treated exactly like an anonymous visitor.
- **`GET /api/folders/{id}` leaks the owner's absolute storage path** plus
  `created_by`/`updated_by`. `without_hierarchy_info()` already exists
  (`folder_dto.rs:174`, used at `grant_handler.rs:1074`). Must land with that
  route's swap.
- **Force `is_shared = false`** for token callers — it is a *subject-less* EXISTS
  (`folder_db_repository.rs:1705-1709`, `:1737-1741`) that tells a visitor which
  items inside the share are separately shared.
- **Emit `AccessSourceKind::Token`** — it exists but is `#[allow(dead_code)]`
  (`folder_dto.rs:417-421`); today a token caller reports `DirectShare`. Leave
  `fetch_grant_by` unmatched deliberately: its
  `SELECT username FROM auth.users WHERE id = granted_by` would publish the
  owner's username.
- ✅ The **file mount branch** denies token callers (404). The mount layer
  authenticates to the remote provider as a *user*; a visitor has no identity
  there to borrow, and 404 keeps "is this id a mount child" unprobeable.
  `folder_handler.rs:544-561` still to decide — same answer expected.

### Phase 3 — Component read-only + share page rewrite ✅ **DONE**

Shipped in `eee0ac45` (rewrite), `11031845` (layout), `a05254ed` (branding),
`99c79f58` (`?file=` deep links), `eea3e1db` (single-file share),
`772132a2` (the thumbnail-generation gate below).

750 → ~390 lines, half of it CSS and the password form. `GET /api/s/{token}` is
the only share-specific call left. Gone with the rewrite: a bespoke grid, a
hand-rolled lightbox with its own keyboard handling, a lazy-video
IntersectionObserver that seeked each `<video>` to rasterise a poster
client-side, an image-retry action, a hand-built breadcrumb, and a private
`oxi-share-view` localStorage toggle.

**The warning below was right and worth the ink.** `ResourceList`'s thumbnail
`onerror` fallback fetches the FULL ORIGINAL and `PUT`s three sizes back. For a
visitor the PUTs 403 but the fetch SUCCEEDS — #721 reappearing inside the
component that fixed it, on exactly the files most likely to be in a fresh
share (no server thumbnail yet). `allowThumbnailGenerate` gates it. Kept
separate from `enableThumbnails`, which governs *display*: a visitor should
still see server-rendered thumbnails, just never generate one.

Corrections to what follows: `readOnly` was needed on `PhotoLightbox`
(favorite + delete, rendered unconditionally) and `FileViewer` (the WOPI Edit
button, gated on whether the file TYPE is editable, not on permission) — but
not on `ResourceList` beyond the thumbnail gate. `useFolderTopic` never needed
guarding: no folder-view logic was factored out, the share page calls
`fetchFolderPage` directly, and it issues zero `POST /api/rt/ticket`.

Two layout traps, both from living outside `AppShell`: the page used
`--spacing-*` tokens that do not exist (the scale is `--space-1`…`--space-24`),
so every padding declaration was dead — and Stylelint enforces `var()` but not
that the variable *resolves*, so `npm run check` passed throughout. And `body`
is `display: flex`, so a bare `<main>` is a flex ITEM sized to its content: a
~390px column. Both fixed by reusing AppShell's own `main-content` +
`content-area` classes rather than approximating them.

<details>
<summary>Original plan text</summary>


`readOnly` on `ResourceList`, `PhotoLightbox`, `FileViewer`. No `apiBase` — URLs
don't change.

> **`ResourceList` is not free**: `:1134-1144` fires `queueThumbnailGenerate` by
> default, and `utils/thumbnail.ts:119` **fetches the full original** then `PUT`s
> a thumbnail — a default-on mutate path and a second instance of #721 inside the
> component being reused.

Share page 750 → ~180 lines. `/s/*` already renders outside `AppShell`
(`+layout.svelte:35`, `:258-261`), so **no AppShell minimal mode is needed**.

**The one structural constraint in this phase**: whatever folder-view logic is
factored out of `routes/files/[...path]/+page.svelte`, the `useFolderTopic` call
at `:471` stays behind in the authenticated page. If it migrates into the shared
module, every share visitor's browser starts the ticket retry storm described
above. Verified by a frontend test asserting the share page issues **zero**
`POST /api/rt/ticket`.

</details>

### Phase 4 — Delete the legacy surface ✅ **DONE** *(BREAKING)*

Shipped in `69623e7d`. 867 net lines: the six `/api/s/{token}` browsing
endpoints, `ShareBrowseService`, `is_folder_in_subtree` /
`is_file_in_subtree` on `FolderRepository` and its PG impl, and the frontend's
`getShareContents` plus three URL builders.

`public_shares.hurl` now asserts 404 on the deleted paths — the routing table
answering rather than a handler — while the browsing behaviour they covered is
exercised against the real endpoints in `anonymous_share_session.hurl`.

---

## Verification

**The route-coverage test is the load-bearing one** — ✅ shipped, though not in
the shape proposed. Walking the assembled router turned out unnecessary: both
markers are GENERATED from their gates (`ANONYMOUS_ALLOWLIST` and the
`/api/admin` prefix), so `scope_markers_match_their_gates` asserts the spec and
the gates agree in both directions, and drift by human omission is impossible
by construction rather than caught after the fact.

It still earns its place: it fails when an allowlisted route carries no
`#[utoipa::path]` — silently skipped by the injector, which would leave the
spec understating what a share link reaches. A second assertion pins that no
`http` scheme carries non-empty scopes, so the non-conformance noted at the top
cannot return.

**Shipped** in `tests/api/anonymous_share_session.hurl` (`730a4309`) — hurl keeps
one cookie jar per file, so it behaves like a browser, which also makes the
"logged-in Alice carries both credentials" case the default rather than a
special setup:

**The jar is the trap.** `POST /api/auth/login` sets `oxicloud_access`
(HttpOnly, `Path=/`) alongside the Bearer token, and hurl keeps one cookie jar
per file — so dropping the `Authorization` header does NOT make a request
anonymous. The first draft tested the admin's own permissions throughout and
looked like an authorization leak. The file is now split into sessions
separated by `POST /api/auth/logout` (which clears the three auth cookies and
leaves `oxi_shares` alone), and every anonymous section opens with a
`GET /api/auth/me` → 403 canary, because the failure is otherwise silent.

**Both routes, every time.** Each file assertion runs through
`/api/files/{id}` AND `/api/files/{id}/thumbnail/{size}`. They reach the same
answer by different paths, and the disagreement between them is what exposed
the un-swapped route.

- Ring issued on the password-less `GET /api/s/{token}`; `HttpOnly`, `Path=/`.
- Anonymous `GET /api/files/{id}/thumbnail/preview` → 200 (#721), via the ltree
  cascade from a *folder* share down to a file inside it.
- A file the same owner holds outside the shared subtree → **404, not 403**.
- Denied with a valid ring: `/api/users/{id}` (the owner's own name and email —
  and the visitor knows the owner exists), `/api/shares`, `/api/drives`,
  `/api/favorites`, `/api/trash/resources`, `/api/search`, `/api/auth/me`. Each
  403s *before* the handler rather than returning an empty list.
- Allowlist shape: `/api/folders` must not inherit from the allowlisted
  `/api/folders/{id}` (pattern, not prefix), and `DELETE` must not inherit from
  a listed path's `GET`.
- Password share issues **no** ring on the bare GET or a wrong password; the
  right password does.
- The ring **appends** — a second unlock does not evict the first.
- Revoking the shares kills access though the signed cookie still names them.
  This is what justifies the 8h TTL.

Unit: `CallerSubjects` composition (visitor contributes no `Subject::User`; a
user's own subject comes first; empty set refused) and the `require_role` gate.

Also shipped (`9c1fe7a2`) — the denials the allowlist does NOT cover, each
refusing for its own reason, so a change to the allowlist would not affect them
and nothing else in the suite would notice if one opened:

- `/webdav/`, `/caldav/`, `/carddav/` → 401 + `WWW-Authenticate`. The DAV
  challenge is emitted BEFORE the ring arm, so ordering is the guarantee.
- `POST /api/auth/app-passwords` → 403 from `require_role`, not the allowlist:
  the `/api/auth` nest has its own stack. Minting a DAV credential is the last
  thing a share link should reach.
- `POST /api/auth/refresh` → 401 from the handler itself (public, no middleware).
- `GET /api/rt/ws` → 401 from `authenticate_upgrade`. Sends a real RFC 6455
  handshake: `WebSocketUpgrade` is an extractor, so a plain GET is rejected
  with 400 before the handler body and would only prove axum parses headers.
- All seven allowlisted paths, as a 4×4 folder matrix — share root and a CHILD
  resolve, the PARENT and a SIBLING 404. The child is the one that matters:
  every other assertion targets the share root, so a cascade broken DOWNWARD
  would pass the whole file.

Still to do:
- **Playwright**: every grid image request on `/s/` matches `/thumbnail/`, none
  a bare `/files/{id}` — the direct #721 regression. Lower value now that the
  page structurally IS the real grid and the API test pins the thumbnail route
  anonymously, but it is the only check that would catch a future component
  change reintroducing a full-resolution fetch in the browser.
- Trash regressions.
- A file added *after* sharing is visible. **Declined** (Ed, 2026-09-16) — the
  grant cascades at check time by construction, and the 4×4 matrix already
  exercises the cascade in both directions.

---

## Operational

- **Metrics and cleanup: no longer applicable.** Both existed only because of
  the session row. Stateless sessions create nothing to count, nothing to
  exclude from `session_liveness_gauges`, nothing to purge, and no 7+90-day
  retention problem. The only residual check is that an anonymous JWT carries
  **no `sid`**, so `LastSeenTracker::stamp` never fires for one — it is keyed
  on `sid` (`auth.rs:240-242`, `:373-375`), so this holds by construction.
  *(The original text noted that `oxicloud_sessions_online_users` is
  `COUNT(DISTINCT user_id)`, which silently skips NULLs — sessions would have
  inflated while users did not, corrupting the tabs-per-user ratio with no
  error anywhere. Worth remembering if a session row is ever reconsidered.)*
- **Docs**: no session taxonomy exists anywhere. `architecture/auth-model.md` gains
  a "Session roles" section covering the three roles and the six existing
  `SessionOrigin` variants; `architecture/share-integration.md:35-40, :66-67`
  updated; `architecture/rebac-authorization.md` for `Subject::Token` as a
  request-time principal; `config/authentication.md` for operators.

---

## Risks

| Risk | Severity | Mitigation |
|---|---|---|
| A fifth auth bypass is added later | **Critical** | Route-coverage test over the assembled router; allowlist constant |
| `role` is a `SmolStr` — no compile error guides the change | **High** | `CurrentUser::is_anonymous()` as the single grep point |
| Anonymous refresh outliving share revocation | ~~High~~ **Resolved by design** | Stateless sessions cannot refresh, and the engine re-checks the token grant per request — deleting a share denies every outstanding token immediately, whatever its TTL |
| Outstanding token survives a share **password** change until TTL | **Low** | Bounded by `OXICLOUD_SHARE_SESSION_EXPIRY_SECS` (4 h). The pre-existing `oxi_share_unlock_{token}` cookie already has this property, so it is not a regression |
| Cookie collision downgrades a logged-in user's session | **High** | Distinct share-scoped cookie name |
| A share with no `role_grants` token row 404s | **High** | Pre-flight gate on Phase 4: count must be 0, else backfill |
| A Nextcloud surface builds `/api/s/{token}/file/{id}` | **High** | `public_shares.hurl:139-147` claims so — grep `nextcloud/`; **hard gate on Phase 4** |
| Session-minting oracle at `/verify` | **High** | Rate-limit per IP; the session table is otherwise a public write endpoint |
| "No WS" becomes a `/api/rt/ticket` retry storm | **High** | Share page never subscribes (structural — the connection is lazy); plus fix the client's terminal-failure handling |
| Share `access_count` stops incrementing once `/api/s/{token}` is no longer the browse entry | **Medium** | Product decision — increment at mint instead |
| 30 s cascade cache delays revocation | **Medium** | Invalidate `Subject::Token(share_id)` on revoke; accept for moves |

**Ruled out**: adding `caller_role` to `FileItem`/`FolderItem` (per-row SQL cost on
the hottest listing endpoint, to answer what `capabilities.read_only` already
says); letting `UserRole` default to anything but `Anonymous`; reusing the owner's
uuid for `sessions.user_id`.

---

## Pre-existing bugs found by the audits *(independent of this work)*

1. **`rt_ws.rs:38-40` documents that DPoP-bound tokens are rejected on the bearer
   path. No such rejection exists** — `validate_token` checks signature and expiry
   only. A stolen DPoP-bound access token opens a WebSocket today with no proof of
   possession. This false comment is why the ticket looked like the only door.
2. **`require_internal_user` fails open** (`user.rs:64-71`) — any `get_user_flags`
   error admits the caller past the internal-only gate on all three DAV surfaces.
3. **`decide_live_role` fails open on transient DB error** (`user.rs:169-176`),
   resurrecting the claim role. Under load this is correlated: a connection blast
   causes the DB pressure that unlocks it.
4. **No connection budget anywhere** — `POST /api/rt/ticket` is explicitly
   unlimited (`rt_ticket_handler.rs:50-54`), and WS connections are bound to a
   *user*, never a *session*, so logout does not close them (`rt_ticket_store.rs:68`).
5. **The bus client retries permanent authorization failures forever.**
   `#onTicketFailure` (`client.svelte.ts:487-493`) ignores the status code, so a
   401/403 is retried like a transient blip; and `#handleVisibilityChange`
   (`:391-394`) resets `#consecutiveFailures`, so tab-focus cycling re-arms the
   circuit breaker indefinitely. This already affects **revoked sessions today** —
   the exact case the breaker's own comment says it was added to prevent. Fix:
   treat 401/403 as terminal, and add a `#terminal` flag `reconnect()` refuses to
   clear.
