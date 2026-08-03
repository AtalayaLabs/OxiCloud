#!/usr/bin/env bash
# AUTOMATED SSO-only integration test — two phases.
#
# Both phases run against the SAME fake IdP + SAME database (spawned
# once) but restart OxiCloud between them so the boot config differs:
#
#   Phase A — server-with-oidc-only-no-policy.env
#             OXICLOUD_AUTH_METHODS=oidc, no AUTH_POLICIES
#             → GET /login must return 200 (SPA shell, no redirect)
#             → providers.auto_redirect_to_oidc == false
#             Driven by sso-only-no-policy.hurl.
#
#   Phase B — server-with-oidc-only.env
#             OXICLOUD_AUTH_METHODS=oidc AND
#             OXICLOUD_AUTH_POLICIES=auto_redirect_if_standalone_oidc
#             → GET /login must return 307 to /api/auth/oidc/authorize
#             → providers.auto_redirect_to_oidc == true
#             → full OIDC dance + RP-initiated logout assertions
#             Driven by sso-only.hurl.
#
# Phase order matters: A runs first because it doesn't touch DB state
# (no admin bootstrap). B runs second and does the admin bootstrap via
# JIT provisioning. Restarting OxiCloud between phases is cheap
# (~500ms) and cleaner than a hot config reload.
#
# Sibling script tests/oidc/run-manual-sso-only.sh runs Phase B only
# and stops after "server ready" so a human can eyeball the browser
# flow — keep both: this script proves the wire contract, the manual
# one proves the UX.
#
# Ports: OxiCloud on 8090, fake IdP on 1081 (distinct from 8087 / 1080
# so this can run alongside `just api-test` or a local dev server).
#
# Prerequisites: docker, cargo, node >= 20, npm, hurl >= 4.0.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
OIDC_DIR="$REPO_ROOT/tests/oidc"
FAKE_IDP_DIR="$OIDC_DIR/fake_idp"

# shellcheck source=sso-only.env
source "$OIDC_DIR/sso-only.env"

SERVER_PORT="${base_url##*:}"
IDP_PORT="${oidc_issuer##*:}"

# ── Helpers ────────────────────────────────────────────────────────────────
log() { echo "[sso-only] $*"; }
die() { echo "[sso-only] ERROR: $*" >&2; exit 1; }

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
  if command -v lsof >/dev/null 2>&1; then
    local pids
    pids=$(lsof -ti :"$IDP_PORT" 2>/dev/null || true)
    if [[ -n "$pids" ]]; then
      # shellcheck disable=SC2086
      kill -9 $pids 2>/dev/null || true
    fi
  fi
}

# ── Server process management ──────────────────────────────────────────────
SERVER_PID=""

start_oxicloud() {
  local env_file="$1"
  set -a
  # shellcheck disable=SC1090
  source "$env_file"
  OXICLOUD_SERVER_PORT=$SERVER_PORT
  OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/oidc/storage-sso-only"
  set +a
  log "Starting OxiCloud (config: $(basename "$env_file"))..."
  "$OXICLOUD_BIN" --config "$env_file" &
  SERVER_PID=$!
  wait_for_http "$base_url/ready" 120
  log "Server is ready (pid $SERVER_PID)."
}

stop_oxicloud() {
  if [[ -n "$SERVER_PID" ]]; then
    log "Stopping OxiCloud (pid $SERVER_PID)..."
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
    # Give the OS a moment to release the port; without this a fast
    # restart occasionally loses the bind on macOS.
    sleep 0.3
  fi
}

# ── Teardown (always runs on exit) ─────────────────────────────────────────
cleanup() {
  stop_oxicloud
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
  OXICLOUD_BASE_URL_FOR_BCL="$base_url" \
  node "$FAKE_IDP_DIR/server.js" > /tmp/fake-idp-sso-only.log 2>&1 &
log "Waiting for fake-idp discovery endpoint..."
wait_for_http "$oidc_issuer/.well-known/openid-configuration" 30
log "fake-idp is ready (logs: /tmp/fake-idp-sso-only.log)"

# ── 3. Wipe storage once ───────────────────────────────────────────────────
export OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/oidc/storage-sso-only"
# shellcheck source=../common/wipe-storage.sh
source "$COMMON/wipe-storage.sh"
wipe_storage "$OXICLOUD_STORAGE_PATH"

# ── 3.5. Ensure the SPA is built (static-dist/) ────────────────────────────
# Both phases hit /login and expect the SPA shell response (Phase A as
# the primary assertion, Phase B as the loop-guard fallthrough). Without
# static-dist/ the ServeDir fallback would 404 those calls.
DIST_DIR="$REPO_ROOT/static-dist"
if [[ ! -f "$DIST_DIR/index.html" ]]; then
  log "Building SvelteKit SPA (static-dist/index.html missing)..."
  (cd "$REPO_ROOT/frontend" \
    && npm ci --silent --no-audit --no-fund \
    && npm run build) || die "Frontend build failed; static-dist/ is required"
fi

# ── 4. Build OxiCloud once ─────────────────────────────────────────────────
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

# ══════════════════════════════════════════════════════════════════════════
# Phase A — SSO-only, NO auto-redirect policy
# ══════════════════════════════════════════════════════════════════════════
log ""
log "════════════ Phase A: SSO-only, no auto-redirect ════════════"
start_oxicloud "$COMMON/server-with-oidc-only-no-policy.env"
log "Running sso-only-no-policy.hurl..."
hurl --variables-file "$OIDC_DIR/sso-only.env" \
     --file-root "$REPO_ROOT/tests" \
     --test --jobs 1 \
     "$OIDC_DIR/sso-only-no-policy.hurl"
log "Phase A passed."
stop_oxicloud

# ══════════════════════════════════════════════════════════════════════════
# Phase B — SSO-only, auto_redirect_if_standalone_oidc policy on
# ══════════════════════════════════════════════════════════════════════════
log ""
log "════════════ Phase B: SSO-only, auto-redirect ON ════════════"
start_oxicloud "$COMMON/server-with-oidc-only.env"
log "Running sso-only.hurl..."
hurl --variables-file "$OIDC_DIR/sso-only.env" \
     --file-root "$REPO_ROOT/tests" \
     --test --jobs 1 \
     "$OIDC_DIR/sso-only.hurl"
log "Phase B passed."

log ""
log "SSO-only tests (both phases) passed."
