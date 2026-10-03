#!/usr/bin/env bash
# Full Hurl API test runner.
# Starts postgres + OxiCloud server, runs Hurl tests, tears everything down.
#
# Usage (from repo root):
#   bash tests/api/run.sh                      # everything
#   bash tests/api/run.sh --list               # show the selectable names
#   bash tests/api/run.sh rechunk_legacy       # only matching scenarios
#   bash tests/api/run.sh drive_polic refcount # several
#   SKIP_BUILD=1 bash tests/api/run.sh rechunk_legacy   # reuse the current binary
#
# The server is rebuilt on every LOCAL run so the suite always tests the working
# tree. In CI ($CI set) the pre-built artifact is used as-is — see the build
# block below. `SKIP_BUILD=1` forces that skip locally too.
#
# Selectors are plain substring matches against the scenario name (a .hurl
# basename or a *_check.sh basename), so `drive_polic` picks up drive_policies,
# drive_policy_defaults and drive_policy_repair — note `drive_policy` would
# match the last two but NOT drive_policies, which is "polic-ies" rather than
# "policy-s". Substring, not fuzzy. With no selector the whole
# suite runs, which is what CI does — selection exists for iterating on one
# scenario, where paying for the full suite on every change is the difference
# between a ten-second loop and a several-minute one.
#
# setup.hurl ALWAYS runs: it seeds the admin account every other scenario
# authenticates with, so a selective run without it would fail on login rather
# than on anything the selection was meant to exercise.
#
# ── What selection does and does not change ───────────────────────────────────
#
# It does NOT affect freshness. Every invocation recreates the environment
# regardless of selectors: `spawn-db.sh` runs `docker compose down -v` (volumes
# included) before starting postgres, and `wipe_storage` empties the storage
# directory. So scenarios that require an empty DB and empty storage — the
# migration checks in particular, which manufacture legacy state and would
# otherwise import somebody else's leftovers — are as safe selectively as they
# are in a full run.
#
# What it DOES change is ordering context. Scenarios share one database within a
# run and several depend on state an earlier one left behind, so selecting a
# scenario without its predecessors can fail for reasons that have nothing to do
# with the code under test. A selective run is therefore a development tool, not
# proof: green here does not imply green in a full run, and any surprising pass
# or failure deserves a full run before you believe it.
#
# Prerequisites: docker, cargo, hurl ≥ 4.0

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
API_DIR="$REPO_ROOT/tests/api"

# Shared cargo-build wrapper (quiet on success, tailable while it runs).
# shellcheck source=../common/cargo_build.sh
source "$COMMON/cargo_build.sh"

# test.env is the single source of truth for connection details and credentials.
# shellcheck source=test.env
source "$API_DIR/test.env"

# Derive server port from base_url (e.g. http://localhost:8087 → 8087)
SERVER_PORT="${base_url##*:}"

# ── Helpers ───────────────────────────────────────────────────────────────────

log()  { echo "[api-test] $*"; }
die()  { echo "[api-test] ERROR: $*" >&2; exit 1; }

# ── Scenario selection ────────────────────────────────────────────────────────

SELECTORS=()
LIST_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --list) LIST_ONLY=1 ;;
    -h|--help)
      sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    -*) die "unknown option '$arg' (try --help)" ;;
    *)  SELECTORS+=("$arg") ;;
  esac
done

# Phases invoked outside the hurl file list — the helper binaries and the
# shell-driven checks. Needed up here so `--list` and the selector validation
# below know about names that are not backed by a `$API_DIR/...` file: `dpop`
# builds `target/…/dpop-hurl-helper` and appears nowhere else in this script.
SELECTABLE_PHASES=(opaque dpop)

# The set of names a selector may match.
#
# Scraped from this script rather than from a hand-kept list, so it cannot drift
# from what actually runs, and from `$API_DIR/...` references rather than `ls`,
# so a scenario file that is present but not wired in is not advertised as
# runnable. `_diag` helpers are excluded: they are failure diagnostics invoked by
# another scenario, not scenarios themselves.
scenario_names() {
  {
    grep -oE '\$API_DIR/[a-z0-9_-]+\.(hurl|sh)' "$0" \
      | sed 's|.*/||; s|\.hurl$||; s|\.sh$||' \
      | grep -v '_diag$'
    printf '%s\n' "${SELECTABLE_PHASES[@]}"
  } | sort -u
}

# Answered before anything is started — a listing that needs docker, postgres
# and a server build is not a listing.
if (( LIST_ONLY )); then
  echo "Selectable scenarios (substring match):"
  scenario_names | sed 's/^/  /'
  exit 0
fi

# Validate selectors up front, for the same reason: a typo should cost a second,
# not a docker spin-up plus a cargo build. A mistyped name that produced a green
# run would be the worst outcome available here, so this dies rather than warns.
if (( ${#SELECTORS[@]} > 0 )); then
  AVAILABLE="$(scenario_names)"
  for sel in "${SELECTORS[@]}"; do
    # -F because `selected` does substring matching, not regex — validating with
    # regex semantics would accept selectors that then match nothing.
    printf '%s\n' "$AVAILABLE" | grep -qF -- "$sel" \
      || die "no scenario matches '$sel' — run with --list to see the names"
  done
fi

# True when a scenario should run: everything when no selector was given,
# otherwise a substring match on its name.
selected() {
  (( ${#SELECTORS[@]} == 0 )) && return 0
  local name="$1" sel
  for sel in "${SELECTORS[@]}"; do
    [[ "$name" == *"$sel"* ]] && return 0
  done
  return 1
}

# Filter a list of paths down to the selected ones. setup.hurl survives every
# filter — see the header.
filter_scenarios() {
  local f base
  for f in "$@"; do
    base="$(basename "$f")"
    base="${base%.hurl}"
    base="${base%.sh}"
    if [[ "$base" == "setup" ]] || selected "$base"; then
      printf '%s\n' "$f"
    fi
  done
}

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
#
# Build locally on every run, but NOT in CI.
#
# Locally the old `[[ ! -x $BIN ]]` guard reused an existing binary however
# stale, so editing server code and re-running the suite silently tested the
# OLD build — a green run that proves nothing. Cargo is the right thing to
# ask "is this current?"; file existence is not, and an unchanged tree costs
# a no-op build of a second or two.
#
# CI is the opposite case and must be left alone. The `api-test` workflow job
# declares `needs: build` and downloads the release binary as an ARTIFACT into
# `target/release/`, with no cargo cache and no `target/` tree — so a
# `cargo build --release` here would be a full COLD release build of several
# minutes, once per test job, defeating the shared build job entirely.
# `$CI` is set by GitHub Actions (and by every other CI worth naming).
#
# `SKIP_BUILD=1` forces the same skip locally, for iterating on the test
# scripts themselves where the server has not changed.
if [[ ! -x "$OXICLOUD_BIN" ]]; then
  NEED_BUILD=1   # nothing to run at all
elif [[ "${SKIP_BUILD:-0}" == "1" ]]; then
  NEED_BUILD=0
elif [[ -n "${CI:-}" ]]; then
  NEED_BUILD=0   # a pre-built artifact is the contract; see above
else
  NEED_BUILD=1
fi

if (( NEED_BUILD )); then
  # Wording matters now that this runs every time: nothing here cleans, and on
  # an unchanged tree it is a no-op of a second or two. Only a cold target/
  # takes minutes.
  log "Building OxiCloud server ($BUILD_TARGET) — incremental; minutes only on a cold target/..."
  # Cargo's debug profile is the implicit default (`cargo build` alone)
  # — there is NO `--profile debug` flag (it would error). Only the
  # release path needs an explicit flag.
  case "$BUILD_TARGET" in
    debug)   cargo_build_logged "OxiCloud server (debug)"             || die "cargo build failed" ;;
    release) cargo_build_logged "OxiCloud server (release)" --release || die "cargo build --release failed" ;;
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
# An array rather than inline arguments so `--list` and the selectors can
# filter it. The backslash continuations and the `# comment` entries below are
# unchanged and still work: a backslash-newline is a line continuation inside
# `( )` too, and an unquoted empty command substitution contributes no word.
HURL_FILES=( \
  "$API_DIR/setup.hurl" \
  "$API_DIR/auth_login.hurl" \
  "$API_DIR/opaque_substrate.hurl" \
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
  "$API_DIR/refcount_same_content_rewrite.hurl" \
  "$API_DIR/derived_blob_copy.hurl" \
  "$API_DIR/thumbnail_etag_content_keyed.hurl" \
  "$API_DIR/attached_thumbnail_copy.hurl" \
  "$API_DIR/transcode_cache.hurl" \
  "$API_DIR/cache_header.hurl" \
  "$API_DIR/transcode_import.hurl" \
  "$API_DIR/dedup_admin_gate.hurl" \
  "$API_DIR/admin_jobs.hurl" \
  "$API_DIR/recoverable_jobs.hurl" \
  "$API_DIR/jobs_notify.hurl" \
  "$API_DIR/storage_multi_entry.hurl" \
  "$API_DIR/default_caldav_carddav.hurl" \
  "$API_DIR/dav_error_mapping.hurl" \
  "$API_DIR/carddav_vcard_properties.hurl" \
  "$API_DIR/contacts.hurl" \
  "$API_DIR/calendar.hurl" \
  "$API_DIR/caldav_shared_roles.hurl" \
  "$API_DIR/caldav_recurring.hurl" \
  "$API_DIR/caldav_calendar_query.hurl" \
  "$API_DIR/playlists.hurl" \
  "$API_DIR/public_shares.hurl" \
  "$API_DIR/anonymous_share_session.hurl" \
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
  "$API_DIR/admin_role_hierarchy.hurl" \
  "$API_DIR/external_mounts.hurl" \
  "$API_DIR/chunked_upload_cap.hurl" \
  "$API_DIR/nc_auth_failures.hurl" \
  "$API_DIR/dedup_create.hurl" \
  "$API_DIR/trash_per_drive.hurl" \
  "$API_DIR/drive_quota.hurl" \
  "$API_DIR/user_envelope_quota.hurl" \
  "$API_DIR/regression_595_unlimited_user_quota.hurl" \
  "$API_DIR/drive_policies.hurl" \
  "$API_DIR/drive_policy_defaults.hurl" \
  "$API_DIR/drive_policy_repair.hurl" \
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
  "$API_DIR/nfc_normalization.hurl" \
  "$API_DIR/wopi_authz.hurl" \
  "$API_DIR/wopi_shared_drive.hurl" \
  `# Second-to-last. The slowest file in the suite BY DESIGN: it waits` \
  `# out an unreachable endpoint (~31s) to prove the wait is bounded, so` \
  `# that cost belongs at the end rather than in the middle. It leaves no` \
  `# read-only freeze behind — the migration fails at target init, before` \
  `# the gate is engaged — and cancels its own run, so the shared DB is` \
  `# clean for whatever follows.` \
  "$API_DIR/backend_migration_blackhole.hurl" \
  `# LAST, deliberately — and kept last even though it no longer cuts` \
  `# the storage pointer over. It is the only scenario that depends on a` \
  `# second service (Azurite on 10000), so if that container is missing` \
  `# or wedged the failure lands after everything else has reported,` \
  `# rather than in the middle of an otherwise-green run. It is also` \
  `# where a cutover comes back once the official Azure SDK lands (see` \
  `# the file header), and that WILL need to be last.` \
  "$API_DIR/backend_consistency_azure.hurl"
)

# The shell-driven scenarios, in the order they must run. Listed here purely so
# `--list` can show them and selectors can pick them; each is still invoked at
# its own point below, because several depend on where they sit relative to the
# cleanup sweep.
SHELL_CHECKS=(
  "$API_DIR/thumb_import_check.sh"
  "$API_DIR/rechunk_legacy_check.sh"
  "$API_DIR/storage_cleanup_check.sh"
  "$API_DIR/rt_bus_check.sh"
)

if (( LIST_ONLY )); then
  echo "Selectable scenarios (substring match):"
  for f in "${HURL_FILES[@]}" "${SHELL_CHECKS[@]}" "$API_DIR/refcount_cascade.hurl"; do
    b="$(basename "$f")"; b="${b%.hurl}"; b="${b%.sh}"
    echo "  $b"
  done | sort -u
  exit 0
fi

# A read loop rather than `mapfile`/`readarray`: those are bash 4+, and macOS
# still ships bash 3.2 as /bin/bash — which `#!/usr/bin/env bash` picks up for
# anyone who has not put a newer bash first on PATH. Process substitution is
# fine in 3.2; the array builtins are not.
SELECTED_HURL=()
while IFS= read -r scenario_path; do
  [[ -n "$scenario_path" ]] && SELECTED_HURL+=("$scenario_path")
done < <(filter_scenarios "${HURL_FILES[@]}")
if (( ${#SELECTORS[@]} > 0 )); then
  log "selective run: ${SELECTORS[*]} → $(( ${#SELECTED_HURL[@]} - 1 )) hurl scenario(s) of $(( ${#HURL_FILES[@]} - 1 )), plus any matching phase"
fi

# ALWAYS run the hurl phase, because SELECTED_HURL always contains setup.hurl
# and that seeds the admin account every later scenario — including the shell
# checks — logs in with. Skipping it when only a shell check was selected made
# `run.sh rechunk_legacy_check` fail with "no such user 'admin'" from the check's
# own login, which looks like a broken check rather than a missing precondition.
hurl --variables-file "$API_DIR/test.env" --file-root "$REPO_ROOT/tests" --test --jobs 1 \
  "${SELECTED_HURL[@]}"

#bash "$API_DIR/dedup_bulk_upload.sh"

# ── 4b. copy-folder ref_count regression — dedicated block ──────────────
# Ref_count mismatches surface as "expected 1 got 2" at hurl-assert
# level, which doesn't tell you WHICH half of the invariant broke
# (cascade delete missed rows vs decrement hook didn't fire). The
# `_diag.sh` script inspects `storage.blobs.ref_count` and the
# auditor's `actual_ref_count` formula on the two fixture hashes to
# pin the mode. Extracted from the main hurl array so `set -e`
# doesn't skip the diagnostic on failure — the `if !` guard runs
# the diag first, THEN exits with hurl's failure code so CI still
# reports the regression.
if selected refcount_cascade; then
  if ! hurl --variables-file "$API_DIR/test.env" --file-root "$REPO_ROOT/tests" --test --jobs 1 \
       "$API_DIR/refcount_cascade.hurl"; then
    bash "$API_DIR/refcount_cascade_diag.sh" || true
    exit 1
  fi
fi

# Migration checks run BEFORE the cleanup sweep, which deletes everything
# they would otherwise need.
selected thumb_import_check && bash "$API_DIR/thumb_import_check.sh"

# `backend_rechunk`: manufactures a pre-CDC whole-file blob (which no API can
# create on a current build) and asserts the job converts it to real chunks
# with the content intact.
selected rechunk_legacy_check && bash "$API_DIR/rechunk_legacy_check.sh"

selected storage_cleanup_check && bash "$API_DIR/storage_cleanup_check.sh"

# ── 5. Message bus — WebSocket smoke test ───────────────────────────────
# Runs BEFORE the OPAQUE helper so its user registration + login uses
# the legacy password path (opaque_substrate.hurl migrates the admin
# account, but by running first this check is unaffected by whatever
# order later scenarios touch the auth substrate). Four scenarios:
# positive delivery, topic isolation, AuthZ denial on subscribe,
# anti-enumeration parity. See `tests/api/rt_bus_check.sh` and
# `docs/plan/message-bus.md`.
if selected rt_bus_check; then
  log "Running message-bus smoke test..."
  BUILD_TARGET="$BUILD_TARGET" bash "$REPO_ROOT/tests/api/rt_bus_check.sh" \
    || die "message-bus smoke test failed"
fi

# ── 6. OPAQUE crypto handshake — the parts Hurl can't drive ─────────────
# Full OPAQUE register + login handshake against the running server,
# using the real ciphersuite client-side. Closes the gap left by
# `opaque_substrate.hurl` (which covers only wire shape, not OPRF-
# blinded happy path). See `src/bin/opaque-hurl-helper.rs` for what
# it exercises and why. Skips itself when the server reports OPAQUE
# disabled so an operator running the suite with mode=off doesn't
# get a spurious failure.
# Gated for the same reason as the DPoP phase below — it builds its own binary.
# Selector name: `opaque`.
if selected opaque; then
OPAQUE_HELPER_BIN="$REPO_ROOT/target/$BUILD_TARGET/opaque-hurl-helper"
if [[ ! -x "$OPAQUE_HELPER_BIN" ]]; then
  case "$BUILD_TARGET" in
    debug)   cargo_build_logged "opaque-hurl-helper (debug)"             --features test_utils --bin opaque-hurl-helper || die "opaque-hurl-helper build failed" ;;
    release) cargo_build_logged "opaque-hurl-helper (release)" --release --features test_utils --bin opaque-hurl-helper || die "opaque-hurl-helper build failed" ;;
  esac
fi
log "Running OPAQUE crypto handshake helper..."
OPAQUE_HELPER_BASE_URL="$base_url" \
OPAQUE_HELPER_USERNAME="$username" \
OPAQUE_HELPER_PASSWORD="$password" \
  "$OPAQUE_HELPER_BIN" || die "OPAQUE crypto handshake failed"
fi

# ── 7. DPoP wire protocol — the parts Hurl can't drive ──────────────────
# Each proof carries a fresh jti, current iat, htm/htu matching the
# exact request, an ES256 signature, and a threaded nonce — none of
# which a declarative .hurl template can compute. See
# `src/bin/dpop-hurl-helper.rs` for the scenario matrix (happy path,
# wrong htm/htu/alg/typ, stale nonce, replay, malformed, fail-open
# when the session is unbound). Runs against the SAME server target
# the OPAQUE helper used — but the server config must set
# `OXICLOUD_DPOP_MODE=opportunistic` (or `required`) or the middleware
# is a pass-through and every failure scenario silently 200s.
# Gated like the other phases, and worth gating specifically because it builds
# its own binary — on a selective run that cargo build dwarfs the scenario you
# are actually iterating on. Selector name: `dpop`.
if selected dpop; then
DPOP_HELPER_BIN="$REPO_ROOT/target/$BUILD_TARGET/dpop-hurl-helper"
if [[ ! -x "$DPOP_HELPER_BIN" ]]; then
  case "$BUILD_TARGET" in
    debug)   cargo_build_logged "dpop-hurl-helper (debug)"             --features test_utils --bin dpop-hurl-helper || die "dpop-hurl-helper build failed" ;;
    release) cargo_build_logged "dpop-hurl-helper (release)" --release --features test_utils --bin dpop-hurl-helper || die "dpop-hurl-helper build failed" ;;
  esac
fi
log "Running DPoP wire-protocol helper..."
DPOP_HELPER_BASE_URL="$base_url" \
DPOP_HELPER_USERNAME="$username" \
DPOP_HELPER_PASSWORD="$password" \
  "$DPOP_HELPER_BIN" || die "DPoP wire-protocol test failed"
fi

if (( ${#SELECTORS[@]} > 0 )); then
  log "Selected scenarios passed (${SELECTORS[*]}). This was a SELECTIVE run — \
run the full suite before treating it as green."
else
  log "All tests passed."
fi
