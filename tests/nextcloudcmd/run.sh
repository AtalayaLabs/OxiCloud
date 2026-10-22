#!/usr/bin/env bash
# Sync-convergence suite driven by the real Nextcloud desktop sync engine
# (`nextcloudcmd`, run from a pinned Docker image — see ./Dockerfile).
#
# Litmus proves RFC 4918 conformance and the Hurl/WebDAV suites pin wire
# shapes; neither proves that the client NextCloud users actually install
# can keep a folder in sync with OxiCloud. This suite changes files on both
# sides between syncs (renames, moves, delete + restore from trash,
# conflicting edits, killed syncs) and requires the local and server trees
# to end byte-identical. See test_sync_convergence.py.
#
# Usage (from repo root via justfile):
#   just test-nextcloudcmd
#
# Or directly (extra args go to pytest):
#   bash tests/nextcloudcmd/run.sh -k churn
#
# Knobs:
#   NCC_SEED=<n>           replay one churn run (the seed is in the failure)
#   NCC_CHURN_ROUNDS=<n>   churn rounds (default 8)
#
# Requires: docker, python3 (>= 3.10), curl, jq

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
NCC_DIR="$REPO_ROOT/tests/nextcloudcmd"

# shellcheck source=test.env
source "$NCC_DIR/test.env"

SERVER_PORT="${base_url##*:}"
IMAGE="oxicloud-nextcloudcmd:local"

log()  { echo "[nextcloudcmd] $*"; }
die()  { echo "[nextcloudcmd] ERROR: $*" >&2; exit 1; }

# ── Dependency checks ─────────────────────────────────────────────────────────

for bin in docker python3 jq curl; do
    command -v "$bin" >/dev/null 2>&1 || die "$bin not found."
done

# ── Teardown (always runs on exit) ────────────────────────────────────────────

SERVER_PID=""
SUITE_EXIT=0

cleanup() {
    if [[ $SUITE_EXIT -ne 0 && -n "${SERVER_LOG:-}" && -f "$SERVER_LOG" ]]; then
        log "── server log tail (last 40 lines) ─────────────────────────"
        tail -n 40 "$SERVER_LOG" >&2
        log "── end server log tail ─────────────────────────────────────"
    fi
    # A killed pytest can leave a throttled sync container behind.
    docker ps -q --filter "name=^oxicloud-ncc-" | xargs -r docker kill >/dev/null 2>&1 || true
    if [[ -n "$SERVER_PID" ]]; then
        log "Stopping OxiCloud (pid $SERVER_PID)..."
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    bash "$COMMON/stop-db.sh"
}

trap cleanup EXIT

# ── 1. Client image ───────────────────────────────────────────────────────────
#
# Built before the server starts: on a cold cache this is the slowest step,
# and a broken pin should fail before anything else is spun up. Host network
# because some Docker setups have no outbound DNS on the default bridge.

log "Building $IMAGE from $NCC_DIR/Dockerfile..."
docker build --network host -q -t "$IMAGE" "$NCC_DIR" >/dev/null
log "Client: $(docker run --rm "$IMAGE" --version 2>/dev/null | head -n 1)"

# ── 2. Start postgres ────────────────────────────────────────────────────────

bash "$COMMON/spawn-db.sh"

# ── 3. Start OxiCloud ────────────────────────────────────────────────────────

set -a
# shellcheck source=../common/server.env
source "$COMMON/server.env"
OXICLOUD_SERVER_PORT=$SERVER_PORT
OXICLOUD_STORAGE_PATH="$NCC_DIR/storage"
set +a

# shellcheck source=../common/wipe-storage.sh
source "$COMMON/wipe-storage.sh"
wipe_storage "$OXICLOUD_STORAGE_PATH"

BUILD_TARGET="${BUILD_TARGET:-debug}"
OXICLOUD_BIN="$REPO_ROOT/target/$BUILD_TARGET/oxicloud"

# Use the binary if it's already there — CI downloads the pre-built release
# artifact. `just test-nextcloudcmd` runs `cargo build` first, so local runs
# through the justfile never test a stale binary.
if [[ ! -x "$OXICLOUD_BIN" ]]; then
    log "Building OxiCloud ($BUILD_TARGET) — no pre-built binary at $OXICLOUD_BIN..."
    case "$BUILD_TARGET" in
        debug)   (cd "$REPO_ROOT" && cargo build           2>&1 | tail -n 20) || die "cargo build failed" ;;
        release) (cd "$REPO_ROOT" && cargo build --release 2>&1 | tail -n 20) || die "cargo build --release failed" ;;
        *)       die "Unsupported BUILD_TARGET='$BUILD_TARGET' (expected 'debug' or 'release')" ;;
    esac
    [[ -x "$OXICLOUD_BIN" ]] || die "Build completed but $OXICLOUD_BIN is missing"
else
    log "Using pre-built OxiCloud at $OXICLOUD_BIN ($BUILD_TARGET)"
fi

log "Starting OxiCloud ($BUILD_TARGET) on port $SERVER_PORT..."
# `--config` pins the env file, suppressing the default `.env` probe so a
# developer's repo-root `.env` can never leak into a test run.
SERVER_LOG="$NCC_DIR/server.log"
: > "$SERVER_LOG"
"$OXICLOUD_BIN" --config "$COMMON/server.env" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
log "Server log: $SERVER_LOG (tail -f to watch live)"

log "Waiting for server at $base_url..."
deadline=$(( $(date +%s) + 60 ))
until curl -sf "$base_url/ready" >/dev/null 2>&1; do
    [[ $(date +%s) -ge $deadline ]] && die "Server did not become ready within 60s"
    sleep 1
done
log "Server ready."

# ── 4. Bootstrap admin + app password ────────────────────────────────────────

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

# DAV surfaces accept app passwords only — the same credential the desktop
# client obtains through login flow v2.
APP_PW_RESP=$(curl -s -X POST \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer $JWT" \
    -d '{"label":"nextcloudcmd-test"}' \
    "$base_url/api/auth/app-passwords")
APP_PASSWORD=$(jq -r '.password' <<<"$APP_PW_RESP")
[[ -z "$APP_PASSWORD" || "$APP_PASSWORD" == "null" ]] && die "App password creation failed: $APP_PW_RESP"
log "App password created."

# ── 5. Python venv ───────────────────────────────────────────────────────────

VENV="$NCC_DIR/.venv"
if [[ ! -d "$VENV" ]]; then
    log "Creating Python venv at $VENV..."
    python3 -m venv "$VENV"
fi
# shellcheck source=/dev/null
source "$VENV/bin/activate"
if ! python3 -c "import pytest, requests" 2>/dev/null; then
    log "Installing pytest + requests into venv..."
    pip install --quiet 'pytest>=7,<9' 'requests>=2.31,<3'
fi

# ── 6. Run pytest ────────────────────────────────────────────────────────────

export OXICLOUD_BASE_URL="$base_url"
export OXICLOUD_USERNAME="$username"
export OXICLOUD_APP_PASSWORD="$APP_PASSWORD"
export NEXTCLOUDCMD_IMAGE="$IMAGE"

# Per-test local trees and nextcloudcmd logs live under .work/ so a failure
# can be inspected afterwards (CI uploads it). Wiped at the start of a run.
WORK="$NCC_DIR/.work"
rm -rf "$WORK"

cd "$NCC_DIR"
set +e
pytest -v --tb=short --basetemp="$WORK" "$@"
SUITE_EXIT=$?
set -e

if [[ $SUITE_EXIT -eq 0 ]]; then
    log "nextcloudcmd suite passed."
else
    log "nextcloudcmd suite failed (exit $SUITE_EXIT). Sync logs + local trees: $WORK"
fi
exit "$SUITE_EXIT"
