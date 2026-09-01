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
//! * `derived_dangling_blob` (`data_loss`) — its `blob_hash` has no Blob.
//!   The mapping promises an artifact that is gone, so a read finds the row
//!   and then fails. Recoverable in practice: a derived artifact is a pure
//!   function of its source, so re-rendering restores it.
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
//! Read-only, per the house default. Findings name a row rather than a range,
//! so recovery can act on them individually.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};

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
}

#[derive(Debug, sqlx::FromRow)]
struct DerivedRow {
    source_hash: String,
    kind: String,
    variant: String,
    blob_hash: String,
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
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
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
        blob_exists!("d.blob_hash"),
        " AS artifact_exists
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
         breaks none of their invariants. Read-only."
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
        _args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        let start = match resume_cursor.map(decode_cursor).transpose() {
            Ok(c) => c.flatten(),
            Err(message) => return RunOutcome::Failed { message },
        };

        let mut finding_count = 0u64;

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
                                "note": "source Blob is gone, so purge_derived_blobs can never \
                                         fire; this row pins its artifact forever",
                            }),
                        )
                        .await;
                    }
                    if !row.artifact_exists {
                        finding_count += 1;
                        record_or_log(
                            store,
                            SATELLITES_CONSISTENCY_JOB_NAME,
                            "derived_dangling_blob",
                            "data_loss",
                            None,
                            serde_json::json!({
                                "source_hash": row.source_hash,
                                "kind":        row.kind,
                                "variant":     row.variant,
                                "blob_hash":   row.blob_hash,
                                "recoverable": true,
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
            "satellites_consistency completed with {} finding(s)",
            finding_count
        );

        RunOutcome::completed()
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
}
