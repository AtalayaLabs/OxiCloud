#!/usr/bin/env bash
# =============================================================
# OxiCloud – Storage disk-cleanup verification
# =============================================================
# 1. Moves every live file and folder to trash via the REST API.
# 2. Calls DELETE /api/trash/empty to permanently delete all
#    remaining trash items (including any left by previous tests).
# 3. Asserts that no regular files remain under
#    $OXICLOUD_STORAGE_PATH/.thumbnails or .blobs.
#
# Called by run.sh after all Hurl tests have passed.
# Can also be run standalone (server must already be up):
#   bash tests/api/storage_cleanup_check.sh
# =============================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
STORAGE_PATH="${OXICLOUD_STORAGE_PATH:-$REPO_ROOT/tests/api/storage}"

# shellcheck source=test.env
source "$SCRIPT_DIR/test.env"

log()  { echo "[storage-check] $*"; }
fail() { echo $'\e[31m'"[storage-check] FAIL: $*"$'\e[0m' >&2; exit 1; }

# ── 1. Login ──────────────────────────────────────────────────────────────────

TOKEN=$(curl -sf -X POST "$base_url/api/auth/login" \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"$username\",\"password\":\"$password\"}" \
  | jq -r '.access_token')

[[ -z "$TOKEN" || "$TOKEN" == "null" ]] && fail "login failed"
log "Logged in."

AUTH="Authorization: Bearer $TOKEN"

# ── 1b. Upload a probe image and verify its blob + thumbnail exist on disk ─────

# shellcheck source=../common/internal_storage_helper.sh
source "$REPO_ROOT/tests/common/internal_storage_helper.sh"

FIXTURE="$REPO_ROOT/tests/fixtures/blue-image.png"

HOME_FOLDER_ID=$(curl -sf -H "$AUTH" "$base_url/api/folders" | jq -r '.[0].id')
[[ -z "$HOME_FOLDER_ID" || "$HOME_FOLDER_ID" == "null" ]] && fail "could not get home folder id"

PROBE_FILE_ID=$(curl -sf -X POST -H "$AUTH" \
    -F "folder_id=$HOME_FOLDER_ID" \
    -F "file=@$FIXTURE;type=image/png" \
    "$base_url/api/files/upload" | jq -r '.id')
[[ -z "$PROBE_FILE_ID" || "$PROBE_FILE_ID" == "null" ]] && fail "probe file upload failed"
log "Probe file uploaded (id=$PROBE_FILE_ID)."

# GET thumbnail to trigger on-demand generation
HTTP_STATUS=$(curl -sf -o /dev/null -w "%{http_code}" -H "$AUTH" \
    "$base_url/api/files/$PROBE_FILE_ID/thumbnail/icon")
[[ "$HTTP_STATUS" != "200" ]] && fail "thumbnail GET returned HTTP $HTTP_STATUS (expected 200)"
log "Thumbnail fetched (HTTP 200)."

assert_local_blob_existsy "$FIXTURE" "$STORAGE_PATH" || fail "probe blob not found on disk"
assert_preview_existsy    "$FIXTURE" "$STORAGE_PATH" || fail "probe thumbnail not found on disk"
log "Probe blob and thumbnail confirmed present on disk."

# ── 1c. Delete every non-admin user created by earlier Hurl tests ─────────────
#
# Tests like permissions.hurl and grants.hurl create user accounts (bob,
# dave, eve, adam, frank, …) that own their own folders/files. The probe
# cleanup below only sees admin-owned roots, so those other users' files
# would leak as orphan blobs on disk. Deleting the users cascades through
# the schema (storage.folders/storage.files via ON DELETE CASCADE), which
# fires the file-delete trigger and decrements blob ref_counts. The
# subsequent trash-empty triggers garbage_collect() to remove the
# now-orphaned blob files from disk.

# /api/admin/users returns { users: [FullUserDto…], total, limit, offset }
# — public identity nests under `.user`; admin-visible extras
# (`storage_used_bytes`, `last_login_at`, …) sit at the top level of
# each row. See `docs/plan/userdto-refactor.md`.
USERS_JSON=$(curl -sf -H "$AUTH" "$base_url/api/admin/users?limit=500")

ADMIN_USER_ID=$(echo "$USERS_JSON" \
    | jq -r --arg u "$username" '.users[] | select(.user.username == $u) | .user.id')
[[ -z "$ADMIN_USER_ID" || "$ADMIN_USER_ID" == "null" ]] && fail "could not resolve admin user id"

OTHER_USER_IDS=$(echo "$USERS_JSON" \
    | jq -r --arg admin_id "$ADMIN_USER_ID" '.users[] | select(.user.id != $admin_id) | .user.id')

OTHER_USER_COUNT=0
while IFS= read -r uid; do
    [[ -z "$uid" ]] && continue
    OTHER_USER_COUNT=$((OTHER_USER_COUNT + 1))
    curl -sf -X DELETE -H "$AUTH" "$base_url/api/admin/users/$uid" >/dev/null \
        || fail "failed to delete user $uid"
done <<< "$OTHER_USER_IDS"

log "Deleted $OTHER_USER_COUNT non-admin user(s) created by tests."

# ── 1d. Drain every non-default drive ─────────────────────────────────────────
#
# Hurl tests that create shared drives (or future secondary personal
# drives) for OTHER users can leave them dangling: file rows in those
# drives carry `user_id = <root_folder.user_id>` — which is the admin
# who CREATED the drive, not the uploader — so the legacy
# `storage.files.user_id` FK CASCADE we leaned on above does NOT reach
# them when the test user is deleted. The drive + its content stays
# live → blobs stay referenced → garbage_collect() leaves them on
# disk → the leftover-detector at the end of this script fails.
#
# Strategy: enumerate every drive via the admin-wide listing, grant
# admin Owner role on each non-default drive (admin-bypass route from
# D2a), then walk the drive's root via the regular Owner-side
# endpoints, trash everything, empty per-drive trash, and delete the
# drive itself. With the drive gone, its blobs lose all references
# and the force-GC at the end of the script reaps them.
#
# Skipped: the admin's OWN default-personal drive — that's drained
# above by the existing `/api/folders` loop.

ADMIN_DRIVE_IDS=$(curl -sf -H "$AUTH" "$base_url/api/admin/drives" \
    | jq -r --arg admin_id "$ADMIN_USER_ID" \
        '.[] | select(.default_for_user != $admin_id) | .id')

DRAINED_DRIVES=0
while IFS= read -r drive_id; do
    [[ -z "$drive_id" ]] && continue

    # Drive metadata — we need the root_folder_id to drain its content.
    DRIVE_META=$(curl -sf -H "$AUTH" "$base_url/api/admin/drives" \
                    | jq --arg id "$drive_id" '.[] | select(.id == $id)')
    DRIVE_ROOT=$(echo "$DRIVE_META" | jq -r '.root_folder_id')
    DRIVE_NAME=$(echo "$DRIVE_META" | jq -r '.name')

    # Grant admin Owner on the drive (admin-bypass — `caller_is_admin =
    # true` skips the `Manage` precheck so we don't need to already
    # have a role). Idempotent: if admin is already Owner, the call
    # refreshes the role.
    curl -sf -X POST -H "$AUTH" -H "Content-Type: application/json" \
        -d "{\"subject\":{\"type\":\"user\",\"id\":\"$ADMIN_USER_ID\"},\"role\":\"owner\"}" \
        "$base_url/api/admin/drives/$drive_id/members" >/dev/null \
        || fail "could not grant admin Owner on drive $drive_id ($DRIVE_NAME)"

    # Now drain the drive's root through the regular user-facing
    # endpoints. Admin is Owner → Read passes.
    CONTENTS=$(curl -sf -H "$AUTH" "$base_url/api/folders/$DRIVE_ROOT/resources?limit=500")

    while IFS= read -r sub_id; do
        [[ -z "$sub_id" ]] && continue
        curl -sf -X DELETE -H "$AUTH" "$base_url/api/folders/$sub_id" >/dev/null
    done < <(echo "$CONTENTS" | jq -r '.items[] | select(.resource_type == "folder") | .resource.id')

    while IFS= read -r file_id; do
        [[ -z "$file_id" ]] && continue
        HTTP_STATUS=$(curl -s -H "$AUTH" -o /tmp/del.json -w '%{http_code}' \
                          -X DELETE "$base_url/api/files/$file_id")
        if [[ "$HTTP_STATUS" != "204" ]]; then
            log "FILE DELETE FAILED: file=$file_id drive=$drive_id ($DRIVE_NAME) status=$HTTP_STATUS body=$(cat /tmp/del.json)"
        fi
    done < <(echo "$CONTENTS" | jq -r '.items[] | select(.resource_type == "file") | .resource.id')


    # Empty the drive's per-drive trash so D3b's "drive must be empty"
    # guard passes on the delete. `/api/trash/drive/{id}` is the
    # Owner-only per-drive empty (admin is Owner now via the grant
    # above).
    curl -sf -X DELETE -H "$AUTH" "$base_url/api/trash/drive/$drive_id" >/dev/null \
        || fail "could not empty trash on drive $drive_id ($DRIVE_NAME)"

    # Delete the drive itself via the admin-bypass DELETE.
    curl -sf -X DELETE -H "$AUTH" "$base_url/api/admin/drives/$drive_id" >/dev/null \
        || fail "could not delete drive $drive_id ($DRIVE_NAME)"

    DRAINED_DRIVES=$((DRAINED_DRIVES + 1))
done <<< "$ADMIN_DRIVE_IDS"

log "Drained + deleted $DRAINED_DRIVES non-default drive(s)."

# ── 2. Move all live files and folders to trash ───────────────────────────────
#
# For each root folder, list its direct children and soft-delete them.
# The server cascades folder deletion to all nested contents, so we only
# need to iterate one level deep.

ROOT_FOLDERS=$(curl -sf -H "$AUTH" "$base_url/api/folders" | jq -r '.[].id')

# `/api/folders/{id}/resources` superseded the legacy `/listing` route
# (commit 5790a145). Response shape:
#   { "items": [ { "resource_type": "folder"|"file",
#                  "resource": { "id": "<uuid>", … } } ],
#     "next_cursor": "…" }
# We trash one level deep — the server cascades into children.
#
# `GET /api/folders` (root listing) still uses the legacy
# `user_id`-keyed query, so it can surface folders the admin
# *created* but doesn't have a role on (e.g. shared drives spawned by
# `drive_quota.hurl` for other users). Those return 404 on
# `/resources` (no Read in the role bundle). Skip them — they aren't
# admin's content to drain.
for folder_id in $ROOT_FOLDERS; do
    RES_HTTP=$(curl -s -H "$AUTH" -o /tmp/storage_cleanup_resources.json \
                    -w "%{http_code}" \
                    "$base_url/api/folders/$folder_id/resources?limit=500")
    if [[ "$RES_HTTP" == "404" ]]; then
        log "Skipping folder $folder_id (404 on /resources — not readable by admin)"
        continue
    fi
    if [[ "$RES_HTTP" != "200" ]]; then
        fail "/api/folders/$folder_id/resources returned HTTP $RES_HTTP"
    fi
    CONTENTS=$(cat /tmp/storage_cleanup_resources.json)

    while IFS= read -r sub_id; do
        [[ -z "$sub_id" ]] && continue
        curl -sf -X DELETE -H "$AUTH" "$base_url/api/folders/$sub_id" >/dev/null
    done < <(echo "$CONTENTS" | jq -r '.items[] | select(.resource_type == "folder") | .resource.id')

    while IFS= read -r file_id; do
        [[ -z "$file_id" ]] && continue
        curl -sf -X DELETE -H "$AUTH" "$base_url/api/files/$file_id" >/dev/null
    done < <(echo "$CONTENTS" | jq -r '.items[] | select(.resource_type == "file") | .resource.id')
done

log "All live objects moved to trash."

# ── 2b. Verify all root folders are empty according to the API ────────────────

for folder_id in $ROOT_FOLDERS; do
    RES_HTTP=$(curl -s -H "$AUTH" -o /tmp/storage_cleanup_resources.json \
                    -w "%{http_code}" \
                    "$base_url/api/folders/$folder_id/resources?limit=500")
    # Same skip-on-404 as the trash loop above — admin owns the row but
    # has no role-grant Read on it (shared drive created for someone else).
    if [[ "$RES_HTTP" == "404" ]]; then
        continue
    fi
    if [[ "$RES_HTTP" != "200" ]]; then
        fail "/api/folders/$folder_id/resources returned HTTP $RES_HTTP"
    fi
    CONTENTS=$(cat /tmp/storage_cleanup_resources.json)
    SUB_COUNT=$(echo  "$CONTENTS" | jq '[.items[] | select(.resource_type == "folder")] | length')
    FILE_COUNT=$(echo "$CONTENTS" | jq '[.items[] | select(.resource_type == "file")]   | length')
    if [[ "$SUB_COUNT" -ne 0 || "$FILE_COUNT" -ne 0 ]]; then
        fail "folder $folder_id still has $SUB_COUNT subfolder(s) and $FILE_COUNT file(s)"
    fi
done

log "API confirms all root folders are empty."

# ── 3. Permanently delete everything in trash ─────────────────────────────────

curl -sf -X DELETE -H "$AUTH" "$base_url/api/trash/empty" >/dev/null
log "Trash emptied."

# ── 3b. Verify trash is empty according to the API ───────────────────────────

TRASH_COUNT=$(curl -sf -H "$AUTH" "$base_url/api/trash/resources" | jq '.items | length')
if [[ "$TRASH_COUNT" -ne 0 ]]; then
    fail "trash still contains $TRASH_COUNT item(s) after empty"
fi

log "API confirms trash is empty."

# ── 3c. Force the maintenance sweeps synchronously ────────────────────────────
#
# `trash/empty` already triggers an inline `garbage_collect()` at the end of
# its `clear_trash_in` path, but that GC honours the 1-hour orphan-grace
# window — a blob orphaned seconds ago survives the inline sweep. The
# regular periodic sweep would catch it eventually, but tests need the
# disk state to be quiescent NOW. The two JobRegistry admin triggers
# below (production surface, always on) make this deterministic:
#
#   1. usage_reconcile  — reconciles users.storage_used_bytes and
#                           drives.used_bytes from SUM(size) — keeps
#                           the cached counters honest for any quota
#                           assertions that follow.
#   2. dedup_gc?force=true — same `garbage_collect()` as the inline
#                           call, but `force=true` bypasses the
#                           orphan grace so freshly-orphaned blobs
#                           ARE reaped. Safe here because the test
#                           has no concurrent uploaders to race the
#                           row-delete → unlink window the grace
#                           normally protects.

curl -sf -X POST -H "$AUTH" "$base_url/api/admin/jobs/usage_reconcile/trigger" >/dev/null \
    || fail "usage_reconcile trigger failed"
log "Reconciliation sweep triggered."

# One GC pass is NOT enough, and this is by design rather than a bug.
# Reaping a source blob releases the references its DERIVED artifacts hold
# (thumbnails live in storage.content_derived_blobs and each row pins a
# manifest). Those releases happen mid-sweep, so the derived chunks are
# only stamped orphaned as the pass is already walking past them —
# `remove_manifest_reference` deliberately does not unlink, to avoid racing
# a concurrent upload re-referencing the same chunk. They become
# collectible on the NEXT sweep.
#
# Loop until a pass reclaims nothing rather than hardcoding two passes.
# Two is correct only while the derivation graph is one level deep — a
# thumbnail is derived from a file and nothing is derived from a thumbnail.
# That is a property of the data, not an invariant the code enforces, so a
# fixed count would silently under-drain the day transcodes-of-thumbnails
# or E2E-wrapped derivatives appear, and the failure would surface as a
# confusing leftover-file assertion rather than as the design change it is.
#
# Production does NOT need this loop: derived chunks land inside the 1-hour
# orphan grace, so a second immediate pass would collect nothing and the
# next scheduled sweep picks them up. It is only `force=true` (grace 0)
# that can drain a cascade in one go, which is exactly this test.
GC_TOTAL_BLOBS=0
GC_TOTAL_BYTES=0
GC_DRAINED=0
for gc_pass in 1 2 3; do
    GC_RESULT=$(curl -sf -X POST -H "$AUTH" "$base_url/api/admin/jobs/dedup_gc/trigger?force=true")
    [[ -z "$GC_RESULT" ]] && fail "trigger-gc returned an empty body (pass $gc_pass)"
    GC_BLOBS=$(echo "$GC_RESULT" | jq -r '.outcome.count // 0')
    GC_BYTES=$(echo "$GC_RESULT" | jq -r '.outcome.extra.bytes_reclaimed // 0')
    GC_TOTAL_BLOBS=$((GC_TOTAL_BLOBS + GC_BLOBS))
    GC_TOTAL_BYTES=$((GC_TOTAL_BYTES + GC_BYTES))
    log "GC pass $gc_pass reaped $GC_BLOBS blob(s), $GC_BYTES byte(s) freed."
    if [[ "$GC_BLOBS" -eq 0 ]]; then
        GC_DRAINED=1
        break
    fi
    # Breathe before the next trigger, for two reasons:
    #
    #   * The JobRegistry serialises runs of the same job. Firing the next
    #     trigger before the previous run has fully unwound risks it being
    #     rejected as already-running — which would come back as 0 reaped
    #     and exit this loop early, declaring success with blobs still on
    #     disk. A false pass is worse than a slow one.
    #   * `on_blob_deleted` spawns detached unlink tasks that nothing
    #     awaits, so some of the previous pass's disk work may still be in
    #     flight.
    sleep 1
done
if [[ "$GC_DRAINED" -ne 1 ]]; then
    log "WARNING: GC still reaping after 3 passes — the derivation graph may"
    log "         be deeper than one level; raise the bound and check why."
fi
log "GC total: $GC_TOTAL_BLOBS blob(s), $GC_TOTAL_BYTES byte(s) freed."

# ── 4. Disk verification ──────────────────────────────────────────────────────

THUMB_FILES=$(find "$STORAGE_PATH/.thumbnails" -type f 2>/dev/null || true)
BLOB_FILES=$(find  "$STORAGE_PATH/.blobs"      -type f 2>/dev/null || true)

if [[ -n "$THUMB_FILES" || -n "$BLOB_FILES" ]]; then
    # Even with the synchronous sweep + force-GC above, the on-disk
    # unlink for thumbnails/blobs is handled by async workers that may
    # still be draining when this `find` runs. Keep the short
    # retry loop as a race guard. TODO: replace with a deterministic
    # worker-drain signal (e.g. queue depth on /ready) when one exists.
    log "Thumb/blob leftovers detected — polling for async worker drain (race guard)"
    for attempt in 1 2 3 4 5; do
        sleep 1
        THUMB_FILES=$(find "$STORAGE_PATH/.thumbnails" -type f 2>/dev/null || true)
        BLOB_FILES=$(find  "$STORAGE_PATH/.blobs"      -type f 2>/dev/null || true)
        [[ -z "$THUMB_FILES" && -z "$BLOB_FILES" ]] && break
        log "  attempt $attempt: still present, retrying..."
    done
fi

# Chunked-upload spool. After every chunked-upload session is either
# completed (assembled + promoted) or aborted, this dir MUST be empty
# — a leftover chunk file means a session-cleanup path forgot its
# `remove_dir_all`, which under sustained sync workloads is the
# classic "disk fills up over the weekend" failure mode.
#
# `.uploads/` is the default chunked-upload root when
# `OXICLOUD_CHUNK_DIR` is unset (see `common/di.rs`). REST sessions
# land under `.uploads/<session_id>/`; NC sessions land under
# `.uploads/nextcloud/<user>/<session_id>/`.
#
# We deliberately do NOT check the direct-PUT spool dir here: when
# `OXICLOUD_UPLOAD_TEMP_DIR` is unset (the default in the test env)
# it falls back to the OS temp dir (`/tmp/…`) which is shared with
# the rest of the system and would produce false positives. To
# extend the check to direct-PUT, set OXICLOUD_UPLOAD_TEMP_DIR in
# tests/common/server.env to a path under $STORAGE_PATH and add it
# to the find list below.
UPLOAD_FILES=$(find "$STORAGE_PATH/.uploads"  -type f 2>/dev/null || true)

if [[ -n "$THUMB_FILES" ]]; then
    THUMB_COUNT=$(echo "$THUMB_FILES" | wc -l | tr -d ' ')
    log "Leftover thumbnail files ($THUMB_COUNT):"
    echo "$THUMB_FILES"
    fail "$THUMB_COUNT thumbnail file(s) remain on disk after full cleanup"
fi

if [[ -n "$BLOB_FILES" ]]; then
    BLOB_COUNT=$(echo "$BLOB_FILES" | wc -l | tr -d ' ')
    log "Leftover blob files ($BLOB_COUNT):"
    echo "$BLOB_FILES"
    fail "$BLOB_COUNT blob file(s) remain on disk after full cleanup"
fi

if [[ -n "$UPLOAD_FILES" ]]; then
    UPLOAD_COUNT=$(echo "$UPLOAD_FILES" | wc -l | tr -d ' ')
    log "Leftover chunked-upload files ($UPLOAD_COUNT):"
    echo "$UPLOAD_FILES"
    fail "$UPLOAD_COUNT chunked-upload file(s) remain in .uploads after full cleanup"
fi

log "OK — no blobs, thumbnails, or chunked-upload leftovers remain on disk."

# ── 5. Whole-suite consistency sweep ──────────────────────────────────────────
#
# The disk checks above prove nothing LEAKED. These prove the bookkeeping
# behind it is honest — that every refcount matches what the reference
# sources actually hold, and that no row points at bytes that are gone.
#
# End of suite is the right place, and the only place it is cheap. One
# database serves every hurl file (which is why each must tear down after
# itself), so by the time we get here the counters have absorbed every
# upload, copy, move, share, trash and purge the suite performed — across
# both copy paths, the derived tier and the attached tier. A drift that no
# single test would notice, because each only inspects its own file, shows
# up here as a mismatch.
#
# It runs AFTER the GC drain deliberately: mid-sweep state is legitimately
# inconsistent (a manifest can sit at zero waiting for the next pass), so
# checking before the drain would report normal in-flight state as drift.
#
# Zero findings is the assertion. These jobs are read-only, so a finding
# here is a real invariant violation, not a repair opportunity.

CONSISTENCY_JOBS=(
    files_consistency
    blobs_consistency
    manifests_consistency
    backend_consistency
)

CONSISTENCY_FAILED=0
for job in "${CONSISTENCY_JOBS[@]}"; do
    TRIGGER=$(curl -sf -X POST -H "$AUTH" "$base_url/api/admin/jobs/$job/trigger") \
        || { log "WARNING: $job not registered in this build — skipped"; continue; }
    [[ -z "$TRIGGER" ]] && { log "WARNING: $job returned an empty body — skipped"; continue; }

    # The trigger is synchronous for these tenants, but the run row is what
    # carries the findings, so read it back rather than trusting the
    # trigger's own summary.
    # `list_job_runs` returns a bare JSON array, newest first. The `.runs` /
    # `.items` fallbacks are there so a future wrapping of the response does
    # not silently turn this check into a no-op.
    RUN_ID=$(curl -sf -H "$AUTH" "$base_url/api/admin/jobs/$job/runs?limit=1" \
             | jq -r 'if type == "array" then .[0].id
                      else ((.runs // .items // [])[0].id) end // empty')
    if [[ -z "$RUN_ID" ]]; then
        log "WARNING: could not resolve a run id for $job — skipped"
        continue
    fi

    FINDINGS=$(curl -sf -H "$AUTH" \
        "$base_url/api/admin/jobs/$job/runs/$RUN_ID/findings?limit=100")
    COUNT=$(echo "$FINDINGS" | jq -r \
        'if type == "array" then length
         else ((.findings // .items // []) | length) end')

    if [[ "$COUNT" -gt 0 ]]; then
        log "$job reported $COUNT finding(s):"
        echo "$FINDINGS" | jq -r \
            'if type == "array" then .[] else (.findings // .items // [])[] end
             | "    \(.severity // "?") \(.kind // .finding_kind // "?") \(.details // {} | tostring)"' \
            2>/dev/null | head -20
        CONSISTENCY_FAILED=1
    else
        log "$job: clean."
    fi
done

if [[ "$CONSISTENCY_FAILED" -eq 1 ]]; then
    fail "consistency jobs reported findings after the full suite — see above"
fi

log "OK — all consistency jobs clean after the full suite."
