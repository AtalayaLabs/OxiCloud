//! "Run all consistency checks" coordinator.
//!
//! A plain [`JobHandler`] (not [`RecoverableJobHandler`]) — it walks
//! nothing, holds no cursor. Its whole job is to snapshot the
//! registry, filter to names ending `_consistency`, and dispatch each
//! sequentially via `registry.trigger(name, args)`. Sub-jobs receive
//! the SAME `JobRunArgs` the batch was invoked with — so
//! `?deep=true` on the batch propagates to whichever tenants respect
//! it (currently future `storage_consistency`, but the plumbing is in
//! place).
//!
//! ### Why a wrapper instead of a bulk endpoint
//!
//! Operators want one click for "run all". Building a general-purpose
//! `POST /api/admin/jobs/*/trigger` group-endpoint would need its own
//! auth path, its own concurrency envelope, its own outcome shape.
//! A wrapper JobHandler reuses ALL of that infrastructure:
//!
//! - Same admin URL: `POST /api/admin/jobs/consistency_batch/trigger`.
//! - Same audit trail: one line per batch invocation.
//! - Same exclusivity primitive: the Part 1 per-job semaphore keeps
//!   two `consistency_batch` runs from stomping each other. Two
//!   batches (say, one `?deep=false` + one `?deep=true`) share the
//!   same lock — an admin cannot accidentally start a deep pass
//!   while a normal one is still walking.
//! - Same JSON outcome envelope — `per_check` lands under
//!   `outcome.extra`, which the admin UI can drill into without
//!   inventing a new response schema.
//!
//! ### Why the batch always returns `Ok`
//!
//! The batch's job is **dispatch**, not investigation. A child
//! failing means the child failed — not the batch. Failures surface
//! in `extra.per_check[<name>].outcome = "err"`; the operator drills
//! in. Reporting the batch itself as `Err` would confuse the metric
//! "did the batch run" with "did all children succeed", which are
//! genuinely different questions.
//!
//! ### Registration ordering
//!
//! `consistency_batch` MUST register AFTER every tenant it dispatches
//! — but only for a debug-affordance reason: the ordering of the
//! `GET /api/admin/jobs` response mirrors registration order, and
//! having the wrapper sit at the end of the consistency block reads
//! more naturally. Snapshot filtering happens at RUN time, so a
//! reversed order would still work; DI's ordering is aesthetic.
//!
//! ### Arc cycle avoidance
//!
//! [`ConsistencyBatch`] holds a `Weak<JobRegistry>` — the registry
//! owns an `Arc<dyn JobHandler>` for the batch, and the batch needs
//! access back to `trigger`. A strong `Arc<JobRegistry>` inside the
//! handler would leak the registry forever. Upgrading the weak on
//! each `run()` is cheap (one refcount bump) and gracefully surfaces
//! "registry dropped mid-shutdown" as an error rather than a hang.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use serde_json::json;

use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs};

pub const CONSISTENCY_BATCH_JOB_NAME: &str = "consistency_batch";

pub struct ConsistencyBatch {
    registry: Weak<JobRegistry>,
}

impl ConsistencyBatch {
    pub fn new(registry: &Arc<JobRegistry>) -> Self {
        Self {
            registry: Arc::downgrade(registry),
        }
    }

    /// Chainable self-registration — mirrors the per-tenant helpers.
    /// On-demand only; there is no periodic tick (operators fire it
    /// when they want to sweep, or the frontend "run all" button in
    /// `/admin/jobs` triggers it once the UI ships).
    pub async fn register_job(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        registry.register(self.clone(), None, None).await;
        self
    }
}

#[async_trait]
impl JobHandler for ConsistencyBatch {
    fn name(&self) -> &str {
        CONSISTENCY_BATCH_JOB_NAME
    }

    async fn run(&self, args: &JobRunArgs) -> JobOutcome {
        // Upgrade the Weak. Only fails if the registry has been
        // dropped — which can only happen during process shutdown,
        // in which case the scheduler is winding down anyway.
        let registry = match self.registry.upgrade() {
            Some(r) => r,
            None => {
                return JobOutcome::err(
                    "consistency_batch: registry dropped (shutdown in progress?)",
                );
            }
        };

        // Snapshot + filter. `snapshot_all` would give us Arc<JobEntry>
        // handles too, but we don't need them — `registry.trigger`
        // does the lookup by name itself. `snapshot` returns the
        // per-job public DTOs, which is exactly the shape we want.
        let targets: Vec<String> = registry
            .snapshot()
            .await
            .into_iter()
            .filter(|s| s.name.ends_with("_consistency") && s.name != CONSISTENCY_BATCH_JOB_NAME)
            .map(|s| s.name)
            .collect();

        let mut per_check = serde_json::Map::new();
        let mut ok_count = 0u64;
        let mut err_count = 0u64;

        // Sequential dispatch. Parallel would give us tail-latency
        // wins but also multiplies DB pressure — the maintenance pool
        // is shared with the periodic sweeps that keep running while
        // the batch runs. Sequential keeps memory + IO envelope
        // predictable; the batch is a "run once in a while, take as
        // long as it takes" workflow, not a hot path.
        for name in &targets {
            let child = registry.trigger(name, args).await;
            match &child {
                Some(JobOutcome::Ok { count, extra }) => {
                    ok_count += 1;
                    let mut entry = json!({
                        "outcome": "ok",
                        "count": count,
                    });
                    if !extra.is_null() {
                        // Preserve per-check `extra` (e.g.
                        // drives_consistency emits drift counts here
                        // once `run_findings` lands). Nested under
                        // its own key so operators reading
                        // `per_check[name]` see a stable shape.
                        entry["extra"] = extra.clone();
                    }
                    per_check.insert(name.clone(), entry);
                }
                Some(JobOutcome::Err { message }) => {
                    err_count += 1;
                    per_check.insert(
                        name.clone(),
                        json!({
                            "outcome": "err",
                            "message": message,
                        }),
                    );
                }
                None => {
                    // Race: the job disappeared between snapshot and
                    // trigger. In practice this only happens if some
                    // future code path deregisters a tenant at
                    // runtime. Report so operators see it in the
                    // batch outcome and can chase the cause.
                    err_count += 1;
                    per_check.insert(
                        name.clone(),
                        json!({
                            "outcome": "err",
                            "message": "job no longer registered (race with deregistration)",
                        }),
                    );
                }
            }
        }

        JobOutcome::ok_with(
            targets.len() as u64,
            json!({
                "per_check": per_check,
                "deep": args.deep,
                "force": args.force,
                "ok": ok_count,
                "err": err_count,
            }),
        )
    }
}
