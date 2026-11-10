#!/usr/bin/env bash
# WebDAV RFC 4918 compliance test using the litmus test suite.
#
# Usage (from repo root via justfile):
#   just litmus-test
#
# Or directly (server + postgres must already be running):
#   bash tests/webdav/run-litmus.sh
#
# Requires: litmus (apt install litmus), jq, curl
# litmus tests: basic copymove props locks

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
WEBDAV_DIR="$REPO_ROOT/tests/webdav"

source "$WEBDAV_DIR/test.env"

SERVER_PORT="${base_url##*:}"

log()  { echo "[litmus] $*"; }
die()  { echo "[litmus] ERROR: $*" >&2; exit 1; }

# ── Dependency checks ──────────────────────────────────────────────────────────

if ! command -v litmus >/dev/null 2>&1; then
    die "litmus not found. Install with: sudo apt install litmus"
fi
if ! command -v jq >/dev/null 2>&1; then
    die "jq not found. Install with: sudo apt install jq"
fi

# ── Teardown ───────────────────────────────────────────────────────────────────

SERVER_PID=""

cleanup() {
    if [[ -n "$SERVER_PID" ]]; then
        log "Stopping OxiCloud (pid $SERVER_PID)..."
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    bash "$COMMON/stop-db.sh"
}

trap cleanup EXIT

# ── 1. Start postgres ──────────────────────────────────────────────────────────

bash "$COMMON/spawn-db.sh"

# ── 2. Start OxiCloud ─────────────────────────────────────────────────────────

set -a
source "$COMMON/server.env"
OXICLOUD_SERVER_PORT=$SERVER_PORT
OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/webdav/storage-litmus"
set +a

rm -rf "$OXICLOUD_STORAGE_PATH"
mkdir -p "$OXICLOUD_STORAGE_PATH"

BUILD_TARGET="${BUILD_TARGET:-debug}"
OXICLOUD_BIN="$REPO_ROOT/target/$BUILD_TARGET/oxicloud"

if [[ -x "$OXICLOUD_BIN" ]]; then
    log "Starting pre-built OxiCloud ($BUILD_TARGET) on port $SERVER_PORT..."
    "$OXICLOUD_BIN" --config "$COMMON/server.env" &
else
    log "Building and starting OxiCloud on port $SERVER_PORT..."
    cd "$REPO_ROOT"
    # `--features plugins` to share one artifact with every other build of
    # this binary — see `tests/common/cargo_build.sh`.
    cargo build --features plugins 2>&1
    "$REPO_ROOT/target/debug/oxicloud" --config "$COMMON/server.env" &
fi
SERVER_PID=$!

log "Waiting for server at $base_url..."
deadline=$(( $(date +%s) + 60 ))
until curl -sf "$base_url/ready" >/dev/null 2>&1; do
    [[ $(date +%s) -ge $deadline ]] && die "Server did not become ready within 60s"
    sleep 1
done
log "Server ready."

# ── 3. Bootstrap admin + app password ────────────────────────────────────────

SETUP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST -H "Content-Type: application/json" \
    -d "{\"username\":\"$username\",\"email\":\"$email\",\"password\":\"$password\"}" \
    "$base_url/api/setup")
case "$SETUP_STATUS" in
    201) log "Admin account created." ;;
    403) log "Admin account already exists." ;;
    *)   die "Unexpected /api/setup status: $SETUP_STATUS" ;;
esac

LOGIN_RESP=$(curl -s -X POST -H "Content-Type: application/json" \
    -d "{\"username\":\"$username\",\"password\":\"$password\"}" \
    "$base_url/api/auth/login")
JWT=$(jq -r '.access_token' <<<"$LOGIN_RESP")
[[ -z "$JWT" || "$JWT" == "null" ]] && die "Login failed: $LOGIN_RESP"
log "Logged in as $username."

APP_PW_RESP=$(curl -s -X POST \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer $JWT" \
    -d '{"label":"litmus-test"}' \
    "$base_url/api/auth/app-passwords")
APP_PASSWORD=$(jq -r '.password' <<<"$APP_PW_RESP")
[[ -z "$APP_PASSWORD" || "$APP_PASSWORD" == "null" ]] && die "App password creation failed: $APP_PW_RESP"
log "App password created."

# ── 4. Run litmus ─────────────────────────────────────────────────────────────

LITMUS_TESTS="${LITMUS_TESTS:-basic copymove props locks}"
WEBDAV_URL="$base_url/webdav/"

log "Running litmus $LITMUS_TESTS against $WEBDAV_URL"
TESTS="$LITMUS_TESTS" litmus "$WEBDAV_URL" "$username" "$APP_PASSWORD"

log "litmus passed."
