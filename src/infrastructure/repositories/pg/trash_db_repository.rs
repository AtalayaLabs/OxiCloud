//! PostgreSQL-backed trash repository.
//!
//! Implements `TrashRepository` using soft-delete columns in `storage.files`
//! and `storage.folders`.  There is no separate trash table — trashed items
//! are files/folders with `is_trashed = TRUE`.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tracing::error;
use uuid::Uuid;

use crate::application::dtos::trash_dto::{TrashCursor, TrashResourceRow};
use crate::common::errors::{DomainError, ErrorKind, Result};
use crate::domain::entities::trashed_item::{TrashedItem, TrashedItemType};
use crate::domain::repositories::trash_repository::TrashRepository;
use crate::domain::services::authorization::ResourceKind;

/// Default retention period (days) used when computing deletion_date.
const _DEFAULT_RETENTION_DAYS: i64 = 30;

/// PostgreSQL-backed trash repository using soft-delete flags.
pub struct TrashDbRepository {
    pool: Arc<PgPool>,
    retention_days: i64,
}

impl TrashDbRepository {
    pub fn new(pool: Arc<PgPool>, retention_days: u32) -> Self {
        Self {
            pool,
            retention_days: retention_days as i64,
        }
    }

    /// Creates a stub instance for testing — never hits PG.
    #[cfg(test)]
    pub fn new_stub() -> Self {
        Self {
            pool: Arc::new(
                sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                    .max_connections(1)
                    .connect_lazy("postgres://invalid:5432/none")
                    .unwrap(),
            ),
            retention_days: 30,
        }
    }

    /// Runs a LIMIT-ed DELETE statement repeatedly until a round affects
    /// fewer rows than `batch_size`, yielding to the runtime between rounds.
    ///
    /// `sql` must bind `$1` = cutoff timestamp and `$2` = batch size; the
    /// candidate sub-select is served by the `idx_*_trash_expiry` partial
    /// indexes. Each round is its own implicit transaction, so row locks,
    /// WAL volume and the statement-trigger transition tables stay bounded
    /// no matter how many items expired. Partial progress is fine — the
    /// next retention sweep continues where this one stopped.
    async fn delete_expired_batch_loop(
        &self,
        sql: &'static str,
        cutoff: DateTime<Utc>,
        batch_size: i64,
    ) -> Result<u64> {
        let mut total: u64 = 0;
        loop {
            let affected = sqlx::query(sql)
                .bind(cutoff)
                .bind(batch_size)
                .execute(self.pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error("TrashDb", format!("bulk delete batch: {e}"))
                })?
                .rows_affected();
            total += affected;
            if affected < batch_size as u64 {
                return Ok(total);
            }
            tokio::task::yield_now().await;
        }
    }

    /// Convert a trash_items view row into a TrashedItem entity.
    fn row_to_trashed_item(
        &self,
        id: Uuid,
        name: String,
        item_type: String,
        user_id: Uuid,
        trashed_at: Option<DateTime<Utc>>,
        original_path: String,
    ) -> TrashedItem {
        let trashed_at = trashed_at.unwrap_or_else(Utc::now);
        let deletion_date = trashed_at + chrono::Duration::days(self.retention_days);

        let item_type_enum = match item_type.as_str() {
            "folder" => TrashedItemType::Folder,
            _ => TrashedItemType::File,
        };

        // In the soft-delete model, the trash entry ID is the same as the
        // original item ID since there is no separate trash table.
        TrashedItem::from_raw(
            id,      // trash entry id (same as original)
            id,      // original item id
            user_id, // owner
            item_type_enum,
            name.clone(),
            original_path, // parent folder path at time of trash
            trashed_at,
            deletion_date,
        )
    }
}

impl TrashRepository for TrashDbRepository {
    async fn add_to_trash(&self, _item: &TrashedItem) -> Result<()> {
        // No-op: the actual flagging is done by FileWritePort::move_to_trash
        // or FolderRepository::move_to_trash.  This method exists for interface
        // compatibility with the TrashService.
        Ok(())
    }

    async fn get_trash_items(&self, user_id: &Uuid) -> Result<Vec<TrashedItem>> {
        // Post-D7: the `WHERE t.user_id = $1` filter is gone — the
        // `user_id` column was dropped from `storage.{files,folders}`
        // and the view no longer projects it. Scope is drive-membership
        // via role_grants; group memberships expand inline through
        // `storage.caller_group_ids`. Same predicate shape as
        // `list_root_folders_for_caller` / the file listings.
        //
        // Legacy method — the paginated `list_resources_paged` is the
        // modern shape and takes explicit drive_ids from the service
        // layer.
        let rows = sqlx::query_as::<_, (Uuid, String, String, Option<DateTime<Utc>>, String)>(
            r#"
            SELECT t.id, t.name, t.item_type, t.trashed_at,
                   COALESCE(p.path || '/' || t.name, t.name) AS original_path
              FROM storage.trash_items t
              LEFT JOIN storage.folders p ON p.id = t.original_parent_id
             WHERE EXISTS (
                     SELECT 1 FROM storage.role_grants g
                      WHERE g.resource_type = 'drive'
                        AND g.resource_id   = t.drive_id
                        AND (g.expires_at IS NULL OR g.expires_at > NOW())
                        AND (
                              (g.subject_type = 'user'  AND g.subject_id = $1)
                           OR (g.subject_type = 'group' AND g.subject_id IN
                                   (SELECT storage.caller_group_ids($1)))
                            )
                   )
             ORDER BY t.trashed_at DESC
            "#,
        )
        .bind(user_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("TrashDb", format!("list: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|(id, name, item_type, trashed_at, path)| {
                self.row_to_trashed_item(id, name, item_type, *user_id, trashed_at, path)
            })
            .collect())
    }

    async fn get_trash_item(&self, id: &Uuid) -> Result<Option<TrashedItem>> {
        // D2b stage 3: lookup by id only — the `user_id` filter that used to
        // gate this query is replaced by an explicit `authz.require(Delete,
        // …)` in the service callers (`restore_item`, `delete_permanently`).
        // The drive precheck in `pg_acl_engine` then resolves Owner-on-drive
        // → Delete-permission for items in shared drives.
        //
        // Post-D7: `t.user_id` no longer exists — the column is dropped
        // from `storage.{files,folders}` and no longer projected by the
        // view. The entity's `user_id` field is still non-optional;
        // synthesize `Uuid::nil()`. AuthZ decisions don't consult this
        // field — they've already resolved the caller's role on the
        // target's drive.
        let row = sqlx::query_as::<_, (Uuid, String, String, Option<DateTime<Utc>>, String)>(
            r#"
            SELECT t.id, t.name, t.item_type, t.trashed_at,
                   COALESCE(p.path || '/' || t.name, t.name) AS original_path
              FROM storage.trash_items t
              LEFT JOIN storage.folders p ON p.id = t.original_parent_id
             WHERE t.id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("TrashDb", format!("get: {e}")))?;

        Ok(row.map(|(id, name, item_type, trashed_at, path)| {
            self.row_to_trashed_item(id, name, item_type, Uuid::nil(), trashed_at, path)
        }))
    }

    async fn restore_from_trash(&self, _id: &Uuid, _user_id: &Uuid) -> Result<()> {
        // No-op: the actual restore is done by FileWritePort::restore_from_trash
        // or FolderRepository::restore_from_trash.  The TrashService also removes
        // the index entry — which in the soft-delete model means the flag is
        // already cleared.
        Ok(())
    }

    async fn delete_permanently(&self, _id: &Uuid, _user_id: &Uuid) -> Result<()> {
        // No-op: the actual delete is done by FileWritePort::delete_file_permanently
        // or FolderRepository::delete_folder_permanently.
        Ok(())
    }

    async fn clear_trash(&self, drive_ids: &[Uuid]) -> Result<()> {
        if drive_ids.is_empty() {
            return Ok(());
        }
        // Delete all trashed files in the given drives.
        sqlx::query("DELETE FROM storage.files WHERE drive_id = ANY($1) AND is_trashed = TRUE")
            .bind(drive_ids)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("TrashDb", format!("clear files: {e}")))?;

        // Delete all trashed folders in the given drives. FK ON DELETE CASCADE
        // sweeps descendant rows; the `trg_files_decrement_blob_ref` trigger
        // handles blob refcount drops automatically.
        sqlx::query("DELETE FROM storage.folders WHERE drive_id = ANY($1) AND is_trashed = TRUE")
            .bind(drive_ids)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("TrashDb", format!("clear folders: {e}")))?;

        Ok(())
    }

    async fn get_all_trashed_file_ids(&self, drive_ids: &[Uuid]) -> Result<Vec<String>> {
        if drive_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT id::text FROM storage.files WHERE drive_id = ANY($1) AND is_trashed = TRUE",
        )
        .bind(drive_ids)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("TrashDb", format!("all_trashed_files: {e}")))?;
        Ok(rows)
    }

    async fn delete_expired_bulk(&self) -> Result<(u64, u64)> {
        let cutoff = Utc::now() - chrono::Duration::days(self.retention_days);

        // The `read_only` policy on a drive is a compliance-grade freeze:
        // NO state on the drive changes while the policy is on, including
        // background retention. The `JOIN storage.drives d ... AND
        // (d.effective_policies->>'read_only')::boolean IS NOT TRUE` filter excludes
        // frozen drives at SELECT time. Retention clock keeps ticking; on
        // unfreeze, the next sweep tick catches up on anything past its
        // TTL. Legal-hold guarantee documented in `docs/plan/drive.md` §8
        // and `docs/guide/trash.md`.
        //
        // `(policies->>'read_only')::boolean IS NOT TRUE` semantics:
        //   - key missing        → NULL::boolean → IS NOT TRUE → included
        //   - explicit `false`   → FALSE         → IS NOT TRUE → included
        //   - explicit `true`    → TRUE          → IS TRUE     → excluded
        // Correct for both current data (most drives omit the key) and
        // freshly-frozen drives.

        // 1. Bulk-delete expired trashed files in batches.
        //    The PG trigger `trg_files_decrement_blob_ref` automatically
        //    decrements blob ref_count for every deleted row.
        let files_deleted = self
            .delete_expired_batch_loop(
                "DELETE FROM storage.files
                  WHERE id IN (SELECT f.id
                                 FROM storage.files f
                                 JOIN storage.drives_effective d ON d.id = f.drive_id
                                WHERE f.is_trashed = TRUE
                                  AND f.trashed_at < $1
                                  AND (d.effective_policies->>'read_only')::boolean IS NOT TRUE
                                ORDER BY f.trashed_at
                                LIMIT $2)",
                cutoff,
                1_000,
            )
            .await?;

        // 2. Bulk-delete expired trashed folders in batches.
        //    FK ON DELETE CASCADE handles descendant folders and their
        //    files, so each row can fan out to an entire subtree — hence
        //    the smaller batch size. Same read_only exclusion applies:
        //    a subtree rooted in a frozen drive isn't purged even if the
        //    folder's own trashed_at is past retention.
        let folders_deleted = self
            .delete_expired_batch_loop(
                "DELETE FROM storage.folders
                  WHERE id IN (SELECT f.id
                                 FROM storage.folders f
                                 JOIN storage.drives_effective d ON d.id = f.drive_id
                                WHERE f.is_trashed = TRUE
                                  AND f.trashed_at < $1
                                  AND (d.effective_policies->>'read_only')::boolean IS NOT TRUE
                                ORDER BY f.trashed_at
                                LIMIT $2)",
                cutoff,
                100,
            )
            .await?;

        Ok((files_deleted, folders_deleted))
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Cursor-paginated trash listing  (used by GET /api/trash/resources)
// ════════════════════════════════════════════════════════════════════════════
impl TrashDbRepository {
    /// Cursor-paginated list of trashed resources the caller can read.
    ///
    /// D2b: scope is drive-membership-based — pass the set of drive UUIDs
    /// the caller can read (resolved upstream by `DriveRepository::list_for_subjects`
    /// or equivalent). Items in drives outside this set drop out at the
    /// WHERE clause. The legacy `WHERE user_id = $1::uuid` filter is gone;
    /// for single-drive users this returns exactly the same items as before
    /// because the caller's own personal drive is always in `drive_ids`.
    ///
    /// Mirrors the favorites/grants pattern: a UNION-ALL CTE over folder and
    /// file branches (each pre-computing sort columns), then a per-dimension
    /// keyset WHERE + ORDER BY.
    ///
    /// Returns rows in caller-requested sort order. The caller is expected to
    /// fetch `limit + 1` to detect end-of-results. Empty `drive_ids` returns
    /// an empty page without hitting PG.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_resources_paged(
        &self,
        drive_ids: &[Uuid],
        caller_id: Uuid,
        limit: usize,
        cursor: Option<&TrashCursor>,
        order_by: &str,
        kinds: Option<&[ResourceKind]>,
        reverse: bool,
    ) -> Result<Vec<TrashResourceRow>> {
        if drive_ids.is_empty() {
            return Ok(Vec::new());
        }
        let include_folders =
            kinds.is_none_or(|k| k.iter().any(|r| matches!(r, ResourceKind::Folder)));
        let include_files = kinds.is_none_or(|k| k.iter().any(|r| matches!(r, ResourceKind::File)));

        // ── Build the UNION ALL CTE ─────────────────────────────────────────
        // Only top-level trashed items: a file/folder whose parent is itself
        // trashed is implicitly in trash as a descendant, mirroring the
        // `storage.trash_items` view's filter.
        let mut cte_branches: Vec<&str> = Vec::new();

        let folder_branch = r#"
    SELECT
        'folder'::text                                       AS resource_type,
        fld.id                                               AS resource_id,
        fld.name,
        fld.parent_id,
        NULL::text                                           AS mime_type,
        -1::bigint                                           AS size,
        fld.created_at                                       AS resource_created_at,
        fld.updated_at                                       AS modified_at,
        fld.drive_id                                         AS drive_id,
        NULL::text                                           AS blob_hash,
        fld.created_by                                       AS created_by,
        fld.updated_by                                       AS updated_by,
        EXISTS (
            SELECT 1 FROM auth.user_favorites uf
             WHERE uf.user_id   = $8::uuid
               AND uf.item_id   = fld.id::text
               AND uf.item_type = 'folder'
        )                                                    AS is_favorite,
        EXISTS (
            SELECT 1 FROM storage.role_grants g
             WHERE g.resource_id   = fld.id
               AND g.resource_type = 'folder'
        )                                                    AS is_shared,
        fld.trashed_at                                       AS trashed_at,
        (fld.trashed_at + ($7::int * INTERVAL '1 day'))      AS deletion_date,
        fld.path::text                                       AS resource_path,
        LOWER(fld.name)                                      AS sort_str,
        0::bigint                                            AS type_order,
        0::int                                               AS folder_first
    FROM storage.folders fld
    WHERE fld.drive_id = ANY($1)
      AND fld.is_trashed = TRUE
      AND (fld.parent_id IS NULL
           OR NOT EXISTS (
               SELECT 1 FROM storage.folders p
                WHERE p.id = fld.parent_id AND p.is_trashed = TRUE))"#;

        let file_branch = r#"
    SELECT
        'file'::text                                         AS resource_type,
        f.id                                                 AS resource_id,
        f.name,
        f.folder_id                                          AS parent_id,
        f.mime_type,
        f.size::bigint                                       AS size,
        f.created_at                                         AS resource_created_at,
        f.updated_at                                         AS modified_at,
        f.drive_id                                           AS drive_id,
        f.blob_hash,
        f.created_by                                         AS created_by,
        f.updated_by                                         AS updated_by,
        EXISTS (
            SELECT 1 FROM auth.user_favorites uf
             WHERE uf.user_id   = $8::uuid
               AND uf.item_id   = f.id::text
               AND uf.item_type = 'file'
        )                                                    AS is_favorite,
        EXISTS (
            SELECT 1 FROM storage.role_grants g
             WHERE g.resource_id   = f.id
               AND g.resource_type = 'file'
        )                                                    AS is_shared,
        f.trashed_at                                         AS trashed_at,
        (f.trashed_at + ($7::int * INTERVAL '1 day'))        AS deletion_date,
        COALESCE(pfld.path::text || '/' || f.name, f.name)   AS resource_path,
        LOWER(f.name)                                        AS sort_str,
        f.category_order::bigint                             AS type_order,
        1::int                                               AS folder_first
    FROM storage.files f
    LEFT JOIN storage.folders pfld
           ON pfld.id = f.folder_id
    WHERE f.drive_id = ANY($1)
      AND f.is_trashed = TRUE
      AND (f.folder_id IS NULL
           OR NOT EXISTS (
               SELECT 1 FROM storage.folders p
                WHERE p.id = f.folder_id AND p.is_trashed = TRUE))"#;

        if include_folders {
            cte_branches.push(folder_branch);
        }
        if include_files {
            cte_branches.push(file_branch);
        }

        if cte_branches.is_empty() {
            return Ok(Vec::new());
        }

        let union_sql = cte_branches.join("\n    UNION ALL\n");
        let cte = format!("WITH resources AS ({union_sql}\n)");

        // ── Cursor values ───────────────────────────────────────────────────
        let cur_str: Option<&str> = cursor.and_then(|c| c.sort_str.as_deref());
        let cur_int: Option<i64> = cursor.and_then(|c| c.sort_int);
        let cur_ts: Option<chrono::DateTime<chrono::Utc>> = cursor.and_then(|c| c.sort_ts);
        let cur_id: Option<Uuid> = cursor.map(|c| c.resource_id);

        // ── Per-dimension keyset WHERE + ORDER BY ───────────────────────────
        // Binds: $1=user_id, $2=cur_str, $3=cur_int, $4=cur_ts,
        //        $5=cur_id, $6=limit, $7=retention_days.
        let (keyset, order_by_clause) = match (order_by, reverse) {
            // ── deletion_date (DEFAULT) — ASC = expiring soonest first ───────
            ("deletion_date", false) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (deletion_date > $4)
                    OR (deletion_date = $4 AND resource_id > $5::uuid)",
                "ORDER BY deletion_date ASC, resource_id ASC",
            ),
            ("deletion_date", true) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (deletion_date < $4)
                    OR (deletion_date = $4 AND resource_id < $5::uuid)",
                "ORDER BY deletion_date DESC, resource_id DESC",
            ),
            // ── trashed_at — DESC = most recently trashed first ──────────────
            ("trashed_at", false) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (trashed_at < $4)
                    OR (trashed_at = $4 AND resource_id < $5::uuid)",
                "ORDER BY trashed_at DESC, resource_id DESC",
            ),
            ("trashed_at", true) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (trashed_at > $4)
                    OR (trashed_at = $4 AND resource_id > $5::uuid)",
                "ORDER BY trashed_at ASC, resource_id ASC",
            ),
            // ── name — folders first ─────────────────────────────────────────
            ("name", false) => (
                "WHERE ($3::bigint IS NULL)
                    OR (folder_first::bigint > $3)
                    OR (folder_first::bigint = $3 AND sort_str > $2)
                    OR (folder_first::bigint = $3 AND sort_str = $2 AND resource_id > $5::uuid)",
                "ORDER BY folder_first ASC, sort_str ASC, resource_id ASC",
            ),
            ("name", true) => (
                "WHERE ($3::bigint IS NULL)
                    OR (folder_first::bigint > $3)
                    OR (folder_first::bigint = $3 AND sort_str < $2)
                    OR (folder_first::bigint = $3 AND sort_str = $2 AND resource_id < $5::uuid)",
                "ORDER BY folder_first ASC, sort_str DESC, resource_id DESC",
            ),
            // ── type — folders get type_order=0 so they sort first naturally ─
            ("type", false) => (
                "WHERE ($3::bigint IS NULL)
                    OR (type_order > $3)
                    OR (type_order = $3 AND sort_str > $2)
                    OR (type_order = $3 AND sort_str = $2 AND resource_id > $5::uuid)",
                "ORDER BY type_order ASC, sort_str ASC, resource_id ASC",
            ),
            ("type", true) => (
                "WHERE ($3::bigint IS NULL)
                    OR (type_order < $3)
                    OR (type_order = $3 AND sort_str < $2)
                    OR (type_order = $3 AND sort_str = $2 AND resource_id < $5::uuid)",
                "ORDER BY type_order DESC, sort_str DESC, resource_id DESC",
            ),
            // ── size — folders first (via -1 sentinel grouping at top) ──────
            ("size", false) => (
                "WHERE ($3::bigint IS NULL)
                    OR (size > $3)
                    OR (size = $3 AND resource_id > $5::uuid)",
                "ORDER BY size ASC, resource_id ASC",
            ),
            ("size", true) => (
                "WHERE ($3::bigint IS NULL)
                    OR (size < $3)
                    OR (size = $3 AND resource_id < $5::uuid)",
                "ORDER BY size DESC, resource_id DESC",
            ),
            // ── default = deletion_date ASC ─────────────────────────────────
            (_, false) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (deletion_date > $4)
                    OR (deletion_date = $4 AND resource_id > $5::uuid)",
                "ORDER BY deletion_date ASC, resource_id ASC",
            ),
            (_, true) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (deletion_date < $4)
                    OR (deletion_date = $4 AND resource_id < $5::uuid)",
                "ORDER BY deletion_date DESC, resource_id DESC",
            ),
        };

        let sql = format!(
            "{cte}
SELECT
    r.resource_type, r.resource_id, r.name, r.parent_id,
    r.mime_type, r.size, r.resource_created_at, r.modified_at,
    r.drive_id, r.blob_hash, r.created_by, r.updated_by,
    r.is_favorite, r.is_shared,
    r.trashed_at, r.deletion_date, r.resource_path,
    r.sort_str, r.type_order, r.folder_first
FROM resources r
{keyset}
{order_by_clause}
LIMIT $6"
        );

        let rows = sqlx::query(&sql)
            .bind(drive_ids) // $1 (was user_id pre-D2b)
            .bind(cur_str) // $2
            .bind(cur_int) // $3
            .bind(cur_ts) // $4
            .bind(cur_id) // $5
            .bind(limit as i64) // $6
            .bind(self.retention_days as i32) // $7
            .bind(caller_id) // $8 — favorites EXISTS on the branch SELECTs
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| {
                error!("Database error listing trash resources: {e}");
                DomainError::new(
                    ErrorKind::InternalError,
                    "Trash",
                    format!("Failed to list trash resources: {e}"),
                )
            })?;

        let result = rows
            .iter()
            .map(|row| {
                let resource_type: String = row.get("resource_type");
                let sort_str_val: Option<String> = row.try_get("sort_str").ok();
                let type_order: i64 = row.try_get("type_order").unwrap_or(0);
                let folder_first: i32 = row.try_get("folder_first").unwrap_or(0);
                let size: i64 = row.get("size");
                let trashed_at: DateTime<Utc> = row.get("trashed_at");
                let deletion_date: DateTime<Utc> = row.get("deletion_date");

                // Pre-compute the cursor sort fields based on order_by.
                let (c_sort_str, c_sort_int, c_sort_ts) = match order_by {
                    "deletion_date" => (None, None, Some(deletion_date)),
                    "trashed_at" => (None, None, Some(trashed_at)),
                    "name" => (sort_str_val, Some(folder_first as i64), None),
                    "type" => (sort_str_val, Some(type_order), None),
                    "size" => (None, Some(size), None),
                    _ => (None, None, Some(deletion_date)),
                };

                TrashResourceRow {
                    resource_type,
                    resource_id: row.get("resource_id"),
                    name: row.get("name"),
                    parent_id: row.try_get("parent_id").ok(),
                    mime_type: row.try_get("mime_type").ok(),
                    size,
                    resource_created_at: row.get("resource_created_at"),
                    modified_at: row.get("modified_at"),
                    drive_id: row.get("drive_id"),
                    blob_hash: row.try_get("blob_hash").ok(),
                    created_by: row.try_get("created_by").ok(),
                    updated_by: row.try_get("updated_by").ok(),
                    is_favorite: row.try_get("is_favorite").unwrap_or(false),
                    is_shared: row.try_get("is_shared").unwrap_or(false),
                    trashed_at,
                    deletion_date,
                    path: row.try_get("resource_path").ok(),
                    sort_str: c_sort_str,
                    sort_int: c_sort_int,
                    sort_ts: c_sort_ts,
                }
            })
            .collect();

        Ok(result)
    }
}
