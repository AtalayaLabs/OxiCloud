use sqlx::{PgPool, Row};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::error;
use uuid::Uuid;

use crate::application::dtos::favorites_dto::{
    FavoriteItemDto, FavoriteResourceRow, FavoritesCursor, FavoritesOrderBy,
};
use crate::application::ports::favorites_ports::FavoritesRepositoryPort;
use crate::common::errors::{DomainError, ErrorKind, Result};
use crate::domain::services::authorization::ResourceKind;

/// PostgreSQL implementation of the favorites persistence port.
pub struct FavoritesPgRepository {
    db_pool: Arc<PgPool>,
}

impl FavoritesPgRepository {
    pub fn new(db_pool: Arc<PgPool>) -> Self {
        Self { db_pool }
    }
}

impl FavoritesRepositoryPort for FavoritesPgRepository {
    async fn get_favorites(&self, user_id: Uuid) -> Result<Vec<FavoriteItemDto>> {
        // `id`/`user_id`/`parent_id` decode as binary UUIDs (16 B on the wire,
        // no server-side `::TEXT` cast) and render app-side — the ROUND6 §10
        // pattern the two legacy listing methods here never picked up.
        let rows = sqlx::query(
            r#"
            SELECT
                uf.id                                           AS "id",
                uf.user_id                                      AS "user_id",
                uf.item_id                                      AS "item_id",
                uf.item_type                                    AS "item_type",
                uf.created_at                                   AS "created_at",
                COALESCE(f.name, fld.name)                      AS "item_name",
                f.size                                          AS "item_size",
                f.mime_type                                     AS "item_mime_type",
                COALESCE(f.folder_id, fld.parent_id)            AS "parent_id",
                COALESCE(f.content_modified_at, fld.content_modified_at) AS "content_modified_at",
                CASE
                    WHEN uf.item_type = 'folder' THEN fld.path
                    WHEN uf.item_type = 'file'   THEN COALESCE(pfld.path || '/' || f.name, f.name)
                    ELSE NULL
                END                                             AS "item_path"
            FROM auth.user_favorites uf
            LEFT JOIN storage.files   f   ON uf.item_type = 'file'
                                         AND f.id = uf.item_id::UUID
            LEFT JOIN storage.folders pfld ON uf.item_type = 'file'
                                          AND pfld.id = f.folder_id
            LEFT JOIN storage.folders fld ON uf.item_type = 'folder'
                                         AND fld.id = uf.item_id::UUID
            WHERE uf.user_id = $1
            ORDER BY uf.created_at DESC
            LIMIT 500
            "#,
        )
        .bind(user_id)
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error fetching favorites: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "Favorites",
                format!("Failed to fetch favorites: {}", e),
            )
        })?;

        // Mapper returns `Result<_>` so a corrupt `item_type` propagates
        // out via `?` into the outer `collect::<Result<Vec<_>>>()`. The
        // CHECK constraint on the column means this is unreachable in
        // practice — the explicit error is a belt-and-braces for the
        // "DB migrated by hand" failure mode.
        let favorites: Vec<FavoriteItemDto> = rows
            .iter()
            .map(|row| -> Result<FavoriteItemDto> {
                let item_type = row.get::<String, _>("item_type").parse().map_err(|e| {
                    DomainError::new(
                        ErrorKind::InternalError,
                        "Favorites",
                        format!("corrupt favorites.item_type: {e}"),
                    )
                })?;
                Ok(FavoriteItemDto {
                    id: row.get::<i32, _>("id").to_string(),
                    user_id: row.get::<Uuid, _>("user_id").to_string(),
                    item_id: row.get("item_id"),
                    item_type,
                    created_at: row.get("created_at"),
                    item_name: row.try_get("item_name").ok(),
                    item_size: row.try_get("item_size").ok(),
                    item_mime_type: row.try_get("item_mime_type").ok(),
                    parent_id: row
                        .try_get::<Option<Uuid>, _>("parent_id")
                        .ok()
                        .flatten()
                        .map(|u| u.to_string()),
                    modified_at: row.try_get("content_modified_at").ok(),
                    item_path: row.try_get("item_path").ok(),
                    // Temporary defaults; with_display_fields() computes the real values
                    category: String::new(),
                }
                .with_display_fields())
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(favorites)
    }

    async fn add_favorite(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO auth.user_favorites (user_id, item_id, item_type)
            VALUES ($1, $2, $3)
            ON CONFLICT (user_id, item_id, item_type) DO NOTHING
            "#,
        )
        .bind(user_id)
        .bind(item_id)
        .bind(item_type)
        .execute(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error adding favorite: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "Favorites",
                format!("Failed to add to favorites: {}", e),
            )
        })?;

        Ok(())
    }

    async fn remove_favorite(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"
            DELETE FROM auth.user_favorites
            WHERE user_id = $1 AND item_id = $2 AND item_type = $3
            "#,
        )
        .bind(user_id)
        .bind(item_id)
        .bind(item_type)
        .execute(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error removing favorite: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "Favorites",
                format!("Failed to remove from favorites: {}", e),
            )
        })?;

        Ok(result.rows_affected() > 0)
    }

    async fn is_favorite(&self, user_id: Uuid, item_id: &str, item_type: &str) -> Result<bool> {
        let row = sqlx::query(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM auth.user_favorites
                WHERE user_id = $1 AND item_id = $2 AND item_type = $3
            ) AS "is_favorite"
            "#,
        )
        .bind(user_id)
        .bind(item_id)
        .bind(item_type)
        .fetch_one(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error checking favorite status: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "Favorites",
                format!("Failed to check favorite status: {}", e),
            )
        })?;

        Ok(row.try_get("is_favorite").unwrap_or(false))
    }

    async fn add_favorites_batch(&self, user_id: Uuid, items: &[(String, String)]) -> Result<u64> {
        if items.is_empty() {
            return Ok(0);
        }

        // Validate all item_types upfront
        for (_, item_type) in items {
            if item_type != "file" && item_type != "folder" {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "Favorites",
                    format!("Item type must be 'file' or 'folder', got '{}'", item_type),
                ));
            }
        }

        // Build a multi-row INSERT with ON CONFLICT DO NOTHING
        // Using a single transaction for atomicity
        let mut tx = self.db_pool.begin().await.map_err(|e| {
            error!("Database error starting transaction: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "Favorites",
                format!("Failed to start transaction: {}", e),
            )
        })?;

        let mut total_inserted: u64 = 0;

        // Insert in chunks to stay within Postgres' parameter limit (max ~32k params)
        for chunk in items.chunks(5000) {
            let mut query = String::from(
                "INSERT INTO auth.user_favorites (user_id, item_id, item_type) VALUES ",
            );
            let mut param_idx = 1u32;
            let mut first = true;

            for _ in chunk {
                if !first {
                    query.push_str(", ");
                }
                query.push_str(&format!(
                    "(${}, ${}, ${})",
                    param_idx,
                    param_idx + 1,
                    param_idx + 2
                ));
                param_idx += 3;
                first = false;
            }
            query.push_str(" ON CONFLICT (user_id, item_id, item_type) DO NOTHING");

            let mut q = sqlx::query(&query);
            for (item_id, item_type) in chunk {
                q = q.bind(user_id).bind(item_id).bind(item_type);
            }

            let result = q.execute(&mut *tx).await.map_err(|e| {
                error!("Database error in batch insert favorites: {}", e);
                DomainError::new(
                    ErrorKind::InternalError,
                    "Favorites",
                    format!("Failed to batch insert favorites: {}", e),
                )
            })?;

            total_inserted += result.rows_affected();
        }

        tx.commit().await.map_err(|e| {
            error!("Database error committing batch favorites: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "Favorites",
                format!("Failed to commit batch favorites: {}", e),
            )
        })?;

        Ok(total_inserted)
    }

    async fn batch_check_favorites(
        &self,
        user_id: Uuid,
        item_ids: &[(&str, &str)],
    ) -> Result<HashSet<String>> {
        if item_ids.is_empty() {
            return Ok(HashSet::new());
        }

        // Collect just the IDs for the IN clause — sqlx binds `&[&str]` as
        // text[], so no per-id String is needed.
        let ids: Vec<&str> = item_ids.iter().map(|(id, _)| *id).collect();

        let rows = sqlx::query(
            "SELECT item_id FROM auth.user_favorites WHERE user_id = $1 AND item_id = ANY($2)",
        )
        .bind(user_id)
        .bind(&ids)
        .fetch_all(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error batch-checking favorites: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "Favorites",
                format!("Failed to batch-check favorites: {}", e),
            )
        })?;

        Ok(rows.iter().map(|r| r.get::<String, _>("item_id")).collect())
    }

    async fn caller_flags(
        &self,
        caller_id: Uuid,
        resource_type: &str,
        resource_id: Uuid,
    ) -> Result<(bool, bool)> {
        // One round trip, two per-row EXISTS in the SELECT. Both hit
        // covering indexes: `auth.user_favorites` UNIQUE on
        // `(user_id, item_id, item_type)` and
        // `idx_role_grants_resource` on `(resource_type, resource_id)`.
        // Sub-millisecond on hot data.
        //
        // Positional tuple decode (Dio's pattern) — no per-column
        // name lookup, no HashMap. Sqlx's 2-tuple `query_as` decodes
        // by index, matching the perf shape used in the
        // folder_db_repository listing hot path.
        let (is_favorite, is_shared): (bool, bool) = sqlx::query_as(
            "SELECT \
                EXISTS ( \
                    SELECT 1 FROM auth.user_favorites \
                     WHERE user_id = $1 \
                       AND item_id = $2::text \
                       AND item_type = $3 \
                ), \
                EXISTS ( \
                    SELECT 1 FROM storage.role_grants \
                     WHERE resource_id = $2 \
                       AND resource_type = $3 \
                )",
        )
        .bind(caller_id)
        .bind(resource_id)
        .bind(resource_type)
        .fetch_one(&*self.db_pool)
        .await
        .map_err(|e| {
            error!("Database error running caller_flags: {}", e);
            DomainError::new(
                ErrorKind::InternalError,
                "CallerFlags",
                format!("Failed to compute caller flags: {}", e),
            )
        })?;

        Ok((is_favorite, is_shared))
    }

    async fn list_resources_paged(
        &self,
        user_id: Uuid,
        limit: usize,
        cursor: Option<&FavoritesCursor>,
        order_by: FavoritesOrderBy,
        kinds: Option<&[ResourceKind]>,
        reverse: bool,
    ) -> Result<Vec<FavoriteResourceRow>> {
        let include_folders =
            kinds.is_none_or(|k| k.iter().any(|r| matches!(r, ResourceKind::Folder)));
        let include_files = kinds.is_none_or(|k| k.iter().any(|r| matches!(r, ResourceKind::File)));

        // ── Build the UNION ALL CTE ─────────────────────────────────────────
        let mut cte_branches: Vec<&str> = Vec::new();

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
        -- Every row on this feed IS a favorite by construction —
        -- hardcode the flag to skip a per-row EXISTS.
        TRUE                             AS is_favorite,
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
        uf.created_at                    AS favorited_at,
        fld.path::text                   AS resource_path,
        LOWER(fld.name)                  AS sort_str,
        0::bigint                        AS type_order,
        0::int                           AS folder_first
    FROM auth.user_favorites uf
    INNER JOIN storage.folders fld
           ON fld.id = uf.item_id::UUID AND NOT fld.is_trashed
    WHERE uf.user_id = $1::uuid AND uf.item_type = 'folder'"#;

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
        -- Every row on this feed IS a favorite by construction.
        TRUE                             AS is_favorite,
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
        uf.created_at                    AS favorited_at,
        COALESCE(pfld.path::text || '/' || f.name, f.name) AS resource_path,
        LOWER(f.name)                    AS sort_str,
        f.category_order::bigint         AS type_order,
        1::int                           AS folder_first
    FROM auth.user_favorites uf
    INNER JOIN storage.files f
           ON f.id = uf.item_id::UUID AND NOT f.is_trashed
    LEFT JOIN storage.folders pfld
           ON pfld.id = f.folder_id
    WHERE uf.user_id = $1::uuid AND uf.item_type = 'file'"#;

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
            (FavoritesOrderBy::Name, false) => (
                "WHERE ($3::bigint IS NULL)
                    OR (folder_first::bigint > $3)
                    OR (folder_first::bigint = $3 AND sort_str > $2)
                    OR (folder_first::bigint = $3 AND sort_str = $2 AND resource_id > $5::uuid)",
                "ORDER BY folder_first ASC, sort_str ASC, resource_id ASC",
                false,
            ),
            (FavoritesOrderBy::Name, true) => (
                "WHERE ($3::bigint IS NULL)
                    OR (folder_first::bigint > $3)
                    OR (folder_first::bigint = $3 AND sort_str < $2)
                    OR (folder_first::bigint = $3 AND sort_str = $2 AND resource_id < $5::uuid)",
                "ORDER BY folder_first ASC, sort_str DESC, resource_id DESC",
                false,
            ),
            // ── type ────────────────────────────────────────────────────────
            (FavoritesOrderBy::Type, false) => (
                "WHERE ($3::bigint IS NULL)
                    OR (type_order > $3)
                    OR (type_order = $3 AND sort_str > $2)
                    OR (type_order = $3 AND sort_str = $2 AND resource_id > $5::uuid)",
                "ORDER BY type_order ASC, sort_str ASC, resource_id ASC",
                false,
            ),
            (FavoritesOrderBy::Type, true) => (
                "WHERE ($3::bigint IS NULL)
                    OR (type_order < $3)
                    OR (type_order = $3 AND sort_str < $2)
                    OR (type_order = $3 AND sort_str = $2 AND resource_id < $5::uuid)",
                "ORDER BY type_order DESC, sort_str DESC, resource_id DESC",
                false,
            ),
            // ── favorited_at ─────────────────────────────────────────────────
            (FavoritesOrderBy::FavoritedAt, false) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (favorited_at < $4)
                    OR (favorited_at = $4 AND resource_id < $5::uuid)",
                "ORDER BY favorited_at DESC, resource_id DESC",
                false,
            ),
            (FavoritesOrderBy::FavoritedAt, true) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (favorited_at > $4)
                    OR (favorited_at = $4 AND resource_id > $5::uuid)",
                "ORDER BY favorited_at ASC, resource_id ASC",
                false,
            ),
            // ── modified_at ──────────────────────────────────────────────────
            (FavoritesOrderBy::ModifiedAt, false) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (content_modified_at < $4)
                    OR (content_modified_at = $4 AND resource_id < $5::uuid)",
                "ORDER BY content_modified_at DESC, resource_id DESC",
                false,
            ),
            (FavoritesOrderBy::ModifiedAt, true) => (
                "WHERE ($4::timestamptz IS NULL)
                    OR (content_modified_at > $4)
                    OR (content_modified_at = $4 AND resource_id > $5::uuid)",
                "ORDER BY content_modified_at ASC, resource_id ASC",
                false,
            ),
            // ── size ─────────────────────────────────────────────────────────
            (FavoritesOrderBy::Size, false) => (
                "WHERE ($3::bigint IS NULL)
                    OR (size > $3)
                    OR (size = $3 AND resource_id > $5::uuid)",
                "ORDER BY size ASC, resource_id ASC",
                false,
            ),
            (FavoritesOrderBy::Size, true) => (
                "WHERE ($3::bigint IS NULL)
                    OR (size < $3)
                    OR (size = $3 AND resource_id < $5::uuid)",
                "ORDER BY size DESC, resource_id DESC",
                false,
            ),
            // ── owner ────────────────────────────────────────────────────────
            (FavoritesOrderBy::Owner, false) => (
                "WHERE ($2::text IS NULL)
                    OR (LOWER(u.username) > $2)
                    OR (LOWER(u.username) = $2 AND favorited_at < $4)
                    OR (LOWER(u.username) = $2 AND favorited_at = $4 AND resource_id < $5::uuid)",
                "ORDER BY LOWER(u.username) ASC, favorited_at DESC, resource_id DESC",
                true,
            ),
            (FavoritesOrderBy::Owner, true) => (
                "WHERE ($2::text IS NULL)
                    OR (LOWER(u.username) < $2)
                    OR (LOWER(u.username) = $2 AND favorited_at > $4)
                    OR (LOWER(u.username) = $2 AND favorited_at = $4 AND resource_id > $5::uuid)",
                "ORDER BY LOWER(u.username) DESC, favorited_at ASC, resource_id ASC",
                true,
            ),
            // §7: no catch-all arm — all six `FavoritesOrderBy` variants
            // are named above, so this match is exhaustive and a new axis
            // is a compile error here rather than silently ordering by
            // name while the caller's cursor carries a different axis.
        };

        let user_join = if need_user_join {
            // Post-D7: `owner_id` retired; join by `created_by`.
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
    r.is_owner, r.favorited_at, r.resource_path,
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
                error!("Database error listing favorite resources: {e}");
                DomainError::new(
                    ErrorKind::InternalError,
                    "Favorites",
                    format!("Failed to list favorite resources: {e}"),
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
                    FavoritesOrderBy::Name => (sort_str_val, Some(folder_first as i64), None),
                    FavoritesOrderBy::Type => (sort_str_val, Some(type_order), None),
                    FavoritesOrderBy::Size => (None, Some(size), None),
                    FavoritesOrderBy::FavoritedAt => {
                        let ts: Option<chrono::DateTime<chrono::Utc>> =
                            row.try_get("favorited_at").ok();
                        (None, None, ts)
                    }
                    FavoritesOrderBy::ModifiedAt => {
                        let ts: Option<chrono::DateTime<chrono::Utc>> =
                            row.try_get("content_modified_at").ok();
                        (None, None, ts)
                    }
                    FavoritesOrderBy::Owner => {
                        // For "owner" sort the JOIN added LOWER(u.username) AS username_lower.
                        // The cursor's sort_str must carry the username (not the file name).
                        let username: Option<String> = row.try_get("username_lower").ok();
                        let ts: Option<chrono::DateTime<chrono::Utc>> =
                            row.try_get("favorited_at").ok();
                        (username, None, ts)
                    }
                };

                FavoriteResourceRow {
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
                    is_favorite: row.try_get("is_favorite").unwrap_or(true),
                    is_shared: row.try_get("is_shared").unwrap_or(false),
                    is_owner: row.try_get("is_owner").unwrap_or(false),
                    favorited_at: row.get("favorited_at"),
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
