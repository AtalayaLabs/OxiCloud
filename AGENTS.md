# AGENTS.md

This file provides guidance to coding agents (Claude Code, Codex, Cursor, Aider, …) working with this repository. Claude Code reads it via `@AGENTS.md` in `CLAUDE.md`.

# Purpose — what OxiCloud is, and what it is not

Read this before designing anything. Most "should we…" questions are answered by
the scale target rather than by taste.

- **Open source, and it stays that way.** MIT (`LICENSE`). This is a constraint on
  what you may add, not a footnote: dependencies must be license-compatible — a
  GPL/AGPL crate or npm package would force the whole project to relicense, and
  that is not on the table. No feature may be gated behind a licence key or an
  "enterprise edition", and nothing core may hard-depend on a proprietary service
  or SDK. Vendored frontend assets (`frontend/static/vendors/`) carry the same
  rule; record the licence when vendoring.
- **Self-hosted**, for an individual or an enterprise. The operator is not an SRE
  team: defaults must be safe, failures loud, and nothing may silently depend on a
  cloud service.
- **Target scale is up to ~10k users.** Design against that number in both
  directions. Do not build million-user machinery (sharding, eventual consistency,
  service decomposition) for a load that will never arrive; equally, never ship
  anything O(users) per request, or a table scan that is fine at 50 users and
  fatal at 10k.
- **Not a mass hoster.** OxiCloud does not claim to serve millions of users on one
  deployment, and trade-offs should not pretend otherwise.
- **Decentralised by intent.** Many instances federating beats one large instance —
  OpenCloudMesh is one route. Prefer designs that survive "this is one of many
  instances" over ones assuming a single authoritative deployment.

Targets:

- **Feature ambition: Google Workspace / Office 365.** Breadth of capability is a
  goal, not scope creep.
- **Collaboration is the main feature axis.** OxiCloud is not a personal backup
  drive that happens to have sharing bolted on — sharing, shared drives, grants,
  co-editing (WOPI) and live updates are the product. When choosing what to build
  or how to build it, the multi-user case is the primary one, not the case to
  generalise to later. A feature that works only for a single owner is unfinished.
- **Customer target: NextCloud users.** Hence the NextCloud-compatible API surface
  (`/remote.php`, `/ocs`, `/status.php`) — compatibility is a feature, and breaking
  it costs adopters.

## Design axes

Four things decide an open design question. **Security and resilience are
absolute** — they are not traded against anything. Performance is measured against
the 10k target. Privacy is a direction with a stated endpoint.

- **Resilience.** This is a storage product: **no data loss, no data corruption,
  ever.** Anything that can silently drop or alter bytes is a top-severity defect,
  not a trade-off. In practice that means: a job that skips work must never report
  success (pause at a cursor instead — `docs/plan/jobs-handling-recoverable-error.md`);
  a read failure is never proof that data is absent; content-addressing and
  ref-counting are load-bearing, not decoration; and consistency checks are
  discovery-only unless repair is explicitly requested.
- **Security.** Prefer deny-by-default over assert-later; a guarantee enforced by
  the type system or the router beats one a reviewer must remember. AuthZ lives in
  the service layer, never in handlers. See `src/AGENTS.md` § AuthZ enforcement
  points.
- **Performance.** Measure against 10k users, not a dev instance. The hot paths are
  listing, thumbnails and auth — a per-row query or an extra round trip there is a
  real regression even when it looks harmless.
- **Privacy.** When the backend belongs to a third party (S3, Azure), encryption at
  rest is a *should-have*; **end-to-end encryption is the target.** Designs that
  assume the server can always read plaintext will have to be undone — the `Vault`
  drive kind is reserved for the E2E case.

Where two conflict, resilience and security win, and the cost is documented.

# Architecture

This project is split into two parts:
- `/src` — OxiCloud Backend server in **Rust**
- `/frontend` — OxiCloud Frontend: a **SvelteKit (Svelte 5) + TypeScript** single-page app built with Vite

> The original vanilla-JS/CSS frontend still lives in `/static` and is retained
> during the migration, but new frontend work goes in `/frontend`. Vite builds
> the SvelteKit app to `static-dist/`, which the Rust web layer serves in
> release.

# Backend part

## Backend Build & Dev Commands

```bash
cargo build                          # Dev build
cargo build --release                # Optimized release build
cargo run                            # Run server (port 8086)
cargo test --workspace               # Run all tests (~208)
cargo test <test_name>               # Run a single test by name
cargo test --features test_utils     # Run tests that use mockall mocks
cargo clippy -- -D warnings          # Lint (zero warnings policy)
cargo fmt --all --check              # Format check
cargo fmt --all                      # Auto-format
RUST_LOG=debug cargo run             # Run with debug logging
cargo run --bin generate-openapi     # Regenerate resources/gen/openapi.json
```

A `justfile` is available for common tasks (`just --list` to see all). Key recipes: `just check` (fmt + clippy), `just test`, `just openapi`.

Requires **Rust 1.93+** (edition 2024) and **PostgreSQL 13+** (with `pg_trgm` and `ltree` extensions).

Database setup: `docker compose up -d postgres` — schema is applied automatically via sqlx migrations on app startup. Migration files live in `migrations/`. For local dev, set `DATABASE_URL` in `.env` (see `example.env`).

## Backend Pre-commit checks

Always run these before committing, in this order:

```bash
cargo fmt --all                                              # Auto-format
cargo clippy --all-features --all-targets -- -D warnings     # Lint (must pass with zero warnings)
```

CI enforces both — commits that fail either check will not merge.

## Backend Pre-push checks

When the change touches server code (anything under `src/`, `migrations/`,
`Cargo.toml`, or `tests/`), run the full suite locally before pushing — CI
is slower and a red CI run after a public push wastes maintainer attention:

```bash
just check               # cargo fmt --check + cargo clippy -D warnings
just test                # cargo test --workspace
just test-integration    # cargo test --tests with integration cfg
just api-test            # Hurl API + WebDAV scenarios
```

Run them in that order — `just check` is fastest and catches the most
common issues first. Don't push if any step fails; investigate locally.

## Backend Architecture

Hexagonal / Clean Architecture with four layers. Dependencies point inward only.

### Layer structure (`src/`)

- **`domain/`** — Core business entities (`entities/`) and repository trait definitions (`repositories/`). Pure Rust, no framework dependencies. Entity types: `File`, `Folder`, `User`, `Calendar`, `CalendarEvent`, `Contact`, `Share`, `TrashedItem`, `Session`, `DeviceCode`, `AppPassword`.

- **`application/`** — Use cases and orchestration.
  - `ports/` — Trait definitions (inbound/outbound) for storage, auth, caching, compression, dedup, thumbnails, chunked uploads, CalDAV/CardDAV, etc. This is the hexagonal "ports" layer.
  - `services/` — Use case implementations (`FileManagementService`, `FolderService`, `ShareService`, `TrashService`, `CalendarService`, `ContactService`, `SearchService`, `BatchOperations`, etc.).
  - `adapters/` — CalDAV/CardDAV protocol adapters (iCalendar/vCard parsing).
  - `dtos/` — Data transfer objects for API boundaries.

- **`infrastructure/`** — Concrete implementations of ports.
  - `repositories/pg/` — All PostgreSQL repository implementations (via `sqlx`). Uses `auth` schema for users/sessions, `storage` schema for files/folders/blobs (content-addressable dedup with ltree paths).
  - `services/` — JWT, password hashing (Argon2), OIDC, compression, thumbnails, chunked uploads, WOPI discovery, WebDAV locking, file content caching (moka).
  - `adapters/` — CalDAV/CardDAV storage adapters bridging domain traits to PG.
  - `db.rs` — Dual connection pool setup (user pool + maintenance pool).

- **`interfaces/`** — HTTP layer (Axum).
  - `api/handlers/` — REST API handlers for files, folders, auth, admin, search, shares, WebDAV, CalDAV, CardDAV, WOPI, chunked uploads, batch operations.
  - `api/routes.rs` — Route registration, splits protected vs public routes.
  - `nextcloud/` — NextCloud-compatible API (WebDAV, OCS, login flow v2, trashbin) with Basic Auth middleware.
  - `middleware/` — Auth (JWT validation), CSRF, rate limiting.
  - `web/` — Static file serving.

- **`common/`** — Cross-cutting concerns.
  - `di.rs` — `AppServiceFactory` builds all services and produces `AppState` (the central DI container passed to Axum). This is the composition root.
  - `config.rs` — `AppConfig::from_env()` loads all `OXICLOUD_*` env vars.

### Key patterns

- **DI via `AppState`**: All services are `Arc`-wrapped and assembled in `common/di.rs`. `AppState` is wrapped in `Arc` and passed as Axum state. Many services are `Option<Arc<T>>` because they depend on features being enabled (auth, WOPI, trash, etc.).

- **Content-addressable storage**: Files use BLAKE3 blob dedup. `storage.file_blobs` stores content; `storage.file_metadata` references blobs with ref-counting. See `file_blob_write_repository.rs` and `file_blob_read_repository.rs`.

- **ltree paths**: Folder hierarchy uses PostgreSQL `ltree` for efficient subtree queries (recursive copies, moves, searches).

- **Dual DB pools**: `DbPools` in `infrastructure/db.rs` separates user-facing queries from maintenance/background tasks to prevent starvation.

- **Feature flags**: Major features (auth, trash, search, sharing, quotas) are toggled via `OXICLOUD_ENABLE_*` env vars in `FeaturesConfig`.

- **UUID columns**: All ID columns use native PostgreSQL `UUID` type. SQL queries must use `::uuid` casts when passing string parameters to UUID columns.

### Database schemas

- `auth` schema: `users`, `sessions`, `app_passwords`, `device_codes`, `admin_settings`
- `storage` schema: `folders`, `file_metadata`, `file_blobs`, `trash`, `shares`, `favorites`, `recent_items`, `nextcloud_object_ids`
- `caldav` schema: `calendars`, `calendar_events`
- `carddav` schema: `address_books`, `contacts`, `contact_groups`, `contact_group_members`

Schema definition: `migrations/` (sqlx migrations, applied on startup)

### Protocol support

The server exposes multiple protocol interfaces simultaneously:
- REST API under `/api/`
- WebDAV at `/webdav/` (RFC 4918)
- CalDAV at `/caldav/`
- CardDAV at `/carddav/`
- NextCloud-compatible API at `/remote.php/`, `/ocs/`, `/status.php`
- WOPI at `/wopi/` (when enabled)
- Well-known discovery at `/.well-known/caldav` and `/.well-known/carddav`

### Test organization

Tests are primarily `#[cfg(test)]` modules within source files (~36 files have inline tests). Dedicated test files exist at `*_test.rs` alongside their source. The `test_utils` feature flag enables `mockall` mock generation for trait-heavy testing. No separate `tests/` directory.

### Code duplication

Never duplicate logic across handlers or services. If the same behaviour is needed in more than one place, extract it into a shared function, method, or service before writing the second callsite. Preferred homes by layer:
- Cross-handler request logic → method on `CoreServices` or `AppState` (`common/di.rs`)
- Reusable infrastructure behaviour → method on the relevant service struct
- Shared port behaviour → default method on the trait

### Authorization (AuthZ)

**AuthZ is enforced exclusively in the application service layer, never in handlers.** All permission checks go through `AuthorizationEngine` (port: `application/ports/authorization_ports.rs`) via service methods named with the `_with_perms` suffix. HTTP handlers (REST, WebDAV, NextCloud, CalDAV, CardDAV) authenticate the caller and pass `caller_id` into the service — they MUST NOT perform their own ownership/permission checks. The authentication middleware extracts the caller; the service decides if the action is allowed.

This rule prevents drift between layers and ensures every code path goes through the same policy. New service methods that touch a user-scoped resource must take `caller_id: Uuid` and call `authz.require(...)` before any read or mutation.

### Audit logging for denials and rejections

**Every permission denial or auth rejection MUST emit a structured audit log line before returning the error.** Without one, security-relevant outcomes are invisible to operators and incident response loses its primary signal.

The convention:

```rust
tracing::info!(
    target: "audit",
    event = "<domain>.<outcome>",     // e.g. "authz.denied", "auth.login_rejected",
                                       //      "magic_link.redemption_rejected",
                                       //      "user_profile.rejected"
    reason = "<short_key>",            // stable machine-readable key for filtering
                                       // (e.g. "bad_password", "expired", "no_visibility_path")
    // …structured fields naming the actors / targets…
    caller_id = %caller_id,            // or subject_id, user_id, granted_by, etc.
    target_id = %target_id,            // or resource_id, subject_id, etc.
    "👮🏻‍♂️ human-readable message: …",  // helpful for live tailing, do not parse
);
```

Rules:

- **`target: "audit"`** routes the line to the audit channel (separable from operational `oxicloud::*` debug noise).
- **`event`** uses the dotted form `<domain>.<verb_past_tense>` and stays stable — log aggregators key off it.
- **`reason`** is a machine-readable enum-style key. Don't reword across releases. New denial cause → new `reason` value, never repurpose an existing one.
- **Structured fields** carry every actor/target involved (`caller_id`, `target_id`, `resource_id`, `subject_id`, role, is_external flag, etc.). Request id and client IP come from the request-scope span automatically — don't duplicate them.
- **Anti-enumeration is preserved.** Returning `NotFound` to the caller while logging the real reason internally is the canonical pattern (e.g. `user_profile.rejected` with `reason = "external_caller_no_relationship"` returns 404, never 403). Operators see the truth; the attacker sees the same response shape regardless of whether the user exists.
- **Success paths stay quiet** by default — every authorized request would otherwise flood the log. Use `tracing::debug!` with `target: "oxicloud::authz"` (or similar) when a low-volume granted-trace helps debugging. Reserve `tracing::info!(target: "audit", …)` for outcomes worth surfacing in security reviews.

Canonical examples to mirror: `authz.denied` in `application/ports/authorization_ports.rs::require`, `auth.login_rejected` and `magic_link.redemption_rejected` and `user_profile.rejected` in `application/services/auth_application_service.rs`.

# Frontend part

The frontend is a **SvelteKit** single-page app (Svelte 5 + TypeScript, Vite,
`adapter-static`) under `frontend/`. Vite builds it to `static-dist/`, which the
Rust web layer serves in release (unmatched client routes fall back to the SPA
shell); `PROFILE=dev` serves the unbuilt source. The legacy vanilla frontend in
`static/` is retained for now but is **not** where new work goes.

## Frontend Build & Dev Commands

Run from `frontend/` (or via the `fe-*` justfile recipes from the repo root):

```bash
npm ci                 # install deps                     (just fe-install)
npm run dev            # Vite dev server + HMR             (just fe-dev) — backend must run on :8086
npm run build          # build the SPA → static-dist/      (just fe-build)
npm run check          # svelte-check + ESLint + Stylelint + Prettier (just fe-check)
npm run test:unit      # Vitest                            (just fe-test)
npm run format         # prettier --write .
```

`just dev` runs the backend and the Vite dev server together. CI uses **Node 26**; Node 24+ works locally.

## Frontend Architecture (`frontend/src/`)

- `routes/` — SvelteKit pages (`+page.svelte`, `+layout.svelte`), one folder per route (`files/[...path]`, `photos`, `shared`, `trash`, `admin`, `s/[token]`, …).
- `lib/components/` — reusable Svelte components (`AppShell`, `PhotoLightbox`, `ShareDialog`, `Modal`, …).
- `lib/api/` — HTTP layer: `client.ts` (`apiFetch`/`apiJson`), `csrf.ts` (`getCsrfHeaders`), `types.ts` (API DTO types — map the backend here), and `endpoints/*.ts` (one module per area: files, folders, photos, people, grants, …).
- `lib/stores/` — global reactive state as `*.svelte.ts` rune stores (`session`, `ui`, `theme`, `dialogs`).
- `lib/composables/` — reusable rune logic (`useSelection`, `useOwnerCache`).
- `lib/i18n/` — bespoke reactive i18n; `t(key, [params], fallback)` reads `frontend/static/locales/*.json` (16 locales) with `{{param}}` interpolation and an English fallback.
- `lib/icons/` — `Icon.svelte` + a generated Font Awesome `registry.ts`.
- `lib/utils/`, `lib/vendor/` — shared helpers and minimal typings/loaders for vendored libs.
- `lib/styles/` — global CSS (`app.css`, `base/`, `ported/`).
- `static/` — served at the web root: `locales/`, `vendors/` (maplibre-gl, pmtiles, hash-wasm), `workers/` (deltaWorker), optional `basemaps/`.

## Code conventions

### Svelte / TypeScript

- **Svelte 5 runes** — `$state`, `$derived`, `$props`, `$effect`, `$bindable`. No legacy `export let` for new components.
- **TypeScript everywhere** (`lang="ts"` in components). **No `any`** — `typescript-eslint` recommended is enforced; prefer precise types, `unknown` + narrowing, or a minimal declared interface for an untyped global (see `lib/vendor/maplibre.ts`).
- ES Modules; `camelCase` for variables/functions, `PascalCase` for components/classes; `const`/`let`, never `var`.
- API DTO shapes live in `lib/api/types.ts`; call the backend through `lib/api/endpoints/*` — don't bare-`fetch` `/api` from components.

### Code duplication

Never duplicate logic across modules/components. Extract shared behaviour:
- DOM/UI helpers → `lib/utils/`
- API wrappers → the relevant `lib/api/endpoints/*` module
- Cross-component state/logic → a `lib/stores/*.svelte.ts` store or a `lib/composables/*`
- Shared markup → a component (e.g. `PhotoLightbox` is shared by the photos grid, People and Places)

### CSS

- BEM methodology for class names (`.block__element--modifier`).
- Component styles live in the component's scoped `<style>`; cross-cutting tokens/styles in `lib/styles/`.
- **All colors must use `var(--*)`** — no raw hex, rgb, or named colors (Stylelint enforces `function-disallowed-list`); define tokens in `lib/styles/base/variables.css`.
- Mobile-first: media queries expand, they don't restrict.
- Dark mode keys off `<html data-color-scheme="dark">`.

## Frontend Pre-commit checks

Always run from `frontend/` before committing:

```bash
npm run check       # svelte-kit sync && svelte-check && eslint . && stylelint "src/**/*.{css,svelte}" && prettier --check .
npm run test:unit   # Vitest
```

CI runs the same `npm run check` (plus Vitest) — commits that fail will not merge.

# What agents must NOT do
- Edit `Cargo.lock` or `frontend/package-lock.json` by hand
- Introduce a different JS framework (React, Vue, etc.) — the frontend is SvelteKit/Svelte 5
- Add a heavy runtime npm dependency without discussion — prefer vendoring + lazy-loading under `frontend/static/vendors/` (see maplibre-gl / pmtiles)
- Use `any` in TypeScript
- Leave debug `console.log` statements in code
- Use raw color values in CSS — always use CSS custom properties
- Commit without passing all linters (`npm run check` for the frontend; `cargo fmt` + `cargo clippy` for the backend)
