//! `GET /api/files/recent-changes` — the read side of content provenance.
//!
//! Design: `docs/plan/recently-modified.md` §3 (access filter) and §4 (the
//! two-step query). The write side lives in `file_blob_write_repository`.
//!
//! ## Two statements, not one
//!
//! The `modifier` axis orders lanes by their own recency, which the obvious
//! single query expresses as `MAX(touched_at) OVER (PARTITION BY user_id)`.
//! That cannot be computed incrementally under a `LIMIT`: Postgres must read
//! every qualifying row in the window, sort by the partition key, aggregate,
//! then sort again before the limit applies — so page 7 costs what page 1
//! costs, proportional to changes-in-window across every accessible drive.
//!
//! So: aggregate the lanes on a three-column projection first
//! ([`lanes`]), then fetch the rows for the lanes this page needs
//! ([`rows_for_lanes`]) by index scan. The `changed_at` axis needs none of
//! this and walks `idx_touches_touched` directly.
//!
//! ## The access filter is a UNION, never an OR
//!
//! Three doors lead to a file — the drive, a grant on the file, a cascading
//! grant on an ancestor folder — and `PgAclEngine::check` honours all three.
//! Written as `A OR B OR C` across three tables the planner collapses to a
//! sequential scan on `storage.files`; as a union of three separately
//! indexable selects over file ids it does not. Only `user` and `group`
//! subjects count: a `token` grant is held by a public link, not by a person,
//! and admitting it would put every publicly-linked file in every user's
//! feed.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::application::dtos::recent_changes_dto::{ChangeLane, FileChangeRow};
use crate::common::errors::DomainError;

/// Every file id the caller may see, as a CTE the two queries share.
///
/// `MATERIALIZED` on the drive arm for the same reason the photos listing
/// does it: the planner otherwise inlines the STABLE function per row.
const VISIBLE_FILES_CTE: &str = r#"
    accessible_drives AS MATERIALIZED (
        SELECT drive_id FROM storage.caller_accessible_drives($1, NULL)
    ),
    caller_subjects AS MATERIALIZED (
        SELECT $1::uuid AS id
        UNION
        SELECT storage.caller_group_ids($1)
    ),
    granted_folders AS MATERIALIZED (
        SELECT fo.lpath
          FROM storage.role_grants g
          JOIN storage.folders fo ON fo.id = g.resource_id
         WHERE g.resource_type = 'folder'
           AND g.subject_type IN ('user', 'group')
           AND g.subject_id IN (SELECT id FROM caller_subjects)
           AND (g.expires_at IS NULL OR g.expires_at > NOW())
    ),
    visible_files AS (
        SELECT f.id
          FROM storage.files f
         WHERE NOT f.is_trashed
           AND f.drive_id IN (SELECT drive_id FROM accessible_drives)
        UNION
        SELECT f.id
          FROM storage.files f
          JOIN storage.role_grants g
            ON g.resource_id = f.id
           AND g.resource_type = 'file'
           AND g.subject_type IN ('user', 'group')
           AND g.subject_id IN (SELECT id FROM caller_subjects)
           AND (g.expires_at IS NULL OR g.expires_at > NOW())
         WHERE NOT f.is_trashed
        UNION
        SELECT f.id
          FROM storage.files f
          JOIN storage.folders fo ON fo.id = f.folder_id
         WHERE NOT f.is_trashed
           AND fo.lpath <@ ANY (SELECT lpath FROM granted_folders)
    )
"#;

/// Reads for the recent-changes feed. Caller-scoped: every query takes
/// `caller_id` as `$1` and filters through [`VISIBLE_FILES_CTE`], so there is
/// no shape in which this returns a row the caller could not already open.
pub struct RecentChangesPgRepository {
    pool: Arc<PgPool>,
}

impl RecentChangesPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Step 1 — the lanes, ordered by each person's most recent change.
    ///
    /// `COUNT(*)` is the number of FILES that person changed, exactly: the
    /// touch table holds one row per (file, person), so no `DISTINCT` is
    /// needed and a file saved forty times still counts once. Counted inside
    /// the access filter, so a lane total can never hint at a file the caller
    /// cannot see.
    /// `only` restricts to named people — the `?person=` drill-in, and how the
    /// handler re-reads the totals for a lane a cursor landed inside.
    /// `after` is the lane keyset `(lane_latest, user_id)`, so a page-2
    /// request resumes at the right lane instead of re-aggregating from the
    /// top and paging by offset.
    pub async fn lanes(
        &self,
        caller_id: Uuid,
        days: i32,
        only: Option<&[Uuid]>,
        after: Option<(DateTime<Utc>, Uuid)>,
        limit: i64,
    ) -> Result<Vec<ChangeLane>, DomainError> {
        let sql = format!(
            "WITH {VISIBLE_FILES_CTE}
             SELECT t.user_id,
                    MAX(t.touched_at) AS lane_latest,
                    COUNT(*)::bigint  AS lane_total
               FROM storage.file_content_touches t
              WHERE t.touched_at > NOW() - ($2 || ' days')::interval
                AND t.file_id IN (SELECT id FROM visible_files)
                AND ($3::uuid[] IS NULL OR t.user_id = ANY($3))
              GROUP BY t.user_id
             HAVING $4::timestamptz IS NULL
                 OR MAX(t.touched_at) < $4
                 OR (MAX(t.touched_at) = $4 AND t.user_id > $5)
              ORDER BY lane_latest DESC, t.user_id
              LIMIT $6"
        );
        let rows = sqlx::query(&sql)
            .bind(caller_id)
            .bind(days.to_string())
            .bind(only)
            .bind(after.map(|(ts, _)| ts))
            .bind(after.map(|(_, id)| id))
            .bind(limit)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("RecentChanges", format!("lanes: {e}")))?;

        rows.into_iter()
            .map(|r| {
                Ok(ChangeLane {
                    user_id: r.try_get("user_id").map_err(decode)?,
                    lane_latest: r.try_get("lane_latest").map_err(decode)?,
                    lane_total: r.try_get("lane_total").map_err(decode)?,
                })
            })
            .collect()
    }

    /// Step 2 — the rows.
    ///
    /// `lane_ids` empty means "every lane in one chronological stream", which
    /// is the `changed_at` axis: keyset `(changed_at, file_id)`.
    ///
    /// With lanes, the order is **lane-major**: `array_position` over the
    /// ordered lane list is the leading sort key, so a lane's older file still
    /// sorts above the next lane's newer one. Ordering globally by time would
    /// interleave people, and a client drawing a swimlane break whenever the
    /// person changes would open and close the same lane repeatedly down the
    /// page. The keyset therefore compares `(lane_rank, changed_at, file_id)`
    /// in that precedence, which is why `after_rank` exists — the caller
    /// resolves the cursor's lane to its position in the lane list it just
    /// fetched.
    #[allow(clippy::too_many_arguments)]
    pub async fn rows_for_lanes(
        &self,
        caller_id: Uuid,
        days: i32,
        lane_ids: &[Uuid],
        after_rank: Option<i32>,
        after_ts: Option<DateTime<Utc>>,
        after_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<FileChangeRow>, DomainError> {
        let sql = format!(
            "WITH {VISIBLE_FILES_CTE}
             SELECT f.id                                           AS file_id,
                    f.name                                         AS name,
                    f.folder_id                                    AS folder_id,
                    fo.path                                        AS folder_path,
                    f.size                                         AS size,
                    f.mime_type                                    AS mime_type,
                    f.created_at                                   AS created_at,
                    f.content_modified_at                          AS content_modified_at,
                    f.drive_id                                     AS drive_id,
                    f.blob_hash                                    AS blob_hash,
                    f.created_by                                   AS created_by,
                    f.updated_by                                   AS updated_by,
                    EXISTS (
                        SELECT 1 FROM auth.user_favorites uf
                         WHERE uf.user_id = $1
                           AND uf.item_id = f.id::text
                           AND uf.item_type = 'file'
                    )                                              AS is_favorite,
                    EXISTS (
                        SELECT 1 FROM storage.role_grants g
                         WHERE g.resource_id = f.id
                           AND g.resource_type = 'file'
                    )                                              AS is_shared,
                    t.touched_at                                   AS changed_at,
                    t.user_id                                      AS changed_by
               FROM storage.file_content_touches t
               JOIN storage.files f ON f.id = t.file_id
               LEFT JOIN storage.folders fo ON fo.id = f.folder_id
              WHERE t.touched_at > NOW() - ($2 || ' days')::interval
                AND t.file_id IN (SELECT id FROM visible_files)
                AND ($3::uuid[] IS NULL OR t.user_id = ANY($3))
                AND ($5::timestamptz IS NULL
                     OR ($4::int IS NOT NULL
                         AND array_position($3, t.user_id) > $4)
                     OR (($4::int IS NULL
                          OR array_position($3, t.user_id) = $4)
                         AND (t.touched_at < $5
                              OR (t.touched_at = $5 AND t.file_id < $6))))
              ORDER BY CASE WHEN $3::uuid[] IS NULL THEN 0
                            ELSE array_position($3, t.user_id) END,
                       t.touched_at DESC, t.file_id DESC
              LIMIT $7"
        );
        let lanes: Option<&[Uuid]> = if lane_ids.is_empty() {
            None
        } else {
            Some(lane_ids)
        };
        let rows = sqlx::query(&sql)
            .bind(caller_id)
            .bind(days.to_string())
            .bind(lanes)
            .bind(after_rank)
            .bind(after_ts)
            .bind(after_id)
            .bind(limit)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("RecentChanges", format!("rows: {e}")))?;

        rows.into_iter()
            .map(|r| {
                Ok(FileChangeRow {
                    file_id: r.try_get("file_id").map_err(decode)?,
                    name: r.try_get("name").map_err(decode)?,
                    folder_id: r.try_get("folder_id").map_err(decode)?,
                    folder_path: r.try_get("folder_path").map_err(decode)?,
                    size: r.try_get("size").map_err(decode)?,
                    mime_type: r.try_get("mime_type").map_err(decode)?,
                    created_at: r.try_get("created_at").map_err(decode)?,
                    content_modified_at: r.try_get("content_modified_at").map_err(decode)?,
                    drive_id: r.try_get("drive_id").map_err(decode)?,
                    blob_hash: r.try_get("blob_hash").map_err(decode)?,
                    created_by: r.try_get("created_by").map_err(decode)?,
                    updated_by: r.try_get("updated_by").map_err(decode)?,
                    is_favorite: r.try_get("is_favorite").map_err(decode)?,
                    is_shared: r.try_get("is_shared").map_err(decode)?,
                    changed_at: r.try_get("changed_at").map_err(decode)?,
                    changed_by: r.try_get("changed_by").map_err(decode)?,
                })
            })
            .collect()
    }
}

fn decode(e: sqlx::Error) -> DomainError {
    DomainError::internal_error("RecentChanges", format!("decode: {e}"))
}
