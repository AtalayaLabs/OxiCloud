#!/usr/bin/env bash
# MANUAL, human-run SSO-only auto-redirect check. NOT part of `just
# api-test` / CI — there is no automated assertion here, this launches a
# real server + real fake IdP and waits for a human to open a browser and
# eyeball the behavior.
#
# What it proves that the automated suites can't:
#   * tests/oidc/oidc.hurl drives the OIDC flow via curl against
#     tests/common/server-with-oidc.env, which keeps password login
#     enabled — the frontend's login-page auto-redirect guard
#     (frontend/src/routes/login/+page.svelte) never fires there.
#   * The Vitest coverage for that guard (frontend/src/routes/login/
#     page.test.ts) mocks getOidcProviders() and stubs
#     window.location.replace — it proves the logic is right, not that a
#     real browser actually navigates away when the backend is genuinely
#     OIDC-only.
#
# This script starts OxiCloud with tests/common/server-with-oidc-only.env
# (OIDC is the ONLY login method) against the same fake IdP used by the
# automated suite, then blocks until you Ctrl-C.
#
# Ports (deliberately distinct from tests/oidc/run.sh's 8087 / 1080, so
# this can run alongside `just api-test` or a local `cargo run` dev
# server): OxiCloud on 8090, fake IdP on 1081.
#
# Prerequisites: docker, cargo, node >= 20, npm.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
OIDC_DIR="$REPO_ROOT/tests/oidc"
FAKE_IDP_DIR="$OIDC_DIR/fake_idp"

SERVER_PORT=8090
IDP_PORT=1081
base_url="http://localhost:$SERVER_PORT"
oidc_issuer="http://localhost:$IDP_PORT"

# ── Helpers ────────────────────────────────────────────────────────────────
log() { echo "[oidc-manual] $*"; }
die() { echo "[oidc-manual] ERROR: $*" >&2; exit 1; }

wait_for_http() {
  local url="$1" timeout="${2:-60}"
  local deadline=$(( $(date +%s) + timeout ))
  until curl -sf "$url" >/dev/null 2>&1; do
    [[ $(date +%s) -ge $deadline ]] && die "Timeout waiting for $url"
    sleep 0.5
  done
}

# ── Fake-IdP process management (mirrors tests/oidc/run.sh) ────────────────
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
  (cd "$FAKE_IDP_DIR" && npm ci --silent --no-audit --no-fund)
else
  (cd "$FAKE_IDP_DIR" && npm install --silent --no-audit --no-fund)
fi

log "Sweeping any orphan fake-idp processes from prior runs..."
kill_fake_idp
sleep 0.3

log "Starting fake-idp on port $IDP_PORT..."
FAKE_IDP_ISSUER="$oidc_issuer" FAKE_IDP_PORT="$IDP_PORT" \
  node "$FAKE_IDP_DIR/server.js" > /tmp/fake-idp-manual.log 2>&1 &
log "Waiting for fake-idp discovery endpoint..."
wait_for_http "$oidc_issuer/.well-known/openid-configuration" 30
log "fake-idp is ready (logs: /tmp/fake-idp-manual.log)"

# ── 3. Load shared server env (SSO-only) ────────────────────────────────────
set -a
# shellcheck source=../common/server-with-oidc-only.env
source "$COMMON/server-with-oidc-only.env"
OXICLOUD_SERVER_PORT=$SERVER_PORT
OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/oidc-manual/storage"
set +a

# shellcheck source=../common/wipe-storage.sh
source "$COMMON/wipe-storage.sh"
wipe_storage "$OXICLOUD_STORAGE_PATH"

# ── 3.5. Ensure the SPA is built (static-dist/) ────────────────────────────
# The auto-redirect only fires against the production SPA bundle; without
# it `resolve_static_path` falls back to OXICLOUD_STATIC_PATH=./static,
# which doesn't have it. The frontend is a pure CSR SPA (prerender=false in
# +layout.ts) — there is only ONE shell file, static-dist/index.html, that
# every route (including /login) falls back to. Check for that, not a
# per-route file (one never gets emitted; checking for it would force a
# full rebuild on every single invocation).
DIST_DIR="$REPO_ROOT/static-dist"
if [[ ! -f "$DIST_DIR/index.html" ]]; then
  log "Building SvelteKit SPA (static-dist/index.html missing)..."
  (cd "$REPO_ROOT/frontend" \
    && npm ci --silent --no-audit --no-fund \
    && npm run build) || die "Frontend build failed; static-dist/ is required"
fi

# ── 4. Start OxiCloud server with OIDC-only config ──────────────────────────
BUILD_TARGET="${BUILD_TARGET:-debug}"
OXICLOUD_BIN="$REPO_ROOT/target/$BUILD_TARGET/oxicloud"

if [[ ! -x "$OXICLOUD_BIN" ]]; then
  log "Building OxiCloud server ($BUILD_TARGET)..."
  # `--features plugins` to share one artifact with every other build of this
  # binary — see `tests/common/cargo_build.sh`.
  case "$BUILD_TARGET" in
    debug)   (cd "$REPO_ROOT" && cargo build --features plugins           2>&1 | tail -n 20) || die "cargo build failed" ;;
    release) (cd "$REPO_ROOT" && cargo build --features plugins --release 2>&1 | tail -n 20) || die "cargo build --release failed" ;;
    *)       die "Unsupported BUILD_TARGET='$BUILD_TARGET' (expected 'debug' or 'release')" ;;
  esac
fi

log "Starting OxiCloud server with OIDC-only config on port $SERVER_PORT..."
"$OXICLOUD_BIN" --config "$COMMON/server-with-oidc-only.env" &
SERVER_PID=$!
log "Waiting for server at $base_url..."
wait_for_http "$base_url/ready" 120
log "Server is ready."

# ── 5. Hand off to the human ────────────────────────────────────────────────
cat <<EOF

==========================================================
 SSO-ONLY MANUAL TEST — server ready at $base_url
==========================================================
Open $base_url/login in a browser.

Expected: the page redirects immediately to the fake IdP
($oidc_issuer/...) with no login form flash. The fake IdP
auto-approves — you should land back on
$base_url/login?oidc_code=... and then on the authenticated app.

Edge cases to also eyeball:
  * $base_url/login?error=access_denied
    -> must NOT redirect (loop guard); shows the login form.
  * First run / no admin yet (already handled above by wiping
    storage) -> shows the setup wizard, not a redirect, until
    you complete it once via the IdP.

Press Ctrl-C to stop the server and tear down.
==========================================================

EOF

wait "$SERVER_PID"
