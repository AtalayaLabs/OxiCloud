# Plan — Backup and restore: one bundle format, two profiles

**Status:** design captured 2026-10-15, **not started**. Triggered by the state
of the current tooling: [`scripts/backup.sh`](../../scripts/backup.sh) is five
lines of hardcoded sandbox `pg_dump` with no error handling, and
[`scripts/restore.sh`](../../scripts/restore.sh) — which is careful and
well-reasoned — covers the database only, and says so:

> this restores the DB ONLY. Blob storage on disk (`${OXICLOUD_STORAGE_PATH}`)
> is NOT touched — snapshot + restore that separately with rsync if you need
> lockstep DB/disk state.

So today there is no way to back up the blob store, and no documented way to
obtain a consistent database/blob pair. For a product whose stated first design
axis is **no data loss, no data corruption, ever**, and whose ref-counting and
content-addressing are load-bearing rather than decorative, that is the gap.

Two use cases drove this design and both are first-class:

1. **Operator disaster recovery** — take a backup, keep several, restore one.
2. **CI regression fixtures** — upload a small set of documents, back it up,
   commit the bundle, and use it to prove that an OxiCloud upgrade does not
   alter or break previously stored data. *Are old documents still readable?*

The second is the more valuable of the two, for a reason that is easy to miss:
it keeps the first honest. Most deployments discover their restore path is
broken during an incident. Here, every PR restores a real backup and reads it
back through the product.

Progress, kept current as work lands:

| phase | state |
|---|---|
| §0 harden `backup.sh` | **NOT STARTED** — small, independent, worth doing regardless |
| §1 bundle format + manifest | **NOT STARTED** — the foundation everything composes on |
| §2 backup as a registry job | **NOT STARTED** — DB dump + blob copy, ordering, reclaim interlock |
| §3 restore + verification gate | **NOT STARTED** — asymmetric version rule, consistency pass |
| §4 CI fixture profile | **NOT STARTED** — `expected.json` and the harness |
| §5 retention by reachability | **NOT STARTED** — N dumps over one blob set |

---

## The machinery already exists

This feature is mostly composition. The jobs registry already carries every
moving part:

| job | role here |
|---|---|
| `backend_migration` | copies blobs between backends — **the blob half of a backup** |
| `backend_reclaim` + `storage.pending_actions` | blob deletion is **deferred**, not inline |
| `blobs_consistency` | DB-side blob reference integrity |
| `backend_consistency` | DB ↔ backend reconciliation |

Deferred deletion (`migrations/20261031000000_pending_actions.sql`) is the
load-bearing detail — see §2.

## Three properties that fall out for free

Because blobs are content-addressed and immutable:

**Incremental with no diff logic.** Blob names *are* their hashes, so a second
backup copies only what the target lacks. "Skip if exists" is the entire
algorithm. Only the SQL dump is full each time — see § Incremental for why that
split lands well, and why it applies to the DR profile only.

**Self-verifying without the original.** Recompute BLAKE3 over a backed-up blob
and compare against its own filename. A backup can be proven intact years
later, offline, with no checksum sidecar — the names *are* the checksums. Most
backup tools have to build this.

**N dumps share one blob store.** Thirty daily dumps against a single blob set,
because immutable blobs are shareable. This is what makes §5 cheap.

---

## §0 — Harden `backup.sh` **NOT STARTED**

Independent of everything else, and the current script has defects that matter
more than their size suggests:

```bash
NOW="$(date '+%Y-%m-%d %H:%M:%S')"
pg_dump postgres://postgres:postgres@localhost:5432/oxicloud -F c --disable-triggers > "backup.${NOW}.dump"
```

- **No `set -euo pipefail`.** A failed `pg_dump` still leaves a file behind, via
  the redirect. A truncated dump that looks like a successful backup is the
  worst failure this tooling can have.
- **The timestamp contains a space and colons**, so filenames are illegal on
  SMB/Windows and awkward everywhere else. `%Y%m%dT%H%M%SZ` instead.
- **Hardcoded sandbox credentials.** Should read `DATABASE_URL`, falling back to
  the sandbox values only when it is unset.
- **`--disable-triggers` on `pg_dump` is probably a no-op here.** That option is
  documented as relevant only to `--data-only` dumps; this is a full custom-format
  dump. It belongs on `pg_restore`, where `restore.sh` already correctly passes
  it. Verify against `pg_dump --help` before removing, and if confirmed, fix the
  command recorded in memory `bug_pg_dump_folders_circular_fk` too, since that
  is where the flag was copied from.
- **No verification.** At minimum, `pg_restore --list` the result to prove the
  archive is readable before reporting success.

---

## §1 — Bundle format + manifest **NOT STARTED**

A backup is three things that must travel together, plus a fourth for the CI
profile:

```
backup-<version>-<timestampZ>/
  manifest.json      self-description — see below
  dump.sql           plain format for CI (reviewable in a diff),
                     -F c for DR (size)
  blobs/             content-addressed; inherently incremental
  expected.json      CI profile only — golden assertions (§4)
```

### The manifest is what makes it a bundle

Without it you have two artifacts and a hope. It records:

| field | why |
|---|---|
| `format` | the manifest's own version — without it, the first format change makes old fixtures unreadable |
| `migration_head` | `_sqlx_migrations` head at dump time. Drives the version rule in §3 |
| `oxicloud_version` | `OXICLOUD_VERSION` of the process that took it |
| `backend_kind` + locator | Local / S3 / Azure, and where |
| `blob_count`, `blob_bytes` | lets restore detect a partial bundle before starting |
| `dump_sha256` | the dump is *not* content-addressed, so it needs its own checksum |
| `window_start`, `window_end` | the consistency window from §2 |
| `source` | `live` or `snapshot` — see § Future. Costs nothing now, avoids a format bump later |
| `encryption` | which KEK the blobs are under, or `none` — see [§ Not in the bundle](#not-in-the-bundle) |

### Two profiles, one format

| | DR profile | CI-fixture profile |
|---|---|---|
| size | whole instance, incremental | a few KB, deliberately minimal |
| blob target | remote backend | local dir, committed to a repo |
| lifetime | retention window | kept forever, one per released version |
| restore direction | same or newer binary | **deliberately older → newer** |
| dump format | `-F c` | plain, so a regenerated fixture is reviewable |
| `expected.json` | absent | present |

---

## §2 — Backup as a registry job **NOT STARTED**

Not a cron shell script. As a job it inherits cursor paging, retry-then-pause
(per [`jobs-handling-recoverable-error.md`](jobs-handling-recoverable-error.md)),
progress reporting and the shipped alert sinks — so a stalled or failed backup
pages somebody instead of silently not existing.

### The consistency problem

The database and the blob store are separate systems, and **neither ordering is
safe on its own**:

| order | what goes wrong |
|---|---|
| blobs first, then dump | uploads during the window land in the dump but not in the copy → **the dump references a missing blob**. Guaranteed on a live instance. |
| dump first, then blobs | uploads during the window become harmless orphans in the copy; the danger is GC deleting a blob the dump still references |

So: **dump first, then copy the blob set that dump references.** The residual
race is reclamation — and blob deletion already goes through
`storage.pending_actions` drained by `backend_reclaim`, rather than happening
inline at delete time.

That gives two ways to close it, and the plan should pick one:

1. **Interlock** — hold `backend_reclaim` paused for the backup window (the
   registry already supports operator pause).
2. **Grace period** — require `pending_actions` to retain entries longer than
   the longest expected backup. No coordination needed, but it is a global
   setting serving a local need.

Either way: no table locks, no read-only mode, no snapshot isolation. The
manifest records the window so a reviewer can see what it covered.

### Blob copy

Driven from the blob set the dump references, not from a backend enumeration —
that way the copy is exactly what the restore needs, and anything missing is
detected here rather than at restore time. `backend_migration` already walks
blobs between backends; this is that walk with a different target and a
different completion criterion.

**Verification is free**: for each copied blob, recompute the hash and compare
to its key. A mismatch means the source is already corrupt, which is worth
knowing immediately and separately from the backup succeeding.

---

## §3 — Restore + verification gate **NOT STARTED**

### The version rule is asymmetric

This is the rule that makes the CI profile possible, and getting it backwards
would block the entire second use case:

| manifest `migration_head` vs the binary's | behaviour |
|---|---|
| **newer** than the binary | **refuse.** The dump contains schema the code does not know. Unrestorable; say so rather than attempting it. |
| equal | restore |
| **older** | **restore and let migrations run.** This is not a degraded case — it *is* the regression test. |

### A restore is not done when `pg_restore` exits

It is done when the restored pair is proven coherent. `blobs_consistency` and
`backend_consistency` already detect exactly the failure that matters — the
database referencing a blob the backend does not have — so the gate is wiring
rather than new logic:

1. restore the dump (reusing `restore.sh`'s drop-and-recreate, which already
   handles post-dump migrations blocking `--clean`)
2. run migrations
3. point the instance at the bundle's blob set
4. run `blobs_consistency` + `backend_consistency` and **fail the restore on any
   finding**

Compare with today's final line: `echo "[restore] Done."`

---

## §4 — CI fixture profile **NOT STARTED**

The fixture is created once per released version, committed, and restored by
every later version. CI iterates over **every** retained generation, so
upgrade-from-two-versions-back is covered and not just N−1.

### `expected.json` is generated, never hand-written

The backup tool emits it at creation time by reading the instance **through the
same API the test will later use**. The expectations are therefore
true-by-construction when the fixture is made, and CI's only job is to assert
they are *still* true under a new binary. Creating a fixture generation becomes
`oxicloud backup --with-expectations`, not maintaining JSON by hand.

### Assert behaviour, not values, for anything we may intend to change

The live example is the ETag. `docs/plan/normalize-file-dates.md` deliberately
changes its formula, so pinning the exact string would fail *by design* and
teach everyone to ignore the suite. Assert instead that the ETag is stable
across two reads and that `If-None-Match` yields 304 — the mechanism, not the
value.

### Tier the strictness

The real failure mode of golden tests is becoming a wall of false failures
somebody switches off:

| tier | meaning | examples |
|---|---|---|
| `strict` | byte-identical or fail | downloaded content hash, size, path, id, folder children |
| `behavioural` | the mechanism must work; the value may differ | ETag stability, conditional 304, thumbnail *present*, share token resolves, trash item restorable |
| `informational` | record drift, do not fail | search ranking, `storage_used_bytes` totals |

### Shape

```json
{
  "format": 1,
  "created_by": "0.9.2",
  "migration_head": "20261103000004",

  "files": [{
    "id": "3f2a7c…", "path": "/Documents/report.pdf",
    "size": 184320, "sha256": "9c1e…",
    "mime": "application/pdf",
    "surfaces": ["rest", "webdav", "nextcloud"],
    "tier": "strict"
  }],

  "folders": [{
    "id": "b41d…", "path": "/Documents",
    "children": ["report.pdf", "notes.md", "photos"],
    "tier": "strict"
  }],

  "authz": [
    { "subject": "user:bob",   "op": "read",  "resource": "folder:b41d…", "expect": "allow" },
    { "subject": "user:carol", "op": "read",  "resource": "folder:b41d…", "expect": "deny", "status": 404 },
    { "subject": "user:bob",   "op": "write", "resource": "folder:b41d…", "expect": "deny", "status": 403 }
  ],

  "behavioural": [
    { "check": "etag_stable",          "file": "3f2a7c…" },
    { "check": "conditional_304",      "file": "3f2a7c…" },
    { "check": "thumbnail_present",    "file": "…", "size": "medium" },
    { "check": "share_token_resolves", "token": "…" },
    { "check": "trash_restorable",     "item": "…" }
  ],

  "informational": [
    { "check": "search_hit",  "query": "quarterly", "expect_file": "…" },
    { "check": "usage_bytes", "drive": "…", "value": 2847561 }
  ]
}
```

### Three details that decide whether this catches anything

**`sha256` is of the downloaded bytes, not the stored `blob_hash`.** They should
agree, but the point is to exercise the *serving* path. A blob can be
byte-perfect on disk while a migration broke the row pointing at it — verifying
the blob store directly would pass while the product is broken. This is the
same principle as the standing rule against bash that duplicates production
logic: exercise the real API.

**Hit every surface.** `/api/files/{id}/download`, native WebDAV GET by path,
and NC WebDAV by path are separate code paths; a migration can break one while
the other two pass. The `surfaces` array says which to check.

**The denials are the important half of `authz`.** A positive-only suite passes
if a regression makes everything world-readable — which is precisely the
regression worth catching. Assert the *status shape* too: 404 versus 403 is
load-bearing under the anti-enumeration rule, so a denial that starts returning
403 where it returned 404 is an information leak even though access is still
correctly refused.

Express the ops as capabilities (`read`, `write`, `share`) rather than role
names — `GrantRole` versus `DriveRole` naming could legitimately change and the
fixture should not break on a rename.

### What this catches that nothing currently does

Several pending migrations carry exactly the risk "does old data survive", and
today that rests on reasoning:

- the **legacy whole-file blob paths** slated for removal next major — a fixture
  written by a version that used them is how you prove the removal is safe
- `backend_rechunk` and the thumbnail import migrations
- the **ETag change** in `normalize-file-dates.md` — a v0.9.2 fixture shows
  exactly which files a client would re-download
- dedup / ref-count migrations miscounting
- key rotation breaking old ciphertext

### Harness

Restore into a throwaway Postgres (`testcontainers` is already a dependency and
`tests/common/spawn-db.sh` exists), point a freshly built binary at the bundle's
blob directory, walk `expected.json`, assert. One harness, N fixtures — adding a
generation is data, not code.

---

## §5 — Retention by reachability **NOT STARTED**

Keep N dumps against one shared blob store. A blob in the backup store is
reclaimable only once **no retained dump references it** — the same ref-count
shape as production, with far simpler inputs.

CI fixtures are exempt: they are retained indefinitely and deliberately small.

---

## Incremental — which half, and for which profile

"Can backups be incremental?" has a different answer for each half, and the
split happens to land well.

**Blobs: incremental for free.** Names are hashes, so "skip if exists" is the
entire algorithm — no diffing, no comparison against the previous manifest. The
second backup of a mostly-unchanged instance copies almost nothing.

**The SQL dump: always full — and that is fine.** Because file content lives in
blobs, the database is *metadata only*. The part that would normally make a full
backup expensive is exactly the part that is already incremental, so the hybrid
is naturally balanced: full metadata, incremental content.

Making the database half incremental is possible but not worth it here:

- **WAL archiving** is the correct mechanism, but it is PITR — a different
  feature, requiring `archive_mode`, and a WAL stream is not a portable bundle.
- **Logical deltas** ("rows where `updated_at` > the last window") look tempting
  because `updated_at` already marks every row touch, but deletes leave no row
  to select, so it needs tombstones, and restore then depends on replaying
  deltas in FK-safe order. Too much correctness risk for a backup path.

### Incremental applies to the DR profile only

A consequence worth stating before somebody optimises the wrong profile: an
incremental bundle is **not self-contained**. Bundle N depends on blobs copied
during bundles 1…N−1 into the same shared store. For disaster recovery that is
exactly right — one blob target, many dumps, which is also what makes §5's
retention cheap.

CI fixtures are the opposite: each one must stand alone, because it is committed
and restored in isolation. So the **fixture profile is deliberately full**, on
both halves. Chaining fixtures would create a dependency graph inside the
repository, where a missing ancestor silently breaks a test years later.

## Future — volume snapshots

Noted as a likely addition rather than an alternative, because the two compose
better than they compete.

**What snapshots do better.** An atomic snapshot of the database volume and the
blob directory together (same ZFS/LVM pool, or a consistent multi-volume EBS
snapshot) is genuinely point-in-time: the §2 ordering dance and the
`backend_reclaim` interlock are simply unnecessary on that path. ZFS/LVM
snapshots are also incremental on *both* halves (`zfs send -i` ships block
deltas), which is strictly better than full-dump-plus-incremental-blobs.

**What snapshots cannot do.** They are tied to the storage layer and the host:
not portable across deployment shapes (local → S3), not committable as a CI
fixture, no manifest, no logical assertions, and restore is all-or-nothing at
the volume level rather than selective.

**The composition.** Where snapshots are available, the best arrangement is to
use a snapshot as the *source* of a bundle — snapshot for atomicity, then mount
it read-only and produce the dump and blob copy from it. That yields a
guaranteed-consistent bundle with no interlock at all, while keeping the bundle
portable. Under that arrangement §2's dump-then-blobs ordering becomes the
fallback for deployments without snapshot capability, not the only strategy.

| | bundle from live | bundle from snapshot | raw snapshot |
|---|---|---|---|
| consistency | ordering + reclaim interlock | atomic, free | atomic, free |
| incremental | blobs only | blobs only | both halves |
| portable | yes | yes | no |
| CI fixture | yes | yes | no |
| selective restore | possible | possible | no |

So the manifest carries a `source` field (`live` or `snapshot`) from §1 onward.
It costs nothing now and avoids a format bump later — which matters more than
usual here, because old fixtures must stay readable by definition.

## Not in the bundle

State this loudly, in the operator docs as well as here. A bundle that silently
omits something required for recovery is false comfort of the most expensive
kind.

- **Encryption keys.** If `EncryptedBlobBackend` is in use — and `backend_rotate`
  implies key rotation exists — the copied blobs are ciphertext and worthless
  without the KEK. The manifest records *which* key; the key itself must be
  backed up separately, by a different mechanism, with different access control.
- **Configuration and environment**, which hold the key reference.
- **Point-in-time recovery.** This produces a crash-consistent snapshot pair, not
  PITR. PITR needs WAL archiving and is a different feature — named here so
  nobody assumes otherwise.

---

## Rejected alternatives

- **`pg_dump` + `rsync`, documented as a runbook.** This is effectively today's
  state. It has no manifest, so nothing detects a mismatched pair; no ordering
  guarantee, so the dump can reference missing blobs; and no verification, so a
  bad backup is indistinguishable from a good one until needed.
- **Taking the instance read-only for the window.** Removes the ordering problem
  outright, but deferred deletion already removes it at no availability cost.
- **Logical export (a JSON/tar of files via the API).** Portable and
  version-independent, but loses ids, grants, ref-counts, trash state and
  sharing — so it cannot test whether *this* data survives an upgrade, only
  whether bytes can be re-imported.

---

## Open questions

1. **Reclaim interlock or grace period?** (§2) The interlock is precise but
   needs the backup job to coordinate with another job's pause state; the grace
   period is simpler but sets a global to serve a local need.
2. **Does the CI fixture carry its own Postgres version expectation?** A dump
   taken under PG 16 restoring into PG 13 is its own failure mode, separate from
   the migration head.
3. **Where do fixture bundles live?** In-repo (simple, but grows the clone
   forever), Git LFS, or release assets fetched by CI. Affects nothing in the
   format; decide before the first fixture is committed, because moving them
   later rewrites history.
4. **Should `expected.json` be generated for the DR profile too?** It costs
   little and would make every operator backup self-verifying on restore, not
   just the CI ones.
