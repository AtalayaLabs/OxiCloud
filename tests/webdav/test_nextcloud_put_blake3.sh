#!/usr/bin/env bash
# =============================================================
# OxiCloud — NextCloud single-file PUT BLAKE3 round-trip
# =============================================================
# Validates that the NextCloud single-file PUT surface
# (`PUT /remote.php/dav/files/{user}/{path}`) writes byte-exact
# content to blob storage. This is the spool-based streaming
# path in `nextcloud/webdav_handler::handle_put` — it streams
# the request body to a temp file via `spool_body_to_temp`
# (which also computes BLAKE3 on the fly), then promotes the
# blob into `.blobs/{prefix}/{hash}.blob`.
#
# Sister tests:
#   - `test_nextcloud_chunked_upload_cap.sh` — NC chunked PUT
#     (the `/dav/uploads/...` surface, which uses
#     `stream_body_to_path` instead of `spool_body_to_temp`).
#   - `test_dedup_webdav_multichunk.sh` — native `/webdav/...`
#     PUT (different handler, same spool helper).
#
# Together these three pin BLAKE3 correctness on all three of
# OxiCloud's first-class file-write surfaces.
#
# Prerequisites:
#   - Server running at $base_url with admin credentials.
#   - OXICLOUD_ENABLE_AUTH=true, OXICLOUD_NEXTCLOUD_ENABLED=true.
#   - jq, curl in PATH.
# =============================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$SCRIPT_DIR"

source test.env
source common.sh

# ── helpers ──────────────────────────────────────────────────────────────────

PASS=0
FAIL=0

pass() { PASS=$(( PASS + 1 )); echo "  PASS: $*"; }
fail() { FAIL=$(( FAIL + 1 )); echo "  FAIL: $*" >&2; exit 1; }

rest_get()    { curl -s -H "Authorization: Bearer $TOKEN" "$base_url$1"; }
rest_delete() { curl -s -o /dev/null -w "%{http_code}" -X DELETE -H "Authorization: Bearer $TOKEN" "$base_url$1"; }

purge_from_trash() {
    local name="$1"
    local tid
    tid=$(rest_get "/api/trash/resources" \
        | jq -r --arg n "$name" 'first(.items[] | select(.resource.name == $n) | .resource.id) // empty')
    [[ -n "$tid" ]] && rest_delete "/api/trash/$tid" > /dev/null || true
}

# ── fixture ──────────────────────────────────────────────────────────────────

FIXTURE="$REPO_ROOT/tests/fixtures/hello.txt"
[[ -f "$FIXTURE" ]] || { echo "Missing fixture: $FIXTURE" >&2; exit 1; }
EXPECTED_BLAKE3="b2208c5dc33ff951227bd0c139f5eccb04105d6da6a7519ee23f7bc00a17bb5a"
EXPECTED_SIZE=32
REMOTE_NAME="nc-put-blake3-test.txt"

echo
echo "=== NC single-file PUT: BLAKE3 round-trip ==="
echo

# ── authenticate ─────────────────────────────────────────────────────────────

oxicloud_login

APP_PASSWORD_RESPONSE=$(curl -s -X POST \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"label":"nc-put-blake3-test","scopes":"oxicloud:files:rw"}' \
    "$base_url/api/auth/app-passwords")
APP_PASSWORD=$(jq -r '.password' <<<"$APP_PASSWORD_RESPONSE")
APP_PASSWORD_ID=$(jq -r '.id' <<<"$APP_PASSWORD_RESPONSE")
[[ -n "$APP_PASSWORD" && "$APP_PASSWORD" != "null" ]] \
    || fail "Failed to mint NC app password: $APP_PASSWORD_RESPONSE"

trap '[[ -n "${APP_PASSWORD_ID:-}" ]] && rest_delete "/api/auth/app-passwords/$APP_PASSWORD_ID" > /dev/null || true' EXIT

# Idempotent cleanup of any leftover file.
HOME_FOLDER_ID=$(rest_get "/api/folders" | jq -r '.[0].id')
EXISTING_ID=$(rest_get "/api/files?folder_id=$HOME_FOLDER_ID" \
    | jq -r --arg n "$REMOTE_NAME" 'first(.[] | select(.name == $n) | .id) // empty')
if [[ -n "$EXISTING_ID" ]]; then
    echo "  cleaning up leftover $REMOTE_NAME (id=$EXISTING_ID)"
    rest_delete "/api/files/$EXISTING_ID" > /dev/null
    purge_from_trash "$REMOTE_NAME"
fi

# ── PUT via the NC surface ───────────────────────────────────────────────────

echo "  step 1: PUT /remote.php/dav/files/$username/$REMOTE_NAME"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
    -X PUT \
    -u "$username:$APP_PASSWORD" \
    -H "Content-Type: text/plain" \
    --data-binary "@$FIXTURE" \
    "$base_url/remote.php/dav/files/$username/$REMOTE_NAME")
# 201 = created (first PUT), 204 = updated (idempotent overwrite path).
# Either signals a successful write to the spool + blob promotion.
[[ "$STATUS" =~ ^(201|204)$ ]] || fail "PUT got $STATUS, expected 201/204"
pass "PUT status=$STATUS"

# ── Verify BLAKE3 + size via the REST listing ────────────────────────────────

echo "  step 2: GET /api/files → assert content_hash + size"
LISTED=$(rest_get "/api/files?folder_id=$HOME_FOLDER_ID" \
    | jq -r --arg n "$REMOTE_NAME" 'first(.[] | select(.name == $n))')
ACTUAL_HASH=$(jq -r '.content_hash' <<<"$LISTED")
ACTUAL_SIZE=$(jq -r '.size' <<<"$LISTED")
FILE_ID=$(jq -r '.id' <<<"$LISTED")

[[ "$ACTUAL_SIZE" == "$EXPECTED_SIZE" ]] \
    || fail "size mismatch: got $ACTUAL_SIZE, expected $EXPECTED_SIZE"
[[ "$ACTUAL_HASH" == "$EXPECTED_BLAKE3" ]] \
    || fail "BLAKE3 mismatch: got $ACTUAL_HASH, expected $EXPECTED_BLAKE3"
pass "content_hash + size match fixture ($EXPECTED_BLAKE3, $EXPECTED_SIZE B)"

# ── Cleanup ──────────────────────────────────────────────────────────────────

rest_delete "/api/files/$FILE_ID" > /dev/null
purge_from_trash "$REMOTE_NAME"

echo
echo "=== NC single-file PUT BLAKE3 test: $PASS passed, $FAIL failed ==="
[[ "$FAIL" == "0" ]] || exit 1
