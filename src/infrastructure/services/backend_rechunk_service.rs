//! `backend_rechunk` — convert pre-CDC whole-file blobs into CDC chunks.
//!
//! The conversion itself is not new: `BlobHandler::rechunk_legacy_blobs` has
//! always done it, spawned as a detached task at boot behind
//! `OXICLOUD_LEGACY_RECHUNK`. What is new is that it is a *job*
//! (`docs/plan/storage-consistency.md` §1a).
//!
//! A detached task has no admin trigger, no run history, no findings and no
//! visible cursor, so the only way to answer "has this converged on my
//! instance?" is to grep boot logs — and the answer matters, because §1's
//! reason for existing is to leave **one** content model rather than two.
//! Every code path that still has to ask "manifest or legacy blob?" is
//! surface that cannot be deleted until this reports zero.
//!
//! As a job it also becomes pausable. That is not cosmetic on a remote
//! backend: converting a blob means reading it in full, so on S3 the sweep is
//! egress and wall-clock, and an operator needs to be able to stop it.

use async_trait::async_trait;

use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, RecoverableJobHandler,
    RunOutcome, RunStatus, record_or_log,
};
use crate::infrastructure::services::blob_handler::BlobHandler;
use std::sync::Arc;

/// Stable job name — also the admin URL fragment.
///
/// `backend_` groups it with `backend_consistency` / `backend_migration` /
/// `backend_rotate` in the sorted admin panel, and the subject really is the
/// storage backend. `rechunk` rather than `chunk_migration` because
/// `backend_migration` already means moving to a different storage entry.
pub const BACKEND_RECHUNK_JOB_NAME: &str = "backend_rechunk";

/// Blobs per checkpoint. Deliberately small: each one is a full read of the
/// blob plus a write of every chunk, so on a remote backend a single item can
/// take seconds and a coarse checkpoint would redo real egress after a pause.
const CHECKPOINT_EVERY: usize = 8;

/// Candidates fetched per query. Independent of the checkpoint interval — one
/// round trip to Postgres is cheap next to converting even one blob.
const PAGE_SIZE: i64 = 64;

/// Consecutive **permanent** per-blob failures tolerated before giving up.
///
/// A handful of corrupt blobs must not block the rest of the sweep, but a
/// systemic fault should stop and be looked at rather than grinding through
/// every blob on the instance recording a finding for each. Transient failures
/// never reach this counter — those pause the run instead — so a streak here
/// means something that will not fix itself: a wrong encryption key making every
/// blob fail its hash check, say.
///
/// The old sweep capped at 1000 *total* failures. A consecutive streak is much
/// stronger evidence of a systemic fault than a total is, so this is far lower.
const MAX_CONSECUTIVE_FAILURES: u64 = 20;

pub struct BackendRechunk {
    dedup: Arc<BlobHandler>,
}

impl BackendRechunk {
    pub fn new(dedup: Arc<BlobHandler>) -> Self {
        Self { dedup }
    }

    pub async fn register_recoverable_job(
        self: Arc<Self>,
        registry: &JobRegistry,
        provider: &Arc<dyn JobStoreProvider>,
    ) -> Arc<Self> {
        // On-demand, with no interval. This is a migration with an end: once
        // the instance has converged, nothing creates a whole-file blob any
        // more — `store_from_stream` always writes a manifest — so the work
        // queue cannot grow again and a periodic tick would be a COUNT
        // returning zero, forever.
        //
        // Boot behaviour belongs to `OXICLOUD_JOBS_STARTUP`, which is the point
        // of retiring `OXICLOUD_LEGACY_RECHUNK`: an operator who wants the
        // sweep at boot lists it there, and one who wants to defer the egress
        // leaves it out and triggers it by hand. One knob, not two.
        registry
            .register_recoverable_job(self.clone(), provider.clone(), None)
            .await;
        self
    }
}

#[async_trait]
impl RecoverableJobHandler for BackendRechunk {
    fn name(&self) -> &str {
        BACKEND_RECHUNK_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Converts pre-CDC whole-file blobs into content-defined chunks plus a \
         manifest, so the instance has one content model instead of two. Each \
         blob is read in full, re-chunked, and its file references moved onto \
         the new manifest; the whole-file copy is then released unless it \
         doubles as its own single chunk. Files stay readable throughout — a \
         blob that has not been converted yet is served by the legacy path. \
         Incremental and resumable: a manifest row is the per-blob done \
         marker, so a re-run continues rather than repeating work. Reports \
         zero once the instance has converged, which is the signal that the \
         legacy read paths can be deleted."
    }

    fn mutates(&self) -> Mutates {
        // Always, and not gated behind `repair`: converting IS the job, so a
        // read-only default run would do nothing at all. That also makes it
        // wrong to add to `consistency_batch`, whose members are discovery-only.
        Mutates::Always
    }

    fn repair_description(&self) -> Option<&'static str> {
        // No repair arm: there is no discovery-only half to opt out of, and
        // `count_total` already answers "how much is left?" without mutating.
        None
    }

    async fn count_total(&self) -> Option<u64> {
        match self.dedup.count_legacy_blobs().await {
            Ok(n) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::warn!("backend_rechunk: legacy blob count failed: {e}");
                None
            }
        }
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        _args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // Cursor is the last hash converted. Hashes are hex, so byte order and
        // `ORDER BY hash` agree and the walk is totally ordered.
        let mut cursor: Option<String> = match resume_cursor {
            None => None,
            Some(b) if b.is_empty() => None,
            Some(b) => match String::from_utf8(b) {
                Ok(s) => Some(s),
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("invalid cursor: not valid UTF-8: {e}"),
                    };
                }
            },
        };

        let mut migrated = 0u64;
        let mut failed = 0u64;
        let mut freed_bytes = 0u64;
        let mut consecutive_failures = 0u64;
        let mut since_checkpoint = 0usize;

        loop {
            let page = match self
                .dedup
                .legacy_blob_candidates_after(cursor.as_deref(), PAGE_SIZE)
                .await
            {
                Ok(p) => p,
                // A transient DB error pauses for retry rather than failing the
                // run, so a blip does not discard the cursor we have earned.
                Err(e) => {
                    return RunOutcome::from_domain_error(
                        Some(&cursor_bytes(&cursor)),
                        "legacy candidate page",
                        &e,
                    );
                }
            };

            if page.is_empty() {
                break;
            }

            for (hash, content_type) in page {
                match store.status().await {
                    Ok(RunStatus::CancelRequested) => {
                        // Pause *before* this hash, not after: the cursor names
                        // the last hash actually converted, so resuming
                        // re-selects this one rather than skipping it.
                        return RunOutcome::Paused {
                            cursor: cursor_bytes(&cursor),
                        };
                    }
                    Ok(_) => {}
                    Err(e) => {
                        return RunOutcome::Failed {
                            message: format!("status poll: {e}"),
                        };
                    }
                }

                match self.dedup.rechunk_legacy_blob(&hash, content_type).await {
                    Ok(converted) => {
                        migrated += 1;
                        freed_bytes += converted.freed_bytes;
                        consecutive_failures = 0;

                        // Record what changed, not just how much. A log line
                        // covers the live case (see `rechunk_one_legacy_blob`),
                        // but logs rotate and this is a one-time rewrite of how
                        // a file's content is stored — the question "did this
                        // job touch the file that is now misbehaving?" arrives
                        // weeks later. `jobs.run_findings` is the only durable,
                        // drillable per-resource record an operator has.
                        //
                        // Severity "info" with a past-tense kind, following the
                        // `refcount_repaired` convention in
                        // `blobs_consistency_service`: a successful mutation is
                        // not an anomaly, and filing it as one would train
                        // operators to ignore the findings drawer.
                        //
                        // `resource_id` gets the file only when there is exactly
                        // one. A deduplicated blob can back many files, and
                        // picking an arbitrary one would make the drawer imply a
                        // single culprit; the full list is in the detail either
                        // way.
                        record_or_log(
                            store,
                            BACKEND_RECHUNK_JOB_NAME,
                            "legacy_blob_rechunked",
                            "info",
                            (converted.file_ids.len() == 1).then(|| converted.file_ids[0]),
                            serde_json::json!({
                                "hash": hash,
                                "chunk_count": converted.chunk_count,
                                "file_ids": converted.file_ids,
                                "freed_bytes": converted.freed_bytes,
                            }),
                        )
                        .await;
                    }
                    // A transient failure is the backend being unreachable, not
                    // this blob being bad — so pause at the cursor and let the
                    // engine retry, rather than marking the blob failed and
                    // walking on. Without this, an outage would march through
                    // every remaining blob recording a finding for each, and a
                    // run that converted nothing would still look like it had
                    // examined the whole instance.
                    //
                    // The cursor is NOT advanced past this hash, so the retry
                    // re-selects it. See `docs/plan/jobs-handling-recoverable-error.md`.
                    Err(e) if e.is_transient() => {
                        return RunOutcome::from_domain_error(
                            Some(&cursor_bytes(&cursor)),
                            &format!("rechunk {}", &hash[..hash.len().min(12)]),
                            &e,
                        );
                    }
                    Err(e) => {
                        failed += 1;
                        consecutive_failures += 1;
                        // A finding rather than only a log line: a blob that
                        // cannot be converted is exactly the thing an operator
                        // needs to see, and it is why the instance will never
                        // report zero. The old sweep logged and moved on, so
                        // the reason was gone by the time anyone asked.
                        record_or_log(
                            store,
                            BACKEND_RECHUNK_JOB_NAME,
                            "legacy_blob_rechunk_failed",
                            "anomaly",
                            None,
                            serde_json::json!({
                                "hash": hash,
                                "error": e.to_string(),
                            }),
                        )
                        .await;
                        tracing::error!(
                            "backend_rechunk: blob {} failed (left untouched): {e}",
                            &hash[..hash.len().min(12)],
                        );

                        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                            return RunOutcome::Failed {
                                message: format!(
                                    "aborting after {consecutive_failures} consecutive per-blob \
                                     failures — inspect blob storage integrity before re-running"
                                ),
                            };
                        }
                    }
                }

                // Advance past this hash whether it converted or not. A failed
                // hash keeps its `storage.blobs` row and no manifest, so it
                // stays a candidate for the NEXT run — which is what we want —
                // but must not be retried within this one.
                cursor = Some(hash);
                since_checkpoint += 1;

                if since_checkpoint >= CHECKPOINT_EVERY {
                    let scanned = since_checkpoint as u64;
                    since_checkpoint = 0;
                    if let Err(e) = store.checkpoint(cursor_bytes(&cursor), scanned).await {
                        return RunOutcome::Failed {
                            message: format!("checkpoint: {e}"),
                        };
                    }
                }

                // Converting a blob is CPU-heavy (hashing) and this runs on the
                // maintenance pool; yield so a long sweep cannot starve request
                // handling on a small instance.
                tokio::task::yield_now().await;
            }
        }

        if since_checkpoint > 0
            && let Err(e) = store
                .checkpoint(cursor_bytes(&cursor), since_checkpoint as u64)
                .await
        {
            return RunOutcome::Failed {
                message: format!("final checkpoint: {e}"),
            };
        }

        if migrated > 0 || failed > 0 {
            tracing::info!(
                migrated,
                failed,
                freed_bytes,
                "backend_rechunk complete: {migrated} blob(s) converted to CDC manifests, \
                 {failed} failed (left untouched), {freed_bytes} bytes of whole-file blobs freed"
            );
        }

        RunOutcome::completed_with(serde_json::json!({
            "migrated": migrated,
            "failed": failed,
            "freed_bytes": freed_bytes,
        }))
    }
}

/// `None` becomes an empty cursor — the same encoding `run_resumable` accepts
/// on the way in, so "nothing converted yet" round-trips to a fresh walk
/// instead of a cursor of the literal string "None".
fn cursor_bytes(cursor: &Option<String>) -> Vec<u8> {
    cursor
        .as_ref()
        .map(|c| c.as_bytes().to_vec())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_through_bytes() {
        // The empty encoding must mean "start over", not a literal cursor —
        // getting this wrong would make a paused-before-first-item run resume
        // past every blob whose hash sorts below the encoded sentinel.
        assert!(cursor_bytes(&None).is_empty());

        let hash = "00ff".repeat(16);
        assert_eq!(
            String::from_utf8(cursor_bytes(&Some(hash.clone()))).unwrap(),
            hash
        );
    }

    #[test]
    fn job_name_matches_the_admin_url_fragment() {
        // The name is the URL fragment and operators script against it, so it
        // is effectively permanent. Pinned so a rename has to be deliberate.
        assert_eq!(BACKEND_RECHUNK_JOB_NAME, "backend_rechunk");
        assert!(
            !BACKEND_RECHUNK_JOB_NAME.ends_with("_consistency"),
            "the _consistency suffix auto-enrols a job into consistency_batch, \
             which is discovery-only — this job always mutates"
        );
    }
}
