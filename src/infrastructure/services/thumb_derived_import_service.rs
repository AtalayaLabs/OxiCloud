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
//! **Thumbnails only.** The table also holds `kind = 'transcode'`, and those
//! need their own import — `ImageTranscodeService` already exists and caches
//! to `.transcoded/{ext}/{file_id}.{ext}`, a different tree with a different
//! key. Importing them means **re-keying** file→content, which is legitimate
//! only because a transcode is derivable from the source bytes. Separate job;
//! this one will not grow a transcode arm.
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

use crate::application::ports::thumbnail_ports::{ThumbnailFormat, ThumbnailSize};
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, RecoverableJobHandler,
    RunOutcome, RunStatus, record_or_log,
};
use crate::infrastructure::services::dedup_service::DedupService;

pub const THUMB_DERIVED_IMPORT_JOB_NAME: &str = "thumb_derived_import";

/// Record a sidecar deletion on the audit channel.
///
/// Both import jobs delete user-visible files during a one-way migration, so
/// the trail has to survive the run history: findings are per-run and get
/// purged, whereas `target: "audit"` is separable and retained. If a preview
/// later turns out to be missing, this is the only record that says the
/// migration removed it, when, and on whose behalf.
///
/// `owner` is the id the file belonged to — a `source_hash` for content-keyed
/// sidecars, a `file_id` for uploaded ones. That is the field an
/// investigation starts from, and the raw `NEW BLOB` logs cannot supply it:
/// they name the hash of the stored bytes, which is a different value from
/// the sidecar's own name.
///
/// `reason` is a stable machine-readable key, per the convention: `imported`
/// (replaced by a verified blob), `source_gone`, `orphaned`.
pub(crate) fn audit_sidecar_deleted(
    job: &str,
    reason: &str,
    owner: &str,
    blob_hash: &str,
    path: &std::path::Path,
) {
    tracing::info!(
        target: "audit",
        event = "thumbnail.sidecar_deleted",
        reason = reason,
        job = job,
        owner = owner,
        blob_hash = blob_hash,
        path = %path.display(),
        "👮🏻‍♂️ migration deleted a thumbnail sidecar ({reason})",
    );
}

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
        // Daily tick rather than manual-only. Ops cannot be relied on to
        // remember a migration, and boot-time would delay readiness for a
        // filesystem walk — whereas this is idempotent and resumable, so
        // periodic is safe and it drains on its own.
        //
        // The tick does NOT delete: `repair` defaults false, so scheduled
        // runs import and stop. Deletion stays a deliberate operator action,
        // per no-silent-auto-repair. Once drained, a run is a `read_dir` over
        // three directories that returns nothing — and after the directory is
        // removed, not even that.
        registry
            .register_recoverable_job(
                self.clone(),
                provider.clone(),
                Some(std::time::Duration::from_secs(24 * 3600)),
            )
            .await;
        self
    }

    /// The hash and format a sidecar filename names, or `None` when the file
    /// is not one of ours.
    ///
    /// Strict, and deliberately rejects `ext-{file_id}.jpg`: those are
    /// user-supplied, file-keyed bytes. Importing them here would content-key
    /// them and share one user's uploaded preview onto every file with
    /// identical content — the poisoning `file_attached_blobs` exists to
    /// prevent. They belong to `thumb_attached_import`. That rejection
    /// carries the weight now that `.jpg` is otherwise claimed, since the two
    /// jobs would otherwise both want it.
    ///
    /// Returns the format too, because the row
    /// key needs both since migration `20261022000000`.
    ///
    /// Both codecs are claimed. `persist_rendered` writes
    /// `{hash}.{format.ext()}`, so any client that does not advertise WebP
    /// leaves `{hash}.jpg` on disk. While the derived tier was WebP-only
    /// those were unmigratable by design; now that `variant` carries the
    /// format they are ordinary content, and skipping them would leave
    /// `.thumbnails/` permanently non-empty — which is the signal step 10e
    /// gates the fallback removal on.
    fn hash_from_sidecar_name(name: &str) -> Option<(&str, ThumbnailFormat)> {
        let (stem, format) = ThumbnailFormat::ALL
            .iter()
            .find_map(|f| name.strip_suffix(&format!(".{}", f.ext())).map(|s| (s, *f)))?;
        if stem.len() != 64 || !stem.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some((stem, format))
    }

    /// Delete a sidecar, but only after proving the blob that replaced it can
    /// actually be read back.
    ///
    /// The verification is the whole point. `store_derived_blob` reporting
    /// success is not proof the bytes are retrievable — a backend that
    /// accepted a write it cannot serve would otherwise have the last copy
    /// deleted on top of it. This is a migration, and the difference between
    /// a migration and a data-loss bug is exactly this read.
    ///
    /// Length is compared rather than full bytes: it catches the realistic
    /// failures (absent, empty, truncated) without a second full read of the
    /// sidecar, which the already-imported path would otherwise need.
    ///
    /// Returns whether the file was removed. A failed verification leaves the
    /// sidecar in place — the run reports it and the next one retries, which
    /// is the safe direction.
    /// Shared with `thumb_attached_import` rather than copied into it: both
    /// jobs delete a sidecar only after proving its replacement is readable,
    /// and two copies of that rule would be two chances to weaken one.
    pub(crate) async fn verify_and_unlink(
        dedup: &DedupService,
        job: &str,
        owner: &str,
        stored_hash: &str,
        path: &std::path::Path,
    ) -> bool {
        let Ok(meta) = fs::metadata(path).await else {
            return false;
        };
        let Ok(stored) = dedup.read_blob_bytes(stored_hash).await else {
            return false;
        };
        if stored.is_empty() || stored.len() as u64 != meta.len() {
            return false;
        }
        if fs::remove_file(path).await.is_err() {
            return false;
        }
        audit_sidecar_deleted(job, "imported", owner, stored_hash, path);
        true
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

    fn description(&self) -> &'static str {
        "Migrates server-rendered thumbnails from the legacy .thumbnails/ \
         directory into content-addressed blob storage. Local-disk sidecars \
         are invisible to other instances and are not carried by a backend \
         migration; importing them is what lets that directory be deleted."
    }

    /// `Always`: a plain run inserts rows and writes blobs. Repair-capable on
    /// top of that, which is why the two are independent.
    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    fn repair_description(&self) -> Option<&'static str> {
        Some(
            "Also DELETES each sidecar once its replacement has been read \
             back from blob storage, and removes the directory when empty. \
             Files whose source no longer exists are deleted without a \
             readback — they cannot be imported and nothing can reference \
             them. Irreversible.",
        )
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
        args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // `?repair=true` opts into deleting each sidecar once it has been
        // imported AND read back. Off by default, matching the house rule
        // that a job does not mutate on its default setting — early runs
        // import only, so an operator can inspect before committing.
        //
        // Deleting from the job rather than from a later release is what
        // makes the migration self-draining: sidecars are LOCAL disk, so no
        // release can know whether every instance has finished, whereas each
        // instance draining itself needs no coordination at all.
        let delete_imported = args.repair;
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
        let mut deleted = 0u64;
        let mut unverified = 0u64;
        let mut dead_source = 0u64;
        let mut since_checkpoint = 0usize;
        // The DIRECTORY is `{size}` on disk; the VARIANT is `{size}.{ext}`
        // since migration `20261022000000`. Conflating them is a real trap:
        // using the variant as a path yields `.thumbnails/preview.webp/…`,
        // which does not exist, so every file reads as unreadable and nothing
        // imports. The variant is therefore built per FILE, from the format
        // its extension names, not once per size.
        for size in ThumbnailSize::all() {
            let dir_name = size.dir_name(); // on-disk directory
            for name in Self::sidecar_names(&self.thumbnails_root, *size).await {
                // Cursor position uses the DIRECTORY, so a run paused before
                // this change resumes at the same place.
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

                let Some((hash, format)) = Self::hash_from_sidecar_name(&name) else {
                    continue;
                };
                // Both derived from the file's OWN extension, so a `.jpg`
                // sidecar becomes a JPEG row rather than being mislabelled
                // WebP — which would serve the wrong codec to anyone the read
                // path then matched it for.
                let variant = format!("{dir_name}.{}", format.ext());
                let content_type = format.mime();

                // Already mapped — the common case on a re-run, and the
                // reason this job is safe to trigger repeatedly.
                //
                // Deletion applies here too, not just to fresh imports: a run
                // without `repair` leaves the sidecar behind, and a later run
                // with it would otherwise classify the file as "already
                // imported" and never drain it. Import-then-enable-deletion
                // is the expected operator sequence, so this is the common
                // path, not an edge case.
                if let Some(existing) = self
                    .dedup
                    .find_derived_blob(hash, "thumbnail", &variant)
                    .await
                {
                    already += 1;
                    if delete_imported {
                        let path = self.thumbnails_root.join(dir_name).join(&name);
                        if Self::verify_and_unlink(
                            &self.dedup,
                            THUMB_DERIVED_IMPORT_JOB_NAME,
                            hash,
                            &existing.blob_hash,
                            &path,
                        )
                        .await
                        {
                            deleted += 1;
                        } else {
                            unverified += 1;
                            record_or_log(
                                store,
                                THUMB_DERIVED_IMPORT_JOB_NAME,
                                "sidecar_delete_unverified",
                                "anomaly",
                                None,
                                serde_json::json!({
                                    "path": position,
                                    "hash": hash,
                                    "note": "derived blob did not read back; sidecar kept",
                                }),
                            )
                            .await;
                        }
                    }
                } else if !self.dedup.blob_exists(hash).await {
                    // The source is gone, so this sidecar cannot be imported:
                    // a mapping to a dead source is precisely the orphan row
                    // `store_derived_blob` now refuses, because nothing would
                    // ever reap that hash again and the row would pin its
                    // artifact forever.
                    //
                    // Checked BEFORE the read and the blob write, not after.
                    // Without this the refusal still happens, but only once
                    // the bytes have been stored — so every run writes a blob
                    // and immediately deletes its manifest again, per dead
                    // sidecar, forever. On a real install where `.thumbnails/`
                    // has outlived years of deleted files, that is most of
                    // them.
                    //
                    // It also matters for the tail: these files are
                    // unimportable by definition, so a run that keeps
                    // rediscovering them never reports zero and step 10e's
                    // gate never opens. Under `repair` they are deleted —
                    // safe, and the only unlink here that needs no readback,
                    // since there is nothing to read back and nothing to
                    // regenerate from.
                    dead_source += 1;
                    let mut removed = false;
                    if delete_imported {
                        let path = self.thumbnails_root.join(dir_name).join(&name);
                        if fs::remove_file(&path).await.is_ok() {
                            deleted += 1;
                            removed = true;
                            // Audited explicitly: this unlink bypasses
                            // verify_and_unlink, which has nothing to verify
                            // against here.
                            audit_sidecar_deleted(
                                THUMB_DERIVED_IMPORT_JOB_NAME,
                                "source_gone",
                                hash,
                                "-",
                                &path,
                            );
                        }
                    }
                    // Recorded in BOTH modes. The finding used to be the
                    // `else` of the deletion, so a repair run unlinked files
                    // and reported a clean sweep — the audit stream held the
                    // only trace, and the run drawer an operator actually
                    // looks at said zero. A deletion is the outcome most
                    // worth a finding, not least.
                    record_or_log(
                        store,
                        THUMB_DERIVED_IMPORT_JOB_NAME,
                        "sidecar_source_gone",
                        // `anomaly` in both modes — it is what the panel
                        // renders as "notices", and `detail.deleted` carries
                        // whether the run left the sidecar alone or removed
                        // it. A separate severity for the deleted case would
                        // render identically and split one badge across two
                        // values.
                        "anomaly",
                        None,
                        serde_json::json!({
                            "path":        position,
                            "source_hash": hash,
                            "deleted":     removed,
                            "note": if removed {
                                "source Blob no longer exists; sidecar was unimportable and \
                                 has been deleted"
                            } else {
                                "source Blob no longer exists; the thumbnail is unimportable \
                                 and is deleted on a repair run"
                            },
                        }),
                    )
                    .await;
                } else {
                    let path = self.thumbnails_root.join(dir_name).join(&name);
                    match fs::read(&path).await {
                        Ok(data) => {
                            match self
                                .dedup
                                .store_derived_blob(
                                    hash,
                                    "thumbnail",
                                    &variant,
                                    content_type,
                                    Bytes::from(data),
                                )
                                .await
                            {
                                Ok(derived_hash) => {
                                    imported += 1;
                                    if delete_imported {
                                        if Self::verify_and_unlink(
                                            &self.dedup,
                                            THUMB_DERIVED_IMPORT_JOB_NAME,
                                            hash,
                                            &derived_hash,
                                            &path,
                                        )
                                        .await
                                        {
                                            deleted += 1;
                                        } else {
                                            unverified += 1;
                                            record_or_log(
                                                store,
                                                THUMB_DERIVED_IMPORT_JOB_NAME,
                                                "sidecar_delete_unverified",
                                                "anomaly",
                                                None,
                                                serde_json::json!({
                                                    "path": position,
                                                    "hash": hash,
                                                    "note": "derived blob did not read back; sidecar kept",
                                                }),
                                            )
                                            .await;
                                        }
                                    }
                                }
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

        // Remove the size directories once genuinely empty, because ABSENCE
        // is what step 10e gates the fallback removal on — not emptiness.
        // Empty is momentary: an on-demand render can repopulate it the next
        // second. Absence is one-way, and far cheaper to test besides — one
        // `stat` versus an opendir/readdir/closedir.
        //
        // `remove_dir` refuses a non-empty directory, so this needs no
        // emptiness check of its own and cannot race a concurrent write into
        // deleting live files.
        if delete_imported {
            for size in ThumbnailSize::all() {
                let dir = self.thumbnails_root.join(size.dir_name());
                let _ = fs::remove_dir(&dir).await;
            }
            let _ = fs::remove_dir(&self.thumbnails_root).await;
        }

        tracing::info!(
            target: "oxicloud::dedup",
            event = "thumb_derived_import.completed",
            run_id = %store.run_id(),
            imported = imported,
            already_present = already,
            failed = failed,
            deleted = deleted,
            unverified = unverified,
            dead_source = dead_source,
            "thumb_derived_import: {imported} imported, {already} already present, \
             {failed} failed, {deleted} sidecar(s) deleted, {unverified} kept unverified, \
             {dead_source} skipped (source gone)"
        );

        // Surfaced on the run row, not just in the process log. A repair run
        // that unlinks hundreds of files while reporting only a finding
        // total tells an operator nothing about what it did with them.
        RunOutcome::completed_with(serde_json::json!({
            "imported":        imported,
            "already_present": already,
            "deleted":         deleted,
            "unverified":      unverified,
            "dead_source":     dead_source,
            "failed":          failed,
        }))
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
    /// A second hash, for the JPEG sidecar in `legacy_tree`.
    const H2: &str = "c222222222222222222222222222222222222222222222222222222222222222";

    /// BOTH codecs are claimed, and the format comes from the extension.
    ///
    /// `.jpg` was previously rejected here, which was correct only while the
    /// derived tier was WebP-only. Once `variant` carried the format
    /// (migration `20261022000000`) a JPEG sidecar became ordinary content,
    /// and leaving it unclaimed would keep `.thumbnails/` permanently
    /// non-empty — the very signal step 10e gates on.
    #[test]
    fn accepts_both_codecs_and_reports_the_format() {
        assert_eq!(
            ThumbDerivedImport::hash_from_sidecar_name(&format!("{H}.webp")),
            Some((H, ThumbnailFormat::Webp))
        );
        assert_eq!(
            ThumbDerivedImport::hash_from_sidecar_name(&format!("{H}.jpg")),
            Some((H, ThumbnailFormat::Jpeg)),
            "a JPEG sidecar must import, and as JPEG — labelling it WebP \
             would serve the wrong codec"
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
            // Server-rendered JPEG: what a client not advertising WebP
            // leaves behind. Claimed by the derived import, and must not be
            // confused with the `ext-` upload above despite sharing an
            // extension.
            tokio::fs::write(dir.join(format!("{H2}.jpg")), b"jpeg")
                .await
                .unwrap();
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
                format!("{H2}.jpg"),
            ],
            "must claim every content-keyed sidecar of EITHER codec, sorted, \
             and nothing else"
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
            // `ext-` prefixed: user-supplied and file-keyed, whatever the
            // extension. Now that .jpg is otherwise claimed, this is the case
            // that keeps the two jobs disjoint.
            format!("ext-{H}.jpg"),
            "ext-3f2b1c00-0000-0000-0000-000000000000.jpg".to_string(),
            format!("{}.webp", &H[..63]),
            H.to_string(),
            "junk.webp".to_string(),
            "junk.jpg".to_string(),
        ] {
            assert_eq!(
                ThumbDerivedImport::hash_from_sidecar_name(&name),
                None,
                "must not be imported as a derived thumbnail: {name}"
            );
        }
    }
}
