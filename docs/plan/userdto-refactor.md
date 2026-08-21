# UserDto Refactor — Three-Layer Split (Public / Full / Self)

> **Status — SHIPPED 2026-08-21.** All eight phases landed and all gates
> pass: `cargo clippy --all-targets --all-features -D warnings` clean,
> `cargo fmt --check` clean, `cargo test three_layer_quarantine` (2/2
> structural-quarantine tests pass), `npm run check` (593 files, 0
> errors, 0 warnings), `npm run test:unit` (414 pass / 1 skipped / 0
> failed), OpenAPI regenerated at `resources/gen/openapi.json`. See the
> [Phasing](#phasing) section below for the per-step outcome. The doc
> is retained as the reference for anyone extending the three-layer
> shape (new field → decide by audience per the rule in the opening
> section).

Establish three DTO shapes for representing a user on the wire, each
with a single unambiguous audience, composed hierarchically so the
overlap between audiences is defined ONCE:

- **`PublicUserDto`** — public identity. What any authenticated caller
  may see about *another* user. Returned by `/api/users/{id}`, share
  responses, group members, magic-link invitees, recipient enrichment.
- **`FullUserDto`** = `PublicUserDto` + all fields that BOTH an admin
  (viewing another user) AND the subject themselves (viewing
  themselves) may see. Returned as `Vec<FullUserDto>` by
  `/api/admin/users`. Closest DTO to the underlying `auth.users` row.
- **`SelfUserDto`** = `FullUserDto` + self-only preferences,
  session-scoped flags, and caller-scoped permissions. Returned by
  `/api/auth/me` and by the login / refresh / OIDC / magic-link auth
  response.

Composition (`FullUserDto.user: PublicUserDto`,
`SelfUserDto.full: FullUserDto`) means the public-identity contract
has ONE definition; the overlap between admin's view and self's view
is another single definition. Adding a new field naturally finds its
level:

- Useful to any authenticated caller? → `PublicUserDto`.
- Useful only to admin (about another user) and the subject
  themselves? → `FullUserDto`.
- Meaningful only to the caller viewing themselves? → `SelfUserDto`.

Companion of `docs/plan/sessions.md` (which introduced `is_online`
and motivated widening the DTO for presence). Same principle: pick
the audience first, structure the DTO around it, don't let the same
field mean different things on different endpoints.

## Why now — the problems this fixes

Today's single `UserDto` conflates three audiences. Symptoms:

1. **The "quiet lie"**. `UserDto::has_password` is populated only by
   `/api/auth/me`; every other emitter (`From<User>`) leaves it
   `false`. A share-recipient DTO on the wire says
   `has_password: false` unconditionally, which an attacker
   scraping share responses could misread as "this user is
   passwordless" when the truth is "we didn't fill this field in
   for you". Same pattern for `force_password_change` and
   `is_dpop_bound`. See `src/application/dtos/user_dto.rs:134-138`
   for the explicit disclaimer — the convention exists precisely
   because the field placement is wrong.

2. **Private signals leak by default**. `last_login_at`,
   `notify_on_share`, `ui_preferences`, `preferred_locale`,
   `federation_kind`, `email_verified_at`, and
   `storage_used_bytes` all ride on `UserDto` and are returned to
   any authenticated caller who can see a given user. Group
   members can see when their peers last logged in, which IdP they
   federate with, and how full their disks are. None of this is
   information a share picker or a member listing needs.

3. **`AdminUserSummaryDto` duplicates a chunk of `UserDto` verbatim**
   (id / username / email / role / quotas / last_login_at / active /
   federation_* / is_external), then adds three admin-only fields
   (has_password / opaque_registered / opaque_migrated). The two
   shapes drift naturally as new fields are added — no compile-time
   guarantee they stay in sync.

4. **N+1 in the admin panel**. Because `AdminUserSummaryDto` doesn't
   include `image`, the admin users table fires `/api/users/{id}`
   per row so `UserVignette` can render the avatar. Composition
   (`FullUserDto.user.image`) lets the admin listing seed the SPA's
   per-user cache from the list rows directly.

## Target shapes

### `PublicUserDto` — public identity (9 fields)

Applied rule: "would a share picker / group member listing /
recipient enrichment need this? if no, it doesn't belong here."

```rust
pub struct PublicUserDto {
    pub id: String,
    pub username: Option<String>,
    pub email: String,
    pub role: String,                    // sharee UI renders admin badge
    pub image: Option<String>,           // avatar
    pub is_external: bool,               // external badge
    pub given_name: Option<String>,      // social identity
    pub family_name: Option<String>,     // social identity
    pub is_online: bool,                 // presence — social signal
}
```

Every existing UserDto emitter site (`From<User>`, share responses,
group members, magic-link invitees, sharee-vignette lookup)
returns this slim shape. All private signals below vanish from
those wire paths.

### `FullUserDto` — admin's view of anyone + self's view of self (13 extras)

The fields the SUBJECT themselves may know about themselves that
an ADMIN may also know about the subject. Composed on top of
`PublicUserDto`. This is the DTO closest to the underlying
`auth.users` row.

```rust
pub struct FullUserDto {
    /// Public identity — same set any authenticated caller can see.
    pub user: PublicUserDto,
    /// IdP linkage. Which SSO provider a peer uses is a soft
    /// org-affiliation leak; not needed by share pickers.
    pub federation_kind: Option<String>,
    pub federation_issuer: Option<String>,
    /// Subject's own locale preference. Only THEY or an admin
    /// managing them needs this — other callers use their own.
    pub preferred_locale: Option<String>,
    /// Email-verification stamp. Trust signal — meaningful to admin
    /// (auditing verification status) and to self (own record), but
    /// not to a share picker rendering a vignette.
    pub email_verified_at: Option<DateTime<Utc>>,
    /// Row bookkeeping — not rendered on any non-admin surface today.
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Activity signal — private.
    pub last_login_at: Option<DateTime<Utc>>,
    /// Account-active flag — private (a deactivated user couldn't
    /// reach `/me` anyway, but admin needs to see it).
    pub active: bool,
    /// Storage quotas — personal financials. Admin manages others';
    /// self sees own.
    pub storage_quota_bytes: i64,
    pub storage_used_bytes: i64,
    /// Auth capability set — has_password / OPAQUE flags. Kept off
    /// public identity because per-user auth adoption leaks through
    /// directory endpoints.
    pub has_password: bool,
    pub opaque_registered: bool,
    pub opaque_migrated: bool,
}
```

### `SelfUserDto` — /api/auth/me (5 extras)

Everything the caller may see about themselves that no other
caller (not even an admin) needs to see: pure self-scoped state.

```rust
pub struct SelfUserDto {
    /// Full profile. Every field an admin would see about you is
    /// here — same shape as one row of /api/admin/users.
    pub full: FullUserDto,
    /// Opaque UI preferences bag — my own UI state. Cross-device
    /// via PATCH /api/auth/me/profile.
    pub ui_preferences: serde_json::Value,
    /// Whether I want share-notification emails.
    pub notify_on_share: bool,
    /// Session-scoped: my current session carries a DPoP thumbprint.
    /// SPA skips a redundant /api/auth/dpop/bind on load when true.
    pub is_dpop_bound: bool,
    /// Admin-set temp-password gate — SPA nav guard blocks everything
    /// but /change-password until this flips back.
    pub force_password_change: bool,
    /// Caller-scoped permission: can I edit my own avatar? False for
    /// OIDC users whose avatar comes from the IdP. Only meaningful
    /// when caller == subject; nonsense on any other DTO.
    pub can_edit_image: bool,
}
```

## Endpoint mapping

| Endpoint | Old shape | New shape |
|---|---|---|
| `/api/auth/me` | `UserDto` (fat) | `SelfUserDto` |
| `/api/auth/login` / `/refresh` / OIDC callback / magic-link redemption | `AuthResponseDto { user: UserDto }` | `AuthResponseDto { user: SelfUserDto }` |
| `/api/admin/users` | `Vec<AdminUserSummaryDto>` | `Vec<FullUserDto>` |
| `/api/users/{id}` | `UserDto` (fat) | `PublicUserDto` (9 fields) |
| Share responses / group members / magic-link invitees / recipient enrichment | `UserDto` (fat) | `PublicUserDto` |

**Login response ships `SelfUserDto`, not `PublicUserDto`.** The SPA
needs `has_password`, `ui_preferences`, `is_dpop_bound`, and
`force_password_change` immediately post-login to avoid a UI race
with the first `/me` fetch. Same rationale for refresh and
OIDC/magic-link callback: the SPA's post-auth state must be
complete in one round trip.

## Backend callsite inventory

**DTO layer** (`src/application/dtos/user_dto.rs`):

- Replace `UserDto` with `PublicUserDto` (renamed AND slimmed —
  same rename forces every consumer to consciously pick the new
  shape rather than silently losing fields).
- Add `FullUserDto` and `SelfUserDto`.
- Delete `AdminUserSummaryDto` (superseded by `FullUserDto`).
- `impl From<User> for PublicUserDto` — the entry point. Maps 1:1
  to `User`'s public identity accessors.
- `FullUserDto::build(user: User, flags: UserDerivedFlags)` —
  wraps a `PublicUserDto` plus the DB-computed booleans not on
  `User` (`has_password`, `opaque_registered`, `opaque_migrated`,
  `is_online`). Every other FullUserDto field comes from `User`
  directly. Not a `From` impl because the second argument is
  needed and Rust's `From` is single-arg.
- `SelfUserDto::build(full: FullUserDto, session_ctx: SessionContext)`
  — helper taking a FullUserDto plus caller context (session's
  DPoP-bound flag, admin-set force-password-change flag). Same
  reason as FullUserDto's builder — not a `From` impl.

**Repository layer**
(`src/infrastructure/repositories/pg/user_pg_repository.rs`):

- **Delete `UserListEntry`** — the narrow projection was a perf
  optimization; Path B decision supersedes it.
- `list_users` returns `Vec<(User, UserDerivedFlags)>` where
  `UserDerivedFlags` is a small named struct in
  `domain/repositories/user_repository.rs`:

  ```rust
  /// DB-computed booleans about a user that aren't fields on the
  /// `User` entity itself — either derived from column presence
  /// (`password_hash IS NOT NULL`) or from a cross-table lookup
  /// (`auth.sessions.last_seen_at` for `is_online`). Companion
  /// to `User` on the list projection: the repo computes both,
  /// the application layer packs them into `FullUserDto`.
  pub struct UserDerivedFlags {
      pub has_password: bool,
      pub opaque_registered: bool,
      pub opaque_migrated: bool,
      pub is_online: bool,
  }
  ```

  Not "admin-only" — every field ends up on `FullUserDto`, which
  both admin AND self read. The name reflects "derived from the
  DB row, not intrinsic to the User entity".
- SELECT widens to include `image` (previously narrowed away
  per ROUND12 §Q1) + the `EXISTS(...)` scalar for `is_online`,
  bound with `ONLINE_WINDOW.as_secs_f64()` via
  `make_interval(secs => $N)` — same pattern as
  `session_liveness_gauges.rs:104-114`, single source of truth,
  no SQL literal.

**Handler / service layer**:

- `/api/auth/me` handler — builds `SelfUserDto` from
  `(User, UserDerivedFlags, session_context)`. The
  `UserDerivedFlags` for /me comes from a reuse of the
  list-repo path scoped to `WHERE id = $me` or a new small
  dedicated query (implementer's call — either works).
- `AuthResponseDto` shape follows — `user: SelfUserDto` field.
- Every login/refresh/OIDC/magic-link path that mints an
  `AuthResponseDto` computes the same SelfUserDto.
- Admin service `list_users_admin` — returns `Vec<FullUserDto>`.
- `/api/users/{id}` handler — returns `PublicUserDto`. Every
  other public consumer stays on `PublicUserDto`.

## Frontend callsite inventory

**Type changes** (`frontend/src/lib/api/types.ts`):

- Rename `User` interface → `PublicUser` and slim to match new
  DTO (9 fields).
- Add `FullUser` interface — `{ user: PublicUser, federation_kind, ... }`.
- Add `SelfUser` interface — `{ full: FullUser, ui_preferences, ... }`.
- Delete `AdminUser` (replaced by `FullUser`).

**Store changes**:

- `lib/stores/session.svelte.ts` — reads /me, must handle SelfUser
  shape. Recommendation: keep a derived `session.me: SelfUser` for
  the full record and shorthand accessors:
  `session.user: PublicUser` = `session.me.full.user`,
  `session.full: FullUser` = `session.me.full`.
  Existing `session.user.username` calls keep working via the
  shorthand; new self-only reads go through `session.me.foo` or
  `session.full.foo`.

**Component changes**:

- Profile / change-password / DPoP-bind pages — reads
  `session.me.has_password`, `session.me.is_dpop_bound`,
  `session.me.can_edit_image`, `session.full.preferred_locale`,
  etc.
- `routes/admin/[[tab]]/+page.svelte` users table — every
  `u.username` → `u.user.username`, every `u.last_login_at` /
  `u.active` / `u.has_password` stays top-level (FullUserDto
  fields). Also **seed `resolveUser` cache with `u.user`** in the
  load path — kills the N+1 that motivated widening the query.
- Admin sessions table's `UserVignette` — no change, `user_id`
  passed through unchanged; the users-table cache seed above
  satisfies the vignette lookup on cross-table navigation.
- `lib/composables/useOwnerCache.ts` /
  `lib/api/endpoints/users.ts` — `resolveUser` returns
  `PublicUser`. No signature change; the return shape only gets
  smaller. Add a `seedUser(u: PublicUser)` export so the admin
  table can prime the cache.

## Phasing

Each step compiles standalone; each is a reasonable review chunk.

1. **Introduce the new DTOs** — add `PublicUserDto`, `FullUserDto`,
   and `SelfUserDto` alongside the existing `UserDto`. Don't
   change `UserDto` yet. Compiles; no behaviour change.
2. **Widen repo projection** — add `is_online` (via EXISTS
   subquery) and `image` back to `list_users` SELECT. Introduce
   `AdminExtras` struct. `UserListEntry` still exists but is now
   redundant (fields also available on `User`).
3. **Migrate the emitter sites** — `/api/auth/me`,
   login/refresh/OIDC/magic-link, admin service. Each now builds
   the new nested shape. Old `UserDto` still ships every field.
4. **Rename `UserDto` → `PublicUserDto` and slim** — remove the
   moved fields. The Rust compiler flags every remaining consumer
   that reads a removed field; those either move to `.full.foo` /
   `.user.foo` (embedded) or promote themselves to a Self/Full
   DTO.
5. **Delete `UserListEntry` + `AdminUserSummaryDto`** — dead after
   the cutover.
6. **Frontend** — rename types (`User` → `PublicUser`), add
   `FullUser` / `SelfUser`, update session store, all consumers.
   Seed `resolveUser` cache from admin table.
7. **Regenerate OpenAPI** — `cargo run --bin generate-openapi`
   picks up the new schemas; the shrunken `PublicUserDto` schema
   documents the new contract.
8. **Delete obsolete doc comments** — `has_password` /
   `is_dpop_bound` / `force_password_change` comments on the old
   UserDto explaining "populated only by `/me`" become obsolete
   (the field structurally can't exist on non-self emitters).

## Wire-shape breaking changes

All in-repo consumers (backend + SPA) migrate in the same commit.
External consumers: none today — `/api/admin/users` is
admin-panel-only, `/me` is SPA-only, share/group endpoints are
SPA-only. Ship as one clean break; skip a `?shape=v2` deprecation
window.

Every removed field from a public UserDto path (share responses,
group members, magic-link invitees, `/api/users/{id}`) is a
deliberate leak reduction, not a regression. Any FE consumer that
was reading e.g. `sharee.has_password` was reading a "quiet lie"
anyway (always `false`).

## Testing

**Backend**:

- Round-trip tests for each new DTO type (already have for
  UserDto; extend to PublicUserDto + FullUserDto + SelfUserDto).
- **Structural quarantine tests** —
  `self_user_dto_does_not_leak_ui_preferences_via_public_paths`:
  serialize a `SelfUserDto`, assert `ui_preferences` appears
  ONLY at top level, not inside `.full.user` or `.full`. Same for
  `FullUserDto` — `has_password` at top level of `FullUserDto`,
  not inside `.user`.
- Update every service test that constructs `UserDto` fixtures.

**Frontend**:

- The TS type system catches every consumer that reads a removed
  field. `npm run check` surfaces the whole blast radius on the
  first pass — no new test infrastructure needed.
- Add one Vitest integration on the admin users table asserting
  the presence dot renders AND `/api/users/{id}` is NOT called
  per row (checks `apiFetch` mock call count).

**Wire-shape guard**:

- Hurl test hitting `/api/users/{id}` as a non-admin caller,
  asserting the response does NOT contain moved fields
  (`has_password`, `last_login_at`, `notify_on_share`,
  `ui_preferences`, `storage_used_bytes`, `federation_kind`,
  `preferred_locale`, `email_verified_at`, `created_at`, etc.).
  Anti-regression guard for the whole point of this refactor.

## Non-goals

- **Reworking the `User` domain entity** — this refactor is
  DTO-shape only. The entity keeps all its fields.
- **Visibility-rule changes on `/api/users/{id}`** — who can see
  whom stays as-is; only the field set narrows.
- **Splitting other DTOs** — `SessionSummaryDto`, `FileDto`, etc.
  Same principle would apply, but each is a separate design call.
- **Moving avatars out of the row** — planned separately. This
  refactor keeps `image` on `PublicUserDto` so the admin-panel
  N+1 fix survives.
- **Flattening the nested shape via `#[serde(flatten)]`** — the
  three-level wire shape (`me.full.user.username`) is slightly
  deeper than a flat DTO would be, but the structural quarantine
  is worth the cost. Reconsider if consumer readability suffers.

## Open questions

1. **Wire nesting depth on `/me`**: `me.full.user.username` is 3
   levels. Acceptable? Alternative: `#[serde(flatten)]` on
   FullUserDto and SelfUserDto so the wire is flat
   (`me.username`, `me.has_password`, `me.ui_preferences` all
   at top level), while keeping structural quarantine at compile
   time only. Simpler for FE consumers, loses runtime
   introspectability (a receiver can't tell which fields are
   public vs full vs self from the shape). Recommendation: ship
   nested; revisit if FE readability suffers.
2. **`created_at` / `updated_at` on FullUserDto** — not rendered
   anywhere currently. Keep for compat unless there's a
   compelling reason to drop.

## Memory notes to update on landing

- Extend `project_sessions_last_seen_at_shipped` with a "led to"
  pointer at this refactor.
- New note `project_userdto_three_layer_split` — captures the
  PublicUserDto / FullUserDto / SelfUserDto pattern + the
  decision rule ("would any authenticated caller need this?
  PublicUserDto. would self+admin? FullUserDto. self only?
  SelfUserDto.").
- Delete the `AdminUserSummaryDto` and `UserListEntry`
  references in earlier memory notes.
