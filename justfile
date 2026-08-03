default:
    @just --list

build:
    cargo build

release:
    # Build the SvelteKit SPA into static-dist/ (build.rs no longer bundles),
    # then compile the release binary which serves it.
    cd frontend && npm ci && npm run build
    cargo build --release

run:
    cargo run

# ── Docker image builds ──────────────────────────────────────────────────────
# Build the runtime image locally with BuildKit cargo cache mounts so repeat
# builds recompile only the crates that changed (true incremental). Routes the
# runtime stage to the `builder-cache` stage. A plain `docker build .` defaults
# to the slower `builder` stage that CI relies on for GitHub Actions layer
# caching — locally that recompiles the whole app crate on every source change,
# so prefer this recipe for iterating on the image. The cargo registry + target
# cache mounts persist in the local BuildKit cache across runs.
docker-build tag="oxicloud:dev":
    DOCKER_BUILDKIT=1 docker build \
        --build-arg BUILDER=builder-cache \
        --build-arg BIN_DIR=/app/bin \
        --tag {{tag}} \
        .

# Same incremental image but keeps the `data-testid` hooks (VITE_E2E=1) so the
# resulting image can back the Playwright e2e flow. Mirrors the build args the
# Testcontainers fixture passes (tests/e2e/fixtures/oxicloud-stack.ts).
docker-build-e2e tag="oxicloud-e2e:latest":
    DOCKER_BUILDKIT=1 docker build \
        --build-arg BUILDER=builder-cache \
        --build-arg BIN_DIR=/app/bin \
        --build-arg VITE_E2E=1 \
        --tag {{tag}} \
        .

run-debug:
    RUST_LOG=debug cargo run

test:
    cargo test --workspace

test-mocks:
    cargo test --features test_utils

# Source-based line coverage (HTML + summary) via cargo-llvm-cov.
# Requires `cargo install cargo-llvm-cov` and llvm-tools (rustup component
# llvm-tools-preview, or set LLVM_COV / LLVM_PROFDATA to an llvm-binutils).
# Unit tests only (no DB). Pass a filter: `just coverage external_mount`.
coverage filter='':
    cargo llvm-cov --lib --html {{ if filter == '' { '' } else { '--' } }} {{filter}}
    cargo llvm-cov --lib --summary-only {{ if filter == '' { '' } else { '--' } }} {{filter}}

# Coverage including the testcontainers integration tests (needs docker).
coverage-integration filter='mount':
    RUSTFLAGS='--cfg integration_tests' cargo llvm-cov --lib --summary-only -- {{filter}}

# DB-dependent integration tests gated on `--cfg integration_tests`.
# Spins up the test postgres on port 5433 first. Requires one row in
# auth.users on the test DB (start the server against it once to seed).
#
# IMPORTANT: DATABASE_URL is pinned to the test container on port 5433
# so a stray DATABASE_URL in `.env` (which `set dotenv-load` at the top
# of this file would otherwise leak in) cannot point the tests at the
# real dev DB. The test pool helpers also refuse non-`oxicloud_test`
# URLs as defence in depth.
test-integration filter='':
    bash tests/common/spawn-db.sh
    PGHOST=localhost PGPORT=5433 PGUSER=oxicloud_test PGPASSWORD=oxicloud_test \
      PGDATABASE=oxicloud_test \
      bash tests/common/init-test-schema.sh
    DATABASE_URL='postgres://oxicloud_test:oxicloud_test@localhost:5433/oxicloud_test' \
      RUSTFLAGS='--cfg integration_tests' \
      cargo test --workspace --tests {{filter}}
    bash tests/common/stop-db.sh

test-one name:
    cargo test {{name}}

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

lint:
    cargo clippy --all-features --all-targets -- -D warnings

check:
    cargo fmt --all
    cargo clippy --all-features --all-targets -- -D warnings


# Verify every new sqlx migration this branch introduces has a
# timestamp STRICTLY greater than any migration already on the target
# branch (default: main). Guards against the "PR is fine at creation
# but got overtaken by a later-timestamped migration merging first"
# case that would trip sqlx strict-mode on deploy — and the local case
# where you rebased and now your migration file is out-of-order vs.
# what your dev DB already applied.
#
# Usage:
#   just check-migrations            # compares against origin/main
#   just check-migrations dev        # compares against origin/dev
#
# Same logic runs in `.github/workflows/migrations.yml` on every PR.
check-migrations base='main':
    #!/usr/bin/env bash
    set -euo pipefail
    git fetch --quiet origin {{base}}
    base_max=$(git ls-tree -r "origin/{{base}}" --name-only -- migrations/ \
        | grep -oE 'migrations/[0-9]{14}_' \
        | sed 's|migrations/||; s|_$||' \
        | sort | tail -1)
    if [[ -z "$base_max" ]]; then
        echo "No migrations on origin/{{base}} — skipping ordering check."
        exit 0
    fi
    new=$(git diff --name-only --diff-filter=A "origin/{{base}}...HEAD" -- migrations/ \
        | grep -E 'migrations/[0-9]{14}_' || true)
    if [[ -z "$new" ]]; then
        echo "No new migrations on this branch — nothing to check."
        exit 0
    fi
    fail=0
    for m in $new; do
        ts=$(basename "$m" | grep -oE '^[0-9]{14}')
        if [[ "$ts" -le "$base_max" ]]; then
            echo "ERROR: $m has timestamp $ts, not strictly > origin/{{base}}'s latest ($base_max)."
            echo "       Rename to a timestamp > $base_max to avoid sqlx strict-mode errors on deploy."
            fail=1
        fi
    done
    if [[ $fail == 0 ]]; then
        echo "OK — all new migrations post-date origin/{{base}}'s latest ($base_max)."
    fi
    exit $fail


wasm-check:
    cd wasm/oxicloud-hash; cargo fmt --all
    cd wasm/oxicloud-hash; cargo clippy --all-features --release -- -D warnings

wasm-test:
    cd wasm/oxicloud-hash; cargo test --release

# Run the host plugin-runtime tests (compiles the Extism/wasmtime runtime).
test-plugins:
    cargo test --features plugins

# Rebuild the committed plugin .wasm fixtures from wasm/oxicloud-plugin-hello/.
# Requires the wasm32 target (devenv provides it; else `rustup target add
# wasm32-unknown-unknown`). Commit the regenerated files; CI fails on drift.
plugin-build:
    bash scripts/build-plugin-hello.sh

# Build the example plugin and bundle plugin.toml + .wasm into an installable
# .zip at dist/oxicloud-plugin-hello.zip (upload via the admin Plugins tab).
plugin-example-zip:
    bash scripts/build-plugin-zip.sh

# fmt + clippy the example plugin crate (standalone workspace, wasm32 target).
plugin-check:
    cd wasm/oxicloud-plugin-hello; cargo fmt --all
    cd wasm/oxicloud-plugin-hello; cargo clippy --target wasm32-unknown-unknown --release -- -D warnings

# audit security (condition: cargo install cargo-audit)
audit:
    cargo audit

openapi:
    cargo run --bin generate-openapi

db:
    docker compose up -d postgres

db-down:
    docker compose down

# Frontend design-system / a11y guardrails (pure Node, no deps). The entry
# point the UI/UX workflow uses; delegates to `front-design`.
frontend-check: front-design

# End-to-end Playwright — SvelteKit SPA suite (tests/e2e/spa/).
# Default target for all e2e work since the frontend migration.
# Depends on `fe-build-e2e`: the runner serves `./static-dist/`, so
# the built assets have to be current with `COVERAGE=1 VITE_E2E=1`
# instrumentation or the SPA-side data-testids won't exist. Server
# stdout/stderr is captured at `tests/e2e/server-startup.log`;
# `tail -F` it in another terminal to see the cold-start progress
# (webServer boot can take minutes on a cold cargo cache and the
# `list` reporter prints nothing until the first test runs).
front-test: fe-build-e2e
    cd tests/e2e && npm run test:coverage

# Records against a throwaway container stack (its own Postgres + the OxiCloud
# SPA). Each starting point is a file in tests/e2e/scenarios/codegen/ that sets
# up state then calls page.pause(); drop a new *.spec.ts there to add one — this
# menu discovers them automatically.

# Interactive Playwright codegen — pick a starting point, then record
front-codegen:
    bash tests/e2e/scripts/codegen.sh

# Frontend design-system guardrails — pure Node, no extra deps, run against the
# SvelteKit frontend (frontend/). Locale completeness, dead-token report, and
# brand-mark drift. For the full svelte-check/eslint/stylelint/prettier gate use
# `just fe-check` (needs the frontend devDependencies installed).
front-design:
    node scripts/check-locales.mjs
    node scripts/check-dead-tokens.mjs
    node scripts/check-brand-drift.mjs


# Hurl-driven functional tests (starts postgres + server, tears down after).
#
# Four runners — each isolated, brings up its own sidecars + server config:
#   * tests/api/run.sh              — REST API surface, default server.env
#   * tests/webdav/run.sh           — native WebDAV + NextCloud DAV, default server.env
#   * tests/webdav-drive-root/run.sh — WebDAV `OXICLOUD_WEBDAV_DRIVE_PATH=""`
#                                     variant (drive listing served at
#                                     `/webdav/` instead of `/webdav/@drive/`).
#                                     Server launched with
#                                     --config server-webdav-drive-root.env
#                                     so the default runners stay on the
#                                     `"@drive"` config.
#   * tests/oidc/run.sh             — OIDC SSO end-to-end against a fake IdP
#                                     (tests/oidc/fake_idp, a Node
#                                     panva/oidc-provider wrapper); server
#                                     launched with
#                                     --config server-with-oidc.env so the
#                                     api and webdav suites stay on the
#                                     OIDC-off config.
#   * tests/oidc/run-manual-sso-only.sh — NOT part of this chain (see
#                                     `oidc-manual-sso-only` below): a
#                                http://localhost:8090/files/1bf4713c-891e-46fb-acf0-b10231fe32c8     human-run check that OIDC-as-only-
#                                     login-method actually redirects a
#                                     real browser, which the curl-driven
#                                     suite above can't observe.
#
# Same chain runs in CI under the `api-test` job in
# .github/workflows/ci.yml; keep the order in sync so a local pass means
# CI passes.
api-test:
    #!/usr/bin/env bash
    set -x
    set -euo pipefail
    ./tests/api/run.sh
    ./tests/webdav/run.sh
    ./tests/webdav-drive-root/run.sh
    ./tests/oidc/run.sh
    if which litmus >/dev/null 2>/dev/null
    then
        ./tests/webdav/run-litmus.sh
    else
        echo "XXX litmus webdav not found, ignore test"
    fi

# CalDAV client-driven conformance suite.
#
# Drives OxiCloud through the maintained `python-caldav` client library
# — the same VObject/RFC 5545 stack Thunderbird / DAVx⁵ / Gnome Calendar
# use. Complements Hurl coverage (which exercises raw HTTP) by proving
# a real client can round-trip recurring events, per-instance overrides
# (RFC 5545 §3.8.4.4), and all-day masters (the shape #528 was filed
# against).
#
# Not chained into `api-test` because it needs python3; run explicitly.
# The orchestrator spawns its own postgres + server on port 8091 so it
# can run in parallel with api-test/webdav.
#
# Runs `cargo build` first so the orchestrator always sees a fresh
# binary. run-pycaldav.sh itself doesn't rebuild — it uses whatever
# binary is on disk (CI pattern: pre-built release artifact). Doing
# the build here in the recipe means local iterative dev never runs
# pytest against a stale binary from an earlier `cargo check`, while
# CI still gets to skip the recompile.
test-caldav:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v python3 >/dev/null 2>&1; then
        echo "XXX python3 not found — skipping CalDAV client-driven tests"
        exit 0
    fi
    cargo build
    ./tests/caldav/run-pycaldav.sh

# Manual, human-run: launches OxiCloud with OIDC as the ONLY login method
# (fake IdP on :1081, server on :8090) and waits for you to eyeball the
# /login auto-redirect in a real browser. Not part of `just api-test` —
# there's no automated assertion here, it's a visual check. Ctrl-C to stop.
#oidc-manual-sso-only:
#    bash tests/oidc/run-manual-sso-only.sh

# ---------------------------------------------------------------------------
# SvelteKit frontend (frontend/) — the only frontend. These `fe-*` recipes
# drive its dev server, build, lint and tests.
# ---------------------------------------------------------------------------

# install frontend dependencies
fe-install:
    cd frontend && npm ci

# Vite dev server only (HMR) — backend must already be running on :8086
fe-dev:
    cd frontend && npm run dev

# build the SPA (Phase 0: -> frontend/build; Phase 5: -> static-dist)
fe-build:
    cd frontend && npm run build

# Build the SPA with e2e instrumentation for the Playwright coverage
# suite. Both env vars are load-bearing:
#   * VITE_E2E=1  — keeps the `data-testid` tile hooks the release
#                   build strips, so `page.getByTestId(filename)` and
#                   the drop-zone / preferences selectors work.
#   * COVERAGE=1  — Istanbul-instruments the SPA so per-test
#                   `window.__coverage__` lands in `.nyc_output/`
#                   (see `playwright.coverage.config.ts`). Missing
#                   this makes the runner start but the coverage
#                   report empty.
# Called automatically by `front-test`; run manually if you're
# invoking Playwright directly.
fe-build-e2e:
    cd frontend && COVERAGE=1 VITE_E2E=1 npm run build

# svelte-check + eslint + stylelint + prettier
fe-check:
    cd frontend && npm run check

# Vitest unit/component tests
fe-test:
    cd frontend && npm run test:unit

# Run backend (API) and the Vite dev server together; one Ctrl-C stops both.
dev:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo run &
    backend=$!
    trap 'kill $backend 2>/dev/null' EXIT INT TERM
    cd frontend && npm run dev

# k6 load suite — full scenarios + regression diff vs baseline/load.json.
# Used by the nightly workflow and on demand. Release build for fair timings.
load:
    bash tests/load/run.sh

# k6 smoke — single happy-path iteration. PR-tier liveness check, no gate.
load-smoke:
    bash tests/load/smoke.sh

# Re-run the full suite, then bake the latest summary into baseline/load.json.
# The leading `-` lets `run.sh`'s regression-exit not abort the bake step;
# review the diff and commit deliberately:
#   chore(load): accept new baseline for <reason>
load-baseline:
    -bash tests/load/run.sh
    node tests/load/bake-baseline.mjs

# Standalone seeder (poking around in psql). Needs the test DB up:
#   just db
load-seed:
    cargo run --bin load-seed -- --depth 5 --fanout 4 --files-per-leaf 3

# Check and test everything
# recommanded before pull request
pre-pull-request: check fe-check audit check-migrations test test-integration fe-test build api-test fe-build-e2e  front-test
