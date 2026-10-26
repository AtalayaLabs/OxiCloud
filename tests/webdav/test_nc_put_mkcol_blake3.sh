#!/usr/bin/env bash
# =============================================================
# OxiCloud — Baseline: NC WebDAV PUT / MKCOL + BLAKE3 round-trip
# =============================================================
# Group F from BASELINE_TESTS_NC_WEBDAV.md.
#
# Headline assertions:
#   F8 / F9 — local b3sum of the uploaded bytes equals the
#             server's FileDto.content_hash retrieved via the
#             REST API. Proves the hash-on-write streaming path
#             actually produces the canonical BLAKE3 — the
#             value every downstream dedup / lifecycle hook
#             keys on. This is the load-bearing check that
#             would have caught the `etag` vs `content_hash`
#             confusion (the 0135930d regression).
#
# Pinned behaviour notes (not bugs, but worth catching if any
# change in either direction):
#   F5 / F6 — NC PUT does NOT process If-None-Match / If-Match
#             today. Both succeed with 201/204 instead of the
#             strict-RFC-4918 412. If a later commit adds
#             conditional support, the assertions here will
#             trip and you can update them deliberately.
#   F1     — NC PUT returns ETag + oc-etag + oc-fileid headers.
#             The NC desktop client needs OC-FileId on the write
#             response (F2, docs/plan/nc-sync-client-findings.md);
#             the exact value is pinned in
#             tests/api/nc_write_response_headers.hurl.
# =============================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

source test.env
source common.sh
source lib/dav_helpers.sh

echo
echo "=== NC WebDAV PUT / MKCOL + BLAKE3 (Group F baseline) ==="
echo

oxicloud_login
mint_app_password
resolve_home_folder_id
wipe_home_folder    # defensive against cross-test contamination

NC_FILES_BASE="$base_url/remote.php/dav/files/$username"

PUT_FIXTURE_DIR=$(mktemp -d)
trap 'rm -rf "$PUT_FIXTURE_DIR"' EXIT

# ── Pre-flight: confirm `b3sum` is available ─────────────────────────────────
command -v b3sum >/dev/null 2>&1 \
    || fail "preflight: b3sum required for F8/F9 — install via 'brew install b3sum' or 'apt install b3sum'"

# ── Helper: extract the first ETag value from a curl `-D -` dump ─────────────
header_value() {
    grep -i "^$1:" <<< "$2" | awk '{print $2}' | tr -d '\r"' | head -n 1
}

# ── Helper: list home folder, find file by name, capture id + content_hash ───
#
# Uses the cursor-paginated `/resources` listing (the `/listing` and
# `/contents` endpoints were removed). `?resource_types=file` returns only
# file rows as `{ items: [{ resource_type, resource }], next_cursor }`, so
# the file DTO (id + content_hash) is at `.items[].resource`.
nc_lookup_via_rest() {
    local name="$1"
    local response
    response=$(api_curl "$base_url/api/folders/$HOME_FOLDER_ID/resources?resource_types=file&limit=200")
    LAST_FILE_ID=$(jq -r --arg n "$name" '.items[]?.resource | select(.name == $n) | .id'           <<< "$response")
    LAST_FILE_CONTENT_HASH=$(jq -r --arg n "$name" '.items[]?.resource | select(.name == $n) | .content_hash' <<< "$response")
    [[ -n "$LAST_FILE_ID" && "$LAST_FILE_ID" != "null" ]] \
        || fail "REST lookup for '$name' in home folder returned no id (response: $response)"
}

# ─────────────────────────────────────────────────────────────
# F1 — PUT a new file → 201 + ETag + oc-etag
# ─────────────────────────────────────────────────────────────
echo "  F1: PUT new file → 201"
SMALL_CONTENT="hello from group F"
SMALL_PATH="$PUT_FIXTURE_DIR/f1-small.txt"
printf '%s' "$SMALL_CONTENT" > "$SMALL_PATH"
SMALL_LEN=$(wc -c < "$SMALL_PATH" | tr -d ' ')

HEADERS=$(nc_curl -D - -o /dev/null -X PUT \
    -H "Content-Type: text/plain" \
    --data-binary "@$SMALL_PATH" \
    "$NC_FILES_BASE/f1-small.txt")
STATUS=$(awk 'NR==1{print $2}' <<< "$HEADERS" | tr -d '\r')
[[ "$STATUS" == "201" ]] \
    || fail "F1: PUT new expected 201, got $STATUS"
F1_ETAG=$(header_value "etag" "$HEADERS")
F1_OC_ETAG=$(header_value "oc-etag" "$HEADERS")
[[ -n "$F1_ETAG" ]]    || fail "F1: response missing ETag header"
[[ -n "$F1_OC_ETAG" ]] || fail "F1: response missing oc-etag header"
[[ "$F1_ETAG" == "$F1_OC_ETAG" ]] \
    || fail "F1: ETag ($F1_ETAG) and oc-etag ($F1_OC_ETAG) should match"
# The sync client reads OC-FileId off the PUT response; without it the
# client reports "Missing File ID from server" (F2 in
# docs/plan/nc-sync-client-findings.md).
[[ -n "$(header_value "oc-fileid" "$HEADERS")" ]] \
    || fail "F1: response missing oc-fileid header"
pass "F1: PUT new → 201 + matching ETag/oc-etag + oc-fileid"

# ─────────────────────────────────────────────────────────────
# F2 — GET retrieves the bytes we just PUT
# ─────────────────────────────────────────────────────────────
echo "  F2: GET file just PUT"
ACTUAL=$(nc_curl "$NC_FILES_BASE/f1-small.txt")
[[ "$ACTUAL" == "$SMALL_CONTENT" ]] \
    || fail "F2: body mismatch — got '$ACTUAL', expected '$SMALL_CONTENT'"
pass "F2: GET returns exact bytes from F1's PUT"

# ─────────────────────────────────────────────────────────────
# F3 — PUT overwrite same path → 204 + NEW ETag
# ─────────────────────────────────────────────────────────────
echo "  F3: PUT overwrite → 204 + new ETag"
NEW_CONTENT="goodbye from group F"
NEW_PATH="$PUT_FIXTURE_DIR/f3-overwrite.txt"
printf '%s' "$NEW_CONTENT" > "$NEW_PATH"

HEADERS=$(nc_curl -D - -o /dev/null -X PUT \
    -H "Content-Type: text/plain" \
    --data-binary "@$NEW_PATH" \
    "$NC_FILES_BASE/f1-small.txt")
STATUS=$(awk 'NR==1{print $2}' <<< "$HEADERS" | tr -d '\r')
[[ "$STATUS" == "204" ]] \
    || fail "F3: PUT overwrite expected 204, got $STATUS"
F3_ETAG=$(header_value "etag" "$HEADERS")
[[ -n "$F3_ETAG" ]] || fail "F3: overwrite response missing ETag"
[[ "$F3_ETAG" != "$F1_ETAG" ]] \
    || fail "F3: ETag must change on overwrite ($F1_ETAG → $F3_ETAG)"
pass "F3: PUT overwrite → 204 + new ETag (different from F1)"

# ─────────────────────────────────────────────────────────────
# F4 — GET after overwrite returns new content (cache regression
#      guard — see commit f4ce4092)
# ─────────────────────────────────────────────────────────────
echo "  F4: GET after overwrite returns NEW content"
ACTUAL=$(nc_curl "$NC_FILES_BASE/f1-small.txt")
[[ "$ACTUAL" == "$NEW_CONTENT" ]] \
    || fail "F4: STALE content after overwrite — got '$ACTUAL', expected '$NEW_CONTENT' (regression of f4ce4092?)"
pass "F4: GET after overwrite serves the new bytes (no stale-cache)"

# ─────────────────────────────────────────────────────────────
# F5 / F6 — Conditional PUT (RFC 7232 §3.1/§3.2, RFC 4918 §10)
#
# F5 covers `If-None-Match: *`: server MUST refuse the PUT with
# 412 when the target representation already exists (used by
# clients to do "create only if absent"). The mirror case — same
# header on a NEW path — must succeed; covered by F5b.
#
# F6 covers `If-Match: "<etag>"`: server MUST refuse the PUT with
# 412 when the supplied ETag doesn't strong-match the current
# representation (used by clients to do "update only if
# unchanged"). The mirror case — correct ETag → success — is
# covered by F6b.
# ─────────────────────────────────────────────────────────────
echo "  F5: PUT with If-None-Match: * on existing path → 412"
HEADERS=$(nc_curl -D - -o /dev/null -X PUT \
    -H "If-None-Match: *" -H "Content-Type: text/plain" \
    --data-binary 'F5-payload' \
    "$NC_FILES_BASE/f1-small.txt")
STATUS=$(awk 'NR==1{print $2}' <<< "$HEADERS" | tr -d '\r')
[[ "$STATUS" == "412" ]] \
    || fail "F5: expected 412 Precondition Failed for If-None-Match: * on existing path, got $STATUS"
pass "F5: If-None-Match: * on existing path → 412"

echo "  F5b: PUT with If-None-Match: * on NEW path → 201/204"
HEADERS=$(nc_curl -D - -o /dev/null -X PUT \
    -H "If-None-Match: *" -H "Content-Type: text/plain" \
    --data-binary 'F5b-payload' \
    "$NC_FILES_BASE/f5b-new.txt")
STATUS=$(awk 'NR==1{print $2}' <<< "$HEADERS" | tr -d '\r')
[[ "$STATUS" == "201" || "$STATUS" == "204" ]] \
    || fail "F5b: expected 201/204 for If-None-Match: * on new path, got $STATUS"
pass "F5b: If-None-Match: * on new path → $STATUS"

echo "  F6: PUT with If-Match: \"wrong-etag\" → 412"
HEADERS=$(nc_curl -D - -o /dev/null -X PUT \
    -H 'If-Match: "deadbeef-never-matches"' -H "Content-Type: text/plain" \
    --data-binary 'F6-payload' \
    "$NC_FILES_BASE/f1-small.txt")
STATUS=$(awk 'NR==1{print $2}' <<< "$HEADERS" | tr -d '\r')
[[ "$STATUS" == "412" ]] \
    || fail "F6: expected 412 Precondition Failed for non-matching If-Match, got $STATUS"
pass "F6: If-Match with non-matching ETag → 412"

echo "  F6b: PUT with correct If-Match → 204"
# Fetch the current ETag of f1-small.txt via PROPFIND-ish HEAD,
# then re-PUT with that exact value as If-Match. Must succeed.
CURRENT_ETAG=$(nc_curl -D - -o /dev/null -X HEAD "$NC_FILES_BASE/f1-small.txt" \
    | awk 'BEGIN{IGNORECASE=1} /^etag:/ {print $2}' | tr -d '\r')
[[ -n "$CURRENT_ETAG" ]] || fail "F6b: could not read current ETag via HEAD"
HEADERS=$(nc_curl -D - -o /dev/null -X PUT \
    -H "If-Match: $CURRENT_ETAG" -H "Content-Type: text/plain" \
    --data-binary 'F6b-payload' \
    "$NC_FILES_BASE/f1-small.txt")
STATUS=$(awk 'NR==1{print $2}' <<< "$HEADERS" | tr -d '\r')
[[ "$STATUS" == "204" ]] \
    || fail "F6b: expected 204 for If-Match with correct ETag, got $STATUS"
pass "F6b: If-Match with current ETag → 204"

echo "  F6c: PUT with If-Match: * on existing path → 204 (catch-all)"
HEADERS=$(nc_curl -D - -o /dev/null -X PUT \
    -H 'If-Match: *' -H "Content-Type: text/plain" \
    --data-binary 'F6c-payload' \
    "$NC_FILES_BASE/f1-small.txt")
STATUS=$(awk 'NR==1{print $2}' <<< "$HEADERS" | tr -d '\r')
[[ "$STATUS" == "204" ]] \
    || fail "F6c: expected 204 for If-Match: * on existing path, got $STATUS"
pass "F6c: If-Match: * on existing path → 204"

echo "  F6d: PUT with If-Match on NEW path → 412 (resource absent → cannot match)"
HEADERS=$(nc_curl -D - -o /dev/null -X PUT \
    -H 'If-Match: "anything"' -H "Content-Type: text/plain" \
    --data-binary 'F6d-payload' \
    "$NC_FILES_BASE/f6d-new.txt")
STATUS=$(awk 'NR==1{print $2}' <<< "$HEADERS" | tr -d '\r')
[[ "$STATUS" == "412" ]] \
    || fail "F6d: expected 412 for If-Match on absent path, got $STATUS"
pass "F6d: If-Match on absent path → 412"

# ─────────────────────────────────────────────────────────────
# F7 — PUT a "large" file → succeeds, GET returns exact bytes
#
# Size is 3 MiB, deliberately just under `OXICLOUD_DIRECT_PUT_MAX_BYTES`
# (4 MiB in the test env — see `tests/common/server.env`). Files
# above that cap are expected to use the chunked-upload protocol,
# which is Group J territory. 3 MiB is still big enough to exercise
# the streaming-spool / hash-on-write path that F9 then validates
# end-to-end via the BLAKE3 round-trip. The BASELINE doc said "10 MB"
# but the test env constraint takes precedence.
# ─────────────────────────────────────────────────────────────
echo "  F7: PUT 3 MiB random binary → 201/204 + GET returns same bytes"
LARGE_PATH="$PUT_FIXTURE_DIR/f7-large.bin"
dd if=/dev/urandom of="$LARGE_PATH" bs=1024 count=3072 status=none
LARGE_LEN=$(wc -c < "$LARGE_PATH" | tr -d ' ')
LARGE_LOCAL_HASH=$(b3sum --no-names "$LARGE_PATH" | awk '{print $1}')

# Disable `Expect: 100-continue` — curl sends it for large bodies,
# and the resulting interim "HTTP/1.1 100 Continue" line would be
# the FIRST line in the `-D -` dump, making `awk 'NR==1'` pick up
# 100 instead of the final 201/204. The Expect handshake serves
# no functional purpose for the test.
HEADERS=$(nc_curl -D - -o /dev/null -X PUT \
    -H "Expect:" \
    -H "Content-Type: application/octet-stream" \
    --data-binary "@$LARGE_PATH" \
    "$NC_FILES_BASE/f7-large.bin")
STATUS=$(awk 'NR==1{print $2}' <<< "$HEADERS" | tr -d '\r')
[[ "$STATUS" == "201" || "$STATUS" == "204" ]] \
    || fail "F7: PUT 3 MiB expected 201/204, got $STATUS"
DOWNLOADED="$PUT_FIXTURE_DIR/f7-large.downloaded"
nc_curl -o "$DOWNLOADED" "$NC_FILES_BASE/f7-large.bin"
cmp -s "$LARGE_PATH" "$DOWNLOADED" \
    || fail "F7: downloaded bytes differ from uploaded — streaming integrity broken"
pass "F7: 3 MiB streamed PUT round-trips byte-identically"

# ─────────────────────────────────────────────────────────────
# F8 — BLAKE3 round-trip (small file)
#
# Uses a dedicated path (`f8-blake3-probe.txt`) that no other
# scenario in this script touches. F1-F6 all overwrite
# `f1-small.txt` repeatedly, so by the time F8 runs the server
# holds whatever F6's last PUT wrote (`F6-payload`), not what
# F3 wrote — comparing F3's local b3sum against the server
# would be a false mismatch. A fresh single-write fixture
# isolates the BLAKE3 round-trip from the F1-F6 sequence.
#
# Verifies the streaming hash-on-write path produced the
# canonical BLAKE3 the dedup/lifecycle layer expects.
# ─────────────────────────────────────────────────────────────
echo "  F8: BLAKE3 round-trip (small file — local b3sum vs server content_hash)"
F8_PATH="$PUT_FIXTURE_DIR/f8-probe.txt"
printf 'f8 blake3 round-trip probe — single write, known bytes' > "$F8_PATH"
F8_LOCAL_HASH=$(b3sum --no-names "$F8_PATH" | awk '{print $1}')
nc_curl -X PUT -H "Content-Type: text/plain" \
    --data-binary "@$F8_PATH" \
    "$NC_FILES_BASE/f8-blake3-probe.txt" > /dev/null
nc_lookup_via_rest "f8-blake3-probe.txt"
[[ -n "$LAST_FILE_CONTENT_HASH" && "$LAST_FILE_CONTENT_HASH" != "null" ]] \
    || fail "F8: REST returned empty content_hash for f8-blake3-probe.txt"
[[ "$LAST_FILE_CONTENT_HASH" == "$F8_LOCAL_HASH" ]] \
    || fail "F8: BLAKE3 mismatch — server '$LAST_FILE_CONTENT_HASH' vs local '$F8_LOCAL_HASH'"
pass "F8: small-file content_hash matches local b3sum ($F8_LOCAL_HASH)"

# ─────────────────────────────────────────────────────────────
# F9 — BLAKE3 round-trip (streamed file)
#
# Same check on the streaming hash-on-write path. The 3 MiB
# upload from F7 exercises the streaming spool /
# hasher.update / final blob promotion sequence — F8 only
# validates the small-buffer path. Size was 10 MB in the
# BASELINE doc; reduced to 3 MiB so it stays under the test
# env's direct-PUT cap (see F7 comment).
# ─────────────────────────────────────────────────────────────
echo "  F9: BLAKE3 round-trip (3 MiB streamed file)"
nc_lookup_via_rest "f7-large.bin"
[[ -n "$LAST_FILE_CONTENT_HASH" && "$LAST_FILE_CONTENT_HASH" != "null" ]] \
    || fail "F9: REST returned empty content_hash for f7-large.bin"
[[ "$LAST_FILE_CONTENT_HASH" == "$LARGE_LOCAL_HASH" ]] \
    || fail "F9: BLAKE3 mismatch on 3 MiB — server '$LAST_FILE_CONTENT_HASH' vs local '$LARGE_LOCAL_HASH'"
pass "F9: 3 MiB streamed content_hash matches local b3sum"

# ─────────────────────────────────────────────────────────────
# F10 — MKCOL creates a folder → 201
# ─────────────────────────────────────────────────────────────
echo "  F10: MKCOL new folder → 201"
STATUS=$(nc_curl -o /dev/null -w "%{http_code}" -X MKCOL "$NC_FILES_BASE/f10-folder/")
[[ "$STATUS" == "201" ]] \
    || fail "F10: MKCOL new expected 201, got $STATUS"
# PROPFIND it to confirm
BODY=$(nc_curl -X PROPFIND -H "Depth: 0" "$NC_FILES_BASE/f10-folder/")
grep -q '<d:collection/>' <<< "$BODY" \
    || fail "F10: PROPFIND of just-created folder lacks <d:collection/>"
pass "F10: MKCOL creates folder, PROPFIND sees it as a collection"

# ─────────────────────────────────────────────────────────────
# F11 / F11b / F11c — MKCOL parent semantics (RFC 4918 §9.3.1)
#
# F11  : missing intermediate parent → 409 Conflict
# F11b : parent exists, target new   → 201 Created (positive case)
# F11c : target already exists       → 405 Method Not Allowed
#
# Sabre/DAV and the actual NC server both 409 on a missing
# intermediate; our previous `mkdir -p` behaviour deviated. NC
# desktop walks ancestors one MKCOL at a time during sync so
# nothing real breaks from dropping the auto-create.
# ─────────────────────────────────────────────────────────────
echo "  F11: MKCOL with missing intermediate parent → 409"
STATUS=$(nc_curl -o /dev/null -w "%{http_code}" -X MKCOL \
    "$NC_FILES_BASE/f11-nonexistent-parent/inner/")
[[ "$STATUS" == "409" ]] \
    || fail "F11: expected 409 Conflict for MKCOL with missing parent, got $STATUS"
# The non-existent parent must NOT have been auto-created either.
[[ "$(nc_status_propfind_depth0 "$NC_FILES_BASE/f11-nonexistent-parent/")" == "404" ]] \
    || fail "F11: intermediate parent was silently created — auto-create still happening"
pass "F11: MKCOL with missing parent → 409, parent not silently created"

echo "  F11b: MKCOL with existing parent + new target → 201"
nc_curl -o /dev/null -X MKCOL "$NC_FILES_BASE/f11b-parent/" > /dev/null
STATUS=$(nc_curl -o /dev/null -w "%{http_code}" -X MKCOL \
    "$NC_FILES_BASE/f11b-parent/child/")
[[ "$STATUS" == "201" ]] \
    || fail "F11b: expected 201 Created for MKCOL with existing parent, got $STATUS"
[[ "$(nc_status_propfind_depth0 "$NC_FILES_BASE/f11b-parent/child/")" == "207" ]] \
    || fail "F11b: target collection not visible via PROPFIND after MKCOL"
pass "F11b: MKCOL with existing parent → 201, target reachable"

echo "  F11c: MKCOL with target that already exists → 405"
STATUS=$(nc_curl -o /dev/null -w "%{http_code}" -X MKCOL \
    "$NC_FILES_BASE/f11b-parent/child/")
[[ "$STATUS" == "405" ]] \
    || fail "F11c: expected 405 Method Not Allowed for MKCOL on existing collection, got $STATUS"
pass "F11c: MKCOL on existing target → 405"

# ─────────────────────────────────────────────────────────────
# F12 — MKCOL on existing folder → 405
# ─────────────────────────────────────────────────────────────
echo "  F12: MKCOL on existing folder → 405"
STATUS=$(nc_curl -o /dev/null -w "%{http_code}" -X MKCOL "$NC_FILES_BASE/f10-folder/")
[[ "$STATUS" == "405" ]] \
    || fail "F12: existing-folder MKCOL expected 405, got $STATUS"
pass "F12: MKCOL on existing folder → 405"

# ── Cleanup ──────────────────────────────────────────────────────────────────

echo "  cleanup: delete fixtures + empty trash"
# Use the NC DELETE (covered in group G) to round-trip through the
# same surface we're trying to baseline.
nc_curl -o /dev/null -X DELETE "$NC_FILES_BASE/f1-small.txt"          || true
nc_curl -o /dev/null -X DELETE "$NC_FILES_BASE/f7-large.bin"          || true
nc_curl -o /dev/null -X DELETE "$NC_FILES_BASE/f8-blake3-probe.txt"   || true
nc_curl -o /dev/null -X DELETE "$NC_FILES_BASE/f10-folder/"           || true
api_empty_trash || true
pass "cleanup done"

# ── summary ───────────────────────────────────────────────────────────────────

echo
echo "Results: $PASS passed, $FAIL failed."
[[ "$FAIL" -eq 0 ]] && echo "All tests passed." || exit 1
