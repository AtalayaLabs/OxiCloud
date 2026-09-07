#!/bin/bash
# scripts/restore.sh — restore a pg_dump custom-format snapshot into a
# fresh oxicloud DB.
#
# Drops + recreates the whole DB before restoring so migrations added
# AFTER the dump was taken don't block --clean's DROPs. Symptom of
# that class (hit 2026-08-30):
#
#     pg_restore: erreur : ...
#     cannot drop constraint files_pkey on table storage.files because
#     other objects depend on it
#     DÉTAIL : constraint file_attached_blobs_file_id_fkey on table
#     storage.file_attached_blobs depends on index storage.files_pkey
#
# The dump only knows about objects that existed at dump time;
# `pg_restore --clean` DROPs exactly those. Anything added since
# (`file_attached_blobs` in the example above) survives and blocks
# DROPs of things it depends on. Drop-and-recreate the whole DB
# sidesteps the problem entirely.
#
# Usage:
#   ./scripts/restore.sh <dump-file>
#
# Companion of the pg_dump command in memory
# bug_pg_dump_folders_circular_fk.md:
#     pg_dump postgres://... -F c --disable-triggers > backup.$NOW.dump
#
# NB: this restores the DB ONLY. Blob storage on disk
# (${OXICLOUD_STORAGE_PATH}) is NOT touched — snapshot + restore that
# separately with rsync if you need lockstep DB/disk state.

set -euo pipefail

DUMP="${1:?usage: $0 <dump-file>}"
[[ -f "$DUMP" ]] || { echo "[restore] ERROR: dump file not found: $DUMP" >&2; exit 1; }

# Admin connection — connect to the `postgres` maintenance DB so we can
# drop `oxicloud` itself (can't drop the DB you're connected to). Both
# connection strings share credentials from the sandbox setup.
ADMIN="postgres://postgres:postgres@localhost:5432/postgres"
TARGET="postgres://postgres:postgres@localhost:5432/oxicloud"

# 1. Terminate every connection to `oxicloud` so DROP DATABASE can
#    proceed. OxiCloud running against 5432? rust-analyzer with
#    sqlx-cli open? Any lingering psql session? All of them block
#    `DROP DATABASE` with "database is being accessed by other users".
#    pg_terminate_backend kicks them cleanly (they'll reconnect if
#    they retry).
echo "[restore] Terminating connections to oxicloud..."
psql "$ADMIN" -c "
  SELECT pg_terminate_backend(pid)
    FROM pg_stat_activity
   WHERE datname = 'oxicloud'
     AND pid <> pg_backend_pid();
" >/dev/null

# 2. Drop + recreate. IF EXISTS on DROP so a fresh workstation without
#    an existing `oxicloud` database doesn't error on the first-ever
#    invocation.
echo "[restore] Dropping + recreating oxicloud database..."
psql "$ADMIN" <<'SQL'
DROP DATABASE IF EXISTS oxicloud;
CREATE DATABASE oxicloud OWNER postgres;
SQL

# 3. Restore. `--clean --if-exists` no longer needed (fresh DB from
#    step 2). The remaining flags:
#
#    * --disable-triggers — turn triggers off during data load so the
#      `storage.folders.parent_id` self-FK doesn't reject rows whose
#      parent hasn't been inserted yet in the same COPY batch. See
#      memory bug_pg_dump_folders_circular_fk for background.
#    * --single-transaction — atomic restore (all-or-nothing) AND
#      lets --disable-triggers work without superuser (superuser is
#      required otherwise).
#    * --no-owner --no-privileges — portable, ignores ownership /
#      GRANTs from the source system so a dump taken from one machine
#      restores cleanly on another with different user names.
echo "[restore] Restoring from $DUMP..."
pg_restore \
    --disable-triggers \
    --single-transaction \
    --no-owner \
    --no-privileges \
    -d "$TARGET" \
    "$DUMP"

echo "[restore] Done. Database restored to snapshot state in $DUMP."
