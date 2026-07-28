# Plan — Resumable consistency checks + `StatefulAdapter` contract

> ⚠️ **PARTIALLY SUPERSEDED (Ed 2026-07-28).** The current shipping
> design organises consistency checks **by the subject they iterate**
> (drives / folders / files / storage), NOT by concern (blob / thumbnail
> / used_bytes). Each `*_consistency` job is a direct
> `RecoverableJobHandler` impl on the Part 2 engine — no
> `ConsistencyCheck` trait, no `StatefulAdapter` supertrait, no per-
> subsystem check registry. Cursor = row PK of the iterated subject.
>
> **See instead:**
> - Memory: `project_consistency_jobs_landscape` — the current taxonomy.
> - `docs/plan/job-registry.md` Part 2 §Native tenants — updated table.
> - `docs/architecture/jobs.md` — implementor guide.
>
> Sections below discuss `BlobConsistencyCheck`, `ThumbnailConsistencyCheck`,
> `UsedBytesConsistencyCheck` etc. as separate impls of a
> `ConsistencyCheck` trait. That IS retired. Read those sections for
> the invariants (grace-window trap, cursor discipline, findings
> idempotency) — they still apply. Ignore the trait shapes /
> registration wiring — the Part 2 engine covers those uniformly.

## Context

OxiCloud persists state in several independent subsystems: content-addressable
blobs on disk / S3, thumbnails (server-generated per blob + user-uploaded per
file), text-extraction cache, audio metadata cache, `storage.folders` + `file_metadata`,
`storage.trash`, `storage.drives.used_bytes`, WebDAV dead properties, and more.
Each has invariants that can silently drift:

- A blob on disk with no `file_blobs` reference (leak — wasted disk).
- A `file_blobs` row whose bytes are gone from storage (**data loss** — GET
  returns 500).
- A `drives.used_bytes` counter that no longer matches `SUM(size)`.
- A `folders.parent_id` pointing at a deleted row (historical raw-SQL fix).
- A thumbnail cache entry with no live file id (leak) or a missing entry
  the user actually uploaded (data loss).

Today the only "consistency" primitives are targeted point solutions —
`dedup_service` GC (orphan blob reap with 1 h grace), `storage_usage_service`
reconciliation (rebuild `used_bytes` from `SUM(size)`), and the trash cleaner.
None of them SURFACE inconsistencies for operators; they act blindly and
best-effort. `tests/api/storage_cleanup_check.sh` polls with a 5 s window
and races the 1 h GC grace (memory note `project_dedup_gc_test_trigger`).

At scale — Ed's example: 1000 users × ~1000 files each = 1M files — an ad-hoc
"is my disk usage accurate?" check must be **resumable** across restarts,
cooperative on cancellation, and non-blocking to live traffic. A batch job
that starts over from scratch after a container restart or SIGTERM never
completes.

This plan lands:

1. Two **traits** — `ConsistencyCheck` (one check) and `StatefulAdapter`
   (marker + registration on every state-owning port).
2. A **contract** every new adapter must satisfy at compile time — via a
   supertrait bound on existing state-owning ports, no new adapter
   compiles without declaring its consistency contract.
3. An **educational surface** in trait doc-comments — decision axes
   (severity, direction, grace, cursor) and canonical-example pointers.
4. **Consistency-specific persistence** — `jobs.run_findings`,
   idempotent-on-`(run_id, kind, resource_id)`.
5. A **first check** — `BlobConsistencyCheck` (both directions,
   blob-keyed cursor, severity split).

**Layer boundary — the runtime is not in this plan.** The resumable
execution engine (cursor persistence, exclusivity, cancel protocol,
crash recovery, `jobs.recoverable_runs` schema, `JobStore`,
`RunOutcome`, `run_or_resume`) lives in `docs/plan/job-registry.md`
Part 2. This plan describes what `ConsistencyCheck` implementors
write and how the check-specific bits (findings, severity,
`StatefulAdapter` supertrait wiring) compose on top of that engine.

**Order:** ships **after** the job-registry Part 2 engine lands.
Consistency closes an operator-visibility gap today, but it depends
on Part 2's `RecoverableJobHandler` + `JobStore` + `jobs.recoverable_runs`
primitives — those come first. Once both are in, consistency runs
are admin-triggered v1, becoming periodic-triggered when a
`JobRegistry` (Part 1) tenant wraps `run_or_resume` for each
`ConsistencyCheck`.

## Design decisions

### Two-trait split

`ConsistencyCheck` = one check (implements `run_resumable`).
`StatefulAdapter` = a subsystem that CONTRIBUTES checks (one or more).

This split is load-bearing:
- Some subsystems emit **multiple** checks (`ThumbnailStore` emits four —
  server-generated × 2 directions, user-uploaded × 2 directions).
- Some checks are **composed across** adapters (`UsedBytesConsistencyCheck`
  reads from both `FileMetadataRepository` and `DriveRepository`).

Bundling them into one trait would over-constrain the shape.

### Compile-time enforcement via supertrait bound

`StatefulAdapter` is added as a **supertrait** on every port that persists
state:

```rust
pub trait BlobStorage: StatefulAdapter { … }
pub trait ThumbnailStore: StatefulAdapter { … }
pub trait FileBlobReadRepository: StatefulAdapter { … }
pub trait FolderRepository: StatefulAdapter { … }
```

Any new impl of these ports — a new S3-alike backend, a new mock in tests,
a plugin-provided storage backend — will not compile without providing
`subsystem()` and `consistency_checks()`. The compiler is the enforcement;
reviewers cannot merge a stateful adapter without an answer to "what can
go wrong with this state, and how do you check it?"

### The severity axis

Every finding carries a `Severity` so the admin UI can order results and
operators can dismiss the low-impact ones without hiding real risk.

| Severity | Meaning | Examples |
|---|---|---|
| `DataLoss` | User-visible impact (500 on GET, missing user bytes) | Missing blob for a live `file_blobs` row; missing user-uploaded thumbnail |
| `Reclaimable` | Disk waste, no user impact | Orphan blob on storage, orphan thumbnail file |
| `Regenerable` | Auto-heals on next request | Missing server-generated thumbnail (server rebuilds), missing text-index row |
| `Drift` | Accounting mismatch, no user impact | `drives.used_bytes` vs `SUM(size)` |

Rule of thumb: if a human user notices, it's `DataLoss`. If only the disk
accountant notices, it's `Reclaimable` or `Drift`. If the next automatic
regeneration will fix it, it's `Regenerable`.

### Bidirectional in every check

Every check emits BOTH directions where they exist:

- **Backward (storage → DB) — orphan detection.** Wasted disk. `Reclaimable`.
- **Forward (DB → storage) — missing detection.** User-visible data loss.
  `DataLoss`. Higher severity — a single missing content-addressable blob
  silently breaks every file that referenced it.

Skipping the forward direction is the single most common consistency-check
mistake. It's easy because Pass 1 (list storage, cross-check DB) LOOKS
complete. Pass 2 (list DB, cross-check storage) is where data-loss surfaces.

### Report shape for missing findings — blob-level, not file-level

`MissingInStorage { blob_hash, ref_count, affected_file_ids: Vec<Uuid> }`.
One row per missing blob with the fan-out of broken files. Operator gets
a triage-ordered "biggest impact first" list. File-per-line reports lose
that ordering.

### Two-pass discipline eliminates the need for maintenance mode

Pass 1 — build candidate list from a snapshot read (storage listing for
orphan direction, DB SELECT for missing direction). Exclude anything younger
than `grace_window`.

Pass 2 — per candidate, re-read the OTHER side's state right before
flagging. If it transitioned (ref went up, blob just landed, row was
deleted, etc.), silently drop.

Race matrix — orphan direction:
- **Upload lands mid-scan** (dedup hit → ref_count ↑ after we sampled) —
  grace window skips young objects.
- **Last ref deleted mid-scan** (ref_count → 0, GC not yet) — cross-reference
  `blobs.orphaned_at`; expected transient state, not flagged.
- **Deep hash on partial upload** — deep mode runs only on rows older
  than a LONGER grace (24 h).

Race matrix — missing direction:
- **Blob written but DB row not yet inserted** (young file looks missing
  at flag time) — grace window skips DB rows younger than 1 h.
- **File deleted mid-scan** — Pass 2 re-reads `file_metadata` by id;
  if gone, drop the finding.
- **Blob just now landed** — Pass 2 re-verifies storage `HEAD`; if now
  present, drop.

Nothing before "byte-exact whole-table snapshot verification" needs a
quiescent server. Reserve `concurrent_safe() = false` for that one.

### Resumability — runs live in Part 2's `recoverable_runs`

Consistency runs are ordinary `RecoverableJobHandler`s. The runtime plumbing
— cursor persistence, exclusivity, cancel protocol, crash recovery,
`jobs.recoverable_runs` schema, `JobStore` trait, `RunOutcome`,
`run_or_resume` helper — lives in `docs/plan/job-registry.md` Part 2.
This plan does not redefine any of it.

**How consistency slots into the shared table:**
- Each consistency check registers under `job_name =
  'consistency_<check_name>'` (e.g. `'consistency_blobs'`,
  `'consistency_thumbnails'`). Naming convention lets a single
  `SELECT DISTINCT ON (job_name)` return the last run of every check
  alongside every other background job.
- Consistency's per-check knobs — `grace_window_secs`, `batch_size`,
  `concurrent_safe` — live inside `recoverable_runs.params` JSONB at
  run-start time. The check reads them back via
  `serde_json::from_value(store.params()?)`.
- `recoverable_runs.stats` accumulates `{"scanned_count": …,
  "findings_this_run": …}`; readers call
  `(stats->>'scanned_count')::bigint`.

The findings themselves are Layer C (this plan) — they don't
generalise to storage-migration or reextract:

```sql
CREATE TABLE jobs.run_findings (
    id             UUID PRIMARY KEY,
    run_id         UUID NOT NULL REFERENCES jobs.recoverable_runs(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,             -- OrphanBlob / MissingBlob / ...
    severity       TEXT NOT NULL,             -- DataLoss / Reclaimable / ...
    resource_id    TEXT NOT NULL,
    detail         JSONB,
    found_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (run_id, kind, resource_id)        -- idempotent re-scan on resume
);
CREATE INDEX ON jobs.run_findings (run_id, severity);
```

FK on `recoverable_runs.id` links a finding back to the run that
produced it; `ON DELETE CASCADE` clears findings when their run row
is pruned by a future retention job.

`jobs.*` is a NEW schema, created by Part 2's migration — keep it
distinct from `auth.*` / `storage.*` / `admin.*` so operational tables don't
pollute domain schemas.

### Non-obvious traps

Recorded here (and in the trait doc-comments) because every one has been
learned the hard way in similar systems:

1. **Grace window uses `scan_started_at`, NOT `NOW()`.** A resumable scan
   spanning 6 h must snapshot its grace boundary at start. Otherwise items
   uploaded 30 min in flip from "young, skip" (Pass 1's view) to "old, flag"
   (Pass 2's view) mid-flight — the scan produces false positives against
   itself.
2. **Cursor is per-check, opaque bytes.** Blob check cursors on BLAKE3
   hash (fixed 64 hex chars — natural lex order). Thumbnail cursors on
   file_id UUID. Folder-tree cursor on ltree path. The trait treats it
   as `Vec<u8>`; each impl serializes what it needs.
3. **Findings are idempotent on `(run_id, kind, resource_id)`.** Resume
   revisit must not double-count. Pass 2 can also DELETE findings that
   transitioned (was `MissingBlob`, blob has since landed → drop the
   finding, not the whole run).
4. **Cooperative cancellation ONLY.** Between batches, poll
   `recoverable_runs.status`. A `tokio::spawn` abort mid-batch leaks —
   cursor unpersisted, findings half-written. Cancel path writes
   `status='Paused'` + current cursor before returning.
5. **Crash recovery on boot.** Any `status='Running'` at server start =
   server died mid-scan. Auto-transition to `Paused`; DON'T auto-resume
   (the bug that killed the last run may still be present). Admin decides.
6. **Batch size 1000 items or 30 s, whichever comes first.** Cursor commit
   per-row makes DB write cost dominate at 1M items; longer batches leak
   more progress on crash.
7. **Two directions don't share a cursor.** `BlobConsistencyCheck` orphan
   side walks storage listing (S3 continuation token / readdir); missing
   side walks `file_blobs` by hash. Sequence them (orphan phase → missing
   phase); cursor encodes current phase.
   `ThumbnailConsistencyCheck` is worse — four phases (2 subspaces × 2
   directions), each with its own natural cursor. Cursor encodes
   `(subspace, direction, key)`.

## Trait shapes

### `ConsistencyCheck`

```rust
/// A single consistency check with a resumable, cursor-based scan.
///
/// # For implementors
///
/// Every implementation must decide five things before the first line of
/// code. Answer them in comments at the top of the impl:
///
/// 1. **Direction.** Backward (storage → DB) surfaces orphans; forward
///    (DB → storage) surfaces missing. Most checks do BOTH — sequence
///    them and encode the current phase in the cursor.
///
/// 2. **Severity per finding kind.** `DataLoss` (user impact) /
///    `Reclaimable` (disk waste) / `Regenerable` (auto-heals) /
///    `Drift` (accounting). The single most common mistake is treating
///    a missing user-uploaded thumbnail as `Regenerable` — it's not,
///    the server can't recreate what the user provided. It's `DataLoss`.
///
/// 3. **Cursor format.** Opaque `Vec<u8>` to the framework. Yours to
///    serialize. Content-addressable blobs → 32-byte BLAKE3. UUID rows →
///    16-byte UUID. Path rows → the path bytes. Multi-phase check →
///    prepend a phase byte.
///
/// 4. **Grace window.** Default 1 h (matches dedup GC). Deep checks
///    (hash verification) use 24 h. Grace ALWAYS refers to
///    `scan_started_at`, never `NOW()` — see trap #1 below.
///
/// 5. **Batch boundary.** 1000 items or 30 s. Call `store.checkpoint`
///    and `store.should_cancel` between batches — cancellation is
///    cooperative, never task-abort.
///
/// # Two-pass discipline
///
/// Pass 1 — build candidate list from a snapshot read, excluding items
/// younger than `grace_window`.
///
/// Pass 2 — per candidate, re-read the OTHER side's state right before
/// flagging. If it transitioned (ref went up, blob just landed, row was
/// deleted), silently drop.
///
/// Pass 1 alone LOOKS complete but produces false positives on every
/// race. Never skip Pass 2.
///
/// # Canonical example
///
/// See `BlobConsistencyCheck` in
/// `src/infrastructure/services/consistency/blob_check.rs` — it exercises
/// every axis (both directions, both severities, grace window, cursor,
/// cooperative cancel, blob-level report shape for missing findings).
#[async_trait]
pub trait ConsistencyCheck: Send + Sync {
    /// Machine-readable name — appears in the admin endpoint slug and in
    /// audit `event` values. Lowercase snake_case, one per check.
    fn name(&self) -> &'static str;

    fn grace_window(&self) -> Duration { Duration::from_secs(3600) }

    /// `true` (default) → safe to run against live traffic; the check
    /// respects grace window + two-pass re-verify. Only false for a
    /// check that genuinely needs a quiescent DB (whole-table snapshot
    /// verification of hashes) — not required for anything in v1-v5.
    fn concurrent_safe(&self) -> bool { true }

    /// `cursor: None` → fresh run. `Some(bytes)` → resume from last
    /// persisted checkpoint. Impls MUST:
    /// - call `store.checkpoint(cursor).await` between batches
    ///   (~1000 items or 30 s, whichever comes first);
    /// - call `store.should_cancel().await` between batches — return
    ///   `RunOutcome::Paused { cursor }` when it returns `true`;
    /// - use `store.scan_started_at()` (not `now()`) as the grace
    ///   window reference.
    async fn run_resumable(
        &self,
        opts: &CheckOptions,
        cursor: Option<Vec<u8>>,
        store: &dyn CheckStore,
    ) -> Result<RunOutcome, DomainError>;
}

#[derive(Debug)]
pub enum RunOutcome {
    Completed,
    Paused { cursor: Vec<u8> },
    Failed(DomainError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    DataLoss,     // user impact — top of triage
    Reclaimable,  // disk waste, no user impact
    Regenerable,  // auto-heals on next request
    Drift,        // accounting mismatch, no user impact
}

pub struct Inconsistency {
    pub kind: &'static str,       // "OrphanBlob", "MissingBlob", ...
    pub severity: Severity,
    pub resource_id: String,      // opaque
    pub detail: serde_json::Value,
}
```

### `CheckStore`

The framework hands each check a `CheckStore` — the only side effect a
check performs on shared state.

```rust
#[async_trait]
pub trait CheckStore: Send + Sync {
    fn run_id(&self) -> Uuid;
    fn scan_started_at(&self) -> chrono::DateTime<chrono::Utc>;

    /// Persist the cursor + last-progress timestamp. Called between
    /// batches. If a crash happens after this returns, the next resume
    /// starts from `cursor`.
    async fn checkpoint(&self, cursor: Vec<u8>, scanned_count: u64)
        -> Result<(), DomainError>;

    /// Poll the run's `status` column. Returns `true` when an admin
    /// requested cancellation. The check MUST return `Paused` with the
    /// current cursor.
    async fn should_cancel(&self) -> Result<bool, DomainError>;

    /// Upsert a finding. `UNIQUE (run_id, kind, resource_id)` in the
    /// schema means re-scanning the same resource on resume is safe.
    async fn record_finding(&self, finding: Inconsistency)
        -> Result<(), DomainError>;

    /// Delete a previously-recorded finding — used when Pass 2 sees
    /// the resource transitioned out of the inconsistent state.
    async fn drop_finding(&self, kind: &str, resource_id: &str)
        -> Result<(), DomainError>;
}
```

### `StatefulAdapter`

```rust
/// Marker + registration trait for any adapter that persists state OUTSIDE
/// process memory: blobs on disk / S3, DB tables, on-disk caches, message
/// queues you own.
///
/// Added as a SUPERTRAIT on every state-owning port
/// (`trait BlobStorage: StatefulAdapter`, `trait ThumbnailStore:
/// StatefulAdapter`, `trait FolderRepository: StatefulAdapter`, …), which
/// means: NO NEW ADAPTER CAN COMPILE without declaring its consistency
/// contract. The compiler is the enforcement; these doc-comments are the
/// education.
///
/// # For implementors adding a new stateful adapter
///
/// You cannot skip this trait. If you're reading this because your PR
/// won't compile, work through:
///
/// 1. **Am I actually stateful?** State means "bytes or rows outside
///    process memory that can desync from other subsystems". Config,
///    caches keyed by session, and derived indexes are NOT stateful
///    for this purpose (they can be dropped and rebuilt). If you're
///    not stateful, drop the `StatefulAdapter` impl entirely — but
///    then your port shouldn't have `StatefulAdapter` as a supertrait
///    either, so this compile error means the port author already
///    decided you were.
///
/// 2. **What are the DIRECTIONS of drift I can detect?** Almost every
///    stateful adapter has both:
///    - Backward (my storage → the DB that references it): orphans.
///    - Forward (the DB → my storage): missing.
///    Return one check that covers both by sequencing phases, OR two
///    checks (one per direction). The former is easier to operate.
///
/// 3. **What's the SEVERITY of each finding?** See `Severity` in
///    `consistency_check.rs`. Missing user-uploaded data is `DataLoss`;
///    missing server-derived data is `Regenerable`; orphan bytes are
///    `Reclaimable`; accounting drift is `Drift`.
///
/// 4. **What CURSOR fits my walk?** Content-addressable → hash prefix.
///    UUID-keyed → UUID lex. Path-keyed → path bytes. Whatever you pick,
///    it's opaque `Vec<u8>` to the framework — decode inside your check.
///
/// See `BlobConsistencyCheck` for the canonical impl to copy-adapt.
pub trait StatefulAdapter: Send + Sync {
    /// Subsystem slug — appears in the JobRegistry-registered
    /// `job_name` (`consistency_<subsystem>`, e.g. `consistency_blobs`)
    /// and in audit log `event` values. Lowercase snake_case, unique
    /// per adapter. Convention: `"blobs"`, `"thumbnails"`, `"trash"`,
    /// `"folder_tree"`, `"used_bytes"`.
    fn subsystem(&self) -> &'static str;

    /// REQUIRED (no default impl). Return every consistency check
    /// this adapter contributes. Most adapters return exactly one.
    /// Multi-keying subsystems return more — `ThumbnailStore` returns
    /// FOUR checks (server-generated + user-uploaded, each in both
    /// directions).
    ///
    /// Returning `vec![]` is a red flag. If your adapter has state but
    /// no check, either:
    /// - Your state is fully covered by another adapter's check
    ///   (rare — document exactly WHERE in a comment on this method).
    /// - You haven't written the check yet — return
    ///   `vec![]` with a `TODO(consistency): add <Name>ConsistencyCheck`
    ///   comment, ship the trait wiring, add the check in a follow-up PR.
    ///
    /// Reviewers will grep `TODO(consistency)` and ask when it lands.
    fn consistency_checks(&self) -> Vec<Arc<dyn ConsistencyCheck>>;
}
```

### `ConsistencyRegistry`

```rust
/// Collects `StatefulAdapter`s at wire-up time. Instantiated once in
/// `AppServiceFactory`, exposed on `AppState`, consumed by the admin
/// handler + (when JobRegistry lands) the scheduler.
pub struct ConsistencyRegistry {
    adapters: Vec<Arc<dyn StatefulAdapter>>,
}

impl ConsistencyRegistry {
    pub fn register(&mut self, adapter: Arc<dyn StatefulAdapter>) {
        // Trait bound forces `subsystem()` + `consistency_checks()` to exist.
        self.adapters.push(adapter);
    }

    /// Every check contributed by every registered adapter, flat.
    pub fn all_checks(&self) -> Vec<Arc<dyn ConsistencyCheck>> {
        self.adapters
            .iter()
            .flat_map(|a| a.consistency_checks())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ConsistencyCheck>> {
        self.all_checks().into_iter().find(|c| c.name() == name)
    }
}
```

## Admin surface

Consistency runs are ordinary `RecoverableJobHandler`s (see
`docs/plan/job-registry.md` Part 2), so most operator actions reach
them through the shared scheduler surface:

```
GET    /api/admin/jobs
       → summary list — consistency runs appear as
         `job_name = "consistency_<name>"`

POST   /api/admin/jobs/consistency_{name}/trigger
       → 200 { ok, outcome: { run_id, status } }
         (starts a new run or resumes the latest Paused one — see
         Part 2's `run_or_resume`)

POST   /api/admin/jobs/consistency_{name}/cancel
       → 200 { run_id, status: "CancelRequested" }
       (cooperative — check finishes its current batch and returns Paused)

GET    /api/admin/jobs/consistency_{name}/runs?status=<status>
       → 200 [{ id, status, scanned_count, last_progress_at, … }]

GET    /api/admin/jobs/consistency_{name}/runs/{id}
       → 200 { run: {...}, findings: [...paginated] }
```

Findings enrichment on `runs/{id}` is consistency-specific — read
from `jobs.run_findings` and joined into the response.
Everything else is generic Part 2 behaviour.

Production surface — always on, audit-logged. No feature-flag gate.

## Approach

### 1. Traits + framework in isolation

`src/application/ports/consistency.rs`
- Define `ConsistencyCheck`, `RunOutcome`, `Severity`, `Inconsistency`,
  `CheckStore`, `StatefulAdapter`, `CheckOptions`.
- Full doc-comments as sketched above — these are the educational
  surface, don't cut them.

`src/infrastructure/services/consistency/mod.rs`
- `ConsistencyRegistry` (data structure only).
- `PgCheckStore` — impl of `CheckStore` reading/writing
  `jobs.recoverable_runs` (filtered to `job_name LIKE 'consistency_%'`)
  + `jobs.run_findings`.
- `run_check(check, cursor, store)` — the runner that calls
  `run_resumable`, applies timeout, records outcome.

### 2. Schema migration

`migrations/YYYYMMDDHHMMSS_background_runs_admin_schema.sql` — creates
the merged `jobs.recoverable_runs` table shared with the JobRegistry
plan. Consistency checks own the `jobs.run_findings` table
alone and reference `recoverable_runs.id` via FK.

```sql
CREATE SCHEMA IF NOT EXISTS admin;

CREATE TABLE jobs.recoverable_runs (
    id                 UUID PRIMARY KEY,
    job_name           TEXT NOT NULL,               -- 'consistency_blobs', 'storage_migration', 'reextract_audio', ...
    status             TEXT NOT NULL,               -- Running / Paused / Completed / Failed / CancelRequested
    started_at         TIMESTAMPTZ NOT NULL,
    last_progress_at   TIMESTAMPTZ NOT NULL,
    completed_at       TIMESTAMPTZ,
    cursor             BYTEA,
    stats              JSONB NOT NULL DEFAULT '{}'::jsonb,   -- e.g. {"scanned_count": 12345}
    params             JSONB NOT NULL DEFAULT '{}'::jsonb,   -- e.g. {"grace_window_secs": 3600}
    error_message      TEXT
);
CREATE UNIQUE INDEX one_active_run_per_job
    ON jobs.recoverable_runs (job_name)
    WHERE status IN ('Running', 'Paused');
CREATE INDEX ON jobs.recoverable_runs (last_progress_at) WHERE status = 'Running';

CREATE TABLE jobs.run_findings (
    id             UUID PRIMARY KEY,
    run_id         UUID NOT NULL REFERENCES jobs.recoverable_runs(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,
    severity       TEXT NOT NULL,
    resource_id    TEXT NOT NULL,
    detail         JSONB NOT NULL DEFAULT '{}'::jsonb,
    found_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (run_id, kind, resource_id)
);
CREATE INDEX ON jobs.run_findings (run_id, severity);
```

### 3. Supertrait bounds on existing state-owning ports

Add `StatefulAdapter` as a supertrait on:

- `src/application/ports/storage_ports.rs::BlobStorage`
  (or wherever the blob-storage port lives).
- `src/application/ports/thumbnails.rs::ThumbnailStore` (both server-generated
  and user-uploaded paths).
- `src/application/ports/text_extraction.rs::TextExtractionCache`.
- `src/application/ports/audio_metadata.rs::AudioMetadataCache` (if a
  distinct port exists).
- `src/domain/repositories/file_blob_read_repository.rs::FileBlobReadRepository`
  (via the port trait it exposes to application services).
- `src/domain/repositories/folder_repository.rs::FolderRepository`.
- `src/domain/repositories/trash_repository.rs::TrashRepository`.
- `src/infrastructure/services/webdav_dead_property_store.rs`
  (`DeadPropertyStore` — has its own leak class per the deferred-rekey
  memory note).

Each of these will trigger compile errors in its impls. Each impl gets
a two-line stub:

```rust
impl StatefulAdapter for LocalFsBlobStorage {
    fn subsystem(&self) -> &'static str { "blobs" }
    fn consistency_checks(&self) -> Vec<Arc<dyn ConsistencyCheck>> {
        // TODO(consistency): add BlobConsistencyCheck once impl lands.
        vec![]
    }
}
```

Ship this PR without the actual checks. Grep `TODO(consistency)` = punch list.

### 4. First real check — `BlobConsistencyCheck`

`src/infrastructure/services/consistency/blob_check.rs`

- Depends on `BlobStorage` (storage listing) + `FileBlobReadRepository`
  (DB SELECT).
- Phase 1 (orphan direction): walk storage listing, cursor on hash prefix.
  Batch of 1000, checkpoint, cancel-poll. For each batch: SELECT ref_count
  FROM `storage.file_blobs` WHERE hash IN (…). Pass 2 re-verifies at flag
  time. Severity: `Reclaimable`.
- Phase 2 (missing direction): walk `file_blobs` ordered by hash, cursor
  on hash. Batch of 1000. For each row: `HEAD` on storage backend. If
  missing AND row hasn't disappeared AND row is older than
  `grace_window`, flag `MissingInStorage` with `affected_file_ids` from
  a JOIN to `file_metadata`. Severity: `DataLoss`.
- Cursor format: `[phase: u8, hash_key: 32 bytes]`.
- `LocalFsBlobStorage::consistency_checks()` returns
  `vec![Arc::new(BlobConsistencyCheck::new(self.clone(), ...))]`.

### 5. Admin handlers

`src/interfaces/api/handlers/admin_handler.rs`

- `start_consistency_check(name, force)` — insert an
  `jobs.recoverable_runs` row with `job_name = 'consistency_<name>'`
  and `status = 'Running'`, spawn a tokio task calling `run_check`,
  return `run_id`. Concurrent triggers hit the partial unique index
  and short-circuit to returning the surviving row.
- `cancel_run(id)` — UPDATE status = 'CancelRequested'.
- `resume_run(id)` — verify status == 'Paused', spawn task with the
  persisted cursor.
- `list_runs(filter)` — SELECT with filters + paginate.
- `get_run(id)` — SELECT run + paginated findings.

Same admin-guard as the JobRegistry surface (`trigger_job`,
`list_jobs`). Production surface — always on, audit-logged, no
feature-flag gate.

### 6. Boot-time crashed-run recovery

In `AppServiceFactory` init, after DB pool is up:

```rust
sqlx::query!(
    "UPDATE jobs.recoverable_runs
        SET status = 'Paused',
            error_message = COALESCE(error_message, 'server restart mid-run')
      WHERE job_name LIKE 'consistency_%'
        AND (status = 'Running' OR status = 'CancelRequested')"
).execute(&pool).await?;
```

Filtering on `job_name LIKE 'consistency_%'` scopes the sweep to
consistency runs; other tenants of `recoverable_runs` (storage
migration, reextract-*) run the same auto-Pause sweep from their own
boot-time hook. The JobRegistry supervisor's boot check may
generalise this into a single scheduler-wide sweep — until then, one
per subsystem is fine.

Do NOT auto-resume — the bug that killed the last run may still be there.
Log a warning if any rows were flipped so operators notice.

### 7. Hurl regression — `tests/api/consistency_check.hurl`

- Setup: login admin, seed one file (which creates one blob).
- Trigger `blobs` check with `force=true` (grace_days=0). Poll runs
  list until `status='Completed'`. Assert 0 findings.
- Manually orphan a blob (SQL: `DELETE FROM file_metadata WHERE …`,
  leave `file_blobs` + storage in place). Trigger check again.
  Assert 1 finding with `kind='OrphanInStorage'`, `severity='Reclaimable'`.
- Manually break a blob (SQL: leave `file_blobs` alone, wipe the
  storage backend for that hash — actually, use the storage service's
  test hook if one exists; otherwise skip this in Hurl and cover in
  integration tests).
- Cancel a run mid-scan (large seed, poll for scanned_count > 0, POST
  cancel, poll until status='Paused'). Resume. Assert scanned_count
  after resume > checkpoint.

### 8. Follow-up PRs (remaining checks)

Priority order:

| # | Check | Direction | Complexity |
|---|---|---|---|
| 1 | `BlobConsistencyCheck` | both | high (canonical) |
| 2 | `ThumbnailConsistencyCheck` | both × 2 subspaces = 4 sub-scans | high |
| 3 | `UsedBytesConsistencyCheck` | pure SQL | low — wrap existing reconciliation diff |
| 4 | `FolderTreeConsistencyCheck` | pure SQL | low — closure over `folders.parent_id` |
| 5 | Deep-hash sub-mode on `BlobConsistencyCheck` | forward | medium — 24 h grace, opt-in |
| 6 | `DeadPropertyConsistencyCheck` | forward | low, blocked on rekey (see `project_webdav_dead_properties_drive_rekey`) |
| 7 | `TrashConsistencyCheck` | both | medium — trash rows vs `file_metadata` soft-delete flags |

Each is a separate PR against the stable trait. `TODO(consistency)`
count decreases by one per PR.

## Critical files

**Create:**
- `src/application/ports/consistency.rs` (~250 lines — traits + doc-comments)
- `src/infrastructure/services/consistency/mod.rs` (~40 lines — pub types)
- `src/infrastructure/services/consistency/registry.rs` (~80 lines)
- `src/infrastructure/services/consistency/pg_check_store.rs` (~150 lines)
- `src/infrastructure/services/consistency/runner.rs` (~100 lines)
- `src/infrastructure/services/consistency/blob_check.rs` (~300 lines — canonical impl)
- `migrations/YYYYMMDDHHMMSS_consistency_check_admin_schema.sql` (~30 lines)
- `tests/api/consistency_check.hurl` (~150 lines)

**Modify (add supertrait bound):**
- `src/application/ports/storage_ports.rs` — `BlobStorage: StatefulAdapter`.
- `src/application/ports/thumbnails.rs` — `ThumbnailStore: StatefulAdapter`.
- `src/application/ports/text_extraction.rs`.
- `src/application/ports/audio_metadata.rs` (if applicable).
- `src/domain/repositories/file_blob_read_repository.rs`.
- `src/domain/repositories/folder_repository.rs`.
- `src/domain/repositories/trash_repository.rs`.
- `src/infrastructure/services/webdav_dead_property_store.rs`.

**Modify (add `StatefulAdapter` stubs):**
- Every impl of the above ports. Each gets `subsystem()` + a `vec![]` stub
  with `TODO(consistency)`.

**Modify (wire up admin surface):**
- `src/common/di.rs` — build `Arc<ConsistencyRegistry>`, expose on
  `AppState`, register every stateful adapter.
- `src/interfaces/api/handlers/admin_handler.rs` — five handlers.
- `src/interfaces/api/routes.rs` — five routes.
- `src/interfaces/api/mod.rs` — utoipa paths.
- `tests/api/run.sh` — register `consistency_check.hurl`.

## Reused existing utilities

- **Admin-guard + audit-log pattern** at
  `src/interfaces/api/handlers/admin_handler.rs::trigger_job` —
  same shape for the new endpoints (production surface, always-on,
  audit-logged; no feature-flag gate).
- **Dedup GC's orphan-detection logic** (`dedup_service.rs`) — the
  algorithmic template for `BlobConsistencyCheck`'s orphan phase.
  Reference impl, not a callsite — the check needs its own two-pass
  discipline; GC currently reap-and-forgets.
- **Reconciliation SQL diff** in `storage_usage_service.rs` — becomes
  `UsedBytesConsistencyCheck` almost verbatim, wrapped in report-only mode.
- **`AGENTS.md` audit convention** — every finding double-logs to
  `target: "audit"`, `event: "consistency.{check}.finding"`, plus
  operational log to `target: "oxicloud::consistency"`.

## Verification

1. **Compile**: `cargo check --all-features --all-targets` +
   `cargo clippy -- -D warnings` clean.
2. **Schema**: `just fe-nothing … cargo run` starts; migration lands
   the `admin` schema; `psql -c "\dt admin.*"` shows the two tables.
3. **Boot line**: `consistency: N adapter(s) registered, M check(s)
   available`. Grep `TODO(consistency)` in the source; count should
   equal M in v1 minus the shipped `BlobConsistencyCheck`.
4. **Hurl** (`tests/api/consistency_check.hurl`):
   - clean state → 0 findings
   - forced orphan → 1 `OrphanInStorage` finding, severity `Reclaimable`
   - cancel + resume round-trip preserves `scanned_count`
5. **Crash recovery**: kill server mid-scan (`kill -9`); restart;
   confirm the row is `Paused` with `error_message='server restart
   mid-run'`; POST resume; check completes.
6. **Trait enforcement**: add a new dummy adapter impl of `BlobStorage`
   without `StatefulAdapter` — compile MUST fail. Add the stub; compile
   succeeds. This is the load-bearing property of the design.
7. **Grace-window sanity**: run against a fresh 10 s window; upload a
   file mid-scan; confirm the young blob does NOT surface as
   `MissingInStorage` (grace window covers it).
8. **AuthZ gate**: non-admin caller hits `POST /api/admin/jobs/consistency_blobs/trigger`
   → 403 from the admin middleware, audit line records the rejection.

## Out of scope

- **JobRegistry integration**. Consistency checks are admin-triggered
  in v1. When `docs/plan/job-registry.md` lands, JobRegistry will
  consume `ConsistencyRegistry::all_checks()` for scheduled execution
  — no code change needed here.
- **Auto-repair**. Findings are reported, not fixed. Repair primitives
  live in the existing services (dedup GC's reaper, storage_usage
  reconciler); a future admin surface could trigger targeted repair
  after human review.
- **Distributed scheduling**. Single-process. If OxiCloud ever runs
  multi-node, add `SELECT … FOR UPDATE SKIP LOCKED` on the run rows.
- **Byte-exact whole-table snapshot verification**. The `concurrent_safe
  = false` case — reserved for a future `DeepBlobConsistencyCheck` that
  requires either `pg_export_snapshot` + S3-consistent list OR a
  read-only mode. Not needed for v1-v5.
- **Cursor pagination on the `GET /runs/{id}` findings list**. Simple
  offset/limit for v1. Add cursor only if operators actually hit a
  10k-findings run.
- **Findings retention**. Runs + findings accumulate forever until an
  operator manually deletes. Add a background cleaner once volume
  actually matters — most likely alongside JobRegistry.
- **Auto-scheduling in v1**. No `tokio::spawn` interval loop. Admin
  triggers only. Every scheduled invocation goes through JobRegistry
  when it lands.

## Related memory notes

- `feedback_no_abbreviated_env_vars` — full-word env var names if any
  land (e.g. `OXICLOUD_CONSISTENCY_BATCH_SIZE`, not
  `OXICLOUD_CC_BATCH`).
- `project_consistency_check_trait` — the memory that captures this
  design's decisions and the traps that shape the trait.
- `project_dedup_gc_test_trigger` — motivates the check's grace-window
  discipline; also the source of the algorithmic template for the
  orphan-blob direction.
- `project_webdav_dead_properties_drive_rekey` — `DeadPropertyStore`
  will get a check, but only after the rekey lands.
- `bug_thumbnail_dedup`, `bug_folder_cascade_hooks_missing` — surface
  the four-sub-scan complexity of `ThumbnailConsistencyCheck`.
- `bug_orphan_seed_null_orphaned_at_flaky` — reminds implementors that
  the orphan-blob direction MUST check `orphaned_at`, not just
  `ref_count = 0`.
