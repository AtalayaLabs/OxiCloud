#!/usr/bin/env bash
# =============================================================
# OxiCloud – Chunked upload API + dedup check
# =============================================================
# Uploads free_video_over_1MB.mp4 (2760653 bytes) in 3 chunks
# of 1 MiB each via the TUS-like chunked upload API, then:
#   1. Verifies the file appears in folder listing with video/mp4 MIME type
#   2. Checks GET /api/dedup/check/{hash} → HTTP 200 (uploader owns it)
#
# BLAKE3 hash of free_video_over_1MB.mp4:
#   95d42b25a2d39f24f1b2f38bf1b947d4ec74201271a98ea0e76a9cea421eff80
#
# Prerequisites:
#   - Server running at base_url with credentials from test.env
#   - OXICLOUD_ENABLE_AUTH=true
#   - jq, dd in PATH
#
# Run (from repo root):
#   bash tests/webdav/test_chunked_upload_dedup.sh
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
dedup_check_status() {
    # The user-facing /api/dedup/check/{hash} returns 200 when the
    # caller owns a visible reference, 404 otherwise (§9b route split).
    # Status alone is sufficient here — this test only needs to know
    # the chunked upload registered the blob under the caller's name.
    curl -s -o /dev/null -w "%{http_code}" \
         -H "Authorization: Bearer $TOKEN" \
         "$base_url/api/dedup/check/$1"
}

purge_from_trash() {
    local name="$1"
    local tid
    tid=$(rest_get "/api/trash/resources" \
        | jq -r --arg n "$name" 'first(.items[] | select(.resource.name == $n) | .resource.id) // empty')
    [[ -n "$tid" ]] && rest_delete "/api/trash/$tid" > /dev/null || true
}

# ── fixture ───────────────────────────────────────────────────────────────────

BLOB_HASH="95d42b25a2d39f24f1b2f38bf1b947d4ec74201271a98ea0e76a9cea421eff80"
FIXTURE="$REPO_ROOT/tests/fixtures/free_video_over_1MB.mp4"
[[ -f "$FIXTURE" ]] || { echo "Missing fixture: $FIXTURE" >&2; exit 1; }

REMOTE_NAME="chunked-upload-test.mp4"
FILE_SIZE=2760653
CHUNK_SIZE=1048576   # 1 MiB — minimum accepted by the server
TOTAL_CHUNKS=3       # ceil(2760653 / 1048576) = 3

echo
echo "=== Chunked upload API + dedup check ==="
echo

# ── authenticate ──────────────────────────────────────────────────────────────

oxicloud_login

# ── home folder ───────────────────────────────────────────────────────────────

HOME_FOLDER_ID=$(rest_get "/api/folders" | jq -r '.[0].id')
[[ -n "$HOME_FOLDER_ID" && "$HOME_FOLDER_ID" != "null" ]] \
    || fail "Could not retrieve home folder ID"
echo "  home folder id: $HOME_FOLDER_ID"

# ── idempotent pre-test cleanup ───────────────────────────────────────────────

EXISTING_ID=$(rest_get "/api/files?folder_id=$HOME_FOLDER_ID" \
    | jq -r --arg n "$REMOTE_NAME" 'first(.[] | select(.name == $n) | .id) // empty')
if [[ -n "$EXISTING_ID" ]]; then
    echo "  cleanup: deleting existing $REMOTE_NAME (id=$EXISTING_ID)"
    rest_delete "/api/files/$EXISTING_ID" > /dev/null
fi
purge_from_trash "$REMOTE_NAME"

# ── Step 1: Create upload session ─────────────────────────────────────────────

echo "  step 1: POST /api/uploads (create session, $TOTAL_CHUNKS chunks of ${CHUNK_SIZE}B)..."
SESSION=$(curl -s \
    -X POST \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"filename\":\"$REMOTE_NAME\",\"folder_id\":\"$HOME_FOLDER_ID\",\"content_type\":\"video/mp4\",\"total_size\":$FILE_SIZE,\"chunk_size\":$CHUNK_SIZE}" \
    "$base_url/api/uploads")

UPLOAD_ID=$(jq -r '.upload_id' <<< "$SESSION")
[[ -n "$UPLOAD_ID" && "$UPLOAD_ID" != "null" ]] \
    || fail "POST /api/uploads: could not get upload_id (response: $SESSION)"
pass "Upload session created: upload_id=$UPLOAD_ID"

# ── Step 2: Upload chunks ─────────────────────────────────────────────────────

echo "  step 2: PATCH chunks 0..$(( TOTAL_CHUNKS - 1 ))..."
for (( i=0; i<TOTAL_CHUNKS; i++ )); do
    PATCH_RESP=$(dd if="$FIXTURE" bs="$CHUNK_SIZE" skip="$i" count=1 2>/dev/null \
        | curl -s \
            -X PATCH \
            -H "Authorization: Bearer $TOKEN" \
            -H "Content-Type: application/octet-stream" \
            --data-binary @- \
            "$base_url/api/uploads/$UPLOAD_ID?chunk_index=$i")

    IS_COMPLETE=$(jq -r '.is_complete' <<< "$PATCH_RESP")
    BYTES=$(jq -r '.bytes_received' <<< "$PATCH_RESP")
    [[ -n "$BYTES" && "$BYTES" != "null" ]] \
        || fail "PATCH chunk $i: unexpected response: $PATCH_RESP"
    echo "    chunk $i: bytes_received=$BYTES  is_complete=$IS_COMPLETE"
done
pass "All $TOTAL_CHUNKS chunks uploaded"

# ── Step 3: Complete the upload ───────────────────────────────────────────────

echo "  step 3: POST /api/uploads/$UPLOAD_ID/complete..."
COMPLETE_RESP=$(curl -s \
    -X POST \
    -H "Authorization: Bearer $TOKEN" \
    "$base_url/api/uploads/$UPLOAD_ID/complete")

FILE_ID=$(jq -r '.file_id' <<< "$COMPLETE_RESP")
[[ -n "$FILE_ID" && "$FILE_ID" != "null" ]] \
    || fail "POST complete: could not get file_id (response: $COMPLETE_RESP)"
pass "Upload complete: file_id=$FILE_ID"

# ── Step 4: Verify file appears in folder listing with correct MIME type ──────

echo "  step 4: verify file in folder listing with video/mp4 MIME type..."
LISTING=$(rest_get "/api/files?folder_id=$HOME_FOLDER_ID")
LISTED_FILE=$(jq -r --arg id "$FILE_ID" 'first(.[] | select(.id == $id))' <<< "$LISTING")

[[ -n "$LISTED_FILE" && "$LISTED_FILE" != "null" ]] \
    || fail "File $FILE_ID not found in folder listing"

MIME=$(jq -r '.mime_type' <<< "$LISTED_FILE")
[[ "$MIME" == "video/mp4" ]] \
    || fail "Expected MIME type video/mp4, got: $MIME"
pass "File listed with MIME type: $MIME"

# ── Step 4b: server's content_hash matches our local BLAKE3 ──────────────────
# Cross-check the file we just uploaded: the server's view of its
# content identity (FileDto.content_hash, exposed in REST JSON since
# the etag-centralization refactor) must equal the BLAKE3 we know
# the fixture has. Catches any chunk-assembly bug that would
# silently produce a different blob than the source bytes.

LISTED_HASH=$(jq -r '.content_hash // empty' <<< "$LISTED_FILE")
[[ "$LISTED_HASH" == "$BLOB_HASH" ]] \
    || fail "content_hash mismatch: server=$LISTED_HASH expected=$BLOB_HASH"
pass "content_hash matches local BLAKE3 ($BLOB_HASH)"

# ── Step 5: Dedup check → 200 (caller owns the blob) ────────────────────────

echo "  step 5: GET /api/dedup/check/$BLOB_HASH..."
STATUS=$(dedup_check_status "$BLOB_HASH")
[[ "$STATUS" == "200" ]] \
    || fail "dedup/check: expected 200 (uploader owns the blob), got $STATUS"
pass "chunked upload registered the blob under the uploader"

# ── cleanup ───────────────────────────────────────────────────────────────────

echo "  cleanup..."
ST=$(rest_delete "/api/files/$FILE_ID")
[[ "$ST" == "204" ]] || fail "DELETE file expected 204, got $ST"
purge_from_trash "$REMOTE_NAME"
pass "Cleanup complete"

# ── summary ───────────────────────────────────────────────────────────────────

echo
echo "Results: $PASS passed, $FAIL failed."
[[ "$FAIL" -eq 0 ]] && echo "All tests passed." || exit 1
