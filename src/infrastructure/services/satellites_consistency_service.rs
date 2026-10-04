//! `satellites_consistency` — the last unbuilt row of the coverage matrix.
//!
//! Walks both satellite tables and reports mappings pointing at Blobs that no
//! longer exist. One job rather than two, because the tables are one concept
//! — the content-keyed and file-keyed halves of "things attached to a Blob" —
//! and the vocabulary already exists in `storage.copy_file_satellites`.
//!
//! ### Why nothing else finds these
//!
//! Every other job reasons from a Blob outwards: `blobs_consistency` and
//! `manifests_consistency` recompute refcounts for rows that exist,
//! `backend_consistency` merge-joins the registry against the backend. A
//! satellite row whose SOURCE is gone breaks none of those invariants — the
//! row holds a valid reference to a real artifact, the refcount is exactly
//! right, and the bytes are present on the backend. Every check agrees the
//! system is healthy.
//!
//! It is only wrong one level up: nothing will ever reap that source again,
//! so `purge_derived_blobs` can never fire, so the mapping is unreachable and
//! its artifact is pinned forever. A leak that looks like correctness, which
//! is why it survived four full suite runs before being named.
//!
//! That is not hypothetical — it shipped. Background thumbnail generation is
//! spawned and unawaited, so an upload deleted promptly had its render
//! complete after GC reaped the blob and then record three mappings to a
//! corpse. Fixed at the write side in `store_derived_blob`, which now refuses
//! a mapping whose source is gone; this job finds the ones already on disk,
//! which that fix cannot reach.
//!
//! ### Per-row checks
//!
//! * `derived_orphan_mapping` (`inconsistent`) — a `content_derived_blobs`
//!   row whose `source_hash` has neither a manifest nor a blob row. Storage
//!   that grows and never reclaims.
//! * `derived_dangling_blob` (`inconsistent`) — its `blob_hash` has no Blob.
//!   The mapping promises an artifact that is gone, so a read finds the row
//!   and then fails. Recoverable in practice: a derived artifact is a pure
//!   function of its source, so re-rendering restores it — which is exactly
//!   why this is not `data_loss`. It used to be, beside a payload that said
//!   `"recoverable": true` in the same call, and alerting is severity-gated
//!   at `data_loss` by default: a regenerable thumbnail was paging an
//!   operator as loudly as missing user bytes.
//! * `attached_dangling_blob` (`data_loss`) — the same for
//!   `file_attached_blobs`, and **the one that cannot be recovered**. These
//!   bytes are user-supplied — a client-generated PDF preview has no
//!   server-side render path — so there is nothing to regenerate from. Same
//!   finding shape as the derived case, materially higher stakes.
//!
//! There is deliberately no orphan-mapping check for the attached table:
//! `file_id` is `REFERENCES storage.files(id) ON DELETE CASCADE`, so a row
//! cannot outlive its file. The database enforces what the derived table
//! cannot, since a content hash has no row to point a foreign key at — which
//! is precisely why only that half could rot.
//!
//! Read-only by default. Findings name a row rather than a range, so recovery
//! can act on them individually.
//!
//! ### `?repair=true`
//!
//! Deletes an orphaned or dangling **derived** mapping and releases the blob
//! reference it held. The row was only ever the symptom: the pinned reference
//! is the leak, and for an orphan nothing will ever reach it again — that is
//! the finding's definition.
//!
//! The freed blob goes through the **deletion queue**, not a direct unlink.
//! `remove_reference` does that already, which is why it is called rather
//! than reimplemented: the drain re-verifies under a row lock immediately
//! before unlinking, so content that became referenced again in the meantime
//! is never deleted — its queued intent is discarded instead.
//!
//! **The attached table is never repaired.** Those bytes are user-supplied
//! and nothing can recompute them, so the only safe action is to tell
//! someone. The asymmetry is provenance, not caution.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::infrastructure::scheduler::{
    JobParam, JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, RecoverableJobHandler,
    RunOutcome, RunStatus, record_or_log,
};
use crate::infrastructure::services::blob_handler::BlobHandler;

/// Kept at module scope so `parameters()` can return a `'static` slice.
static PARAMETERS: [JobParam; 1] = [JobParam::boolean(
    "repair",
    false,
    "Delete orphaned derived mappings and release the blob references they \
     pin. Default off — the run only reports. Never offered for \
     file_attached_blobs: those bytes were uploaded by a user and are not \
     recomputable, so the only safe action there is to tell someone.",
)];

pub const SATELLITES_CONSISTENCY_JOB_NAME: &str = "satellites_consistency";

/// Rows per page. Existence probes fold into the page query, so a page costs
/// one round-trip rather than `2 × rows`.
const BATCH_SIZE: i64 = 500;

/// "Does this hash name a Blob?" — either table, because a Blob is a manifest
/// for CDC content and a bare `storage.blobs` row for legacy whole-file
/// content. Checking one would report every legacy blob as missing.
macro_rules! blob_exists {
    ($col:literal) => {
        concat!(
            "(EXISTS (SELECT 1 FROM storage.chunk_manifests m WHERE m.file_hash = ",
            $col,
            ") OR EXISTS (SELECT 1 FROM storage.blobs b WHERE b.hash = ",
            $col,
            "))"
        )
    };
}

pub struct SatellitesConsistencyCheck {
    pool: Arc<PgPool>,
    /// Releases a blob reference when `?repair=true` deletes the mapping
    /// that held it.
    ///
    /// Injected rather than reimplemented as a local decrement:
    /// `remove_reference` already handles the CDC manifest path,
    /// the legacy whole-file path, and the reap-and-enqueue that hands a
    /// zero-reference blob to `backend_reclaim`. A second decrement
    /// written here would be a fourth refcount surface in a plan whose
    /// whole subject is that there are already too many.
    dedup: Arc<BlobHandler>,
}

#[derive(Debug, sqlx::FromRow)]
struct DerivedRow {
    source_hash: String,
    kind: String,
    variant: String,
    /// `None` on a NEGATIVE row — the derivation was attempted and is
    /// known not to be worth storing for this content (a transcode that
    /// came out larger, an undecodable source). Those rows point at
    /// nothing on purpose and must not be read as dangling.
    blob_hash: Option<String>,
    source_exists: bool,
    artifact_exists: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct AttachedRow {
    file_id: Uuid,
    kind: String,
    variant: String,
    blob_hash: String,
    uploaded_by: Uuid,
    artifact_exists: bool,
}

impl SatellitesConsistencyCheck {
    pub fn new(pool: Arc<PgPool>, dedup: Arc<BlobHandler>) -> Self {
        Self { pool, dedup }
    }

    /// Delete one orphaned derived mapping and release the reference it
    /// held, returning whether the blob reached zero references (and so
    /// was queued for reclamation).
    ///
    /// **Delete the row first, release second**, and the order is the
    /// whole safety argument. If the release fails after the row is gone
    /// we have leaked a reference — recoverable, and `blobs_consistency`
    /// finds it. The reverse order risks leaving a mapping that points
    /// at a blob which has just been queued for unlink: a dangling
    /// artifact served to a user. Part C's rule decides it — best-effort
    /// must err toward leak, never loss.
    async fn repair_derived_mapping(
        &self,
        source_hash: &str,
        kind: &str,
        variant: &str,
        blob_hash: Option<&str>,
    ) -> Result<bool, String> {
        sqlx::query(
            "DELETE FROM storage.content_derived_blobs
              WHERE source_hash = $1 AND kind = $2 AND variant = $3",
        )
        .bind(source_hash)
        .bind(kind)
        .bind(variant)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| format!("delete derived mapping: {e}"))?;

        // A NEGATIVE row points at nothing by design, so there is no
        // reference to give back.
        let Some(hash) = blob_hash else {
            return Ok(false);
        };

        self.dedup
            .remove_reference(hash)
            .await
            .map_err(|e| format!("release derived blob reference: {e}"))
    }

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

    /// Both page queries key on the full primary key with a row-value
    /// comparison, not on the first column: a source (or file) has several
    /// variants, so a page boundary can fall inside one and advancing by the
    /// first column alone would skip the rest. The tuple form also matches
    /// the primary key's own ordering, so it stays index-friendly.
    const DERIVED_PAGE_SQL: &'static str = concat!(
        "SELECT d.source_hash, d.kind, d.variant, d.blob_hash, ",
        blob_exists!("d.source_hash"),
        " AS source_exists, ",
        // A NEGATIVE row (NULL blob_hash) has no artifact BY DESIGN, so it
        // counts as satisfied. Without this it reads as dangling: SQL
        // comparison against NULL is NULL, so `EXISTS` is false, and every
        // "this content is not worth transcoding" verdict would be reported
        // as `data_loss`. The check has to be here rather than in the Rust
        // arm below, so the column means "this row is in the state it
        // should be" for both row shapes.
        "(d.blob_hash IS NULL OR ",
        blob_exists!("d.blob_hash"),
        ") AS artifact_exists
           FROM storage.content_derived_blobs d
          WHERE ($1::text IS NULL
                 OR (d.source_hash, d.kind, d.variant) > ($1::text, $2::text, $3::text))
          ORDER BY d.source_hash, d.kind, d.variant
          LIMIT $4"
    );

    const ATTACHED_PAGE_SQL: &'static str = concat!(
        "SELECT a.file_id, a.kind, a.variant, a.blob_hash, a.uploaded_by, ",
        blob_exists!("a.blob_hash"),
        " AS artifact_exists
           FROM storage.file_attached_blobs a
          WHERE ($1::uuid IS NULL
                 OR (a.file_id, a.kind, a.variant) > ($1::uuid, $2::text, $3::text))
          ORDER BY a.file_id, a.kind, a.variant
          LIMIT $4"
    );
}

/// Cursor is `{phase}\n{a}\n{b}\n{c}`.
///
/// The phase is what lets one job walk two tables and still resume exactly:
/// without it, a cursor from the attached pass would be replayed against the
/// derived table and silently re-scan or skip. Newline is a safe delimiter —
/// hashes are hex, uuids are uuids, `kind` comes from a CHECK constraint, and
/// `variant` is a size/format token.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Phase {
    Derived,
    Attached,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Derived => "derived",
            Phase::Attached => "attached",
        }
    }
}

fn encode_cursor(phase: Phase, a: &str, b: &str, c: &str) -> Vec<u8> {
    format!("{}\n{a}\n{b}\n{c}", phase.as_str()).into_bytes()
}

type Cursor = Option<(Phase, String, String, String)>;

fn decode_cursor(bytes: Vec<u8>) -> Result<Cursor, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let s = String::from_utf8(bytes).map_err(|e| format!("not valid UTF-8: {e}"))?;
    let mut parts = s.splitn(4, '\n');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("derived"), Some(a), Some(b), Some(c)) => {
            Ok(Some((Phase::Derived, a.into(), b.into(), c.into())))
        }
        (Some("attached"), Some(a), Some(b), Some(c)) => {
            Ok(Some((Phase::Attached, a.into(), b.into(), c.into())))
        }
        _ => Err(format!("malformed cursor: {s:?}")),
    }
}

#[async_trait]
impl RecoverableJobHandler for SatellitesConsistencyCheck {
    fn name(&self) -> &str {
        SATELLITES_CONSISTENCY_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Walks both satellite tables — content_derived_blobs (thumbnails \
         keyed by source content) and file_attached_blobs (previews keyed \
         by file) — and reports mappings whose source or target no longer \
         exists. Nothing else finds these: every other job reasons from a \
         Blob outwards, and a satellite row pointing at a deleted source \
         breaks none of their invariants. Read-only unless ?repair=true."
    }

    fn mutates(&self) -> Mutates {
        // Conditional, not Always: a default run reports and changes
        // nothing, which is what makes it safe to schedule weekly in the
        // consistency batch.
        Mutates::OnRepairOnly
    }

    fn repair_description(&self) -> Option<&'static str> {
        Some(
            "Deletes orphaned derived mappings and releases the blob \
             references they pin, routing any blob that reaches zero \
             references through the deletion queue rather than unlinking \
             it here — the drain re-verifies under a row lock, so content \
             that became referenced again is never deleted. Mappings whose \
             artifact is missing are deleted too, since derived content is \
             a pure function of its source and re-renders on the next \
             request. file_attached_blobs is never touched: those bytes \
             were uploaded by a user and nothing can recompute them.",
        )
    }

    fn parameters(&self) -> &'static [JobParam] {
        &PARAMETERS
    }

    async fn count_total(&self) -> Option<u64> {
        sqlx::query_as::<_, (i64,)>(
            "SELECT (SELECT COUNT(*) FROM storage.content_derived_blobs)
                  + (SELECT COUNT(*) FROM storage.file_attached_blobs)",
        )
        .fetch_one(self.pool.as_ref())
        .await
        .ok()
        .map(|(n,)| n.max(0) as u64)
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        let repair = args.get_bool("repair");
        let start = match resume_cursor.map(decode_cursor).transpose() {
            Ok(c) => c.flatten(),
            Err(message) => return RunOutcome::Failed { message },
        };

        let mut finding_count = 0u64;
        // Repair counters, surfaced on the run so "what did ?repair=true
        // actually do" is answerable without reading logs — and so a
        // sweep that failed half its repairs cannot report as a clean
        // success.
        let mut repaired_count = 0u64;
        let mut queued_count = 0u64;
        let mut repair_failures = 0u64;

        // ── Phase 1: content-keyed ───────────────────────────────────────
        // Skipped entirely when resuming mid-attached, since that phase runs
        // strictly after this one.
        let mut derived_cursor = match &start {
            Some((Phase::Attached, ..)) => None,
            Some((Phase::Derived, a, b, c)) => Some((a.clone(), b.clone(), c.clone())),
            None => None,
        };
        let skip_derived = matches!(&start, Some((Phase::Attached, ..)));

        if !skip_derived {
            loop {
                if let Some(outcome) = poll_cancel(
                    store,
                    derived_cursor
                        .as_ref()
                        .map(|(a, b, c)| encode_cursor(Phase::Derived, a, b, c)),
                )
                .await
                {
                    return outcome;
                }

                let (ch, ck, cv) = match &derived_cursor {
                    Some((a, b, c)) => (Some(a.as_str()), Some(b.as_str()), Some(c.as_str())),
                    None => (None, None, None),
                };

                let rows: Vec<DerivedRow> = match sqlx::query_as(Self::DERIVED_PAGE_SQL)
                    .bind(ch)
                    .bind(ck)
                    .bind(cv)
                    .bind(BATCH_SIZE)
                    .fetch_all(self.pool.as_ref())
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        return RunOutcome::Failed {
                            message: format!("derived page: {e}"),
                        };
                    }
                };
                if rows.is_empty() {
                    break;
                }

                for row in &rows {
                    if !row.source_exists {
                        finding_count += 1;
                        // Under `?repair=true`, release the reference
                        // instead of only naming it. The row is the
                        // symptom; the pinned reference is the leak, and
                        // nothing else will ever reach it — that is the
                        // definition of this finding.
                        let repaired = if repair {
                            match self
                                .repair_derived_mapping(
                                    &row.source_hash,
                                    &row.kind,
                                    &row.variant,
                                    row.blob_hash.as_deref(),
                                )
                                .await
                            {
                                Ok(queued) => {
                                    repaired_count += 1;
                                    if queued {
                                        queued_count += 1;
                                    }
                                    Some(queued)
                                }
                                Err(e) => {
                                    // Audited and reported on the finding
                                    // rather than failing the run: the
                                    // remaining orphans are still worth
                                    // walking, and a half-repaired sweep
                                    // that reported success would be the
                                    // worse outcome.
                                    tracing::warn!(
                                        target: "oxicloud::consistency",
                                        event = "satellites_consistency.repair_failed",
                                        source_hash = %row.source_hash,
                                        kind = %row.kind,
                                        variant = %row.variant,
                                        error = %e,
                                        "could not repair an orphaned derived mapping: {e}"
                                    );
                                    repair_failures += 1;
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        record_or_log(
                            store,
                            SATELLITES_CONSISTENCY_JOB_NAME,
                            "derived_orphan_mapping",
                            "inconsistent",
                            None,
                            serde_json::json!({
                                "source_hash": row.source_hash,
                                "kind":        row.kind,
                                "variant":     row.variant,
                                "blob_hash":   row.blob_hash,
                                "repaired":    repaired.is_some(),
                                "blob_queued": repaired,
                                "note": "source Blob is gone, so purge_derived_blobs can never \
                                         fire; this row pins its artifact forever",
                            }),
                        )
                        .await;
                    }
                    if !row.artifact_exists {
                        finding_count += 1;
                        // `inconsistent`, not `data_loss`. The payload
                        // here always said `"recoverable": true` and
                        // "re-rendering restores it" while the severity
                        // beside it claimed bytes were gone — one call
                        // contradicting itself. Nothing is lost: the
                        // artifact is a pure function of a source that
                        // still exists.
                        //
                        // It is not cosmetic any more either. Alerting is
                        // severity-gated and the shipped floor is
                        // `data_loss`, so this was paging an operator for
                        // a regenerable thumbnail exactly as loudly as
                        // for missing user bytes — the fastest way to get
                        // a channel muted.
                        //
                        // `attached_dangling_blob` below keeps
                        // `data_loss`, and the difference is provenance,
                        // not taste: those bytes were uploaded and
                        // nothing can recompute them.
                        let repaired = if repair {
                            match self
                                .repair_derived_mapping(
                                    &row.source_hash,
                                    &row.kind,
                                    &row.variant,
                                    // The artifact is already gone, so
                                    // there is nothing to hand to the
                                    // queue — but the reference it held
                                    // still has to come back, or the
                                    // chunks underneath stay pinned.
                                    row.blob_hash.as_deref(),
                                )
                                .await
                            {
                                Ok(_) => {
                                    repaired_count += 1;
                                    true
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        target: "oxicloud::consistency",
                                        event = "satellites_consistency.repair_failed",
                                        source_hash = %row.source_hash,
                                        kind = %row.kind,
                                        variant = %row.variant,
                                        error = %e,
                                        "could not drop a dangling derived mapping: {e}"
                                    );
                                    repair_failures += 1;
                                    false
                                }
                            }
                        } else {
                            false
                        };
                        record_or_log(
                            store,
                            SATELLITES_CONSISTENCY_JOB_NAME,
                            "derived_dangling_blob",
                            "inconsistent",
                            None,
                            serde_json::json!({
                                "source_hash": row.source_hash,
                                "kind":        row.kind,
                                "variant":     row.variant,
                                "blob_hash":   row.blob_hash,
                                "recoverable": true,
                                "repaired":    repaired,
                                "note": "artifact missing; derived content is a pure function of \
                                         its source, so re-rendering restores it",
                            }),
                        )
                        .await;
                    }
                }

                let scanned = rows.len() as u64;
                let last = rows.last().unwrap();
                derived_cursor = Some((
                    last.source_hash.clone(),
                    last.kind.clone(),
                    last.variant.clone(),
                ));
                if let Err(e) = store
                    .checkpoint(
                        encode_cursor(Phase::Derived, &last.source_hash, &last.kind, &last.variant),
                        scanned,
                    )
                    .await
                {
                    return RunOutcome::Failed {
                        message: format!("checkpoint: {e}"),
                    };
                }
                if scanned < BATCH_SIZE as u64 {
                    break;
                }
            }
        }

        // ── Phase 2: file-keyed ──────────────────────────────────────────
        // No orphan-mapping check here: `file_id` is ON DELETE CASCADE, so a
        // row cannot outlive its file. Only the artifact side can rot.
        let mut attached_cursor: Option<(Uuid, String, String)> = match &start {
            Some((Phase::Attached, a, b, c)) => match Uuid::parse_str(a) {
                Ok(id) => Some((id, b.clone(), c.clone())),
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("attached cursor is not a uuid: {e}"),
                    };
                }
            },
            _ => None,
        };

        loop {
            if let Some(outcome) = poll_cancel(
                store,
                attached_cursor
                    .as_ref()
                    .map(|(a, b, c)| encode_cursor(Phase::Attached, &a.to_string(), b, c)),
            )
            .await
            {
                return outcome;
            }

            let (ch, ck, cv) = match &attached_cursor {
                Some((a, b, c)) => (Some(*a), Some(b.as_str()), Some(c.as_str())),
                None => (None, None, None),
            };

            let rows: Vec<AttachedRow> = match sqlx::query_as(Self::ATTACHED_PAGE_SQL)
                .bind(ch)
                .bind(ck)
                .bind(cv)
                .bind(BATCH_SIZE)
                .fetch_all(self.pool.as_ref())
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("attached page: {e}"),
                    };
                }
            };
            if rows.is_empty() {
                break;
            }

            for row in &rows {
                if !row.artifact_exists {
                    finding_count += 1;
                    record_or_log(
                        store,
                        SATELLITES_CONSISTENCY_JOB_NAME,
                        "attached_dangling_blob",
                        "data_loss",
                        None,
                        serde_json::json!({
                            "file_id":     row.file_id,
                            "kind":        row.kind,
                            "variant":     row.variant,
                            "blob_hash":   row.blob_hash,
                            "uploaded_by": row.uploaded_by,
                            "recoverable": false,
                            "note": "UNRECOVERABLE: these bytes were user-supplied and have no \
                                     server-side render path, so nothing can regenerate them",
                        }),
                    )
                    .await;
                }
            }

            let scanned = rows.len() as u64;
            let last = rows.last().unwrap();
            attached_cursor = Some((last.file_id, last.kind.clone(), last.variant.clone()));
            if let Err(e) = store
                .checkpoint(
                    encode_cursor(
                        Phase::Attached,
                        &last.file_id.to_string(),
                        &last.kind,
                        &last.variant,
                    ),
                    scanned,
                )
                .await
            {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }
            if scanned < BATCH_SIZE as u64 {
                break;
            }
        }

        tracing::info!(
            target: "oxicloud::consistency",
            event = "satellites_consistency.completed",
            run_id = %store.run_id(),
            finding_count = finding_count,
            repair = repair,
            repaired_count = repaired_count,
            queued_count = queued_count,
            repair_failures = repair_failures,
            "satellites_consistency completed with {} finding(s)",
            finding_count
        );

        if !repair {
            return RunOutcome::completed();
        }
        RunOutcome::Completed {
            extra_stats: serde_json::Map::from_iter([
                ("repaired".to_string(), repaired_count.into()),
                ("blobs_queued".to_string(), queued_count.into()),
                ("repair_failures".to_string(), repair_failures.into()),
            ]),
        }
    }
}

/// Cooperative cancel, shared by both phases so neither can forget it.
async fn poll_cancel(store: &dyn JobStore, cursor: Option<Vec<u8>>) -> Option<RunOutcome> {
    match store.status().await {
        Ok(RunStatus::CancelRequested) => Some(RunOutcome::Paused {
            cursor: cursor.unwrap_or_default(),
        }),
        Ok(_) => None,
        Err(e) => Some(RunOutcome::Failed {
            message: format!("status poll: {e}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phase is what lets one job walk two tables and resume exactly.
    /// Without it an attached cursor would be replayed against the derived
    /// table, silently re-scanning or skipping — an audit job under-reporting
    /// is the worst failure available to it.
    #[test]
    fn cursor_round_trips_and_keeps_its_phase() {
        for phase in [Phase::Derived, Phase::Attached] {
            let encoded = encode_cursor(phase, "0a1b", "thumbnail", "preview.webp");
            assert_eq!(
                decode_cursor(encoded).unwrap(),
                Some((
                    phase,
                    "0a1b".to_string(),
                    "thumbnail".to_string(),
                    "preview.webp".to_string()
                ))
            );
        }
    }

    #[test]
    fn empty_cursor_starts_from_the_beginning() {
        assert_eq!(decode_cursor(Vec::new()).unwrap(), None);
    }

    /// Loudly, rather than silently restarting: a corrupt checkpoint that
    /// reads as "start over" gives a job that never finishes and never says
    /// why.
    #[test]
    fn malformed_cursor_is_an_error() {
        assert!(decode_cursor(b"only-one-field".to_vec()).is_err());
        assert!(decode_cursor(b"bogus\na\nb\nc".to_vec()).is_err());
    }

    /// The repair arm must be conditional, not always-on. A detector that
    /// deleted rows on every tick would be repairing with nobody's
    /// consent after the first time — which is why `repair` cannot be
    /// scheduled (`OXICLOUD_JOBS_SCHEDULED` refuses it at boot) and why
    /// `consistency_batch` can sweep this job weekly at all.
    // `#[tokio::test]` because the lazy pool below needs a reactor —
    // `connect_lazy` panics with "requires a Tokio context" otherwise,
    // which is why every `new_stub` test is async too.
    #[tokio::test]
    async fn the_job_declares_itself_destructive_only_under_repair() {
        let job = SatellitesConsistencyCheck::new(
            Arc::new(
                sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                    .max_connections(1)
                    .connect_lazy("postgres://invalid:5432/none")
                    .expect("lazy pool"),
            ),
            Arc::new(BlobHandler::new_stub()),
        );
        assert_eq!(job.mutates(), Mutates::OnRepairOnly);
        assert!(
            job.repair_description().is_some(),
            "the UI offers the toggle only when this is Some, and writes its \
             confirmation text from it"
        );
        // Declared, or `?repair=true` is rejected as an unknown parameter
        // by `JobRunArgs::from_declared` — the failure `backend_consistency`
        // shipped with for one boot.
        assert!(job.parameters().iter().any(|p| p.name == "repair"));
    }
}

// Gated on `--cfg integration_tests` (see `just test-integration`).
// Requires a test PG on 5433 with `oxicloud_test`, schema applied via
// `tests/common/init-test-schema.sh`:
//   just test-integration --  satellites_consistency_service
//
// The repair arm cannot be unit-tested and cannot be driven from the API
// suite either: an orphan mapping is by definition a row whose source is
// already gone, and the race that produces one in production — a render
// landing after its source was reaped — is not reproducible on demand.
// So it is seeded directly, the same approach
// `drives_consistency_service` takes for drift.
#[cfg(integration_tests)]
#[allow(dead_code, unused_imports)]
mod integration_tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Arc<sqlx::PgPool> {
        let url = crate::integration_test_support::test_db_url();
        Arc::new(
            PgPoolOptions::new()
                .max_connections(4)
                .connect(&url)
                .await
                .expect("connect to test DB — run tests/common/spawn-db.sh first"),
        )
    }

    /// One orphaned derived mapping: a manifest holding the artifact at
    /// `ref_count = 1`, plus a `content_derived_blobs` row whose
    /// `source_hash` exists nowhere. That is the leak — nothing will
    /// reap that source again, so `purge_derived_blobs` can never fire
    /// and the artifact stays pinned forever.
    ///
    /// Hashes are derived from `tag` so two tests cannot collide.
    async fn seed_orphan(pool: &sqlx::PgPool, tag: &str) -> (String, String) {
        let source = format!("dead{tag}{}", "0".repeat(64 - 4 - tag.len()));
        let artifact = format!("a4t{tag}{}", "0".repeat(64 - 3 - tag.len()));

        sqlx::query(
            "INSERT INTO storage.chunk_manifests
                 (file_hash, chunk_hashes, chunk_sizes, total_size, chunk_count, ref_count)
             VALUES ($1, ARRAY[$1], ARRAY[1::BIGINT], 1, 1, 1)
             ON CONFLICT (file_hash) DO UPDATE SET ref_count = 1",
        )
        .bind(&artifact)
        .execute(pool)
        .await
        .expect("seed artifact manifest");

        // The chunk the manifest points at, as a real `storage.blobs` row.
        // Needed because releasing the last manifest reference decrements
        // its CHUNKS — without this row the decrement updates nothing and
        // the test can observe no effect at all.
        sqlx::query(
            "INSERT INTO storage.blobs (hash, size, ref_count)
             VALUES ($1, 1, 1)
             ON CONFLICT (hash) DO UPDATE SET ref_count = 1, orphaned_at = NULL",
        )
        .bind(&artifact)
        .execute(pool)
        .await
        .expect("seed artifact chunk row");

        sqlx::query(
            "INSERT INTO storage.content_derived_blobs
                 (source_hash, kind, variant, blob_hash, content_type)
             VALUES ($1, 'thumbnail', 'integration', $2, 'image/webp')
             ON CONFLICT (source_hash, kind, variant)
                 DO UPDATE SET blob_hash = EXCLUDED.blob_hash",
        )
        .bind(&source)
        .bind(&artifact)
        .execute(pool)
        .await
        .expect("seed orphan mapping");

        (source, artifact)
    }

    async fn mapping_count(pool: &sqlx::PgPool, source: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.content_derived_blobs WHERE source_hash = $1",
        )
        .bind(source)
        .fetch_one(pool)
        .await
        .expect("count mappings")
    }

    async fn manifest_refcount(pool: &sqlx::PgPool, hash: &str) -> Option<i32> {
        sqlx::query_scalar("SELECT ref_count FROM storage.chunk_manifests WHERE file_hash = $1")
            .bind(hash)
            .fetch_optional(pool)
            .await
            .expect("read manifest refcount")
    }

    /// `(ref_count, orphaned_at IS NOT NULL)` for a chunk row, or `None`
    /// if the row is gone.
    async fn chunk_state(pool: &sqlx::PgPool, hash: &str) -> Option<(i32, bool)> {
        sqlx::query_as::<_, (i32, bool)>(
            "SELECT ref_count, orphaned_at IS NOT NULL
               FROM storage.blobs WHERE hash = $1",
        )
        .bind(hash)
        .fetch_optional(pool)
        .await
        .expect("read chunk state")
    }

    async fn cleanup(pool: &sqlx::PgPool, source: &str, artifact: &str) {
        let _ = sqlx::query("DELETE FROM storage.content_derived_blobs WHERE source_hash = $1")
            .bind(source)
            .execute(pool)
            .await;
        for sql in [
            "DELETE FROM storage.pending_actions WHERE hash = $1",
            "DELETE FROM storage.chunk_manifests WHERE file_hash = $1",
            "DELETE FROM storage.blobs WHERE hash = $1",
        ] {
            let _ = sqlx::query(sql).bind(artifact).execute(pool).await;
        }
    }

    /// Drive the job through the real engine rather than calling
    /// `run_resumable` directly, mirroring `drives_consistency_service`:
    /// `PgJobStoreProvider` opens the run row, `run_or_resume` dispatches
    /// and writes the terminal status. A direct call would skip the
    /// store the findings and the repair counters are written through.
    async fn run_job(pool: &Arc<sqlx::PgPool>, args: &JobRunArgs) {
        let provider: Arc<dyn JobStoreProvider> = Arc::new(
            crate::infrastructure::scheduler::PgJobStoreProvider::new(pool.clone()),
        );
        let handler: Arc<dyn RecoverableJobHandler> = Arc::new(SatellitesConsistencyCheck::new(
            pool.clone(),
            Arc::new(BlobHandler::new_for_test(pool.clone())),
        ));
        let outcome =
            crate::infrastructure::scheduler::run_or_resume(handler, provider, args).await;
        assert!(outcome.is_ok(), "run must complete: {outcome:?}");

        // `is_ok()` alone is not enough, and this assertion exists because
        // the absence of it cost a debugging round. `open_or_start` enforces
        // one non-terminal run per job, so a caller arriving while another
        // run is open gets a successful "already running" envelope with no
        // dispatch at all — indistinguishable from a clean run that simply
        // found nothing to do. `completed` is only set by the Completed arm,
        // so it proves the handler actually walked.
        let crate::infrastructure::scheduler::JobOutcome::Ok { extra, .. } = outcome else {
            panic!("expected Ok");
        };
        assert_eq!(
            extra.get("completed").and_then(|v| v.as_bool()),
            Some(true),
            "the run did not dispatch — most likely another run of this job \
             was still open: {extra}"
        );
    }

    /// Both halves of the contract, in one test and deliberately so.
    ///
    /// They cannot be two `#[tokio::test]`s: cargo runs tests in
    /// parallel, `open_or_start` permits one non-terminal run per job,
    /// and the loser attaches to the winner's run rather than
    /// dispatching — so the second test got a successful envelope, never
    /// executed, and failed on an untouched row. One test, two sequential
    /// runs of the same job, is the shape that works.
    ///
    /// Phase 1 — a default run reports and deletes nothing. That is what
    /// makes the job safe for the weekly consistency batch to sweep.
    ///
    /// Phase 2 — `?repair=true` deletes the mapping AND gives the
    /// reference back. The second half is the one that matters: the row
    /// is only the symptom, the pinned reference is the leak. The freed
    /// blob must be **queued**, not unlinked from inside the scan, so
    /// that the drain can re-verify under a row lock and skip content
    /// that became referenced again.
    #[tokio::test]
    async fn repair_is_opt_in_and_releases_the_reference() {
        let pool = test_pool().await;
        let (source, artifact) = seed_orphan(&pool, "rp").await;

        // ── Phase 1: discovery only ──────────────────────────────────
        run_job(&pool, &JobRunArgs::default()).await;

        assert_eq!(
            mapping_count(&pool, &source).await,
            1,
            "a read-only run deleted an orphan mapping"
        );
        assert_eq!(
            manifest_refcount(&pool, &artifact).await,
            Some(1),
            "a read-only run released a reference"
        );
        assert_eq!(
            chunk_state(&pool, &artifact).await,
            Some((1, false)),
            "a read-only run orphaned a chunk"
        );

        // ── Phase 2: repair ──────────────────────────────────────────
        let args = JobRunArgs::new(
            [(
                "repair".to_string(),
                crate::infrastructure::scheduler::JobParamValue::Boolean(true),
            )]
            .into_iter()
            .collect(),
        );
        run_job(&pool, &args).await;

        assert_eq!(
            mapping_count(&pool, &source).await,
            0,
            "repair left the orphan mapping in place"
        );
        // The manifest is GONE, not merely decremented: releasing the last
        // reference deletes it. Asserting `ref_count == 0` would pass on a
        // decrement that never reached zero, which is the half-fixed state
        // this job exists to prevent.
        assert_eq!(
            manifest_refcount(&pool, &artifact).await,
            None,
            "repair deleted the mapping but never released its reference — \
             the leak this job exists to close"
        );
        // And the chunk underneath is reclaimable: dereferenced and
        // stamped `orphaned_at`.
        //
        // Deliberately NOT asserting a `pending_actions` row here. The
        // enqueue is one hop further on — `remove_reference` stops at
        // orphaning, and `garbage_collect` queues the chunk only once it
        // is orphaned past the grace window, because a chunk hash can be
        // re-uploaded concurrently and unlinking right after this commit
        // would race that re-reference. So the chain is: release → orphan
        // → dedup_gc → pending_actions → backend_reclaim, and this test
        // owns the first hop. Asserting the third one immediately is how
        // the first version of this test failed.
        assert_eq!(
            chunk_state(&pool, &artifact).await,
            Some((0, true)),
            "the freed chunk must be dereferenced and orphaned, so the GC \
             can reclaim it — not unlinked from inside the scan"
        );

        cleanup(&pool, &source, &artifact).await;
    }
}
