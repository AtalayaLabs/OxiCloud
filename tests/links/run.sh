#!/usr/bin/env bash
# Links suite runner (docs/plan/links.md).
#
# Boots the server twice against one database: first with
# OXICLOUD_LINKS_MODE=off to seed grants (00_seed_off.hurl), then with the
# server-links.env overlay (mode auto, short job intervals), where the
# off → auto switch runs the backfill and the remaining scenarios run in order.
#
# Usage (from repo root):
#   bash tests/links/run.sh
#   SKIP_BUILD=1 bash tests/links/run.sh   # reuse the current binary
#
# Prerequisites: docker, cargo, hurl ≥ 4.0, jq
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
TEST_DIR="$REPO_ROOT/tests/links"

# shellcheck source=../common/cargo_build.sh
source "$COMMON/cargo_build.sh"

# shellcheck source=test.env
source "$TEST_DIR/test.env"

SERVER_PORT="${base_url##*:}"

log()  { echo "[links] $*"; }
die()  { echo "[links] ERROR: $*" >&2; exit 1; }

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

stop_server() {
  if [[ -n "$SERVER_PID" ]]; then
    log "Stopping OxiCloud server (pid $SERVER_PID)..."
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
  fi
}

cleanup() {
  stop_server
  bash "$COMMON/stop-db.sh"
}

trap cleanup EXIT

# ── 1. Build ──────────────────────────────────────────────────────────────────

BUILD_TARGET="${BUILD_TARGET:-debug}"
OXICLOUD_BIN="$REPO_ROOT/target/$BUILD_TARGET/oxicloud"

if [[ ! -x "$OXICLOUD_BIN" ]]; then
  NEED_BUILD=1
elif [[ "${SKIP_BUILD:-0}" == "1" || -n "${CI:-}" ]]; then
  NEED_BUILD=0
else
  NEED_BUILD=1
fi

if (( NEED_BUILD )); then
  case "$BUILD_TARGET" in
    debug)   cargo_build_logged "OxiCloud server (debug)"             || die "cargo build failed" ;;
    release) cargo_build_logged "OxiCloud server (release)" --release || die "cargo build --release failed" ;;
    *)       die "Unsupported BUILD_TARGET='$BUILD_TARGET' (expected 'debug' or 'release')" ;;
  esac
fi

[[ -x "$OXICLOUD_BIN" ]] || die "$OXICLOUD_BIN is missing — wrong BUILD_TARGET?"

# ── 2. Start postgres, wipe storage ───────────────────────────────────────────

bash "$COMMON/spawn-db.sh"

export OXICLOUD_SERVER_PORT=$SERVER_PORT
export OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/links/storage"

# shellcheck source=../common/wipe-storage.sh
source "$COMMON/wipe-storage.sh"
wipe_storage "$OXICLOUD_STORAGE_PATH"

SERVER_LOG="$REPO_ROOT/target/links-server.log"
mkdir -p "$(dirname "$SERVER_LOG")"
: > "$SERVER_LOG"

fail() {
  log "Last 200 lines of $SERVER_LOG:"
  tail -n 200 "$SERVER_LOG" >&2
  die "$*"
}

# The process environment wins over `--config`, so each boot's links knobs are exported.
start_server() {
  log "Starting OxiCloud server (links mode $OXICLOUD_LINKS_MODE) on port $SERVER_PORT..."
  (cd "$REPO_ROOT" && exec "$OXICLOUD_BIN" --config "$COMMON/server.env") >>"$SERVER_LOG" 2>&1 &
  SERVER_PID=$!
  wait_for_http "$base_url/ready" 120
  log "Server is ready."
}

run_hurl() {
  hurl --variables-file "$TEST_DIR/test.env" \
       --file-root "$REPO_ROOT/tests" \
       --test --jobs 1 \
       "$@"
}

# ── 3. Seed with links off ────────────────────────────────────────────────────

export OXICLOUD_LINKS_MODE=off
start_server
run_hurl "$REPO_ROOT/tests/api/setup.hurl" "$TEST_DIR/00_seed_off.hurl" || fail "seed failed"
stop_server

# ── 4. Reboot in auto, run the scenarios in order ─────────────────────────────

set -a
# shellcheck source=../common/server-links.env
source "$COMMON/server-links.env"
set +a
start_server

# The expiry is computed after the slow setup (users, logins), which hands its captures over.
run_expiry() {
  local json vars=() expires_soon
  json="$(run_hurl --json "$TEST_DIR/25_expiry_setup.hurl")" || return 1
  mapfile -t vars < <(jq -r '.entries[].captures[]? | "\(.name)=\(.value)"' <<<"$json")
  expires_soon="$(date -u -d '+12 seconds' '+%Y-%m-%dT%H:%M:%SZ')"
  run_hurl "${vars[@]/#/--variable=}" --variable "expires_soon=$expires_soon" "$TEST_DIR/25_expiry.hurl"
}

run_hurl "$TEST_DIR/10_backfill.hurl" || fail "10_backfill.hurl failed; later files depend on it"

FAILED=()
for f in "$TEST_DIR"/[2-9]*.hurl; do
  case "$(basename "$f")" in
    25_expiry_setup.hurl) continue ;;
    25_expiry.hurl) run_expiry || FAILED+=("25_expiry.hurl") ;;
    *) run_hurl "$f" || FAILED+=("$(basename "$f")") ;;
  esac
done

(( ${#FAILED[@]} == 0 )) || fail "failed: ${FAILED[*]}"
log "links tests passed."
