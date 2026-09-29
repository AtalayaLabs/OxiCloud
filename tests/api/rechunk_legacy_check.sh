#!/usr/bin/env bash
# =============================================================
# OxiCloud – pre-CDC whole-file blob → CDC chunks (`backend_rechunk`)
# =============================================================
# Exercises the `backend_rechunk` job end to end. Nothing else can: the
# conversion only has anything to do when *legacy* data exists, and a test
# environment starts fresh.
#
# ── Why the legacy state has to be manufactured ──────────────────────────
#
# It cannot be produced through the API. `store_from_stream` always writes a
# `chunk_manifests` row — there is no small-file short-circuit — so every
# upload on a current build is already CDC. A whole-file blob is genuinely
# historical: pre-CDC data, or data from a build where the sweep never ran.
#
# So we build it, and the reconstruction is exact rather than an imitation.
# A pre-CDC install had, for one file:
#
#   * `storage.files.blob_hash`  → the whole-file content hash H
#   * `storage.blobs`            → one row at H
#   * `chunk_manifests`          → NOTHING for H
#   * backend                    → ONE object at H holding the whole file
#
# After a CDC upload we instead have a manifest, N chunk rows, and N chunk
# objects with no object at H at all. Getting from there to the above is four
# steps: place the whole-file object, drop the manifest, release and remove
# the chunk rows + their objects, and insert the whole-file row. That last
# detail is the one an imitation would skip — without the object at H the job
# would fail to read the blob and the test would be asserting on an error
# path rather than on the migration.
#
# The fixture is >1 MiB of VARIED content, deliberately:
#
#   * larger than `CDC_MAX_CHUNK` (1 MiB) so FastCDC must produce several
#     chunks — a single-chunk file re-chunks to one chunk and would let a
#     broken migration pass;
#   * varied rather than zeros, because `chunk-over-cap-5mb.bin` is 5 MiB of
#     zeros and its chunks would be IDENTICAL to ours. Sharing chunks would
#     make the refcount bookkeeping here describe someone else's data too.
#
# ── What is asserted ─────────────────────────────────────────────────────
#
#   1. The upload really is multi-chunk (guards the fixture, not the code).
#   2. The manufactured state is what the job looks for — `count_legacy_blobs`
#      sees it, which is the same predicate the job pages through.
#   3. After the run: a manifest exists again, with MORE THAN ONE chunk. This
#      is the "correctly chunked" assertion.
#   4. The file still downloads byte-for-byte. The point of the migration is
#      cheaper Range reads, not a re-encoded file — if content can change,
#      nothing else here matters.
#   5. The whole-file object is GONE from the backend. Step 3 of the
#      conversion frees it once its row is removed; leaving it behind would
#      mean the migration doubles storage instead of reorganising it.
#   6. Re-running converts nothing — the manifest row is the done marker, so
#      a second run must be a no-op rather than re-chunking what it just
#      wrote.
#   7. No refcount drift, compared against baselines rather than zero.
#
# Runs BEFORE storage_cleanup_check.sh, which deletes everything it needs.
#
# Prerequisites: setup.hurl has run (admin exists); docker compose db up.
# =============================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/tests/common/docker-compose.test.yml"
STORAGE_PATH="${OXICLOUD_STORAGE_PATH:-$REPO_ROOT/tests/api/storage}"

# shellcheck source=test.env
source "$SCRIPT_DIR/test.env"

log()  { echo "[rechunk] $*"; }
fail() { echo "[rechunk] FAIL: $*" >&2; exit 1; }

# psql inside the compose container — no host psql dependency, matching
# thumb_import_check.sh. The provider banner podman's shim writes to stderr is
# filtered rather than discarded so real psql errors still surface.
sql() {
  # `< /dev/null` is load-bearing, not tidiness. `docker compose exec` drains
  # stdin, so calling this inside a `while read … done <<< "$list"` loop
  # swallows every remaining line of the list and the loop runs exactly once.
  #
  # That bug silently produced the failure this script was written to
  # investigate: only 1 of 11 chunk rows got decremented and removed, the other
  # 10 survived at ref_count 1, and the migration then re-pinned them to 2 —
  # ten `refcount_mismatch` findings that looked like a defect in the migration
  # and were an artifact of the test's own setup. Hence the explicit count
  # assertion after the loop as well.
  docker compose -f "$COMPOSE_FILE" exec -T postgres-test \
    psql -U oxicloud_test -d oxicloud_test -tAqc "$1" \
    < /dev/null \
    2> >(grep -v 'Executing external compose provider' >&2)
}

# Path the local backend stores a blob at: .blobs/{hash[..2]}/{hash}.blob
blob_path() { echo "$STORAGE_PATH/.blobs/${1:0:2}/$1.blob"; }

# Per-chunk stored vs. auditor-computed ref_count for one file hash.
#
# Exists because "findings moved N → M" says drift happened and nothing about
# which direction. `stored > actual` is a leaked reference (bytes pinned
# forever); `stored < actual` is an under-count, which is the dangerous one —
# reclamation could reap a chunk a live manifest still needs. The formula is
# `blobs_consistency`'s own: direct file references only count when no manifest
# exists for that hash, plus one per manifest containing the chunk.
dump_refcounts() {
  local file_hash="$1"
  echo "  chunk refcounts for ${file_hash:0:12} (stored | actual | delta):" >&2
  sql "SELECT b.hash || ' | ' || b.ref_count || ' | ' || (
              (SELECT COUNT(*) FROM storage.files f
                WHERE f.blob_hash = b.hash
                  AND NOT EXISTS (SELECT 1 FROM storage.chunk_manifests m2
                                   WHERE m2.file_hash = f.blob_hash))
            + (SELECT COUNT(*) FROM storage.chunk_manifests m3
                WHERE b.hash = ANY(m3.chunk_hashes))) || ' | ' || (
              b.ref_count - (
              (SELECT COUNT(*) FROM storage.files f
                WHERE f.blob_hash = b.hash
                  AND NOT EXISTS (SELECT 1 FROM storage.chunk_manifests m2
                                   WHERE m2.file_hash = f.blob_hash))
            + (SELECT COUNT(*) FROM storage.chunk_manifests m3
                WHERE b.hash = ANY(m3.chunk_hashes))))
         FROM storage.blobs b
        WHERE b.hash IN (
              SELECT unnest(chunk_hashes) FROM storage.chunk_manifests
               WHERE file_hash = '$file_hash')
        ORDER BY b.hash;" >&2 || true
  echo "  manifest: $(sql "SELECT 'chunks=' || chunk_count || ' ref_count=' || ref_count FROM storage.chunk_manifests WHERE file_hash = '$file_hash';")" >&2
}

# Dump the job's findings before dying. A run that converted nothing looks
# identical to one that never ran, and `legacy_blob_rechunk_failed` records why
# a blob was skipped — exactly what is needed when this script fails.
#
# Through the API rather than SQL, matching thumb_import_check.sh: the job
# tables' column names are an internal detail, and a diagnostic that breaks
# when they change is worse than none.
dump_findings() {
  local job="${1:-backend_rechunk}" run_id findings
  run_id=$(curl -sf -H "$AUTH" "$base_url/api/admin/jobs/$job/runs?limit=1" 2>/dev/null \
           | jq -r 'if type == "array" then .[0].id else ((.runs // .items // [])[0].id) end // empty')
  [[ -z "$run_id" ]] && { echo "  ($job: no run found)" >&2; return 0; }
  findings=$(curl -sf -H "$AUTH" \
    "$base_url/api/admin/jobs/$job/runs/$run_id/findings?limit=20" 2>/dev/null || echo '[]')
  echo "  $job findings:" >&2
  echo "$findings" | jq -r \
    'if type == "array" then .[] else (.findings // .items // [])[] end
     | "    \(.kind // .finding_kind // "?") \(.details // .detail // {} | tostring)"' 2>/dev/null >&2 \
    || echo "    (unparseable)" >&2
}

TOKEN=$(curl -sf -X POST "$base_url/api/auth/login" \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"$username\",\"password\":\"$password\"}" \
  | jq -r '.access_token')
[[ -n "$TOKEN" && "$TOKEN" != "null" ]] || fail "login failed"
AUTH="Authorization: Bearer $TOKEN"

# ── 0a. Remove leftovers from a previous failed run ──────────────────────
# This script manufactures state by hand and asserts absolute refcounts, so a
# partial run poisons the next one: the folder name collides, and a surviving
# file keeps a reference that makes the counts describe two files instead of
# one. Unlike the hurl suites this is bash, so it can just clean up — and while
# this migration is being iterated on, that is the difference between re-running
# and hand-purging the trash every time.
purge_by_id() {
  local id="$1" kind="$2" tid
  curl -sf -X DELETE "$base_url/api/$kind/$id" -H "$AUTH" >/dev/null 2>&1 || true
  tid=$(curl -sf "$base_url/api/trash/resources" -H "$AUTH" 2>/dev/null \
        | jq -r --arg f "$id" '.items[]? | select(.resource.id == $f) | .resource.id' | head -1)
  [[ -n "${tid:-}" ]] && curl -sf -X DELETE "$base_url/api/trash/$tid" -H "$AUTH" >/dev/null 2>&1 || true
}

stale_folder=$(curl -sf "$base_url/api/folders" -H "$AUTH" 2>/dev/null \
  | jq -r '.[]? | select(.name == "rechunk-legacy-check") | .id' | head -1)
if [[ -n "${stale_folder:-}" ]]; then
  log "cleaning up leftover folder $stale_folder from a previous run"
  while read -r fid; do
    [[ -z "$fid" ]] && continue
    purge_by_id "$fid" files
  done < <(curl -sf "$base_url/api/files?folder_id=$stale_folder" -H "$AUTH" 2>/dev/null \
           | jq -r '.[]?.id // empty')
  purge_by_id "$stale_folder" folders
fi

# ── 0. Baselines for the refcount sweeps ─────────────────────────────────
base_blobs=$(curl -sf -X POST "$base_url/api/admin/jobs/blobs_consistency/trigger" \
  -H "$AUTH" | jq -r '.outcome.count')
base_manifests=$(curl -sf -X POST "$base_url/api/admin/jobs/manifests_consistency/trigger" \
  -H "$AUTH" | jq -r '.outcome.count')
# `backend_consistency?deep=true` reads every blob back and RE-HASHES it. That is
# the strongest assertion available for this migration: the job's entire purpose
# is rewriting how a file's bytes are stored, so "does every chunk still hash to
# its own name" is the question that matters. It is also strictly stronger than
# the download comparison later on, which a warm plaintext cache could satisfy
# without the backend holding correct bytes at all.
base_deep=$(curl -sf -X POST "$base_url/api/admin/jobs/backend_consistency/trigger?deep=true" \
  -H "$AUTH" | jq -r '.outcome.count')
log "baselines: blobs=$base_blobs manifests=$base_manifests deep=$base_deep"

# ── 1. Fixture: >1 MiB of deterministic pseudo-random bytes ──────────────
# Deterministic so a re-run after a partial failure produces the same content
# (and therefore the same hash) instead of stranding a second blob. openssl in
# CTR mode over /dev/zero is a portable seeded stream; /dev/urandom is the
# fallback and only costs determinism.
FIXTURE="$REPO_ROOT/tests/fixtures/rechunk-legacy-3mb.bin"
if [[ ! -s "$FIXTURE" ]]; then
  log "generating fixture → $FIXTURE"
  if command -v openssl >/dev/null 2>&1; then
    head -c 3145728 /dev/zero \
      | openssl enc -aes-256-ctr -pass pass:oxicloud-rechunk-fixture -nosalt 2>/dev/null \
      > "$FIXTURE"
  else
    head -c 3145728 /dev/urandom > "$FIXTURE"
  fi
fi
[[ -s "$FIXTURE" ]] || fail "fixture generation produced nothing"

# ── 2. Upload it through the real API ────────────────────────────────────
folder_id=$(curl -sf -X POST "$base_url/api/folders" -H "$AUTH" \
  -H "Content-Type: application/json" \
  -d '{"name":"rechunk-legacy-check"}' | jq -r '.id')
[[ -n "$folder_id" && "$folder_id" != "null" ]] || fail "folder create failed"

upload=$(curl -sf -X POST "$base_url/api/files/upload" -H "$AUTH" \
  -F "folder_id=$folder_id" \
  -F "file=@$FIXTURE;type=application/octet-stream")
H=$(echo "$upload" | jq -r '.content_hash')
file_id=$(echo "$upload" | jq -r '.id')
[[ -n "$H" && "$H" != "null" ]] || fail "upload returned no content_hash: $upload"
log "uploaded: file=$file_id hash=${H:0:12}"

# Keep a copy of what the user should still be able to download at the end.
EXPECTED_SUM=$(shasum -a 256 "$FIXTURE" | awk '{print $1}')

# ── 3. Guard the fixture: the upload must be multi-chunk ─────────────────
chunks_before=$(sql "SELECT chunk_count FROM storage.chunk_manifests WHERE file_hash = '$H';")
[[ -n "$chunks_before" ]] || fail "no manifest after upload — cannot manufacture legacy state"
(( chunks_before > 1 )) || fail "fixture produced $chunks_before chunk(s); a single-chunk file \
would let a broken migration pass. Increase the fixture size."
log "upload produced $chunks_before chunks"

chunk_hashes=$(sql "SELECT unnest(chunk_hashes) FROM storage.chunk_manifests WHERE file_hash = '$H';")

# ── 4. Manufacture the pre-CDC state ────────────────────────────────────
log "manufacturing legacy whole-file blob"

# (a) the whole-file object a pre-CDC install had
mkdir -p "$(dirname "$(blob_path "$H")")"
cp "$FIXTURE" "$(blob_path "$H")"

# (b) drop the manifest — the "already converted" marker
sql "DELETE FROM storage.chunk_manifests WHERE file_hash = '$H';" >/dev/null

# (c+d) release the reference that manifest held on each chunk, and remove the
#       row and object for chunks nothing references any more.
#
#       Decrement-then-delete rather than a blind DELETE: the fixture's content
#       is unique so its chunks should be unshared, but if one ever IS shared,
#       reaping it would corrupt somebody else's file and the failure would
#       surface far from here. One loop, one hash at a time — no list-to-SQL
#       quoting to get wrong.
removed_chunks=0
kept_chunks=0
while read -r c; do
  [[ -z "$c" ]] && continue
  sql "UPDATE storage.blobs SET ref_count = GREATEST(ref_count - 1, 0) WHERE hash = '$c';" >/dev/null
  remaining=$(sql "SELECT COALESCE(ref_count, -1) FROM storage.blobs WHERE hash = '$c';" | tr -d '[:space:]')
  # Numeric compare, not string: a stray space would make `== "0"` false, the
  # row would silently survive at ref_count 0, and the migration would then see
  # the chunk as already-known — bumping its refcount WITHOUT writing the bytes
  # (`pin_claimable_chunks` drops known chunks from RAM). The manufactured state
  # would be subtly wrong and the failure would surface far from here.
  if [[ "$remaining" =~ ^-?[0-9]+$ ]] && (( remaining == 0 )); then
    sql "DELETE FROM storage.blobs WHERE hash = '$c';" >/dev/null
    rm -f "$(blob_path "$c")"
    removed_chunks=$(( removed_chunks + 1 ))
  else
    kept_chunks=$(( kept_chunks + 1 ))
    log "  chunk ${c:0:12} kept (ref_count=$remaining) — shared with other data"
  fi
done <<< "$chunk_hashes"
log "chunk rows removed=$removed_chunks kept=$kept_chunks"
(( removed_chunks + kept_chunks == chunks_before )) \
  || fail "accounted for $(( removed_chunks + kept_chunks )) of $chunks_before chunks"

# (e) the whole-file row, referenced by our one file
size=$(stat -f%z "$FIXTURE" 2>/dev/null || stat -c%s "$FIXTURE")
sql "INSERT INTO storage.blobs (hash, size, ref_count, content_type)
     VALUES ('$H', $size, 1, 'application/octet-stream')
     ON CONFLICT (hash) DO UPDATE SET ref_count = 1, size = $size;" >/dev/null

# ── 5. The job must SEE it — same predicate it pages through ─────────────
legacy_count=$(sql "SELECT COUNT(*) FROM storage.blobs b
                     WHERE NOT EXISTS (SELECT 1 FROM storage.chunk_manifests m
                                        WHERE m.file_hash = b.hash)
                       AND EXISTS (SELECT 1 FROM storage.files f
                                    WHERE f.blob_hash = b.hash);")
(( legacy_count >= 1 )) || fail "manufactured state is not a legacy candidate (count=$legacy_count)"
log "legacy candidates visible to the job: $legacy_count"

[[ -f "$(blob_path "$H")" ]] || fail "whole-file object missing before the run"

# ── 6. Run the migration ────────────────────────────────────────────────
run=$(curl -sf -X POST "$base_url/api/admin/jobs/backend_rechunk/trigger" -H "$AUTH")
echo "$run" | jq -e '.ok == true' >/dev/null \
  || { dump_findings; fail "trigger did not report ok: $run"; }
echo "$run" | jq -e '.outcome.outcome == "ok"' >/dev/null \
  || { dump_findings; fail "run did not complete: $run"; }
migrated=$(echo "$run" | jq -r '.outcome.extra.extra_stats.migrated // 0')
(( migrated >= 1 )) || { dump_findings; fail "job reported migrated=$migrated, expected >= 1"; }
log "job migrated $migrated blob(s)"

# ── 7. Correctly chunked: a manifest is back, with MORE THAN ONE chunk ───
chunks_after=$(sql "SELECT chunk_count FROM storage.chunk_manifests WHERE file_hash = '$H';")
[[ -n "$chunks_after" ]] || { dump_findings; fail "no manifest after the run — nothing was chunked"; }
(( chunks_after > 1 )) || { dump_findings; fail "manifest has $chunks_after chunk(s); the blob was \
not chunked, it was wrapped"; }
log "re-chunked into $chunks_after chunks"

# ── 8. Content is unchanged — the assertion everything else serves ───────
got_sum=$(curl -sf "$base_url/api/files/$file_id" -H "$AUTH" | shasum -a 256 | awk '{print $1}')
[[ "$got_sum" == "$EXPECTED_SUM" ]] \
  || { dump_findings; fail "download differs after re-chunk: $got_sum != $EXPECTED_SUM"; }
log "download is byte-identical"

# ── 9. The whole-file copy is freed ─────────────────────────────────────
[[ ! -f "$(blob_path "$H")" ]] \
  || fail "whole-file object still on disk — the migration doubled storage instead of moving it"
[[ -z "$(sql "SELECT 1 FROM storage.blobs WHERE hash = '$H';")" ]] \
  || fail "whole-file blob row survived the conversion"
log "whole-file copy freed"

# ── 10. Re-running is a no-op ───────────────────────────────────────────
rerun=$(curl -sf -X POST "$base_url/api/admin/jobs/backend_rechunk/trigger" -H "$AUTH")
again=$(echo "$rerun" | jq -r '.outcome.extra.extra_stats.migrated // 0')
(( again == 0 )) || fail "second run migrated $again blob(s); the manifest is not acting as a \
done marker and the job would re-chunk forever"
log "re-run converted nothing, as expected"

# ── 11. No refcount drift ───────────────────────────────────────────────
now_blobs=$(curl -sf -X POST "$base_url/api/admin/jobs/blobs_consistency/trigger" \
  -H "$AUTH" | jq -r '.outcome.count')
now_manifests=$(curl -sf -X POST "$base_url/api/admin/jobs/manifests_consistency/trigger" \
  -H "$AUTH" | jq -r '.outcome.count')
if [[ "$now_blobs" != "$base_blobs" || "$now_manifests" != "$base_manifests" ]]; then
  dump_refcounts "$H"
  dump_findings blobs_consistency
  dump_findings manifests_consistency
  fail "refcount drift: blobs $base_blobs → $now_blobs, manifests $base_manifests → $now_manifests"
fi
log "no refcount drift"

# ── 11b. Deep pass: every chunk must still hash to its own name ──────────
# The one that would catch a migration corrupting content — a wrong chunk
# boundary, a truncated write, a mis-sized range. `blobs_consistency` cannot:
# it is DB-only by design and never opens a blob. Watch for `blob_corrupted`.
now_deep=$(curl -sf -X POST "$base_url/api/admin/jobs/backend_consistency/trigger?deep=true" \
  -H "$AUTH" | jq -r '.outcome.count')
# `<=`, not `==`, and the reason is specific rather than lax.
#
# `backend_consistency` counts `orphan_blob` — backend objects with no DB row —
# which is exactly what the deletion queue holds between a reap and its drain.
# `backend_reclaim` runs on a 300 s schedule, so during a suite that takes
# minutes it WILL fire and unlink some of those, and the count legitimately DROPS
# between the baseline and here. An equality assertion would fail on a background
# job doing its job.
#
# What this test actually cares about is that the re-chunk did not ADD findings —
# a wrong chunk boundary, a truncated write, a hash that no longer matches its
# name. That is an upper bound, so state it as one.
if [[ "$now_deep" -gt "$base_deep" ]]; then
  dump_findings backend_consistency
  fail "deep backend check grew $base_deep → $now_deep — a re-hash of the migrated chunks \
disagrees with their names"
fi
log "deep re-hash of every chunk agrees"

# ── 12. Teardown. Shared database: leaving rows breaks later suites. ─────
curl -sf -X DELETE "$base_url/api/files/$file_id" -H "$AUTH" >/dev/null || true
trash_id=$(curl -sf "$base_url/api/trash/resources" -H "$AUTH" \
  | jq -r --arg f "$file_id" '.items[] | select(.resource.id == $f) | .resource.id' | head -1)
[[ -n "$trash_id" ]] && curl -sf -X DELETE "$base_url/api/trash/$trash_id" -H "$AUTH" >/dev/null || true

curl -sf -X DELETE "$base_url/api/folders/$folder_id" -H "$AUTH" >/dev/null || true
trash_folder=$(curl -sf "$base_url/api/trash/resources" -H "$AUTH" \
  | jq -r --arg f "$folder_id" '.items[] | select(.resource.id == $f) | .resource.id' | head -1)
[[ -n "$trash_folder" ]] && curl -sf -X DELETE "$base_url/api/trash/$trash_folder" -H "$AUTH" >/dev/null || true

log "PASS"
