#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Message bus smoke test — the parts Hurl can't drive.
#
# Hurl is HTTP-only and cannot open a WebSocket, so the WS half of the test
# runs through `rt-hurl-helper` (a small Rust bin gated on `test_utils`).
# This script orchestrates it against a live oxicloud server: bootstraps
# state with curl, exercises the bus, asserts on the helper's JSON output.
#
# Nine scenarios:
#   S1  Positive delivery       — subscribe to folder A, upload into A, see event.
#   S2  Topic isolation         — subscribe to folder A only, upload into B and
#                                 then A; must see A's event only.
#   S3  AuthZ denial            — user2 subscribes to folder A owned by user1
#                                 without a grant; expect wire reason `no_read`.
#   S4  Anti-enumeration        — subscribe to a folder that does not exist;
#                                 must return the SAME wire reason (`no_read`)
#                                 as S3, per the plan's anti-enum invariant.
#   S5  Server keepalive        — 3 s of idle surfaces multiple RFC 6455 Ping
#                                 frames from the server (proves the interval
#                                 fires), and the session still delivers an
#                                 event on the same subscription afterwards.
#   S6  Delete emits            — DELETE a pre-uploaded file → subscriber sees
#                                 one `file_deleted` event with correct
#                                 `file_id` + `parent_id` (snapshotted
#                                 pre-delete since the row is gone by then).
#   S7  Move fan-out            — MOVE A→B while subscribed to BOTH topics on
#                                 one session → observe TWO `file_moved`
#                                 events (one via the A topic, one via B).
#                                 Same file_id/from/to on both.
#   S8  Grant-revoke eviction   — user2 subscribes to A + B (both granted),
#                                 user1 revokes only A → user2 sees
#                                 `rt.revoked` for folder:A AND an event
#                                 on folder:B (upload after revoke).
#                                 Locks in three invariants: eviction
#                                 fires, scoping is per-topic, session
#                                 survives.
#   S9  Cross-user identity     — user1 tries to subscribe to
#                                 `user:{user2_id}:authz` (an identity-scoped
#                                 topic that resolves to somebody else). Server
#                                 must reject with `topic_forbidden` (same wire
#                                 shape as an unknown topic — anti-enumeration).
#                                 Guards the strict-privacy Class-2 AuthZ gate:
#                                 no admin bypass, direct UUID equality only.
#   S10 Ticket happy path       — user1 POSTs `/api/rt/ticket`, receives a
#                                 short-lived opaque token, opens the WS with
#                                 `Sec-WebSocket-Protocol: oxi.ticket.<uuid>`
#                                 and successfully subscribes + delivers an
#                                 event. Exercises the ticket path — the only
#                                 path a DPoP-required browser can take.
#   S11 Ticket single-use       — a ticket redeemed once cannot be redeemed
#                                 again. Guards replay: a captured token
#                                 outside its 30 s TTL, or one already
#                                 consumed, MUST fail the upgrade with 401.
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

# ── Rebuild the helper every run ────────────────────────────────────────────
# Deliberately unconditional — the previous `[[ ! -x $HELPER_BIN ]]` guard
# silently reused a stale binary whenever the helper's source changed
# without touching the caller shell script, producing "unknown flag"
# exits that looked like test bugs (see the S10/S11 --ticket rollout).
# Cargo incremental short-circuits in ~50 ms when nothing changed, so
# the cost of the always-build is negligible; the cost of a stale binary
# is a wild-goose chase.
log "Building rt-hurl-helper ($BUILD_TARGET)..."
case "$BUILD_TARGET" in
  debug)   (cd "$REPO_ROOT" && cargo build           --features test_utils --bin rt-hurl-helper 2>&1 | tail -n 20) || die "rt-hurl-helper build failed" ;;
  release) (cd "$REPO_ROOT" && cargo build --release --features test_utils --bin rt-hurl-helper 2>&1 | tail -n 20) || die "rt-hurl-helper build failed" ;;
esac

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

# Block until the helper writes `--ready-file <path>` (touched the
# moment every requested subscribe is ack'd) or `$timeout` seconds
# elapse. Replaces the older `sleep 0.4` heuristic that flaked on
# cold-cache runs where the helper's fork/tokio-init/connect chain
# crossed 400 ms and the shell's mkfile_in publish arrived at an
# empty topic. See `Args::ready_file` in rt-hurl-helper.rs.
wait_ready() {
  local path="$1" timeout="${2:-5}"
  local waited=0
  while [[ ! -f "$path" && "$waited" -lt "$((timeout * 20))" ]]; do
    sleep 0.05
    waited=$((waited + 1))
  done
  [[ -f "$path" ]] || die "wait_ready: $path never appeared within ${timeout}s (subscribe likely never ack'd)"
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
# S8 needs user2's UUID to target them as the grant subject.
user2_id=$(printf '%s' "$user2_login" | jq -r '.user.full.user.id')
[[ -n "$user2_id" && "$user2_id" != "null" ]] || die "no user2 id: $user2_login"

# ── Helper: create a small file inside a folder via the byte-upload path.
# Not delta / instant-upload; keeps the wire simple and hits the same
# `upload_file_streaming` publish hook.
mkfile_in() {
  local folder_id="$1" name="$2" token="$3"
  local tmpfile respfile status
  tmpfile="$(mktemp -t rtbus_body.XXXXXX)"
  respfile="$(mktemp -t rtbus_resp.XXXXXX)"
  printf 'rt-bus-test-payload' > "$tmpfile"
  # Multipart-upload path used by the frontend for byte uploads.
  # NOTE: `folder_id` MUST come BEFORE the `file` part — file_handler.rs
  # streams the parts in order and the fail-fast folder-required check
  # fires the moment it sees the file bytes; a folder_id sent after the
  # file arrives too late (returns 400 "folder_id is required").
  #
  # Capture body + status so a silent 4xx doesn't look like a timing
  # bug in the bus. A stale server binary that lost the publish hook,
  # or a schema change that broke the endpoint, would otherwise
  # present as "subscribe works, no event, timeout" — exactly the
  # shape of a real regression but a completely different root cause.
  status=$(curl -sS -o "$respfile" -w "%{http_code}" -X POST \
    -H "Authorization: Bearer $token" \
    -F "folder_id=$folder_id" \
    -F "file=@$tmpfile;filename=$name" \
    "$base_url/api/files/upload")
  rm -f "$tmpfile"
  if [[ "$status" -lt 200 || "$status" -ge 300 ]]; then
    printf 'mkfile_in FAIL: HTTP %s\nbody: %s\n' "$status" "$(cat "$respfile")" >&2
    rm -f "$respfile"
    return 1
  fi
  rm -f "$respfile"
}

# ── Scenario 1 — Positive delivery ──────────────────────────────────────────
log "S1: subscribe to folder A, upload into A, expect one file_created event."
out_s1="$(mktemp -t rtbus_s1.XXXXXX)"
ready_s1="$(mktemp -t rtbus_s1_ready.XXXXXX)"
rm -f "$ready_s1"  # mktemp creates it; ready-file semantics need "appears when subscribed"
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --token "$user1_token" \
  --subscribe "folder:$folder_a" \
  --expect-events 1 \
  --timeout 5s \
  --ready-file "$ready_s1" \
  --output "$out_s1" &
helper_pid=$!
# Block on the helper's ready-file signal, not a wall-clock sleep —
# see wait_ready doc. Closes the "publish before subscribe installed"
# race that flaked S1 on cold-cache runs.
wait_ready "$ready_s1"
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
ready_s2="$(mktemp -t rtbus_s2_ready.XXXXXX)"; rm -f "$ready_s2"
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --token "$user1_token" \
  --subscribe "folder:$folder_a" \
  --expect-events 1 \
  --timeout 5s \
  --ready-file "$ready_s2" \
  --output "$out_s2" &
helper_pid=$!
wait_ready "$ready_s2"
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

# ── Scenario 5 — Server-initiated keepalive ─────────────────────────────────
# Verifies the WS handler sends RFC 6455 Ping control frames on the
# `OXICLOUD_RT_WS_KEEPALIVE_SECONDS` cadence (1 s in tests/common/server.env).
# Two invariants:
#   (a) idling on a live subscription surfaces multiple Ping frames — the
#       keepalive interval genuinely fires, not just at connect and never again.
#   (b) after 3 s of app-layer idle + keepalive traffic, the session is
#       still healthy: an upload's event still delivers cleanly.
# If the keepalive impl were broken (missed-tick burst, dead select! arm,
# stalled write on the socket), either (a) trips (0-1 pings observed) or
# (b) trips (event never arrives after idle).
log "S5: server-initiated keepalive fires on idle; session still delivers."
out_s5="$(mktemp -t rtbus_s5.XXXXXX)"
ready_s5="$(mktemp -t rtbus_s5_ready.XXXXXX)"; rm -f "$ready_s5"
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --token "$user1_token" \
  --subscribe "folder:$folder_a" \
  --expect-events 1 \
  --timeout 6s \
  --ready-file "$ready_s5" \
  --output "$out_s5" &
helper_pid=$!
# Wait for the subscribe to install BEFORE starting the idle window —
# otherwise slow helper startup eats into the 3 s and we observe
# fewer pings than the assertion below tolerates.
wait_ready "$ready_s5"
# 3 s of pure idle — with 1 s keepalive on the server, that's ~3 Pings.
sleep 3
mkfile_in "$folder_a" "s5.txt" "$user1_token"
if ! wait "$helper_pid"; then
  cat "$out_s5" >&2 || true
  die "S5: helper did not observe the expected event after idle"
fi
# (a) At least 2 Pings during the 3 s idle. Tolerant floor: with 1 s
# interval and a first-tick discard, 2 is the minimum credible observation
# before flakiness (missed tick, timer coalesce) becomes a concern.
pings=$(jq -r '.pings_received' "$out_s5")
if [[ "$pings" -lt 2 ]]; then
  cat "$out_s5" >&2 || true
  die "S5: expected >=2 keepalive pings during 3 s idle, got $pings"
fi
# (b) Exactly one event on the folder A subscription, from the post-idle upload.
[[ "$(jq -r '.events | length' "$out_s5")" == "1" ]] \
  || { cat "$out_s5"; die "S5: expected 1 event after idle, got $(jq -r '.events | length' "$out_s5")"; }
[[ "$(jq -r '.events[0].data.parent_id' "$out_s5")" == "$folder_a" ]] \
  || die "S5: parent_id mismatch after idle"
log "S5 OK ($pings pings observed)"

# ── Scenario 6 — File delete emits `file_deleted` ───────────────────────────
# Pre-create a file in folder A, then subscribe to `folder:$folder_a`, then
# DELETE the file. The subscription must observe exactly one
# `file_deleted` event — proves the delete publish hook fires and carries
# the correct `parent_id` (snapshotted pre-delete, since the row is gone
# by publish time).
log "S6: DELETE a file → subscriber observes file_deleted."
# Pre-create the file BEFORE the subscriber goes up, so S6 asserts on the
# delete event alone (S1 already covered the create-side).
s6_upload=$(curl -sS -X POST \
  -H "Authorization: Bearer $user1_token" \
  -F "folder_id=$folder_a" \
  -F "file=@$(mktemp -t rtbus_s6_body.XXXXXX);filename=s6.txt" \
  "$base_url/api/files/upload")
s6_file_id=$(printf '%s' "$s6_upload" | jq -r '.id')
[[ -n "$s6_file_id" && "$s6_file_id" != "null" ]] \
  || die "S6: pre-upload failed: $s6_upload"

out_s6="$(mktemp -t rtbus_s6.XXXXXX)"
ready_s6="$(mktemp -t rtbus_s6_ready.XXXXXX)"; rm -f "$ready_s6"
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --token "$user1_token" \
  --subscribe "folder:$folder_a" \
  --expect-events 1 \
  --timeout 5s \
  --ready-file "$ready_s6" \
  --output "$out_s6" &
helper_pid=$!
wait_ready "$ready_s6"
# `DELETE /api/files/{id}` routes to `delete_and_cleanup_with_perms` —
# the trash-first path. Publish fires on BOTH the trash and the
# permanent-delete branch, so this covers whichever the test hits.
curl -sS -X DELETE \
  -H "Authorization: Bearer $user1_token" \
  "$base_url/api/files/$s6_file_id" > /dev/null
if ! wait "$helper_pid"; then
  cat "$out_s6" >&2 || true
  die "S6: helper did not observe the expected file_deleted event"
fi
[[ "$(jq -r '.events | length' "$out_s6")" == "1" ]] \
  || { cat "$out_s6"; die "S6: expected 1 event, got $(jq -r '.events | length' "$out_s6")"; }
[[ "$(jq -r '.events[0].event' "$out_s6")" == "file_deleted" ]] \
  || die "S6: wrong event: $(jq -r '.events[0].event' "$out_s6")"
[[ "$(jq -r '.events[0].data.file_id' "$out_s6")" == "$s6_file_id" ]] \
  || die "S6: file_id mismatch"
[[ "$(jq -r '.events[0].data.parent_id' "$out_s6")" == "$folder_a" ]] \
  || die "S6: parent_id mismatch"
log "S6 OK"

# ── Scenario 7 — Move fans out on BOTH source and destination ───────────────
# Pre-create a file in folder A, subscribe to BOTH `folder:$folder_a` and
# `folder:$folder_b` on ONE session, then MOVE the file A → B. The single
# session must observe TWO `file_moved` events — one delivered on the A
# topic, one on the B topic. Same file_id in both. Same event contents
# (from=A, to=B). Proves the plan's "fan out on both source AND
# destination" invariant.
# Broken publish (source-only or dest-only) would surface as 1 event.
# Broken publish-after-commit would surface as 0 events.
log "S7: MOVE fans out on both source AND destination folder topics."
s7_upload=$(curl -sS -X POST \
  -H "Authorization: Bearer $user1_token" \
  -F "folder_id=$folder_a" \
  -F "file=@$(mktemp -t rtbus_s7_body.XXXXXX);filename=s7.txt" \
  "$base_url/api/files/upload")
s7_file_id=$(printf '%s' "$s7_upload" | jq -r '.id')
[[ -n "$s7_file_id" && "$s7_file_id" != "null" ]] \
  || die "S7: pre-upload failed: $s7_upload"

out_s7="$(mktemp -t rtbus_s7.XXXXXX)"
ready_s7="$(mktemp -t rtbus_s7_ready.XXXXXX)"; rm -f "$ready_s7"
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --token "$user1_token" \
  --subscribe "folder:$folder_a" \
  --subscribe "folder:$folder_b" \
  --expect-events 2 \
  --timeout 5s \
  --ready-file "$ready_s7" \
  --output "$out_s7" &
helper_pid=$!
wait_ready "$ready_s7"
# `PUT /api/files/{id}/move` — MoveFilePayload = { folder_id: <dest> }.
curl -sS -X PUT \
  -H "Authorization: Bearer $user1_token" \
  -H "Content-Type: application/json" \
  -d "$(printf '{"folder_id":"%s"}' "$folder_b")" \
  "$base_url/api/files/$s7_file_id/move" > /dev/null
if ! wait "$helper_pid"; then
  cat "$out_s7" >&2 || true
  die "S7: helper did not observe 2 file_moved events"
fi
# Both events same shape, same file_id, from = A, to = B.
[[ "$(jq -r '.events | length' "$out_s7")" == "2" ]] \
  || { cat "$out_s7"; die "S7: expected 2 events (fan-out on A + B), got $(jq -r '.events | length' "$out_s7")"; }
# Every event has event=file_moved, correct file_id/from/to.
if ! jq -e --arg fid "$s7_file_id" --arg from "$folder_a" --arg to "$folder_b" \
     '.events | all(.event == "file_moved" and .data.file_id == $fid and .data.from == $from and .data.to == $to)' \
     "$out_s7" > /dev/null; then
  cat "$out_s7"
  die "S7: event contents mismatch (expected file_moved, from=$folder_a, to=$folder_b)"
fi
log "S7 OK"

# ── Scenario 8 — Grant-revocation eviction (scoped, session survives) ───────
# The strong version of "eviction fires": prove that revoking one grant
# affects ONLY the corresponding subscription — the session stays alive,
# unrelated subs keep delivering events, and only the revoked topic
# gets `rt.revoked`.
#
# Setup:
#   - user1 grants user2 `viewer` on folders A AND B (two independent
#     grants; user2 has no prior access).
#   - user2 subscribes to BOTH folders on one WS session.
# Action:
#   - user1 revokes the grant on folder A only.
#   - user1 uploads a file to folder B (the surviving sub).
# Invariants:
#   (a) helper output records exactly ONE `rt.revoked` for `folder:A`
#       with reason `grant_revoked` — the eviction fired.
#   (b) helper output records exactly ONE `file_created` event for
#       `folder:B` — the unrelated sub is still delivering. Regression
#       that mass-drops subs on any AuthzChanged would surface as 0
#       events.
#   (c) `subscribed` contains BOTH folder:A and folder:B — both
#       original subs were installed (regression that failed the initial
#       subscribe under AuthZ would fail here).
#   (d) `timed_out == false` — session actor kept running through the
#       revoke + subsequent event. Regression that killed the whole
#       session on AuthzChanged would surface as a timeout or the
#       helper's `wait` failing.
log "S8: revoke user2's grant on folder A; unrelated sub on B still delivers."
# 8.1 Grant user2 viewer role on folder A + folder B.
grant_a=$(c_post "$base_url/api/grants" "$user1_token" \
  "$(printf '{"subject":{"type":"user","id":"%s"},"resource":{"type":"folder","id":"%s"},"role":"viewer"}' \
       "$user2_id" "$folder_a")")
grant_a_id=$(printf '%s' "$grant_a" | jq -r '.grants[0].id')
[[ -n "$grant_a_id" && "$grant_a_id" != "null" ]] \
  || die "S8: grant on folder A failed: $grant_a"

grant_b=$(c_post "$base_url/api/grants" "$user1_token" \
  "$(printf '{"subject":{"type":"user","id":"%s"},"resource":{"type":"folder","id":"%s"},"role":"viewer"}' \
       "$user2_id" "$folder_b")")
grant_b_id=$(printf '%s' "$grant_b" | jq -r '.grants[0].id')
[[ -n "$grant_b_id" && "$grant_b_id" != "null" ]] \
  || die "S8: grant on folder B failed: $grant_b"

# 8.2 user2 subscribes to BOTH folder topics; --expect-events 1 exits
# when the post-revoke upload lands on the SURVIVING sub.
out_s8="$(mktemp -t rtbus_s8.XXXXXX)"
ready_s8="$(mktemp -t rtbus_s8_ready.XXXXXX)"; rm -f "$ready_s8"
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --token "$user2_token" \
  --subscribe "folder:$folder_a" \
  --subscribe "folder:$folder_b" \
  --expect-events 1 \
  --timeout 6s \
  --ready-file "$ready_s8" \
  --output "$out_s8" &
helper_pid=$!
wait_ready "$ready_s8"  # both subscribes installed before we revoke/upload

# 8.3 user1 revokes only the folder-A grant.
curl -sS -X DELETE \
  -H "Authorization: Bearer $user1_token" \
  "$base_url/api/grants/$grant_a_id" > /dev/null
sleep 0.3  # let AuthzChanged propagate

# 8.4 user1 uploads to folder B → triggers file_created on the
# surviving sub.
mkfile_in "$folder_b" "s8.txt" "$user1_token"

if ! wait "$helper_pid"; then
  cat "$out_s8" >&2 || true
  die "S8: helper did not observe the post-revoke event on folder B"
fi

# 8.5 Assert on the four invariants.
# (a) One rt.revoked for folder:A with reason grant_revoked.
revoked_count=$(jq -r '.revoked | length' "$out_s8")
[[ "$revoked_count" == "1" ]] \
  || { cat "$out_s8"; die "S8: expected 1 rt.revoked, got $revoked_count"; }
[[ "$(jq -r '.revoked[0].topic' "$out_s8")" == "folder:$folder_a" ]] \
  || die "S8: revoked wrong topic: $(jq -r '.revoked[0].topic' "$out_s8")"
[[ "$(jq -r '.revoked[0].reason' "$out_s8")" == "grant_revoked" ]] \
  || die "S8: revoked wrong reason: $(jq -r '.revoked[0].reason' "$out_s8")"

# (b) One file_created for folder:B — surviving sub delivered.
event_count=$(jq -r '.events | length' "$out_s8")
[[ "$event_count" == "1" ]] \
  || { cat "$out_s8"; die "S8: expected 1 event on surviving sub, got $event_count (regression: mass eviction?)"; }
[[ "$(jq -r '.events[0].event' "$out_s8")" == "file_created" ]] \
  || die "S8: wrong event kind on surviving sub"
[[ "$(jq -r '.events[0].data.parent_id' "$out_s8")" == "$folder_b" ]] \
  || die "S8: wrong parent_id on surviving-sub event"

# (c) Both original subs were installed.
sub_count=$(jq -r '.subscribed | length' "$out_s8")
[[ "$sub_count" == "2" ]] \
  || { cat "$out_s8"; die "S8: expected both subs installed, got $sub_count"; }

# (d) Session did not time out — main loop kept running.
[[ "$(jq -r '.timed_out' "$out_s8")" == "false" ]] \
  || die "S8: session timed out (regression: session died on AuthzChanged?)"

log "S8 OK"

# ── Scenario 9 — Cross-user identity topic denial ───────────────────────────
# Identity-scoped topics (`user:{u}:authz`, later `user:{u}:notifications`,
# `user:{u}:sessions`) use a Class-2 AuthZ gate: `caller_id == user_id` by
# direct UUID equality. No admin bypass, no group expansion — privacy is
# absolute. Regression guard: user1 asks for user2's authz stream; server
# MUST reject.
#
# The wire response uses `topic_forbidden` — the SAME error string the
# server returns for a malformed/unknown topic — so an attacker cannot
# distinguish "no such user" from "user exists but not you". `expect-denied
# --reason topic_forbidden` matches on the wire `error.message` string
# emitted by `application/ports/message_bus_ports.rs::error_message`.
#
# If this ever regresses to `no_read` or delivers events, someone changed
# the identity gate (removed the equality check, wired the AuthorizationEngine
# on the identity path, or reused the folder AuthZ dispatch). All three
# would leak user metadata across accounts.
log "S9: user1 subscribes to user:{user2_id}:authz; expect topic_forbidden."
if ! "$HELPER_BIN" expect-denied \
     --url "$ws_url" \
     --token "$user1_token" \
     --subscribe "user:${user2_id}:authz" \
     --reason topic_forbidden \
     --timeout 3s; then
  die "S9: user1 was NOT denied on user2's authz topic (identity gate broken?)"
fi
log "S9 OK"

# ── Scenario 10 — Ticket happy path ─────────────────────────────────────────
# The browser flow: POST /api/rt/ticket under the full middleware stack
# (auth + DPoP proofed), then open the WS with `oxi.ticket.<uuid>` in
# Sec-WebSocket-Protocol. Same delivery guarantees as the bearer path.
# `curl` mints the ticket; `rt-hurl-helper --ticket` redeems it on the
# upgrade.
log "S10: issue rt ticket, open WS with subprotocol, subscribe + deliver."
# c_post takes the raw JWT as its second arg (not the full
# `Authorization:` line); it assembles the header itself.
tkt_resp=$(c_post "$base_url/api/rt/ticket" "$user1_token" "")
ticket=$(printf '%s' "$tkt_resp" | jq -r '.ticket')
[[ -n "$ticket" && "$ticket" != "null" ]] \
  || die "S10: no ticket in POST /api/rt/ticket response: $tkt_resp"
out_s10="$(mktemp -t rtbus_s10.XXXXXX)"
ready_s10="$(mktemp -t rtbus_s10_ready.XXXXXX)"; rm -f "$ready_s10"
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --ticket "$ticket" \
  --subscribe "folder:$folder_a" \
  --expect-events 1 \
  --timeout 3s \
  --ready-file "$ready_s10" \
  --output "$out_s10" &
helper_pid=$!
wait_ready "$ready_s10"
mkfile_in "$folder_a" "s10.txt" "$user1_token"
if ! wait "$helper_pid"; then
  cat "$out_s10" >&2 || true
  die "S10: helper did not observe event on ticket-authenticated WS"
fi
[[ "$(jq -r '.events | length' "$out_s10")" == "1" ]] \
  || { cat "$out_s10"; die "S10: expected 1 event, got $(jq -r '.events | length' "$out_s10")"; }
log "S10 OK"

# ── Scenario 11 — Ticket single-use ─────────────────────────────────────────
# S10 already redeemed the ticket. A second connection with the SAME
# token MUST be refused at the upgrade with 401 (`ticket_invalid`
# audit reason). Proves replay protection — the store removes entries
# on first successful redeem, even if the caller reconnects before
# the 30 s TTL would have expired anyway.
#
# The helper distinguishes "expectation failure" (exit 1 — WS opened
# and then something was off) from "protocol/connect failure" (exit 2
# — connect_ws itself refused). Ticket rejection lands in the second
# bucket, so we assert on exit code 2. Bash's `!` inverter treats any
# non-zero as success, so we capture the exact code.
log "S11: reuse the redeemed ticket, expect upgrade rejected."
set +e
"$HELPER_BIN" subscribe-and-collect \
  --url "$ws_url" \
  --ticket "$ticket" \
  --subscribe "folder:$folder_a" \
  --expect-events 1 \
  --timeout 2s \
  --output /dev/null
reuse_exit=$?
set -e
[[ "$reuse_exit" -eq 2 ]] \
  || die "S11: expected exit 2 (connect refused), got $reuse_exit"
log "S11 OK"

log "All eleven message-bus scenarios passed."
