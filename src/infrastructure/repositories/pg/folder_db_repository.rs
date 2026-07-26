//! PostgreSQL-backed folder repository.
//!
//! Implements `FolderRepository` (and thus `FolderStoragePort`) using the
//! `storage.folders` table.  Folders are purely virtual — no physical
//! directories are created on the filesystem.
//!
//! Folder paths are **materialized** in a `path TEXT` column maintained by
//! database triggers, so reading a folder's full path is always O(1) — no
//! recursive CTEs or N+1 queries.

use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use super::transaction_utils::retry_on_deadlock;
use crate::application::dtos::folder_dto::{FolderResourceCursor, FolderResourceRow};
use crate::common::errors::DomainError;
use crate::domain::entities::folder::Folder;
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::services::authorization::ResourceKind;
use crate::domain::services::path_service::StoragePath;

/// Type alias for folder metadata rows from SQL queries.
/// Tuple order: id, name, path, parent_id, drive_id, created_at,
/// modified_at, tree_modified_at, created_by, updated_by.
/// The trailing `tree_modified_at` feeds [`Folder::etag`] — every
/// SELECT here must include `EXTRACT(EPOCH FROM tree_modified_at)::bigint`.
/// `drive_id` is the post-D0 `NOT NULL` scope axis for path-based
/// lookups. `created_by` / `updated_by` are the §14 provenance
/// columns, nullable because the FK is `ON DELETE SET NULL`.
///
/// Post-D7-step-6: `storage.folders.user_id` dropped, so the tuple
/// no longer carries it. The domain entity's `user_id` field is
/// populated with `None` at `row_to_folder` construction.
/// `id` / `parent_id` decode as binary `Uuid` (16 bytes on the wire vs 36
/// as `::text`, and the server skips the cast); `row_to_folder` renders
/// them to `String` once app-side — the round-6 `row_to_file` shape
/// (benches/ROUND6.md §10) applied to the folder listings.
type FolderRow = (
    Uuid,
    String,
    String,
    Option<Uuid>,
    Uuid,
    i64,
    i64,
    i64,
    Option<Uuid>,
    Option<Uuid>,
);

/// `FolderRow` + trailing `(is_favorite, is_shared)` — the caller-scoped
/// booleans populated by per-row EXISTS in `search_folders`. Split out so
/// the many `sqlx::query_as::<_, FolderRowWithFlags>(...)` sites stay
/// terse instead of repeating a 12-element inline tuple.
type FolderRowWithFlags = (
    Uuid,
    String,
    String,
    Option<Uuid>,
    Uuid,
    i64,
    i64,
    i64,
    Option<Uuid>,
    Option<Uuid>,
    bool,
    bool,
);

/// Return shape of `search_folders`: two parallel vecs aligned 1:1
/// (folder domain entity + `(is_favorite, is_shared)` caller flags).
/// Kept as a type alias so the SQL builder + trait implementations
/// share one name (`clippy::type_complexity`).
pub(crate) type FoldersWithFlags = (Vec<Folder>, Vec<(bool, bool)>);

/// Convert `FolderRowWithFlags` rows into the `(Vec<Folder>, caller_flags)`
/// pair `search_folders` returns. Keeps the two parallel vecs aligned
/// 1:1 by index.
fn build_folders_with_flags(
    rows: Vec<FolderRowWithFlags>,
) -> Result<FoldersWithFlags, DomainError> {
    let mut folders = Vec::with_capacity(rows.len());
    let mut flags = Vec::with_capacity(rows.len());
    for (id, name, path, pid, did, ca, ma, tma, cb, ub, is_fav, is_shr) in rows {
        let folder =
            FolderDbRepository::row_to_folder(id, name, path, pid, did, ca, ma, tma, cb, ub)?;
        folders.push(folder);
        flags.push((is_fav, is_shr));
    }
    Ok((folders, flags))
}

/// Row projected by `fetch_ancestor_walk`. One per folder in the
/// leaf→root walk (root order is reversed to root-first by the caller).
/// `has_folder_grant` = caller has a `role_grants` row on THIS folder;
/// `has_drive_grant` = caller has drive-membership on the containing drive.
/// Either grant satisfies Read; the split lets the service pick the right
/// `AccessSource` kind (`Drive` vs `DirectShare`).
#[derive(Debug, sqlx::FromRow)]
pub struct AncestorRow {
    pub id: Uuid,
    pub name: String,
    pub parent_id: Option<Uuid>,
    pub drive_id: Uuid,
    #[allow(dead_code)]
    pub depth: i32,
    pub has_folder_grant: bool,
    pub has_drive_grant: bool,
}

/// Type alias for paginated folder rows (includes total_count as
/// the last element after the §14 provenance columns). Same
/// column set as [`FolderRow`] plus the trailing count.
type FolderRowPaginated = (
    Uuid,
    String,
    String,
    Option<Uuid>,
    Uuid,
    i64,
    i64,
    i64,
    Option<Uuid>,
    Option<Uuid>,
    i64,
);

/// SQL `EXISTS (…)` predicate — true when the caller (bound to `$1`) has
/// any active `role_grants` on the drive owning `fo` (the aliased folder
/// row). Group memberships (direct + transitive) are expanded inline via
/// `storage.caller_group_ids($1)` (recursive; see migration
/// `20260901000002_caller_group_ids_function.sql`).
///
/// Used by every drive-scoped folder query in this repo:
/// - `list_root_folders_for_caller` / `_paginated`
/// - `search_folders` (all three branches)
/// - `list_descendant_folders`
///
/// **Alias contract**: queries splicing this in MUST alias
/// `storage.folders` as `fo`. `$1` is reserved for `caller_id`; other
/// bind params start at `$2`.
///
/// This mirrors — but is not shared with — the drive-membership shape in
/// `drive_pg_repository::list_readable_by` (uses `JOIN` on `d.id`) and
/// the media-scoping subqueries in `file_blob_read_repository` (use
/// `IN (SELECT d.id …)` with the `include_in_photo_index` policy
/// filter). When the grant model changes, update all sites in parallel.
const CALLER_CAN_READ_DRIVE: &str = "EXISTS (\
        SELECT 1 \
          FROM storage.role_grants g \
         WHERE g.resource_type = 'drive' \
           AND g.resource_id   = fo.drive_id \
           AND (g.expires_at IS NULL OR g.expires_at > NOW()) \
           AND ( \
                 (g.subject_type = 'user'  AND g.subject_id = $1) \
              OR (g.subject_type = 'group' AND g.subject_id IN \
                      (SELECT storage.caller_group_ids($1))) \
               ) \
    )";

/// PostgreSQL-backed folder repository.
///
/// All folder metadata lives in the `storage.folders` table.  The physical
/// filesystem is never touched for folder operations.
pub struct FolderDbRepository {
    pool: Option<Arc<PgPool>>,
}

impl FolderDbRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool: Some(pool) }
    }

    /// Creates a stub instance for `AppState::default()`.
    /// This is never called in production — only used for route scaffolding.
    pub fn new_stub() -> Self {
        Self { pool: None }
    }

    /// Get the pool, panicking if stub.
    fn pool(&self) -> &PgPool {
        self.pool
            .as_deref()
            .expect("FolderDbRepository: pool not available (stub instance)")
    }

    // ── helpers ──────────────────────────────────────────────────

    /// Convert a database row into a `Folder` domain entity.
    ///
    /// The `path` comes directly from the materialized `path` column — no
    /// extra queries needed. `created_by` / `updated_by` carry the
    /// §14 provenance signal through the entity layer; both are
    /// `Option<Uuid>` because the FK is `ON DELETE SET NULL`.
    #[allow(clippy::too_many_arguments)]
    fn row_to_folder(
        id: Uuid,
        name: String,
        path: String,
        parent_id: Option<Uuid>,
        drive_id: Uuid,
        created_at: i64,
        modified_at: i64,
        tree_modified_at: i64,
        created_by: Option<Uuid>,
        updated_by: Option<Uuid>,
    ) -> Result<Folder, DomainError> {
        Folder::from_materialized_row(
            id.to_string(),
            name,
            path,
            parent_id.map(|u| u.to_string()),
            drive_id,
            created_at as u64,
            modified_at as u64,
            tree_modified_at as u64,
            created_by,
            updated_by,
        )
        .map_err(|e| DomainError::internal_error("FolderDb", format!("entity: {e}")))
    }

    /// Batch-fetch folders by id — the by-ids counterpart of `get_folder`,
    /// resolving a page of ACL grants or favorites in ONE query instead of
    /// one per id. Same `NOT is_trashed` filter and column mapping as
    /// `get_folder`; missing or trashed ids drop out and callers re-associate
    /// by id; ordering is not guaranteed.
    pub async fn get_folders_by_ids(&self, ids: &[String]) -> Result<Vec<Folder>, DomainError> {
        let uuid_ids: Vec<Uuid> = ids.iter().filter_map(|id| id.parse().ok()).collect();
        if uuid_ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query_as::<_, FolderRow>(
            r#"
            SELECT id, name, path, parent_id, drive_id,
                   EXTRACT(EPOCH FROM created_at)::bigint,
                   EXTRACT(EPOCH FROM updated_at)::bigint,
                   EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                   created_by, updated_by
              FROM storage.folders
             WHERE id = ANY($1) AND NOT is_trashed
            "#,
        )
        .bind(&uuid_ids)
        .fetch_all(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("get_folders_by_ids: {e}")))?;

        rows.into_iter()
            .map(|r| Self::row_to_folder(r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9))
            .collect()
    }
}

impl FolderRepository for FolderDbRepository {
    async fn create_folder(
        &self,
        name: String,
        parent_id: Option<String>,
        caller_id: Uuid,
    ) -> Result<Folder, DomainError> {
        // Derive `drive_id` from the parent folder. Root-level folders
        // are reserved for the atomic drive-creation transaction in
        // `DrivePgRepository::create_personal_drive_atomic` (see
        // `docs/plan/drive.md` §3) — the no-orphan-root-folder trigger
        // enforces this at the DB level.
        //
        // Post-D7: only `drive_id` is fetched from the parent. The
        // legacy `user_id` column is no longer written to on new rows
        // (migration `20260902000000_files_folders_user_id_nullable.sql`);
        // provenance flows through `created_by` / `updated_by` (§14).
        let drive_id: Uuid = if let Some(ref pid) = parent_id {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT drive_id FROM storage.folders WHERE id = $1::uuid",
            )
            .bind(pid)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| DomainError::internal_error("FolderDb", format!("parent lookup: {e}")))?
            .ok_or_else(|| DomainError::not_found("Folder", pid))?
        } else {
            return Err(DomainError::internal_error(
                "FolderDb",
                "Cannot create root folder — root folders are reserved for the \
                 atomic drive-creation transaction in DrivePgRepository::\
                 create_personal_drive_atomic (docs/plan/drive.md §3). The \
                 no-orphan-root-folder trigger enforces this at the DB level.",
            ));
        };

        // Post-D7: no `user_id` in the INSERT column list — the column
        // is nullable and copied rows / new rows leave it NULL.
        // `created_by` / `updated_by` carry §14 provenance (both bind to
        // the caller — pre-D2 that's silently the parent's owner too,
        // but the distinction matters once shared drives let an Editor
        // mutate a folder owned by someone else).
        //
        // RETURNING surfaces the two provenance columns so the built
        // entity / DTO carries fresh values without a re-read.
        let row = sqlx::query_as::<
            _,
            (
                Uuid,
                Option<Uuid>,
                String,
                i64,
                i64,
                i64,
                Option<Uuid>,
                Option<Uuid>,
            ),
        >(
            r#"
            INSERT INTO storage.folders
                (name, parent_id, drive_id, created_by, updated_by)
            VALUES ($1, $2::uuid, $3, $4, $4)
            RETURNING id,
                      parent_id,
                      path,
                      EXTRACT(EPOCH FROM created_at)::bigint,
                      EXTRACT(EPOCH FROM updated_at)::bigint,
                      EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                      created_by,
                      updated_by
            "#,
        )
        .bind(&name)
        .bind(&parent_id)
        .bind(drive_id)
        .bind(caller_id)
        .fetch_one(self.pool())
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db_err) = e
                && db_err.code().as_deref() == Some("23505")
            {
                return DomainError::already_exists(
                    "Folder",
                    format!("{name} already exists in parent"),
                );
            }
            DomainError::internal_error("FolderDb", format!("insert: {e}"))
        })?;

        Self::row_to_folder(
            row.0, name, row.2, row.1, drive_id, row.3, row.4, row.5,
            // Fresh from RETURNING — caller_id was bound to both columns.
            row.6, row.7,
        )
    }

    async fn get_folder(&self, id: &str) -> Result<Folder, DomainError> {
        let row = sqlx::query_as::<_, FolderRow>(
            r#"
            SELECT id, name, path, parent_id, drive_id,
                   EXTRACT(EPOCH FROM created_at)::bigint,
                   EXTRACT(EPOCH FROM updated_at)::bigint,
                   EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                     created_by, updated_by
              FROM storage.folders
             WHERE id = $1::uuid AND NOT is_trashed
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("get: {e}")))?
        .ok_or_else(|| DomainError::not_found("Folder", id))?;

        Self::row_to_folder(
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9,
        )
    }

    async fn get_folder_by_path(
        &self,
        storage_path: &StoragePath,
        drive_id: Uuid,
    ) -> Result<Folder, DomainError> {
        let path_str = storage_path.to_string();
        // Strip leading '/' if present — DB stores "Home - user/Docs", not "/Home - user/Docs"
        let lookup = path_str.strip_prefix('/').unwrap_or(&path_str);

        if lookup.is_empty() {
            return Err(DomainError::not_found("Folder", "empty path"));
        }

        // Scoped by drive_id: post-D0 `storage.folders.path` is unique
        // only within a single drive. Root-folder names like
        // `"Personal"` repeat across drives, so without the drive_id
        // filter the planner returns a non-deterministic row — which
        // breaks owner-short-circuit checks and crosses drive
        // boundaries (the AuthZ axis that replaces the old per-user
        // wrapper scoping post-D0).
        let row = sqlx::query_as::<_, FolderRow>(
            r#"
            SELECT id, name, path, parent_id, drive_id,
                   EXTRACT(EPOCH FROM created_at)::bigint,
                   EXTRACT(EPOCH FROM updated_at)::bigint,
                       EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                     created_by, updated_by
              FROM storage.folders
             WHERE path = $1 AND drive_id = $2 AND NOT is_trashed
            "#,
        )
        .bind(lookup)
        .bind(drive_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("path lookup: {e}")))?
        .ok_or_else(|| DomainError::not_found("Folder", lookup))?;

        Self::row_to_folder(
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9,
        )
    }

    #[allow(clippy::type_complexity)]
    async fn list_folders(&self, parent_id: Option<&str>) -> Result<Vec<Folder>, DomainError> {
        let rows: Vec<FolderRow> = if let Some(pid) = parent_id {
            sqlx::query_as(
                r#"
                SELECT id, name, path, parent_id, drive_id,
                       EXTRACT(EPOCH FROM created_at)::bigint,
                       EXTRACT(EPOCH FROM updated_at)::bigint,
                       EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                         created_by, updated_by
                  FROM storage.folders
                 WHERE parent_id = $1::uuid AND NOT is_trashed
                 ORDER BY name
                "#,
            )
            .bind(pid)
            .fetch_all(self.pool())
            .await
        } else {
            sqlx::query_as(
                r#"
                SELECT id, name, path, parent_id, drive_id,
                       EXTRACT(EPOCH FROM created_at)::bigint,
                       EXTRACT(EPOCH FROM updated_at)::bigint,
                       EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                         created_by, updated_by
                  FROM storage.folders
                 WHERE parent_id IS NULL AND NOT is_trashed
                 ORDER BY name
                "#,
            )
            .fetch_all(self.pool())
            .await
        }
        .map_err(|e| DomainError::internal_error("FolderDb", format!("list: {e}")))?;

        rows.into_iter()
            .map(|(id, name, path, pid, did, ca, ma, tma, cb, ub)| {
                Self::row_to_folder(id, name, path, pid, did, ca, ma, tma, cb, ub)
            })
            .collect()
    }

    async fn list_root_folders_for_caller(
        &self,
        caller_id: Uuid,
    ) -> Result<Vec<Folder>, DomainError> {
        // Drive-scoped root-folder listing: return every root folder
        // whose drive the caller has any role_grant on. Group
        // memberships (direct + transitive) resolve inline via
        // `storage.caller_group_ids($1)`.
        //
        // Closes `bug_root_folder_listing_legacy_user_id`: pre-D7 this
        // query filtered on `folders.user_id = $caller`, which returned
        // rows admin had created for other users' drives without ever
        // getting a role on them. The drive-membership predicate below
        // makes the "admin's own listing" correct without a separate
        // filter.
        //
        // `caller_role` is NOT surfaced here — see the memory
        // `project_caller_role_on_file_folder_dto` and the note at the
        // top of `folder_repository.rs`. Frontend cross-references
        // `/api/drives::caller_role` via `folder.drive_id`.
        let sql = format!(
            "SELECT fo.id, fo.name, fo.path, fo.parent_id, \
                    fo.drive_id, \
                    EXTRACT(EPOCH FROM fo.created_at)::bigint, \
                    EXTRACT(EPOCH FROM fo.updated_at)::bigint, \
                    EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint, \
                    fo.created_by, fo.updated_by \
               FROM storage.folders fo \
              WHERE fo.parent_id IS NULL \
                AND NOT fo.is_trashed \
                AND {CALLER_CAN_READ_DRIVE} \
              ORDER BY fo.name"
        );
        let rows: Vec<FolderRow> = sqlx::query_as(&sql)
            .bind(caller_id)
            .fetch_all(self.pool())
            .await
            .map_err(|e| {
                DomainError::internal_error("FolderDb", format!("list_root_folders: {e}"))
            })?;

        rows.into_iter()
            .map(|(id, name, path, pid, did, ca, ma, tma, cb, ub)| {
                Self::row_to_folder(id, name, path, pid, did, ca, ma, tma, cb, ub)
            })
            .collect()
    }

    /// Paginated folder listing — single query with `COUNT(*) OVER()` window
    /// function so the total matching count comes back alongside the data rows,
    /// eliminating a separate COUNT round-trip.
    #[allow(clippy::type_complexity)]
    async fn list_folders_paginated(
        &self,
        parent_id: Option<&str>,
        offset: usize,
        limit: usize,
        include_total: bool,
    ) -> Result<(Vec<Folder>, Option<usize>), DomainError> {
        let rows: Vec<FolderRowPaginated> = if let Some(pid) = parent_id {
            sqlx::query_as(
                r#"
                SELECT id, name, path, parent_id, drive_id,
                       EXTRACT(EPOCH FROM created_at)::bigint,
                       EXTRACT(EPOCH FROM updated_at)::bigint,
                       EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                       created_by, updated_by,
                       COUNT(*) OVER() AS total_count
                  FROM storage.folders
                 WHERE parent_id = $1::uuid AND NOT is_trashed
                 ORDER BY name
                 LIMIT $2 OFFSET $3
                "#,
            )
            .bind(pid)
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(self.pool())
            .await
        } else {
            sqlx::query_as(
                r#"
                SELECT id, name, path, parent_id, drive_id,
                       EXTRACT(EPOCH FROM created_at)::bigint,
                       EXTRACT(EPOCH FROM updated_at)::bigint,
                       EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                       created_by, updated_by,
                       COUNT(*) OVER() AS total_count
                  FROM storage.folders
                 WHERE parent_id IS NULL AND NOT is_trashed
                 ORDER BY name
                 LIMIT $1 OFFSET $2
                "#,
            )
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(self.pool())
            .await
        }
        .map_err(|e| DomainError::internal_error("FolderDb", format!("paginate: {e}")))?;

        // total_count is identical in every row; 0 when the result set is empty.
        let total = if include_total {
            Some(rows.first().map_or(0, |r| r.10) as usize)
        } else {
            None
        };

        let folders: Result<Vec<Folder>, DomainError> = rows
            .into_iter()
            .map(|(id, name, path, pid, did, ca, ma, tma, cb, ub, _total)| {
                Self::row_to_folder(id, name, path, pid, did, ca, ma, tma, cb, ub)
            })
            .collect();
        Ok((folders?, total))
    }

    /// Keyset sub-folder page: `name > $after ORDER BY name LIMIT $limit`,
    /// one bounded index-range read off `idx_folders_unique_name` — the
    /// cursor predicate is only emitted when a cursor exists (a bound
    /// disjunction would block the index condition under generic plans,
    /// same rule as `list_files_batch`). Root scope (`parent_id = None`)
    /// keeps the trait's in-memory default: roots are one-per-drive, a
    /// handful of rows.
    async fn list_folders_batch(
        &self,
        parent_id: Option<&str>,
        after_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Folder>, DomainError> {
        let Some(pid) = parent_id else {
            let mut all = self.list_folders(None).await?;
            all.sort_by(|a, b| a.name().cmp(b.name()));
            return Ok(all
                .into_iter()
                .filter(|f| after_name.is_none_or(|a| f.name() > a))
                .take(limit)
                .collect());
        };

        let cursor_pred = if after_name.is_some() {
            "AND name > $3"
        } else {
            "AND $3::text IS NULL"
        };
        let sql = format!(
            "SELECT id, name, path, parent_id, drive_id, \
                    EXTRACT(EPOCH FROM created_at)::bigint, \
                    EXTRACT(EPOCH FROM updated_at)::bigint, \
                    EXTRACT(EPOCH FROM tree_modified_at)::bigint, \
                    created_by, updated_by \
               FROM storage.folders \
              WHERE parent_id = $1::uuid AND NOT is_trashed \
                {cursor_pred} \
              ORDER BY name \
              LIMIT $2"
        );
        let rows: Vec<FolderRow> = sqlx::query_as(&sql)
            .bind(pid)
            .bind(limit as i64)
            .bind(after_name)
            .fetch_all(self.pool())
            .await
            .map_err(|e| DomainError::internal_error("FolderDb", format!("batch: {e}")))?;

        rows.into_iter()
            .map(|(id, name, path, pid, did, ca, ma, tma, cb, ub)| {
                Self::row_to_folder(id, name, path, pid, did, ca, ma, tma, cb, ub)
            })
            .collect()
    }

    /// Paginated companion to `list_root_folders_for_caller` — same
    /// drive-membership predicate, adds LIMIT/OFFSET and an optional
    /// window-function COUNT so total pages can be surfaced without a
    /// second round-trip.
    async fn list_root_folders_for_caller_paginated(
        &self,
        caller_id: Uuid,
        offset: usize,
        limit: usize,
        include_total: bool,
    ) -> Result<(Vec<Folder>, Option<usize>), DomainError> {
        let sql = format!(
            "SELECT fo.id, fo.name, fo.path, fo.parent_id, \
                    fo.drive_id, \
                    EXTRACT(EPOCH FROM fo.created_at)::bigint, \
                    EXTRACT(EPOCH FROM fo.updated_at)::bigint, \
                    EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint, \
                    fo.created_by, fo.updated_by, \
                    COUNT(*) OVER() AS total_count \
               FROM storage.folders fo \
              WHERE fo.parent_id IS NULL \
                AND NOT fo.is_trashed \
                AND {CALLER_CAN_READ_DRIVE} \
              ORDER BY fo.name \
              LIMIT $2 OFFSET $3"
        );
        let rows: Vec<FolderRowPaginated> = sqlx::query_as(&sql)
            .bind(caller_id)
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(self.pool())
            .await
            .map_err(|e| {
                DomainError::internal_error("FolderDb", format!("list_root_folders_paginated: {e}"))
            })?;

        let total = if include_total {
            Some(rows.first().map_or(0, |r| r.10) as usize)
        } else {
            None
        };

        let folders: Result<Vec<Folder>, DomainError> = rows
            .into_iter()
            .map(|(id, name, path, pid, did, ca, ma, tma, cb, ub, _total)| {
                Self::row_to_folder(id, name, path, pid, did, ca, ma, tma, cb, ub)
            })
            .collect();
        Ok((folders?, total))
    }

    async fn rename_folder(
        &self,
        id: &str,
        new_name: String,
        caller_id: Uuid,
    ) -> Result<Folder, DomainError> {
        // The BEFORE UPDATE trigger recomputes path/lpath for this row;
        // the AFTER UPDATE cascade trigger then batch-updates all
        // descendants in a single UPDATE using the GiST lpath index.
        // That multi-row rewrite can deadlock against the tree-ETag
        // flusher's id-ordered ancestor bump — retry instead of failing
        // the user's operation (40P01 only; 23505 still maps below).
        //
        // §14: `updated_by = $3` (caller_id) — the caller mutated this
        // row, not the row's owner. In D2 a shared-drive member can
        // rename a row they don't own; the previous `updated_by = user_id`
        // would have silently recorded the wrong principal.
        let row = retry_on_deadlock("folders.rename", || {
            sqlx::query_as::<_, FolderRow>(
                r#"
                UPDATE storage.folders
                   SET name = $1, updated_at = NOW(), updated_by = $3
                 WHERE id = $2::uuid AND NOT is_trashed
                RETURNING id, name, path, parent_id, drive_id,
                          EXTRACT(EPOCH FROM created_at)::bigint,
                          EXTRACT(EPOCH FROM updated_at)::bigint,
                           EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                             created_by, updated_by
                "#,
            )
            .bind(&new_name)
            .bind(id)
            .bind(caller_id)
            .fetch_optional(self.pool())
        })
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db_err) = e
                && db_err.code().as_deref() == Some("23505")
            {
                return DomainError::already_exists("Folder", format!("{new_name} already exists"));
            }
            DomainError::internal_error("FolderDb", format!("rename: {e}"))
        })?
        .ok_or_else(|| DomainError::not_found("Folder", id))?;

        Self::row_to_folder(
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9,
        )
    }

    async fn move_folder(
        &self,
        id: &str,
        new_parent_id: Option<&str>,
        caller_id: Uuid,
    ) -> Result<Folder, DomainError> {
        // The BEFORE UPDATE trigger recomputes path/lpath for this row;
        // the AFTER UPDATE cascade trigger then batch-updates all
        // descendants in a single UPDATE using the GiST lpath index.
        // Retried on deadlock vs the tree-ETag flusher (see rename_folder).
        //
        // §14: `updated_by = $3` (caller_id), see rename_folder.
        //
        // D6: also sync `drive_id` from the destination parent on
        // cross-drive moves. The CTE-derived `dest.drive_id` is
        // assigned via COALESCE so a root-level move (no destination —
        // `new_parent_id = NULL`) keeps the existing drive_id, mirroring
        // the file move path. The cascade trigger
        // (`cascade_folder_path`) then propagates the new drive_id to
        // every descendant folder + file in the subtree — see
        // `migrations/20260807000000_cascade_drive_id_on_folder_move.sql`.
        let row = retry_on_deadlock("folders.move", || {
            sqlx::query_as::<_, FolderRow>(
                r#"
                WITH dest AS (
                    SELECT drive_id FROM storage.folders WHERE id = $1::uuid
                )
                UPDATE storage.folders f
                   SET parent_id  = $1::uuid,
                       drive_id   = COALESCE((SELECT drive_id FROM dest), f.drive_id),
                       updated_at = NOW(),
                       updated_by = $3
                 WHERE f.id = $2::uuid AND NOT f.is_trashed
                RETURNING f.id, f.name, f.path, f.parent_id, f.drive_id,
                          EXTRACT(EPOCH FROM f.created_at)::bigint,
                          EXTRACT(EPOCH FROM f.updated_at)::bigint,
                          EXTRACT(EPOCH FROM f.tree_modified_at)::bigint,
                          f.created_by, f.updated_by
                "#,
            )
            .bind(new_parent_id)
            .bind(id)
            .bind(caller_id)
            .fetch_optional(self.pool())
        })
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("move: {e}")))?
        .ok_or_else(|| DomainError::not_found("Folder", id))?;

        Self::row_to_folder(
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9,
        )
    }

    async fn delete_folder(&self, id: &str) -> Result<(), DomainError> {
        // Delete all files whose folder is anywhere in the subtree.
        // Uses the GiST-indexed ltree `<@` operator — O(log N) vs the
        // O(depth × N) recursive CTE it replaces.
        // Both statements retried on deadlock vs the tree-ETag flusher's
        // id-ordered ancestor bump (multi-row exclusive locks).
        retry_on_deadlock("folders.delete_files", || {
            sqlx::query(
                "DELETE FROM storage.files \
                  WHERE folder_id IN ( \
                      SELECT id FROM storage.folders \
                       WHERE lpath <@ (SELECT lpath FROM storage.folders WHERE id = $1::uuid) \
                  )",
            )
            .bind(id)
            .execute(self.pool())
        })
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("delete files: {e}")))?;

        // Then delete the folder (CASCADE will remove descendant folders)
        let result = retry_on_deadlock("folders.delete", || {
            sqlx::query("DELETE FROM storage.folders WHERE id = $1::uuid")
                .bind(id)
                .execute(self.pool())
        })
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("delete: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(DomainError::not_found("Folder", id));
        }
        Ok(())
    }

    async fn folder_exists(
        &self,
        storage_path: &StoragePath,
        drive_id: Uuid,
    ) -> Result<bool, DomainError> {
        let path_str = storage_path.to_string();
        let lookup = path_str.strip_prefix('/').unwrap_or(&path_str);

        // Post-D0 `storage.folders.path` repeats across drives —
        // filter by `drive_id` to scope the existence check.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM storage.folders \
             WHERE path = $1 AND drive_id = $2 AND NOT is_trashed)",
        )
        .bind(lookup)
        .bind(drive_id)
        .fetch_one(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("exists: {e}")))?;

        Ok(exists)
    }

    async fn get_folder_path(&self, id: &str) -> Result<StoragePath, DomainError> {
        let path: String =
            sqlx::query_scalar("SELECT path FROM storage.folders WHERE id = $1::uuid")
                .bind(id)
                .fetch_optional(self.pool())
                .await
                .map_err(|e| DomainError::internal_error("FolderDb", format!("get_path: {e}")))?
                .ok_or_else(|| DomainError::not_found("Folder", id))?;

        Ok(StoragePath::from_string(&path))
    }

    // ── Trash operations ──

    async fn move_to_trash(&self, folder_id: &str, caller_id: Uuid) -> Result<(), DomainError> {
        // Soft-delete the whole subtree in one statement: the root flips
        // `is_trashed` and records `original_parent_id` so restore knows
        // where to put it back; every descendant (folder or file) that
        // wasn't already in trash flips `is_trashed` too but leaves the
        // `original_*` column NULL. That NULL is the marker the restore
        // path uses to tell "cascade-trashed with the root" from
        // "independently trashed earlier" — the latter must stay in
        // trash even when the root is restored.
        //
        // Without this cascade, descendants used to remain `is_trashed = false`
        // and stay directly addressable by their full path (PROPFIND on
        // `/g9-tree/file.txt` still resolved 207 even though the parent
        // collection was gone) — a class of data-integrity drift that
        // confused desktop-sync tree walks.
        //
        // §14: all three CTE branches stamp `updated_by = $2`
        // (caller_id). The cascade is "the caller trashed this
        // subtree", not "each owner trashed their own row".
        let result = retry_on_deadlock("folders.trash", || {
            sqlx::query_scalar::<_, i64>(
                r#"
                WITH trash_root AS (
                    UPDATE storage.folders
                       SET is_trashed = TRUE,
                           trashed_at = NOW(),
                           original_parent_id = parent_id,
                           updated_at = NOW(),
                           updated_by = $2
                     WHERE id = $1::uuid AND NOT is_trashed
                    RETURNING id, lpath
                ),
                trash_descendant_folders AS (
                    UPDATE storage.folders f
                       SET is_trashed = TRUE,
                           trashed_at = NOW(),
                           updated_at = NOW(),
                           updated_by = $2
                      FROM trash_root tr
                     WHERE f.lpath <@ tr.lpath
                       AND f.id != tr.id
                       AND NOT f.is_trashed
                    RETURNING 1
                ),
                trash_descendant_files AS (
                    UPDATE storage.files fi
                       SET is_trashed = TRUE,
                           trashed_at = NOW(),
                           updated_at = NOW(),
                           updated_by = $2
                      FROM trash_root tr
                      JOIN storage.folders f ON f.lpath <@ tr.lpath
                     WHERE fi.folder_id = f.id
                       AND NOT fi.is_trashed
                    RETURNING 1
                )
                SELECT COUNT(*) FROM trash_root
                "#,
            )
            .bind(folder_id)
            .bind(caller_id)
            .fetch_one(self.pool())
        })
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("trash: {e}")))?;

        if result == 0 {
            return Err(DomainError::not_found("Folder", folder_id));
        }

        Ok(())
    }

    async fn restore_from_trash(
        &self,
        folder_id: &str,
        _original_path: &str,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        // Inverse of the cascade in `move_to_trash`: restore the root
        // (BEFORE UPDATE trigger recomputes path/lpath via the parent_id
        // change), then un-trash every descendant whose `original_*`
        // column is NULL — those are the rows we cascade-trashed
        // ourselves. Descendants that were independently trashed
        // *before* this folder went to trash have `original_*` set, so
        // they correctly stay in trash and continue to show up as
        // top-level trash entries via `storage.trash_items`.
        //
        // §14: all three CTE branches stamp `updated_by = $2`
        // (caller_id). Restoration is "the caller restored this
        // subtree", regardless of who originally owned each row.
        let result = retry_on_deadlock("folders.restore", || {
            sqlx::query_scalar::<_, i64>(
                r#"
                WITH restore_root AS (
                    UPDATE storage.folders
                       SET is_trashed = FALSE,
                           trashed_at = NULL,
                           parent_id = COALESCE(original_parent_id, parent_id),
                           original_parent_id = NULL,
                           updated_at = NOW(),
                           updated_by = $2
                     WHERE id = $1::uuid AND is_trashed
                    RETURNING id, lpath
                ),
                restore_descendant_folders AS (
                    UPDATE storage.folders f
                       SET is_trashed = FALSE,
                           trashed_at = NULL,
                           updated_at = NOW(),
                           updated_by = $2
                      FROM restore_root rr
                     WHERE f.lpath <@ rr.lpath
                       AND f.id != rr.id
                       AND f.is_trashed
                       AND f.original_parent_id IS NULL
                    RETURNING 1
                ),
                restore_descendant_files AS (
                    UPDATE storage.files fi
                       SET is_trashed = FALSE,
                           trashed_at = NULL,
                           updated_at = NOW(),
                           updated_by = $2
                      FROM restore_root rr
                      JOIN storage.folders f ON f.lpath <@ rr.lpath
                     WHERE fi.folder_id = f.id
                       AND fi.is_trashed
                       AND fi.original_folder_id IS NULL
                    RETURNING 1
                )
                SELECT COUNT(*) FROM restore_root
                "#,
            )
            .bind(folder_id)
            .bind(caller_id)
            .fetch_one(self.pool())
        })
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("restore: {e}")))?;

        if result == 0 {
            return Err(DomainError::not_found("Folder", folder_id));
        }

        Ok(())
    }

    async fn list_file_ids_in_subtree(&self, folder_id: &str) -> Result<Vec<String>, DomainError> {
        // Same GiST subtree predicate the bulk DELETEs in `delete_folder` /
        // `delete_folder_permanently` use — single index scan on
        // `storage.folders.lpath`. Returns the file ids the cascade is
        // about to reap so the caller can fire `on_file_deleted` per id.
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT f.id::text FROM storage.files f \
              WHERE f.folder_id IN ( \
                  SELECT id FROM storage.folders \
                   WHERE lpath <@ (SELECT lpath FROM storage.folders WHERE id = $1::uuid) \
              )",
        )
        .bind(folder_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("list subtree files: {e}")))?;
        Ok(rows)
    }

    async fn delete_folder_permanently(&self, folder_id: &str) -> Result<(), DomainError> {
        // Delete all files whose folder is anywhere in the subtree
        // (GiST ltree index, same pattern as delete_folder — both
        // statements retried on deadlock vs the tree-ETag flusher).
        retry_on_deadlock("folders.perm_delete_files", || {
            sqlx::query(
                "DELETE FROM storage.files \
                  WHERE folder_id IN ( \
                      SELECT id FROM storage.folders \
                       WHERE lpath <@ (SELECT lpath FROM storage.folders WHERE id = $1::uuid) \
                  )",
            )
            .bind(folder_id)
            .execute(self.pool())
        })
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("perm delete files: {e}")))?;

        // Then permanently delete folder — CASCADE handles descendant folders
        let result = retry_on_deadlock("folders.perm_delete", || {
            sqlx::query("DELETE FROM storage.folders WHERE id = $1::uuid")
                .bind(folder_id)
                .execute(self.pool())
        })
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("perm delete: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(DomainError::not_found("Folder", folder_id));
        }
        Ok(())
    }

    /// Lists every folder in a subtree rooted at `folder_id` (inclusive).
    ///
    /// Single GiST-indexed query: `fo.lpath <@ (root's lpath)`.
    /// Ordered by `fo.path` so callers can iterate in directory order.
    #[allow(clippy::type_complexity)]
    async fn list_subtree_folders(&self, folder_id: &str) -> Result<Vec<Folder>, DomainError> {
        let sql = "SELECT fo.id, fo.name, fo.path, fo.parent_id, \
                          fo.drive_id, \
                          EXTRACT(EPOCH FROM fo.created_at)::bigint, \
                          EXTRACT(EPOCH FROM fo.updated_at)::bigint, \
                          EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint, \
                        fo.created_by, fo.updated_by \
                     FROM storage.folders fo \
                    WHERE fo.is_trashed = false \
                      AND fo.lpath <@ (SELECT lpath FROM storage.folders WHERE id = $1::uuid) \
                    ORDER BY fo.path";

        let rows: Vec<FolderRow> = sqlx::query_as(sql)
            .bind(folder_id)
            .fetch_all(self.pool())
            .await
            .map_err(|e| {
                DomainError::internal_error("FolderDb", format!("subtree folders: {e}"))
            })?;

        rows.into_iter()
            .map(|(id, name, path, pid, did, ca, ma, tma, cb, ub)| {
                Self::row_to_folder(id, name, path, pid, did, ca, ma, tma, cb, ub)
            })
            .collect()
    }

    /// SQL-level folder search with name filter, user isolation, and
    /// recursive / non-recursive modes.
    ///
    /// - Non-recursive: `WHERE parent_id = $1 AND user_id = $2 [AND LIKE]`
    /// - Recursive + folder_id: delegates to `list_descendant_folders`
    /// - Recursive + no folder_id: drive-scoped `EXISTS role_grants` [AND LIKE]
    ///
    /// Post-PR-B: filters by drive-membership grants inline (via
    /// `caller_group_ids`) instead of `user_id = $caller`.
    #[allow(clippy::type_complexity)]
    async fn search_folders(
        &self,
        parent_id: Option<&str>,
        name_contains: Option<&str>,
        caller_id: Uuid,
        recursive: bool,
    ) -> Result<(Vec<Folder>, Vec<(bool, bool)>), DomainError> {
        // Recursive with folder scope → existing optimised ltree scan.
        // `list_descendant_folders` doesn't compute caller_flags today —
        // return `(false, false)` per row until the ltree path is
        // upgraded in a follow-up. Bounded UI impact: subtree-scoped
        // searches rarely surface a specific folder as favorited /
        // shared, and the flags path elsewhere fills the gap for the
        // hot `/api/search` case.
        if recursive && let Some(fid) = parent_id {
            let folders = self
                .list_descendant_folders(fid, name_contains, caller_id)
                .await?;
            let flags = vec![(false, false); folders.len()];
            return Ok((folders, flags));
        }

        // Build optional name filter — use ILIKE (case-insensitive) so the
        // GIN trigram index idx_folders_name_trgm is used instead of a seq scan.
        let (name_clause, name_pattern) = match name_contains {
            Some(name) if name.len() >= 3 => (
                if recursive {
                    " AND fo.name ILIKE $2"
                } else {
                    " AND fo.name ILIKE $3"
                },
                Some(super::like_escape(name)),
            ),
            _ => ("", None),
        };

        // Per-row caller-flag EXISTS subqueries. Same pattern as
        // `search_files_paginated` in the sibling file repo: narrow
        // indexes make this a few extra μs per row.
        const FAV_SHR_COLUMNS: &str = "EXISTS ( \
                    SELECT 1 FROM auth.user_favorites uf \
                     WHERE uf.user_id   = $1 \
                       AND uf.item_id   = fo.id::text \
                       AND uf.item_type = 'folder' \
                ) AS is_favorite, \
                EXISTS ( \
                    SELECT 1 FROM storage.role_grants g \
                     WHERE g.resource_id   = fo.id \
                       AND g.resource_type = 'folder' \
                ) AS is_shared";

        if recursive {
            // Recursive, no folder scope → ALL folders in caller's readable drives
            let sql = format!(
                "SELECT fo.id, fo.name, fo.path, fo.parent_id, \
                        fo.drive_id, \
                        EXTRACT(EPOCH FROM fo.created_at)::bigint, \
                        EXTRACT(EPOCH FROM fo.updated_at)::bigint, \
                          EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint, \
                      fo.created_by, fo.updated_by, \
                      {FAV_SHR_COLUMNS} \
                   FROM storage.folders fo \
                  WHERE {CALLER_CAN_READ_DRIVE} \
                    AND fo.is_trashed = false \
                    {name_clause} \
                  ORDER BY fo.name"
            );

            let rows: Vec<FolderRowWithFlags> = if let Some(ref pattern) = name_pattern {
                sqlx::query_as(&sql)
                    .bind(caller_id)
                    .bind(pattern)
                    .fetch_all(self.pool())
                    .await
            } else {
                sqlx::query_as(&sql)
                    .bind(caller_id)
                    .fetch_all(self.pool())
                    .await
            }
            .map_err(|e| DomainError::internal_error("FolderDb", format!("search_folders: {e}")))?;

            return build_folders_with_flags(rows);
        }

        // Non-recursive: direct children of parent_id, restricted to drives
        // the caller can read (parent_id already establishes the subtree).
        let sql = if parent_id.is_some() {
            format!(
                "SELECT fo.id, fo.name, fo.path, fo.parent_id, \
                        fo.drive_id, \
                        EXTRACT(EPOCH FROM fo.created_at)::bigint, \
                        EXTRACT(EPOCH FROM fo.updated_at)::bigint, \
                          EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint, \
                      fo.created_by, fo.updated_by, \
                      {FAV_SHR_COLUMNS} \
                   FROM storage.folders fo \
                  WHERE fo.parent_id = $2::uuid \
                    AND {CALLER_CAN_READ_DRIVE} \
                    AND fo.is_trashed = false \
                    {name_clause} \
                  ORDER BY fo.name"
            )
        } else {
            // Root folders: parent_id IS NULL, params ($1=caller_id, $2=pattern)
            let name_clause_root = match name_contains {
                Some(name) if name.len() >= 3 => " AND fo.name ILIKE $2",
                _ => "",
            };
            format!(
                "SELECT fo.id, fo.name, fo.path, fo.parent_id, \
                        fo.drive_id, \
                        EXTRACT(EPOCH FROM fo.created_at)::bigint, \
                        EXTRACT(EPOCH FROM fo.updated_at)::bigint, \
                          EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint, \
                      fo.created_by, fo.updated_by, \
                      {FAV_SHR_COLUMNS} \
                   FROM storage.folders fo \
                  WHERE fo.parent_id IS NULL \
                    AND {CALLER_CAN_READ_DRIVE} \
                    AND fo.is_trashed = false \
                    {name_clause_root} \
                  ORDER BY fo.name"
            )
        };

        let rows: Vec<FolderRowWithFlags> = if let Some(pid) = parent_id {
            if let Some(ref pattern) = name_pattern {
                sqlx::query_as(&sql)
                    .bind(caller_id)
                    .bind(pid)
                    .bind(pattern)
                    .fetch_all(self.pool())
                    .await
            } else {
                sqlx::query_as(&sql)
                    .bind(caller_id)
                    .bind(pid)
                    .fetch_all(self.pool())
                    .await
            }
        } else if let Some(ref pattern) = name_pattern {
            sqlx::query_as(&sql)
                .bind(caller_id)
                .bind(pattern)
                .fetch_all(self.pool())
                .await
        } else {
            sqlx::query_as(&sql)
                .bind(caller_id)
                .fetch_all(self.pool())
                .await
        }
        .map_err(|e| DomainError::internal_error("FolderDb", format!("search_folders: {e}")))?;

        build_folders_with_flags(rows)
    }

    /// Lists all descendant folders in a subtree using ltree GiST index,
    /// scoped to drives the caller can read.
    ///
    /// Single SQL query: `fo.lpath <@ (root's lpath)` fetches the entire
    /// subtree in one indexed scan. Post-PR-B: drive-membership filter
    /// inline via `caller_group_ids`, replacing the legacy
    /// `fo.user_id = $caller` predicate.
    #[allow(clippy::type_complexity)]
    async fn list_descendant_folders(
        &self,
        folder_id: &str,
        name_contains: Option<&str>,
        caller_id: Uuid,
    ) -> Result<Vec<Folder>, DomainError> {
        let (where_extra, name_pattern) = match name_contains {
            Some(name) if name.len() >= 3 => {
                (" AND fo.name ILIKE $3", Some(super::like_escape(name)))
            }
            _ => ("", None),
        };

        let sql = format!(
            "SELECT fo.id, fo.name, fo.path, fo.parent_id, \
                    fo.drive_id, \
                    EXTRACT(EPOCH FROM fo.created_at)::bigint, \
                    EXTRACT(EPOCH FROM fo.updated_at)::bigint, \
                    EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint, \
                    fo.created_by, fo.updated_by \
               FROM storage.folders fo \
              WHERE {CALLER_CAN_READ_DRIVE} \
                AND fo.is_trashed = false \
                AND fo.lpath <@ (SELECT lpath FROM storage.folders WHERE id = $2::uuid) \
                AND fo.id != $2::uuid \
                {where_extra} \
              ORDER BY fo.name"
        );

        let rows: Vec<FolderRow> = if let Some(ref pattern) = name_pattern {
            sqlx::query_as(&sql)
                .bind(caller_id)
                .bind(folder_id)
                .bind(pattern)
                .fetch_all(self.pool())
                .await
        } else {
            sqlx::query_as(&sql)
                .bind(caller_id)
                .bind(folder_id)
                .fetch_all(self.pool())
                .await
        }
        .map_err(|e| DomainError::internal_error("FolderDb", format!("descendant search: {e}")))?;

        rows.into_iter()
            .map(|(id, name, path, pid, did, ca, ma, tma, cb, ub)| {
                Self::row_to_folder(id, name, path, pid, did, ca, ma, tma, cb, ub)
            })
            .collect()
    }

    #[allow(clippy::type_complexity)]
    async fn suggest_folders_by_name(
        &self,
        parent_id: Option<&str>,
        query: &str,
        limit: usize,
        caller_id: uuid::Uuid,
    ) -> Result<Vec<Folder>, DomainError> {
        // Same drive-scope filter as `suggest_files_by_name` — closed as
        // AuthZ audit finding #1 (2026-07-12). `CALLER_CAN_READ_DRIVE`
        // aliases `storage.folders` as `fo`; the pre-fix query aliased it
        // as an unqualified `storage.folders`, so this rewrite adds the
        // `fo` alias in every branch.
        let pattern = super::like_escape(query);
        let limit_i64 = limit as i64;

        let rows: Vec<FolderRow> = if let Some(pid) = parent_id {
            sqlx::query_as(&format!(
                r#"
                SELECT fo.id, fo.name, fo.path, fo.parent_id, fo.drive_id,
                       EXTRACT(EPOCH FROM fo.created_at)::bigint,
                       EXTRACT(EPOCH FROM fo.updated_at)::bigint,
                       EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint,
                         fo.created_by, fo.updated_by
                  FROM storage.folders fo
                 WHERE {CALLER_CAN_READ_DRIVE}
                   AND fo.parent_id = $2::uuid
                   AND NOT fo.is_trashed
                   AND fo.name ILIKE $3
                 ORDER BY CASE
                            WHEN fo.name ILIKE $4 THEN 0
                            WHEN fo.name ILIKE $4 || '%' THEN 1
                            ELSE 2
                          END,
                          fo.name
                 LIMIT $5
                "#
            ))
            .bind(caller_id)
            .bind(pid)
            .bind(&pattern)
            .bind(query)
            .bind(limit_i64)
            .fetch_all(self.pool())
            .await
        } else {
            sqlx::query_as(&format!(
                r#"
                SELECT fo.id, fo.name, fo.path, fo.parent_id, fo.drive_id,
                       EXTRACT(EPOCH FROM fo.created_at)::bigint,
                       EXTRACT(EPOCH FROM fo.updated_at)::bigint,
                       EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint,
                         fo.created_by, fo.updated_by
                  FROM storage.folders fo
                 WHERE {CALLER_CAN_READ_DRIVE}
                   AND fo.parent_id IS NULL
                   AND NOT fo.is_trashed
                   AND fo.name ILIKE $2
                 ORDER BY CASE
                            WHEN fo.name ILIKE $3 THEN 0
                            WHEN fo.name ILIKE $3 || '%' THEN 1
                            ELSE 2
                          END,
                          fo.name
                 LIMIT $4
                "#
            ))
            .bind(caller_id)
            .bind(&pattern)
            .bind(query)
            .bind(limit_i64)
            .fetch_all(self.pool())
            .await
        }
        .map_err(|e| DomainError::internal_error("FolderDb", format!("suggest: {e}")))?;

        rows.into_iter()
            .map(|(id, name, path, pid, did, ca, ma, tma, cb, ub)| {
                Self::row_to_folder(id, name, path, pid, did, ca, ma, tma, cb, ub)
            })
            .collect()
    }

    async fn is_folder_in_subtree(
        &self,
        candidate_folder_id: &str,
        root_folder_id: &str,
    ) -> Result<bool, DomainError> {
        let (Ok(candidate_uuid), Ok(root_uuid)) = (
            Uuid::parse_str(candidate_folder_id),
            Uuid::parse_str(root_folder_id),
        ) else {
            return Ok(false);
        };

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM storage.folders c, storage.folders r \
                 WHERE c.id = $1 \
                   AND r.id = $2 \
                   AND c.is_trashed = false \
                   AND r.is_trashed = false \
                   AND c.lpath <@ r.lpath \
             )",
        )
        .bind(candidate_uuid)
        .bind(root_uuid)
        .fetch_one(self.pool())
        .await
        .map_err(|e| {
            DomainError::internal_error("FolderDb", format!("is_folder_in_subtree: {e}"))
        })?;
        Ok(exists)
    }

    async fn is_file_in_subtree(
        &self,
        file_id: &str,
        root_folder_id: &str,
    ) -> Result<bool, DomainError> {
        let (Ok(file_uuid), Ok(root_uuid)) =
            (Uuid::parse_str(file_id), Uuid::parse_str(root_folder_id))
        else {
            return Ok(false);
        };

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM storage.files f \
                 JOIN storage.folders parent ON f.folder_id = parent.id \
                 JOIN storage.folders root   ON root.id = $2 \
                 WHERE f.id = $1 \
                   AND f.is_trashed = false \
                   AND parent.is_trashed = false \
                   AND root.is_trashed = false \
                   AND parent.lpath <@ root.lpath \
             )",
        )
        .bind(file_uuid)
        .bind(root_uuid)
        .fetch_one(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("is_file_in_subtree: {e}")))?;
        Ok(exists)
    }
}

// ── Extra helpers for blob-storage bootstrap ──

impl FolderDbRepository {
    /// Returns `drive_id` for a given folder. Drives the new permission-floor
    /// short-circuit in `PgAclEngine::check_inner` (a caller with any role
    /// on the folder's drive automatically passes the check — drive
    /// membership is the baseline floor per `drive.md §5`).
    pub async fn get_folder_drive_id(&self, folder_id: &str) -> Result<Uuid, DomainError> {
        sqlx::query_scalar::<_, Uuid>("SELECT drive_id FROM storage.folders WHERE id = $1::uuid")
            .bind(folder_id)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| DomainError::internal_error("FolderDb", format!("drive_id lookup: {e}")))?
            .ok_or_else(|| DomainError::not_found("Folder", folder_id))
    }

    /// Raw ancestor row returned by the recursive walk. `depth` is 0 at
    /// the leaf, growing as we move up. `has_drive_grant` / `has_folder_grant`
    /// are the two Read predicates the service uses to identify the
    /// share/drive boundary and choose the access-source kind.
    pub async fn fetch_ancestor_walk(
        &self,
        caller_id: Uuid,
        leaf_id: Uuid,
    ) -> Result<Vec<AncestorRow>, DomainError> {
        // Recursive CTE walks `parent_id` from the leaf upward. Group ids
        // are hoisted into a one-row CTE so `caller_group_ids($1)` fires
        // once per query instead of per ancestor row (perf: the function
        // is `RECURSIVE` and non-trivial). Grant EXISTS are unions over
        // user + group subjects; the drive-grant subquery matches the
        // ambient `CALLER_CAN_READ_DRIVE` predicate used elsewhere so
        // access decisions stay consistent across the repo.
        let sql = r#"
            WITH RECURSIVE
            groups AS (
                SELECT ARRAY(SELECT storage.caller_group_ids($1)) AS ids
            ),
            chain AS (
                SELECT id, name, parent_id, drive_id, 0::int AS depth
                  FROM storage.folders WHERE id = $2::uuid
                UNION ALL
                SELECT f.id, f.name, f.parent_id, f.drive_id, c.depth + 1
                  FROM storage.folders f
                  JOIN chain c ON f.id = c.parent_id
                 WHERE c.parent_id IS NOT NULL
                   AND c.depth < 64
            )
            SELECT
                c.id,
                c.name,
                c.parent_id,
                c.drive_id,
                c.depth,
                EXISTS (
                    SELECT 1 FROM storage.role_grants g, groups
                     WHERE g.resource_type = 'folder'
                       AND g.resource_id = c.id
                       AND (g.expires_at IS NULL OR g.expires_at > NOW())
                       AND ((g.subject_type = 'user'  AND g.subject_id = $1)
                         OR (g.subject_type = 'group' AND g.subject_id = ANY(groups.ids)))
                ) AS has_folder_grant,
                EXISTS (
                    SELECT 1 FROM storage.role_grants g, groups
                     WHERE g.resource_type = 'drive'
                       AND g.resource_id = c.drive_id
                       AND (g.expires_at IS NULL OR g.expires_at > NOW())
                       AND ((g.subject_type = 'user'  AND g.subject_id = $1)
                         OR (g.subject_type = 'group' AND g.subject_id = ANY(groups.ids)))
                ) AS has_drive_grant
              FROM chain c
             ORDER BY c.depth DESC
        "#;
        sqlx::query_as::<_, AncestorRow>(sql)
            .bind(caller_id)
            .bind(leaf_id)
            .fetch_all(self.pool())
            .await
            .map_err(|e| DomainError::internal_error("FolderDb", format!("ancestor walk: {e}")))
    }

    /// Drive header (`id + name + kind`) for the drive-source arm of
    /// `AccessSourceDto`. Read-only; no authz gate — the caller already
    /// proved drive-membership via the ancestor walk before invoking.
    ///
    /// Drive name lives on the drive's root folder, not the drive row
    /// itself (`docs/plan/drive.md §3`). The JOIN resolves it; a drive
    /// with a NULL `root_folder_id` returns None (backfill invariant
    /// violation — surfaced as "drive vanished mid-query" in the caller).
    pub async fn fetch_drive_header(
        &self,
        drive_id: Uuid,
    ) -> Result<Option<(Uuid, String, String)>, DomainError> {
        sqlx::query_as::<_, (Uuid, String, String)>(
            "SELECT d.id, fo.name, d.kind::text \
               FROM storage.drives d \
               JOIN storage.folders fo ON fo.id = d.root_folder_id \
              WHERE d.id = $1::uuid",
        )
        .bind(drive_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("FolderDb", format!("drive header lookup: {e}")))
    }

    /// Cursor-paginated combined listing of sub-folders and files inside
    /// `parent_id`, sorted by `order_by`.
    ///
    /// **Authorization must be verified by the caller** before invoking this
    /// method — no ownership filter is applied here.
    ///
    /// Fetches `limit` rows (caller should pass `desired_page_size + 1` to
    /// detect the existence of a next page).  Returns raw [`FolderResourceRow`]
    /// values; the handler / service layer converts them to DTOs.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_resources_paged(
        &self,
        parent_id: Uuid,
        caller_id: Uuid,
        limit: usize,
        cursor: Option<&FolderResourceCursor>,
        order_by: &str,
        kinds: Option<&[ResourceKind]>,
        reverse: bool,
    ) -> Result<Vec<FolderResourceRow>, DomainError> {
        let include_folders = kinds.is_none_or(|k| k.contains(&ResourceKind::Folder));
        let include_files = kinds.is_none_or(|k| k.contains(&ResourceKind::File));

        if !include_folders && !include_files {
            return Ok(Vec::new());
        }

        // ── CTE branches ────────────────────────────────────────────────────
        let folder_branch = r#"
            SELECT
                'folder'::text            AS resource_type,
                f.id,
                f.name,
                f.parent_id               AS folder_id,
                NULL::text                AS mime_type,
                -1::bigint                AS size,
                f.created_at,
                f.updated_at              AS modified_at,
                f.drive_id,
                NULL::text                AS blob_hash,
                f.created_by,
                f.updated_by,
                EXISTS (
                    SELECT 1 FROM auth.user_favorites uf
                     WHERE uf.user_id   = $7::uuid
                       AND uf.item_id   = f.id::text
                       AND uf.item_type = 'folder'
                )                         AS is_favorite,
                EXISTS (
                    SELECT 1 FROM storage.role_grants g
                     WHERE g.resource_id   = f.id
                       AND g.resource_type = 'folder'
                )                         AS is_shared,
                LOWER(f.name)             AS sort_str,
                0::bigint                 AS type_order,
                0::int                    AS folder_first
            FROM storage.folders f
            WHERE f.parent_id = $1::uuid AND NOT f.is_trashed
        "#;

        let file_branch = r#"
            SELECT
                'file'::text              AS resource_type,
                fm.id,
                fm.name,
                fm.folder_id,
                fm.mime_type,
                fm.size::bigint,
                fm.created_at,
                fm.updated_at             AS modified_at,
                fm.drive_id,
                fm.blob_hash,
                fm.created_by,
                fm.updated_by,
                EXISTS (
                    SELECT 1 FROM auth.user_favorites uf
                     WHERE uf.user_id   = $7::uuid
                       AND uf.item_id   = fm.id::text
                       AND uf.item_type = 'file'
                )                         AS is_favorite,
                EXISTS (
                    SELECT 1 FROM storage.role_grants g
                     WHERE g.resource_id   = fm.id
                       AND g.resource_type = 'file'
                )                         AS is_shared,
                LOWER(fm.name)            AS sort_str,
                fm.category_order::bigint AS type_order,
                1::int                    AS folder_first
            FROM storage.files fm
            WHERE fm.folder_id = $1::uuid AND NOT fm.is_trashed
        "#;

        // ── Cursor binds ─────────────────────────────────────────────────────
        // $1 = parent_id   $2 = cursor_str   $3 = cursor_int
        // $4 = cursor_ts   $5 = cursor_id    $6 = limit
        let cursor_str = cursor.and_then(|c| c.sort_str.clone());
        let cursor_int = cursor.and_then(|c| c.sort_int);
        let cursor_ts = cursor.and_then(|c| c.sort_ts);
        let cursor_id = cursor.map(|c| c.resource_id);

        // ── Per-branch cursor pushdown ───────────────────────────────────────
        // The cursor is applied INSIDE each UNION-ALL branch as a sargable
        // row-value comparison on base columns — not on the CTE's computed
        // columns — and every branch pre-sorts and pre-limits, so Postgres
        // reads O(limit) rows per branch instead of rescanning and
        // top-N-sorting the entire folder on every page (19.5x on a
        // 20k-entry folder, benches/LISTING-KEYSET.md). The "name" sort is
        // served by the expression indexes idx_files_folder_lname /
        // idx_folders_parent_lname (migration 20260918000000).
        //
        // Sort-key columns that are CONSTANT within a branch (folder_first,
        // the folder branch's type_order = 0 and size = -1) are folded in
        // Rust: depending on which group the cursor points into, the branch
        // predicate shortens to a row-value over the remaining keys, the
        // branch keeps all its rows, or the branch drops out entirely.
        enum BranchCursor {
            /// The cursor has moved past every row this branch can produce.
            Drop,
            /// Every row in this branch sorts after the cursor.
            All,
            /// Row-value comparison over the branch's non-constant sort keys.
            Pred(String),
        }
        use BranchCursor::{All, Drop, Pred};

        let has_cursor = cursor.is_some();
        // (folder-branch cursor, file-branch cursor, per-branch ORDER BY on
        //  the branch's output aliases, outer merge ORDER BY)
        let (folder_cur, file_cur, branch_order, outer_order) = match order_by {
            "type" => {
                let (op, ord) = if reverse {
                    ("<", "ORDER BY type_order DESC, sort_str DESC, id DESC")
                } else {
                    (">", "ORDER BY type_order ASC, sort_str ASC, id ASC")
                };
                let folder_cur = match cursor_int {
                    None => All,
                    // Folder rows have type_order = 0; a cursor sitting on a
                    // file (type_order > 0) either exhausts the folder group
                    // (ASC) or precedes all of it (DESC).
                    Some(c_to) if c_to > 0 => {
                        if reverse {
                            All
                        } else {
                            Drop
                        }
                    }
                    Some(_) => Pred(format!("(LOWER(f.name), f.id) {op} ($2, $5::uuid)")),
                };
                let file_cur = if has_cursor {
                    Pred(format!(
                        "(fm.category_order::bigint, LOWER(fm.name), fm.id) {op} ($3, $2, $5::uuid)"
                    ))
                } else {
                    All
                };
                (folder_cur, file_cur, ord, ord)
            }
            "modified_at" => {
                let (op, ord) = if reverse {
                    (">", "ORDER BY modified_at ASC, id ASC")
                } else {
                    ("<", "ORDER BY modified_at DESC, id DESC")
                };
                let mk = |col: &str| {
                    if has_cursor {
                        Pred(format!("({col}.updated_at, {col}.id) {op} ($4, $5::uuid)"))
                    } else {
                        All
                    }
                };
                (mk("f"), mk("fm"), ord, ord)
            }
            "created_at" => {
                let (op, ord) = if reverse {
                    (">", "ORDER BY created_at ASC, id ASC")
                } else {
                    ("<", "ORDER BY created_at DESC, id DESC")
                };
                let mk = |col: &str| {
                    if has_cursor {
                        Pred(format!("({col}.created_at, {col}.id) {op} ($4, $5::uuid)"))
                    } else {
                        All
                    }
                };
                (mk("f"), mk("fm"), ord, ord)
            }
            "size" => {
                let (op, ord) = if reverse {
                    ("<", "ORDER BY size DESC, id DESC")
                } else {
                    (">", "ORDER BY size ASC, id ASC")
                };
                let folder_cur = match cursor_int {
                    None => All,
                    // Folder rows have size = -1; a cursor sitting on a file
                    // (size >= 0) exhausts the folder group (ASC) or precedes
                    // all of it (DESC).
                    Some(c_sz) if c_sz > -1 => {
                        if reverse {
                            All
                        } else {
                            Drop
                        }
                    }
                    Some(_) => Pred(format!("f.id {op} $5::uuid")),
                };
                let file_cur = if has_cursor {
                    Pred(format!("(fm.size::bigint, fm.id) {op} ($3, $5::uuid)"))
                } else {
                    All
                };
                (folder_cur, file_cur, ord, ord)
            }
            _ => {
                // "name" (default): folder_first stays ASC so folders always
                // precede files; only the alpha order within each group flips
                // when reversed. cursor_int carries folder_first (0|1).
                let op = if reverse { "<" } else { ">" };
                let branch_ord = if reverse {
                    "ORDER BY sort_str DESC, id DESC"
                } else {
                    "ORDER BY sort_str ASC, id ASC"
                };
                let outer_ord = if reverse {
                    "ORDER BY folder_first ASC, sort_str DESC, id DESC"
                } else {
                    "ORDER BY folder_first ASC, sort_str ASC, id ASC"
                };
                let (folder_cur, file_cur) = match cursor_int {
                    None => (All, All),
                    // Cursor inside the folder group: folders continue after
                    // the row-value cursor; every file still follows.
                    Some(0) => (
                        Pred(format!("(LOWER(f.name), f.id) {op} ($2, $5::uuid)")),
                        All,
                    ),
                    // Cursor inside the file group: the folder group is done.
                    Some(_) => (
                        Drop,
                        Pred(format!("(LOWER(fm.name), fm.id) {op} ($2, $5::uuid)")),
                    ),
                };
                (folder_cur, file_cur, branch_ord, outer_ord)
            }
        };

        let wrap = |branch: &str, cur: &BranchCursor| -> Option<String> {
            let extra = match cur {
                Drop => return None,
                All => String::new(),
                Pred(p) => format!(" AND {p}"),
            };
            Some(format!(
                "(SELECT * FROM ({branch}{extra}) b {branch_order} LIMIT $6)"
            ))
        };
        let mut branches = Vec::with_capacity(2);
        if include_folders && let Some(b) = wrap(folder_branch, &folder_cur) {
            branches.push(b);
        }
        if include_files && let Some(b) = wrap(file_branch, &file_cur) {
            branches.push(b);
        }
        // Every requested branch dropped out (e.g. folders-only listing with
        // the cursor already past the folder group).
        if branches.is_empty() {
            return Ok(Vec::new());
        }
        let inner = branches.join(" UNION ALL ");

        let sql = format!(
            "SELECT resource_type, id, name, folder_id, mime_type, size, \
                    created_at, modified_at, drive_id, blob_hash, \
                    created_by, updated_by, \
                    is_favorite, is_shared, \
                    sort_str, type_order, folder_first \
             FROM ({inner}) r \
             {outer_order} \
             LIMIT $6"
        );

        // 17 columns exceed sqlx's 16-element tuple `FromRow` limit,
        // so we can't use `query_as::<_, (T1..T17)>` directly. Instead
        // we fetch raw `PgRow`s and decode each column positionally
        // into `FolderResourceRow` via `try_get_unchecked(idx)`. This
        // matches Dio's positional shape (index decode, no per-row
        // column-name HashMap) AND skips the intermediate tuple
        // struct + row-to-struct move that a manual `FromRow` impl
        // would introduce — one construction, one destination.
        //
        // Column indices below MUST match the outer `SELECT` list
        // above (resource_type, id, name, folder_id, mime_type, size,
        // created_at, modified_at, drive_id, blob_hash, created_by,
        // updated_by, is_favorite, is_shared, sort_str, type_order,
        // folder_first).
        let rows = sqlx::query(&sql)
            .bind(parent_id)
            .bind(cursor_str)
            .bind(cursor_int)
            .bind(cursor_ts)
            .bind(cursor_id)
            .bind(limit as i64)
            .bind(caller_id)
            .fetch_all(self.pool())
            .await
            .map_err(|e| {
                DomainError::internal_error("FolderDb", format!("list_resources_paged: {e}"))
            })?;

        use sqlx::Row as _;
        let decode_err = |col: usize, e: sqlx::Error| -> DomainError {
            DomainError::internal_error(
                "FolderDb",
                format!("list_resources_paged decode col {col}: {e}"),
            )
        };

        rows.into_iter()
            .map(|r| {
                Ok(FolderResourceRow {
                    resource_type: r.try_get_unchecked(0).map_err(|e| decode_err(0, e))?,
                    id: r.try_get_unchecked(1).map_err(|e| decode_err(1, e))?,
                    name: r.try_get_unchecked(2).map_err(|e| decode_err(2, e))?,
                    parent_id: r.try_get_unchecked(3).map_err(|e| decode_err(3, e))?,
                    mime_type: r.try_get_unchecked(4).map_err(|e| decode_err(4, e))?,
                    size: r.try_get_unchecked(5).map_err(|e| decode_err(5, e))?,
                    created_at: r.try_get_unchecked(6).map_err(|e| decode_err(6, e))?,
                    modified_at: r.try_get_unchecked(7).map_err(|e| decode_err(7, e))?,
                    drive_id: r.try_get_unchecked(8).map_err(|e| decode_err(8, e))?,
                    blob_hash: r.try_get_unchecked(9).map_err(|e| decode_err(9, e))?,
                    created_by: r.try_get_unchecked(10).map_err(|e| decode_err(10, e))?,
                    updated_by: r.try_get_unchecked(11).map_err(|e| decode_err(11, e))?,
                    is_favorite: r.try_get_unchecked(12).map_err(|e| decode_err(12, e))?,
                    is_shared: r.try_get_unchecked(13).map_err(|e| decode_err(13, e))?,
                    sort_str: r.try_get_unchecked(14).map_err(|e| decode_err(14, e))?,
                    type_order: r.try_get_unchecked(15).map_err(|e| decode_err(15, e))?,
                    folder_first: r.try_get_unchecked(16).map_err(|e| decode_err(16, e))?,
                })
            })
            .collect()
    }
}
