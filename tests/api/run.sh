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
API_DIR="$REPO_ROOT/tests/api"

# test.env is the single source of truth for connection details and credentials.
# shellcheck source=test.env
source "$API_DIR/test.env"

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

WOPI_MOCK_PID=""

cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    log "Stopping OxiCloud server (pid $SERVER_PID)..."
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  if [[ -n "$WOPI_MOCK_PID" ]]; then
    log "Stopping WOPI mock discovery (pid $WOPI_MOCK_PID)..."
    kill "$WOPI_MOCK_PID" 2>/dev/null || true
    wait "$WOPI_MOCK_PID" 2>/dev/null || true
  fi
  bash "$COMMON/stop-db.sh"
}

trap cleanup EXIT

# ── 0. WOPI mock discovery ────────────────────────────────────────────────────
# Serves the static discovery.xml `OXICLOUD_WOPI_DISCOVERY_URL`
# points at (server.env pins port 9100). Started BEFORE OxiCloud so
# the server's cache-fill on first WOPI request finds it. The mock
# is stdlib-only Python (no deps) — see the file header for what it
# returns and why it's cheap.
log "Starting WOPI mock discovery on port 9100..."
node "$COMMON/wopi_mock_discovery.js" > /tmp/wopi-mock-discovery.log 2>&1 &
WOPI_MOCK_PID=$!

# ── 1. Start postgres ─────────────────────────────────────────────────────────

bash "$COMMON/spawn-db.sh"

# ── 2. Load shared server env + port from .env ───────────────────────────────

set -a
# shellcheck source=../common/server.env
source "$COMMON/server.env"
OXICLOUD_SERVER_PORT=$SERVER_PORT
OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/api/storage"
set +a

# ensure storage is empty before starting (regex-gated rm -rf)
# shellcheck source=../common/wipe-storage.sh
source "$COMMON/wipe-storage.sh"
wipe_storage "$OXICLOUD_STORAGE_PATH"

# ── 3. Start OxiCloud server ──────────────────────────────────────────────────

BUILD_TARGET="${BUILD_TARGET:-debug}"
OXICLOUD_BIN="$REPO_ROOT/target/$BUILD_TARGET/oxicloud"

# Build synchronously (no time cap — clean builds take minutes) BEFORE
# starting the server, so the `/ready` poll below only times what we
# actually want it to time: server startup, not compilation. Earlier
# this ran `cargo run &` directly, which conflated the two and tripped
# the 120 s readiness timeout on any `cargo clean` run.
if [[ ! -x "$OXICLOUD_BIN" ]]; then
  log "Building OxiCloud server ($BUILD_TARGET) — this can take a few minutes after \`cargo clean\`..."
  # Cargo's debug profile is the implicit default (`cargo build` alone)
  # — there is NO `--profile debug` flag (it would error). Only the
  # release path needs an explicit flag.
  case "$BUILD_TARGET" in
    debug)   (cd "$REPO_ROOT" && cargo build           2>&1 | tail -n 20) || die "cargo build failed" ;;
    release) (cd "$REPO_ROOT" && cargo build --release 2>&1 | tail -n 20) || die "cargo build --release failed" ;;
    *)       die "Unsupported BUILD_TARGET='$BUILD_TARGET' (expected 'debug' or 'release')" ;;
  esac
fi

if [[ ! -x "$OXICLOUD_BIN" ]]; then
  die "Build completed but $OXICLOUD_BIN is missing — wrong BUILD_TARGET?"
fi

log "Starting OxiCloud server ($BUILD_TARGET) on port $SERVER_PORT..."
# `--config` pins the env file the binary reads AND suppresses the default
# `.env` probe in main.rs, so a developer's repo-root `.env` can never leak
# into a test run. Bash also sourced the same file above, so anything the
# test harness itself reads via $OXICLOUD_* stays available; dotenvy won't
# override those already-exported values.
"$OXICLOUD_BIN" --config "$COMMON/server.env" &
SERVER_PID=$!
log "Waiting for server at $base_url..."
wait_for_http "$base_url/ready" 120
log "Server is ready."

# ── 3.5. Generate ephemeral test fixtures ─────────────────────────────────────
# chunked_upload_cap.hurl needs a body > OXICLOUD_CHUNK_MAX_BYTES (4 MiB) to
# trigger the 413. Committing a 5 MiB binary to the repo would bloat git for
# every clone; generating it at run time is reproducible and the file is in
# `.gitignore`.

OVER_CAP_FIXTURE="$REPO_ROOT/tests/fixtures/chunk-over-cap-5mb.bin"
if [[ ! -s "$OVER_CAP_FIXTURE" ]]; then
  log "Generating 5 MiB fixture for chunk-cap test → $OVER_CAP_FIXTURE"
  dd if=/dev/zero of="$OVER_CAP_FIXTURE" bs=1024 count=5120 status=none
fi

# ── 4. Run Hurl tests ─────────────────────────────────────────────────────────

log "Running Hurl tests..."
# NC baseline tests (groups A + B + C from BASELINE_TESTS_NC_WEBDAV.md)
# are interleaved early because they use a separate code surface and
# their failures should not be masked by later test regressions.
# The auth-failure / lockout file (group P) runs LAST — it locks out
# a throwaway username so admin Basic Auth stays usable for everything
# above it.
hurl --variables-file "$API_DIR/test.env" --file-root "$REPO_ROOT/tests" --test --jobs 1 \
  "$API_DIR/setup.hurl" \
  "$API_DIR/auth_login.hurl" \
  "$API_DIR/user_ui_preferences.hurl" \
  "$API_DIR/auth_session_lifecycle.hurl" \
  "$API_DIR/auth_magic_link_login.hurl" \
  "$API_DIR/auth_upgrade_to_internal.hurl" \
  "$API_DIR/registration.hurl" \
  "$API_DIR/nc_status_capabilities.hurl" \
  "$API_DIR/nc_login_flow_v2.hurl" \
  "$API_DIR/nc_login_flow_v2_drive_picker.hurl" \
  "$API_DIR/nc_ocs_user_info.hurl" \
  "$API_DIR/nc_avatar_preview.hurl" \
  "$API_DIR/files-folders.hurl" \
  "$API_DIR/folder_ancestors.hurl" \
  "$API_DIR/photos_etag.hurl" \
  "$API_DIR/favorites.hurl" \
  "$API_DIR/trash.hurl" \
  "$API_DIR/trash_resources.hurl" \
  "$API_DIR/recent.hurl" \
  "$API_DIR/batch_folder_copy.hurl" \
  "$API_DIR/dedup_blob_cleanup.hurl" \
  "$API_DIR/dedup_admin_gate.hurl" \
  "$API_DIR/admin_jobs.hurl" \
  "$API_DIR/recoverable_jobs.hurl" \
  "$API_DIR/default_caldav_carddav.hurl" \
  "$API_DIR/dav_error_mapping.hurl" \
  "$API_DIR/carddav_vcard_properties.hurl" \
  "$API_DIR/contacts.hurl" \
  "$API_DIR/calendar.hurl" \
  "$API_DIR/caldav_recurring.hurl" \
  "$API_DIR/caldav_calendar_query.hurl" \
  "$API_DIR/playlists.hurl" \
  "$API_DIR/public_shares.hurl" \
  "$API_DIR/permissions.hurl" \
  "$API_DIR/grants.hurl" \
  "$API_DIR/grant_cleanup.hurl" \
  "$API_DIR/role_grants.hurl" \
  "$API_DIR/subject_groups.hurl" \
  "$API_DIR/groups_effective_members.hurl" \
  "$API_DIR/grants_nested_groups.hurl" \
  "$API_DIR/drives_foundation.hurl" \
  "$API_DIR/drives_membership.hurl" \
  "$API_DIR/external_users.hurl" \
  "$API_DIR/search_basic.hurl" \
  "$API_DIR/nc_second_user_setup.hurl" \
  "$API_DIR/nc_admin_views_other_user.hurl" \
  "$API_DIR/admin_user_ops.hurl" \
  "$API_DIR/external_mounts.hurl" \
  "$API_DIR/chunked_upload_cap.hurl" \
  "$API_DIR/nc_auth_failures.hurl" \
  "$API_DIR/dedup_create.hurl" \
  "$API_DIR/trash_per_drive.hurl" \
  "$API_DIR/drive_quota.hurl" \
  "$API_DIR/user_envelope_quota.hurl" \
  "$API_DIR/regression_595_unlimited_user_quota.hurl" \
  "$API_DIR/drive_policies.hurl" \
  "$API_DIR/drive_read_only.hurl" \
  "$API_DIR/cross_drive_move.hurl" \
  "$API_DIR/cross_drive_copy.hurl" \
  "$API_DIR/nc_multidrive_move_regression.hurl" \
  "$API_DIR/webdav_dead_properties.hurl" \
  "$API_DIR/nc_webdav_dead_properties.hurl" \
  "$API_DIR/webdav_protected_properties.hurl" \
  "$API_DIR/webdav_quota_properties.hurl" \
  "$API_DIR/nc_webdav_quota_properties.hurl" \
  "$API_DIR/webdav_patch.hurl" \
  "$API_DIR/nc_webdav_patch.hurl" \
  "$API_DIR/webdav_patch_consistency.hurl" \
  "$API_DIR/nc_webdav_patch_consistency.hurl" \
  "$API_DIR/nc_webdav_put_gaps.hurl" \
  "$API_DIR/webdav_drive_root.hurl" \
  "$API_DIR/webdav_permissions.hurl" \
  "$API_DIR/webdav_nested_move_cascade.hurl" \
  "$API_DIR/wopi_authz.hurl" \
  "$API_DIR/wopi_shared_drive.hurl"

#bash "$API_DIR/dedup_bulk_upload.sh"

bash "$API_DIR/storage_cleanup_check.sh"

log "All tests passed."
