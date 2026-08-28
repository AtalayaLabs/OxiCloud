//! `derived_consistency` — the last unbuilt row of the coverage matrix.
//!
//! Walks `storage.content_derived_blobs` and reports mappings that point at
//! Blobs which no longer exist, in either direction.
//!
//! ### Why nothing else finds these
//!
//! Every other job reasons from a Blob outwards: `blobs_consistency` and
//! `manifests_consistency` recompute refcounts for rows that exist,
//! `backend_consistency` merge-joins the registry against the backend. A
//! derived row whose SOURCE is gone breaks none of those invariants — the
//! row holds a perfectly valid reference to a real artifact, the refcount is
//! exactly right, and the bytes are present on the backend. Every check
//! agrees the system is healthy.
//!
//! It is only wrong one level up: nothing will ever reap that source again,
//! so `purge_derived_blobs` can never fire, so the mapping is unreachable and
//! its artifact is pinned forever. A leak that looks like correctness.
//!
//! That is not hypothetical — it shipped. Background thumbnail generation is
//! spawned and unawaited, so an upload deleted promptly had its render
//! complete after GC reaped the blob and then record three mappings to a
//! corpse (fixed at the write side in `store_derived_blob`, which now
//! refuses a mapping whose source is gone). This job finds the ones already
//! on disk, which that fix cannot reach.
//!
//! ### Per-row checks
//!
//! * `derived_orphan_mapping` (severity `inconsistent`) — `source_hash` has
//!   neither a manifest nor a blob row. Storage overhead that grows and never
//!   reclaims. Recovery = delete the row, which releases the artifact.
//! * `derived_dangling_blob` (severity `data_loss`) — `blob_hash` has no Blob
//!   behind it. The opposite and the more serious one: the mapping promises
//!   an artifact that is gone, so a read finds a row and then fails.
//!
//! Read-only, per the house default. Both findings name a row rather than a
//! range, so recovery can act on them individually.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};

pub const DERIVED_CONSISTENCY_JOB_NAME: &str = "derived_consistency";

/// Rows per page. Each is two indexed existence probes folded into the page
/// query, so this can be larger than a job doing per-row I/O.
const BATCH_SIZE: i64 = 500;

pub struct DerivedConsistencyCheck {
    pool: Arc<PgPool>,
}

/// One row plus the two existence answers, resolved server-side so a page
/// costs one round-trip rather than `2 × rows`.
#[derive(Debug, sqlx::FromRow)]
struct DerivedRow {
    source_hash: String,
    kind: String,
    variant: String,
    blob_hash: String,
    source_exists: bool,
    artifact_exists: bool,
}

impl DerivedConsistencyCheck {
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

    /// Page query, keyed on the full primary key.
    ///
    /// Row-value comparison (`(a,b,c) > ($1,$2,$3)`) rather than
    /// `source_hash > $1`: a source has several variants, so a page boundary
    /// can fall inside one, and advancing by source alone would skip its
    /// remaining rows. The tuple form is also index-friendly — it matches the
    /// primary key's own ordering.
    ///
    /// "Exists" means EITHER table, because `source_hash` and `blob_hash` both
    /// name a Blob: a manifest for CDC content, a bare `storage.blobs` row for
    /// legacy whole-file content. Checking only one would report every legacy
    /// blob as missing.
    const PAGE_SQL: &'static str = r#"
        SELECT d.source_hash,
               d.kind,
               d.variant,
               d.blob_hash,
               (EXISTS (SELECT 1 FROM storage.chunk_manifests m WHERE m.file_hash = d.source_hash)
             OR EXISTS (SELECT 1 FROM storage.blobs           b WHERE b.hash      = d.source_hash))
                   AS source_exists,
               (EXISTS (SELECT 1 FROM storage.chunk_manifests m WHERE m.file_hash = d.blob_hash)
             OR EXISTS (SELECT 1 FROM storage.blobs           b WHERE b.hash      = d.blob_hash))
                   AS artifact_exists
          FROM storage.content_derived_blobs d
         WHERE ($1::text IS NULL
                OR (d.source_hash, d.kind, d.variant) > ($1::text, $2::text, $3::text))
         ORDER BY d.source_hash, d.kind, d.variant
         LIMIT $4"#;
}

/// Cursor is the primary-key triple, newline-joined.
///
/// Safe as a delimiter: `source_hash` is hex, `kind` comes from a CHECK
/// constraint, and `variant` is a size/format token — none can contain a
/// newline.
fn encode_cursor(r: &DerivedRow) -> Vec<u8> {
    format!("{}\n{}\n{}", r.source_hash, r.kind, r.variant).into_bytes()
}

fn decode_cursor(bytes: Vec<u8>) -> Result<Option<(String, String, String)>, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let s = String::from_utf8(bytes).map_err(|e| format!("not valid UTF-8: {e}"))?;
    let mut parts = s.splitn(3, '\n');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), Some(c)) => Ok(Some((a.into(), b.into(), c.into()))),
        _ => Err(format!(
            "expected three newline-separated fields, got {s:?}"
        )),
    }
}

#[async_trait]
impl RecoverableJobHandler for DerivedConsistencyCheck {
    fn name(&self) -> &str {
        DERIVED_CONSISTENCY_JOB_NAME
    }

    async fn count_total(&self) -> Option<u64> {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM storage.content_derived_blobs")
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
        let mut cursor = match resume_cursor.map(decode_cursor).transpose() {
            Ok(c) => c.flatten(),
            Err(message) => return RunOutcome::Failed { message },
        };

        let mut finding_count = 0u64;

        loop {
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    return RunOutcome::Paused {
                        cursor: cursor
                            .as_ref()
                            .map(|(a, b, c)| format!("{a}\n{b}\n{c}").into_bytes())
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

            let (ch, ck, cv) = match &cursor {
                Some((a, b, c)) => (Some(a.as_str()), Some(b.as_str()), Some(c.as_str())),
                None => (None, None, None),
            };

            let rows: Vec<DerivedRow> = match sqlx::query_as(Self::PAGE_SQL)
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
                        DERIVED_CONSISTENCY_JOB_NAME,
                        "derived_orphan_mapping",
                        "inconsistent",
                        None,
                        serde_json::json!({
                            "source_hash": row.source_hash,
                            "kind":        row.kind,
                            "variant":     row.variant,
                            "blob_hash":   row.blob_hash,
                            "note": "source Blob is gone, so purge_derived_blobs can never fire; \
                                     this row pins its artifact forever",
                        }),
                    )
                    .await;
                }

                if !row.artifact_exists {
                    finding_count += 1;
                    record_or_log(
                        store,
                        DERIVED_CONSISTENCY_JOB_NAME,
                        "derived_dangling_blob",
                        "data_loss",
                        None,
                        serde_json::json!({
                            "source_hash": row.source_hash,
                            "kind":        row.kind,
                            "variant":     row.variant,
                            "blob_hash":   row.blob_hash,
                            "note": "mapping promises an artifact with no Blob behind it; \
                                     a read finds the row and then fails",
                        }),
                    )
                    .await;
                }
            }

            let scanned = rows.len() as u64;
            cursor = rows
                .last()
                .map(|r| (r.source_hash.clone(), r.kind.clone(), r.variant.clone()));

            let checkpoint = rows.last().map(encode_cursor).unwrap_or_default();
            if let Err(e) = store.checkpoint(checkpoint, scanned).await {
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
            event = "derived_consistency.completed",
            run_id = %store.run_id(),
            finding_count = finding_count,
            "derived_consistency completed with {} finding(s)",
            finding_count
        );

        RunOutcome::completed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(source: &str, kind: &str, variant: &str) -> DerivedRow {
        DerivedRow {
            source_hash: source.into(),
            kind: kind.into(),
            variant: variant.into(),
            blob_hash: "b".into(),
            source_exists: true,
            artifact_exists: true,
        }
    }

    /// The cursor must survive the round trip, or a resumed run silently
    /// restarts or skips — the failure mode a paged audit job can least
    /// afford, since it would under-report rather than error.
    #[test]
    fn cursor_round_trips() {
        let r = row("0a1b", "thumbnail", "preview.webp");
        let decoded = decode_cursor(encode_cursor(&r)).unwrap();
        assert_eq!(
            decoded,
            Some((
                "0a1b".to_string(),
                "thumbnail".to_string(),
                "preview.webp".to_string()
            ))
        );
    }

    /// An empty cursor means "from the beginning", not a parse error — the
    /// scheduler hands one back for a fresh run.
    #[test]
    fn empty_cursor_starts_from_the_beginning() {
        assert_eq!(decode_cursor(Vec::new()).unwrap(), None);
    }

    /// A malformed cursor must fail loudly. Silently treating it as "start
    /// over" would turn a corrupt checkpoint into a job that never finishes
    /// and never says why.
    #[test]
    fn malformed_cursor_is_an_error() {
        assert!(decode_cursor(b"only-one-field".to_vec()).is_err());
    }
}
