//! Single-query WebDAV path resolver.
//!
//! Replaces the double-query pattern (`get_folder_by_path` + `get_file_by_path`)
//! with a single `UNION ALL` query that returns the first match.  PostgreSQL's
//! `Append` node short-circuits on `LIMIT 1`, so if the folder branch matches
//! the file branch is never executed.

use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::display_helpers::{
    classify_display, format_file_size, intern_display, intern_mime,
};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::common::errors::DomainError;
use crate::domain::entities::file::File;
use crate::domain::entities::folder::Folder;

/// Result of resolving a WebDAV path — either a folder or a file.
#[derive(Debug, Clone)]
pub enum ResolvedResource {
    Folder(FolderDto),
    File(FileDto),
}

/// Resolves a WebDAV path to a folder or file in a single SQL round-trip.
pub struct PathResolverService {
    pool: Arc<PgPool>,
}

impl PathResolverService {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Resolve `path` to a folder or file **within the given drive**.
    ///
    /// Filters on `fo.drive_id = $4` / `fi.drive_id = $4`. Callers
    /// pre-resolve which drive they're operating in — native WebDAV
    /// derives it from the caller's default drive
    /// (`resolve_drive_id_for_native_webdav`); NC WebDAV takes it from
    /// the URL-selected chroot (`chroot.drive_id`). Shared by both
    /// surfaces so the single-query UNION ALL optimisation lands
    /// consistently and no path lookup keys on the doomed
    /// `storage.{files,folders}.user_id` column.
    pub async fn resolve_path_in_drive(
        &self,
        path: &str,
        drive_id: Uuid,
    ) -> Result<ResolvedResource, DomainError> {
        let path = path.trim_start_matches('/').trim_end_matches('/');
        if path.is_empty() {
            return Err(DomainError::not_found("Resource", "empty path"));
        }

        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let filename = segments[segments.len() - 1];
        let folder_path = if segments.len() > 1 {
            segments[..segments.len() - 1].join("/")
        } else {
            String::new()
        };

        // Widened SELECT: also fetches `blob_hash` (for file ETag) and
        // `tree_modified_at` (for folder ETag). Both share the same
        // canonical formulas as the rest of the codebase — see
        // [`File::compute_etag`] and [`Folder::compute_etag`]. Without
        // these two extra columns the resolver used to emit empty
        // ETag strings, and NC's `If-Match` round-trips broke
        // (see the F6b regression on `test_nc_put_mkcol_blake3.sh`).
        let row = sqlx::query_as::<
            _,
            (
                String,         // resource_type
                String,         // id
                String,         // name
                String,         // path
                Option<String>, // parent_id
                Uuid,           // drive_id
                i64,            // created_at
                i64,            // modified_at
                Option<i64>,    // size
                Option<String>, // mime_type
                Option<String>, // folder_id
                Option<String>, // blob_hash (files only)
                Option<i64>,    // tree_modified_at (folders only)
            ),
        >(
            r#"
            SELECT resource_type, id, name, path, parent_id, drive_id,
                   created_at, modified_at, size, mime_type, folder_id,
                   blob_hash, tree_modified_at
              FROM (
                SELECT 'folder'::text       AS resource_type,
                       fo.id::text,
                       fo.name,
                       fo.path,
                       fo.parent_id::text,
                       fo.drive_id,
                       EXTRACT(EPOCH FROM fo.created_at)::bigint AS created_at,
                       EXTRACT(EPOCH FROM fo.updated_at)::bigint AS modified_at,
                       NULL::bigint         AS size,
                       NULL::text           AS mime_type,
                       NULL::text           AS folder_id,
                       NULL::text           AS blob_hash,
                       EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint AS tree_modified_at
                  FROM storage.folders fo
                 WHERE fo.path = $1 AND NOT fo.is_trashed
                   AND fo.drive_id = $4

                UNION ALL

                SELECT 'file'::text         AS resource_type,
                       fi.id::text,
                       fi.name,
                       CASE
                         WHEN fo.path IS NOT NULL AND fo.path != ''
                         THEN fo.path || '/' || fi.name
                         ELSE fi.name
                       END                  AS path,
                       NULL::text           AS parent_id,
                       fi.drive_id,
                       EXTRACT(EPOCH FROM fi.created_at)::bigint AS created_at,
                       EXTRACT(EPOCH FROM fi.updated_at)::bigint AS modified_at,
                       fi.size,
                       fi.mime_type,
                       fi.folder_id::text,
                       fi.blob_hash,
                       NULL::bigint         AS tree_modified_at
                  FROM storage.files fi
                  LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
                 WHERE fi.name = $2
                   AND (
                         ($3 = '' AND fi.folder_id IS NULL)
                         OR fo.path = $3
                       )
                   AND NOT fi.is_trashed
                   AND fi.drive_id = $4
              ) sub
             LIMIT 1
            "#,
        )
        .bind(path) // $1
        .bind(filename) // $2
        .bind(&folder_path) // $3
        .bind(drive_id) // $4
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PathResolver", format!("resolve_in_drive: {e}")))?
        .ok_or_else(|| DomainError::not_found("Resource", path))?;

        let (
            resource_type,
            id,
            name,
            res_path,
            parent_id,
            drive_id,
            created_at,
            modified_at,
            size,
            mime_type,
            folder_id,
            blob_hash,
            tree_modified_at,
        ) = row;

        match resource_type.as_str() {
            "folder" => {
                let tree_mod = tree_modified_at.unwrap_or(modified_at) as u64;
                Ok(ResolvedResource::Folder(FolderDto {
                    etag: Folder::compute_etag(&id, tree_mod),
                    id,
                    name: name.clone(),
                    path: res_path,
                    parent_id,
                    drive_id,
                    created_at: created_at as u64,
                    modified_at: modified_at as u64,
                    is_root: false,
                    icon_class: intern_display("fas fa-folder"),
                    icon_special_class: intern_display("folder-icon"),
                    category: intern_display("Folder"),
                    // §14 provenance not selected by this resolver path —
                    // it's used for existence/type discrimination, not
                    // detailed DTO emission. Callers that need provenance
                    // reload through the repo.
                    created_by: None,
                    updated_by: None,
                    // Caller state flags not looked up here — the
                    // resolver is an internal utility that answers
                    // existence/type questions, not a wire emission
                    // path. Callers that emit to the SPA reload
                    // through the listing repo or the caller_flags
                    // helper.
                    is_favorite: false,
                    is_shared: false,
                    mount: None,
                }))
            }
            _ => {
                let mime = mime_type.unwrap_or_else(|| "application/octet-stream".to_string());
                let sz = size.unwrap_or(0) as u64;
                let hash = blob_hash.unwrap_or_default();
                let modified_at_u = modified_at as u64;
                let etag = File::compute_etag(&hash, modified_at_u);
                let classes = classify_display(&name, &mime);
                Ok(ResolvedResource::File(FileDto {
                    id,
                    name: name.clone(),
                    path: res_path,
                    size: sz,
                    mime_type: intern_mime(&mime),
                    folder_id,
                    created_at: created_at as u64,
                    modified_at: modified_at_u,
                    icon_class: intern_display(classes.icon_class),
                    icon_special_class: intern_display(classes.icon_special_class),
                    category: intern_display(classes.category),
                    size_formatted: format_file_size(sz),
                    content_hash: hash,
                    etag,
                    // §14 provenance not selected by this resolver path
                    created_by: None,
                    updated_by: None,
                    // Caller state flags not looked up here — see
                    // the folder branch above for rationale.
                    is_favorite: false,
                    is_shared: false,
                }))
            }
        }
    }

    /// Returns `true` if the resource at `path` belongs to `user_id`.
    /// Check whether `path` resolves to a folder or file within the
    /// given drive. Companion to `resolve_path_in_drive` — same scope
    /// filter, existence-only projection.
    pub async fn exists_in_drive(&self, path: &str, drive_id: Uuid) -> Result<bool, DomainError> {
        let path = path.trim_start_matches('/').trim_end_matches('/');
        if path.is_empty() {
            return Ok(false);
        }

        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let filename = segments[segments.len() - 1];
        let folder_path = if segments.len() > 1 {
            segments[..segments.len() - 1].join("/")
        } else {
            String::new()
        };

        let exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
              SELECT 1 FROM storage.folders
               WHERE path = $1 AND NOT is_trashed AND drive_id = $4
            ) OR EXISTS(
              SELECT 1
                FROM storage.files fi
                LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
               WHERE fi.name = $2
                 AND (($3 = '' AND fi.folder_id IS NULL) OR fo.path = $3)
                 AND NOT fi.is_trashed
                 AND fi.drive_id = $4
            )
            "#,
        )
        .bind(path)
        .bind(filename)
        .bind(&folder_path)
        .bind(drive_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("PathResolver", format!("exists_in_drive: {e}"))
        })?;

        Ok(exists)
    }
}
