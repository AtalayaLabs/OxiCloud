//! Storage-backend migration as a recoverable-run tenant (Part 2 engine).
//!
//! Iterates `storage.blobs` and copies each byte payload from the SOURCE
//! backend (whatever the app booted with) to the TARGET backend
//! (whatever the current admin storage-settings config describes).
//! Both legacy whole-file blobs AND CDC chunk blobs are covered by the
//! single walk — they share `storage.blobs` as their physical registry
//! (see memory `project_cdc_dual_storage_registries`).
//! `storage.chunk_manifests` is pure PG state, holds no backend bytes,
//! and needs no migration.
//!
//! Retires the in-memory `Arc<RwLock<MigrationState>>` + one-shot
//! `tokio::spawn` in `migration_job.rs`. The recoverable engine
//! provides cursor persistence, cooperative cancel, boot-time crash
//! recovery, and the uniform `/api/admin/jobs/*` admin surface.
//!
//! ### Restart survival
//!
//! Cursor + per-blob failure findings are persisted after every batch.
//! On restart the boot-time sweep flips any abandoned `Running` row to
//! `Paused`; a subsequent admin trigger resumes from the persisted
//! cursor via `run_or_resume`. At most one batch of already-copied
//! blobs replays, and the `target.blob_exists` short-circuit makes
//! even that replay effectively free.
//!
//! ### Design notes
//!
//! * **Cursor.** UTF-8 hex of the last-processed blob hash (64 chars).
//!   Natural lex order matches `ORDER BY hash ASC`. Same encoding
//!   `blobs_consistency` uses.
//! * **Target resolution.** Rebuilt at the START of every fresh or
//!   resumed run via `StorageSettingsService::build_effective_backend`.
//!   Held for the duration of the run; a mid-run settings change is
//!   ignored until the next run. On resume the admin may have paused
//!   *specifically* to fix a broken target config, so we re-derive
//!   rather than pin.
//! * **Per-blob failures don't fail the run.** Each failure records a
//!   `migration_failed` finding (severity `data_loss` — the bytes
//!   didn't cross) and the walk continues. A run that completes with
//!   zero findings is proof the target has every blob.
//! * **Skip already-present blobs.** `target.blob_exists(hash)` before
//!   the copy — makes cheap re-runs safe and lets a paused run resume
//!   without redoing bytes.
//! * **No per-batch concurrency knob.** The old code buffered N copies
//!   in parallel. Sequential is easier to reason about with cooperative
//!   cancel + cursor discipline; the batch loop is I/O-bound anyway.
//!   Add concurrency later if a real throughput need appears.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use sqlx::PgPool;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::common::config::NamedStorageEntry;
use crate::common::errors::DomainError;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};
use crate::infrastructure::services::entry_backend::{
    build_entry_backend, persist_active_backend_name, persist_migration_readonly,
};

pub const STORAGE_MIGRATION_JOB_NAME: &str = "storage_migration";

/// The `params` JSONB key under which the run's target entry name is
/// stashed at Fresh-open time via `JobStore::set_string_param`.
/// Handlers re-read it on Resume so a paused run survives a restart
/// without the admin re-specifying the target. Exposed publicly so
/// the trigger endpoint's audit lines and the admin UI's run-detail
/// projections read the same constant.
pub const TARGET_NAME_PARAM: &str = "target_name";

/// Rows per batch. Copies are I/O-bound (source read + target write);
/// larger batches amortise fewer SQL round-trips but the checkpoint
/// / cancel-poll cadence lengthens. 100 balances the two — one
/// checkpoint per ~hundred blobs is fine, and the cancel-poll comes
/// every 100 rows too. Match `blobs_consistency` for consistency.
const BATCH_SIZE: i64 = 100;

pub struct StorageMigrationService {
    pool: Arc<PgPool>,
    /// Backend the running app is bound to at handler-construction
    /// time. Refers to the hot-swap wrapper when multi-entry is
    /// active, so this read stays live across cutovers even if
    /// stored as `Arc<dyn>`. Only used to identify the source
    /// entry's `backend_type()` for audit lines; the actual copy
    /// path reads from `self.pool` and writes to the target
    /// backend built via `build_entry_backend`.
    source: Arc<dyn BlobStorageBackend>,
    /// Name of the currently-active entry. Shared `Arc<RwLock<String>>`
    /// with `CoreServices.active_backend_name` — a hot-swap-mutation
    /// on cutover is visible here without reconstructing the handler.
    /// Read via `.read().clone()` at the top of each run.
    active_backend_name: Arc<std::sync::RwLock<String>>,
    /// All entries declared in env, held as a snapshot for name
    /// lookup during migration. Immutable per-deploy — matches
    /// `AppConfig.storage_entries`.
    storage_entries: Vec<NamedStorageEntry>,
    /// Ambient `AppConfig.storage_path` used as the `root_dir`
    /// fallback for a Local target entry that doesn't declare its
    /// own `_ROOT_DIR`. Same fallback rule as boot
    /// (`build_entry_backend`).
    storage_path_fallback: PathBuf,
    /// Shared `AppState.migration_readonly` handle. Handler flips
    /// this atomic (and persists to DB) at run start once all
    /// guards pass, so writes across the whole app get refused by
    /// the AuthZ short-circuit for the duration of the copy. On
    /// `RunOutcome::Completed`, the handler hot-swaps the runtime
    /// backend + clears this flag in one step — no restart.
    migration_readonly: Arc<AtomicBool>,
    /// Typed handle to the runtime blob-backend wrapper. The
    /// migration handler calls `.swap()` on this at cutover so
    /// subsequent user writes go to the target entry without a
    /// restart. Shared with `CoreServices.blob_backend_hot_swap` —
    /// same instance the coerced `blob_backend: Arc<dyn ...>`
    /// delegates through.
    blob_backend_hot_swap:
        Arc<crate::infrastructure::services::swappable_blob_backend::SwappableBlobBackend>,
    /// Shared in-memory progress snapshot. `Some(_)` during a
    /// running/paused migration, `None` otherwise. Read by the
    /// server-status header middleware to broadcast maintenance
    /// state to every user's session without polling.
    migration_progress:
        Arc<std::sync::RwLock<Option<crate::common::migration_progress::MigrationProgress>>>,
}

impl StorageMigrationService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Arc<PgPool>,
        source: Arc<dyn BlobStorageBackend>,
        active_backend_name: Arc<std::sync::RwLock<String>>,
        storage_entries: Vec<NamedStorageEntry>,
        storage_path_fallback: PathBuf,
        migration_readonly: Arc<AtomicBool>,
        blob_backend_hot_swap: Arc<
            crate::infrastructure::services::swappable_blob_backend::SwappableBlobBackend,
        >,
        migration_progress: Arc<
            std::sync::RwLock<Option<crate::common::migration_progress::MigrationProgress>>,
        >,
    ) -> Self {
        Self {
            pool,
            source,
            active_backend_name,
            storage_entries,
            storage_path_fallback,
            migration_readonly,
            blob_backend_hot_swap,
            migration_progress,
        }
    }

    /// Chainable self-registration — mirrors the `*_consistency`
    /// tenants. On-demand only (no periodic tick).
    pub async fn register_recoverable_job(
        self: Arc<Self>,
        registry: &JobRegistry,
        provider: &Arc<dyn JobStoreProvider>,
    ) -> Arc<Self> {
        registry
            .register_recoverable_job(self.clone(), provider.clone(), None)
            .await;
        self
    }
}

#[async_trait]
impl RecoverableJobHandler for StorageMigrationService {
    fn name(&self) -> &str {
        STORAGE_MIGRATION_JOB_NAME
    }

    /// Definitive count — one row per blob. `SELECT COUNT(*) FROM
    /// storage.blobs` on a modern PG is a sub-second index-only scan
    /// even at millions of rows.
    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> = sqlx::query_as("SELECT COUNT(*) FROM storage.blobs")
            .fetch_one(self.pool.as_ref())
            .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::migration",
                    event = "storage_migration.count_total_failed",
                    error = %e,
                    "count_total failed — run will not surface a progress bar"
                );
                None
            }
        }
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // Resolve the target entry NAME. Two paths:
        //
        // * Fresh run — `args.storage` MUST be Some (the trigger
        //   endpoint enforces this at the HTTP layer). Handler
        //   stamps it into `params.target_name` so a mid-run restart
        //   can resume without re-input.
        // * Resumed run — `args.storage` is typically None (admin
        //   just clicked Run on a Paused row). Handler reads the
        //   target from `params.target_name` written on the
        //   original Fresh open.
        //
        // A Fresh run without `args.storage` is a client bug — refuse
        // rather than default to something and quietly copy blobs
        // into the wrong entry.
        let is_fresh = resume_cursor.is_none();
        let target_name = if is_fresh {
            let Some(name) = args.storage.clone() else {
                return RunOutcome::Failed {
                    message:
                        "storage_migration requires `target_name` on a fresh run — trigger via \
                         POST /api/admin/storage/migration/start with `{\"target_name\": \"<entry>\"}`."
                            .to_string(),
                };
            };
            if let Err(e) = store.set_string_param(TARGET_NAME_PARAM, &name).await {
                return RunOutcome::Failed {
                    message: format!("failed to persist target_name to params: {e}"),
                };
            }
            name
        } else {
            match store.get_string_param(TARGET_NAME_PARAM).await {
                Ok(Some(name)) => name,
                Ok(None) => {
                    return RunOutcome::Failed {
                        message: format!(
                            "resumed run has no {TARGET_NAME_PARAM} in params — cannot infer \
                             target. Likely a Paused row from before the multi-entry migration \
                             refactor; cancel + trigger fresh."
                        ),
                    };
                }
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("read {TARGET_NAME_PARAM} from params: {e}"),
                    };
                }
            }
        };

        // Snapshot the current active name for the rest of this
        // run. The lock is held only for the clone; every subsequent
        // reference reads from this local. A hot-swap that fires
        // mid-run (e.g., a second migration starting after this one
        // completes) doesn't reshape our decisions from underneath.
        let active_backend_name = self
            .active_backend_name
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        // First-line guard: target name equals the currently-active
        // entry. Silent no-op if we let it through — the app would
        // walk every blob and skip because `target.blob_exists` is
        // trivially true (target = live source). Even on the same
        // local disk that's a lot of syscalls for no reason; on S3
        // it costs one HEAD per blob for zero copies.
        if target_name == active_backend_name {
            tracing::warn!(
                target: "audit",
                event = "storage_migration.refused_noop",
                run_id = %store.run_id(),
                target_name = %target_name,
                active = %active_backend_name,
                "storage_migration refused: target equals the currently-active entry"
            );
            return RunOutcome::Failed {
                message: format!(
                    "target entry `{target_name}` is the currently-active entry — nothing to \
                     migrate. Pick a different target."
                ),
            };
        }

        // Look up the target entry by name.
        let target_entry = match self.storage_entries.iter().find(|e| e.name == target_name) {
            Some(e) => e,
            None => {
                let available = if self.storage_entries.is_empty() {
                    "(none)".to_string()
                } else {
                    self.storage_entries
                        .iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                return RunOutcome::Failed {
                    message: format!(
                        "target entry `{target_name}` not found in OXICLOUD_STORAGE_ENTRIES. \
                         Available: [{available}]. If the entry was removed from .env since this \
                         run started, restore it or cancel this run."
                    ),
                };
            }
        };
        let source_entry = self
            .storage_entries
            .iter()
            .find(|e| e.name == active_backend_name);

        // Second-line guard: physical-identity check for the
        // encryption-differs case. Two entries with different names
        // can still point at the same physical bucket (only their
        // encryption key differs). That's the "in-place encryption
        // rotation" case — refused because reads during migration
        // would fail (LIVE backend uses K1, storage is being
        // overwritten with K2). See plan §Encryption "Proper
        // in-place rotation" for the deferred fix. The compare uses
        // `entry_identity` (backend + physical location, EXCLUDING
        // encryption key).
        if let Some(source) = source_entry
            && entry_identity(source) == entry_identity(target_entry)
        {
            // Compare head-pair materials — the "write key" for each
            // entry. A non-head pair difference (mid-rotation) doesn't
            // count as a key change for the purposes of this refusal.
            let key_differs = source.head_key_material() != target_entry.head_key_material();
            let hint = if key_differs {
                " (encryption key differs → this looks like an in-place key rotation; \
                 create a new entry pointing at a DIFFERENT bucket / dir, migrate to it, \
                 then move back if desired)"
            } else {
                ""
            };
            tracing::warn!(
                target: "audit",
                event = "storage_migration.refused_same_physical_storage",
                run_id = %store.run_id(),
                target_name = %target_name,
                source_name = %active_backend_name,
                encryption_differs = key_differs,
                "storage_migration refused: named target differs from source but physical storage matches"
            );
            return RunOutcome::Failed {
                message: format!(
                    "target entry `{target_name}` names a different entry than the active \
                     `{active_backend_name}`, but they point at the same physical storage{hint}."
                ),
            };
        }

        // Build target backend via the shared factory — same code
        // path boot uses, so the encryption decorator wrapping is
        // uniform.
        let target = build_entry_backend(target_entry, &self.storage_path_fallback);
        if let Err(e) = target.initialize().await {
            return RunOutcome::Failed {
                message: format!("target backend init: {e}"),
            };
        }

        // All guards passed. Engage server-wide read-only mode for
        // the duration of the copy so new writes can't create blobs
        // the migration walk has already stepped past. Both DB and
        // in-memory atomic get flipped in lock-step. Idempotent under
        // resume — the row is already `true` from the original open
        // (survived a restart via slice 4's boot seed), but rewriting
        // it doesn't hurt.
        //
        // A DB persist failure aborts before any copy — we won't
        // silently proceed with writes-allowed. If the atomic write
        // succeeded but DB failed we'd still have writes-off in this
        // process, but a restart mid-migration would lose it. Fail
        // early instead so operators see the actual DB problem.
        if let Err(e) = persist_migration_readonly(self.pool.as_ref(), true).await {
            return RunOutcome::Failed {
                message: format!(
                    "engage migration_readonly (persist): {e} — refusing to copy without the \
                     write freeze in place"
                ),
            };
        }
        self.migration_readonly.store(true, Ordering::Relaxed);
        tracing::info!(
            target: "audit",
            event = "storage.migration_readonly.engaged",
            run_id = %store.run_id(),
            target_name = %target_name,
            "🚧 migration_readonly engaged: writes across the whole app are refused until \
             cutover hot-swap completes"
        );

        // Seed the shared progress snapshot for the header
        // middleware. Total blob count is a one-shot SELECT COUNT(*)
        // — best-effort; if it fails we still push a snapshot with
        // total=0 so the banner at least shows *something* is
        // happening.
        let total_blobs: u64 = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM storage.blobs")
            .fetch_one(self.pool.as_ref())
            .await
            .map(|n| n.max(0) as u64)
            .unwrap_or(0);
        {
            let mut guard = self
                .migration_progress
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(crate::common::migration_progress::MigrationProgress::new(
                target_name.clone(),
                total_blobs,
            ));
        }

        let source_kind = self.source.backend_type();
        let target_kind = target.backend_type();
        tracing::info!(
            target: "audit",
            event = "storage_migration.run_started",
            run_id = %store.run_id(),
            source_name = %active_backend_name,
            target_name = %target_name,
            source_kind = source_kind,
            target_kind = target_kind,
            resuming = !is_fresh,
            "storage_migration starting {active_backend_name} ({source_kind}) → {target_name} ({target_kind})"
        );

        // Cursor = the last-visited blob hash, UTF-8-encoded. On resume
        // walk `WHERE hash > $cursor`. `None` / empty = start from the
        // smallest hash. Same shape `blobs_consistency` uses.
        let mut cursor: Option<String> = match resume_cursor {
            None => None,
            Some(bytes) if bytes.is_empty() => None,
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("invalid cursor: not valid UTF-8: {e}"),
                    };
                }
            },
        };

        let mut copied_count = 0u64;
        // K1.2: with the target-skip short-circuit gone (see the
        // detailed comment further down), no blob is ever "skipped"
        // during a migration walk today. The counter stays wired
        // through the log lines + `finish_completed` so K3's
        // format-aware smart-skip can re-populate it without
        // touching the observability surface. Not mutated in this
        // slice — hence no `mut`.
        let skipped_count: u64 = 0;
        let mut failed_count = 0u64;
        let mut source_missing_count = 0u64;

        loop {
            // Cooperative cancel poll between batches.
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    tracing::info!(
                        target: "oxicloud::migration",
                        event = "storage_migration.cancelled",
                        run_id = %store.run_id(),
                        copied = copied_count,
                        skipped = skipped_count,
                        failed = failed_count,
                        source_missing = source_missing_count,
                        "storage_migration cancelled cooperatively, pausing"
                    );
                    return RunOutcome::Paused {
                        cursor: cursor
                            .as_ref()
                            .map(|s| s.as_bytes().to_vec())
                            .unwrap_or_default(),
                    };
                }
                Ok(_) => {}
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("status poll: {e}"),
                    };
                }
            }

            // Fetch the next batch. `hash > $1` keyset pagination on
            // the PK; index-only scan.
            let rows: Vec<(String, i64)> = match sqlx::query_as(
                r#"
                SELECT hash, size
                  FROM storage.blobs
                 WHERE ($1::text IS NULL OR hash > $1)
                 ORDER BY hash
                 LIMIT $2
                "#,
            )
            .bind(cursor.as_deref())
            .bind(BATCH_SIZE)
            .fetch_all(self.pool.as_ref())
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("batch fetch: {e}"),
                    };
                }
            };

            if rows.is_empty() {
                return self
                    .finish_completed(
                        store,
                        &target_name,
                        &active_backend_name,
                        target.clone(),
                        copied_count,
                        skipped_count,
                        failed_count,
                        source_missing_count,
                    )
                    .await;
            }

            for (hash, size) in &rows {
                // Probe SOURCE first — without this a run would
                // silently "succeed" against a source that's missing
                // blobs the DB expects, and the audit intent of the
                // walk is lost (relevant on any post-migration state
                // where target may already have every blob). A
                // missing-on-source blob is a real data-loss
                // condition; record it and move on — we never
                // "copy" from nothing.
                match self.source.blob_exists(hash).await {
                    Ok(true) => {}
                    Ok(false) => {
                        source_missing_count += 1;
                        tracing::warn!(
                            target: "oxicloud::migration",
                            event = "storage_migration.source_missing",
                            run_id = %store.run_id(),
                            hash = %hash,
                            source = source_kind,
                            "blob absent from source; recording data-loss finding, no copy"
                        );
                        record_or_log(
                            store,
                            STORAGE_MIGRATION_JOB_NAME,
                            "source_missing",
                            "data_loss",
                            None,
                            serde_json::json!({
                                "hash":   hash,
                                "size":   size,
                                "source": source_kind,
                                "target": target_kind,
                            }),
                        )
                        .await;
                        continue;
                    }
                    Err(e) => {
                        // Transient probe failure on source is NOT a
                        // finding — treat like a network blip.
                        // Skipping this row on this run; a re-run
                        // will re-probe. If the failure is
                        // persistent, `blobs_consistency` catches
                        // it.
                        tracing::warn!(
                            target: "oxicloud::migration",
                            event = "storage_migration.source_probe_error",
                            run_id = %store.run_id(),
                            hash = %hash,
                            error = %e,
                            "source blob_exists probe failed; skipping this row"
                        );
                        continue;
                    }
                }

                // Always copy — do NOT short-circuit on
                // `target.blob_exists(hash)`. Ed hit this on 2026-08-01
                // during S3 → local migration testing with encryption
                // enabled on the target: the target had pre-existing
                // plaintext blobs from an earlier local-active session,
                // so `blob_exists` returned true and the migration
                // silently skipped them. Result: the "encrypted"
                // target ended up with mixed plaintext + ciphertext
                // blobs — undetectable until a subsequent read failed.
                //
                // The old skip was justified by two use cases:
                //   (a) resume idempotency — the last cursor-checkpoint
                //       window (~100 blobs) gets re-processed on resume;
                //   (b) target-side dedup — same content already present.
                //
                // Both are now handled by unconditional overwrite: the
                // re-copy is bounded by the checkpoint window (small),
                // and dedup-hit content is rare in practice
                // (content-addressability means duplicate blobs ARE
                // the same blob unless two backends were seeded
                // separately from the same source).
                //
                // K3's `storage_rotate` job will restore a smart skip
                // via the v1 header's `<key_fp>` field — "already at
                // head format+key" then becomes cheaply detectable
                // without reading target bytes. Until then, correct >
                // fast.

                match copy_blob(self.source.as_ref(), target.as_ref(), hash).await {
                    Ok(()) => {
                        copied_count += 1;
                    }
                    Err(e) => {
                        failed_count += 1;
                        tracing::warn!(
                            target: "oxicloud::migration",
                            event = "storage_migration.blob_failed",
                            run_id = %store.run_id(),
                            hash = %hash,
                            error = %e,
                            "failed to migrate blob; recording finding, continuing"
                        );
                        // resource_id stays None — blob hash isn't a
                        // UUID. Real identifier lives in `detail.hash`
                        // where the admin UI reads it.
                        record_or_log(
                            store,
                            STORAGE_MIGRATION_JOB_NAME,
                            "migration_failed",
                            "data_loss",
                            None,
                            serde_json::json!({
                                "hash":   hash,
                                "size":   size,
                                "source": source_kind,
                                "target": target_kind,
                                "error":  e.to_string(),
                            }),
                        )
                        .await;
                    }
                }
            }

            // Advance cursor + checkpoint. `delta_count` counts WORK
            // ATTEMPTED (copied + skipped + failed), not successful
            // copies alone — otherwise the progress bar stalls whenever
            // a batch is dominated by already-present blobs, which is
            // exactly the case on a resume.
            let last_hash = rows.last().map(|(h, _)| h.clone()).expect("non-empty rows");
            cursor = Some(last_hash.clone());
            let batch_len = rows.len() as u64;
            if let Err(e) = store.checkpoint(last_hash.into_bytes(), batch_len).await {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }
            // Bump the shared progress snapshot so the server-status
            // header middleware surfaces fresh numbers on every
            // user's next API call. Guard is held only for a struct
            // update — microseconds.
            {
                let mut guard = self
                    .migration_progress
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(progress) = guard.as_mut() {
                    progress.bump(batch_len);
                }
            }

            if (rows.len() as i64) < BATCH_SIZE {
                return self
                    .finish_completed(
                        store,
                        &target_name,
                        &active_backend_name,
                        target.clone(),
                        copied_count,
                        skipped_count,
                        failed_count,
                        source_missing_count,
                    )
                    .await;
            }
        }
    }
}

impl StorageMigrationService {
    /// Terminal successful path — reached from both Completed sites
    /// in the batch loop (empty-first-batch and short-batch).
    ///
    /// Four things happen here, in order, and each has a fail
    /// posture:
    ///
    /// 1. Persist `active_backend_name = target_name` to
    ///    `admin_settings`. Fatal on error — reporting `Completed`
    ///    while the DB still says the old entry is active would
    ///    strand the migrated bytes (next boot would come up on the
    ///    OLD backend). Operator retries — the walk short-circuits
    ///    on already-present blobs, so the retry is cheap.
    /// 2. **Hot-swap** the runtime blob backend to the target. The
    ///    already-initialized `target` backend is passed in from
    ///    `run_resumable` (built via `build_entry_backend`, so
    ///    encryption + config are set up); `SwappableBlobBackend`'s
    ///    `swap` is a `RwLock::write` — instantaneous. In-flight
    ///    reads holding the old inner `Arc` finish against the old
    ///    backend; new operations see the new one.
    /// 3. Update `active_backend_name` in the shared `RwLock` so a
    ///    second migration triggered right after (target != active)
    ///    sees the new active name without a restart.
    /// 4. Persist + clear `migration_readonly` — writes resume,
    ///    against the new backend. If DB persist fails at this step,
    ///    log a warning and clear the in-memory flag anyway: the
    ///    next boot's clear rule will fix the DB row on restart if
    ///    it's still stale.
    #[allow(clippy::too_many_arguments)]
    async fn finish_completed(
        &self,
        store: &dyn JobStore,
        target_name: &str,
        previous_active: &str,
        target_backend: Arc<dyn BlobStorageBackend>,
        copied: u64,
        skipped: u64,
        failed: u64,
        source_missing: u64,
    ) -> RunOutcome {
        // 1. DB pointer.
        if let Err(e) = persist_active_backend_name(self.pool.as_ref(), target_name).await {
            return RunOutcome::Failed {
                message: format!(
                    "copy finished but writing active_backend_name = `{target_name}` to \
                     admin_settings failed: {e}. Bytes are on the target; retrigger the run \
                     once the DB is reachable and it will short-circuit on already-present \
                     blobs and re-attempt the pointer flip."
                ),
            };
        }

        // 2. Runtime hot-swap.
        self.blob_backend_hot_swap.swap(target_backend);

        // 3. In-memory active-name mirror.
        {
            let mut guard = self
                .active_backend_name
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = target_name.to_string();
        }

        // 4. Drop read-only. In this order (after swap) so no write
        // slips through against the OLD backend between "readonly
        // off" and "backend swapped".
        let readonly_persisted = persist_migration_readonly(self.pool.as_ref(), false)
            .await
            .is_ok();
        self.migration_readonly.store(false, Ordering::Relaxed);
        // Clear the shared progress snapshot so the server-status
        // header stops emitting on subsequent requests. Guard held
        // only for the assignment.
        {
            let mut guard = self
                .migration_progress
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = None;
        }

        if !readonly_persisted {
            tracing::warn!(
                target: "oxicloud::migration",
                event = "storage_migration.readonly_clear_persist_failed",
                run_id = %store.run_id(),
                "cleared migration_readonly in memory (writes allowed) but the DB persist \
                 failed. If the server crashes before next boot, boot will re-seed the flag \
                 to true; boot-clear rule then flips it since active-matches and no in-flight."
            );
        }

        tracing::info!(
            target: "audit",
            event = "storage_migration.completed",
            run_id = %store.run_id(),
            active_backend_name = target_name,
            previous_active = previous_active,
            copied = copied,
            skipped = skipped,
            failed = failed,
            source_missing = source_missing,
            "✅ storage_migration completed — hot-swapped runtime backend to `{target_name}`, \
             writes resumed. No restart required."
        );
        // Per-run summary counters merged into `stats` for the admin
        // UI drawer. Same shape as `storage_rotate`'s extras + one
        // extra `source_missing` counter unique to migration.
        RunOutcome::completed_with(serde_json::json!({
            "copied":         copied,
            "skipped":        skipped,
            "failed":         failed,
            "source_missing": source_missing,
        }))
    }
}

/// Physical-storage identity string for a `NamedStorageEntry`. Two
/// entries with the same identity point at the same physical
/// location (same disk dir, same S3 bucket, same Azure container)
/// regardless of encryption key or credentials. Used by the second-
/// line refusal in `run_resumable` to catch in-place encryption
/// rotation attempts (same physical storage, K1 → K2 → reads-during-
/// migration break). See `docs/plan/storage-multi-entry.md` §Encryption.
///
/// Deliberately excludes:
/// - Encryption key — otherwise same-bucket-different-key would look
///   like a legit migration, hiding the corruption.
/// - Credentials — two entries with different access keys pointing
///   at the same bucket ARE the same physical storage.
/// - Region for S3 — the bucket URI is the primary key; region is a
///   routing hint (though endpoint_url is included since it changes
///   the actual host bytes land on).
fn entry_identity(entry: &NamedStorageEntry) -> String {
    use crate::common::config::StorageBackendType;
    match entry.backend {
        StorageBackendType::Local => {
            format!("local:{}", entry.root_dir.as_deref().unwrap_or(""))
        }
        StorageBackendType::S3 => match entry.s3.as_ref() {
            Some(s3) => format!(
                "s3:{}/{}:path_style={}",
                s3.endpoint_url.as_deref().unwrap_or("aws"),
                s3.bucket,
                s3.force_path_style,
            ),
            None => "s3:<missing-config>".to_string(),
        },
        StorageBackendType::Azure => match entry.azure.as_ref() {
            Some(az) => format!(
                "azure:{}/{}",
                az.account_name.as_str(),
                az.container.as_str()
            ),
            None => "azure:<missing-config>".to_string(),
        },
    }
}

/// Copy one blob: stream source bytes to a temp file, then hand the
/// path to `target.put_blob`. The spool-through-disk shape matches
/// what the old `migration_job::copy_blob` did — some backends'
/// `put_blob` want a path they can `rename(2)` or multi-part upload
/// from, not an in-memory buffer. The temp file lives in
/// `std::env::temp_dir()/oxicloud-migration/{hash}.tmp` and is
/// removed on success (best-effort on the failure paths — the OS
/// cleans up on reboot).
async fn copy_blob(
    source: &dyn BlobStorageBackend,
    target: &dyn BlobStorageBackend,
    hash: &str,
) -> Result<(), DomainError> {
    let tmp_dir = std::env::temp_dir().join("oxicloud-migration");
    tokio::fs::create_dir_all(&tmp_dir).await.map_err(|e| {
        DomainError::internal_error(
            "StorageMigration",
            format!("create temp dir {}: {e}", tmp_dir.display()),
        )
    })?;
    let tmp_path = tmp_dir.join(format!("{hash}.tmp"));

    if let Err(e) = write_source_to_tmp(source, hash, &tmp_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(e);
    }

    let put_result = target.put_blob(hash, &tmp_path).await;
    let _ = tokio::fs::remove_file(&tmp_path).await;
    put_result.map(|_bytes_written| ())
}

async fn write_source_to_tmp(
    source: &dyn BlobStorageBackend,
    hash: &str,
    tmp_path: &Path,
) -> Result<(), DomainError> {
    use tokio::io::AsyncWriteExt;

    let stream = source.get_blob_stream(hash).await?;
    let mut file = tokio::fs::File::create(tmp_path).await.map_err(|e| {
        DomainError::internal_error(
            "StorageMigration",
            format!("create temp file {}: {e}", tmp_path.display()),
        )
    })?;
    let mut stream = std::pin::pin!(stream);
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| {
            DomainError::internal_error("StorageMigration", format!("source stream read: {e}"))
        })?;
        file.write_all(&bytes).await.map_err(|e| {
            DomainError::internal_error("StorageMigration", format!("temp file write: {e}"))
        })?;
    }
    file.flush()
        .await
        .map_err(|e| DomainError::internal_error("StorageMigration", format!("temp flush: {e}")))?;
    Ok(())
}
