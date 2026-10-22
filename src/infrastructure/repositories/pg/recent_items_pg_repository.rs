use sqlx::{PgPool, Row};
use std::sync::Arc;
use tracing::error;
use uuid::Uuid;

use crate::application::dtos::favorites_dto::ItemKind;
use crate::application::dtos::recent_dto::{
    RecentCursor, RecentItemDto, RecentOrderBy, RecentResourceRow,
};
use crate::application::ports::recent_ports::RecentItemsRepositoryPort;
use crate::common::errors::{DomainError, ErrorKind, Result};
use crate::domain::services::authorization::ResourceKind;

/// PostgreSQL implementation of the recent items persistence port.
pub struct RecentItemsPgRepository {
    db_pool: Arc<PgPool>,
}

impl RecentItemsPgRepository {
    pub fn new(db_pool: Arc<PgPool>) -> Self {
        Self { db_pool }
    }
}

impl RecentItemsRepositoryPort for RecentItemsPgRepository {
    async fn get_recent_items(&self, user_id: Uuid, limit: i32) -> Result<Vec<RecentItemDto>> {
        // Binary UUID decode + app-side render (ROUND6 §10 pattern) — no
        // server-side `::TEXT` casts, 16 B per id on the wire instead of 36.
        let rows = sqlx::query(
            r#"
            SELECT
                ur.id                                           AS "id",
                ur.user_id                                      AS "user_id",
                ur.item_id                                      AS "item_id",
                ur.item_type                                    AS "item_type",
                ur.accessed_at                                  AS "accessed_at",
                COALESCE(f.name, fld.name)                      AS "item_name",
                f.size                                          AS "item_size",
                f.mime_type                                     AS "item_mime_type",
                COALESCE(f.folder_id, fld.parent_id)            AS "parent_id",
                CASE
                    WHEN ur.item_type = 'folder' THEN fld.path
                    WHEN ur.item_type = 'file'   THEN COALESCE(pfld.path || '/' || f.name, f.name)
                    ELSE NULL
                END                                             AS "item_path"
            FROM auth.user_recent_files ur
            LEFT JOIN storage.files   f   ON ur.item_type = 'file'
                                         AND f.id = ur.item_id::UUID
            LEFT JOIN storage.folders pfld ON ur.item_type = 'file'
                                          AND pfld.id = f.folder_id
            LEFT JOIN storage.folders fld ON ur.item_type = 'folder'
                                         AND fld.id = ur.item_id::UUID
            WHERE ur.user_id = $1
            ORDER BY ur.accessed_at DESC
            LIMIT $2
            "#,
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error fetching recent items: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "RecentItems",
                format!("Failed to fetch recent items: {}", e),
            )
        })?;

        // Mapper returns `Result<_>` so a corrupt `item_type` propagates
        // out via `?`. The CHECK constraint on `recent_items.item_type`
        // means this is unreachable in practice.
        let items: Vec<RecentItemDto> = rows
            .iter()
            .map(|row| -> Result<RecentItemDto> {
                let item_type: ItemKind =
                    row.get::<String, _>("item_type").parse().map_err(|e| {
                        DomainError::new(
                            ErrorKind::InternalError,
                            "RecentItems",
                            format!("corrupt recent_items.item_type: {e}"),
                        )
                    })?;
                Ok(RecentItemDto {
                    id: row.get::<i32, _>("id").to_string(),
                    user_id: row.get::<Uuid, _>("user_id").to_string(),
                    item_id: row.get("item_id"),
                    item_type,
                    accessed_at: row.get("accessed_at"),
                    item_name: row.try_get("item_name").ok(),
                    item_size: row.try_get("item_size").ok(),
                    item_mime_type: row.try_get("item_mime_type").ok(),
                    parent_id: row
                        .try_get::<Option<Uuid>, _>("parent_id")
                        .ok()
                        .flatten()
                        .map(|u| u.to_string()),
                    item_path: row.try_get("item_path").ok(),
                    // Temporary defaults; with_display_fields() computes the real values
                    icon_class: String::new(),
                    category: String::new(),
                    size_formatted: String::new(),
                }
                .with_display_fields())
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(items)
    }

    async fn upsert_access(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool> {
        // `xmax = 0` on the affected row is the canonical upsert idiom for
        // "this was an INSERT, not a DO UPDATE" — lets the caller skip the
        // prune round-trip on the common re-access (UPDATE) path
        // (benches/ROUND13.md §Q3).
        let inserted: bool = sqlx::query_scalar(
            r#"
            INSERT INTO auth.user_recent_files (user_id, item_id, item_type, accessed_at)
            VALUES ($1, $2, $3, CURRENT_TIMESTAMP)
            ON CONFLICT (user_id, item_id, item_type)
            DO UPDATE SET accessed_at = CURRENT_TIMESTAMP
            RETURNING (xmax = 0)
            "#,
        )
        .bind(user_id)
        .bind(item_id)
        .bind(item_type)
        .fetch_one(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error upserting recent item access: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "RecentItems",
                format!("Failed to record item access: {}", e),
            )
        })?;

        Ok(inserted)
    }

    async fn remove_item(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"
            DELETE FROM auth.user_recent_files
            WHERE user_id = $1 AND item_id = $2 AND item_type = $3
            "#,
        )
        .bind(user_id)
        .bind(item_id)
        .bind(item_type)
        .execute(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error removing recent item: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "RecentItems",
                format!("Failed to remove recent item: {}", e),
            )
        })?;

        Ok(result.rows_affected() > 0)
    }

    async fn clear_all(&self, user_id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            DELETE FROM auth.user_recent_files
            WHERE user_id = $1
            "#,
        )
        .bind(user_id)
        .execute(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error clearing recent items: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "RecentItems",
                format!("Failed to clear recent items: {}", e),
            )
        })?;

        Ok(())
    }

    async fn prune(&self, user_id: Uuid, max_items: i32) -> Result<()> {
        sqlx::query(
            r#"
            DELETE FROM auth.user_recent_files
            WHERE id IN (
                SELECT id FROM auth.user_recent_files
                WHERE user_id = $1
                ORDER BY accessed_at DESC
                OFFSET $2
            )
            "#,
        )
        .bind(user_id)
        .bind(max_items)
        .execute(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error pruning old recent items: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "RecentItems",
                format!("Failed to prune recent items: {}", e),
            )
        })?;

        Ok(())
    }

    async fn list_resources_paged(
        &self,
        user_id: Uuid,
        limit: usize,
        cursor: Option<&RecentCursor>,
        order_by: RecentOrderBy,
        kinds: Option<&[ResourceKind]>,
        reverse: bool,
    ) -> Result<Vec<RecentResourceRow>> {
        let include_folders =
            kinds.is_none_or(|k| k.iter().any(|r| matches!(r, ResourceKind::Folder)));
        let include_files = kinds.is_none_or(|k| k.iter().any(|r| matches!(r, ResourceKind::File)));

        // ── Build the UNION ALL CTE ─────────────────────────────────────────
        let mut cte_branches: Vec<&str> = Vec::new();

        // Post-D7: `is_owner` means "the caller holds an Owner
        // role_grant on the drive owning this row". Personal drives:
        // the single-owner invariant makes this trivially true for the
        // owner and false for anyone else. Shared drives: multiple
        // Owners possible; each of them gets `true`. Used only to gate
        // whether the handler exposes the full path (path-hierarchy
        // hiding for share recipients — see `recent_handler.rs`).
        //
        // The `created_by` projection is separate — §14 provenance,
        // used for the "Owner" column and the owner sort's username
        // JOIN. The two signals genuinely differ post-D2: e.g. Bob
        // (Editor on Alice's shared drive) making a file has
        // `created_by = Bob` but `is_owner = false` because Alice owns
        // the drive.
        let folder_branch = r#"
    SELECT
        'folder'::text                   AS resource_type,
        fld.id                           AS resource_id,
        fld.name,
        fld.parent_id,
        NULL::text                       AS mime_type,
        -1::bigint                       AS size,
        fld.created_at                   AS resource_created_at,
        fld.content_modified_at,
        fld.drive_id                     AS drive_id,
        NULL::text                       AS blob_hash,
        fld.created_by                   AS created_by,
        fld.updated_by                   AS updated_by,
        EXISTS (
            SELECT 1 FROM auth.user_favorites uf
             WHERE uf.user_id   = $1::uuid
               AND uf.item_id   = fld.id::text
               AND uf.item_type = 'folder'
        )                                AS is_favorite,
        EXISTS (
            SELECT 1 FROM storage.role_grants g
             WHERE g.resource_id   = fld.id
               AND g.resource_type = 'folder'
        )                                AS is_shared,
        EXISTS (
            SELECT 1 FROM storage.role_grants g
             WHERE g.resource_type = 'drive'
               AND g.resource_id   = fld.drive_id
               AND g.role          = 'owner'
               AND g.subject_type  = 'user'
               AND g.subject_id    = $1::uuid
               AND (g.expires_at IS NULL OR g.expires_at > NOW())
        )                                AS is_owner,
        ur.accessed_at                   AS accessed_at,
        fld.path::text                   AS resource_path,
        LOWER(fld.name)                  AS sort_str,
        0::bigint                        AS type_order,
        0::int                           AS folder_first
    FROM auth.user_recent_files ur
    INNER JOIN storage.folders fld
           ON fld.id = ur.item_id::UUID AND NOT fld.is_trashed
    WHERE ur.user_id = $1::uuid AND ur.item_type = 'folder'"#;

        let file_branch = r#"
    SELECT
        'file'::text                     AS resource_type,
        f.id                             AS resource_id,
        f.name,
        f.folder_id                      AS parent_id,
        f.mime_type,
        f.size::bigint,
        f.created_at                     AS resource_created_at,
        f.content_modified_at,
        f.drive_id                       AS drive_id,
        f.blob_hash,
        f.created_by                     AS created_by,
        f.updated_by                     AS updated_by,
        EXISTS (
            SELECT 1 FROM auth.user_favorites uf
             WHERE uf.user_id   = $1::uuid
               AND uf.item_id   = f.id::text
               AND uf.item_type = 'file'
        )                                AS is_favorite,
        EXISTS (
            SELECT 1 FROM storage.role_grants g
             WHERE g.resource_id   = f.id
               AND g.resource_type = 'file'
        )                                AS is_shared,
        EXISTS (
            SELECT 1 FROM storage.role_grants g
             WHERE g.resource_type = 'drive'
               AND g.resource_id   = f.drive_id
               AND g.role          = 'owner'
               AND g.subject_type  = 'user'
               AND g.subject_id    = $1::uuid
               AND (g.expires_at IS NULL OR g.expires_at > NOW())
        )                                AS is_owner,
        ur.accessed_at                   AS accessed_at,
        COALESCE(pfld.path::text || '/' || f.name, f.name) AS resource_path,
        LOWER(f.name)                    AS sort_str,
        f.category_order::bigint         AS type_order,
        1::int                           AS folder_first
    FROM auth.user_recent_files ur
    INNER JOIN storage.files f
           ON f.id = ur.item_id::UUID AND NOT f.is_trashed
    LEFT JOIN storage.folders pfld
           ON pfld.id = f.folder_id
    WHERE ur.user_id = $1::uuid AND ur.item_type = 'file'"#;

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
        // Binds: $1=user_id (in CTE), $2=cur_str, $3=cur_int, $4=cur_ts,
        //        $5=cur_id, $6=limit (for "owner" sort: JOIN uses no extra binds)
        let (keyset, order_by_clause, need_user_join) = match (order_by, reverse) {
            // ── name ────────────────────────────────────────────────────────
            (RecentOrderBy::Name, false) => (
                "WHERE ($3::bigint IS NULL)
                    OR (folder_first::bigint > $3)
                    OR (folder_first::bigint = $3 AND sort_str > $2)
                    OR (folder_first::bigint = $3 AND sort_str = $2 AND resource_id > $5::uuid)",
                "ORDER BY folder_first ASC, sort_str ASC, resource_id ASC",
                false,
            ),
            (RecentOrderBy::Name, true) => (
                "WHERE ($3::bigint IS NULL)
                    OR (folder_first::bigint > $3)
                    OR (folder_first::bigint = $3 AND sort_str < $2)
                    OR (folder_first::bigint = $3 AND sort_str = $2 AND resource_id < $5::uuid)",
                "ORDER BY folder_first ASC, sort_str DESC, resource_id DESC",
                false,
            ),
            // ── type ────────────────────────────────────────────────────────
            (RecentOrderBy::Type, false) => (
                "WHERE ($3::bigint IS NULL)
                    OR (type_order > $3)
                    OR (type_order = $3 AND sort_str > $2)
                    OR (type_order = $3 AND sort_str = $2 AND resource_id > $5::uuid)",
                "ORDER BY type_order ASC, sort_str ASC, resource_id ASC",
                false,
            ),
            (RecentOrderBy::Type, true) => (
                "WHERE ($3::bigint IS NULL)
                    OR (type_order < $3)
                    OR (type_order = $3 AND sort_str < $2)
                    OR (type_order = $3 AND sort_str = $2 AND resource_id < $5::uuid)",
                "ORDER BY type_order DESC, sort_str DESC, resource_id DESC",
                false,
            ),
            // ── accessed_at ──────────────────────────────────────────────────
            (RecentOrderBy::AccessedAt, false) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (accessed_at < $4)
                    OR (accessed_at = $4 AND resource_id < $5::uuid)",
                "ORDER BY accessed_at DESC, resource_id DESC",
                false,
            ),
            (RecentOrderBy::AccessedAt, true) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (accessed_at > $4)
                    OR (accessed_at = $4 AND resource_id > $5::uuid)",
                "ORDER BY accessed_at ASC, resource_id ASC",
                false,
            ),
            // ── modified_at ──────────────────────────────────────────────────
            (RecentOrderBy::ModifiedAt, false) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (content_modified_at < $4)
                    OR (content_modified_at = $4 AND resource_id < $5::uuid)",
                "ORDER BY content_modified_at DESC, resource_id DESC",
                false,
            ),
            (RecentOrderBy::ModifiedAt, true) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (content_modified_at > $4)
                    OR (content_modified_at = $4 AND resource_id > $5::uuid)",
                "ORDER BY content_modified_at ASC, resource_id ASC",
                false,
            ),
            // ── size ─────────────────────────────────────────────────────────
            (RecentOrderBy::Size, false) => (
                "WHERE ($3::bigint IS NULL)
                    OR (size > $3)
                    OR (size = $3 AND resource_id > $5::uuid)",
                "ORDER BY size ASC, resource_id ASC",
                false,
            ),
            (RecentOrderBy::Size, true) => (
                "WHERE ($3::bigint IS NULL)
                    OR (size < $3)
                    OR (size = $3 AND resource_id < $5::uuid)",
                "ORDER BY size DESC, resource_id DESC",
                false,
            ),
            // ── owner ────────────────────────────────────────────────────────
            (RecentOrderBy::Owner, false) => (
                "WHERE ($2::text IS NULL)
                    OR (LOWER(u.username) > $2)
                    OR (LOWER(u.username) = $2 AND accessed_at < $4)
                    OR (LOWER(u.username) = $2 AND accessed_at = $4 AND resource_id < $5::uuid)",
                "ORDER BY LOWER(u.username) ASC, accessed_at DESC, resource_id DESC",
                true,
            ),
            (RecentOrderBy::Owner, true) => (
                "WHERE ($2::text IS NULL)
                    OR (LOWER(u.username) < $2)
                    OR (LOWER(u.username) = $2 AND accessed_at > $4)
                    OR (LOWER(u.username) = $2 AND accessed_at = $4 AND resource_id > $5::uuid)",
                "ORDER BY LOWER(u.username) DESC, accessed_at ASC, resource_id ASC",
                true,
            ),
            // §7: no catch-all — all six `RecentOrderBy` variants are
            // named above, so this match is exhaustive and a new axis is a
            // compile error rather than silently ordering by accessed_at
            // while the caller's cursor carries a different axis.
        };

        let user_join = if need_user_join {
            // Post-D7: `owner_id` column retired; use `created_by`
            // (§14 provenance) as the "owner" identity for the sort.
            "LEFT JOIN auth.users u ON u.id = r.created_by"
        } else {
            ""
        };
        // For "owner" sort the JOIN makes LOWER(u.username) available; add it to SELECT
        // so the cursor can carry the correct sort key.
        let username_col = if need_user_join {
            ",\n    LOWER(u.username)                AS username_lower"
        } else {
            ""
        };

        let sql = format!(
            "{cte}
SELECT
    r.resource_type, r.resource_id, r.name, r.parent_id,
    r.mime_type, r.size, r.resource_created_at, r.content_modified_at,
    r.drive_id, r.blob_hash, r.created_by, r.updated_by,
    r.is_favorite, r.is_shared,
    r.is_owner, r.accessed_at, r.resource_path,
    r.sort_str, r.type_order, r.folder_first{username_col}
FROM resources r
{user_join}
{keyset}
{order_by_clause}
LIMIT $6"
        );

        let rows = sqlx::query(&sql)
            .bind(user_id) // $1 (in CTE + outer)
            .bind(cur_str) // $2
            .bind(cur_int) // $3
            .bind(cur_ts) // $4
            .bind(cur_id) // $5
            .bind(limit as i64) // $6
            .fetch_all(&*self.db_pool)
            .await
            .map_err(|e| {
                error!("Database error listing recent resources: {e}");
                DomainError::new(
                    ErrorKind::InternalError,
                    "RecentItems",
                    format!("Failed to list recent resources: {e}"),
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

                // Pre-compute the cursor sort fields based on order_by
                let (c_sort_str, c_sort_int, c_sort_ts) = match order_by {
                    RecentOrderBy::Name => (sort_str_val, Some(folder_first as i64), None),
                    RecentOrderBy::Type => (sort_str_val, Some(type_order), None),
                    RecentOrderBy::Size => (None, Some(size), None),
                    RecentOrderBy::AccessedAt => {
                        let ts: Option<chrono::DateTime<chrono::Utc>> =
                            row.try_get("accessed_at").ok();
                        (None, None, ts)
                    }
                    RecentOrderBy::ModifiedAt => {
                        let ts: Option<chrono::DateTime<chrono::Utc>> =
                            row.try_get("content_modified_at").ok();
                        (None, None, ts)
                    }
                    RecentOrderBy::Owner => {
                        // For "owner" sort the JOIN added LOWER(u.username) AS username_lower.
                        // The cursor's sort_str must carry the username (not the file name).
                        let username: Option<String> = row.try_get("username_lower").ok();
                        let ts: Option<chrono::DateTime<chrono::Utc>> =
                            row.try_get("accessed_at").ok();
                        (username, None, ts)
                    }
                };

                RecentResourceRow {
                    resource_type,
                    resource_id: row.get("resource_id"),
                    name: row.get("name"),
                    parent_id: row.try_get("parent_id").ok(),
                    mime_type: row.try_get("mime_type").ok(),
                    size,
                    resource_created_at: row.get("resource_created_at"),
                    modified_at: row.get("content_modified_at"),
                    drive_id: row.get("drive_id"),
                    blob_hash: row.try_get("blob_hash").ok(),
                    created_by: row.try_get("created_by").ok(),
                    updated_by: row.try_get("updated_by").ok(),
                    is_favorite: row.try_get("is_favorite").unwrap_or(false),
                    is_shared: row.try_get("is_shared").unwrap_or(false),
                    is_owner: row.try_get("is_owner").unwrap_or(false),
                    accessed_at: row.get("accessed_at"),
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
