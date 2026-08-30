//! Step 7 migration tenant: drains `.transcoded/` into the derived tier.
//!
//! The twin of `thumb_derived_import`, with one difference that shapes the
//! whole job: **the legacy tree is keyed by file, the destination by
//! content.** Thumbnail sidecars were already named by blob hash, so their
//! import was a move. These are named `{file_id}.webp`, so every entry has
//! to be re-keyed through `storage.files` before it can be stored.
//!
//! That re-keying is not bookkeeping — it is the point. A sandbox with five
//! `.skip` markers had three of them naming the same content, so the
//! file-keyed tree held three copies of one verdict. After the import that
//! is a single row, and any future upload of those bytes inherits it
//! instead of paying for the decision again.
//!
//! ### Two artifact kinds, one walk
//!
//! * `{file_id}.webp` — a cached transcode. Imported as a derived Blob.
//! * `{file_id}.webp.skip` — a zero-byte marker meaning "WebP came out
//!   larger for this file". Imported as a NEGATIVE row (NULL `blob_hash`),
//!   so the verdict survives the deletion of the directory holding it.
//!
//! Both are claimed by the same walk because they share a source file and
//! a cursor; splitting them would mean two passes over one directory and
//! two chances for the pair to disagree about what has been handled.
//!
//! ### What is deliberately not imported
//!
//! Entries whose file is gone. The destination row is keyed by content
//! hash, which is resolved *through* `storage.files` — no file, no hash,
//! nothing to key by. Under `repair` these are deleted, because they are
//! unimportable by definition and a run that keeps rediscovering them
//! never reports zero, so the gate for removing the directory never opens.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use sqlx::PgPool;
use tokio::fs;

use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, RecoverableJobHandler,
    RunOutcome, RunStatus, record_or_log,
};
use crate::infrastructure::services::dedup_service::DedupService;
use crate::infrastructure::services::thumb_derived_import_service::audit_sidecar_deleted;

pub const TRANSCODE_IMPORT_JOB_NAME: &str = "transcode_import";

/// `content_derived_blobs.kind` for everything this job writes. Must match
/// `ImageTranscodeService::DERIVED_KIND`, or the import would file its rows
/// where the read path does not look.
const DERIVED_KIND: &str = "transcode";

/// The only variant this job handles. `.transcoded/` has exactly one
/// subdirectory today; a second output format would add a directory and a
/// variant together, and this walk would grow a loop rather than a branch.
const VARIANT_WEBP: &str = "webp";

/// Files handled between checkpoints. Each is a read plus, at most, a blob
/// write — deliberately smaller than a pure-DB sweep's page.
const BATCH_SIZE: usize = 100;

pub struct TranscodeImport {
    /// `{storage_path}/.transcoded`, matching `ImageTranscodeService::new`.
    transcoded_root: PathBuf,
    dedup: Arc<DedupService>,
    /// Needed for the re-keying: file id → content hash. The thumbnail
    /// imports have no equivalent because their sidecars were already
    /// content-named.
    pool: Arc<PgPool>,
}

impl TranscodeImport {
    pub fn new(transcoded_root: PathBuf, dedup: Arc<DedupService>, pool: Arc<PgPool>) -> Self {
        Self {
            transcoded_root,
            dedup,
            pool,
        }
    }

    pub async fn register_recoverable_job(
        self: Arc<Self>,
        registry: &JobRegistry,
        provider: &Arc<dyn JobStoreProvider>,
    ) -> Arc<Self> {
        // Daily, matching the thumbnail imports: idempotent and resumable,
        // so periodic is safe, and a migration nobody remembers to trigger
        // never finishes. The tick imports but does not delete — `repair`
        // defaults false.
        registry
            .register_recoverable_job(
                self.clone(),
                provider.clone(),
                Some(std::time::Duration::from_secs(24 * 3600)),
            )
            .await;
        self
    }

    /// The `webp/` subdirectory, where both artifact kinds live.
    fn variant_dir(&self) -> PathBuf {
        self.transcoded_root.join(VARIANT_WEBP)
    }

    /// Sorted entry names, so the cursor totally orders the traversal.
    ///
    /// Returns both `{id}.webp` and `{id}.webp.skip`; the caller decides
    /// which is which. Anything else is ignored rather than reported — the
    /// directory is a local cache and has never promised to hold only our
    /// files.
    async fn entry_names(dir: &std::path::Path) -> Vec<String> {
        let Ok(mut entries) = fs::read_dir(dir).await else {
            return Vec::new();
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str()
                && parse_entry(name).is_some()
            {
                names.push(name.to_string());
            }
        }
        names.sort();
        names
    }

    /// Resolve a file id to the BLAKE3 of its content.
    ///
    /// `None` means the file is gone — which is the unimportable case, not
    /// an error: the destination is keyed by content, and a deleted file
    /// has no content to key by.
    async fn content_hash_of(&self, file_id: &str) -> Option<String> {
        sqlx::query_as::<_, (String,)>(
            "SELECT blob_hash FROM storage.files WHERE id = $1::uuid AND blob_hash IS NOT NULL",
        )
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .ok()
        .flatten()
        .map(|(h,)| h)
    }
}

/// Checkpoint once a batch has accumulated, resetting the counter.
///
/// Returns `Some(RunOutcome::Failed)` when the store write fails, for the
/// caller to return. Shared by both exits of the loop body so the cursor
/// advances identically whether an entry was imported or skipped — the
/// alternative is two copies of this, which is how the first draft ended
/// up dropping a future and silently never checkpointing on one path.
async fn checkpoint_if_due(
    store: &dyn JobStore,
    name: &str,
    since_checkpoint: &mut usize,
) -> Option<RunOutcome> {
    if *since_checkpoint < BATCH_SIZE {
        return None;
    }
    let scanned = *since_checkpoint as u64;
    *since_checkpoint = 0;
    match store.checkpoint(name.as_bytes().to_vec(), scanned).await {
        Ok(()) => None,
        Err(e) => Some(RunOutcome::Failed {
            message: format!("checkpoint: {e}"),
        }),
    }
}

/// What a `.transcoded/webp/` entry names.
///
/// Returns the file id and whether it is a negative marker. Anything not
/// matching either shape is not ours.
fn parse_entry(name: &str) -> Option<(&str, bool)> {
    if let Some(id) = name.strip_suffix(".webp.skip") {
        return (!id.is_empty()).then_some((id, true));
    }
    if let Some(id) = name.strip_suffix(".webp") {
        return (!id.is_empty()).then_some((id, false));
    }
    None
}

#[async_trait]
impl RecoverableJobHandler for TranscodeImport {
    fn name(&self) -> &str {
        TRANSCODE_IMPORT_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Migrates cached WebP transcodes out of the legacy .transcoded/ \
         directory into content-addressed blob storage, re-keying each one \
         from its file id to its content hash. Entries for identical \
         content collapse into a single row, so the same image cached under \
         several files stops being stored several times. Zero-byte .skip \
         markers become negative rows, preserving the verdict that a file \
         is not worth transcoding."
    }

    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    fn repair_description(&self) -> Option<&'static str> {
        Some(
            "Also DELETES each cached transcode once its replacement has \
             been read back and compared byte for byte, and removes the \
             directory when empty. Entries whose file no longer exists are \
             deleted without a readback — they cannot be re-keyed and \
             nothing can reference them again. Irreversible, though a \
             transcode is a pure function of its source: anything deleted \
             in error is recomputed on the next request.",
        )
    }

    async fn count_total(&self) -> Option<u64> {
        Some(Self::entry_names(&self.variant_dir()).await.len() as u64)
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // Cursor is the entry name. One directory, sorted, so the name
        // alone totally orders the walk — unlike the thumbnail imports,
        // which need `{size}/{name}` to span three directories.
        let cursor: Option<String> = match resume_cursor {
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

        let delete_imported = args.repair;
        let dir = self.variant_dir();

        let mut imported = 0u64;
        let mut negatives = 0u64;
        let mut already = 0u64;
        let mut file_gone = 0u64;
        let mut deleted = 0u64;
        let mut unverified = 0u64;
        let mut failed = 0u64;
        let mut since_checkpoint = 0usize;

        for name in Self::entry_names(&dir).await {
            if let Some(c) = &cursor
                && name.as_str() <= c.as_str()
            {
                continue;
            }

            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    return RunOutcome::Paused {
                        cursor: name.into_bytes(),
                    };
                }
                Ok(_) => {}
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("status poll: {e}"),
                    };
                }
            }

            let Some((file_id, is_negative)) = parse_entry(&name) else {
                continue;
            };
            let path = dir.join(&name);

            // Re-key. This is the step the thumbnail imports do not have.
            let Some(source_hash) = self.content_hash_of(file_id).await else {
                file_gone += 1;
                let mut removed = false;
                if delete_imported && fs::remove_file(&path).await.is_ok() {
                    deleted += 1;
                    removed = true;
                    audit_sidecar_deleted(
                        TRANSCODE_IMPORT_JOB_NAME,
                        "file_gone",
                        file_id,
                        "-",
                        &path,
                    );
                }
                record_or_log(
                    store,
                    TRANSCODE_IMPORT_JOB_NAME,
                    "transcode_file_gone",
                    "anomaly",
                    None,
                    serde_json::json!({
                        "path":    name,
                        "file_id": file_id,
                        "deleted": removed,
                        "note": "no storage.files row, so the entry cannot be re-keyed to a \
                                 content hash; unimportable and unreachable",
                    }),
                )
                .await;
                // Falls through to the shared checkpoint at the end of the
                // loop rather than duplicating it here. An earlier draft
                // did duplicate it and dropped the future without
                // awaiting — the entry counted toward the batch, the
                // cursor never advanced, and a resumed run would have
                // rewalked everything already handled.
                since_checkpoint += 1;
                if let Some(failure) = checkpoint_if_due(store, &name, &mut since_checkpoint).await
                {
                    return failure;
                }
                continue;
            };

            if is_negative {
                // A verdict, not bytes. `store_derived_negative` is
                // ON CONFLICT DO NOTHING, so the three markers that named
                // one piece of content in the sandbox collapse here rather
                // than fighting over the row.
                match self
                    .dedup
                    .store_derived_negative(&source_hash, DERIVED_KIND, VARIANT_WEBP)
                    .await
                {
                    Ok(()) => {
                        negatives += 1;
                        if delete_imported && fs::remove_file(&path).await.is_ok() {
                            deleted += 1;
                            audit_sidecar_deleted(
                                TRANSCODE_IMPORT_JOB_NAME,
                                "negative_imported",
                                file_id,
                                "-",
                                &path,
                            );
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        tracing::warn!(
                            target: "oxicloud::dedup",
                            event = "transcode_import.negative_failed",
                            file_id = %file_id,
                            source_hash = %source_hash,
                            error = %e,
                            "failed to record negative transcode row; entry kept"
                        );
                    }
                }
            } else if self
                .dedup
                .find_derived_blob(&source_hash, DERIVED_KIND, VARIANT_WEBP)
                .await
                .is_some()
            {
                // Already imported — by an earlier run, or by another file
                // sharing this content. Checked BEFORE storing so a re-run
                // does not release and retake the reference.
                already += 1;
                if delete_imported {
                    let existing = self
                        .dedup
                        .find_derived_blob(&source_hash, DERIVED_KIND, VARIANT_WEBP)
                        .await;
                    if let Some(r) = existing {
                        if crate::infrastructure::services::thumb_derived_import_service::ThumbDerivedImport::verify_and_unlink(
                            &self.dedup,
                            TRANSCODE_IMPORT_JOB_NAME,
                            &source_hash,
                            &r.blob_hash,
                            &path,
                        )
                        .await
                        {
                            deleted += 1;
                        } else {
                            unverified += 1;
                            record_or_log(
                                store,
                                TRANSCODE_IMPORT_JOB_NAME,
                                "transcode_delete_unverified",
                                "anomaly",
                                None,
                                serde_json::json!({
                                    "path":        name,
                                    "file_id":     file_id,
                                    "source_hash": source_hash,
                                    "note": "stored transcode did not read back identical; \
                                             cached copy kept",
                                }),
                            )
                            .await;
                        }
                    }
                }
            } else {
                match fs::read(&path).await {
                    Ok(data) => {
                        match self
                            .dedup
                            .store_derived_blob(
                                &source_hash,
                                DERIVED_KIND,
                                VARIANT_WEBP,
                                "image/webp",
                                Bytes::from(data),
                            )
                            .await
                        {
                            Ok(stored_hash) => {
                                imported += 1;
                                if delete_imported {
                                    if crate::infrastructure::services::thumb_derived_import_service::ThumbDerivedImport::verify_and_unlink(
                                        &self.dedup,
                                        TRANSCODE_IMPORT_JOB_NAME,
                                        &source_hash,
                                        &stored_hash,
                                        &path,
                                    )
                                    .await
                                    {
                                        deleted += 1;
                                    } else {
                                        unverified += 1;
                                    }
                                }
                            }
                            Err(e) => {
                                failed += 1;
                                tracing::warn!(
                                    target: "oxicloud::dedup",
                                    event = "transcode_import.store_failed",
                                    file_id = %file_id,
                                    source_hash = %source_hash,
                                    error = %e,
                                    "failed to store derived transcode; entry kept"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        tracing::warn!(
                            target: "oxicloud::dedup",
                            event = "transcode_import.read_failed",
                            path = %path.display(),
                            error = %e,
                            "failed to read cached transcode; entry kept"
                        );
                    }
                }
            }

            since_checkpoint += 1;
            if let Some(failure) = checkpoint_if_due(store, &name, &mut since_checkpoint).await {
                return failure;
            }
        }

        // Remove the tree once drained. Deletion first, rename only if a
        // non-cache file is in the way — same rule as `.thumbnails/`, and
        // for the same reason: absence is what the read path tests, and a
        // stray `.DS_Store` must not keep the fallback alive forever.
        if delete_imported {
            let _ = fs::remove_dir(&dir).await;
            match fs::remove_dir(&self.transcoded_root).await {
                Ok(()) => tracing::info!(
                    target: "oxicloud::dedup",
                    event = "transcode_import.root_removed",
                    run_id = %store.run_id(),
                    path = %self.transcoded_root.display(),
                    "🧹 legacy transcode directory removed"
                ),
                Err(_) => {
                    let parked = self.transcoded_root.with_file_name(".transcoded.migrated");
                    match fs::rename(&self.transcoded_root, &parked).await {
                        Ok(()) => tracing::info!(
                            target: "oxicloud::dedup",
                            event = "transcode_import.root_parked",
                            run_id = %store.run_id(),
                            to = %parked.display(),
                            "🧹 legacy transcode directory could not be removed (a non-cache \
                             file remains) — moved aside instead"
                        ),
                        Err(e) => tracing::warn!(
                            target: "oxicloud::dedup",
                            event = "transcode_import.root_kept",
                            run_id = %store.run_id(),
                            reason = %e,
                            "legacy transcode directory neither removed nor moved aside"
                        ),
                    }
                }
            }
        }

        tracing::info!(
            target: "oxicloud::dedup",
            event = "transcode_import.completed",
            run_id = %store.run_id(),
            imported = imported,
            negatives = negatives,
            already_present = already,
            file_gone = file_gone,
            deleted = deleted,
            unverified = unverified,
            failed = failed,
            "transcode_import: {imported} imported, {negatives} negative verdict(s), \
             {already} already present, {file_gone} unimportable, {deleted} deleted, \
             {unverified} kept unverified, {failed} failed"
        );

        RunOutcome::completed_with(serde_json::json!({
            "imported":        imported,
            "negatives":       negatives,
            "already_present": already,
            "file_gone":       file_gone,
            "deleted":         deleted,
            "unverified":      unverified,
            "failed":          failed,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The walk claims both artifact kinds and nothing else.
    ///
    /// `.webp.skip` must be tested BEFORE `.webp`, or the shorter suffix
    /// matches first and every marker imports as if it were a cached
    /// transcode — reading a zero-byte file and storing it as the
    /// transcode of its source, which would then be served to clients.
    #[test]
    fn entry_names_are_parsed_by_longest_suffix_first() {
        let id = "3f2b1c00-1111-2222-3333-444455556666";

        assert_eq!(parse_entry(&format!("{id}.webp")), Some((id, false)));
        assert_eq!(parse_entry(&format!("{id}.webp.skip")), Some((id, true)));

        // Not ours: no id, wrong extension, or a bare marker.
        assert_eq!(parse_entry(".webp"), None);
        assert_eq!(parse_entry(".webp.skip"), None);
        assert_eq!(parse_entry(&format!("{id}.jpg")), None);
        assert_eq!(parse_entry(".DS_Store"), None);
    }
}
