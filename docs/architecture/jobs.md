# Background Jobs

OxiCloud runs periodic maintenance work through a central **JobRegistry**
scheduler. This document is for implementors adding a new tenant or
debugging an existing one.

The design rationale (why a registry vs. per-service `tokio::spawn`
loops, the two-engine split, plugin future) lives in the plan doc:
[`docs/plan/job-registry.md`](../plan/job-registry.md). This page
is the "how to plug in" reference.

---

## When does a background loop belong here?

Single decision question:

> **"Would an operator plausibly `POST /api/admin/jobs/{name}/trigger`
> to make it run right now?"**

If yes, register it with JobRegistry. You get:

- Uniform admin surface (list, trigger, last-outcome).
- Uniform `oxicloud::scheduler` log line per run with `elapsed_ms`.
- Panic containment (a handler that panics doesn't kill the scheduler).
- Exclusivity: only one in-flight run per job name (a tick that fires
  while the previous run is still going is skipped, with a warning).
- Optional wall-clock timeout.
- Optional acceleration parameter (`?force=true` → `JobRunArgs.force`).

If no — the loop is a queue drainer (`tree_etag_flush_job`), a
continuous event reactor (`content_index_worker`), or a stats printer
(`db_pool_monitor`) — keep it as its own dedicated `tokio::spawn`
loop. Wedging it into JobRegistry adds framework overhead for no
operator benefit.

---

## Current tenants

| Job name           | Cadence                                    | Force semantic                                     | Service |
|---|---|---|---|
| `trash_cleanup`    | 24 h (hardcoded in DI, no env var yet)     | ignored                                            | [`trash_cleanup_service.rs`](../../src/infrastructure/services/trash_cleanup_service.rs) |
| `usage_reconcile`  | `OXICLOUD_STORAGE_USAGE_RECONCILE_SECS` (default 600s, min 30s) | ignored                            | [`storage_usage_service.rs`](../../src/application/services/storage_usage_service.rs) |
| `dedup_gc`         | on-demand only (trash cleanup runs it inline as its tail step) | `force=true` → `garbage_collect_force()` (skip orphan grace) | [`dedup_service.rs`](../../src/infrastructure/services/dedup_service.rs) |
| `grant_cleanup`    | `OXICLOUD_GRANT_CLEANUP_INTERVAL_HOURS` (default 24h) — feature-gated by `OXICLOUD_GRANT_CLEANUP_ENABLED` | `force=true` → `purge(Some(0))` (grace_days=0) | [`grant_cleanup_service.rs`](../../src/infrastructure/services/grant_cleanup_service.rs) |

---

## Adding a new job — recipe

### 1. Implement `JobHandler` on your service

```rust
use std::sync::Arc;
use async_trait::async_trait;

use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs};

pub const MY_JOB_NAME: &str = "my_job";  // stable snake_case

#[async_trait]
impl JobHandler for MyService {
    fn name(&self) -> &str {
        MY_JOB_NAME
    }

    async fn run(&self, args: &JobRunArgs) -> JobOutcome {
        // …do work…
        match self.do_the_work().await {
            Ok(count) => JobOutcome::ok_with(
                count,
                serde_json::json!({
                    // Anything useful for `GET /api/admin/jobs` or logs.
                    "bytes_processed": total_bytes,
                    "forced": args.force,
                }),
            ),
            Err(e) => JobOutcome::err(format!("my_job failed: {e}")),
        }
    }
}
```

Rules:
- **`name()` must be stable** — it appears in log lines, admin URLs,
  and `admin.background_runs.job_name` (once Part 2 lands). Renaming
  is a breaking change to operator scripts and log dashboards.
- **`args.force` semantics are per-job.** If your job has no
  acceleration mode (e.g. reconciliation, which is always idempotent),
  ignore it. If it does (e.g. skip a grace window), document the
  behaviour on the handler docstring.
- **Return `JobOutcome::ok_with(count, extra)`.** `count` is the
  primary scalar operators read (rows swept, blobs reclaimed).
  `extra` is a free-form JSON blob surfaced in the log line and the
  admin listing.
- **Return `JobOutcome::err(msg)` on failure.** The supervisor logs
  `outcome=err, cause=handler` and continues to the next tick. Don't
  catch panics inside `run()` — the supervisor does it, and hiding
  one loses the `cause=panicked` diagnostic.

### 2. Add a `register()` method to your service

Every service uses the chainable self-registration shape:

```rust
impl MyService {
    /// Register self with the periodic-job scheduler and return the
    /// same `Arc<Self>` for DI-style chaining.
    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        let interval = /* Some(Duration::from_secs(...)) or None for on-demand */;
        registry.register(self.clone(), interval, None /* no timeout */).await;
        self
    }
}
```

- **Interval `Some(dur)`** → **scheduled**. The supervisor fires the
  handler every `dur`.
- **Interval `None`** → **on-demand only**. Never fires periodically;
  only reachable via `POST /api/admin/jobs/{name}/trigger` or a
  programmatic call to `registry.trigger(name, args)`. Use for jobs
  whose periodic work happens elsewhere (dedup GC piggybacks on
  trash cleanup) but which still benefit from a uniform admin trigger.
- **Timeout `Some(dur)`** → the supervisor wraps the handler in
  `tokio::time::timeout`. Timeout trip is logged as
  `outcome=err, cause=timeout`. Use sparingly — most native jobs don't
  need it. Note: aborting a task mid-run is best-effort; a handler
  that ignores await points may still finish in the background.

### 3. Wire in `common/di.rs`

One statement per service:

```rust
let my_service = Arc::new(MyService::new(deps...))
    .register(&core.job_registry)
    .await;
```

`register()` panics on error (duplicate job name = DI wiring bug;
boot must fail loud) and emits a `job.registered` log line on
success. No `if let Err(e)` scaffolding needed at the call site.

`core.job_registry` is populated inside `create_core_services` and
lives on `CoreServices`. `SchedulerEngine` spawns the supervisor
task at the end of `build_app_state` after every service has
registered.

### 4. That's it

- `GET /api/admin/jobs` immediately lists the new job.
- `POST /api/admin/jobs/my_job/trigger` runs one dispatch off-schedule.
- `POST /api/admin/jobs/my_job/trigger?force=true` runs one dispatch
  with `JobRunArgs { force: true }`.
- If the job is scheduled, the supervisor fires it at the configured
  cadence.

No handler registration in the router, no admin trigger endpoint to
add — the framework already covers those uniformly.

---

## Admin surface

Production admin endpoints, always on, audit-logged. Admin-only via
the standard `/api/admin/*` middleware (no dedicated feature flag).

```
GET  /api/admin/jobs
     → [{ name, interval_ms?, next_run_at?, last_run_at?, last_outcome?, running }]

POST /api/admin/jobs/{name}/trigger[?force=<bool>]
     → 200 { ok: true, outcome: { outcome: "ok" | "err", count, extra?, message? } }
     → 404 { error: "job not registered", name }
```

- `interval_ms` / `next_run_at` are absent for on-demand jobs
  (`skip_serializing_if=None`).
- `last_run_at` / `last_outcome` are absent until the first run
  completes.
- `running` is `true` iff the in-flight permit is currently held.
- Every trigger call emits a `target: "audit"` log line
  (`event = "job.trigger"`) before dispatch.

---

## Log lines

Uniform target: `oxicloud::scheduler`.

| Event               | Fields                                                                  | When |
|---|---|---|
| `scheduler.started` | —                                                                       | Supervisor loop starts |
| `scheduler.ready`   | `registered = N`                                                        | All services have registered |
| `job.registered`    | `job`, `cadence` ("every 24 h" / "on-demand")                           | Each `register()` call |
| `job.run`           | `job`, `outcome` (ok\|err), `cause?` (handler\|timeout\|panicked), `count`, `elapsed_ms`, `extra?`, `error?` | Every dispatch |
| `job.tick_skipped`  | `job`, `interval_ms`, `running_for_ms`                                  | Tick fires while the previous run is still going |
| `job.trigger`       | `job`, `force` (audit channel)                                          | Admin `POST /trigger` |

`elapsed_ms` is the raw scalar in the structured field; the human
message renders it as `12ms` / `1.4s` / `4m30s` so `tail -f` operators
see the duration inline.

---

## Testing

The scheduler's own tests live in `src/infrastructure/scheduler/`
(`registry.rs::tests`, `engine.rs::tests`) and use dummy handlers.
No integration test infrastructure is needed to add a new service —
your service's normal unit tests cover the `run()` logic, and the
Hurl suite `tests/api/admin_jobs.hurl` covers the admin surface
generically.

If your service has a test that needs to construct the type WITHOUT
registering with a scheduler (e.g. isolated unit tests), skip the
`.register(&reg).await` chain and use the bare `Arc::new(Service::new(...))`.

---

## Non-goals

- **Cross-job dependencies.** No `depends_on` — each job runs
  independently. If you find yourself needing "job B runs after job A
  completes", route the completion signal through a lifecycle hook
  (`FileLifecycleHook`, `BlobLifecycleHook`), not through the scheduler.
- **Distributed scheduling.** Single-process only. If OxiCloud ever
  runs multi-node, the pattern is `SELECT … FOR UPDATE SKIP LOCKED`
  on a lease table — not this design.
- **Cron expressions.** Fixed intervals only. Real cron
  (day-of-week/month, arbitrary times) can layer on top later via a
  `next_run: Box<dyn NextRun>` trait; nothing needs it today.
- **Backfill on startup.** If the process was down when a scheduled
  tick was due, the missed tick is NOT caught up — the next tick fires
  at its normal interval.

---

## Related

- Plan doc: [`docs/plan/job-registry.md`](../plan/job-registry.md)
- Long-running / resumable jobs (Part 2, not yet built):
  [`docs/plan/job-registry.md#part-2--recoverable-run-engine`](../plan/job-registry.md#part-2--recoverable-run-engine)
- Consistency checks (a future Part 2 consumer):
  [`docs/plan/consistency-check.md`](../plan/consistency-check.md)
