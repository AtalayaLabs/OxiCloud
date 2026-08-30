# Plan — Job engines (periodic + recoverable) + admin surface

## Context

OxiCloud runs several fire-and-forget background daemons today, each
spawned by a service factory in `src/common/di.rs` at startup:

| Service | Cadence | Shape |
|---|---|---|
| `TrashCleanupService` | every 24 h | Fixed interval, no per-run state |
| `StorageUsageService::start_reconciliation_job` | every 600 s | Fixed interval, no per-run state |
| `db_pool_monitor` | every N s | Fixed interval, no per-run state |
| `dedup_service` GC | on demand + inline | Fixed interval, no per-run state |
| `GrantCleanupService` | every 24 h | Fixed interval, no per-run state |
| `tree_etag_flush_job` | every ~500 ms | Fixed interval, no per-run state |
| `content_index` worker | continuous | Fixed interval, no per-run state |
| Blob storage backend migration | admin-triggered | Long-running, cursor, resumable, in-memory state today |
| `admin/audio/metadata/reextract` | admin-triggered | Long-running, blocks HTTP request today |
| `admin/photos/metadata/reextract` | admin-triggered | Long-running, blocks HTTP request today |
| `ConsistencyCheck` runs (see `docs/plan/consistency-check.md`) | admin-triggered v1 | Long-running, cursor, resumable, needs DB state |

Two shapes bleed together in the current codebase but shouldn't. Each
daemon reinvents its own env var pattern, admin trigger endpoint,
logging schema, and (for the long-running ones) its own in-memory
progress state that vanishes on restart.

## Two engines, one file

This plan is intentionally two plans in one file (Ed 2026-07-27),
because the two engines share an admin URL prefix, a config-var
convention, and a logging target — but nothing else:

- **Part 1 — Periodic Scheduler.** In-memory registration + tokio
  interval loop. Serves fixed-interval jobs an operator might trigger
  manually. No DB tables, no cursor, no per-run persistence.
- **Part 2 — Recoverable-Run Engine.** DB-backed cursor persistence +
  exclusivity + crash recovery. Serves the four long-running tenants
  (storage-migration, reextract-audio, reextract-image, consistency
  check runs) and any future work that iterates over a large space
  with restart tolerance.

A recoverable job CAN optionally be periodically-triggered (register
once in each engine; Part 1's tick calls Part 2's `run_or_resume`
instead of a bare handler). Most Layer B tenants are admin-triggered
only.

Cross-cutting concerns (admin URL taxonomy, env vars, logging target,
plugin future) live in a shared section at the bottom so we're not
duplicating them between parts.

## Migration criterion — the trigger question

Not every background loop belongs in JobRegistry. The single question
that decides:

> **"Would an operator plausibly `POST /api/admin/jobs/{name}/trigger`
> to make it run right now?"**

**Yes → migrate.** The whole payoff of JobRegistry is a uniform
*operator surface* — list, trigger, last-outcome, log line, config
knobs. If nobody would ever manually trigger the job, the surface
delivers no value; you're paying framework overhead for nothing.
Anything an operator would manually trigger is by definition
periodic + discrete + meaningful.

**No → leave it as its own loop.** Continuous drains and
event-reactive workers ("core workers") fail this test — "trigger
the content-index worker" makes no sense; it's already running.
Standardise their env var naming and log target as a light
convention (see [Cross-cutting](#cross-cutting) below) but do NOT
wedge them into the scheduler.

Secondary confirmation questions — if the primary is yes and any of
these is no, migrate anyway but flag the mismatch:

1. Does each invocation report a meaningful `count` (rows swept,
   blobs GC'd, bytes reclaimed)? Continuous workers don't have
   discrete invocations to count.
2. Does the operator tune it via env vars beyond enable/disable?
3. Would an operator want a "did this run within the last N?" health
   signal? Periodic jobs benefit from `last_outcome`; always-on
   workers need liveness signals of a different shape.

**Cadence is NOT the trigger** — it's a symptom. Sub-second jobs
almost always fail the primary question (nobody manually triggers
something that fires 2× per second), but a hypothetical 1 s periodic
job that operators do want to kick still belongs in JobRegistry.
Cadence tells you "probably no"; the operator-trigger question is
what decides.

### Applied to the current daemons

| Service | Operator-trigger? | Destination |
|---|---|---|
| `TrashCleanupService` | Yes — "purge expired trash now" | Part 1 |
| `StorageUsageService::start_reconciliation_job` | Yes — "recompute quotas now" | Part 1 |
| `dedup_service` GC | Yes — already has `trigger-gc` | Part 1 |
| `GrantCleanupService` | Yes — already has `trigger-grant-cleanup` | Part 1 |
| `tree_etag_flush_job` | No — a "flush now" is meaningless (queue drains itself) | Core worker, unchanged |
| `content_index` worker | No — continuous drain, no discrete invocation | Core worker, unchanged |
| `db_pool_monitor` | No — "log stats now" is either grep-existing-logs or attach-a-debugger, not a scheduled job trigger | Core worker, unchanged |
| Blob storage backend migration | Yes — already admin-triggered | Part 2 |
| `admin/audio/metadata/reextract` | Yes — currently admin-triggered (synchronously) | Part 2 |
| `admin/photos/metadata/reextract` | Yes — currently admin-triggered (synchronously) | Part 2 |
| `ConsistencyCheck` runs | Yes — needs a trigger endpoint | Part 2 |

The `db_pool_monitor` case is illustrative: cadence-wise it *could*
fit Part 1 (10-30 s periodic, bounded work), but the operator-trigger
question kills it. Nobody manually triggers a stats-log because logs
are already there. Keeping it as its own loop is right.

## Implementation order

1. **Part 1 lands first** — small, self-contained, unblocks migration
   of trash-cleanup + storage-usage + db_pool_monitor + dedup GC +
   grant-cleanup + tree-etag flush + content-index. High mechanical
   payoff, zero new schema, minimal review surface.
2. **Part 2 lands next** — introduces `jobs.recoverable_runs` schema
   + `RecoverableJobHandler` trait + `JobStore` port + `run_or_resume`. On
   its own PR (schema change deserves independent review).
3. **Consistency-check framework (`docs/plan/consistency-check.md`)**
   lands third, consuming Part 2 as its runtime.
4. **Storage-migration and reextract-* migrated to Part 2** as
   follow-ups.

---

## Part 1 — Periodic Scheduler

### Contract — `JobHandler` trait

The implementor-facing surface for a fixed-interval job:

```rust
#[async_trait]
pub trait JobHandler: Send + Sync {
    /// Stable snake_case identifier. Must be unique across the process.
    /// Log lines, admin listing, env vars, and trigger URLs all key on
    /// this name.
    fn name(&self) -> &str;

    /// One execution. Called by the supervisor at the registered
    /// interval and (optionally) on admin trigger. Return `Ok { count,
    /// extra }` on success — the count is the primary scalar the job
    /// reports (rows swept, ETags flushed, blobs GC'd). Return
    /// `Err(msg)` on failure; the supervisor logs it and continues.
    ///
    /// `args` carries per-dispatch parameters. Periodic ticks pass
    /// `JobRunArgs::default()`; admin triggers can set `force: true`
    /// to request acceleration semantics (e.g. dedup GC skips its
    /// orphan grace window, grant cleanup uses grace = 0). Handlers
    /// that don't understand a given arg silently ignore it — no
    /// return-error path just because a caller set an unused flag.
    async fn run(&self, args: &JobRunArgs) -> JobOutcome;
}

/// Per-dispatch parameters. Grows over time; today it carries only
/// `force`. Kept as a struct (not `bool`) so we don't have to change
/// signatures the next time a job needs another knob.
#[derive(Debug, Clone, Default)]
pub struct JobRunArgs {
    /// Request acceleration semantics. Semantics are per-job:
    /// - `dedup_gc`: skip the orphan grace window (grace = 0).
    /// - `grant_cleanup`: grace = 0.
    /// - Others: silently ignored.
    pub force: bool,
}
```

Native services implement this trait on an existing service type (no
new wrapper) and register a single `Arc<dyn JobHandler>` with the
scheduler.

### Self-description — `description` / `mutates` / `repair_description`

Three defaulted methods on both `JobHandler` and `RecoverableJobHandler`
let a job tell the admin UI what it is. `RecoverableAdapter` forwards
them, since the registry only ever holds `dyn JobHandler`.

```rust
fn description(&self) -> &'static str { "" }
fn mutates(&self) -> Mutates { Mutates::Never }
fn repair_description(&self) -> Option<&'static str> { None }

pub enum Mutates { Never, Always, OnRepairOnly }
```

They surface on `JobSummary` (`GET /api/admin/jobs`) and drive the
panel: `Never` earns a read-only badge and triggers straight through,
`Always` confirms first, `OnRepairOnly` is safe to run and confirms only
when the repair variant is picked. `repair_description.is_some()` is
what renders the repair toggle at all, and its text is the confirmation
copy.

**Why three values and not a boolean.** A job can be read-only by
default and destructive under `?repair=true`; a boolean has to answer
wrongly for one of those two modes, and `false` on something that
deletes files is the dangerous direction to be wrong in. It is also
where the recovery framework is heading — discovery-only default,
mutation behind an opt-in — so a tenant that later grows a repair arm
changes this one value and nothing else.

**Why `Option<&str>` and not `supports_repair: bool` + prose.**
Presence gates the toggle, content supplies the wording. Split across
two methods they can disagree; and the frontend cannot invent the
wording itself, because correcting a counter and unlinking files off
disk are not the same warning. The two are independent, not derived
from each other: the thumbnail imports are `Always` *and*
repair-capable.

`OnRepairOnly` with no `repair_description` is rejected at registration
— it claims to mutate only under a flag it does not support, and would
render as safe with no reachable mutating path.

**Why English in the trait, not `locales/*.json`.** A description that
lives away from the behaviour rots the moment a job changes, invisibly,
and a translator cannot know what `manifests_consistency` reconciles.
i18n can layer on later keyed by job name with these as the fallback,
matching the frontend's `t(key, params, fallback)` — a missing
translation then degrades to English from code rather than to a blank
panel. No rework needed to get there.

Defaults exist so the methods could be added without touching every
job at once; every registered job declares all three today.

### `JobOutcome`

```rust
pub enum JobOutcome {
    Ok { count: u64, extra: serde_json::Value },
    Err(String),
}
```

Two variants only. Every reason a run can fail (handler returned an
error, wall-clock timeout, panic caught by the supervisor) collapses
to `Err(String)`, with the *cause* encoded in the message AND in a
`cause` tracing field the supervisor sets when it emits the log line:

- Handler returned `Err(msg)` → `cause = "handler"`, message = `msg`.
- `tokio::time::timeout` tripped → `cause = "timeout"`.
- `catch_unwind` caught a panic → `cause = "panicked"`, message = the
  payload as a string.

Handlers never construct the cause themselves; they either return
`Ok { count, extra }` or `Err(String)`. Keeping the enum to two
variants prevents every consumer of `match outcome` from having to
distinguish diagnostic sub-cases that behave identically for logging,
persistence, retry, and admin display.

### Runtime model

- **One `tokio::spawn`** at startup runs the scheduler main loop.
  Sleeps until the earliest due job, dispatches, sleeps again.
- Per-run **panic catching** via `tokio::spawn` inside the dispatch
  (or `AssertUnwindSafe` + `catch_unwind`). A bad handler crashes
  its own run, not the scheduler.
- **Sequential dispatch within a tick** by default. Two jobs due at
  the same instant run one after the other. Parallel dispatch can
  layer on later as a per-job toggle if a real need appears — most
  handlers touch the DB and don't benefit from concurrency.
- **`ScheduledJob.timeout: Option<Duration>`** is applied by the
  supervisor via `tokio::time::timeout` when set. Optional; use it
  when the handler has a real wall-clock bound. None means "let it
  run to completion."

Single supervisor is chosen for **operational** clarity, not runtime
cost: one place to observe, one panic-containment boundary, one
config surface, one plugin-registration hook when plugins land.

### Exclusivity — one in-flight run per `job_name`

Mirrors Part 2's exclusivity invariant, enforced in-memory since
Part 1 has no DB row:

- Each `RegisteredJob` carries an `is_running` flag (an
  `AtomicBool` or single-permit `Semaphore`).
- Before dispatching a tick, the supervisor tries to acquire the
  flag. If it's already held (the previous run is still executing),
  the tick is **skipped, not queued**:

  ```rust
  tracing::warn!(
      target: "oxicloud::scheduler",
      event = "job.tick_skipped",
      job = %name,
      interval_ms = interval.as_millis(),
      running_for_ms = current_run_start.elapsed().as_millis(),
      "{name} still running past its interval — tick skipped"
  );
  ```

  `next_run_at` advances by one interval so the schedule stays on
  its cadence rather than queueing backlog.
- On completion (or panic caught by the supervisor), the flag is
  released. The next tick is free to fire.
- **Diagnostic value.** A `job.tick_skipped` line on every interval
  is the operator signal that either the job is chronically slower
  than its cadence (retune the interval) or hung (attach a debugger
  / set a timeout / kill the process). Without this warning a slow
  or hung handler would silently starve.
- **Interaction with timeout.** If a job has a `timeout` configured
  and it trips, the supervisor kills the run and releases the flag.
  Timeouts prevent hangs from permanently silencing a job.
  Handlers without a timeout can, in principle, hang forever — the
  repeated `tick_skipped` warning is the only signal.

Cross-job concurrency is unchanged — different `job_name`s can run
sequentially per tick as described above. Exclusivity is per
job_name, not global.

### `JobRegistry`

```rust
pub struct JobRegistry {
    jobs: RwLock<HashMap<String, RegisteredJob>>,
}

struct RegisteredJob {
    handler: Arc<dyn JobHandler>,
    /// `None` = on-demand only (admin trigger + programmatic
    /// `registry.trigger(name)`), never fires periodically.
    /// `Some(dur)` = fires every `dur` AND admin-triggerable.
    interval: Option<Duration>,
    timeout: Option<Duration>,
    /// Single-permit gate that enforces the "one in-flight run per
    /// `job_name`" invariant (see Exclusivity above). A tick that
    /// finds the permit taken emits `job.tick_skipped` and does not
    /// spawn.
    in_flight: Arc<tokio::sync::Semaphore>, // capacity = 1
    /// Set when a run starts, cleared when it ends. Used to include
    /// `running_for_ms` in the skip warning.
    current_run_start: Arc<parking_lot::Mutex<Option<Instant>>>,
    last_outcome: Option<(chrono::DateTime<Utc>, JobOutcome)>,
    /// Only populated for periodic jobs (`interval = Some(_)`). None
    /// for on-demand-only jobs — `pick_next` skips them.
    next_run_at: Option<chrono::DateTime<Utc>>,
}
```

`Arc<JobRegistry>` lives on `AppState`. Native services register
themselves during DI:

```rust
// Scheduled: fires every N hours AND admin-triggerable.
registry.register(
    Arc::clone(&trash_cleanup) as Arc<dyn JobHandler>,
    Some(Duration::from_secs(interval_hours * 3600)),
    None, // no timeout
);

// On-demand only: no periodic tick, but the job is still catalogued
// so the admin endpoint can trigger it uniformly and callers get the
// same panic-containment + exclusivity guarantees. Used by dedup GC
// (piggybacks on trash cleanup for its main work; admin trigger for
// operator-driven runs).
registry.register(
    Arc::clone(&dedup_service) as Arc<dyn JobHandler>,
    None,     // interval — no periodic tick
    None,     // timeout
);
```

**Interval semantics.**
- `Some(dur)` — supervisor fires the job every `dur`. Also admin-triggerable.
- `None` — supervisor never fires the job. Admin-triggerable only. Dispatch still routes through the same `JobRegistry::trigger(name)` path so the job gets the same panic-containment, timeout, exclusivity, and log-line treatment as scheduled ones.

### Manual dispatch — `JobRegistry::trigger(name, args)`

```rust
pub async fn trigger(&self, name: &str, args: &JobRunArgs) -> Option<JobOutcome>;
```

The single entry point for running a registered job outside the
scheduler's tick loop. Called by:
- The admin endpoint (`POST /api/admin/jobs/{name}/trigger?force=<bool>`).
- Any service that wants a scheduler-uniform dispatch of a peer job
  (e.g. an inline call from trash cleanup to `trigger("dedup_gc", &args)`,
  if we later route the piggyback through the registry).

The supervisor's periodic ticks invoke the same underlying dispatch
with `JobRunArgs::default()` — periodic runs never force.

Returns `None` when the name doesn't exist. Returns `Some(JobOutcome)`
otherwise — even when exclusivity kicks the trigger out (that maps
to `Ok { count: 0, extra: {"skipped": "already_running"} }`, not
`None`).

### Design boundary — registry is a catalog, not an event system

Because a job can be triggered from multiple sources (scheduler,
admin, another service), the registry visually resembles an event
system. It is not. The distinction matters so we don't accidentally
extend it into one.

- **Registry:** *"operator or scheduler wants to run this SPECIFIC
  named job right now."* Imperative. Single handler per name. Direct
  dispatch. No subscription API.
- **Event system:** *"when SOMETHING happens, notify anyone
  interested."* Reactive. Multiple listeners per event type.
  Publish + subscribe API. Fan-out semantics.

Event-reactive work in OxiCloud goes through the existing lifecycle
hooks — `FileLifecycleHook`, `BlobLifecycleHook`,
`UserLifecycleHook`. Those already support multi-subscription and
event-typed dispatch. Never add subscription machinery to
`JobRegistry`; if a "when job A finishes, do B" case appears,
publish a `JobCompleted` lifecycle event and let a hook subscribe.

### Engine loop

```rust
async fn run(registry: Arc<JobRegistry>) {
    loop {
        let next = registry.pick_next().await;  // earliest next_run_at
        let sleep = next.deadline().saturating_duration_since(Instant::now());
        tokio::time::sleep(sleep).await;

        let outcome = registry.dispatch(&next.name).await;
        registry.record_outcome(&next.name, outcome).await;
    }
}
```

`dispatch` grabs the handler under a read lock, spawns a task, applies
the timeout, catches panics, and returns the `JobOutcome`. Sequential
dispatch is intentional; two jobs due at the same instant run
one-after-the-other.

### Native tenants and migration order

Four services satisfy the operator-trigger criterion above and migrate:

1. **trash-cleanup** — simplest self-contained loop; reference for the
   migration shape. Ships with Part 1's landing PR.
2. **storage-usage reconciliation** — same shape, different service.
3. **dedup GC** — already has `trigger-gc`; the shim forwards to the
   new registry-backed trigger.
4. **grant-cleanup** — already has `trigger-grant-cleanup`; same shim
   pattern.

Three services are **core workers** and STAY on their own loops
(fail the operator-trigger question — see the criterion table above):

- `tree_etag_flush_job` — 500 ms queue-drain, coalescing semantics.
- `content_index` worker — continuous channel drain, event-reactive.
- `db_pool_monitor` — periodic stats-log with no discrete-invocation
  count and no operator use for manual trigger.

Standardise their env var naming (`OXICLOUD_JOB_<NAME>_*`) and
tracing target for uniform operator ergonomics, but do NOT wedge them
into the scheduler.

### Verification (Part 1)

1. **Compile**: `cargo check --all-features --all-targets` +
   `cargo clippy -- -D warnings` clean.
2. **Boot**: start server; expect `scheduler started, N job(s) registered`.
3. **Admin listing**:
   ```
   curl -s http://localhost:8086/api/admin/jobs -H "Authorization: Bearer $TOKEN"
   ```
   returns a JSON array with each registered job, its `interval_ms`,
   `next_run_at`, and `last_outcome` (null until first tick).
4. **Trigger**: `POST /api/admin/jobs/trash_cleanup/trigger`
   invokes the handler immediately, records the outcome.
5. **Panic containment**: unit test a handler that panics; `last_outcome`
   records `Err(...)` with `cause = "panicked"` in the log; the scheduler
   is still alive (verified by triggering another job); the in-flight
   permit is released so the next tick can fire.
6. **Timeout enforcement**: unit test a handler that blocks longer than
   its declared timeout; `last_outcome` records `Err(...)` with
   `cause = "timeout"`; the in-flight permit is released.
7. **Overrun exclusivity**: unit test a handler with a 100 ms interval
   that sleeps 300 ms. Assert exactly ONE run is in flight at any moment
   (no parallel dispatch), and that two `job.tick_skipped` log events
   fire (one at each missed tick) with `running_for_ms` monotonically
   increasing.
8. **Shim compatibility**: existing per-service trigger endpoints
   (`trigger-sweep`, `trigger-gc`, `trigger-grant-cleanup`) keep working
   as thin forwards. Existing api-test Hurl suites pass unchanged.

---

## Part 2 — Recoverable-Run Engine

### Contract — `RecoverableJobHandler` trait

Sibling to `JobHandler`, NOT a subtrait. A stateless job that only
implements `JobHandler` never needs to know Part 2 exists.

```rust
#[async_trait]
pub trait RecoverableJobHandler: Send + Sync {
    /// Stable snake_case identifier — matches the `job_name` column
    /// in `jobs.recoverable_runs`.
    fn name(&self) -> &str;

    /// Long-running, cooperative scan. The store is the job's ONLY
    /// side effect: cursor checkpointing, cancel polling, run-state
    /// updates all go through it.
    ///
    /// Between batches the handler MUST poll `store.status()` — a
    /// `CancelRequested` return means the operator asked for a pause
    /// and the handler should return `Paused { cursor }` at the next
    /// safe boundary. A mid-batch `tokio::spawn` abort corrupts the
    /// cursor and MUST NEVER happen — that's why the supervisor does
    /// not apply `tokio::time::timeout` to recoverable jobs (Part 1's
    /// timeout policy does not apply here).
    async fn run_resumable(&self, store: &dyn JobStore) -> RunOutcome;
}
```

### `RunOutcome`

```rust
pub enum RunOutcome {
    Completed,
    Paused { cursor: Vec<u8> },
    Failed { message: String },
}
```

- `Completed` — walked the whole space. Engine writes `status = Completed`.
- `Paused { cursor }` — cooperative pause (cancel poll or graceful
  shutdown). Engine persists cursor + writes `status = Paused` so a
  future resume picks up here.
- `Failed { message }` — irrecoverable error. Cursor NOT advanced;
  engine writes `status = Failed` and captures the message.

### `JobStore` trait

The port the engine passes to a recoverable job. Backed by
`jobs.recoverable_runs` in production; can be mocked for unit tests.

```rust
#[async_trait]
pub trait JobStore: Send + Sync {
    /// The `run_id` this handler was invoked with. Uniquely identifies
    /// the row in `jobs.recoverable_runs`.
    fn run_id(&self) -> Uuid;

    /// Fixed at run start; used by consistency checks (and any other
    /// job with a grace boundary) as the reference `NOW()` — NOT
    /// `chrono::Utc::now()`, which would drift across a multi-hour
    /// scan. See `docs/plan/consistency-check.md` trap #1.
    fn started_at(&self) -> chrono::DateTime<chrono::Utc>;

    /// Read the current `status` from the row. Between batches the
    /// handler polls this; a return of `CancelRequested` means the
    /// operator asked for a pause.
    async fn status(&self) -> Result<RunStatus, DomainError>;

    /// The last-persisted cursor (raw bytes, per-job schema), or
    /// `None` on a fresh run. The handler decodes into its own key
    /// type (blob hash, file_id UUID, ltree path, …).
    async fn load_cursor(&self) -> Result<Option<Vec<u8>>, DomainError>;

    /// Advance cursor + stats, bump `last_progress_at`. Called between
    /// batches, typically every ~30 s OR every ~1 000 rows, whichever
    /// comes first. See `docs/plan/consistency-check.md` trap #6.
    async fn checkpoint(&self, cursor: Vec<u8>, delta_count: u64)
        -> Result<(), DomainError>;
}
```

Domain-specific extensions (consistency-check's finding sink, for
instance) are separate traits the impl composes on top of `JobStore`.
`JobStore` itself carries no findings/severity concept — those are
Layer C in the consistency-check plan, not the engine's concern.

### Schema — `jobs.recoverable_runs`

```sql
CREATE SCHEMA IF NOT EXISTS admin;

CREATE TABLE jobs.recoverable_runs (
    id                 UUID PRIMARY KEY,
    job_name           TEXT NOT NULL,
    status             TEXT NOT NULL,               -- Running / Paused / CancelRequested / Completed / Failed
    started_at         TIMESTAMPTZ NOT NULL,        -- fixed at run start
    last_progress_at   TIMESTAMPTZ NOT NULL,        -- heartbeat + last-checkpoint marker
    completed_at       TIMESTAMPTZ,
    cursor             BYTEA,                       -- opaque, per-job resume key (NULL = fresh)
    stats              JSONB NOT NULL DEFAULT '{}'::jsonb,   -- job-specific counters
    params             JSONB NOT NULL DEFAULT '{}'::jsonb,   -- job-specific params
    error_message      TEXT
);

CREATE UNIQUE INDEX one_active_run_per_job
    ON jobs.recoverable_runs (job_name)
    WHERE status IN ('Running', 'Paused', 'CancelRequested');

CREATE INDEX ON jobs.recoverable_runs (last_progress_at)
    WHERE status = 'Running';
```

**The partial unique index is load-bearing.** It enforces the "at
most one non-terminal run per `job_name`" invariant at the DB layer
so it survives concurrent triggers, admin-vs-scheduler races, and
transaction interleavings. The `CancelRequested` inclusion prevents
a second trigger during cancel from spawning a parallel run.

`jobs.*` is a NEW schema — kept distinct from `auth.*` / `storage.*` / `admin.*`
so operational tables don't pollute domain schemas. Consistency
checks own their own `jobs.run_findings` in the same
schema.

Cursor is `BYTEA`, not JSONB, because per-job cursors are fixed-shape
opaque keys (32-byte BLAKE3, 16-byte UUID, ltree bytes) — JSONB adds
encoding overhead and a keying convention every impl has to agree on.
`stats` and `params` ARE JSONB because they carry human-readable
key/value pairs read by observability code, not compared inside SQL.

### Cursor semantics

- **`NULL` cursor** = fresh run, no rows processed yet. Handler
  interprets as "start from the beginning." Every keyset-pagination
  helper handles this as `WHERE ($1::bytea IS NULL OR key > $1)`.
- **Non-NULL cursor** = last-processed key. On resume, `key > cursor`
  in the ORDER BY key ASC iteration.
- **Advance rule** = handler updates its in-memory cursor to the LAST
  row it successfully processed at the end of each batch, checkpoints
  periodically. On crash: at most one batch of work replays. Idempotent
  processing (e.g. `UNIQUE (run_id, kind, resource_id)` on findings)
  makes replay a no-op for anything already recorded.

### Checkpoint mechanics

One `UPDATE` per checkpoint. Cheap, no row-lock contention (this
process owns the row):

```sql
UPDATE jobs.recoverable_runs
   SET cursor            = $2,
       stats             = jsonb_set(
                              stats,
                              '{scanned_count}',
                              ((COALESCE(stats->>'scanned_count','0')::bigint + $3)::text)::jsonb
                           ),
       last_progress_at  = NOW()
 WHERE id = $1;
```

- `cursor` advances to the last row we processed.
- `stats.scanned_count` accumulates the delta — not overwritten. Each
  job's handler picks its own key names inside `stats`. There's ONE
  convention: a top-level `count` field mirroring the value carried
  in `JobOutcome::Ok.count` (see next section) — everything else is
  free-form.
- `last_progress_at` doubles as heartbeat. Boot recovery uses it to
  spot stale-Running rows.

### `RunOutcome` → `JobOutcome` bridge

The supervisor translates so a periodic-triggered recoverable job
records the same `JobOutcome` shape as any other tick:

- `Completed` → `Ok { count, extra: json!({"completed": true}) }`
- `Paused { cursor }` → `Ok { count, extra: json!({"paused": true, "cursor_hex": …}) }`
- `Failed { message }` → `Err(message)`

Paused is deliberately NOT an error — the run cooperatively yielded,
that's a success. Log lines stay meaningful (`outcome=ok`,
`extra.paused=true` distinguishes from full completion). Only
`Failed` alerts an operator.

### `run_or_resume` helper

The engine module exposes:

```rust
pub async fn run_or_resume<J: RecoverableJobHandler + ?Sized>(
    job: Arc<J>,
    store_factory: &dyn JobStoreFactory,
) -> JobOutcome
```

Body:

1. Look up the latest row for `job.name()`.
2. If `Completed`/`Failed` or nothing → `INSERT` a new `Running` row
   with `started_at = NOW()`, cursor NULL. On unique-index conflict
   (rare race), read the winning row and continue from step 3.
3. If `Paused` → `UPDATE ... SET status='Running'` on that row.
4. If `Running`/`CancelRequested` → short-circuit
   `Ok { count: 0, extra: {"skipped": "already_running"} }`.
5. Build a `JobStore` bound to the row's `run_id` and pass it to
   `job.run_resumable(store).await`.
6. Translate the returned `RunOutcome`, write the terminal status
   (`Completed` / `Paused` / `Failed`) with the final cursor/stats
   snapshot, return the `JobOutcome`.

### Concurrency policy — exclusive-by-default

**At most one non-terminal run per `job_name` may exist at any time.**
Non-terminal = `status IN ('Running', 'Paused', 'CancelRequested')`.
This is the default, not opt-in — a job runs to completion, gets
manually paused, or fails; a second trigger while one is active
never spawns a parallel run.

- A storage-migration cannot run twice at once. Neither can a
  reextract-audio, a reextract-image, or a consistency-check.
- The registry's trigger endpoint is idempotent: called while a run
  is active it returns the existing `run_id` + status; called while
  the latest run is `Paused` it resumes it (same cursor, same stats
  accumulator); called when no non-terminal run exists it starts
  fresh.
- The DB-level partial unique index makes the invariant impossible
  to violate even under concurrent triggers or scheduler-vs-operator
  races.
- The scheduler's periodic tick honours the same rule — if the
  latest row for a job is non-terminal, the tick does not spawn
  another. For long-running jobs "interval" effectively means "check
  every N whether a run needs starting", not "start every N."
- Cross-job concurrency is unchanged — different `job_name`s can
  run in parallel subject to Part 1's sequential-dispatch default.
  Exclusivity is per job_name, not global.

### Boot-time crashed-run recovery

At `AppServiceFactory` init, after DB pool is up:

```rust
sqlx::query!(
    "UPDATE jobs.recoverable_runs
        SET status = 'Paused',
            error_message = COALESCE(error_message, 'server restart mid-run')
      WHERE status IN ('Running', 'CancelRequested')"
).execute(&pool).await?;
```

Do NOT auto-resume — the bug that killed the last run may still be
present. Operators decide. The next scheduler tick (or an explicit
trigger) resumes any `Paused` row per the normal flow.

Consistency-check.md's existing consistency-scoped sweep collapses
into this general one.

### Startup jobs — `OXICLOUD_STARTUP_JOBS`

A comma-separated list of jobs to dispatch once, in the background,
after the scheduler is ready. Each entry is a registered job name,
optionally with the same query syntax the admin trigger URL uses.

**The default is both migration jobs, in repair mode:**

```
OXICLOUD_STARTUP_JOBS=thumb_derived_import?repair=true,thumb_attached_import?repair=true,transcode_import?repair=true
```

An explicit value replaces that list; an empty value disables startup
jobs entirely.

**Why it exists.** Scheduled ticks deliberately never pass `repair` — a
job that deletes on its default setting is what no-silent-auto-repair
forbids. But that left the migration jobs unable to finish on their
own: a deployment whose operator never opens the admin panel re-imports
sidecars it already imported, forever, and never drains the directory.

**Why the default deletes anyway.** Relying on operators to edit `.env`
has the same failure mode one level up — the ones who never edit it are
exactly the ones whose migration never completes. So this is a
deliberate exception to no-silent-auto-repair, and it rests on three
properties that must keep holding:

- **Nothing is deleted before its replacement has been read back.**
  `verify_and_unlink` imports, reads the blob back through the normal
  stack, and only then unlinks; a store that reported success but landed
  unreadable keeps its sidecar. This matters most for
  `thumb_attached_import`, whose bytes are user-uploaded previews with
  no render path — a wrong deletion there is permanent, where a wrong
  deletion of a server-rendered thumbnail costs a re-render.
- **Sidecars whose source is gone are deleted without a readback**,
  because there is nothing to read back and nothing can reference them
  again. Unrecoverable and unreachable are different things; these are
  both.
- **Every deletion is audited**, so what a boot removed, and from which
  source, is reconstructable afterwards.

The consequence to hold in mind: an upgrade deletes on first boot, in
every deployment at once, with no operator action. A regression in the
readback path would be simultaneous and unrecoverable, so that code is
load-bearing. Operators who want to inspect before committing set
`OXICLOUD_STARTUP_JOBS=thumb_derived_import,thumb_attached_import` —
same jobs, import only.

It is not a "run everything in repair mode" switch. Each job is named
individually and carries its own flags.

**Validation is fail-fast.** An unknown job name panics at boot — the
registry is fully populated by then, so a name that doesn't resolve is a
typo or a stale rename, and ignoring it would leave a migration that
silently never runs. Unknown flags panic too: a dropped `?repare=true`
would leave the job in discovery-only mode while the operator believed
the tier was draining, and the symptom ("it never finished") surfaces
months later with nothing pointing back at the config.

**Dispatch is non-blocking.** `tokio::spawn`, so readiness never waits
on a job that may walk a filesystem for hours. Jobs in the list run
sequentially within that task, not concurrently: they contend for the
same directories and pool, and the exclusivity gate would turn overlap
into a *skipped* run rather than a queued one.

**Interrupted runs resume.** The boot recovery sweep above runs first
and flips every abandoned `Running` row to `Paused` with its cursor
intact; `run_or_resume` then picks Resume over a fresh start. So a
migration killed by a restart continues where it stopped, and completes
across however many restarts it takes.

That is a deliberate exception to "do NOT auto-resume" — scoped to the
named jobs only. The rule protects against a restart silently resuming
work nobody asked for; here somebody did ask, in configuration, and not
having to ask again is the entire point. Every other paused run still
waits for an operator.

A resumed run keeps the flags it started with (`repair` / `deep` are
persisted to `params` on the fresh open and read back on resume), so
editing the config mid-migration does not retroactively change a run
already in flight.

**Safe to leave set.** Each job is idempotent and resumable; once the
tier has drained, a run is a `read_dir` over three directories that
returns nothing — and after the directory is removed, not even that.

**Visible in the admin panel.** These are ordinary registered jobs:
they appear in `GET /api/admin/jobs`, are triggerable by hand, and
record the same runs and findings. Rows named here additionally carry a
`startup` object with the configured flags, so an operator can see that
a job deletes files on every boot rather than only when someone clicks.

### Admin surface (recoverable runs)

Same URL taxonomy as Part 1 — resource-first, action second, all
under `/api/admin/jobs/{name}/*`. Extended for run identity:

```
POST /api/admin/jobs/{name}/trigger
    → { run_id, status }                  # starts or resumes; idempotent
POST /api/admin/jobs/{name}/cancel
    → { run_id, status: "CancelRequested" }
GET  /api/admin/jobs/{name}/runs
    → [{ run_id, status, started_at, last_progress_at, stats, ... }]
GET  /api/admin/jobs/{name}/runs/{id}
    → { run_id, status, cursor_hex, stats, params, error_message, ... }
```

### Native tenants (Part 2)

Consistency checks are organized **by the subject they iterate**, not
by the concern they check. Cursor = row PK of that subject. Adding a
new check = adding a per-row branch inside the job that walks that
subject. See memory `project_consistency_jobs_landscape` for the full
rationale + the merges/separations that fall out of the rule.

| Tenant | Iterates | Cursor | v1 checks | Notes |
|---|---|---|---|---|
| `drives_consistency` | `storage.drives` | drive UUID | `used_bytes` drift (drive + user envelope) | Shipped Slice 3. |
| `folders_consistency` | `storage.folders` | folder UUID | `parent_trashed_mismatch` (live folder under trashed parent), `path_mismatch`, `lpath_mismatch` — both materialised columns compared to parent-chain reconstruction | Shipped Slice 4. Room to grow: `drive_id_parent_mismatch`, `orphan_root` (self-join already loads the fields). |
| `files_consistency` | `storage.files` | file UUID | `parent_folder_trashed` (live file under trashed folder), `missing_blob` (severity `data_loss` — `blob_hash` present in neither `storage.blobs` nor `storage.chunk_manifests`), `chunk_missing` (severity `data_loss` — manifest exists but points at chunks absent from `storage.blobs`; typical dedup GC race), `blob_size_mismatch` (denormalised `files.size` diverges from the authoritative size — manifest first, blob fallback) | Shipped Slice 6, CDC-aware Slice 10. Handles both storage paths: `storage.chunk_manifests` (post-Apr-2026 FastCDC ingest, dominant path) and `storage.blobs` (pre-CDC whole-file blob, legacy fallback). Physical backend-existence checks (chunk bytes actually on disk) belong in `storage_consistency`. Room to grow: `drive_id_parent_mismatch`, mime-type reconciliation. |
| `storage_consistency` | Storage backend (fs / S3) | object key / path | Each blob has a `storage.blobs` row (orphan detection) | `?deep=true` adds re-BLAKE3 + mime sniff. Orphan-side of the old bidirectional blob check + former `blob_integrity` + former `thumbnail_consistency`. |
| `grants_consistency` (future) | `storage.role_grants` | grant UUID | subject/resource/granted_by exist | |
| `backend_migration` | `storage.blobs` (source) → target backend | blob hash | Copy bytes; failures → `stats.failed_blobs` (and eventually `jobs.run_findings`) | Retires `Arc<RwLock<MigrationState>>` in `migration_job.rs`. |
| `reextract_audio` | `storage.files` where audio | file UUID | Re-run audio-tag parser, upsert `audio_metadata` | Retires synchronous admin-request execution. |
| `reextract_image` | `storage.files` where image/video | file UUID | Re-run EXIF/container date parser, upsert capture date | Same shape as reextract_audio. |
| `consistency_batch` (wrapper) | Iterates registered `*_consistency` jobs | — (JobHandler, not RecoverableJobHandler) | Sequentially triggers each sub-job; `?deep=true` propagates | Shipped Slice 5. One-click "run all" without per-job clicks; exclusivity via `job_name` prevents concurrent batches from stepping on each other. Batch itself always returns `Ok` — child failures land in `outcome.extra.per_check[<name>].outcome`. |

**Not consistency**: `POST /api/admin/dedup/recalculate` is aggregate-
stats-only (`unique_blobs`, `total_references`, `bytes_saved`) — one
SELECT + one UPDATE. Kept as its own admin endpoint; do NOT fold into
`storage_consistency` (different semantic — recompute vs verify).

### Verification (Part 2)

1. **Compile + schema-migration idempotence.**
2. **Fresh run:** `POST /api/admin/jobs/backend_migration/trigger` → new row with
   `status='Running'`, `cursor=NULL`.
3. **Concurrent trigger:** second `POST` while the first is running
   returns the SAME `run_id` (idempotent, DB unique index enforces).
4. **Cancel + resume round-trip:** `/api/admin/jobs/…/cancel` flips to
   `CancelRequested`; handler polls, returns `Paused { cursor }`;
   engine writes `Paused`. `POST /api/admin/jobs/…/trigger` again resumes; cursor
   picks up where left off; `stats.count` continues accumulating.
5. **Crash recovery:** stop the server mid-run; restart; boot sweep
   flips the row to `Paused` with `error_message = 'server restart mid-run'`;
   admin triggers again and it resumes.
6. **Idempotent replay:** for consistency-check specifically, verify
   that re-processing the last unpersisted batch does NOT double-record
   findings (`UNIQUE (run_id, kind, resource_id)` on
   `jobs.run_findings`).
7. **`RunOutcome` bridge log lines:** completed run logs
   `outcome=ok, extra.completed=true`; paused logs
   `outcome=ok, extra.paused=true`; failed logs `outcome=err, cause=handler`.

---

## Cross-cutting

### Admin URL taxonomy

All scheduler endpoints live on the **production admin surface**:
`/api/admin/jobs/*`. Always on, audit-logged, no feature-flag gate —
these are the operational levers you actually want ops to reach in
prod. See `project_admin_url_taxonomy` for the `/admin` vs
`/admin/internal` split we're honouring here.

**Resource-first URL taxonomy** for every scheduler-owned endpoint:

```
GET  /api/admin/jobs                       # list all
POST /api/admin/jobs/{name}/trigger        # one dispatch (Part 1 + 2)
POST /api/admin/jobs/{name}/cancel         # cooperative pause (Part 2)
GET  /api/admin/jobs/{name}/runs           # run history (Part 2)
GET  /api/admin/jobs/{name}/runs/{id}      # single run detail (Part 2)
```

`{name}` is the stable `JobHandler::name()` identifier. `trigger`
accepts an optional `?force=<bool>` query param that maps to
`JobRunArgs.force`.

**Audit logging.** Every `POST` to `/api/admin/jobs/*` emits a
`target: "audit"` line before invoking the registry — bulk-effect
mutations belong on the audit stream. Success/failure outcome fires
its own `oxicloud::scheduler` line via the existing supervisor path.

**Legacy shim retirement** (Stage 2 — landed):

The three legacy internal endpoints have been retired in favour of
the JobRegistry surface. Kept here for archaeology / URL migration
reference for any external tool that still expects the old paths:

| Legacy (retired)                                     | Replacement                                          |
|---|---|
| `POST /admin/internal/trigger-sweep`                 | `POST /admin/jobs/usage_reconcile/trigger`            |
| `POST /admin/internal/trigger-gc?force=X`            | `POST /admin/jobs/dedup_gc/trigger?force=X`           |
| `POST /admin/internal/trigger-grant-cleanup?force=X` | `POST /admin/jobs/grant_cleanup/trigger?force=X`      |

The `OXICLOUD_ENABLE_ADMIN_INTERNAL_ENDPOINTS` env var was removed
alongside — its sole purpose was gating those shims.

Response shape also changed: the old endpoints returned custom fields
(`grants_deleted`, `blobs_deleted`, `bytes_freed`, `forced`); the new
endpoint returns a uniform `{ ok, outcome: JobOutcome }` envelope with
job-specific fields under `outcome.extra`. Any external caller reading
the old fields needs updating.

### Admin UI — /admin/jobs page (frontend, future slice)

Operators shouldn't have to `curl` these endpoints in production —
they need a UI. Ships as a SvelteKit route once the backend surface is
complete. Rough shape:

**Route:** `/admin/jobs` (SvelteKit page under `frontend/src/routes/admin/jobs/`).
**Access:** admin-only; same guard as the rest of `/admin/*`.

**Page layout — one table, one drawer:**

```
┌── Jobs ─────────────────────────────────────────────────────────────┐
│ Name                Cadence     Last run          Status   Actions │
│ ───────────────────────────────────────────────────────────────────│
│ trash_cleanup       every 24 h  3h ago            ok       [Run]   │
│ usage_reconcile     every 10 m  4m ago            ok       [Run]   │
│ dedup_gc            on-demand   1d ago            ok       [Run]   │
│ grant_cleanup       every 24 h  never             —        [Run]   │
│ drives_consistency  on-demand   never             —        [Run]   │
│ consistency_batch   on-demand   never             —        [Run] [Run deep] │
└─────────────────────────────────────────────────────────────────────┘
```

Row click opens a right-side drawer with:
- Full JSON of the last outcome (`extra` fields explained per-job).
- For recoverable jobs: run history table (`GET /jobs/{name}/runs`),
  each row expandable to full `RunSummary` (cursor, stats, params,
  error_message).
- Per-run actions: `Cancel` (for Running rows only), `Trigger resume`
  (for Paused rows — same trigger endpoint, `run_or_resume` picks
  up the cursor).

**Data flow:**
- `GET /api/admin/jobs` — populates the main table. Polled every 5 s
  when the page is visible (`document.visibilityState`).
- `POST /api/admin/jobs/{name}/trigger` — the "Run" button. `deep=true`
  query for the "Run deep" variant (currently only shown on
  `consistency_batch`).
- `POST /api/admin/jobs/{name}/cancel` — Cancel button on a Running
  recoverable run.
- `GET /api/admin/jobs/{name}/runs` — populates the history table when
  the drawer opens.
- `GET /api/admin/jobs/{name}/runs/{id}` — populates the per-run
  detail expander.

**No new backend endpoints required** — every screen is driven by
what already exists.

**Visual conventions:**
- Status colour: `ok` = green, `err` = red, `Running` = blue-pulse,
  `Paused` = amber, `CancelRequested` = amber-flash, `Completed` =
  neutral grey, `Failed` = red.
- Findings surfacing is live as of Slice 7 (`jobs.run_findings` +
  `store.record_finding` + `GET /api/admin/jobs/{name}/runs/{id}/findings`).
  Drawer's "Findings" tab renders `kind`, `severity`, `resource_id`,
  and per-tenant `detail` JSON.

**Slice ordering:** frontend page is a follow-up PR, not blocking any
backend slice. Order of appearance:
1. Backend Part 2 slices (engine, admin surface, first tenant) — done.
2. `jobs.run_findings` table + `store.record_finding` API — done (Slice 7).
3. `consistency_batch` + more tenants — done (Slices 5–6: drives + folders + files, plus batch).
4. Frontend `/admin/jobs` page — takes the completed backend surface
   as-is; no backend changes required by the UI landing.
5. Progress estimation on `RunSummary.progress` (`fraction`, `kind`,
   `scanned`, `total`) — **done (Slice 9)**. Tenants that CAN count
   their subject override `RecoverableJobHandler::count_total()`;
   `run_or_resume` seeds `params.total_rows` + `params.progress_kind`
   on fresh runs; `row_to_summary` derives the `progress` block at
   serialisation time. UI renders a bar; `kind = "approximate"` runs
   get a striped fill so operators recognise proxy-derived
   estimates. See memory `project_job_progress_estimation`.

### Notifications & alerting

Silent failure is the enemy — a consistency check that finds a
data-loss finding at 3 AM Sunday should reach an operator, not sit in
the log stream unread. When SMTP is wired, the supervisor emits an
alert email on the following:

- **Any job dispatch returns `JobOutcome::Err`.** Applies to both
  Part 1 handler errors and Part 2 recoverable `RunOutcome::Failed`
  (which translates to `Err` via `run_or_resume`'s bridge). Subject
  line: `[OxiCloud] Job <name> failed`. Body includes: job name,
  cause (`handler|timeout|panicked`), error message, run_id (Part 2
  only), elapsed_ms, log-timestamp for grep, link to
  `/admin/jobs?highlight=<name>` when the UI lands.
- **Consistency check surfaces one or more findings** (deferred to
  the `jobs.run_findings` migration). Applies only to `*_consistency`
  tenants. Body includes: run_id, findings count grouped by
  `(kind, severity)`, worst-severity example, link to
  `/admin/jobs/{name}/runs/{id}` when the UI lands.

**Delivery conditions:**
- Silent no-op when `email_sender` on `AppState` is `None` (SMTP not
  configured). No error, no log spam — the mechanism is opt-in
  through SMTP presence.
- Recipient: every user with `role = 'admin'`. Not a hardcoded
  address — same rule as any admin-scoped notification the codebase
  already sends.
- Rate limit: **at most 1 email per (job_name, kind) per 6 hours**,
  keyed off an in-memory dedup table on `AppState`. Prevents a
  flapping job (fails, retries, fails, ...) from mailbombing.
  6 h chosen to match the operator-attention interval — a real
  ongoing failure gets 4 alerts/day, enough to be noticed, not
  enough to be filtered.
- Configurable OFF per job via env: `OXICLOUD_JOB_<NAME>_ALERT_ON_FAIL=false`
  (default `true`). Same shape as the existing enable/disable knobs.

**Implementation notes** (for whichever slice picks this up):
- Reuses `EmailSender` port + `MagicLinkInviteService`-style templating
  under `askama`. New template files:
  `templates/emails/job_failed.{html,txt}` and
  `templates/emails/consistency_findings.{html,txt}`.
- Dedup table lives on `AppState.job_alert_dedup:
  Arc<Mutex<HashMap<(String, String), Instant>>>`. Cleaned lazily on
  insert.
- Called from `SchedulerEngine::log_outcome` (Part 1 path) and from
  `run_or_resume`'s terminal-write branch (Part 2 path). Both already
  see the `JobOutcome`; adding a fire-and-forget email dispatch is
  ~10 lines each.

**Scope-out:** no Slack / webhook / PagerDuty integration in v1.
Email is the ONE alert channel until an operator concretely asks for
another. Layering webhooks on top later is trivial — same
"terminal outcome → notification" hook, different sink.

### Config surface — env vars

**No new convention.** Each service keeps its natural per-service
prefix (`OXICLOUD_GRANT_CLEANUP_*`, `OXICLOUD_STORAGE_USAGE_*`, …).
The `GET /api/admin/jobs` endpoint already gives operators a runtime
view of every registered job's interval, so grepping env-var prefixes
is no longer the primary discovery path.

Earlier drafts proposed a uniform `OXICLOUD_JOB_<NAME>_INTERVAL_*`
convention, with legacy names as warned aliases. Killed 2026-07-28
(Ed): normalising only the interval knob while leaving domain-specific
tunables (`GRACE_DAYS`, `BATCH_SIZE`, …) at the natural prefix creates
*intra-service* prefix drift — worse than the *cross-service* drift it
was meant to solve. A service either goes fully to `OXICLOUD_JOB_*`
(disruptive rename of every knob) or fully stays at its native prefix
(no rename). We stay.

The one real gap is **trash_cleanup has no env var today** (hardcoded
24h in DI). Adding `OXICLOUD_TRASH_CLEANUP_INTERVAL_HOURS` when we
need it uses the natural prefix — no new convention needed.

### Logging schema

Uniform structured target across both engines:

```rust
tracing::info!(
    target: "oxicloud::scheduler",
    event = "job.run",
    job = %name,
    outcome = %outcome_kind,      // "ok" | "err"
    cause = %cause,               // omitted on ok; "handler" | "timeout" | "panicked"
    count = ...,
    elapsed_ms = ...,
    // extras from the JobOutcome::Ok.extra map, flattened
    ...,
    "job {name} ran"
);
```

Security-relevant jobs (grant cleanup, authz cache invalidation) still
double-log to `target: "audit"` — the scheduler channel is for
observability; the audit channel is for compliance.

For Part 2 handlers, the same log line fires at run completion. The
`extra` map surfaces `completed`/`paused`/`cursor_hex` per the
`RunOutcome` bridge above.

### Composability

A recoverable job CAN also be periodically-triggered — register with
both engines. Part 1's tick calls Part 2's `run_or_resume(job, store_factory).await`
as its handler. The exclusivity index in Part 2 makes this safe even
if the interval is short enough that a tick fires while a previous
run is still going: the second tick's `run_or_resume` short-circuits
to "already running."

### Ordering and dependencies (deferred)

Cross-job dependencies (e.g. "trash cleanup runs before dedup GC")
are not modelled. Every job runs independently. If a real ordering
constraint appears, we add a `depends_on: Vec<String>` field and
topological scheduling then.

### Shutdown coordination (deferred)

Matches the existing daemons: no cancellation channel. The scheduler
task dies with the runtime. Recoverable jobs surviving a hard shutdown
land as `Paused` on the next boot via the sweep. If graceful shutdown
lands elsewhere in the codebase, the scheduler and all jobs migrate
together.

### Future extension — plugins

Once these engines exist they become the natural place for Extism
plugins to declare scheduled work — manifest `[[jobs]]` entries,
registered on `on_plugin_loaded`, unregistered on unload. Deliberately
deferred: no plugin needs it today, and adding
`JobOwner { Native | Plugin { id } }` + `unregister_by_owner` is a
small type extension the day one does. Nothing in the v1 design
precludes it.

### Job-history observability

`jobs.recoverable_runs` already carries the latest run per Part 2 job
— "last run time + status" is a `SELECT DISTINCT ON (job_name) …`
query. Deeper history (retention window, per-run drill-down UI) is
deferred; the log stream is the source of truth for older runs.

Part 1's periodic jobs only carry the last outcome IN MEMORY — no
DB row. If a periodic-only job needs persisted last-run visibility,
either promote it to a "trivial" recoverable job (immediate
`Completed`) or add a small `admin.periodic_runs_last` table later.
No such need today.

## Out of scope

- **Cross-job dependencies.** Register-time ordering only, not runtime
  graph.
- **Retention pruning of terminal `recoverable_runs` rows.** Deferred
  until the volume warrants a policy.
- **Prometheus / OpenMetrics export.** Log-only for now.
- **Distributed scheduling.** Single-process. If OxiCloud ever runs
  multi-node, `SELECT … FOR UPDATE SKIP LOCKED` on the runs table is
  the pattern; not now.
- **Backfill on startup.** If the process is down when a Part 1 job's
  tick was due, we do NOT catch up — the job runs at its next
  interval. Matches every existing daemon's behaviour today.
- **Cron expressions.** Fixed intervals only.
- **Rate limiting the admin trigger endpoint.** It's already
  admin-gated.

## Related memory notes

- `feedback_no_abbreviated_env_vars` — full-word env var names
  (`OXICLOUD_JOB_TRASH_CLEANUP_INTERVAL_HOURS`, not
  `OXICLOUD_JOB_TC_INTERVAL_H`).
- The grant-cleanup implementation is the closest reference for the
  Part 1 daemon → tenant migration shape: three env vars, one impl of
  an authz trait method, one daemon service, one admin trigger.
- `project_consistency_check_trait` — the consistency framework
  described in `docs/plan/consistency-check.md` is a *consumer* of
  Part 2 (the recoverable-run engine), not a peer. It ships after
  Part 2 lands.
