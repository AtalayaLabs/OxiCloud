#!/usr/bin/env bash
# CalDAV end-to-end conformance test using python-caldav.
#
# python-caldav (https://github.com/python-caldav/caldav) is the same
# maintained client library used to test radicale, xandikos, davical.
# Driving OxiCloud through it exercises the code paths that real
# clients (Thunderbird, Apple Calendar, Gnome Calendar, DAVx⁵) hit —
# it's the closest cognate to what `litmus` does for WebDAV, but for
# the CalDAV surface.
#
# Usage (from repo root via justfile):
#   just test-caldav
#
# Or directly:
#   bash tests/caldav/run-pycaldav.sh
#
# Requires: python3 (>= 3.10 for python-caldav 1.x), curl, jq, docker
# The `caldav` library + pytest are installed into a per-run venv at
# `tests/caldav/.venv/`, gitignored.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
CALDAV_DIR="$REPO_ROOT/tests/caldav"

# shellcheck source=test.env
source "$CALDAV_DIR/test.env"

SERVER_PORT="${base_url##*:}"

log()  { echo "[caldav] $*"; }
die()  { echo "[caldav] ERROR: $*" >&2; exit 1; }

# ── Dependency checks ─────────────────────────────────────────────────────────

if ! command -v python3 >/dev/null 2>&1; then
    die "python3 not found. Install a recent Python 3."
fi
if ! command -v jq >/dev/null 2>&1; then
    die "jq not found."
fi
if ! command -v curl >/dev/null 2>&1; then
    die "curl not found."
fi

# ── Teardown (always runs on exit) ────────────────────────────────────────────

SERVER_PID=""

SUITE_EXIT=0

cleanup() {
    # If pytest failed, show the last chunk of server log so
    # someone debugging doesn't have to hunt for the file.
    if [[ $SUITE_EXIT -ne 0 && -n "${SERVER_LOG:-}" && -f "$SERVER_LOG" ]]; then
        log "── server log tail (last 40 lines) ─────────────────────────"
        tail -n 40 "$SERVER_LOG" >&2
        log "── end server log tail ─────────────────────────────────────"
    fi
    if [[ -n "$SERVER_PID" ]]; then
        log "Stopping OxiCloud (pid $SERVER_PID)..."
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    bash "$COMMON/stop-db.sh"
}

trap cleanup EXIT

# ── 1. Start postgres ────────────────────────────────────────────────────────

bash "$COMMON/spawn-db.sh"

# ── 2. Start OxiCloud ────────────────────────────────────────────────────────

set -a
# shellcheck source=../common/server.env
source "$COMMON/server.env"
OXICLOUD_SERVER_PORT=$SERVER_PORT
OXICLOUD_STORAGE_PATH="$CALDAV_DIR/storage"
# Small on purpose: makes the RFC 6578 row-cap retention path
# (test_sync_collection.py's expiry test) deterministically
# reachable by generating a handful of changes, instead of waiting
# out a real time-based retention window. Harmless to every other
# test in this suite — none of them generate anywhere near 5
# change-log rows on a single collection.
OXICLOUD_SYNC_LOG_MAX_ROWS_PER_COLLECTION=5
set +a

# Wipe storage between runs so a stale run doesn't leak into fresh state.
# Regex-gated via wipe-storage.sh so we can never `rm -rf /`.
# shellcheck source=../common/wipe-storage.sh
source "$COMMON/wipe-storage.sh"
wipe_storage "$OXICLOUD_STORAGE_PATH"

BUILD_TARGET="${BUILD_TARGET:-debug}"
OXICLOUD_BIN="$REPO_ROOT/target/$BUILD_TARGET/oxicloud"

# Use the binary if it's already there — CI downloads a pre-built
# release artifact and would waste ~5 min recompiling from scratch
# (empty target cache) if we always rebuilt. Local devs get the
# fresh-binary guarantee via `just test-caldav`, which runs
# `cargo build` before invoking this script (see the recipe in
# justfile).
#
# The stale-binary trap this used to guard against (a `cargo check`
# or `cargo clippy` leaving the on-disk binary behind while source
# changed) only bites when this script is invoked DIRECTLY without
# going through the justfile — a rare workflow. Documented on
# `just test-caldav` for the record.
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
# `--config` pins the env file, suppressing the default `.env` probe so
# a developer's repo-root `.env` can never leak into a test run.
#
# Redirect server stdout/stderr to a log file — otherwise every audit
# line + tower-http error line interleaves with pytest's per-test
# output, drowning PASSED/XFAIL markers under log spam. Cat the tail
# of the log on cleanup so failures still surface the last events.
SERVER_LOG="$CALDAV_DIR/server.log"
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

# Real CalDAV clients authenticate via app password (Basic Auth), not
# JWT — same rule as WebDAV. Session/account passwords are deliberately
# refused on DAV surfaces (memory: DAV surfaces require app passwords
# only). python-caldav uses HTTP Basic; the app password IS the credential.
APP_PW_RESP=$(curl -s -X POST \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer $JWT" \
    -d '{"label":"pycaldav-test"}' \
    "$base_url/api/auth/app-passwords")
APP_PASSWORD=$(jq -r '.password' <<<"$APP_PW_RESP")
[[ -z "$APP_PASSWORD" || "$APP_PASSWORD" == "null" ]] && die "App password creation failed: $APP_PW_RESP"
log "App password created."

# ── 4. Python venv + install caldav + pytest ─────────────────────────────────

VENV="$CALDAV_DIR/.venv"
if [[ ! -d "$VENV" ]]; then
    log "Creating Python venv at $VENV..."
    python3 -m venv "$VENV"
fi
# shellcheck source=/dev/null
source "$VENV/bin/activate"

# Pin the major to avoid a surprise API break on `caldav` 2.x if/when
# that lands. `pytest` version is loose — no reason to over-constrain
# a test-only dep.
if ! python3 -c "import caldav" 2>/dev/null; then
    log "Installing python-caldav + pytest into venv..."
    pip install --quiet 'caldav>=1.3,<2.0' 'pytest>=7,<9'
fi

# ── 5. Run pytest ────────────────────────────────────────────────────────────

log "Running pytest suite in $CALDAV_DIR/"
export OXICLOUD_CALDAV_URL="$base_url/caldav/"
export OXICLOUD_CALDAV_USERNAME="$username"
export OXICLOUD_CALDAV_APP_PASSWORD="$APP_PASSWORD"
# Admin JWT minted in step 3 above (used there to mint the app
# password) — also needed by test_sync_collection.py's
# retention/expiry test, which triggers the admin-only
# `POST /api/admin/jobs/sync_log_retention/trigger` endpoint. App
# passwords / DAV Basic Auth don't satisfy `require_admin`.
export OXICLOUD_ADMIN_JWT="$JWT"

cd "$CALDAV_DIR"
# `--show-capture=no` hides pytest's "Captured log setup/call" section
# entirely on failure. pycaldav emits a full lxml XMLSyntaxError
# traceback via `logging.critical(..., exc_info=True)` on every
# make_calendar() when the server ignores the URL slug — genuine
# assertion output was drowning in it. Real test failures still show
# the assertion line + short traceback via --tb=short.
#
# Don't let a pytest non-zero exit skip the cleanup trap — capture
# the status, invoke cleanup (which tails the server log on failure),
# then re-emit the exit code.
set +e
pytest -v --tb=short --show-capture=no "$@"
SUITE_EXIT=$?
set -e

if [[ $SUITE_EXIT -eq 0 ]]; then
    log "pycaldav suite passed."
else
    log "pycaldav suite failed (exit $SUITE_EXIT)."
fi
# Always show where the server log is — useful for post-mortem
# ("why did the server log an error next to that XFAIL?") even
# on green runs. On failure the cleanup trap has already dumped
# the tail; the file itself sticks around until the next run
# truncates it.
log "Server log preserved at: $SERVER_LOG"
exit "$SUITE_EXIT"
