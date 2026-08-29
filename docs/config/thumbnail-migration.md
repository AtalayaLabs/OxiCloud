# Thumbnail migration runbook

Thumbnails used to live as files under `{STORAGE_PATH}/.thumbnails/`.
They now live in the content-addressed blob store, alongside file
content. This page is for operators upgrading across that change.

**You do not have to do anything.** The migration runs itself, in the
background, on the first boot after the upgrade. The rest of this page
is for operators who want to verify it, take a safety net first, or
understand what it did.

## What runs, and when

Two background jobs, dispatched once at startup and daily thereafter:

| Job | Migrates | Regenerable if lost? |
|---|---|---|
| `thumb_derived_import` | Thumbnails the server rendered from file content | Yes — the next request re-renders |
| `thumb_attached_import` | Previews a client uploaded (`ext-{file_id}.jpg`) | **No** — there is no render path for these |

Both import each sidecar into blob storage, read it back to confirm the
copy is byte-identical, and only then delete the original. When the
directory is empty it is removed, and `.thumbnails/` stops existing.

Startup dispatch is non-blocking — the server is ready immediately and
the migration proceeds behind it. A run interrupted by a restart resumes
from where it stopped, so a large installation finishes over several
restarts rather than starting again each time.

This is controlled by `OXICLOUD_STARTUP_JOBS`, which defaults to:

```
OXICLOUD_STARTUP_JOBS=thumb_derived_import?repair=true,thumb_attached_import?repair=true
```

To **import without deleting** — migrate now, inspect, delete later:

```
OXICLOUD_STARTUP_JOBS=thumb_derived_import,thumb_attached_import
```

The sidecars then stay on disk. Trigger the deletion when you are ready
from **Admin → Jobs**, using each job's Repair action.

To disable startup jobs entirely, set the variable to an empty value.

## Taking a safety net first

Recommended for any installation where the uploaded previews matter, and
cheap enough to be worth it regardless. Both parts must be captured
together — a database that references blobs a storage snapshot predates
is worse than neither.

**1. Stop the server.** A snapshot taken while writes are in flight can
catch a blob that exists on disk without its database row, or the
reverse.

```bash
systemctl stop oxicloud     # or: docker compose stop oxicloud
```

**2. Snapshot the database.**

```bash
pg_dump --format=custom --file=oxicloud-preflight.dump "$DATABASE_URL"
```

Use `--format=custom`; restoring it needs `pg_restore --disable-triggers`,
because the folder table carries a self-referencing foreign key that a
plain SQL restore cannot order correctly.

**3. Snapshot the storage directory.** At minimum `.thumbnails/`, which
is what the migration touches:

```bash
tar -czf oxicloud-thumbnails-preflight.tar.gz -C "$STORAGE_PATH" .thumbnails
```

A whole-directory snapshot is better if you have the space — filesystem
or volume snapshots (ZFS, LVM, EBS) are ideal, since they are atomic and
near-instant:

```bash
zfs snapshot tank/oxicloud@preflight
```

**4. Start the server.** The migration begins in the background.

Keep both snapshots until you have run the verification below and are
satisfied.

## Verifying the migration

Two checks, both from **Admin → Jobs** or the API. Run them after the
migration reports no remaining work.

**1. Every mapping points at a blob that exists.** Run
`satellites_consistency`. It walks both thumbnail tables and reports any
row whose blob or source is gone. A clean run means nothing was lost in
the bookkeeping.

```
POST /api/admin/jobs/satellites_consistency/trigger
```

**2. Every blob still hashes to what it claims.** Run
`backend_consistency` with `?deep=true`. It reads every blob back from
storage and re-hashes it, which covers the migrated thumbnails along
with everything else. This is a full read of your storage and can take
hours on a large installation — schedule it accordingly.

```
POST /api/admin/jobs/backend_consistency/trigger?deep=true
```

A clean pass on both means the thumbnails are readable, correctly
referenced, and byte-intact in their new home. At that point the
snapshots can be discarded.

## Checking it finished

`.thumbnails/` is gone. That is the whole test:

```bash
ls -d "$STORAGE_PATH/.thumbnails"      # No such file or directory
```

If you instead find `.thumbnails.migrated/`, the migration completed but
could not remove the directory, because something that is not a
thumbnail was inside it — a `.DS_Store` from macOS Finder is the usual
culprit. The tree was moved aside instead of deleted. Its contents are
no longer used and it is safe to remove by hand once you have looked at
what is in there.

While either directory is absent, the server skips the legacy read path
entirely, at no cost. While `.thumbnails/` is present, reads fall back
to it on a miss, which is what makes the migration invisible to users
while it runs.

## If something looks wrong

Every deletion is written to the audit log, naming the job, the file
removed and the blob that replaced it. To review what a migration
removed:

```bash
journalctl -u oxicloud | grep sidecar_deleted
```

A sidecar is only ever deleted after its replacement has been read back
and compared byte-for-byte, so a file that failed that check is still on
disk. Those show up as findings on the job's run in **Admin → Jobs**,
with the reason recorded per file.
