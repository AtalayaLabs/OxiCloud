#!/usr/bin/env bash
# OIDC integration-test runner.
#
# Brings up the test DB, a Node-based fake IdP (panva/node-oidc-provider
# under tests/oidc/fake_idp/), and OxiCloud configured to talk to that
# IdP, then runs the Hurl suite and tears everything down.
#
# Why a separate runner from tests/api/run.sh:
#   * the OxiCloud server here is launched with
#     `--config tests/common/server-with-oidc.env` (OIDC enabled) — the
#     default api run uses server.env with OIDC off, and we don't want
#     to flip flags mid-suite;
#   * the IdP is a Node process this script owns, distinct from the
#     postgres-test container that lives in spawn-db.sh.
#
# Invocation:
#   * locally: chained from `just api-test` after the api + webdav
#     suites, or directly via `bash tests/oidc/run.sh`
#   * in CI:   chained from the `api-test` job in
#     .github/workflows/ci.yml — same shell call, same env.
#
# Prerequisites: docker, cargo, node ≥ 20, npm, hurl ≥ 4.0.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
OIDC_DIR="$REPO_ROOT/tests/oidc"
FAKE_IDP_DIR="$OIDC_DIR/fake_idp"

# Test variables (base_url, admin creds, oidc_issuer, oidc_authorize_endpoint).
# shellcheck source=test.env
source "$OIDC_DIR/test.env"

SERVER_PORT="${base_url##*:}"
# Derive the IdP port from oidc_issuer the same way (e.g. localhost:1080 → 1080).
# Keeps run.sh and test.env in lockstep — change one, the other follows.
IDP_PORT="${oidc_issuer##*:}"

# ── Helpers ────────────────────────────────────────────────────────────────
log() { echo "[oidc-test] $*"; }
die() { echo "[oidc-test] ERROR: $*" >&2; exit 1; }

wait_for_http() {
  local url="$1" timeout="${2:-60}"
  local deadline=$(( $(date +%s) + timeout ))
  until curl -sf "$url" >/dev/null 2>&1; do
    [[ $(date +%s) -ge $deadline ]] && die "Timeout waiting for $url"
    sleep 0.5
  done
}

# ── Fake-IdP process management ────────────────────────────────────────────
# All cleanup paths funnel through this helper so an exit at ANY phase
# (early failure during npm install, hurl assertion fail, Ctrl-C, …)
# always reaps the node process. The earlier subshell+setsid pattern
# leaked daemons whenever the subshell exited before the trap fired,
# leading to the "no change after rerunning" failure mode: an old
# fake-idp from a prior failed run was still bound to port 1080, so
# the new spawn either failed silently (EADDRINUSE) or the tests hit
# the stale config.
#
# Belt-and-braces orphan reaping: kill by script-path pattern AND by
# port. The pattern match misses processes started from a different
# absolute path (e.g. via a symlinked checkout, or from a `node
# server.js` started from inside tests/oidc/fake_idp/ where the
# command line is just `node server.js`). The port-based fallback
# catches anything bound to 1080 regardless of how it was launched —
# the original case that caused the "stale daemon" debugging session.
kill_fake_idp() {
  pkill -f "tests/oidc/fake_idp/server.js" 2>/dev/null || true
  pkill -f "node.*server.js" 2>/dev/null || true
  if command -v lsof >/dev/null 2>&1; then
    local pids
    pids=$(lsof -ti :"$IDP_PORT" 2>/dev/null || true)
    if [[ -n "$pids" ]]; then
      # shellcheck disable=SC2086
      kill -9 $pids 2>/dev/null || true
    fi
  fi
}

# ── Teardown (always runs on exit) ─────────────────────────────────────────
SERVER_PID=""

cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    log "Stopping OxiCloud server (pid $SERVER_PID)..."
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  log "Stopping fake-idp..."
  kill_fake_idp
  bash "$COMMON/stop-db.sh" || true
}
trap cleanup EXIT

# ── 1. Postgres ────────────────────────────────────────────────────────────
bash "$COMMON/spawn-db.sh"

# ── 2. Fake IdP (Node) ─────────────────────────────────────────────────────
log "Installing fake-idp dependencies..."
if [[ -f "$FAKE_IDP_DIR/package-lock.json" ]]; then
  # `npm ci` is faster + deterministic when the lockfile is present.
  (cd "$FAKE_IDP_DIR" && npm ci --silent --no-audit --no-fund)
else
  (cd "$FAKE_IDP_DIR" && npm install --silent --no-audit --no-fund)
fi

# Sweep any orphaned fake-idp processes from a prior crashed/aborted
# run BEFORE starting the new one. Without this, a stale daemon
# bound to port 1080 would either swallow new spawns silently
# (EADDRINUSE buried in /tmp/fake-idp.log) or serve every request
# with its old config — producing the maddening "I changed the
# config but nothing changed" failure mode.
log "Sweeping any orphan fake-idp processes from prior runs..."
kill_fake_idp
# Brief moment for the OS to actually release the listener; without
# this the new node call can race the just-killed process and lose
# the bind.
sleep 0.3

log "Starting fake-idp on port $IDP_PORT..."
# Background the node process directly (no setsid / subshell wrapper).
# pkill-by-path in cleanup means we don't need a process-group dance
# to reap the child; the simpler launch keeps $IDP_PID correct (it's
# the actual node PID, not a wrapping subshell) for any future call
# site that wants to wait on it.
FAKE_IDP_ISSUER="$oidc_issuer" FAKE_IDP_PORT="$IDP_PORT" \
  node "$FAKE_IDP_DIR/server.js" > /tmp/fake-idp.log 2>&1 &
log "Waiting for fake-idp discovery endpoint..."
wait_for_http "$oidc_issuer/.well-known/openid-configuration" 30
log "fake-idp is ready (logs: /tmp/fake-idp.log)"

# ── 3. Load shared server env (port + storage path) ────────────────────────
set -a
# shellcheck source=../common/server-with-oidc.env
source "$COMMON/server-with-oidc.env"
OXICLOUD_SERVER_PORT=$SERVER_PORT
OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/oidc/storage"
set +a

# shellcheck source=../common/wipe-storage.sh
source "$COMMON/wipe-storage.sh"
wipe_storage "$OXICLOUD_STORAGE_PATH"

# ── 3.5. Ensure the SPA is built (static-dist/) ────────────────────────────
# The OIDC suite's Step 4 + Step 9 walk the full redirect chain and assert
# they land on `/login?oidc_code=…` with HTTP 200 — the production contract,
# where the container ships `static-dist/login.html`. Without that bundle
# `resolve_static_path` falls back to `OXICLOUD_STATIC_PATH=./static`, which
# was removed in commit 54639d46 — so ServeDir 404s the route and Step 4
# fails. Build here so local + CI both exercise the production layout.
DIST_DIR="$REPO_ROOT/static-dist"
if [[ ! -f "$DIST_DIR/login.html" ]]; then
  log "Building SvelteKit SPA (static-dist/login.html missing)..."
  (cd "$REPO_ROOT/frontend" \
    && npm ci --silent --no-audit --no-fund \
    && npm run build) || die "Frontend build failed; static-dist/ is required for the OIDC tests"
fi

# ── 4. Start OxiCloud server with OIDC enabled ─────────────────────────────
BUILD_TARGET="${BUILD_TARGET:-debug}"
OXICLOUD_BIN="$REPO_ROOT/target/$BUILD_TARGET/oxicloud"

if [[ ! -x "$OXICLOUD_BIN" ]]; then
  log "Building OxiCloud server ($BUILD_TARGET)..."
  case "$BUILD_TARGET" in
    debug)   (cd "$REPO_ROOT" && cargo build           2>&1 | tail -n 20) || die "cargo build failed" ;;
    release) (cd "$REPO_ROOT" && cargo build --release 2>&1 | tail -n 20) || die "cargo build --release failed" ;;
    *)       die "Unsupported BUILD_TARGET='$BUILD_TARGET' (expected 'debug' or 'release')" ;;
  esac
fi

log "Starting OxiCloud server with OIDC config on port $SERVER_PORT..."
"$OXICLOUD_BIN" --config "$COMMON/server-with-oidc.env" &
SERVER_PID=$!
log "Waiting for server at $base_url..."
wait_for_http "$base_url/ready" 120
log "Server is ready."

# ── 5. Run the OIDC Hurl suite ─────────────────────────────────────────────
# Order matters: oidc.hurl bootstraps the admin and JIT-provisions the
# `oidc_user` federated principal that link_unlink.hurl then reuses.
log "Running OIDC Hurl tests..."
hurl --variables-file "$OIDC_DIR/test.env" \
     --file-root "$REPO_ROOT/tests" \
     --test --jobs 1 \
     "$OIDC_DIR/oidc.hurl" \
     "$OIDC_DIR/link_unlink.hurl"

log "OIDC tests passed."
