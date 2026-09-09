#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Realtime bus smoke test — the parts Hurl can't drive.
#
# Hurl is HTTP-only and cannot open a WebSocket, so the WS half of the test
# runs through `rt-hurl-helper` (a small Rust bin gated on `test_utils`).
# This script orchestrates it against a live oxicloud server: bootstraps
# state with curl, exercises the bus, asserts on the helper's JSON output.
#
# Four scenarios:
#   S1  Positive delivery       — subscribe to folder A, upload into A, see event.
#   S2  Topic isolation         — subscribe to folder A only, upload into B and
#                                 then A; must see A's event only.
#   S3  AuthZ denial            — user2 subscribes to folder A owned by user1
#                                 without a grant; expect wire reason `no_read`.
#   S4  Anti-enumeration        — subscribe to a folder that does not exist;
#                                 must return the SAME wire reason (`no_read`)
#                                 as S3, per the plan's anti-enum invariant.
#
# Exit non-zero on any failure — run.sh treats that as a suite failure.
# ─────────────────────────────────────────────────────────────────────────────

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_TARGET="${BUILD_TARGET:-debug}"
HELPER_BIN="$REPO_ROOT/target/$BUILD_TARGET/rt-hurl-helper"

# ── Env from test.env (base_url, admin username/password) ────────────────────
# shellcheck disable=SC1091
source "$SCRIPT_DIR/test.env"
: "${base_url:?}" "${username:?}" "${password:?}"

# WS URL derived from base_url (test.env uses http://); tolerate https for
# future deployments even though the test suite runs plain HTTP.
case "$base_url" in
  http://*)   ws_url="ws://${base_url#http://}/api/rt/ws" ;;
  https://*)  ws_url="wss://${base_url#https://}/api/rt/ws" ;;
  *)          echo "rt_bus_check: unexpected base_url scheme: $base_url" >&2; exit 2 ;;
esac

log()  { printf '\033[1;36m[rt_bus_check]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[rt_bus_check FAIL]\033[0m %s\n' "$*" >&2; exit 1; }

# ── Build the helper on demand (matches opaque/dpop helper convention) ──────
if [[ ! -x "$HELPER_BIN" ]]; then
  log "Building rt-hurl-helper ($BUILD_TARGET)..."
  case "$BUILD_TARGET" in
    debug)   (cd "$REPO_ROOT" && cargo build           --features test_utils --bin rt-hurl-helper 2>&1 | tail -n 20) || die "rt-hurl-helper build failed" ;;
    release) (cd "$REPO_ROOT" && cargo build --release --features test_utils --bin rt-hurl-helper 2>&1 | tail -n 20) || die "rt-hurl-helper build failed" ;;
  esac
fi

# ── curl wrappers ───────────────────────────────────────────────────────────
c_post() {
  local url="$1" auth="$2" body="$3"
  curl -sS -X POST -H "Content-Type: application/json" \
    ${auth:+-H "Authorization: Bearer $auth"} \
    -d "$body" "$url"
}

c_get() {
  local url="$1" auth="$2"
  curl -sS -H "Accept: application/json" \
    ${auth:+-H "Authorization: Bearer $auth"} \
    "$url"
}

# ── Setup: register fresh users; the test.env admin may be OPAQUE-
# migrated and the legacy password-login path refuses those accounts,
# so we don't use it at all — same pattern as `dedup_admin_gate.hurl`
# and the other hurl scenarios that need a self-contained principal.
# The scenarios only need "a user with their own folders", not admin
# rights.
suffix="$(date +%s)_$$"
user1_name="rtbus_u1_$suffix"
user1_pass="RtBusU1Pass1!"
user2_name="rtbus_u2_$suffix"
user2_pass="RtBusU2Pass1!"

log "Register user1 ($user1_name) and log in..."
c_post "$base_url/api/auth/register" "" \
  "$(printf '{"username":"%s","email":"%s@example.com","password":"%s"}' \
       "$user1_name" "$user1_name" "$user1_pass")" > /dev/null
u1_login=$(c_post "$base_url/api/auth/login" "" \
  "$(printf '{"username":"%s","password":"%s"}' "$user1_name" "$user1_pass")")
user1_token=$(printf '%s' "$u1_login" | jq -r '.access_token')
[[ -n "$user1_token" && "$user1_token" != "null" ]] || die "no user1 token: $u1_login"

log "Discover user1 root folder..."
folders=$(c_get "$base_url/api/folders" "$user1_token")
root_id=$(printf '%s' "$folders" | jq -r '.[0].id')
[[ -n "$root_id" && "$root_id" != "null" ]] || die "no root folder for user1: $folders"

log "Create folder A (rt_bus_A_$suffix) and folder B (rt_bus_B_$suffix)..."
folder_a=$(c_post "$base_url/api/folders" "$user1_token" \
  "$(printf '{"name":"rt_bus_A_%s","parent_id":"%s"}' "$suffix" "$root_id")" | jq -r '.id')
folder_b=$(c_post "$base_url/api/folders" "$user1_token" \
  "$(printf '{"name":"rt_bus_B_%s","parent_id":"%s"}' "$suffix" "$root_id")" | jq -r '.id')
[[ -n "$folder_a" && "$folder_a" != "null" ]] || die "folder A creation failed"
[[ -n "$folder_b" && "$folder_b" != "null" ]] || die "folder B creation failed"

# Register a second user for the AuthZ-denial scenario (S3). No grant on
# folder A → subscribe attempt must be denied.
log "Register user2 ($user2_name) and log in..."
c_post "$base_url/api/auth/register" "" \
  "$(printf '{"username":"%s","email":"%s@example.com","password":"%s"}' \
       "$user2_name" "$user2_name" "$user2_pass")" > /dev/null
user2_login=$(c_post "$base_url/api/auth/login" "" \
  "$(printf '{"username":"%s","password":"%s"}' "$user2_name" "$user2_pass")")
user2_token=$(printf '%s' "$user2_login" | jq -r '.access_token')
[[ -n "$user2_token" && "$user2_token" != "null" ]] || die "no user2 token: $user2_login"

# ── Helper: create a small file inside a folder via the byte-upload path.
# Not delta / instant-upload; keeps the wire simple and hits the same
# `upload_file_streaming` publish hook.
mkfile_in() {
  local folder_id="$1" name="$2" token="$3"
  local tmpfile
  tmpfile="$(mktemp -t rtbus_body.XXXXXX)"
  printf 'rt-bus-test-payload' > "$tmpfile"
  # Multipart-upload path used by the frontend for byte uploads.
  # NOTE: `folder_id` MUST come BEFORE the `file` part — file_handler.rs
  # streams the parts in order and the fail-fast folder-required check
  # fires the moment it sees the file bytes; a folder_id sent after the
  # file arrives too late (returns 400 "folder_id is required").
  curl -sS -X POST \
    -H "Authorization: Bearer $token" \
    -F "folder_id=$folder_id" \
    -F "file=@$tmpfile;filename=$name" \
    "$base_url/api/files/upload" > /dev/null
  rm -f "$tmpfile"
}

# ── Scenario 1 — Positive delivery ──────────────────────────────────────────
log "S1: subscribe to folder A, upload into A, expect one file_created event."
out_s1="$(mktemp -t rtbus_s1.XXXXXX)"
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --token "$user1_token" \
  --subscribe "folder:$folder_a" \
  --expect-events 1 \
  --timeout 5s \
  --output "$out_s1" &
helper_pid=$!
# Give the ack a moment to install so the upload's post-commit publish
# lands on a live receiver, not an orphaned map entry.
sleep 0.4
mkfile_in "$folder_a" "s1.txt" "$user1_token"
if ! wait "$helper_pid"; then
  cat "$out_s1" >&2 || true
  die "S1: helper did not observe the expected event"
fi
# jq assertions — one event, correct parent_id, correct discriminator.
[[ "$(jq -r '.events | length' "$out_s1")" == "1" ]] \
  || { cat "$out_s1"; die "S1: expected 1 event, got $(jq -r '.events | length' "$out_s1")"; }
[[ "$(jq -r '.events[0].event' "$out_s1")" == "file_created" ]] \
  || die "S1: wrong event discriminator: $(jq -r '.events[0].event' "$out_s1")"
[[ "$(jq -r '.events[0].data.parent_id' "$out_s1")" == "$folder_a" ]] \
  || die "S1: parent_id mismatch"
log "S1 OK"

# ── Scenario 2 — Topic isolation ────────────────────────────────────────────
log "S2: subscribe to folder A, upload into B (must be silent) and A (triggers exit)."
out_s2="$(mktemp -t rtbus_s2.XXXXXX)"
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --token "$user1_token" \
  --subscribe "folder:$folder_a" \
  --expect-events 1 \
  --timeout 5s \
  --output "$out_s2" &
helper_pid=$!
sleep 0.4
# B first — should be dropped for the A subscriber.
mkfile_in "$folder_b" "s2_in_B.txt" "$user1_token"
# Small settle so if isolation is BROKEN, the B event has time to arrive
# before A's; the assertion below then catches it as a wrong parent_id.
sleep 0.2
mkfile_in "$folder_a" "s2_in_A.txt" "$user1_token"
if ! wait "$helper_pid"; then
  cat "$out_s2" >&2 || true
  die "S2: helper did not observe the expected A event"
fi
# Exactly one event, and it MUST be from folder A. If isolation were
# broken, we'd either see 2 events or a B-parented event first.
[[ "$(jq -r '.events | length' "$out_s2")" == "1" ]] \
  || { cat "$out_s2"; die "S2: expected 1 event, got $(jq -r '.events | length' "$out_s2") (isolation broken?)"; }
[[ "$(jq -r '.events[0].data.parent_id' "$out_s2")" == "$folder_a" ]] \
  || die "S2: parent_id was $(jq -r '.events[0].data.parent_id' "$out_s2"), expected $folder_a"
log "S2 OK"

# ── Scenario 3 — AuthZ denial ───────────────────────────────────────────────
log "S3: user2 subscribes to folder A (no grant); expect no_read denial."
if ! "$HELPER_BIN" expect-denied \
     --url "$ws_url" \
     --token "$user2_token" \
     --subscribe "folder:$folder_a" \
     --reason no_read \
     --timeout 3s; then
  die "S3: user2 was NOT denied on folder A (AuthZ gate broken?)"
fi
log "S3 OK"

# ── Scenario 4 — Anti-enumeration parity ────────────────────────────────────
log "S4: subscribe to a nonexistent folder; wire reason must equal S3."
fake_folder="00000000-0000-0000-0000-000000000000"
if ! "$HELPER_BIN" expect-denied \
     --url "$ws_url" \
     --token "$user1_token" \
     --subscribe "folder:$fake_folder" \
     --reason no_read \
     --timeout 3s; then
  die "S4: nonexistent folder did not collapse to no_read (anti-enum invariant broken)"
fi
log "S4 OK"

log "All four realtime-bus scenarios passed."
