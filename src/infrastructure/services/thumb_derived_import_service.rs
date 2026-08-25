//! `thumb_derived_import` — backfill `storage.content_derived_blobs` from the
//! on-disk thumbnail sidecars that predate it.
//!
//! Step 10 of `docs/plan/derived-blobs.md`. Every server-rendered thumbnail
//! written before `content_derived_blobs` existed lives only as
//! `{thumbnails_root}/{size}/{hash}.webp`. That is local-disk state: another
//! instance cannot see it, a backend migration does not carry it, and no
//! consistency job covers it. This job moves those bytes into the blob store
//! and records the mapping, after which the derived tier can become
//! authoritative and the sidecar can be deleted.
//!
//! **Thumbnails only, and that is permanent.** The table also holds
//! `kind = 'transcode'`, but transcoding lands *after* this migration, so
//! transcodes are born into the table and never pass through a sidecar era.
//! This job will not grow a transcode arm.
//!
//! ### Idempotent by construction
//!
//! Each file is skipped when a row already exists for its
//! `(source_hash, 'thumbnail', variant)`, and `store_derived_blob` is
//! `ON CONFLICT DO NOTHING` with a release-on-conflict underneath, so a
//! re-run cannot inflate refcounts. Re-running is the expected operator
//! behaviour — Phase 3 (deleting the sidecars) is gated on a run reporting
//! zero imported.
//!
//! ### Multi-instance caveat
//!
//! Sidecars are local. Running this on one instance migrates only that
//! instance's files, so Phase 3 must be gated on *every* instance reporting
//! an empty tail. The run history does not aggregate across instances; that
//! remains an operator responsibility.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::fs;

use crate::application::ports::thumbnail_ports::ThumbnailSize;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};
use crate::infrastructure::services::dedup_service::DedupService;

pub const THUMB_DERIVED_IMPORT_JOB_NAME: &str = "thumb_derived_import";

/// Files handled between checkpoints. Each one is a read plus (at most) a
/// blob write, so this is deliberately smaller than a pure-DB sweep's page.
const BATCH_SIZE: usize = 100;

pub struct ThumbDerivedImport {
    thumbnails_root: PathBuf,
    dedup: Arc<DedupService>,
}

impl ThumbDerivedImport {
    pub fn new(thumbnails_root: PathBuf, dedup: Arc<DedupService>) -> Self {
        Self {
            thumbnails_root,
            dedup,
        }
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

    /// The hash a sidecar filename names, or `None` when the file is not one.
    ///
    /// Strict, and deliberately rejects `ext-{file_id}.jpg`: those are
    /// user-supplied, file-keyed bytes. Importing them here would content-key
    /// them and share one user's uploaded preview onto every file with
    /// identical content — the poisoning `file_attached_blobs` exists to
    /// prevent. They belong to `thumb_attached_import`.
    fn hash_from_sidecar_name(name: &str) -> Option<&str> {
        let stem = name.strip_suffix(".webp")?;
        if stem.len() != 64 || !stem.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(stem)
    }

    /// Sorted sidecar filenames for one size directory.
    ///
    /// Sorted so the cursor is meaningful: resume skips everything at or
    /// before it, which only works over a stable order.
    ///
    /// Takes the root rather than reading `self`, so the walk — the half that
    /// decides which files this job claims, and therefore which keying they
    /// get — is testable against a temp directory with no database in sight.
    pub(crate) async fn sidecar_names(root: &std::path::Path, size: ThumbnailSize) -> Vec<String> {
        let dir = root.join(size.dir_name());
        let Ok(mut entries) = fs::read_dir(&dir).await else {
            return Vec::new();
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str()
                && Self::hash_from_sidecar_name(name).is_some()
            {
                names.push(name.to_string());
            }
        }
        names.sort();
        names
    }
}

#[async_trait]
impl RecoverableJobHandler for ThumbDerivedImport {
    fn name(&self) -> &str {
        THUMB_DERIVED_IMPORT_JOB_NAME
    }

    async fn count_total(&self) -> Option<u64> {
        let mut total = 0u64;
        for size in ThumbnailSize::all() {
            total += Self::sidecar_names(&self.thumbnails_root, *size)
                .await
                .len() as u64;
        }
        Some(total)
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        _args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // Cursor is `{size_dir}/{filename}` — the last file completed. Sizes
        // are walked in `ThumbnailSize::all()` order, and names are sorted
        // within each, so the pair totally orders the walk.
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

        let mut imported = 0u64;
        let mut already = 0u64;
        let mut failed = 0u64;
        let mut since_checkpoint = 0usize;
        let variant_of = |s: ThumbnailSize| s.dir_name().to_string();

        for size in ThumbnailSize::all() {
            let dir_name = variant_of(*size);
            for name in Self::sidecar_names(&self.thumbnails_root, *size).await {
                let position = format!("{dir_name}/{name}");

                // Resume: everything at or before the cursor is done.
                if let Some(c) = &cursor
                    && position.as_str() <= c.as_str()
                {
                    continue;
                }

                match store.status().await {
                    Ok(RunStatus::CancelRequested) => {
                        return RunOutcome::Paused {
                            cursor: position.into_bytes(),
                        };
                    }
                    Ok(_) => {}
                    Err(e) => {
                        return RunOutcome::Failed {
                            message: format!("status poll: {e}"),
                        };
                    }
                }

                let Some(hash) = Self::hash_from_sidecar_name(&name) else {
                    continue;
                };

                // Already mapped — the common case on a re-run, and the
                // reason this job is safe to trigger repeatedly.
                if self
                    .dedup
                    .find_derived_blob(hash, "thumbnail", &dir_name)
                    .await
                    .is_some()
                {
                    already += 1;
                } else {
                    let path = self.thumbnails_root.join(&dir_name).join(&name);
                    match fs::read(&path).await {
                        Ok(data) => {
                            match self
                                .dedup
                                .store_derived_blob(
                                    hash,
                                    "thumbnail",
                                    &dir_name,
                                    "image/webp",
                                    Bytes::from(data),
                                )
                                .await
                            {
                                Ok(_) => imported += 1,
                                Err(e) => {
                                    failed += 1;
                                    record_or_log(
                                        store,
                                        THUMB_DERIVED_IMPORT_JOB_NAME,
                                        "thumbnail_import_failed",
                                        "anomaly",
                                        None,
                                        serde_json::json!({
                                            "path":  position,
                                            "hash":  hash,
                                            "error": format!("{e}"),
                                            "note":  "sidecar left in place; safe to re-run",
                                        }),
                                    )
                                    .await;
                                }
                            }
                        }
                        Err(e) => {
                            // Unreadable, or removed between listing and read
                            // (a concurrent GC unlink). Neither is fatal.
                            failed += 1;
                            record_or_log(
                                store,
                                THUMB_DERIVED_IMPORT_JOB_NAME,
                                "thumbnail_unreadable",
                                "anomaly",
                                None,
                                serde_json::json!({
                                    "path":  position,
                                    "error": format!("{e}"),
                                }),
                            )
                            .await;
                        }
                    }
                }

                since_checkpoint += 1;
                if since_checkpoint >= BATCH_SIZE {
                    if let Err(e) = store
                        .checkpoint(position.clone().into_bytes(), since_checkpoint as u64)
                        .await
                    {
                        return RunOutcome::Failed {
                            message: format!("checkpoint: {e}"),
                        };
                    }
                    since_checkpoint = 0;
                }
            }
        }

        tracing::info!(
            target: "oxicloud::dedup",
            event = "thumb_derived_import.completed",
            run_id = %store.run_id(),
            imported = imported,
            already_present = already,
            failed = failed,
            "thumb_derived_import: {imported} imported, {already} already present, {failed} failed"
        );

        RunOutcome::completed()
    }
}

#[cfg(test)]
// `pub(crate)` so the attached import's test can reuse `legacy_tree`. Both
// jobs walk ONE directory, so the property worth asserting spans them — that
// together they claim every sidecar exactly once — and that needs a shared
// fixture rather than two that can drift apart.
pub(crate) mod tests {
    use super::*;

    const H: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";

    #[test]
    fn accepts_a_canonical_sidecar_name() {
        assert_eq!(
            ThumbDerivedImport::hash_from_sidecar_name(&format!("{H}.webp")),
            Some(H)
        );
    }

    /// A legacy `.thumbnails` tree as it exists before the migration: both
    /// sidecar shapes side by side in the same size directory, which is
    /// exactly how they are written today.
    ///
    /// Returns the temp dir — the caller must hold it, or the directory is
    /// removed while the test is still reading it.
    pub(crate) async fn legacy_tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("create temp dir");
        for size in ThumbnailSize::all() {
            let dir = tmp.path().join(size.dir_name());
            tokio::fs::create_dir_all(&dir).await.unwrap();
            // Server-rendered, content-keyed. `b` sorts after `0a…`, so the
            // pair also proves the listing is ordered rather than incidental.
            tokio::fs::write(dir.join(format!("{H}.webp")), b"webp")
                .await
                .unwrap();
            tokio::fs::write(
                dir.join("b111111111111111111111111111111111111111111111111111111111111111.webp"),
                b"webp2",
            )
            .await
            .unwrap();
            // User-uploaded, file-keyed.
            tokio::fs::write(
                dir.join("ext-3f2b1c00-1111-2222-3333-444455556666.jpg"),
                b"jpeg",
            )
            .await
            .unwrap();
            // Neither: a stray file that must be claimed by no one.
            tokio::fs::write(dir.join("README.txt"), b"nope")
                .await
                .unwrap();
        }
        tmp
    }

    /// The migration's core invariant: this job claims the content-keyed
    /// sidecars and *only* those, leaving the uploaded previews for
    /// `thumb_attached_import`. Getting this wrong content-keys user-supplied
    /// bytes, which shares one user's preview onto every file with identical
    /// content.
    #[tokio::test]
    async fn walk_claims_only_content_keyed_sidecars_in_sorted_order() {
        let tmp = legacy_tree().await;
        let names = ThumbDerivedImport::sidecar_names(tmp.path(), ThumbnailSize::Preview).await;

        assert_eq!(
            names,
            vec![
                format!("{H}.webp"),
                "b111111111111111111111111111111111111111111111111111111111111111.webp".to_string(),
            ],
            "must claim both content-keyed sidecars, sorted, and nothing else"
        );
    }

    /// A missing size directory is normal on a fresh install and must not
    /// abort the walk — the job simply has nothing to import.
    #[tokio::test]
    async fn missing_size_directory_yields_no_work() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        assert!(
            ThumbDerivedImport::sidecar_names(tmp.path(), ThumbnailSize::Icon)
                .await
                .is_empty()
        );
    }

    /// `ext-` files are user-supplied and file-keyed. Importing one here
    /// would content-key it and share it across every file with identical
    /// content — the exact poisoning the table split prevents.
    #[test]
    fn rejects_external_and_malformed_names() {
        for name in [
            format!("ext-{H}.jpg"),
            "ext-3f2b1c00-0000-0000-0000-000000000000.jpg".to_string(),
            format!("{H}.jpg"),
            format!("{}.webp", &H[..63]),
            H.to_string(),
            "junk.webp".to_string(),
        ] {
            assert_eq!(
                ThumbDerivedImport::hash_from_sidecar_name(&name),
                None,
                "must not be imported as a derived thumbnail: {name}"
            );
        }
    }
}
