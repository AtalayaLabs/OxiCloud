//! `thumb_attached_import` — backfill `storage.file_attached_blobs` from the
//! `ext-{file_id}.jpg` sidecars that predate it.
//!
//! Second half of step 10's migration, and the twin of
//! `thumb_derived_import`. These are the thumbnails a *user* supplied — the
//! SPA's client-side generator, notably for PDFs, which have no server-side
//! render path at all. They live only as
//! `{thumbnails_root}/{size}/ext-{file_id}.jpg` on local disk.
//!
//! Until a row exists, a **copy of the file loses the preview**: the sidecar
//! is keyed by `file_id`, no copy path duplicates it, and the server silently
//! falls back to rendering from the source (or to nothing, for a PDF). That
//! is the bug `file_attached_blobs` closed for new uploads; this job closes
//! it for everything already on disk.
//!
//! ### File-keyed, and that is the whole point
//!
//! These bytes are **not** derivable from the file's content, so they must
//! never be content-keyed. Sharing one user's uploaded preview across every
//! file with identical content is the poisoning vector the table split
//! exists to prevent — see `docs/plan/derived-blobs.md`. `thumb_derived_import`
//! deliberately rejects `ext-` names for the same reason, and the two jobs
//! are separate so neither can drift into the other's keying.
//!
//! ### Idempotence needs care here
//!
//! Unlike the derived twin, `store_attached_blob` is `ON CONFLICT DO UPDATE`:
//! calling it for a row that already exists releases the previous reference
//! and takes a new one. Harmless once, but a job that did it on every run
//! would churn refcounts. So each file is skipped when a row is already
//! present, and the store is only reached on a genuine insert.
//!
//! ### Multi-instance caveat
//!
//! Sidecars are local, so this migrates only the instance it runs on. Phase 3
//! must be gated on every instance reporting an empty tail.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use sqlx::PgPool;
use tokio::fs;
use uuid::Uuid;

use crate::application::ports::thumbnail_ports::ThumbnailSize;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};
use crate::infrastructure::services::dedup_service::DedupService;
// The readback-then-unlink rule is shared, not copied: two versions of it
// would be two chances to weaken one, and this is the check standing between
// a migration and permanent loss.
use crate::infrastructure::services::thumb_derived_import_service::ThumbDerivedImport;

pub const THUMB_ATTACHED_IMPORT_JOB_NAME: &str = "thumb_attached_import";

/// Files handled between checkpoints — a read plus at most a blob write each.
const BATCH_SIZE: usize = 100;

/// `uploaded_by` for imported rows.
///
/// Disk records no uploader, and the column is deliberately `NOT NULL` with no
/// FK so provenance survives a user deletion. A sentinel says "imported, real
/// uploader unknown" honestly; inventing an owner — the file's `created_by`,
/// say — would fabricate provenance that could later be read as evidence an
/// Editor replaced someone's preview.
const IMPORTED_UPLOADER: Uuid = Uuid::nil();

pub struct ThumbAttachedImport {
    thumbnails_root: PathBuf,
    dedup: Arc<DedupService>,
    pool: Arc<PgPool>,
}

impl ThumbAttachedImport {
    pub fn new(thumbnails_root: PathBuf, dedup: Arc<DedupService>, pool: Arc<PgPool>) -> Self {
        Self {
            thumbnails_root,
            dedup,
            pool,
        }
    }

    pub async fn register_recoverable_job(
        self: Arc<Self>,
        registry: &JobRegistry,
        provider: &Arc<dyn JobStoreProvider>,
    ) -> Arc<Self> {
        // Daily, matching `thumb_derived_import` — and it does not delete on
        // the tick either, since `repair` defaults false. See that job for
        // the reasoning.
        registry
            .register_recoverable_job(
                self.clone(),
                provider.clone(),
                Some(std::time::Duration::from_secs(24 * 3600)),
            )
            .await;
        self
    }

    /// The file id an external sidecar names, or `None` when the file is not
    /// one of ours.
    ///
    /// Requires a parseable UUID: the name is about to be used as a foreign
    /// key, and a malformed one should be reported rather than fed to the
    /// database.
    fn file_id_from_sidecar_name(name: &str) -> Option<Uuid> {
        let stem = name.strip_prefix("ext-")?.strip_suffix(".jpg")?;
        Uuid::parse_str(stem).ok()
    }

    /// Sorted external-sidecar filenames for one size directory.
    ///
    /// Sorted because the cursor resumes by skipping everything at or before
    /// it, which only works over a stable order.
    ///
    /// Takes the root rather than reading `self`, so the walk — the half that
    /// decides which files this job claims, and therefore which keying they
    /// get — is testable against a temp directory with no database in sight.
    async fn sidecar_names(root: &std::path::Path, size: ThumbnailSize) -> Vec<String> {
        let dir = root.join(size.dir_name());
        let Ok(mut entries) = fs::read_dir(&dir).await else {
            return Vec::new();
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str()
                && Self::file_id_from_sidecar_name(name).is_some()
            {
                names.push(name.to_string());
            }
        }
        names.sort();
        names
    }

    /// Does the file still exist? Checked explicitly rather than letting the
    /// foreign key reject the insert, so an orphaned sidecar is *counted* as
    /// an orphan instead of surfacing as an opaque constraint error.
    /// `SELECT EXISTS(...)`, deliberately, rather than `SELECT 1 … LIMIT 1`.
    ///
    /// PostgreSQL types a bare `1` as `int4`, so decoding it as `i64` fails —
    /// and because a decode error is indistinguishable from "no row" once
    /// swallowed, every sidecar would be misreported as an orphan and nothing
    /// would import. `EXISTS` yields a real `bool` and always returns exactly
    /// one row, so absence means absence.
    ///
    /// A query error still degrades to `false`, which is the safe direction:
    /// the file is reported as an orphan and left on disk for the operator,
    /// rather than imported against a row that may not exist.
    async fn file_exists(&self, file_id: Uuid) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM storage.files WHERE id = $1)")
            .bind(file_id)
            .fetch_one(self.pool.as_ref())
            .await
            .unwrap_or(false)
    }
}

#[async_trait]
impl RecoverableJobHandler for ThumbAttachedImport {
    fn name(&self) -> &str {
        THUMB_ATTACHED_IMPORT_JOB_NAME
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
        // Cursor is `{size_dir}/{filename}`, matching thumb_derived_import:
        // sizes walk in `ThumbnailSize::all()` order and names are sorted
        // within each, so the pair totally orders the traversal.
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
        let mut orphaned = 0u64;
        let mut deleted = 0u64;
        let mut unverified = 0u64;
        // Same opt-in as thumb_derived_import: `?repair=true`.
        //
        // The readback before unlinking matters more here than there. These
        // sidecars are the ones that CANNOT be regenerated — a client-uploaded
        // PDF preview has no server-side render path — so it is not
        // belt-and-braces, it is the only thing between a migration and
        // permanent loss.
        let delete_imported = args.repair;
        let mut failed = 0u64;
        let mut since_checkpoint = 0usize;

        for size in ThumbnailSize::all() {
            let dir_name = size.dir_name().to_string();
            for name in Self::sidecar_names(&self.thumbnails_root, *size).await {
                let position = format!("{dir_name}/{name}");

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

                let Some(file_id) = Self::file_id_from_sidecar_name(&name) else {
                    continue;
                };
                let file_id_str = file_id.to_string();

                // Already mapped. Checked BEFORE storing, because
                // `store_attached_blob` is ON CONFLICT DO UPDATE and would
                // release then retake the reference on every run.
                if let Some(existing) = self
                    .dedup
                    .find_attached_blob(&file_id_str, "preview", &dir_name)
                    .await
                {
                    already += 1;
                    // Drains on a later run too: importing first and enabling
                    // deletion afterwards is the expected operator sequence,
                    // so reaching here is the common path rather than an edge
                    // case.
                    if delete_imported {
                        let path = self.thumbnails_root.join(&dir_name).join(&name);
                        if ThumbDerivedImport::verify_and_unlink(
                            &self.dedup,
                            THUMB_ATTACHED_IMPORT_JOB_NAME,
                            &file_id_str,
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
                                THUMB_ATTACHED_IMPORT_JOB_NAME,
                                "sidecar_delete_unverified",
                                "anomaly",
                                None,
                                serde_json::json!({
                                    "path":    position,
                                    "file_id": file_id_str,
                                    "note":    "attached blob did not read back; sidecar kept",
                                }),
                            )
                            .await;
                        }
                    }
                } else if !self.file_exists(file_id).await {
                    // The file is gone, so this sidecar is unimportable: the
                    // FK on `file_id` would reject the row. Mirrors the
                    // dead-source case in thumb_derived_import.
                    //
                    // Reported by default — a destructive default on a
                    // migration is what no-silent-auto-repair forbids — and
                    // deleted under `repair`, because otherwise it is
                    // rediscovered on every run, the tail never empties, and
                    // step 10e's gate never opens.
                    //
                    // Safe to delete despite these being the non-regenerable
                    // bytes: the preview is keyed to a `file_id` that no
                    // longer exists, so nothing can ever reference it again.
                    // Unrecoverable and unreachable are different things, and
                    // this is both.
                    //
                    // No readback before unlinking, unlike the imported path:
                    // there is no row and no blob to read back, and nothing to
                    // regenerate from either.
                    orphaned += 1;
                    if delete_imported {
                        let path = self.thumbnails_root.join(&dir_name).join(&name);
                        if fs::remove_file(&path).await.is_ok() {
                            deleted += 1;
                            // Explicit: nothing to verify against, so this
                            // bypasses verify_and_unlink. Worth auditing
                            // loudest of all — these bytes were
                            // user-supplied and cannot be regenerated, even
                            // though the file that owned them is gone.
                            crate::infrastructure::services::thumb_derived_import_service::audit_sidecar_deleted(
                                THUMB_ATTACHED_IMPORT_JOB_NAME,
                                "orphaned",
                                &file_id_str,
                                "-",
                                &path,
                            );
                        }
                    } else {
                        record_or_log(
                            store,
                            THUMB_ATTACHED_IMPORT_JOB_NAME,
                            "attached_sidecar_orphan",
                            "anomaly",
                            None,
                            serde_json::json!({
                                "path":    position,
                                "file_id": file_id_str,
                                "note":    "no storage.files row; unimportable, and deleted on a \
                                            repair run since nothing can reference it again",
                            }),
                        )
                        .await;
                    }
                } else {
                    let path = self.thumbnails_root.join(&dir_name).join(&name);
                    match fs::read(&path).await {
                        Ok(data) => {
                            match self
                                .dedup
                                .store_attached_blob(
                                    &file_id_str,
                                    "preview",
                                    &dir_name,
                                    // store_external_thumbnail re-encodes to
                                    // JPEG before writing, so the extension
                                    // is authoritative here.
                                    "image/jpeg",
                                    Bytes::from(data),
                                    IMPORTED_UPLOADER,
                                )
                                .await
                            {
                                Ok(attached_hash) => {
                                    imported += 1;
                                    if delete_imported {
                                        if ThumbDerivedImport::verify_and_unlink(
                                            &self.dedup,
                                            THUMB_ATTACHED_IMPORT_JOB_NAME,
                                            &file_id_str,
                                            &attached_hash,
                                            &path,
                                        )
                                        .await
                                        {
                                            deleted += 1;
                                        } else {
                                            unverified += 1;
                                            record_or_log(
                                                store,
                                                THUMB_ATTACHED_IMPORT_JOB_NAME,
                                                "sidecar_delete_unverified",
                                                "anomaly",
                                                None,
                                                serde_json::json!({
                                                    "path":    position,
                                                    "file_id": file_id_str,
                                                    "note":    "attached blob did not read back; sidecar kept",
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
                                        THUMB_ATTACHED_IMPORT_JOB_NAME,
                                        "attached_import_failed",
                                        "anomaly",
                                        None,
                                        serde_json::json!({
                                            "path":    position,
                                            "file_id": file_id_str,
                                            "error":   format!("{e}"),
                                            "note":    "sidecar left in place; safe to re-run",
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
                                THUMB_ATTACHED_IMPORT_JOB_NAME,
                                "attached_sidecar_unreadable",
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
            event = "thumb_attached_import.completed",
            run_id = %store.run_id(),
            imported = imported,
            already_present = already,
            orphaned = orphaned,
            failed = failed,
            deleted = deleted,
            unverified = unverified,
            "thumb_attached_import: {imported} imported, {already} already present, \
             {orphaned} orphaned, {failed} failed, {deleted} sidecar(s) deleted, \
             {unverified} kept unverified"
        );

        RunOutcome::completed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "3f2b1c00-1111-2222-3333-444455556666";
    const HASH: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";

    #[test]
    fn accepts_an_external_sidecar_name() {
        assert_eq!(
            ThumbAttachedImport::file_id_from_sidecar_name(&format!("ext-{UUID}.jpg")),
            Some(Uuid::parse_str(UUID).unwrap())
        );
    }

    /// The other half of the partition. Reuses the same legacy tree as
    /// `thumb_derived_import`'s test on purpose: the two jobs run over one
    /// directory, so the property that matters is that together they claim
    /// every real sidecar exactly once, and neither takes the other's.
    #[tokio::test]
    async fn walk_claims_only_uploaded_previews() {
        use crate::infrastructure::services::thumb_derived_import_service::ThumbDerivedImport;

        let tmp =
            crate::infrastructure::services::thumb_derived_import_service::tests::legacy_tree()
                .await;

        let attached = ThumbAttachedImport::sidecar_names(tmp.path(), ThumbnailSize::Preview).await;
        let derived = ThumbDerivedImport::sidecar_names(tmp.path(), ThumbnailSize::Preview).await;

        assert_eq!(
            attached,
            vec!["ext-3f2b1c00-1111-2222-3333-444455556666.jpg".to_string()],
            "must claim the uploaded preview and nothing else"
        );

        // Disjoint: no file is imported under both keyings, which would take
        // two references and — worse — content-key user-supplied bytes.
        for a in &attached {
            assert!(
                !derived.contains(a),
                "both jobs claimed {a}; keying would be ambiguous"
            );
        }
        // And nothing real is dropped: README.txt is the only unclaimed file.
        // Two content-keyed .webp, one content-keyed .jpg, one ext- upload.
        // The .jpg pair is the interesting one: same extension, opposite
        // keying, and only the `ext-` prefix separates them.
        assert_eq!(
            attached.len() + derived.len(),
            4,
            "every real sidecar must be claimed exactly once between the two jobs"
        );
    }

    /// The content-keyed sidecars belong to `thumb_derived_import`. Importing
    /// one here would file-key bytes that are shared across every file with
    /// the same content, so each such file would take its own reference to
    /// content it does not own.
    #[test]
    fn rejects_content_keyed_and_malformed_names() {
        for name in [
            format!("{HASH}.webp"),
            format!("{HASH}.jpg"),
            format!("ext-{UUID}.webp"),
            format!("ext-{UUID}"),
            "ext-not-a-uuid.jpg".to_string(),
            format!("{UUID}.jpg"),
        ] {
            assert_eq!(
                ThumbAttachedImport::file_id_from_sidecar_name(&name),
                None,
                "must not be imported as an attached preview: {name}"
            );
        }
    }

    /// The sentinel must be stable: rows carrying it are how an operator
    /// tells an imported preview from one with real provenance.
    #[test]
    fn imported_uploader_is_the_nil_sentinel() {
        assert_eq!(
            IMPORTED_UPLOADER.to_string(),
            "00000000-0000-0000-0000-000000000000"
        );
    }
}
