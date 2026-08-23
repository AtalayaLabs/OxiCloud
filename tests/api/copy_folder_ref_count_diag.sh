#!/usr/bin/env bash
# =============================================================
# copy_folder_ref_count.hurl — post-failure diagnostic
# =============================================================
# When `copy_folder_ref_count.hurl` asserts a specific
# `ref_count` value and the API returns something else, this
# script inspects the two DB tables the API surface consults
# to distinguish which side is broken:
#
#   1. `storage.chunk_manifests.ref_count` — queried FIRST by
#      `dedup_service::get_blob_metadata` (`dedup_service.rs`
#      :1543-1552). If a manifest row exists for the hash,
#      the API returns THIS ref_count.
#   2. `storage.blobs.ref_count` — legacy whole-file fallback,
#      returned only when NO manifest row exists.
#
# Small files (< CDC min chunk size) still get a manifest row
# — one degenerate chunk with `chunk_hashes = [file_hash]` —
# so BOTH tables carry a ref_count for the same hash. When the
# copy or purge path only updates one of the two, the counters
# diverge.
#
# The `actual_auditor` column is the source of truth: the
# `blobs_consistency` auditor formula counting live references
# from `storage.files` + `chunk_manifests.chunk_hashes[]`
# (`blobs_consistency_service.rs:395-408`). Both stored values
# should equal this.
#
# Bug interpretation matrix (S=stored, M=manifest, A=auditor):
#
#   S == M == A                → consistent (test bug, unlikely)
#   S < A  and M == A          → blob decrement over-fires
#   S == A and M > A           → manifest decrement missed
#   S > A  and M > A           → decrement missed both sides
#   S < A  and M < A           → double-decrement both sides
#   S > A  and M == A          → increment missed on blob side
#   S == A and M < A           → increment missed on manifest
#   (S=0, M=2, A=1) [seen 8/23]→ blob double-decrement +
#                                 manifest never decremented
#
# The 2026-08-22 sandbox drift showed the raw shape
# (`stored=0, actual=1`) that this diagnostic now separates
# per-table.
#
# Called from `run.sh` immediately on hurl failure — see the
# dedicated `if ! hurl …; then bash …_diag.sh; fi` block.
# =============================================================

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# Same connection string every other test-time script uses.
export PGPASSWORD=oxicloud_test
PSQL=(psql -h 127.0.0.1 -p 5433 -U oxicloud_test -d oxicloud_test
      --set ON_ERROR_STOP=1 --pset pager=off)

SMALL_HASH='2d8eb13178cff0036a22e0c3c42446061e86579f9d59ea73c8343bccc2df0fd3'
CDC_HASH='fb1e63c28bb792e0f69cd16cd7595989f83c218cf70894e07d1f811ab1dc6f83'

log() { echo "[ref_count-diag] $*"; }

log "─────────────────────────────────────────────────────────"
log "copy_folder_ref_count.hurl failed — running diagnostic."
log "Shows blob.ref_count AND manifest.ref_count side by side —"
log "the API queries manifest first (dedup_service.rs:1543), so"
log "if the two diverge, the API surface + auditor + on-disk"
log "state all report different numbers. See script header."
log "─────────────────────────────────────────────────────────"

# Full picture for each fixture hash — one row per hash.
# LEFT JOINs so a hash present in only one table still surfaces
# (the other column comes back NULL, which is itself diagnostic).
"${PSQL[@]}" <<SQL
SELECT
    h.hash,
    b.ref_count                                       AS blob_stored,
    m.ref_count                                       AS manifest_stored,
    (
        (SELECT COUNT(*) FROM storage.files f
          WHERE f.blob_hash = h.hash
            AND NOT EXISTS (
                SELECT 1 FROM storage.chunk_manifests mm
                 WHERE mm.file_hash = f.blob_hash
            ))
      + (SELECT COUNT(*) FROM storage.chunk_manifests mm
          WHERE h.hash = ANY(mm.chunk_hashes))
    )                                                 AS actual_auditor,
    (SELECT COUNT(*) FROM storage.files f
      WHERE f.blob_hash = h.hash)                     AS all_files_rows,
    (SELECT COUNT(*) FROM storage.files f
      WHERE f.blob_hash = h.hash AND f.is_trashed = TRUE)
                                                      AS trashed_files,
    b.size                                            AS blob_size,
    m.total_size                                      AS manifest_size
  FROM (VALUES ('$SMALL_HASH'), ('$CDC_HASH')) AS h(hash)
  LEFT JOIN storage.blobs           b ON b.hash      = h.hash
  LEFT JOIN storage.chunk_manifests m ON m.file_hash = h.hash;
SQL
psql_status=$?

if [[ $psql_status -ne 0 ]]; then
    log "psql query failed (exit $psql_status) — DB may already be torn down"
    exit 0  # don't mask the hurl failure with a psql error
fi

# Show the offending file rows too — helps confirm whether the
# copy's file row exists and where (source folder vs copy).
log "─────────────────────────────────────────────────────────"
log "File rows referencing the fixture hashes (name → folder id)"
log "─────────────────────────────────────────────────────────"
"${PSQL[@]}" <<SQL
SELECT
    f.blob_hash,
    f.name,
    f.folder_id,
    f.is_trashed,
    f.trashed_at,
    f.created_at
  FROM storage.files f
 WHERE f.blob_hash IN ('$SMALL_HASH', '$CDC_HASH')
 ORDER BY f.blob_hash, f.created_at;
SQL

log "─────────────────────────────────────────────────────────"
log "Interpretation (compare blob_stored, manifest_stored, actual_auditor):"
log "  all three equal                → consistent (test bug, unlikely)"
log "  blob=A, manifest>A             → manifest decrement missed"
log "  blob<A, manifest=A             → blob decrement over-fires"
log "  blob<A, manifest>A             → double-decrement on blob +"
log "                                    no-decrement on manifest"
log "                                    (matches the 2026-08-23 case:"
log "                                     blob=0, manifest=2, actual=1)"
log "  blob>A, manifest=A             → blob increment missed"
log "  blob=A, manifest<A             → manifest increment missed"
log "  Either column NULL             → row absent from that table"
log ""
log "The API surface (`/api/dedup/check/{hash}`) queries manifest"
log "FIRST — that's why hurl saw manifest_stored while auditor +"
log "on-disk state ran off blob_stored."
log "─────────────────────────────────────────────────────────"
