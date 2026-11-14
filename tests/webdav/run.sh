#!/usr/bin/env bash
# Full Hurl API test runner.
# Starts postgres + OxiCloud server, runs Hurl tests, tears everything down.
#
# Usage (from repo root):
#   bash tests/api/run.sh
#
# Prerequisites: docker, cargo, hurl ≥ 4.0

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
WEBDAV_DIR="$REPO_ROOT/tests/webdav"

# test.env is the single source of truth for connection details and credentials.
# shellcheck source=test.env
source "$WEBDAV_DIR/test.env"

# Derive server port from base_url (e.g. http://localhost:8087 → 8087)
SERVER_PORT="${base_url##*:}"

# ── Helpers ───────────────────────────────────────────────────────────────────

log()  { echo "[api-test] $*"; }
die()  { echo "[api-test] ERROR: $*" >&2; exit 1; }

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

# ── 2. Load shared server env + port from .env ───────────────────────────────

set -a
# shellcheck source=../common/server.env
source "$COMMON/server.env"
OXICLOUD_SERVER_PORT=$SERVER_PORT
OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/api/storage"
set +a

# ensure storage is empty before starting (regex-gated rm -rf).
# Previously this script only ran `mkdir -p`, so a standalone webdav run
# inherited state from a prior api run — now both runners wipe uniformly.
# shellcheck source=../common/wipe-storage.sh
source "$COMMON/wipe-storage.sh"
wipe_storage "$OXICLOUD_STORAGE_PATH"

# ── 3. Start OxiCloud server ──────────────────────────────────────────────────

BUILD_TARGET="${BUILD_TARGET:-debug}"
OXICLOUD_BIN="$REPO_ROOT/target/$BUILD_TARGET/oxicloud"

# `--config` pins the env file the binary reads AND suppresses the default
# `.env` probe in main.rs, so a developer's repo-root `.env` can never leak
# into a test run. Bash also sourced the same file above, so anything the
# test harness itself reads via $OXICLOUD_* stays available; dotenvy won't
# override those already-exported values.
if [[ -x "$OXICLOUD_BIN" ]]; then
  log "Starting pre-built OxiCloud server ($BUILD_TARGET) on port $SERVER_PORT..."
  "$OXICLOUD_BIN" --config "$COMMON/server.env" &
else
  log "Building and starting OxiCloud server on port $SERVER_PORT..."
  cd "$REPO_ROOT"
  # `--features plugins` to share one artifact with every other build of this
  # binary — see `tests/common/cargo_build.sh`.
  cargo run --features plugins -- --config "$COMMON/server.env" &
fi
SERVER_PID=$!
log "Waiting for server at $base_url..."
wait_for_http "$base_url/ready" 120
log "Server is ready."

# ── 4. Run Hurl tests ─────────────────────────────────────────────────────────

log "Running Hurl tests..."
for T in "$WEBDAV_DIR"/test_*.sh; do
    if bash "$T"
    then
        echo $'\e[32m'"Success $T"$'\e[0m' >&2
    else
        echo $'\e[31m'"Failure $T"$'\e[0m' >&2
        false
    fi
done

log "All tests passed."
