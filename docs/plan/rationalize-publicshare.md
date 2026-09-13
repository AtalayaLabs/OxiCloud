# Rationalize public shares — anonymous sessions on the normal API

**Status: proposed.** Triggered by issue #721 (public share grids load
full-resolution originals) and by the TODO in `share_handler.rs:571` —
*"remove this and use the classic /api/files & /api/folders get, but with the
token as session ?"*

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

### Enforcement: allowlist at the middleware

Anonymous sessions reach **only** routes on an explicit allowlist constant.
Everything else is denied at `middleware/auth.rs:229 / :294 / :365`, before any
handler, bespoke extractor or hand-rolled gate can see the principal.

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

### Session model

- **`role = anonymous`**, **`origin = SessionOrigin::Share`** (new variant + CHECK
  migration). `origin` is the discriminator for metrics, cleanup and admin
  filtering — a positive assertion, unlike an absent `user_id`.
- **`sessions.user_id` becomes nullable**, with
  `CHECK ((role = 'anonymous') = (user_id IS NULL))`.
  **Reusing the share owner's uuid is disqualified**: `handle_session`
  auto-subscribes on `caller_id` with no authz check (`rt_ws.rs:402`, `:412`) and
  hardcodes `Subject::User(caller_id)` at `:657`, so the visitor would join the
  owner's authz/notification streams and could subscribe to any folder in the
  owner's drive.
- **Distinct, share-scoped cookie name** — *not* `oxicloud_access`. That cookie is
  `Path=/`, so reusing it means **a logged-in user who clicks a public share link
  has their real session overwritten** and every subsequent request becomes
  anonymous. Follow the existing `oxi_share_unlock_{token}` precedent
  (`share_unlock_cookie.rs:79`).
- **Short TTL (≈4 h, capped by `share.expires_at`), no refresh.** `POST
  /api/auth/refresh` is outside `auth_middleware`, so it must reject anonymous
  explicitly — otherwise a visitor renews indefinitely past share revocation. The
  page silently re-runs `/s/{token}/verify` on expiry; the unlock is idempotent.
- **Session carries a bag of share ids.** `subject_match_set`
  (`pg_acl_engine.rs:663-675`) already returns `(Vec<&str>, Vec<Uuid>)`, so
  "any of my tokens grants this" needs **no query change**. This is what makes
  multi-tab work and removes any precedence rule between a user session and a
  share.

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

### Phase 0 — Principal plumbing *(no behaviour change; ships alone)*

1. `UserRole::Anonymous`, `SessionOrigin::Share` + CHECK migration, nullable
   `sessions.user_id` + the paired CHECK, `CurrentUser::is_anonymous()`.
2. **Anonymous short-circuit in `auth_middleware`** at `auth.rs:198`, `:208`,
   `:340`, `:349` — today an anonymous session cannot even pass: the nil-sub check
   rejects it, and `resolve_live_role` → `get_user_flags` → `NotFound` →
   `LiveRole::Revoked` → 401. Early return in
   `require_no_password_change_pending_layer` (`user.rs:320`) too, or every
   anonymous request pays an uncached `auth.users` SELECT (moka never caches
   errors).
3. **Allowlist layer** + `403` + `authz.denied` audit for anything off it.
4. **Close the fail-open sites**: `require_internal_user` (`user.rs:64-71`) admits
   a rowless principal through its `_ => Ok(())` arm — the only middleware
   guarding `/webdav`, `/caldav`, `/carddav`. Also `decide_live_role`
   (`:169-176`), which resurrects the *claim* role on a transient DB error.
5. **Patch the four bypasses**: `middleware/admin.rs:61` and `:119`; the three DAV
   `extract_user` functions; `rt_ws.rs:240-255`.
6. Thread `Subject` into seven entry points — `folder_service.rs:448, :529, :1170,
   :1312, :1377`; `file_retrieval_service.rs:192, :212, :230, :252`;
   `file_management_service.rs:440`. Precedent: `drive_management_service.rs:83`
   already takes `Subject` and guards tokens at `:104`.
7. **Skip `notify_file_accessed`** for callers with no user id
   (`file_retrieval_service.rs:542, :637, :664, :709`) — `auth.user_recent_files.user_id`
   is `NOT NULL REFERENCES auth.users(id)`, so every anonymous download is a
   guaranteed FK violation: spawned and warn-logged, not fatal, but a log flood
   and a permanently poisoned moka throttle entry.
8. **Token predicate in `fetch_ancestor_walk`** (`folder_db_repository.rs:1550`) —
   `has_folder_grant` **only, never the drive grant**. Today the walk is blind to
   token grants, so the chain empties and ancestors 404. With the predicate the
   boundary lands exactly on the share root and nothing above it is returned.

### Phase 1 — `/s/{token}` mints the session

`/verify` (and the no-password path) resolves via `get_shared_link_with_unlock` —
**one call that preserves the password gate and expiry**, being the same call the
bespoke path makes — then mints or augments the anonymous session, adding
`share_id` to its bag. Reshape `GET /api/s/{token}`. Rate-limit `/verify`. Legacy
endpoints stay alive.

### Phase 2 — Disclosure fixes *(the ones most likely to be missed)*

The endpoints "work" without these, which is exactly why they get skipped:

- **`GET /api/folders/{id}` leaks the owner's absolute storage path** plus
  `created_by`/`updated_by`. `without_hierarchy_info()` already exists
  (`folder_dto.rs:174`, used at `grant_handler.rs:1074`). Same on
  `GET /api/files/{id}?metadata=true` (`file_handler.rs:881`).
- **Force `is_shared = false`** for token callers — it is a *subject-less* EXISTS
  (`folder_db_repository.rs:1705-1709`, `:1737-1741`) that tells a visitor which
  items inside the share are separately shared.
- **Emit `AccessSourceKind::Token`** — it exists but is `#[allow(dead_code)]`
  (`folder_dto.rs:417-421`); today a token caller reports `DirectShare`. Leave
  `fetch_grant_by` unmatched deliberately: its
  `SELECT username FROM auth.users WHERE id = granted_by` would publish the
  owner's username.
- Decide the two **mount branches** (`folder_handler.rs:544-561`,
  `file_handler.rs:848-859`): thread `Subject` or deny tokens outright.

### Phase 3 — Component read-only + share page rewrite

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

### Phase 4 — Delete the legacy surface *(BREAKING, single commit)*

After the pre-flight gates.

---

## Verification

**A route-coverage test is the load-bearing one.** Walk the assembled router and
assert every path either denies anonymous or appears in the allowlist constant.
With four independent auth paths, enumeration is the only durable guarantee — a
reviewer will not catch the fifth.

Plus:
- Anonymous denied on `/api/users/{id}`, `/api/groups/search`,
  `/api/address-books`, `/api/admin/*`, `/webdav`, `/caldav`, `/carddav`,
  `POST /api/auth/refresh`, `POST /api/auth/app-passwords`,
  `POST /api/rt/ticket`, and `GET /api/rt/ws` **with a bearer token**.
- All seven allowlisted paths work for an anonymous session; ids outside the share
  404.
- Two share tokens in one session both resolve.
- A logged-in user with no grant still reads a colleague's share.
- A file added *after* sharing is visible (no positive test exists today).
- Trash regressions; password 401-then-200; expiry after revoke.
- Playwright: every grid image request matches `/thumbnail/`, none a bare
  `/files/{id}` — the direct #721 regression.

---

## Operational

- **Metrics**: add `AND origin <> 'share'` to all three queries in
  `session_liveness_gauges.rs:108-135` and to `SessionSummaryDto::is_online`.
  `oxicloud_sessions_online_users` is `COUNT(DISTINCT user_id)`, which **silently
  skips NULLs** — so sessions would inflate while users would not, corrupting the
  documented tabs-per-user ratio with no error anywhere. Consider not stamping
  `last_seen_at` at all for anonymous (`auth.rs:240-242`, `:373-375`).
- **Cleanup**: today a visitor row would persist **7 + 90 days**
  (`expires_at` + `RETENTION_DAYS`). Add a second predicate —
  `origin = 'share' AND expires_at < NOW()` — and revoke anonymous sessions when
  their share is revoked or expires (`share_service.rs:443`). Nothing can reach
  them today: `revoke_all_user_sessions` is keyed on `user_id`.
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
| Anonymous refresh outliving share revocation | **High** | No refresh for anonymous; short TTL; revoke-on-share-revoke |
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
