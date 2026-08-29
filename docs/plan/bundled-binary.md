# Bundled Binary Distribution — Multi-Platform Plan

## Context

Users have asked for a way to run OxiCloud without Docker — a plain
binary. Today `release.yml` only creates a GitHub Release with notes;
no binary is attached. The Docker workflow (`docker-publish.yml`) ships
multi-arch images, but that's a separate audience.

The blocker for a "just download and run" experience is that the
`oxicloud` binary depends on the SvelteKit build output (`static-dist/`
under `<static_path>/static-dist/`, resolved by
`src/interfaces/web/mod.rs::resolve_static_path` at boot). Two files
to distribute per platform is friction; a single self-contained binary
is what users actually want.

The ask has two parts:

1. Ship **single-file binaries with frontend assets embedded**, for
   the common Linux targets and macOS.
2. Audit the current binary set — the crate produces 6+ binaries today,
   some of which are test-only. Strip anything that shouldn't ship to
   end users.

The intended outcome: a `v0.9.0` release attaches **4 musl-static
tarballs** (Linux amd64/arm64 + macOS Intel/Apple Silicon), each
~15-30 MB, containing a **single `oxicloud` binary** with assets +
operator tools + one-off migrations all baked in. User extracts the
tarball, sets `DATABASE_URL`, runs `./oxicloud` — server up.
Subcommands (`oxicloud opaque setup`, `oxicloud migrate
nfc-filenames --dry-run`) provide operator access to the same tools
currently split across `oxicloud-cli` and `migrate-nfc-filenames`.

Design shape (confirmed 2026-08-27):

- **musl-only Linux** — parity with the existing Docker image (Alpine
  base), no glibc-version fragmentation
- **Assets embedded via `rust-embed` with compile-time deflate
  compression** — smaller binary
- **`bundled-assets` is opt-in** — default `cargo build` unchanged;
  `just dev` still uses the filesystem `ServeDir` with Vite HMR
- **Single unified binary** — `oxicloud`, `oxicloud-cli`, and
  `migrate-nfc-filenames` collapse into one clap-driven executable
  with implicit-server default (backwards compat with existing Docker
  CMD / systemd units)

## Current binary inventory

From `Cargo.toml` + `src/bin/`:

| Binary | Path | Purpose | Ship to end users? |
|---|---|---|---|
| `oxicloud` | `src/main.rs` (implicit) | Server | **YES** |
| `oxicloud-cli` | `src/bin/oxicloud-cli.rs` | Operator toolbox (`opaque setup/reset`) | **MERGED** — absorbed into `oxicloud` per Deliverable 1b |
| `migrate-nfc-filenames` | `src/bin/migrate-nfc-filenames.rs` | One-off filename migration (historical, June 2026 fix) | **MERGED** — absorbed into `oxicloud migrate nfc-filenames` per Deliverable 1a→1b |
| `generate-openapi` | `src/bin/generate-openapi.rs` | Regenerate `resources/gen/openapi.json` | NO — dev tool, gate behind `dev_tools` feature |
| `opaque-hurl-helper` | `src/bin/opaque-hurl-helper.rs` | Hurl test companion (OPRF client) | NO — gate behind `test_utils` feature |
| `dpop-hurl-helper` | `src/bin/dpop-hurl-helper.rs` | Hurl test companion (ES256 DPoP proof) | NO — gate behind `test_utils` feature |
| `load-seed` | `src/bin/load-seed.rs` | Test fixture seeder | Already gated behind `load_seed_bin` feature ✅ |

After Deliverables 1 + 1a + 1b, `cargo build --release --bins`
produces exactly ONE binary: `oxicloud`. That single binary ships in
the tarball and in the Docker image.

## Deliverables

### 1. Squash test/dev binaries with `required-features`

Cargo respects `required-features` per `[[bin]]` — a binary is only
built when its listed features are active. This gates test helpers
out of `cargo build --release --bins` cleanly without needing custom
Cargo commands or shell trimming.

Edits to `Cargo.toml`:

```toml
[features]
# ... existing features ...
dev_tools = []             # NEW: gates ops tooling that shouldn't ship

[[bin]]
name = "opaque-hurl-helper"
path = "src/bin/opaque-hurl-helper.rs"
required-features = ["test_utils"]     # NEW gate

[[bin]]
name = "dpop-hurl-helper"
path = "src/bin/dpop-hurl-helper.rs"
required-features = ["test_utils"]     # NEW gate

[[bin]]
name = "generate-openapi"
path = "src/bin/generate-openapi.rs"
required-features = ["dev_tools"]      # NEW gate — `just openapi` flips it

# [[bin]] name = "migrate-nfc-filenames"    ← DELETED per Deliverable 1a
# [[bin]] name = "oxicloud-cli"             ← DELETED per Deliverable 1b
```

Existing invocations that need adjustment:

- `just openapi` recipe → add `--features dev_tools` to the underlying
  `cargo run --bin generate-openapi` call (currently `cargo run --bin
  generate-openapi` per justfile)
- `tests/api/run.sh` → add `--features test_utils` when building the
  two hurl helpers (shape confirmed: `cargo build [--release] --bin
  opaque-hurl-helper` / same for dpop in each helper's build-if-missing
  branch)

After these edits + Deliverables 1a + 1b: `cargo build --release --bins`
produces exactly ONE binary — `oxicloud`. Everything else falls out of
the default build set.

### 1a. Merge `migrate-nfc-filenames` into `oxicloud-cli`

The standalone `migrate-nfc-filenames` binary is a June-2026 one-off:
it cleans up NFD/NFC filename collisions in databases populated
before the write-time fix (`normalize_storage_name()` at
`src/domain/services/path_service.rs:36`, called from
`src/infrastructure/repositories/pg/file_blob_read_repository.rs:1062`).
New installs never need it; only pre-June 2026 databases do.

`oxicloud-cli`'s header docstring (`src/bin/oxicloud-cli.rs:20-23`)
already documents the growth pattern for absorbing tools like this:

> *"each new domain gets its own module below (e.g. `mod opaque`)
> with a `#[derive(Subcommand)]` enum for its actions and a
> `run(args) -> ExitCode` entrypoint. Keep each module self-contained
> so a future extraction is a file move."*

Note: this Deliverable is an intermediate step. Deliverable 1b then
absorbs `oxicloud-cli` itself into `oxicloud`, so the final CLI form
becomes `oxicloud migrate nfc-filenames --dry-run` — but 1a lands
first so the migration logic is proven inside the clap subcommand
tree before the main-binary merge.

Edits:

- **New `mod migrate` in `src/bin/oxicloud-cli.rs`** — moves the ~149
  non-boilerplate lines from `migrate-nfc-filenames.rs::main()` into
  a `run_nfc_filenames(dry_run: bool) -> ExitCode` function.
  `env::args()` parsing goes away; clap handles it.
- **Delete `src/bin/migrate-nfc-filenames.rs`**.
- **Delete the `[[bin]]` entry** in `Cargo.toml`.
- **Update `Dockerfile`** — 6 references to `migrate-nfc-filenames`
  (build commands at :46, :49, :89, `cp` steps at :130, :143, doc
  comment at :170, `COPY --chmod=755 --from=app` at :173).
- **Update `docs/plan/benchmake-and-performance-tracking.md`** — 2
  references to `migrate-nfc-filenames` at lines :44 and :161. Reword
  to reference `oxicloud-cli migrate nfc-filenames` (or, after 1b,
  `oxicloud migrate nfc-filenames`) and update the Cargo.toml
  placement example.
- **Any operator runbook** that documents `docker exec <container>
  migrate-nfc-filenames --dry-run` becomes `docker exec <container>
  oxicloud-cli migrate nfc-filenames --dry-run` (intermediate) then
  `docker exec <container> oxicloud migrate nfc-filenames --dry-run`
  after 1b.

Effort: ~1.5 hours mechanical. Extracts the "should the tarball ship
migrate-nfc-filenames?" question entirely — everything now ships as
one operator toolbox binary that also happens to include the
historical migration.

Future v1.0 removal path (deferred): delete `mod migrate` block + one
enum variant + docs. Much cleaner than removing a whole `.rs` file +
Cargo entry + Dockerfile refs.

### 1b. Merge `oxicloud-cli` into `oxicloud`

Single binary — server + operator tools + migrations — with an
**implicit-server** subcommand tree. `oxicloud` with no arguments
starts the server (backwards compat with existing Docker CMD /
systemd units / user configs). Subcommands add operator actions on
top.

After merge, the CLI shape is:

```
$ oxicloud --help
Usage: oxicloud [OPTIONS] [COMMAND]

Commands:
    opaque      OPAQUE aPAKE substrate management
    migrate     One-time data migrations

If no command is given, oxicloud starts the server (see docs/config).
```

Concrete forms:

- `oxicloud` — start server (unchanged)
- `oxicloud opaque setup` — was `oxicloud-cli opaque setup`
- `oxicloud opaque reset --user alice --dry-run` — was `oxicloud-cli
  opaque reset ...`
- `oxicloud migrate nfc-filenames --dry-run` — was
  `migrate-nfc-filenames --dry-run` (via Deliverable 1a)

**Backwards-compat guarantee**: `oxicloud` with no args continues to
start the server. Every existing `CMD ["oxicloud"]`, `ExecStart=/usr/local/bin/oxicloud`,
docker-compose entry, and k8s Deployment keeps working unchanged.
Users updating to v0.9.0 see no surprise.

**Migration impact**: the user-visible break is that `oxicloud-cli
opaque setup` (etc.) no longer exists as a separate binary. Given the
current audience for `oxicloud-cli` is very small (essentially only
the maintainer), the migration cost is trivial. Any user who had
scripted it can adapt with a one-line find/replace.

Edits:

- **`src/main.rs`** — top of `main()`, before the current server
  init, parse args via clap. If a subcommand is provided, dispatch
  to it and exit; otherwise fall through to the existing server-init
  path. Zero-arg startup cost stays ≤ microseconds (clap parse of
  empty args).
- **`src/cli/mod.rs`** — NEW module. Contains the `Domain` enum + the
  `opaque` and `migrate` submodules moved from
  `src/bin/oxicloud-cli.rs`. Each subcommand module keeps its
  self-contained shape per the growth pattern documented in the
  old `oxicloud-cli.rs` header.
- **Delete `src/bin/oxicloud-cli.rs`** entirely.
- **Delete the `[[bin]] name = "oxicloud-cli"` block** in `Cargo.toml`.
- **`Dockerfile`** — drop all 4 references to `oxicloud-cli` (build
  target lines + COPY steps). Simplified build command becomes
  `cargo build --release --bin oxicloud` — single-binary.
- **Docs** — all `docker exec <container> oxicloud-cli <domain>
  <action>` become `docker exec <container> oxicloud <domain>
  <action>`. Same shape, one fewer word.

Effort: ~2 hours mechanical. Comparable to Deliverable 1a but with
slightly more care at the `main.rs` entry point for the args-vs-server
branch.

**Tarball layout simplification** — the tarball now ships exactly
ONE binary:

```
oxicloud-0.9.0-<triple>/
├── oxicloud              (single file, server + tools + embedded assets)
├── example.env
├── LICENSE
└── README-install.md
```

That's the "just download and run" ethos in physical form: one file,
one command, done.

### 2. Add `bundled-assets` cargo feature

Purpose: at compile time, choose between filesystem-served static
assets (current behaviour — filesystem `ServeDir`) and
embedded-into-binary assets (via `rust-embed`). Feature is
**opt-in** — the default `cargo build --release` still produces a
filesystem-based binary, matching the current Docker image behaviour
(where assets are separate volume layers). Release tarballs are built
with `--features bundled-assets`.

**Dev mode is untouched.** `just dev` runs `PROFILE=dev cargo run` +
`npm run dev`, neither of which activates `bundled-assets`. The dev
workflow continues to:

- Serve from `frontend/` via Vite's dev server with HMR
- Backend reads static assets from `<static_path>/static-dist/` via the
  usual `ServeDir` (or falls back to `frontend/static/` when the
  build hasn't been run)
- No rebuild required to change locales, styles, or vendor JS

The `bundled-assets` code paths only compile when the feature is
explicitly enabled — under a `#[cfg(feature = "bundled-assets")]` gate.
The non-feature build's binary shape, ergonomics, and dev loop stay
identical to today.

Measured footprint (2026-08-27):

| Slice | Size | Notes |
|---|---|---|
| Total `static-dist/` uncompressed | **9.8 MB** | 499 files |
| `_app/` (SvelteKit bundle) | 3.3 MB | JS + CSS chunks |
| `vendors/` | 3.6 MB | maplibre-gl 1.0 MB, pdf.worker 1.0 MB, others |
| `locales/` | 2.2 MB | 16 locales, ru.json + hi.json largest at ~116-140 KB |
| `logo/`, `geo/`, `basemaps/`, `workers/`, misc | ~600 KB | |
| **`.tar.gz` compressed** | **4.65 MB** | realistic embed cost after brotli/gzip inside binary |
| **`.tar.xz` compressed** | **4.22 MB** | not what rust-embed uses; reference only |

Expected release-binary size with embed: `oxicloud` today ships in
the 30-60 MB range (stripped, LTO). Add ~5-10 MB for embedded
static-dist. Tarball compression on top → ~20-30 MB shipped per
platform. Four platforms × ~25 MB = ~100 MB per release. Well within
GitHub Releases limits.

Cargo.toml additions:

```toml
[features]
bundled-assets = ["dep:rust-embed", "dep:mime_guess"]

[dependencies]
rust-embed = { version = "8", features = ["compression"], optional = true }
mime_guess = { version = "2", optional = true }
```

Runtime shape — a new module `src/interfaces/web/embedded.rs`:

```rust
#[cfg(feature = "bundled-assets")]
#[derive(rust_embed::RustEmbed)]
#[folder = "static-dist/"]   // ← repo-root, matches SvelteKit adapter-static output
#[include = "*"]
#[exclude = "*.br"]      // Vite's precompressed sibling — response compression handles on wire
#[exclude = "*.gz"]      // ditto
pub struct EmbeddedAssets;
```

The `#[folder]` path is relative to Cargo.toml (repo root), where the
SvelteKit adapter-static config in `frontend/svelte.config.js` emits:

```js
adapter: adapter({
    pages: '../static-dist',
    assets: '../static-dist',
    ...
})
```

The current filesystem shape (at `src/interfaces/web/mod.rs:47-106`)
is more than one `ServeDir` — the embed swap replaces FOUR sites, all
downstream of `resolve_static_path()`:

1. **`spa` ServeDir** (`mod.rs:60-63`) — root fallback with
   `precompressed_br().precompressed_gzip()` and SPA-shell fallback
   pointing at `<static>/index.html`. Under embed: an axum handler
   that resolves the request path against `EmbeddedAssets::get()`,
   200 with correct MIME (via `mime_guess`) if hit, otherwise return
   the embedded `index.html` bytes with `text/html` for SPA client-routing.
2. **`app_immutable` ServeDir** (`mod.rs:66-77`) — nested at
   `/_app/immutable` with `Cache-Control: public, max-age=31536000,
   immutable`. Under embed: same handler shape as (1), scoped to
   the `_app/immutable/` prefix, plus a `.layer()` that stamps the
   immutable cache header.
3. **`ServeFile::new(index.html)`** SPA fallback (`mod.rs:63`) —
   folds into (1)'s not-found path.
4. **CSP inline-script scan** (`mod.rs:163-233`) — currently reads
   every `.html` file in the resolved static dir via
   `std::fs::read_dir` + `std::fs::read_to_string` at boot to compute
   SHA-256 CSP source expressions for every inline `<script>`. Under
   embed: iterate `EmbeddedAssets::iter()` filtered to `.html`
   extensions, pull bytes via `::get()`, hash the same way. Same
   arithmetic, different source. Boot-time only.

All four flow through `resolve_static_path()` at `src/interfaces/web/mod.rs:25-35`
— that helper is the natural pivot. Add a returned enum:

```rust
#[cfg(feature = "bundled-assets")]
pub enum StaticSource {
    Filesystem(PathBuf),       // OXICLOUD_STATIC_PATH points at a real dir
    Embedded,                  // fall through to compiled-in bytes
}
```

Then the four callsites (`create_web_routes` + CSP scan) match on
`StaticSource` and pick their implementation. Under the default
feature set (no `bundled-assets`), the enum degrades to a bare
`PathBuf` — zero runtime cost, no cfg pollution across the wider
codebase.

**Locale loading — also needs embed treatment.** Two callsites read
locales at runtime:

- `src/main.rs:599-615` — resolves `<static_path>/locales/` at boot
  and passes it to `LocaleRegistry::discover()` at
  `src/common/locale.rs:150-221`, which does `fs::read_dir` +
  `fs::read_to_string` + `serde_json::from_str` on each of 16 files.
  Currently fail-fast panics if the directory is missing.
- `src/infrastructure/services/file_system_i18n_service.rs` — the
  runtime translator, `translations_dir: PathBuf` field, does
  `tokio::fs::read_to_string` on `<dir>/<code>.json` per lazy-load
  miss (cached in `RwLock<HashMap<Locale, Value>>`).

Under `bundled-assets`, both get an alternative implementation that
reads from `EmbeddedAssets` (locale files are at
`static-dist/locales/*.json`, picked up by the same folder embed).
Recommended shape: constructor pair —
`LocaleRegistry::discover_filesystem(path)` and
`#[cfg(feature = "bundled-assets")] LocaleRegistry::discover_embedded()`.
`main.rs` picks based on the resolved `StaticSource`. Simpler than a
trait-based indirection for two static sources with the same interface.

Frontend at runtime ALSO fetches `/locales/*.json` for client-side
i18n — this path is served by the same static router in (1) above,
so no separate work; the embed already covers it.

Precedence rule: even in a bundled build, honour `OXICLOUD_STATIC_PATH`
when it points at an existing directory. Lets ops override embedded
assets for locale patches / theming without a full rebuild. The
`resolve_static_path` return value is checked at boot; a real directory
wins over embedded fallback. If the resolved directory does NOT exist,
fall through to the embedded handler cleanly (log at info level:
"OXICLOUD_STATIC_PATH points at <path> which doesn't exist; serving
embedded assets").

Build-time invariant: `cargo build --features bundled-assets` requires
`static-dist/` to exist AND be non-empty. Add a `build.rs` check that
emits a clear error if missing, pointing at `just fe-build` /
`(cd frontend && npm run build)`.

**Precompression + embed strategy**: minimize binary size by storing
assets compressed inside the binary, and use axum's response
compression on the wire.

`rust-embed`'s `compression` feature deflate-compresses each embedded
file at compile time. Files are decompressed lazily on first access
and cached in a per-file `OnceCell` for the remainder of the process.
Warms up quickly under real traffic — the first user's page load
touches ~30 files, all cached from then on.

On the wire, response compression is handled by axum's
`CompressionLayer` (tower-http) applied to the static router
subtree. Browsers get `Content-Encoding: br` when they Accept-Encoding
brotli; gzip fallback; identity for clients that ask for neither.

Projected embed size after excludes + rust-embed deflate compression:
**~4-5 MB**. Matches the `.tar.xz` reference size and roughly halves
what raw-embed-plus-siblings would cost. Runtime CPU: negligible under
any real load; the compressed variants would benefit from a
reverse-proxy cache in front for CPU-tight hosts (Pi 4/5).

Consequence for the `nginx`/reverse-proxy story users will run in
front: the binary responds correctly to `Accept-Encoding: br, gzip`
without configuration. Users terminating TLS at their proxy get
compressed responses either way (proxy passes through or re-compresses
its cache).

### 3. Target matrix — musl-only Linux

Four triples cover the practical need:

| Triple | Runner + toolchain | Notes |
|---|---|---|
| `x86_64-unknown-linux-musl` | `ubuntu-22.04` running `rust:1.96-alpine3.24` container | Static, no glibc dep, runs on ANY Linux distro from Alpine to CentOS 7 to Debian 10 to Ubuntu 25.04. Parity with existing Docker image. |
| `aarch64-unknown-linux-musl` | `ubuntu-22.04-arm` running `rust:1.96-alpine3.24` container | Native ARM64 runner (no QEMU), same container as amd64 for byte-for-byte parity. Pi 4/5, ARM servers, Graviton. |
| `aarch64-apple-darwin` | `macos-latest` | Apple Silicon, native |
| `x86_64-apple-darwin` | `macos-13` | last Intel-runner tier |

**Rationale for musl-only Linux**:

1. **Parity with Docker.** The Docker image is already Alpine/musl —
   users get identical runtime behaviour whether they pull the
   container or the tarball. One build shape, one test surface.
2. **Face-indexing regression is a NON-issue.** `faces-onnx` requires
   glibc-only `libonnxruntime.so`; it's already unavailable on the
   Docker image. Users who want face indexing build from source with
   `--features faces-onnx` on a glibc host — same as today, no
   change from musl-only tarballs.
3. **Zero glibc-version fragmentation.** No `GLIBC_2.35 not found`
   errors on older distros. One binary works everywhere.
4. **Simpler install docs.** "Download this file, run it" without
   a "which glibc do you have?" branch.
5. **Marginal perf hit is invisible under I/O-bound OxiCloud workloads.**
   Musl's `malloc` and DNS resolver quirks matter for allocation-heavy
   / DNS-heavy servers; OxiCloud is neither.

**External runtime dependencies** — complete list. Codebase audit
2026-08-27 confirmed `ffmpeg` is the ONLY `Command::new` invocation
in `src/`; no other subprocess deps exist.

| Category | Dep | Required? | Notes |
|---|---|---|---|
| Subprocess | `ffmpeg` | Optional | Video thumbnails. Kill switch: `OXICLOUD_ENABLE_VIDEO_THUMBNAILS=false`. Path override: `OXICLOUD_FFMPEG_PATH` |
| System lib | `ca-certificates` | Required | Outbound HTTPS (OIDC, S3, webhooks). Pre-installed on nearly every distro |
| System lib | `tzdata` | Required | Timezone DB for chrono. Pre-installed on nearly every distro |
| External service | PostgreSQL 13+ | Required | With `pg_trgm` + `ltree` extensions. TCP/loopback only — no libpq client lib needed |
| Runtime dylib | `libonnxruntime.so` + ONNX models | N/A for tarball | Face indexing (glibc-only, requires build from source with `--features faces-onnx`). Not shipped in musl tarballs — Docker/tarball users don't have this feature |

**Explicit non-deps** (worth documenting to preempt questions):

- **No libpq** — sqlx uses pure-Rust tokio-postgres
- **No git** — only build-time metadata via `build.rs`, never runtime
- **No ImageMagick / libvips** — image thumbnails via pure-Rust `image` crate
- **No pandoc / rst2html / etc.** — no document conversion
- **No systemd/launchd** — daemon lifecycle user-managed
- **No sendmail / SMTP CLI** — email via pure-Rust SMTP client

**Per-distro install command** (for `README-install.md`):

| Distro | Command |
|---|---|
| Alpine | `apk add ca-certificates tzdata ffmpeg` |
| Debian / Ubuntu | `apt install ca-certificates tzdata ffmpeg` |
| Fedora / RHEL | `dnf install ca-certificates tzdata ffmpeg` (RPMFusion for full codec set) |
| Arch | `pacman -S ca-certificates tzdata ffmpeg` |
| macOS | `brew install ffmpeg` (ca-certificates + tzdata built in) |
| Portable Linux | Static ffmpeg from https://github.com/BtbN/FFmpeg-Builds/releases + `OXICLOUD_FFMPEG_PATH=<path>` |

Postgres install is documented separately (project docs) since it's a
per-distro-per-version story with per-extension setup.

**Windows deliberately deferred** — sqlx feature set, some C deps,
testing story on Windows are all extra work.

**Pi 2 / 32-bit ARM (`armv7-unknown-linux-gnueabihf`) excluded** —
1 GB RAM is below OxiCloud's practical floor even with face indexing
disabled.

**Building strategy for Linux musl targets** — run the compilation
inside the `rust:1.96-alpine3.24` container image the Dockerfile
already uses. Guarantees byte-for-byte parity with what ends up in
the published Docker image; zero new toolchain to maintain. Runner
just needs Docker (all GitHub-hosted Linux runners have it). No
`rustup target add`, no `apt install musl-tools`.

**CPU baseline** — the repo sets `-C target-cpu=native` for x86_64 and
aarch64 hosts (`.cargo/config.toml:11-12`). That flag makes the binary
use every CPU feature the BUILDER exposes — great for local dev,
catastrophic for distributed binaries: a runner with AVX-512 produces
a binary that segfaults on any older CPU. Precedent for the fix at
`.github/workflows/load-smoke.yml:28`, which already overrides with
`RUSTFLAGS="-C target-cpu=x86-64-v3"` for load tests.

Per-target baseline for `release-binaries.yml`:

| Triple | `RUSTFLAGS` |
|---|---|
| `x86_64-unknown-linux-musl` | `-C target-cpu=x86-64-v2` |
| `aarch64-unknown-linux-musl` | `-C target-cpu=generic` (safe ARMv8-A baseline) |
| `aarch64-apple-darwin` | `-C target-cpu=apple-m1` |
| `x86_64-apple-darwin` | `-C target-cpu=x86-64-v2` |

`x86-64-v2` covers ~2010+ processors (Nehalem, Bulldozer). Widest
realistic install base for a "runs everywhere" tarball. Notably
different from Docker's `x86-64-v3` (per `load-smoke.yml:28`) — Docker
targets performance-tuned deployments, tarballs target maximum
compatibility.

Trade-off left on the table: BLAKE3 SIMD + image codecs run somewhat
slower on v2 than v3. For a self-hosted personal cloud workload this
is invisible; for anyone who wants max perf, the Docker image is
still their better option.

### 4. Tarball layout

One archive per platform. **Four files inside**, all rooted under a
per-version-per-triple directory so extraction lands cleanly:

```
oxicloud-0.9.0-<triple>/
├── oxicloud              ← the single binary (server + tools + embedded assets)
├── example.env           ← copied verbatim from repo root (50 KB, all env vars documented)
├── LICENSE               ← copied verbatim from repo root
└── README-install.md     ← NEW, ~100 lines, tarball-audience-specific
```

Deliberate exclusions:

- **`README.md`** (repo root, 10 KB) — the GitHub landing page: features,
  screenshots, tech stack, contribution guide. Wrong orientation for a
  downloaded tarball. Users get `README-install.md` instead — shorter,
  focused on "how do I run this thing on this box?"
- **`oxicloud.service` systemd unit** — inlined as a copy-paste block in
  `README-install.md`. Users have to customize `User=` /
  `WorkingDirectory=` anyway; a documented example beats a shipped file
  that pretends to be canonical.
- **`CHANGELOG.md`** — the GitHub Release page carries the notes.
  Duplicating invites drift.
- **`docs/`** — full documentation stays on GitHub, linked from
  `README-install.md`.

`README-install.md` content shape (~100 lines):

- **Quickstart** — required env vars, one-command run
- **PostgreSQL setup** — link to project docs; note `pg_trgm` + `ltree`
  extensions
- **Optional: video thumbnails** — mention ffmpeg + the
  `OXICLOUD_ENABLE_VIDEO_THUMBNAILS=false` kill switch (first
  user-facing surface for this env var, closing the discoverability
  gap flagged in memory `bug_env_docs_video_thumbnails_missing`)
- **Systemd unit example** — inline copy-paste block, references
  `/etc/oxicloud/oxicloud.env` for env vars
- **First-run** — direct to `/setup` for admin account creation
- **Verification** — `sha256sum -c ../SHA256SUMS` for tarball integrity
- **Upgrading** — replace binary in place, restart service; migrations
  run automatically on boot per `sqlx::migrate!()`
- **Support links** — GitHub Issues, docs site
- **Docker note** — for users who want the container path instead

Tarball name: `oxicloud-<version>-<triple>.tar.gz`.

macOS tarballs stay `.tar.gz` too (not `.zip`) — Homebrew formulas
handle either, and it keeps the CI packaging step uniform. Same
extraction UX cross-platform (`tar xzf`).

`SHA256SUMS` file lists all archives with hashes at the release-level
(next to the tarballs, not inside them) — standard OSS practice.
Users verify via `sha256sum -c SHA256SUMS` before extraction.

### 5. New workflow: `.github/workflows/release-binaries.yml`

Three-stage pipeline, shared frontend build:

```
1. frontend-build       (ubuntu-latest, single job)
   - checkout
   - Node 26 setup
   - npm ci && npm run build     (writes static-dist/ at repo root)
   - upload static-dist/ as artifact "static-dist"

2. binary-build         (matrix over 4 targets, needs: frontend-build)
   - checkout
   - download static-dist artifact into repo-root static-dist/
   - Linux targets: docker run rust:1.96-alpine3.24, cargo build inside
   - macOS targets: rustup target add + native cargo build
   - cargo build --release --features bundled-assets --bin oxicloud
   - tar czf oxicloud-<version>-<triple>.tar.gz oxicloud-<version>-<triple>/
   - upload tarball as per-platform artifact

3. release              (ubuntu-latest, needs: binary-build)
   - download all tarball artifacts
   - compute SHA256SUMS
   - softprops/action-gh-release@v2 with files: dist/*
```

Triggers: `push: tags: v*` (real releases) + `workflow_dispatch` with
`dry_run: true` toggle (build tarballs, upload as workflow artifacts,
skip attaching to a release).

Interaction with existing `release.yml`: **new file**, because the
current `release.yml` is tiny (create release + notes) and mixing
concerns would clutter it. `release.yml` stays as "make the GitHub
Release exist"; `release-binaries.yml` stacks binaries into it. Both
trigger on `push: tags: v*`.

**Parallel-fire behaviour on tag push** — on `git push origin v0.9.0`,
three workflows fire simultaneously:

```
tag push v0.9.0
     │
     ├─── release.yml            (~1 min)      Release + notes
     ├─── docker-publish.yml     (~30-45 min)  multi-arch Docker → GHCR + DockerHub
     └─── release-binaries.yml   (~25-30 min)  4 tarballs → attach to Release
```

Total wall-clock: ~30-45 min (dominated by whichever build is slower).
No sequencing between the three — each has a single responsibility
and runs independently.

Race with `release.yml` is **benign** because `release-binaries.yml`
uses `softprops/action-gh-release@v2`, which:
- **Adds files** to an existing Release if one exists for the tag.
- **Creates** the Release (with default settings, no notes) if
  `release.yml` hasn't finished yet.

Worst case: `release-binaries.yml` finishes first on a tiny tag, creates
a bare Release, `release.yml` catches up and fills in the notes. Users
see the Release progressively; nothing breaks. If this becomes annoying
in practice (unlikely — `release.yml` is ~1 min), flip
`release-binaries.yml` to `on: workflow_run: { workflows: ["Release"],
types: [completed] }` to serialize.

Concurrency: same `${{ github.workflow }}-${{ github.ref }}` group as
`docker-publish.yml`, but `cancel-in-progress: false` — every tag is
unique and immutable, so a superseded release build has nothing to
cancel.

Publish gate: same fork-friendly pattern as `docker-publish.yml` —
`if: github.repository == 'AtalayaLabs/OxiCloud' ||
vars.ENABLE_BINARY_RELEASE == 'true'`. Prevents forks from
auto-attaching binaries to their own tag pushes.

### 6. Docs

- **`docs/install/binary.md`** — quickstart per platform, verify
  SHA256SUMS, minimum env vars (`DATABASE_URL`), systemd unit
  example, Pi-specific advice (link to the "verified on" hardware
  table). Prose only — no code snippets that could go stale.
  Include a "server-side video thumbnails" callout naming the
  `OXICLOUD_ENABLE_VIDEO_THUMBNAILS=false` kill switch — this is
  the first user-facing surface where the env var is discoverable
  (per memory `bug_env_docs_video_thumbnails_missing`, it's not
  in `example.env` nor `docs/env.md` today). Consider fixing the
  underlying gap in `example.env` + `docs/env.md` as a companion
  edit to this PR — small win, high visibility.
- **`README.md`** — add a one-line pointer under Installation:
  "Binary releases attached to each GitHub Release — see
  [docs/install/binary.md]". Do NOT list per-triple download links
  by hand; they'd rot.
- **This file** — the design record. Kept alongside other
  `docs/plan/*.md` docs so the next maintainer sees the rationale
  before touching `release-binaries.yml` or the embed layer.

### 7. `Cargo.toml` `[package.metadata.binstall]` block

Free win: `cargo binstall oxicloud` starts working once the tarballs
land on GitHub Releases with predictable names. Two-line metadata
block declares the URL template:

```toml
[package.metadata.binstall]
pkg-url = "{ repo }/releases/download/v{ version }/oxicloud-{ version }-{ target }.tar.gz"
bin-dir = "oxicloud-{ version }-{ target }/{ bin }{ binary-ext }"
```

No CI change; the tarballs already follow this shape from Deliverable 4.

## Critical files

- `Cargo.toml` — add `bundled-assets` + `dev_tools` features,
  `required-features` on gated bins, `rust-embed` optional dep,
  `[package.metadata.binstall]` block. Delete the
  `[[bin]] name = "oxicloud-cli"` and `[[bin]] name = "migrate-nfc-filenames"`
  blocks (Deliverables 1a + 1b).
- `src/main.rs` — add clap parsing at the top of `main()`. If a
  subcommand is present → dispatch via new `src/cli/` module; otherwise
  fall through to the existing server-init path (backwards-compat
  implicit-server mode).
- `src/cli/mod.rs` — NEW. Root of the operator-tools tree; contains
  `Domain` enum + submodules moved from `src/bin/oxicloud-cli.rs`.
- `src/cli/opaque.rs` — NEW. `opaque setup` + `opaque reset` moved
  from the old `oxicloud-cli.rs`.
- `src/cli/migrate.rs` — NEW. `migrate nfc-filenames` — the ~149
  non-boilerplate lines from the old `migrate-nfc-filenames.rs`,
  wrapped as a clap subcommand.
- `src/bin/oxicloud-cli.rs` — DELETE (contents absorbed into `src/cli/`).
- `src/bin/migrate-nfc-filenames.rs` — DELETE (contents absorbed
  into `src/cli/migrate.rs`).
- `src/interfaces/web/mod.rs` — 400-line file, owns the static-serving
  surface. Four sites gain a `#[cfg(feature = "bundled-assets")]`
  alternative:
  - `resolve_static_path()` (`:25-35`) — returns a `StaticSource`
    enum under bundled mode; a bare `PathBuf` otherwise
  - `create_web_routes()` (`:47-106`) — swap the two `ServeDir`
    constructions for embedded-asset handlers
  - `content_security_policy()` + `inline_script_csp_hashes()`
    (`:163-233`) — iterate `EmbeddedAssets::iter()` instead of
    `fs::read_dir`
  - Import block + type imports for the new source enum
- `src/interfaces/web/embedded.rs` — NEW: `#[derive(RustEmbed)]` struct
  + two axum handlers (root/SPA-fallback + `_app/immutable`-prefixed
  with cache header) + shared MIME helper. ~100 lines.
- `src/main.rs:599-615` — locale-source resolution. Under bundled
  mode, call `LocaleRegistry::discover_embedded()` instead of the
  filesystem variant when `resolve_static_path()` returns
  `StaticSource::Embedded`.
- `src/common/locale.rs:150-221` — add `LocaleRegistry::discover_embedded()`
  under `#[cfg(feature = "bundled-assets")]`. Same parse + registry
  build, source is `EmbeddedAssets::iter()` filtered to `locales/*.json`.
- `src/infrastructure/services/file_system_i18n_service.rs` — either
  extend to accept an `EmbeddedLocales` source alongside the
  filesystem one, OR ship a second `EmbeddedI18nService` impl of the
  same trait. Latter avoids polluting the fast filesystem path with
  cfg gates.
- `build.rs` — EXISTS today (injects `GIT_HASH`/`GIT_BRANCH` from git).
  Extend with a second block: when the `bundled-assets` feature is
  enabled (`env::var("CARGO_FEATURE_BUNDLED_ASSETS").is_ok()`),
  check that repo-root `static-dist/` exists and contains at least
  `index.html`. Emit a clear compile error pointing at
  `just fe-build` / `(cd frontend && npm run build)` if missing.
  Also emit `cargo:rerun-if-changed=static-dist/` so a rebuild of
  the frontend re-triggers rust-embed's compile-time embed step.
- `.cargo/config.toml` — NO CHANGES. The dev-preserving default of
  `-C target-cpu=native` stays. Release CI overrides via per-job
  `RUSTFLAGS` env var, per the load-smoke.yml precedent.
- `.github/workflows/release-binaries.yml` — NEW: three-stage pipeline.
- `justfile` — thread `--features dev_tools` into the `openapi` recipe.
- `tests/api/run.sh` — thread `--features test_utils` into the two
  hurl-helper build lines.
- `docs/install/binary.md` — NEW: user-facing installation guide.

## Verification

1. **Local squash check**: after Cargo.toml + `src/cli/` edits, run
   `cargo build --release --bins` and confirm exactly ONE binary
   appears in `target/release/` (`oxicloud`). Run `cargo build --release
   --bins --features test_utils` and confirm the two hurl helpers
   appear. `cargo build --release --bins --features dev_tools`
   should surface `generate-openapi`. Confirm subcommand shape via:
   - `target/release/oxicloud --help` — shows `opaque` + `migrate`
     domains
   - `target/release/oxicloud opaque setup` — prints a fresh
     ServerSetup base64 line (unchanged behaviour vs the old
     `oxicloud-cli opaque setup`)
   - `target/release/oxicloud migrate nfc-filenames --dry-run`
     (against a sandbox DB) — same behaviour as the old
     `migrate-nfc-filenames --dry-run`
   - `target/release/oxicloud` (no args) — starts the server exactly
     as today, no clap-related output surprises before the server
     init banner.

2. **Local bundled-assets smoke**:
   ```
   (cd frontend && npm ci && npm run build)     # writes ../static-dist/
   cargo build --release --features bundled-assets --bin oxicloud
   # Wipe static-dist/ or point OXICLOUD_STATIC_PATH somewhere
   # nonexistent to force the embedded path to be exercised.
   mv static-dist/ static-dist.hidden/
   OXICLOUD_STATIC_PATH=/tmp/nonexistent DATABASE_URL=... target/release/oxicloud
   # Hit http://localhost:8086 — SPA shell + locales must load.
   # Restore afterward: mv static-dist.hidden/ static-dist/
   ```

3. **Filesystem fallback still works in bundled build**: with the
   same binary, point `OXICLOUD_STATIC_PATH` at a real static-dist,
   confirm files served from disk (change a file, no rebuild → change
   visible in browser). Verifies the precedence rule from Deliverable 2.

4. **Non-bundled build still works**: `cargo build --release`
   (without `--features bundled-assets`) → binary boots + serves from
   `./static/static-dist/` as today. Zero regression on the Docker
   image path.

5. **CI dry-run**: dispatch `release-binaries.yml` with `dry_run: true`
   from a fork. Confirms all four matrix entries build successfully,
   tarballs land in the run's artifact list, no release is created.

6. **Manual extraction test**: download one tarball, extract, run
   `./oxicloud` with just `DATABASE_URL` set (against a local
   Postgres). Log in, upload a file, check that locale switching
   works, confirm `/api/status` returns healthy. Then repeat on a Pi 5
   for the `aarch64-unknown-linux-musl` variant if convenient.

## Not in scope

- **Windows target** — separate work when demand appears.
- **glibc Linux tarballs** — musl covers the Linux audience per the
  design shape above; users wanting glibc-specific features
  (`faces-onnx`) build from source.
- **32-bit ARM (`armv7`)** — hardware below the workload floor.
- **Debian/RPM packages** — post-tarball layer, adds repo-hosting burden.
- **Homebrew tap** — trivial once tarballs exist; separate decision.
- **Signing (Sigstore/GPG)** — worth adding but scope-creeping;
  SHA256SUMS is the minimum table stakes for this PR.

## Delivery order

1. **Feature-flag squash** (Deliverable 1). Cargo.toml edits +
   `just openapi` / `tests/api/run.sh` invocation fixes. Verify
   `cargo build --release --bins` no longer builds hurl helpers.
2. **Merge migrate-nfc-filenames into oxicloud-cli** (Deliverable 1a).
   Move logic to `mod migrate` submodule. Delete standalone bin.
   Verify `oxicloud-cli migrate nfc-filenames --dry-run` works.
3. **Merge oxicloud-cli into oxicloud** (Deliverable 1b). Move
   `src/bin/oxicloud-cli.rs` contents into new `src/cli/` module,
   wire clap into `main.rs` with implicit-server default. Delete
   `src/bin/oxicloud-cli.rs`. Verify `oxicloud` (no args) still
   starts the server; `oxicloud opaque setup` + `oxicloud migrate
   nfc-filenames --dry-run` work.
4. **Add `bundled-assets` feature** (Deliverable 2). `rust-embed` +
   `build.rs` guard + `src/interfaces/web/embedded.rs` + locale
   loader alt + CSP scan alt. Verify locally with the smoke sequence
   in Verification §2.
5. **Add `.github/workflows/release-binaries.yml`** (Deliverable 5).
   Dry-run on a fork. Iterate until all 4 targets green.
6. **Write docs** (Deliverable 6) — `docs/install/binary.md`. Prose
   only, no snippets that will rot. Include the
   `OXICLOUD_ENABLE_VIDEO_THUMBNAILS=false` callout for tarball users
   without ffmpeg.
7. **Add `[package.metadata.binstall]` block** (Deliverable 7).
   One-line change enabling `cargo binstall oxicloud`.
8. **Fix the `OXICLOUD_ENABLE_VIDEO_THUMBNAILS` doc gap** — add to
   `example.env` + `docs/env.md` per memory
   `bug_env_docs_video_thumbnails_missing`. Small companion edit
   surfaced by the binary-install docs work.
9. **Cut a test tag** (`v0.9.0-rc1`?) on a fork with
   `vars.ENABLE_BINARY_RELEASE=true`. Confirm tarballs attach to the
   Release, SHA256SUMS present, `cargo binstall oxicloud` works.
10. **When happy, cut on canonical.**

Total scope: ~2.5 days of careful work.
- Deliverables 1 + 1a + 1b: ~5 hours mechanical (Cargo config, CLI
  merge, subcommand tree)
- Deliverable 2: ~1 day — the only piece with real design surface
  (embed swap, four cfg sites, locale + CSP loaders)
- Deliverable 5: ~4 hours workflow authoring + iteration
- Deliverables 6-8: ~4 hours docs + small edits
- Verification + iteration: ~4 hours
