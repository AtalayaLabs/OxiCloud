#!/usr/bin/env bash
# =============================================================
# OxiCloud – legacy sidecar → blob migration (both import jobs)
# =============================================================
# Exercises `thumb_derived_import` and `thumb_attached_import` end to end,
# which nothing else does: their unit tests cover only the directory walk,
# never a run.
#
# ── How legacy state is manufactured ─────────────────────────────────────
#
# The test environment always starts fresh, so there is no pre-migration
# data to import. We create it, and the reconstruction is EXACT rather than
# an imitation: the on-disk layout did not change in this work. A
# server-rendered thumbnail has always been written to
# `{size}/{hash}.webp`, and an uploaded preview to `{size}/ext-{id}.jpg`.
# The only thing that is new is the DB row.
#
# So: upload through the real API (which writes both the file and the row),
# then delete the row. What remains on disk is byte-for-byte what a
# pre-migration install has.
#
# Deleting the row must also release the reference it held, or the
# manufactured state would carry a reference no legacy install ever had and
# the end-of-suite registry check would report a leak that this script
# caused. `file_attached_blobs` has an ON DELETE trigger that does it;
# `content_derived_blobs` does not, so we decrement explicitly.
#
# ── What is asserted ─────────────────────────────────────────────────────
#
#   1. Both rows come back after the import.
#   2. The uploaded preview survives a COPY — the user-visible point of
#      `file_attached_blobs`, and impossible before the row existed.
#   3. Re-running imports nothing and changes no refcount. This is the
#      defect most likely to be silent: `store_attached_blob` is
#      ON CONFLICT DO UPDATE, so an import that skipped its existence
#      check would release and retake a reference on every run.
#
# Runs BEFORE storage_cleanup_check.sh, which deletes everything.
#
# Prerequisites: setup.hurl has run (admin exists); docker compose db up.
# =============================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/tests/common/docker-compose.test.yml"

# shellcheck source=test.env
source "$SCRIPT_DIR/test.env"

log()  { echo "[thumb-import] $*"; }

# Dump a job's findings before dying. Without this, an import that ran but
# imported nothing looks identical to one that never ran — and the jobs
# record precisely why they skipped a file (orphan, unreadable, store
# failed). The first failure of this script was a misreported orphan, and
# the finding naming it was sitting in the run the whole time.
dump_findings() {
  local job="$1" run_id findings
  run_id=$(curl -sf -H "$AUTH" "$base_url/api/admin/jobs/$job/runs?limit=1" 2>/dev/null \
           | jq -r 'if type == "array" then .[0].id else ((.runs // .items // [])[0].id) end // empty')
  [[ -z "$run_id" ]] && { echo "  ($job: no run found)" >&2; return; }
  findings=$(curl -sf -H "$AUTH" \
    "$base_url/api/admin/jobs/$job/runs/$run_id/findings?limit=20" 2>/dev/null || echo '[]')
  echo "  $job findings:" >&2
  echo "$findings" | jq -r \
    'if type == "array" then .[] else (.findings // .items // [])[] end
     | "    \(.kind // .finding_kind // "?") \(.details // {} | tostring)"' 2>/dev/null >&2 \
    || echo "    (unparseable)" >&2
}

fail() {
  echo $'\e[31m'"[thumb-import] FAIL: $*"$'\e[0m' >&2
  dump_findings thumb_derived_import
  dump_findings thumb_attached_import
  exit 1
}

# psql inside the compose container — no host psql dependency, matching
# how spawn-db.sh probes readiness.
sql() {
  # Podman's docker-compose shim prints a provider banner to stderr on every
  # invocation, which buries this script's own output. Filtered rather than
  # discarded (`2>/dev/null`) so genuine psql errors still surface — losing
  # those would turn a broken query into a silently wrong assertion.
  #
  # Suppressing it at the source needs `[engine] compose_warning_logs = false`
  # in containers.conf, which is per-developer config and cannot be relied on
  # in CI.
  docker compose -f "$COMPOSE_FILE" exec -T postgres-test \
    psql -U oxicloud_test -d oxicloud_test -tAqc "$1" \
    2> >(grep -v 'Executing external compose provider' >&2)
}

TOKEN=$(curl -sf -X POST "$base_url/api/auth/login" \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"$username\",\"password\":\"$password\"}" \
  | jq -r '.access_token')
[[ -n "$TOKEN" && "$TOKEN" != "null" ]] || fail "login failed"
AUTH="Authorization: Bearer $TOKEN"

# ── 1. Create a file with BOTH sidecar shapes ────────────────────────────

SRC_FOLDER=$(curl -sf -X POST "$base_url/api/folders" -H "$AUTH" \
  -H "Content-Type: application/json" \
  -d '{"name":"hurl-import-src"}' | jq -r '.id')
DST_FOLDER=$(curl -sf -X POST "$base_url/api/folders" -H "$AUTH" \
  -H "Content-Type: application/json" \
  -d '{"name":"hurl-import-dst"}' | jq -r '.id')
[[ -n "$SRC_FOLDER" && "$SRC_FOLDER" != "null" ]] || fail "folder create failed"

UPLOAD=$(curl -sf -X POST "$base_url/api/files/upload" -H "$AUTH" \
  -F "folder_id=$SRC_FOLDER" \
  -F "file=@$REPO_ROOT/tests/fixtures/red-image.png;type=image/png")
FILE_ID=$(echo "$UPLOAD" | jq -r '.id')
BLOB_HASH=$(echo "$UPLOAD" | jq -r '.content_hash')
[[ -n "$FILE_ID" && "$FILE_ID" != "null" ]] || fail "upload failed: $UPLOAD"
log "uploaded file=$FILE_ID hash=${BLOB_HASH:0:12}"

# Render → writes {size}/{hash}.webp AND the content_derived_blobs row.
curl -sf -H "$AUTH" "$base_url/api/files/$FILE_ID/thumbnail/preview" -o /dev/null \
  || fail "render thumbnail failed"

# Upload → writes ext-{file_id}.jpg AND the file_attached_blobs row.
curl -sf -X PUT -H "$AUTH" -H "Content-Type: image/png" \
  --data-binary "@$REPO_ROOT/tests/fixtures/green-image.png" \
  "$base_url/api/files/$FILE_ID/thumbnail/preview" -o /dev/null \
  || fail "upload thumbnail failed"

UPLOADED_THUMB=$(mktemp)
curl -sf -H "$AUTH" "$base_url/api/files/$FILE_ID/thumbnail/preview" -o "$UPLOADED_THUMB"

DERIVED_BEFORE=$(sql "SELECT count(*) FROM storage.content_derived_blobs WHERE source_hash='$BLOB_HASH';")
ATTACHED_BEFORE=$(sql "SELECT count(*) FROM storage.file_attached_blobs WHERE file_id='$FILE_ID';")
[[ "$DERIVED_BEFORE"  -ge 1 ]] || fail "expected a content_derived_blobs row before stripping"
[[ "$ATTACHED_BEFORE" -ge 1 ]] || fail "expected a file_attached_blobs row before stripping"
log "rows present before stripping: derived=$DERIVED_BEFORE attached=$ATTACHED_BEFORE"

# ── 2. Strip the rows → this IS the legacy state ─────────────────────────
#
# Release each reference as the row goes, so the manufactured state matches
# a pre-migration install rather than carrying references it never had.
# file_attached_blobs does this via its ON DELETE trigger; the derived table
# has no trigger (its Rust purge path releases explicitly), so do it here.

sql "WITH gone AS (
       DELETE FROM storage.content_derived_blobs
        WHERE source_hash='$BLOB_HASH'
       RETURNING blob_hash
     )
     UPDATE storage.chunk_manifests m
        SET ref_count = GREATEST(m.ref_count - 1, 0)
       FROM gone WHERE m.file_hash = gone.blob_hash;" >/dev/null

sql "DELETE FROM storage.file_attached_blobs WHERE file_id='$FILE_ID';" >/dev/null

[[ "$(sql "SELECT count(*) FROM storage.content_derived_blobs WHERE source_hash='$BLOB_HASH';")" == "0" ]] \
  || fail "derived row survived the strip"
[[ "$(sql "SELECT count(*) FROM storage.file_attached_blobs WHERE file_id='$FILE_ID';")" == "0" ]] \
  || fail "attached row survived the strip"
log "legacy state manufactured: files on disk, no rows."

# ── 3. Run the imports ───────────────────────────────────────────────────

for job in thumb_derived_import thumb_attached_import; do
  curl -sf -X POST -H "$AUTH" "$base_url/api/admin/jobs/$job/trigger" >/dev/null \
    || fail "$job trigger failed"
  log "$job triggered."
done

DERIVED_AFTER=$(sql "SELECT count(*) FROM storage.content_derived_blobs WHERE source_hash='$BLOB_HASH';")
ATTACHED_AFTER=$(sql "SELECT count(*) FROM storage.file_attached_blobs WHERE file_id='$FILE_ID';")
[[ "$DERIVED_AFTER"  -ge 1 ]] || fail "thumb_derived_import did not restore the row"
[[ "$ATTACHED_AFTER" -ge 1 ]] || fail "thumb_attached_import did not restore the row"
log "rows restored: derived=$DERIVED_AFTER attached=$ATTACHED_AFTER"

# Provenance: imported rows carry the sentinel, which is how an operator
# tells them from previews with a real uploader.
UPLOADER=$(sql "SELECT uploaded_by FROM storage.file_attached_blobs WHERE file_id='$FILE_ID';")
[[ "$UPLOADER" == "00000000-0000-0000-0000-000000000000" ]] \
  || fail "imported row should carry the nil uploader sentinel, got '$UPLOADER'"

# ── 4. The user-visible point: a COPY inherits the preview ───────────────
#
# Impossible before the row existed — the ext- sidecar is keyed by file_id
# and no copy path duplicates it, so the copy fell back to a render.

COPY_ID=$(curl -sf -X POST "$base_url/api/batch/files/copy" -H "$AUTH" \
  -H "Content-Type: application/json" \
  -d "{\"file_ids\":[\"$FILE_ID\"],\"target_folder_id\":\"$DST_FOLDER\"}" \
  | jq -r '.successful[0].id')
[[ -n "$COPY_ID" && "$COPY_ID" != "null" ]] || fail "copy failed"

COPY_THUMB=$(mktemp)
curl -sf -H "$AUTH" "$base_url/api/files/$COPY_ID/thumbnail/preview" -o "$COPY_THUMB"
cmp -s "$UPLOADED_THUMB" "$COPY_THUMB" \
  || fail "copy did not inherit the imported preview"
log "copy inherits the imported preview."

# ── 5. Idempotence: a second run imports nothing and churns nothing ──────
#
# The likely silent defect. store_attached_blob is ON CONFLICT DO UPDATE, so
# an import that skipped its existence check would release and retake a
# reference every run — invisible except as refcount drift.

ATTACHED_HASH=$(sql "SELECT blob_hash FROM storage.file_attached_blobs WHERE file_id='$FILE_ID' LIMIT 1;")
[[ -n "$ATTACHED_HASH" ]] || fail "no attached blob_hash to check refcounts against"
# A single-chunk blob has a manifest whose file_hash equals its own hash, so
# this is the counter add_reference actually touches. `-` rather than empty
# keeps the later comparison meaningful if the manifest is unexpectedly absent.
REFS_BEFORE=$(sql "SELECT ref_count FROM storage.chunk_manifests WHERE file_hash='$ATTACHED_HASH';")
REFS_BEFORE=${REFS_BEFORE:--}

for job in thumb_derived_import thumb_attached_import; do
  curl -sf -X POST -H "$AUTH" "$base_url/api/admin/jobs/$job/trigger" >/dev/null \
    || fail "$job re-trigger failed"
done

REFS_AFTER=$(sql "SELECT ref_count FROM storage.chunk_manifests WHERE file_hash='$ATTACHED_HASH';")
REFS_AFTER=${REFS_AFTER:--}
DERIVED_2=$(sql "SELECT count(*) FROM storage.content_derived_blobs WHERE source_hash='$BLOB_HASH';")
ATTACHED_2=$(sql "SELECT count(*) FROM storage.file_attached_blobs WHERE file_id='$FILE_ID';")

[[ "$REFS_AFTER" == "$REFS_BEFORE" ]] \
  || fail "re-run changed the attached blob refcount: $REFS_BEFORE → $REFS_AFTER"
[[ "$DERIVED_2"  == "$DERIVED_AFTER"  ]] || fail "re-run duplicated derived rows"
[[ "$ATTACHED_2" == "$ATTACHED_AFTER" ]] || fail "re-run duplicated attached rows"
log "re-run is a no-op: rows and refcounts unchanged."

# ── 6. Teardown ──────────────────────────────────────────────────────────
# Everything created here must go — one database serves the whole suite,
# and storage_cleanup_check.sh afterwards asserts the registry drains to
# zero.

rm -f "$UPLOADED_THUMB" "$COPY_THUMB"

for folder in "$SRC_FOLDER" "$DST_FOLDER"; do
  curl -sf -X DELETE -H "$AUTH" "$base_url/api/folders/$folder" -o /dev/null || true
done
TRASH=$(curl -sf -H "$AUTH" "$base_url/api/trash/resources" || echo '{}')
for folder in "$SRC_FOLDER" "$DST_FOLDER"; do
  tid=$(echo "$TRASH" | jq -r --arg id "$folder" '.items[]? | select(.resource.id == $id) | .resource.id')
  [[ -n "$tid" ]] && curl -sf -X DELETE -H "$AUTH" "$base_url/api/trash/$tid" -o /dev/null || true
done

log "OK — both imports restore their rows, the copy inherits the preview, and re-running is a no-op."
