//! PostgreSQL + Blob-backed file write repository.
//!
//! Implements `FileWritePort` using:
//! - `storage.files` table for metadata
//! - `DedupPort` for content-addressable blob storage on the filesystem
//!
//! File paths are resolved by querying the materialized `storage.folders.path`
//! column (O(1) per lookup), so no recursive CTEs are needed.

use moka::sync::Cache;
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::display_helpers::category_order_for;
use crate::application::ports::storage_ports::{CopyFolderTreeResult, FileWritePort};
use crate::common::errors::DomainError;
use crate::domain::entities::file::File;

use super::transaction_utils::retry_on_deadlock;
use crate::infrastructure::services::dedup_service::DedupService;

/// File write repository backed by PostgreSQL metadata + blob storage.
pub struct FileBlobWriteRepository {
    pool: Arc<PgPool>,
    dedup: Arc<DedupService>,
    /// Shared handle to `FileBlobReadRepository`'s file_id → blob_hash
    /// cache. Content swaps and hard deletes invalidate the mapping here
    /// so the read side can never serve a stale blob after a PUT update.
    hash_cache: Cache<String, String>,
}

impl FileBlobWriteRepository {
    pub fn new(
        pool: Arc<PgPool>,
        dedup: Arc<DedupService>,
        hash_cache: Cache<String, String>,
    ) -> Self {
        Self {
            pool,
            dedup,
            hash_cache,
        }
    }

    /// Creates a stub instance for testing — never hits PG.
    #[cfg(test)]
    pub fn new_stub() -> Self {
        use crate::infrastructure::services::dedup_service::DedupService;
        Self {
            pool: Arc::new(
                sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                    .max_connections(1)
                    .connect_lazy("postgres://invalid:5432/none")
                    .unwrap(),
            ),
            dedup: Arc::new(DedupService::new_stub()),
            hash_cache: Cache::builder().max_capacity(10_000).build(),
        }
    }

    /// Look up the materialized folder path. O(1) — no recursive CTE.
    async fn lookup_folder_path(
        &self,
        folder_id: Option<&str>,
    ) -> Result<Option<String>, DomainError> {
        match folder_id {
            Some(fid) => {
                let path: String =
                    sqlx::query_scalar("SELECT path FROM storage.folders WHERE id = $1::uuid")
                        .bind(fid)
                        .fetch_optional(self.pool.as_ref())
                        .await
                        .map_err(|e| {
                            DomainError::internal_error(
                                "FileBlobWrite",
                                format!("folder path: {e}"),
                            )
                        })?
                        .ok_or_else(|| DomainError::not_found("Folder", fid))?;
                Ok(Some(path))
            }
            None => Ok(None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn row_to_file(
        id: String,
        name: String,
        folder_id: Option<String>,
        folder_path: Option<String>,
        size: i64,
        mime_type: String,
        created_at: i64,
        modified_at: i64,
        blob_hash: String,
        created_by: Option<Uuid>,
        updated_by: Option<Uuid>,
    ) -> Result<File, DomainError> {
        File::from_materialized_row(
            id,
            name,
            folder_path.as_deref(),
            size as u64,
            mime_type,
            folder_id,
            created_at as u64,
            modified_at as u64,
            blob_hash,
            created_by,
            updated_by,
        )
        .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("entity: {e}")))
    }

    /// Derive `drive_id` from the parent folder. Post-D7: only the
    /// drive is needed — the legacy `user_id` column is no longer
    /// written on new rows.
    async fn resolve_parent_drive(&self, folder_id: Option<&str>) -> Result<Uuid, DomainError> {
        match folder_id {
            Some(fid) => sqlx::query_scalar::<_, Uuid>(
                "SELECT drive_id FROM storage.folders WHERE id = $1::uuid",
            )
            .bind(fid)
            .fetch_optional(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error("FileBlobWrite", format!("parent lookup: {e}"))
            })?
            .ok_or_else(|| DomainError::not_found("Folder", fid)),
            // Same reasoning as the owner lookup below: caller error, not
            // server error.
            None => Err(DomainError::validation_error(
                "folder_id is required: the destination folder determines the \
                 target drive",
            )),
        }
    }

    /// Atomically swap the blob hash of a file.
    ///
    /// Uses a CTE to capture the old hash before updating so the old blob
    /// reference can be decremented afterwards. Compensates on failure by
    /// removing the new blob reference.
    ///
    /// `modified_at`: if `Some`, sets `updated_at` to that Unix timestamp;
    /// if `None`, uses `NOW()` (server time). Returns
    /// `(new_hash, updated_at_epoch)` on success — the effective timestamp
    /// is returned so callers can rebuild the fresh entity without
    /// re-reading the row.
    ///
    /// §14: `updated_by = $5` (caller_id). The caller mutated this
    /// row — not the row's owner. D2 shared drives let non-owners
    /// overwrite content; the previous `updated_by = f.user_id` would
    /// have silently recorded the wrong principal.
    /// `expected_hash`, when `Some`, turns this into a real
    /// compare-and-swap: the SET clause only takes effect if the row's
    /// `blob_hash` still matches at the moment the `FOR UPDATE` lock is
    /// held (same statement, same transaction — no gap a concurrent
    /// writer can land in). A mismatch leaves the row untouched and is
    /// reported back via the `matched` flag rather than silently
    /// overwriting a sibling PATCH's content. `None` preserves the old
    /// blind-overwrite behaviour for PUT/WOPI/chunked-upload finalize,
    /// where last-write-wins is the intended HTTP semantics.
    async fn swap_blob_hash(
        &self,
        file_id: &str,
        new_hash: &str,
        new_size: i64,
        modified_at: Option<i64>,
        caller_id: Uuid,
        expected_hash: Option<&str>,
    ) -> Result<(String, i64), DomainError> {
        // Atomic CTE: capture old hash then conditionally update in one
        // round-trip, no TOCTOU. The CASE arms make the SET a no-op when
        // `expected_hash` is given and doesn't match `old.blob_hash` —
        // the row is still returned (with its unchanged values) so the
        // caller can tell "mismatch" apart from "file not found".
        // Deadlock victims (40P01) retry before the compensation below runs —
        // a successful retry must keep the new blob reference alive.
        let (old_hash, updated_at, matched) =
            match retry_on_deadlock("files.swap_blob_hash", || {
                sqlx::query_as::<_, (String, i64, bool)>(
                    r#"
                WITH old AS (
                    SELECT id, blob_hash FROM storage.files WHERE id = $3::uuid FOR UPDATE
                )
                UPDATE storage.files f
                   SET blob_hash = CASE WHEN $6::text IS NULL OR old.blob_hash = $6
                                        THEN $1 ELSE f.blob_hash END,
                       size = CASE WHEN $6::text IS NULL OR old.blob_hash = $6
                                   THEN $2 ELSE f.size END,
                       updated_at = CASE WHEN $6::text IS NULL OR old.blob_hash = $6
                                         THEN COALESCE(to_timestamp($4), NOW()) ELSE f.updated_at END,
                       updated_by = CASE WHEN $6::text IS NULL OR old.blob_hash = $6
                                         THEN $5 ELSE f.updated_by END
                  FROM old
                 WHERE f.id = old.id
                RETURNING old.blob_hash, EXTRACT(EPOCH FROM f.updated_at)::bigint,
                          ($6::text IS NULL OR old.blob_hash = $6)
                "#,
                )
                .bind(new_hash)
                .bind(new_size)
                .bind(file_id)
                .bind(modified_at.map(|t| t as f64))
                .bind(caller_id)
                .bind(expected_hash)
                .fetch_optional(self.pool.as_ref())
            })
            .await
            {
                Ok(Some(row)) => row,
                Ok(None) => {
                    // File not found — compensate: remove the new blob ref
                    if let Err(e) = self.dedup.remove_reference(new_hash).await {
                        tracing::error!("Blob orphaned after missing file: {}", e);
                    }
                    return Err(DomainError::not_found("File", file_id));
                }
                Err(e) => {
                    // UPDATE failed — compensate: remove the new blob ref
                    if let Err(rollback_err) = self.dedup.remove_reference(new_hash).await {
                        tracing::error!(
                            "Blob orphaned after failed UPDATE — hash: {}, err: {}",
                            &new_hash[..12],
                            rollback_err
                        );
                    }
                    return Err(DomainError::internal_error(
                        "FileBlobWrite",
                        format!("update: {e}"),
                    ));
                }
            };

        if !matched {
            // CAS lost the race — some other writer's content is now the
            // row's truth. Release the blob we ingested for nothing;
            // nothing was written.
            if let Err(e) = self.dedup.remove_reference(new_hash).await {
                tracing::error!("Blob orphaned after CAS mismatch: {}", e);
            }
            return Err(DomainError::precondition_failed(
                "File",
                "content was modified concurrently",
            ));
        }

        // Decrement old blob ref (only if hash changed, best-effort)
        if old_hash != new_hash
            && let Err(e) = self.dedup.remove_reference(&old_hash).await
        {
            tracing::warn!(
                "Failed to decrement old blob ref {}: {}",
                &old_hash[..12],
                e
            );
        }

        Ok((new_hash.to_string(), updated_at))
    }

    /// Register a file row pointing at a blob already stored in the chunk
    /// store (the upload-ingest layer streamed the content in). Consumes the
    /// caller's blob reference: any failure releases it before returning.
    ///
    /// §14: `created_by = $7 = updated_by = caller_id` — authorship
    /// belongs to the principal performing the upload, not to the parent
    /// folder's owner. In D2 shared drives a non-owner member can upload
    /// into a folder Alice owns; binding `parent.user_id` would have
    /// silently recorded Alice as the author.
    async fn save_file_with_blob_impl(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob_hash: &str,
        size: u64,
        caller_id: Uuid,
    ) -> Result<File, DomainError> {
        // Root files have no parent folder to derive an owner from — keep the
        // previous resolve_user_id(None) contract (release the ref, error out).
        let Some(fid) = folder_id.as_deref() else {
            if let Err(rollback_err) = self.dedup.remove_reference(blob_hash).await {
                tracing::error!(
                    "Blob orphaned after missing folder_id — hash: {}, err: {}",
                    &blob_hash[..12],
                    rollback_err
                );
            }
            // A missing required field is the caller's error, not the
            // server's. As `internal_error` this surfaced as 500 /
            // `error_type: Internal Error`, which the SPA cannot tell apart
            // from the server breaking — so a malformed upload looked like an
            // outage. The OpenAPI body description called the field optional,
            // which is how it came to be omitted in the first place.
            return Err(DomainError::validation_error(
                "folder_id is required: the destination folder determines the \
                 file's owner and drive",
            ));
        };

        // ONE round-trip: derive owner + materialized path from the parent
        // folder and insert in a single statement. (Was resolve_user_id +
        // INSERT + lookup_folder_path = 3 trips, two of them re-reading the
        // same folders row.) An empty `parent` CTE — the folder vanished
        // between ingest and insert — inserts zero rows, which surfaces as a
        // clean NotFound instead of a generic owner-resolution error.
        //
        // Deadlock victims (40P01) retry before the compensation below runs —
        // a successful retry must keep the blob reference alive. The final
        // attempt's error falls through untouched so the 23505 mapping holds
        // (a retried INSERT can legitimately lose to a concurrent identical
        // upload).
        // Post-D7: `user_id` omitted from the INSERT column list and the
        // parent CTE. `drive_id` alone is the inherit-from-parent axis;
        // provenance is `created_by` / `updated_by` (§14).
        let result = retry_on_deadlock("files.insert", || {
            sqlx::query_as::<_, (String, String, i64, i64, Option<Uuid>, Option<Uuid>)>(
                r#"
                WITH parent AS (
                    SELECT id, drive_id, path FROM storage.folders WHERE id = $2::uuid
                )
                INSERT INTO storage.files
                    (name, folder_id, drive_id, blob_hash, size,
                     mime_type, category_order, created_by, updated_by)
                SELECT $1, parent.id, parent.drive_id, $3, $4,
                       $5, $6, $7, $7
                  FROM parent
                RETURNING id::text,
                          (SELECT path FROM parent),
                          EXTRACT(EPOCH FROM created_at)::bigint,
                          EXTRACT(EPOCH FROM updated_at)::bigint,
                          created_by,
                          updated_by
                "#,
            )
            .bind(&name)
            .bind(fid)
            .bind(blob_hash)
            .bind(size as i64)
            .bind(&content_type)
            .bind(category_order_for(&name, &content_type))
            .bind(caller_id)
            .fetch_optional(self.pool.as_ref())
        })
        .await;

        let (id, folder_path, created_at, updated_at, created_by, updated_by) = match result {
            Ok(Some(row)) => row,
            Ok(None) => {
                if let Err(rollback_err) = self.dedup.remove_reference(blob_hash).await {
                    tracing::error!(
                        "Blob orphaned after missing parent folder — hash: {}, err: {}",
                        &blob_hash[..12],
                        rollback_err
                    );
                }
                return Err(DomainError::not_found("Folder", fid));
            }
            Err(e) => {
                if let Err(rollback_err) = self.dedup.remove_reference(blob_hash).await {
                    tracing::error!(
                        "Blob orphaned after failed INSERT — hash: {}, err: {}",
                        &blob_hash[..12],
                        rollback_err
                    );
                }
                if let sqlx::Error::Database(ref db_err) = e
                    && db_err.code().as_deref() == Some("23505")
                {
                    // Idempotent re-upload: if the conflicting file already
                    // holds IDENTICAL content (same folder, same name, same
                    // blob hash), treat this as success and return that file
                    // instead of erroring. Re-uploading a partially-uploaded
                    // folder then becomes a clean no-op for everything that
                    // already landed — only the genuinely missing files
                    // transfer — instead of surfacing hundreds of spurious
                    // "already exists" failures. The duplicate blob reference
                    // taken during ingest was just released above, so the
                    // existing file's own reference is the only one (correct);
                    // a different-content clash still returns the conflict.
                    match self.fetch_identical_file(fid, &name, blob_hash).await {
                        Ok(Some(existing)) => {
                            tracing::info!(
                                "♻️ IDEMPOTENT UPLOAD: {} already present, identical content (hash: {})",
                                name,
                                &blob_hash[..12]
                            );
                            return Ok(existing);
                        }
                        Ok(None) => {} // genuine conflict (different content)
                        Err(lookup_err) => {
                            tracing::warn!(
                                "idempotency lookup failed for {} (hash {}): {} — returning conflict",
                                name,
                                &blob_hash[..12],
                                lookup_err
                            );
                        }
                    }
                    return Err(DomainError::already_exists(
                        "File",
                        format!("'{name}' already exists in this folder"),
                    ));
                }
                return Err(DomainError::internal_error(
                    "FileBlobWrite",
                    format!("insert: {e}"),
                ));
            }
        };

        tracing::info!(
            "📡 STREAMING WRITE: {} ({} bytes, hash: {})",
            name,
            size,
            &blob_hash[..12]
        );

        Self::row_to_file(
            id,
            name,
            folder_id,
            Some(folder_path),
            size as i64,
            content_type,
            created_at,
            updated_at,
            blob_hash.to_string(),
            created_by,
            updated_by,
        )
    }

    /// Fetch a non-trashed file in `folder_id` named `name` whose content blob
    /// is `blob_hash` — the "is this re-upload byte-identical?" probe that makes
    /// uploads idempotent on a name conflict. `Ok(None)` means the conflicting
    /// file has *different* content (a genuine clash the caller must report).
    async fn fetch_identical_file(
        &self,
        folder_id: &str,
        name: &str,
        blob_hash: &str,
    ) -> Result<Option<File>, DomainError> {
        // Post-D7: `f.user_id` is nullable on new rows; use
        // `Option<Uuid>` to accept NULL.
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                i64,
                i64,
                Option<Uuid>,
                Option<Uuid>,
                i64,
                String,
            ),
        >(
            r#"
            SELECT f.id::text, fo.path,
                   EXTRACT(EPOCH FROM f.created_at)::bigint,
                   EXTRACT(EPOCH FROM f.updated_at)::bigint,
                   f.created_by, f.updated_by, f.size, f.mime_type
              FROM storage.files f
              JOIN storage.folders fo ON fo.id = f.folder_id
             WHERE f.folder_id = $1::uuid
               AND f.name = $2
               AND f.blob_hash = $3
               AND NOT f.is_trashed
             LIMIT 1
            "#,
        )
        .bind(folder_id)
        .bind(name)
        .bind(blob_hash)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("FileBlobWrite", format!("idempotency lookup: {e}"))
        })?;

        let Some((
            id,
            folder_path,
            created_at,
            updated_at,
            created_by,
            updated_by,
            size,
            mime_type,
        )) = row
        else {
            return Ok(None);
        };

        Self::row_to_file(
            id,
            name.to_string(),
            Some(folder_id.to_string()),
            Some(folder_path),
            size,
            mime_type,
            created_at,
            updated_at,
            blob_hash.to_string(),
            created_by,
            updated_by,
        )
        .map(Some)
    }
}

impl FileWritePort for FileBlobWriteRepository {
    async fn save_file_with_blob(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob_hash: &str,
        size: u64,
        caller_id: Uuid,
    ) -> Result<File, DomainError> {
        self.save_file_with_blob_impl(name, folder_id, content_type, blob_hash, size, caller_id)
            .await
    }

    async fn move_file(
        &self,
        file_id: &str,
        target_folder_id: Option<String>,
        caller_id: Uuid,
    ) -> Result<File, DomainError> {
        // If moving to a different folder, get the new user_id (must be same user).
        //
        // §14: `updated_by = $3` (caller_id) — the caller mutated this
        // row. The previous COALESCE derived authorship from the
        // destination folder's owner, which is wrong: dest's user_id
        // has no claim to authorship of the file's content. D2 shared
        // drives surface this most starkly (Alice moves Bob's file
        // into Charlie's drive — `updated_by` must be Alice).
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                Option<String>,
                i64,
                String,
                i64,
                i64,
                Option<Uuid>,
                Option<Uuid>,
            ),
        >(
            r#"
            WITH dest AS (
                SELECT drive_id FROM storage.folders WHERE id = $1::uuid
            )
            UPDATE storage.files f
               SET folder_id = $1::uuid,
                   drive_id  = COALESCE((SELECT drive_id FROM dest), f.drive_id),
                   updated_at = NOW(),
                   updated_by = $3
             WHERE f.id = $2::uuid AND NOT f.is_trashed
            RETURNING f.id::text, f.name, f.folder_id::text, f.size, f.mime_type,
                      EXTRACT(EPOCH FROM f.created_at)::bigint,
                      EXTRACT(EPOCH FROM f.updated_at)::bigint,
                      f.created_by, f.updated_by
            "#,
        )
        .bind(&target_folder_id)
        .bind(file_id)
        .bind(caller_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("move: {e}")))?
        .ok_or_else(|| DomainError::not_found("File", file_id))?;

        let folder_path = self.lookup_folder_path(row.2.as_deref()).await?;
        Self::row_to_file(
            row.0,
            row.1,
            row.2,
            folder_path,
            row.3,
            row.4,
            row.5,
            row.6,
            String::new(),
            row.7,
            row.8,
        )
    }

    async fn copy_file(
        &self,
        file_id: &str,
        target_folder_id: Option<String>,
        new_name: Option<&str>,
        caller_id: Uuid,
    ) -> Result<File, DomainError> {
        // Two statements in one transaction: insert the new row (same
        // blob_hash — blob content is never copied, dedup makes this
        // zero-copy), then run the shared satellite fan-out.
        //
        // `storage.copy_file_satellites` is the single home for everything
        // that follows a file on copy — dead properties and the
        // manifest-aware blob reference — shared with
        // `storage.copy_folder_tree`. Two sites implementing that
        // separately is what let the tree path ship a version that missed
        // manifests entirely (migration `20261019000000`).
        //
        // It cannot be a CTE arm: data-modifying CTEs all observe the same
        // snapshot, so a function called alongside the INSERT would not see
        // the new `storage.files` row it needs to read `blob_hash` from,
        // and the dead-property INSERT would fail its foreign key. Hence a
        // real transaction — which also fixes the reference being
        // best-effort before: a failed `add_reference` used to log a
        // warning and leave a copy holding no reference at all, the exact
        // shape that gets its content reaped.
        //
        // §14: `created_by = $4 = updated_by = caller_id` — the caller
        // authored this copy. The previous binding used
        // `dest_folder.user_id` which silently recorded the destination
        // folder's owner as the author when Adam copied a file into
        // Alice's folder.
        let target_fid = target_folder_id.clone();
        let rename_to = new_name.map(|s| s.to_string());

        let row = retry_on_deadlock("files.copy", || async {
            let mut tx = self.pool.begin().await?;

            let row = sqlx::query_as::<
                _,
                (
                    String,
                    String,
                    Option<String>,
                    i64,
                    String,
                    i64,
                    i64,
                    String,
                    Option<Uuid>,
                    Option<Uuid>,
                ),
            >(
                r#"
                WITH src AS (
                    SELECT name, folder_id, blob_hash, size, mime_type, category_order
                      FROM storage.files
                     WHERE id = $1::uuid AND NOT is_trashed
                ),
                -- The destination folder may differ from the source's
                -- folder (when $2 is set); derive drive_id from the
                -- DESTINATION so cross-drive copies land in the right
                -- drive. Post-D7: `user_id` no longer projected — the
                -- column is not written on new rows.
                dest_folder AS (
                    SELECT id, drive_id
                      FROM storage.folders
                     WHERE id = COALESCE($2::uuid,
                                         (SELECT folder_id FROM src))
                ),
                new_file AS (
                    INSERT INTO storage.files
                        (name, folder_id, drive_id, blob_hash, size,
                         mime_type, category_order, created_by, updated_by)
                    SELECT COALESCE($3::text, src.name),
                           dest_folder.id,
                           dest_folder.drive_id,
                           src.blob_hash,
                           src.size,
                           src.mime_type,
                           src.category_order,
                           $4,
                           $4
                      FROM src, dest_folder
                    RETURNING id,
                              id::text AS id_text,
                              name, folder_id::text, size, mime_type,
                              EXTRACT(EPOCH FROM created_at)::bigint AS created_at,
                              EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at,
                              blob_hash,
                              created_by,
                              updated_by
                )
                SELECT id_text, name, folder_id, size, mime_type,
                       created_at, updated_at,
                       blob_hash, created_by, updated_by
                  FROM new_file
                "#,
            )
            .bind(file_id)
            .bind(&target_fid)
            .bind(&rename_to)
            .bind(caller_id)
            .fetch_optional(&mut *tx)
            .await?;

            if let Some(ref new_row) = row {
                // `new_row.0` is the new file's id as text; PG casts it.
                sqlx::query(
                    "SELECT storage.copy_file_satellites(ARRAY[$1::uuid], ARRAY[$2::uuid])",
                )
                .bind(file_id)
                .bind(&new_row.0)
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;
            Ok(row)
        })
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db_err) = e
                && db_err.code().as_deref() == Some("23505")
            {
                return DomainError::already_exists(
                    "File",
                    "a file with this name already exists in the target folder",
                );
            }
            DomainError::internal_error("FileBlobWrite", format!("copy: {e}"))
        })?
        .ok_or_else(|| DomainError::not_found("File", file_id))?;

        let blob_hash = &row.7;

        // No `add_reference` here: `copy_file_satellites` took it inside the
        // transaction above, so a copy that exists always holds a reference.

        tracing::info!(
            "📋 BLOB COPY: {} (hash: {}, zero-copy via dedup)",
            row.1,
            &blob_hash[..12]
        );

        let folder_path = self.lookup_folder_path(row.2.as_deref()).await?;
        Self::row_to_file(
            row.0,
            row.1,
            row.2,
            folder_path,
            row.3,
            row.4,
            row.5,
            row.6,
            row.7,
            row.8,
            row.9,
        )
    }

    async fn rename_file(
        &self,
        file_id: &str,
        new_name: &str,
        caller_id: Uuid,
    ) -> Result<File, DomainError> {
        // §14: `updated_by = $3` (caller_id), see move_file.
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                Option<String>,
                i64,
                String,
                i64,
                i64,
                Option<Uuid>,
                Option<Uuid>,
            ),
        >(
            r#"
            UPDATE storage.files
               SET name = $1, updated_at = NOW(), updated_by = $3
             WHERE id = $2::uuid AND NOT is_trashed
            RETURNING id::text, name, folder_id::text, size, mime_type,
                      EXTRACT(EPOCH FROM created_at)::bigint,
                      EXTRACT(EPOCH FROM updated_at)::bigint,
                      created_by, updated_by
            "#,
        )
        .bind(new_name)
        .bind(file_id)
        .bind(caller_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db_err) = e
                && db_err.code().as_deref() == Some("23505")
            {
                return DomainError::already_exists("File", format!("'{new_name}' already exists"));
            }
            DomainError::internal_error("FileBlobWrite", format!("rename: {e}"))
        })?
        .ok_or_else(|| DomainError::not_found("File", file_id))?;

        let folder_path = self.lookup_folder_path(row.2.as_deref()).await?;
        Self::row_to_file(
            row.0,
            row.1,
            row.2,
            folder_path,
            row.3,
            row.4,
            row.5,
            row.6,
            String::new(),
            row.7,
            row.8,
        )
    }

    async fn delete_file(&self, id: &str) -> Result<(), DomainError> {
        // The PG trigger `trg_files_decrement_blob_ref` automatically
        // decrements storage.blobs.ref_count for the deleted row's blob_hash.
        // Disk cleanup of orphaned blobs (ref_count = 0) is handled by
        // garbage_collect().
        let result = sqlx::query("DELETE FROM storage.files WHERE id = $1::uuid")
            .bind(id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("delete: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(DomainError::not_found("File", id));
        }

        // Drop the read-side file_id → blob_hash mapping for the dead row.
        self.hash_cache.invalidate(id);
        Ok(())
    }

    async fn update_file_content_with_blob(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: u64,
        modified_at: Option<i64>,
        caller_id: Uuid,
        expected_hash: Option<&str>,
    ) -> Result<(String, i64), DomainError> {
        // The content was already ingested into the chunk store by the
        // upload-ingest layer; swap_blob_hash consumes its reference and
        // releases it on failure.
        let swapped = self
            .swap_blob_hash(
                file_id,
                blob_hash,
                size as i64,
                modified_at,
                caller_id,
                expected_hash,
            )
            .await?;
        // The file now maps to a different blob — drop the read-side cache
        // entry so streaming downloads cannot serve the previous content
        // for the rest of its TTI window.
        self.hash_cache.invalidate(file_id);
        Ok(swapped)
    }

    async fn register_file_deferred(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        size: u64,
        caller_id: Uuid,
    ) -> Result<(File, PathBuf), DomainError> {
        // For deferred registration we use a placeholder hash.
        // The write-behind cache will call update_file_content later.
        let placeholder_hash = "0000000000000000000000000000000000000000000000000000000000000000";

        // Post-D7: `user_id` omitted from the INSERT column list.
        // §14: `created_by = <caller> = updated_by`.
        //
        // With a parent folder this is the SAME single-round-trip `WITH
        // parent AS (…) INSERT … RETURNING` template `persist_file` uses:
        // the old shape ran three queries per uploaded file — parent drive
        // SELECT, INSERT, parent path SELECT — with the first and third
        // re-reading the identical folders row (benches/ROUND11.md
        // §Q1: 3 → 1 round-trips on the default REST upload path).
        let (row, folder_path) = if let Some(fid) = folder_id.as_deref() {
            let row = retry_on_deadlock("files.insert_deferred", || {
                sqlx::query_as::<_, (String, String, i64, i64, Option<Uuid>, Option<Uuid>)>(
                    r#"
                    WITH parent AS (
                        SELECT id, drive_id, path FROM storage.folders WHERE id = $2::uuid
                    )
                    INSERT INTO storage.files
                        (name, folder_id, drive_id, blob_hash, size,
                         mime_type, category_order, created_by, updated_by)
                    SELECT $1, parent.id, parent.drive_id, $3, $4, $5, $6, $7, $7
                      FROM parent
                    RETURNING id::text,
                              (SELECT path FROM parent),
                              EXTRACT(EPOCH FROM created_at)::bigint,
                              EXTRACT(EPOCH FROM updated_at)::bigint,
                              created_by,
                              updated_by
                    "#,
                )
                .bind(&name)
                .bind(fid)
                .bind(placeholder_hash)
                .bind(size as i64)
                .bind(&content_type)
                .bind(category_order_for(&name, &content_type))
                .bind(caller_id)
                .fetch_optional(self.pool.as_ref())
            })
            .await
            .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("deferred: {e}")))?
            // 0 rows ⇒ the parent folder doesn't exist — same not-found the
            // old `resolve_parent_drive` first query produced.
            .ok_or_else(|| DomainError::not_found("Folder", fid))?;
            ((row.0, row.2, row.3, row.4, row.5), Some(row.1))
        } else {
            let drive_id = self.resolve_parent_drive(None).await?;
            let row = retry_on_deadlock("files.insert_deferred", || {
                sqlx::query_as::<_, (String, i64, i64, Option<Uuid>, Option<Uuid>)>(
                    r#"
                    INSERT INTO storage.files
                        (name, folder_id, drive_id, blob_hash, size,
                         mime_type, category_order, created_by, updated_by)
                    VALUES ($1, NULL, $2, $3, $4, $5, $6, $7, $7)
                    RETURNING id::text,
                              EXTRACT(EPOCH FROM created_at)::bigint,
                              EXTRACT(EPOCH FROM updated_at)::bigint,
                              created_by,
                              updated_by
                    "#,
                )
                .bind(&name)
                .bind(drive_id)
                .bind(placeholder_hash)
                .bind(size as i64)
                .bind(&content_type)
                .bind(category_order_for(&name, &content_type))
                .bind(caller_id)
                .fetch_one(self.pool.as_ref())
            })
            .await
            .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("deferred: {e}")))?;
            (row, None)
        };

        let file = Self::row_to_file(
            row.0.clone(),
            name,
            folder_id,
            folder_path,
            size as i64,
            content_type,
            row.1,
            row.2,
            String::new(),
            row.3,
            row.4,
        )?;

        // The target_path is not meaningful for blob storage (content goes to .blobs/)
        // but the WriteBehindCache API requires it. We return a synthetic path.
        let target_path = PathBuf::from(format!(".pending/{}", row.0));

        Ok((file, target_path))
    }

    // ── Trash operations ──

    async fn move_to_trash(&self, file_id: &str, caller_id: Uuid) -> Result<(), DomainError> {
        // §14: `updated_by = $2` (caller_id), see move_file.
        let result = sqlx::query(
            r#"
            UPDATE storage.files
               SET is_trashed = TRUE,
                   trashed_at = NOW(),
                   original_folder_id = folder_id,
                   updated_at = NOW(),
                   updated_by = $2
             WHERE id = $1::uuid AND NOT is_trashed
            "#,
        )
        .bind(file_id)
        .bind(caller_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("trash: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(DomainError::not_found("File", file_id));
        }
        Ok(())
    }

    async fn restore_from_trash(
        &self,
        file_id: &str,
        _original_path: &str,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        // §14: `updated_by = $2` (caller_id), see move_file.
        let result = sqlx::query(
            r#"
            UPDATE storage.files
               SET is_trashed = FALSE,
                   trashed_at = NULL,
                   folder_id = COALESCE(original_folder_id, folder_id),
                   original_folder_id = NULL,
                   updated_at = NOW(),
                   updated_by = $2
             WHERE id = $1::uuid AND is_trashed
            "#,
        )
        .bind(file_id)
        .bind(caller_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobWrite", format!("restore: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(DomainError::not_found("File", file_id));
        }
        Ok(())
    }

    async fn delete_file_permanently(&self, file_id: &str) -> Result<(), DomainError> {
        // Read blob_hash before deletion so we can clean up disk after the
        // PG trigger has decremented the ref_count.
        let blob_hash: Option<String> =
            sqlx::query_scalar("SELECT blob_hash FROM storage.files WHERE id = $1::uuid")
                .bind(file_id)
                .fetch_optional(self.pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error("FileBlobWrite", format!("fetch blob_hash: {e}"))
                })?;

        // DELETE fires `trg_files_decrement_blob_ref` — post-2026-08-23
        // it dispatches manifest-first (see migration
        // `20261017000000_file_delete_trigger_manifest_aware.sql`):
        // decrements `chunk_manifests.ref_count` if the hash names a
        // manifest (walking chunks on last-ref), else falls back to
        // `storage.blobs.ref_count`. Counter state after this call is
        // already correct.
        self.delete_file(file_id).await?;

        // Physical cleanup only. `cleanup_if_orphaned` was previously
        // manifest-aware and did counter compensation for the old
        // trigger's over-decrement; after the trigger rewrite it's a
        // legacy-blob-eager-reap helper — safe to keep calling
        // unconditionally (no-op for CDC hashes; reaps legacy blobs
        // that reached ref_count = 0).
        if let Some(hash) = blob_hash {
            self.dedup.cleanup_if_orphaned(&hash).await;
        }

        Ok(())
    }

    async fn copy_folder_tree(
        &self,
        source_folder_id: &str,
        target_parent_id: Option<String>,
        dest_name: Option<String>,
    ) -> Result<CopyFolderTreeResult, DomainError> {
        let row = sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT new_root_id, folders_copied, files_copied \
               FROM storage.copy_folder_tree($1::uuid, $2::uuid, $3)",
        )
        .bind(source_folder_id)
        .bind(&target_parent_id)
        .bind(&dest_name)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| {
            // Map PG P0002 (no_data_found) to NotFound
            if let sqlx::Error::Database(ref db_err) = e {
                if db_err.code().as_deref() == Some("P0002") {
                    return DomainError::not_found("Folder", source_folder_id);
                }
                if db_err.code().as_deref() == Some("23505") {
                    return DomainError::already_exists(
                        "Folder",
                        "a folder with this name already exists in the target location",
                    );
                }
            }
            DomainError::internal_error("FileBlobWrite", format!("copy_folder_tree: {e}"))
        })?;

        tracing::info!(
            "📂 TREE COPY: {} folders + {} files (root: {}, zero-copy via dedup)",
            row.1,
            row.2,
            &row.0[..8]
        );

        Ok(CopyFolderTreeResult {
            new_root_folder_id: row.0,
            folders_copied: row.1,
            files_copied: row.2,
        })
    }
}
