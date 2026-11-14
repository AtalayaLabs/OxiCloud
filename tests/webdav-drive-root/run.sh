#!/usr/bin/env bash
# WebDAV drive-root URL-scheme variant runner.
#
# Exercises `OXICLOUD_WEBDAV_DRIVE_LISTING_PREFIX=""` — the config where the
# WebDAV `@drive` path segment is disabled and `/webdav/` IS the
# drive listing. `tests/api/webdav_drive_root.hurl` covers the
# default `"@drive"` config in the main API run; this runner
# starts a separately-configured server to cover the empty-string
# case, mirroring the OIDC runner's shape.
#
# Usage (from repo root):
#   bash tests/webdav-drive-root/run.sh
#
# Prerequisites: docker, cargo, hurl ≥ 4.0
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
TEST_DIR="$REPO_ROOT/tests/webdav-drive-root"

# shellcheck source=test.env
source "$TEST_DIR/test.env"

SERVER_PORT="${base_url##*:}"

log()  { echo "[webdav-drive-root] $*"; }
die()  { echo "[webdav-drive-root] ERROR: $*" >&2; exit 1; }

wait_for_http() {
  local url="$1" timeout="${2:-60}"
  local deadline=$(( $(date +%s) + timeout ))
  until curl -sf "$url" >/dev/null 2>&1; do
    [[ $(date +%s) -ge $deadline ]] && die "Timeout waiting for $url"
    sleep 1
  done
}

# ── Teardown (always runs on exit) ────────────────────────────────────────────

SERVER_PID=""

cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    log "Stopping OxiCloud server (pid $SERVER_PID)..."
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  bash "$COMMON/stop-db.sh"
}

trap cleanup EXIT

# ── 1. Start postgres ─────────────────────────────────────────────────────────

bash "$COMMON/spawn-db.sh"

# ── 2. Load the drive-root-variant server env + port ──────────────────────────

set -a
# shellcheck source=../common/server-webdav-drive-root.env
source "$COMMON/server-webdav-drive-root.env"
OXICLOUD_SERVER_PORT=$SERVER_PORT
OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/webdav-drive-root/storage"
set +a

# shellcheck source=../common/wipe-storage.sh
source "$COMMON/wipe-storage.sh"
wipe_storage "$OXICLOUD_STORAGE_PATH"

# ── 3. Start OxiCloud server with the drive-root-variant config ───────────────

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

log "Starting OxiCloud server with WEBDAV_DRIVE_LISTING_PREFIX='' on port $SERVER_PORT..."
"$OXICLOUD_BIN" --config "$COMMON/server-webdav-drive-root.env" &
SERVER_PID=$!
log "Waiting for server at $base_url..."
wait_for_http "$base_url/ready" 120
log "Server is ready."

# ── 4. Run Hurl tests ─────────────────────────────────────────────────────────
#
# `setup.hurl` from the shared api/ suite bootstraps the initial admin
# account via `POST /api/setup` — the endpoint locks after the first
# admin exists, so it's a one-shot idempotency-by-server-state seed.
# We reuse the file rather than duplicating the setup body so credential
# / schema changes in the api tests automatically flow here.

log "Running Hurl tests..."
hurl --variables-file "$TEST_DIR/test.env" \
     --file-root "$REPO_ROOT/tests" \
     --test --jobs 1 \
     "$REPO_ROOT/tests/api/setup.hurl" \
     "$TEST_DIR/drive_root_empty_config.hurl"

log "webdav-drive-root tests passed."
