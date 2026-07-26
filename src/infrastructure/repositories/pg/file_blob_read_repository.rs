//! PostgreSQL + Blob-backed file read repository.
//!
//! Implements `FileReadPort` using:
//! - `storage.files` table for metadata lookups
//! - `DedupPort` for reading content-addressable blobs from the filesystem
//!
//! File paths are resolved by JOINing with `storage.folders.path` (the
//! materialized path column), so no recursive CTEs or N+1 queries are needed.

/// Row shape returned by media-file queries (avoids `clippy::type_complexity`).
/// Post-D7-step-6: `storage.files.user_id` dropped, so it's no
/// longer projected.
type MediaFileRow = (
    Uuid,           // id (binary decode; benches/ROUND6.md §10)
    String,         // name
    Option<Uuid>,   // folder_id
    Option<String>, // folder path
    i64,            // size
    String,         // mime_type
    i64,            // created_at
    i64,            // updated_at
    String,         // blob_hash
    Option<Uuid>,   // created_by (§14 provenance)
    Option<Uuid>,   // updated_by (§14 provenance)
    bool,           // is_favorite (caller-scoped EXISTS on user_favorites)
    bool,           // is_shared   (resource-scoped EXISTS on role_grants)
    i64,            // sort_date
    Option<i32>,    // width
    Option<i32>,    // height
);

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use moka::sync::Cache;
use sqlx::PgPool;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::application::dtos::geo_dto::{GeoBounds, GeoCluster};
use crate::application::dtos::search_dto::SearchCriteriaDto;
use crate::application::ports::storage_ports::FileReadPort;
use crate::common::errors::DomainError;
use crate::domain::entities::file::File;
use crate::domain::services::path_service::StoragePath;
use crate::infrastructure::services::dedup_service::DedupService;
use uuid::Uuid;

/// SQL `EXISTS (…)` predicate — true when the caller (bound to `$1`) has
/// any active `role_grants` on the drive owning `fi` (the aliased file
/// row). Group memberships (direct + transitive) are expanded inline via
/// `storage.caller_group_ids($1)` (recursive; see migration
/// `20260901000002_caller_group_ids_function.sql`).
///
/// Used by every drive-scoped file search query in this repo:
/// - `search_files_paginated`
/// - `search_files_in_subtree`
///
/// **Alias contract**: queries splicing this in MUST alias
/// `storage.files` as `fi`. `$1` is reserved for `caller_id`; other bind
/// params start at `$2`.
///
/// This mirrors — but is intentionally not shared with — the folder
/// variant in `folder_db_repository.rs` (aliased `fo.drive_id`) and the
/// drive-listing shapes in `drive_pg_repository`/`list_media_files`.
/// When the grant model changes, update all sites in parallel.
const CALLER_CAN_READ_DRIVE: &str = "EXISTS (\
        SELECT 1 \
          FROM storage.role_grants g \
         WHERE g.resource_type = 'drive' \
           AND g.resource_id   = fi.drive_id \
           AND (g.expires_at IS NULL OR g.expires_at > NOW()) \
           AND ( \
                 (g.subject_type = 'user'  AND g.subject_id = $1) \
              OR (g.subject_type = 'group' AND g.subject_id IN \
                      (SELECT storage.caller_group_ids($1))) \
               ) \
    )";

/// Type alias for file metadata rows from SQL queries.
/// Fields: id, name, folder_id, folder_path, size, mime_type,
/// created_at, updated_at, blob_hash, created_by, updated_by.
/// `created_by` / `updated_by` are the §14 provenance columns.
/// Post-D7-step-6: `storage.files.user_id` dropped, so it's no
/// longer part of the tuple; `row_to_file` populates the entity's
/// legacy `user_id` field with `None`.
type FileRow = (
    Uuid,
    String,
    Option<Uuid>,
    Option<String>,
    i64,
    String,
    i64,
    i64,
    String,
    Option<Uuid>,
    Option<Uuid>,
);

/// Append the optional type/date/size filters from `criteria` to
/// `conditions`, continuing placeholder numbering from `bind_idx`. Returns
/// the last placeholder index used. The name filter is NOT handled here —
/// it is search-flavour specific (ILIKE for name search, absent for
/// content-hit hydration). Mirror of [`bind_criteria_filters`]; the two
/// must stay in sync.
fn push_criteria_filters(
    conditions: &mut Vec<String>,
    mut bind_idx: u32,
    criteria: &SearchCriteriaDto,
) -> u32 {
    if let Some(types) = &criteria.file_types
        && !types.is_empty()
    {
        bind_idx += 1;
        conditions.push(format!(
            "LOWER(SUBSTRING(fi.name FROM '\\.([^.]+)$')) = ANY(${bind_idx})"
        ));
    }
    if criteria.created_after.is_some() {
        bind_idx += 1;
        conditions.push(format!(
            "EXTRACT(EPOCH FROM fi.created_at)::bigint >= ${bind_idx}"
        ));
    }
    if criteria.created_before.is_some() {
        bind_idx += 1;
        conditions.push(format!(
            "EXTRACT(EPOCH FROM fi.created_at)::bigint <= ${bind_idx}"
        ));
    }
    if criteria.modified_after.is_some() {
        bind_idx += 1;
        conditions.push(format!(
            "EXTRACT(EPOCH FROM fi.updated_at)::bigint >= ${bind_idx}"
        ));
    }
    if criteria.modified_before.is_some() {
        bind_idx += 1;
        conditions.push(format!(
            "EXTRACT(EPOCH FROM fi.updated_at)::bigint <= ${bind_idx}"
        ));
    }
    if criteria.min_size.is_some() {
        bind_idx += 1;
        conditions.push(format!("fi.size >= ${bind_idx}"));
    }
    if criteria.max_size.is_some() {
        bind_idx += 1;
        conditions.push(format!("fi.size <= ${bind_idx}"));
    }
    bind_idx
}

/// Bind the values for the filters appended by [`push_criteria_filters`],
/// in the same order.
fn bind_criteria_filters<'q, O>(
    mut query: sqlx::query::QueryAs<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments>,
    criteria: &SearchCriteriaDto,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments> {
    if let Some(types) = &criteria.file_types
        && !types.is_empty()
    {
        let lower_types: Vec<String> = types.iter().map(|t| t.to_lowercase()).collect();
        query = query.bind(lower_types);
    }
    if let Some(v) = criteria.created_after {
        query = query.bind(v as i64);
    }
    if let Some(v) = criteria.created_before {
        query = query.bind(v as i64);
    }
    if let Some(v) = criteria.modified_after {
        query = query.bind(v as i64);
    }
    if let Some(v) = criteria.modified_before {
        query = query.bind(v as i64);
    }
    if let Some(v) = criteria.min_size {
        query = query.bind(v as i64);
    }
    if let Some(v) = criteria.max_size {
        query = query.bind(v as i64);
    }
    query
}

/// File read repository backed by PostgreSQL metadata + blob storage.
pub struct FileBlobReadRepository {
    pool: Arc<PgPool>,
    dedup: Arc<DedupService>,
    /// Lock-free cache: file_id → blob_hash.
    /// Populated by `get_file()` and `resolve_blob_hash()` (slow path).
    /// Entries persist until TTI expiry (30 s idle) or capacity eviction.
    /// Content updates DO remap a file_id to a new hash in place
    /// (`swap_blob_hash`), so the write repository shares this cache (see
    /// [`Self::blob_hash_cache`]) and invalidates the entry on every
    /// content swap and hard delete — without that, streaming downloads
    /// kept serving the previous blob for the TTI window after a PUT
    /// update (or 500'd once the old blob was garbage-collected), and
    /// every read refreshed the TTI, extending the window indefinitely.
    hash_cache: Cache<String, String>,
}

impl FileBlobReadRepository {
    pub fn new(
        pool: Arc<PgPool>,
        dedup: Arc<DedupService>,
        _folder_repo: Arc<super::folder_db_repository::FolderDbRepository>,
    ) -> Self {
        Self {
            pool,
            dedup,
            hash_cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_idle(Duration::from_secs(30))
                .build(),
        }
    }

    /// Shared handle to the file_id → blob_hash cache (moka clones share
    /// the underlying storage). Handed to `FileBlobWriteRepository` at DI
    /// time so content swaps and hard deletes invalidate the mapping the
    /// moment they commit.
    pub fn blob_hash_cache(&self) -> Cache<String, String> {
        self.hash_cache.clone()
    }

    /// Hydrate content-index candidate ids into `File`s, re-applying the
    /// caller's scope and the active search filters (owner, trash state,
    /// folder scope, types, dates, sizes). The NAME filter is deliberately
    /// NOT applied — content hits don't need to match it. Ids that fail any
    /// filter (or no longer exist — the index is eventually consistent)
    /// simply drop out, so a stale index can never leak a result.
    pub async fn fetch_files_by_ids_filtered(
        &self,
        ids: &[String],
        criteria: &SearchCriteriaDto,
        caller_id: Uuid,
    ) -> Result<Vec<File>, DomainError> {
        // Index hits are externally produced strings — parse defensively.
        let uuid_ids: Vec<Uuid> = ids.iter().filter_map(|id| id.parse().ok()).collect();
        if uuid_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Post-PR-B: drive-membership scoping via
        // [`CALLER_CAN_READ_DRIVE`] (bound to `$1`) replaces the legacy
        // `fi.user_id = $caller` predicate. Group grants are honoured
        // inline through `storage.caller_group_ids`.
        //
        // Bind order: $1 = caller_id, $2 = ids array, $3.. = criteria.
        let mut conditions: Vec<String> = vec![
            CALLER_CAN_READ_DRIVE.to_string(),
            "fi.id = ANY($2)".to_string(),
            "fi.is_trashed = false".to_string(),
        ];
        let mut bind_idx = 2u32;

        if criteria.folder_id.is_some() {
            bind_idx += 1;
            if criteria.recursive {
                conditions.push(format!(
                    "fo.lpath <@ (SELECT lpath FROM storage.folders WHERE id = ${bind_idx}::uuid)"
                ));
            } else {
                conditions.push(format!("fi.folder_id = ${bind_idx}::uuid"));
            }
        }
        push_criteria_filters(&mut conditions, bind_idx, criteria);

        let where_clause = conditions.join(" AND ");
        let sql = format!(
            "SELECT fi.id, fi.name, fi.folder_id, fo.path, \
                    fi.size, fi.mime_type, \
                    EXTRACT(EPOCH FROM fi.created_at)::bigint, \
                    EXTRACT(EPOCH FROM fi.updated_at)::bigint, \
                    fi.blob_hash, \
                    \
                    fi.created_by, fi.updated_by \
               FROM storage.files fi \
               LEFT JOIN storage.folders fo ON fo.id = fi.folder_id \
              WHERE {where_clause}"
        );

        let mut query = sqlx::query_as::<_, FileRow>(&sql)
            .bind(caller_id)
            .bind(uuid_ids);
        if let Some(folder_id) = criteria.folder_id.as_deref() {
            query = query.bind(folder_id);
        }
        query = bind_criteria_filters(query, criteria);

        let rows = query.fetch_all(self.pool.as_ref()).await.map_err(|e| {
            DomainError::internal_error("FileBlobRead", format!("hydrate by ids: {e}"))
        })?;

        // Pre-size the result Vec. `collect::<Result<Vec<_>, _>>()` size-hints
        // to 0 (the Result shunt may short-circuit on any element), so the Vec
        // grows from capacity 0 — ~⌈log₂N⌉ reallocations, memcpy-ing the
        // accumulated File rows each grow (benches/ROUND20.md §I1).
        let mut files = Vec::with_capacity(rows.len());
        for (id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub) in rows {
            files.push(
                Self::row_to_file(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)
                    .map_err(|e| {
                        DomainError::internal_error("FileBlobRead", format!("hydrate mapping: {e}"))
                    })?,
            );
        }
        Ok(files)
    }

    /// Batch-fetch files by id — the by-ids counterpart of [`get_file`],
    /// used to resolve a page of ACL grants or favorites in ONE round-trip
    /// instead of one query per id (the previous `join_all(ids.map(get_file))`
    /// could fan out to ~200 concurrent pooled connections per page). Applies
    /// the same `NOT is_trashed` filter and identical column mapping as
    /// `get_file`. Ids that are missing or trashed simply drop out, so callers
    /// must re-associate results by id; ordering is not guaranteed.
    pub async fn get_files_by_ids(&self, ids: &[String]) -> Result<Vec<File>, DomainError> {
        let uuid_ids: Vec<Uuid> = ids.iter().filter_map(|id| id.parse().ok()).collect();
        if uuid_ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query_as::<_, FileRow>(
            "SELECT fi.id, fi.name, fi.folder_id, fo.path, \
                    fi.size, fi.mime_type, \
                    EXTRACT(EPOCH FROM fi.created_at)::bigint, \
                    EXTRACT(EPOCH FROM fi.updated_at)::bigint, \
                    fi.blob_hash, \
                    \
                    fi.created_by, fi.updated_by \
               FROM storage.files fi \
               LEFT JOIN storage.folders fo ON fo.id = fi.folder_id \
              WHERE fi.id = ANY($1) AND NOT fi.is_trashed",
        )
        .bind(&uuid_ids)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("FileBlobRead", format!("get_files_by_ids: {e}"))
        })?;

        // Pre-size the result Vec (see the size-hint note in `hydrate`,
        // benches/ROUND20.md §I1).
        let mut files = Vec::with_capacity(rows.len());
        for (id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub) in rows {
            files.push(
                Self::row_to_file(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)
                    .map_err(|e| {
                        DomainError::internal_error(
                            "FileBlobRead",
                            format!("get_files_by_ids mapping: {e}"),
                        )
                    })?,
            );
        }
        Ok(files)
    }

    /// Returns `drive_id` for a given file. Drives the permission-floor
    /// short-circuit in `PgAclEngine::check_inner` — drive membership is
    /// the baseline floor per `drive.md §5`.
    pub async fn get_file_drive_id(&self, file_id: &str) -> Result<uuid::Uuid, DomainError> {
        sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT drive_id FROM storage.files WHERE id = $1::uuid",
        )
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("drive_id lookup: {e}")))?
        .ok_or_else(|| DomainError::not_found("File", file_id))
    }

    /// Batched variant of [`Self::get_file_drive_id`]: one `= ANY($1)`
    /// round-trip for a whole result page. Missing / unknown ids are simply
    /// absent from the output (the single-id variant maps them to
    /// `NotFound`). Used by `PgAclEngine::check_files_read_batch` — the
    /// per-hit loop cost up to 200 sequential point SELECTs per content
    /// search (benches/SEARCH-REBAC.md).
    pub async fn get_file_drive_ids(
        &self,
        file_ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, uuid::Uuid)>, DomainError> {
        if file_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
            "SELECT id, drive_id FROM storage.files WHERE id = ANY($1)",
        )
        .bind(file_ids)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("FileBlobRead", format!("drive_id batch lookup: {e}"))
        })
    }

    /// Creates a stub instance for testing — never hits PG.
    /// Available in both standard unit-test (`cfg(test)`) and integration
    /// (`cfg(integration_tests)`) builds; `PgAclEngine::new_stub` chains
    /// into this stub and is needed from the integration-test module of
    /// `subject_group_service`.
    #[cfg(any(test, integration_tests))]
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
            hash_cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_idle(Duration::from_secs(30))
                .build(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn row_to_file(
        id: Uuid,
        name: String,
        folder_id: Option<Uuid>,
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
            id.to_string(),
            name,
            folder_path.as_deref(),
            size as u64,
            mime_type,
            folder_id.map(|u| u.to_string()),
            created_at as u64,
            modified_at as u64,
            blob_hash,
            created_by,
            updated_by,
        )
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("entity: {e}")))
    }

    /// Resolve the blob hash for a file (internal helper).
    ///
    /// Checks the lock-free moka cache first (populated by `get_file` or
    /// a previous slow-path lookup).  The entry is **kept** in cache so
    /// subsequent reads for the same file (e.g. Range Requests on a video,
    /// thumbnail + download, browser re-fetch) hit the cache instead of PG.
    ///
    /// Staleness safety: content updates remap the file to a new hash in
    /// place — the write repository invalidates this cache (shared via
    /// [`Self::blob_hash_cache`]) right after every swap/delete commits.
    async fn resolve_blob_hash(&self, file_id: &str) -> Result<String, DomainError> {
        // Fast path: cached (lock-free read, refreshes TTI automatically)
        if let Some(hash) = self.hash_cache.get(file_id) {
            return Ok(hash);
        }
        // Slow path: DB round-trip → populate cache for future reads
        let hash = sqlx::query_scalar::<_, String>(
            "SELECT blob_hash FROM storage.files WHERE id = $1::uuid AND NOT is_trashed",
        )
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("hash lookup: {e}")))?
        .ok_or_else(|| DomainError::not_found("File", file_id))?;

        self.hash_cache.insert(file_id.to_owned(), hash.clone());
        Ok(hash)
    }

    /// Lists all image/video files for a user, sorted by capture date (EXIF) or
    /// creation date, with cursor-based pagination for the Photos timeline.
    ///
    /// Returns `(Vec<File>, Vec<i64>)` where the second vec contains the
    /// `sort_date` epoch for each file (used as pagination cursor).
    ///
    /// Uses the denormalised `media_sort_date` column (synced from
    /// `file_metadata.captured_at` by trigger). The accessible drive ids
    /// are materialised once, then a `CROSS JOIN LATERAL (… ORDER BY
    /// media_sort_date DESC LIMIT k)` per drive turns the partial covering
    /// index `idx_files_media_timeline_by_drive` (migration 20260901000001,
    /// `(drive_id, media_sort_date DESC)` filtered on non-trashed
    /// image/video rows) into one BOUNDED index scan per drive; the outer
    /// merge sorts `drives × k` rows. The folders / file_metadata joins sit
    /// outside the top-N so only the k emitted rows pay them.
    ///
    /// The previous shape put the joins and the global `ORDER BY … LIMIT`
    /// above a `drive_id IN (…)` nested loop — Postgres fed EVERY media row
    /// through the join into a top-N heapsort, scanning the timeline index
    /// to exhaustion on every page: O(library) per page, 97 ms on a
    /// 50k-photo library vs 1.6 ms for this shape (55.7x,
    /// benches/PHOTOS-TIMELINE.md).
    ///
    /// Scope (`docs/plan/drive.md` §15): drives with
    /// `policies.include_in_photo_index = true` where the caller has a
    /// direct grant (`subject_type = 'user'`) OR a grant on a group they
    /// belong to transitively. Group membership is expanded inline by the
    /// `storage.caller_group_ids(caller)` SQL function (migration
    /// `20260901000002_caller_group_ids_function.sql`) — no ceremony at
    /// the handler layer, no cross-space ambiguity from the earlier
    /// parallel-arrays pattern.
    ///
    /// Default personal drives always match because the flag is
    /// materialised to `true` at drive creation (see
    /// `DriveRepository::create_personal_drive_atomic` + the backfill
    /// migration `20260901000000_default_personal_photo_music_flags.sql`)
    /// — no per-kind carve-out needed. Non-default drives (secondary
    /// personals, shared drives) surface here only after their owner
    /// flips the flag on via the admin "Manage policies" modal.
    pub async fn list_media_files(
        &self,
        caller_id: Uuid,
        before: Option<i64>,
        limit: i64,
    ) -> Result<
        (
            Vec<File>,
            Vec<i64>,
            Vec<(Option<i32>, Option<i32>)>,
            // Per-row caller flags (is_favorite, is_shared). Aligned
            // with `files` — zip 1:1. Kept parallel to `sort_dates` /
            // `dims` instead of on the File entity so the domain
            // stays caller-agnostic.
            Vec<(bool, bool)>,
        ),
        DomainError,
    > {
        // Sargable keyset cursor: compare the RAW `media_sort_date` column
        // against a timestamptz bind so the planner can use the cursor as
        // an index boundary condition on `idx_files_media_timeline_by_drive`.
        // The old shape wrapped the column in `EXTRACT(EPOCH …)::bigint`
        // (plus an `IS NULL OR` disjunction), which degraded the cursor to
        // a per-row Filter: page k re-read and discarded all k·limit rows
        // already scrolled past (benches/PHOTOS-CURSOR.md). Since `before`
        // is whole seconds, `media_sort_date < to_timestamp(before)` admits
        // exactly the same rows as the old truncated comparison. The
        // predicate is emitted only when a cursor exists — a bound
        // disjunction would block the index condition under generic plans.
        let cursor_ts = before.and_then(|s| chrono::DateTime::from_timestamp(s, 0));
        let cursor_pred = if cursor_ts.is_some() {
            "AND fi.media_sort_date < $2"
        } else {
            "AND $2::timestamptz IS NULL"
        };
        let sql = format!(
            r#"
            WITH accessible AS MATERIALIZED (
                SELECT d.id
                  FROM storage.drives d
                  JOIN storage.role_grants g
                    ON g.resource_type = 'drive'
                   AND g.resource_id   = d.id
                 WHERE (
                         (g.subject_type = 'user'  AND g.subject_id = $1)
                      OR (g.subject_type = 'group' AND g.subject_id IN
                              (SELECT storage.caller_group_ids($1)))
                       )
                   AND (g.expires_at IS NULL OR g.expires_at > NOW())
                   AND (d.policies->>'include_in_photo_index')::boolean = true
            )
            SELECT top.id, top.name, top.folder_id, fo.path,
                   top.size, top.mime_type,
                   EXTRACT(EPOCH FROM top.created_at)::bigint,
                   EXTRACT(EPOCH FROM top.updated_at)::bigint,
                   top.blob_hash,
                   top.created_by, top.updated_by,
                   EXISTS (
                       SELECT 1 FROM auth.user_favorites uf
                        WHERE uf.user_id   = $1
                          AND uf.item_id   = top.id::text
                          AND uf.item_type = 'file'
                   ) AS is_favorite,
                   EXISTS (
                       SELECT 1 FROM storage.role_grants g
                        WHERE g.resource_id   = top.id
                          AND g.resource_type = 'file'
                   ) AS is_shared,
                   EXTRACT(EPOCH FROM top.media_sort_date)::bigint AS sort_date,
                   fm.width, fm.height
              FROM (
                SELECT fi.*
                  FROM accessible a
                 CROSS JOIN LATERAL (
                    SELECT fi.*
                      FROM storage.files fi
                     WHERE fi.drive_id = a.id
                       AND NOT fi.is_trashed
                       AND (fi.mime_type LIKE 'image/%' OR fi.mime_type LIKE 'video/%')
                       {cursor_pred}
                     ORDER BY fi.media_sort_date DESC
                     LIMIT $3
                 ) fi
                 ORDER BY fi.media_sort_date DESC
                 LIMIT $3
              ) top
              LEFT JOIN storage.folders fo ON fo.id = top.folder_id
              LEFT JOIN storage.file_metadata fm ON fm.file_id = top.id
             ORDER BY top.media_sort_date DESC
            "#,
        );
        let rows: Vec<MediaFileRow> = sqlx::query_as(&sql)
            .bind(caller_id)
            .bind(cursor_ts)
            .bind(limit)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("FileBlobRead", format!("list_media: {e}")))?;

        let mut files = Vec::with_capacity(rows.len());
        let mut sort_dates = Vec::with_capacity(rows.len());
        let mut dims = Vec::with_capacity(rows.len());
        let mut flags = Vec::with_capacity(rows.len());

        for (
            id,
            name,
            fid,
            fpath,
            size,
            mime,
            ca,
            ma,
            blob_hash,
            cb,
            ub,
            is_fav,
            is_shr,
            sd,
            w,
            h,
        ) in rows
        {
            files.push(Self::row_to_file(
                id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub,
            )?);
            sort_dates.push(sd);
            dims.push((w, h));
            flags.push((is_fav, is_shr));
        }

        Ok((files, sort_dates, dims, flags))
    }

    /// Aggregate the caller's geotagged photos into grid cells of side `cell`
    /// (degrees) within `bounds`. Plain SQL (no PostGIS).
    ///
    /// Scope: same `include_in_photo_index` predicate as
    /// `list_media_files` (§15). Places is the map view over the same
    /// content set the Photos timeline shows, so the two surfaces MUST
    /// agree on drive scope. Group membership is expanded inline by
    /// `storage.caller_group_ids(caller)`.
    ///
    /// This query is a per-cell aggregate (group by rounded lat/lng
    /// bucket) rather than an ORDER BY / LIMIT hot path — the plain
    /// `idx_files_drive_id` is sufficient to seek by drive.
    pub async fn list_geo_clusters(
        &self,
        caller_id: Uuid,
        bounds: GeoBounds,
        cell: f64,
    ) -> Result<Vec<GeoCluster>, DomainError> {
        let rows: Vec<(i64, f64, f64, String)> = sqlx::query_as(
            r#"
            SELECT count(*)              AS n,
                   avg(fm.longitude)     AS clng,
                   avg(fm.latitude)      AS clat,
                   -- NOTE: `min(fm.file_id)::text` (cast per cluster, not
                   -- per row) was attempted in ROUND11 §Q4 and REJECTED by
                   -- its benchmark gate: PostgreSQL has no `min(uuid)`
                   -- aggregate, and adding a custom one is schema surface
                   -- this viewport query doesn't justify.
                   min(fm.file_id::text) AS sample_id
              FROM storage.file_metadata fm
              JOIN storage.files fi ON fi.id = fm.file_id
             WHERE fi.drive_id IN (
                     SELECT d.id
                       FROM storage.drives d
                       JOIN storage.role_grants g
                         ON g.resource_type = 'drive'
                        AND g.resource_id   = d.id
                      WHERE (
                              (g.subject_type = 'user'  AND g.subject_id = $1)
                           OR (g.subject_type = 'group' AND g.subject_id IN
                                   (SELECT storage.caller_group_ids($1)))
                            )
                        AND (g.expires_at IS NULL OR g.expires_at > NOW())
                        AND (d.policies->>'include_in_photo_index')::boolean = true
                   )
               AND NOT fi.is_trashed
               AND fm.latitude IS NOT NULL
               AND fm.longitude IS NOT NULL
               AND fm.longitude BETWEEN $2 AND $3
               AND fm.latitude  BETWEEN $4 AND $5
             GROUP BY round(fm.longitude / $6), round(fm.latitude / $6)
            "#,
        )
        .bind(caller_id)
        .bind(bounds.west)
        .bind(bounds.east)
        .bind(bounds.south)
        .bind(bounds.north)
        .bind(cell)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("FileBlobRead", format!("list_geo_clusters: {e}"))
        })?;

        Ok(rows
            .into_iter()
            .map(|(n, clng, clat, sample_id)| GeoCluster {
                lng: clng,
                lat: clat,
                count: n,
                sample_file_id: sample_id,
            })
            .collect())
    }
}

impl FileReadPort for FileBlobReadRepository {
    async fn get_file(&self, id: &str) -> Result<File, DomainError> {
        let row = sqlx::query_as::<
            _,
            (
                Uuid,           // id (binary decode)
                String,         // name
                Option<Uuid>,   // folder_id
                Option<String>, // folder path
                i64,            // size
                String,         // mime_type
                i64,            // created_at
                i64,            // updated_at
                String,         // blob_hash
                Option<Uuid>,   // created_by (§14)
                Option<Uuid>,   // updated_by (§14)
            ),
        >(
            r#"
            SELECT fi.id, fi.name, fi.folder_id, fo.path,
                   fi.size, fi.mime_type,
                   EXTRACT(EPOCH FROM fi.created_at)::bigint,
                   EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                   fi.blob_hash,
                   fi.created_by, fi.updated_by
              FROM storage.files fi
              LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
             WHERE fi.id = $1::uuid AND NOT fi.is_trashed
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("get: {e}")))?
        .ok_or_else(|| DomainError::not_found("File", id))?;

        // Cache blob_hash so the subsequent get_file_stream / get_file_content
        // call doesn't need a separate DB round-trip.
        self.hash_cache.insert(id.to_string(), row.8.clone());

        Self::row_to_file(
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10,
        )
    }

    /// Like `get_file` but also returns trashed files, gated by owner_id.
    /// Used exclusively by the thumbnail handler so that thumbnails remain
    /// accessible while a file is in the trash (before permanent deletion).
    async fn get_file_or_trashed(&self, id: &str) -> Result<File, DomainError> {
        let row = sqlx::query_as::<
            _,
            (
                Uuid,
                String,
                Option<Uuid>,
                Option<String>,
                i64,
                String,
                i64,
                i64,
                String,
                Option<Uuid>, // created_by (§14)
                Option<Uuid>, // updated_by (§14)
            ),
        >(
            r#"
            SELECT fi.id, fi.name, fi.folder_id, fo.path,
                   fi.size, fi.mime_type,
                   EXTRACT(EPOCH FROM fi.created_at)::bigint,
                   EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                   fi.blob_hash,
                   fi.created_by, fi.updated_by
              FROM storage.files fi
              LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
             WHERE fi.id = $1::uuid
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("get_trashed: {e}")))?
        .ok_or_else(|| DomainError::not_found("File", id))?;

        self.hash_cache.insert(id.to_string(), row.8.clone());
        Self::row_to_file(
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10,
        )
    }

    #[allow(clippy::type_complexity)]
    async fn list_files(&self, folder_id: Option<&str>) -> Result<Vec<File>, DomainError> {
        let rows: Vec<FileRow> = if let Some(fid) = folder_id {
            sqlx::query_as(
                r#"
                SELECT fi.id, fi.name, fi.folder_id, fo.path,
                       fi.size, fi.mime_type,
                       EXTRACT(EPOCH FROM fi.created_at)::bigint,
                       EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                       fi.blob_hash,

                   fi.created_by, fi.updated_by
                  FROM storage.files fi
                  LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
                 WHERE fi.folder_id = $1::uuid AND NOT fi.is_trashed
                 ORDER BY fi.name
                "#,
            )
            .bind(fid)
            .fetch_all(self.pool.as_ref())
            .await
        } else {
            sqlx::query_as(
                r#"
                SELECT fi.id, fi.name, fi.folder_id, fo.path,
                       fi.size, fi.mime_type,
                       EXTRACT(EPOCH FROM fi.created_at)::bigint,
                       EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                       fi.blob_hash,

                   fi.created_by, fi.updated_by
                  FROM storage.files fi
                  LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
                 WHERE fi.folder_id IS NULL AND NOT fi.is_trashed
                 ORDER BY fi.name
                "#,
            )
            .fetch_all(self.pool.as_ref())
            .await
        }
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("list: {e}")))?;

        rows.into_iter()
            .map(
                |(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)| {
                    Self::row_to_file(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)
                },
            )
            .collect()
    }

    async fn get_blob_hash(&self, file_id: &str) -> Result<String, DomainError> {
        self.resolve_blob_hash(file_id).await
    }

    /// Keyset-paginated file listing in name order — fetches only `limit`
    /// rows after `after_name` (exclusive).
    ///
    /// Names are unique per folder, so `name > $after` is a total cursor.
    /// Served by `idx_files_folder_name (folder_id, name) WHERE NOT
    /// is_trashed` as a pure index-range read: O(page) per page with no
    /// sort, where the old `LIMIT/OFFSET` shape re-scanned and re-sorted
    /// the entire folder for every page (benches/PROPFIND-PAGING.md). The
    /// cursor predicate is emitted only when a cursor exists — a
    /// `$2 IS NULL OR name > $2` disjunction would block the index
    /// condition under the extended protocol's generic plans.
    #[allow(clippy::type_complexity)]
    async fn list_files_batch(
        &self,
        folder_id: Option<&str>,
        after_name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<File>, DomainError> {
        let folder_pred = if folder_id.is_some() {
            "fi.folder_id = $1::uuid"
        } else {
            "fi.folder_id IS NULL AND $1::uuid IS NULL"
        };
        let cursor_pred = if after_name.is_some() {
            "AND fi.name > $3"
        } else {
            "AND $3::text IS NULL"
        };
        let sql = format!(
            r#"
            SELECT fi.id, fi.name, fi.folder_id, fo.path,
                   fi.size, fi.mime_type,
                   EXTRACT(EPOCH FROM fi.created_at)::bigint,
                   EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                   fi.blob_hash,

               fi.created_by, fi.updated_by
              FROM storage.files fi
              LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
             WHERE {folder_pred} AND NOT fi.is_trashed {cursor_pred}
             ORDER BY fi.name
             LIMIT $2
            "#,
        );
        let rows: Vec<FileRow> = sqlx::query_as(&sql)
            .bind(folder_id)
            .bind(limit)
            .bind(after_name)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("FileBlobRead", format!("list_batch: {e}")))?;

        rows.into_iter()
            .map(
                |(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)| {
                    Self::row_to_file(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)
                },
            )
            .collect()
    }

    async fn get_file_stream(
        &self,
        id: &str,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        // True streaming: reads the blob file in 64 KB chunks.
        // Memory usage is ~64 KB regardless of file size.
        let blob_hash = self.resolve_blob_hash(id).await?;
        let stream = self.dedup.read_blob_stream(&blob_hash).await?;
        Ok(Box::new(stream))
    }

    async fn get_file_range_stream(
        &self,
        id: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        // True range streaming: seeks to `start` and reads only the requested range.
        // A 1 MB range on a 1 GB file uses ~64 KB of RAM.
        let blob_hash = self.resolve_blob_hash(id).await?;
        let stream = self
            .dedup
            .read_blob_range_stream(&blob_hash, start, end)
            .await?;
        Ok(Box::new(stream))
    }

    async fn get_file_path(&self, id: &str) -> Result<StoragePath, DomainError> {
        let row = sqlx::query_as::<_, (String, Option<String>)>(
            r#"
            SELECT fi.name, fo.path
              FROM storage.files fi
              LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
             WHERE fi.id = $1::uuid AND NOT fi.is_trashed
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("path: {e}")))?
        .ok_or_else(|| DomainError::not_found("File", id))?;

        Ok(StoragePath::from_folder_and_name(row.1.as_deref(), &row.0))
    }

    async fn get_parent_folder_id(
        &self,
        path: &str,
        drive_id: Uuid,
    ) -> Result<String, DomainError> {
        let path = path.trim_start_matches('/').trim_end_matches('/');
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if segments.is_empty() {
            return Err(DomainError::not_found("Folder", "empty path"));
        }

        // For a path like "Home - user/Docs/file.txt", the parent folder path
        // is everything except the last segment: "Home - user/Docs"
        // We try the longest folder path first.
        let folder_path = segments[..segments.len() - 1].join("/");

        if folder_path.is_empty() {
            return Err(DomainError::not_found(
                "Folder",
                format!("parent for path: {path}"),
            ));
        }

        self.get_folder_id_by_path(&folder_path, drive_id).await
    }

    async fn get_folder_id_by_path(
        &self,
        folder_path: &str,
        drive_id: Uuid,
    ) -> Result<String, DomainError> {
        let folder_path = folder_path.trim_start_matches('/').trim_end_matches('/');

        if folder_path.is_empty() {
            return Err(DomainError::not_found("Folder", "empty path"));
        }

        // Post-D0 `storage.folders.path` repeats across drives —
        // filter by `drive_id` to scope the lookup.
        sqlx::query_scalar::<_, String>(
            "SELECT id::text FROM storage.folders \
             WHERE path = $1 AND drive_id = $2 AND NOT is_trashed",
        )
        .bind(folder_path)
        .bind(drive_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("folder lookup: {e}")))?
        .ok_or_else(|| DomainError::not_found("Folder", format!("path: {folder_path}")))
    }

    /// Direct SQL lookup using materialized folder paths, scoped to a drive.
    /// O(1) query instead of O(depth) folder walk.
    ///
    /// Post-D0 `storage.folders.path` repeats across drives (each drive
    /// has its own root with a name like `"Personal"`). Without the
    /// `drive_id` filter the lookup would be non-deterministic. The
    /// root-level branch filters on `fi.drive_id`; the nested branch
    /// filters on the parent folder's `fo.drive_id` (which closes the
    /// leak cleanly and matches the path semantics — see Step 2 of
    /// the path-lookup refactor).
    async fn find_file_by_path(
        &self,
        path: &str,
        drive_id: Uuid,
    ) -> Result<Option<File>, DomainError> {
        let path = path.trim_start_matches('/').trim_end_matches('/');
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if segments.is_empty() {
            return Ok(None);
        }

        // Last segment is the filename, preceding segments are the
        // folder path. NFC-normalize the filename so a NextCloud
        // client's NFD-encoded path still hits the NFC row stored
        // by a web upload — see `normalize_storage_name` for the
        // full rationale.
        let filename = crate::domain::services::path_service::normalize_storage_name(
            segments[segments.len() - 1],
        );
        let folder_path = segments[..segments.len() - 1].join("/");

        let row = if folder_path.is_empty() {
            // File at root level (no parent folder) — filter on
            // `fi.drive_id` because there's no folder row to join through.
            sqlx::query_as::<
                _,
                (
                    Uuid,
                    String,
                    Option<Uuid>,
                    Option<String>,
                    i64,
                    String,
                    i64,
                    i64,
                    String,
                    Option<Uuid>, // created_by (§14)
                    Option<Uuid>, // updated_by (§14)
                ),
            >(
                r#"
                SELECT fi.id, fi.name, fi.folder_id, fo.path,
                       fi.size, fi.mime_type,
                       EXTRACT(EPOCH FROM fi.created_at)::bigint,
                       EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                       fi.blob_hash,
                       fi.created_by, fi.updated_by
                  FROM storage.files fi
                  LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
                 WHERE fi.name = $1 AND fi.folder_id IS NULL
                   AND fi.drive_id = $2 AND NOT fi.is_trashed
                "#,
            )
            .bind(filename)
            .bind(drive_id)
            .fetch_optional(self.pool.as_ref())
            .await
        } else {
            // File inside a folder — look up by folder path + filename,
            // filtered by the parent folder's drive_id (path semantics
            // are folder-scoped, so this also catches mis-pointed file
            // rows during D0/D7's dual-write window).
            sqlx::query_as::<
                _,
                (
                    Uuid,
                    String,
                    Option<Uuid>,
                    Option<String>,
                    i64,
                    String,
                    i64,
                    i64,
                    String,
                    Option<Uuid>, // created_by (§14)
                    Option<Uuid>, // updated_by (§14)
                ),
            >(
                r#"
                SELECT fi.id, fi.name, fi.folder_id, fo.path,
                       fi.size, fi.mime_type,
                       EXTRACT(EPOCH FROM fi.created_at)::bigint,
                       EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                       fi.blob_hash,
                       fi.created_by, fi.updated_by
                  FROM storage.files fi
                  JOIN storage.folders fo ON fo.id = fi.folder_id
                 WHERE fo.path = $1 AND fi.name = $2
                   AND fo.drive_id = $3 AND NOT fi.is_trashed
                "#,
            )
            .bind(&folder_path)
            .bind(filename)
            .bind(drive_id)
            .fetch_optional(self.pool.as_ref())
            .await
        }
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("find file: {e}")))?;

        match row {
            Some(r) => Ok(Some(Self::row_to_file(
                r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10,
            )?)),
            None => Ok(None),
        }
    }

    /// Streams every file in the subtree rooted at `folder_id`.
    ///
    /// Single GiST-indexed query via ltree `<@`.  Results are delivered
    /// through a PostgreSQL cursor — RAM stays O(1) per row.
    async fn stream_files_in_subtree(
        &self,
        folder_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<File, DomainError>> + Send>>, DomainError> {
        let pool = Arc::clone(&self.pool);
        let folder_id = folder_id.to_owned();

        let stream = async_stream::try_stream! {
            let mut row_stream = sqlx::query_as::<_, (
                Uuid, String, Option<Uuid>, Option<String>,
                i64, String, i64, i64, String,
                Option<Uuid>, Option<Uuid>, // created_by, updated_by (§14)
            )>(
                r#"
                SELECT fi.id, fi.name, fi.folder_id, fo.path,
                       fi.size, fi.mime_type,
                       EXTRACT(EPOCH FROM fi.created_at)::bigint,
                       EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                       fi.blob_hash,
                       fi.created_by, fi.updated_by
                  FROM storage.files fi
                  JOIN storage.folders fo ON fo.id = fi.folder_id
                 WHERE fo.lpath <@ (SELECT lpath FROM storage.folders WHERE id = $1::uuid)
                   AND NOT fi.is_trashed
                 ORDER BY fo.path, fi.name
                "#,
            )
            .bind(&folder_id)
            .fetch(pool.as_ref());

            while let Some(row) = row_stream.try_next().await.map_err(|e| {
                DomainError::internal_error("FileBlobRead", format!("subtree stream: {e}"))
            })? {
                let (id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub) = row;
                let file = FileBlobReadRepository::row_to_file(
                    id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub,
                )?;
                yield file;
            }
        };

        Ok(Box::pin(stream))
    }

    /// Search files with filtering and pagination at database level.
    ///
    /// Uses `COUNT(*) OVER()` window function to return the total matching
    /// count alongside the paginated rows in a **single query** — no separate
    /// COUNT round-trip.
    ///
    /// Post-PR-B: scoped by drive-membership (via [`CALLER_CAN_READ_DRIVE`])
    /// rather than the legacy `fi.user_id = $caller` predicate. Group
    /// grants are honoured inline through `storage.caller_group_ids`.
    async fn search_files_paginated(
        &self,
        folder_id: Option<&str>,
        criteria: &SearchCriteriaDto,
        caller_id: Uuid,
    ) -> Result<(Vec<File>, Vec<(bool, bool)>, usize), DomainError> {
        let offset = criteria.offset as i64;
        let limit = criteria.limit as i64;

        // Determine sort order. Canonical `sort_by` set (matches the wire
        // `SearchResourcesQuery.order_by` 1:1 — no rename at any layer):
        // `relevance | name | size | updated_at | created_at`. Direction
        // comes from `criteria.reverse`; the old `_desc`-suffix pattern
        // was retired 2026-07-26.
        let order_column = match criteria.sort_by.as_str() {
            "name" => "fi.name",
            "updated_at" => "fi.updated_at",
            "created_at" => "fi.created_at",
            "size" => "fi.size",
            // `relevance` (or anything unrecognised) has no dedicated
            // column here — the recursive/non-recursive services blend
            // in content-index hits and re-sort in memory. Falling back
            // to name keeps the SQL page stable.
            _ => "fi.name",
        };
        let order_dir = if criteria.reverse { "DESC" } else { "ASC" };

        // ── Build dynamic WHERE + bind indices ───────────────────────────
        let mut conditions: Vec<String> = vec![
            CALLER_CAN_READ_DRIVE.to_string(),
            "fi.is_trashed = false".to_string(),
        ];
        let mut bind_idx = 1u32; // $1 = caller_id

        if folder_id.is_some() {
            bind_idx += 1;
            conditions.push(format!("fi.folder_id = ${bind_idx}::uuid"));
        }

        if let Some(name) = &criteria.name_contains
            && name.len() >= 3
        {
            bind_idx += 1;
            conditions.push(format!("fi.name ILIKE ${bind_idx}"));
        }

        let where_clause = conditions.join(" AND ");
        let limit_bind = bind_idx + 1;
        let offset_bind = bind_idx + 2;

        // Per-row caller flags — populated in-SQL so the search result
        // is a single round-trip. Mirrors the pattern used by the
        // photos-timeline listing above (see `list_photos_paginated`).
        // Both EXISTS clauses hit narrow indexes (favorites is keyed
        // on `(user_id, item_id, item_type)`; role_grants on
        // `(resource_type, resource_id)`), each a sub-ms lookup — the
        // cost across a 100-row search page is a few ms of index
        // probing vs a full second round-trip for the alternative
        // post-hoc batch approach.
        let sql = format!(
            "SELECT fi.id, fi.name, fi.folder_id, fo.path, \
                    fi.size, fi.mime_type, \
                    EXTRACT(EPOCH FROM fi.created_at)::bigint, \
                    EXTRACT(EPOCH FROM fi.updated_at)::bigint, \
                    fi.blob_hash, \
                    \
                    fi.created_by, fi.updated_by, \
                    EXISTS ( \
                        SELECT 1 FROM auth.user_favorites uf \
                         WHERE uf.user_id   = $1 \
                           AND uf.item_id   = fi.id::text \
                           AND uf.item_type = 'file' \
                    ) AS is_favorite, \
                    EXISTS ( \
                        SELECT 1 FROM storage.role_grants g \
                         WHERE g.resource_id   = fi.id \
                           AND g.resource_type = 'file' \
                    ) AS is_shared, \
                    COUNT(*) OVER() AS total_count \
               FROM storage.files fi \
               LEFT JOIN storage.folders fo ON fo.id = fi.folder_id \
              WHERE {where_clause} \
              ORDER BY {order_column} {order_dir} \
              LIMIT ${limit_bind} OFFSET ${offset_bind}"
        );

        // ── Bind parameters dynamically ──────────────────────────────────
        let mut query = sqlx::query_as::<
            _,
            (
                Uuid,
                String,
                Option<Uuid>,
                Option<String>,
                i64,
                String,
                i64,
                i64,
                String,
                Option<Uuid>, // created_by (§14)
                Option<Uuid>, // updated_by (§14)
                bool,         // is_favorite
                bool,         // is_shared
                i64,          // total_count
            ),
        >(&sql)
        .bind(caller_id);

        if let Some(fid) = folder_id {
            query = query.bind(fid);
        }
        if let Some(name) = &criteria.name_contains
            && name.len() >= 3
        {
            query = query.bind(super::like_escape(name));
        }
        query = query.bind(limit).bind(offset);

        // ── Execute single query ─────────────────────────────────────────
        let rows = query
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("FileBlobRead", format!("search: {e}")))?;

        // total_count is the same in every row; 0 when result set is empty.
        let total_count = rows.first().map_or(0, |r| r.13) as usize;

        // Pre-size both parallel Vecs (size-hint note in `hydrate`,
        // ROUND20 §I1). Caller-flags stay aligned with `files` by index.
        let mut files = Vec::with_capacity(rows.len());
        let mut caller_flags = Vec::with_capacity(rows.len());
        for (id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub, is_fav, is_shr, _total) in
            rows
        {
            files.push(
                Self::row_to_file(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)
                    .map_err(|e| {
                        DomainError::internal_error("FileBlobRead", format!("mapping: {e}"))
                    })?,
            );
            caller_flags.push((is_fav, is_shr));
        }

        Ok((files, caller_flags, total_count))
    }

    /// Recursive subtree search using ltree — single SQL query.
    ///
    /// When `root_folder_id` is Some, JOINs `storage.files` with
    /// `storage.folders` using `lpath <@ (root's lpath)` to find all
    /// files in the entire subtree.
    /// When None, delegates to `search_files_paginated`.
    ///
    /// Uses `COUNT(*) OVER()` to return the total count alongside the
    /// paginated rows — no separate COUNT round-trip.
    ///
    /// Post-PR-B: scoped by drive-membership (via [`CALLER_CAN_READ_DRIVE`])
    /// rather than the legacy `fi.user_id = $caller` predicate — same
    /// group-cascade semantics as `search_files_paginated`.
    async fn search_files_in_subtree(
        &self,
        root_folder_id: Option<&str>,
        criteria: &SearchCriteriaDto,
        caller_id: Uuid,
    ) -> Result<(Vec<File>, Vec<(bool, bool)>, usize), DomainError> {
        // When no root folder specified, delegate to existing paginated search
        let root_id = match root_folder_id {
            None => {
                return self.search_files_paginated(None, criteria, caller_id).await;
            }
            Some(id) => id,
        };

        let offset = criteria.offset as i64;
        let limit = criteria.limit as i64;

        // Determine sort order. Canonical `sort_by` set (matches the wire
        // `SearchResourcesQuery.order_by` 1:1 — no rename at any layer):
        // `relevance | name | size | updated_at | created_at`. Direction
        // comes from `criteria.reverse`; the old `_desc`-suffix pattern
        // was retired 2026-07-26.
        let order_column = match criteria.sort_by.as_str() {
            "name" => "fi.name",
            "updated_at" => "fi.updated_at",
            "created_at" => "fi.created_at",
            "size" => "fi.size",
            // `relevance` (or anything unrecognised) has no dedicated
            // column here — the recursive/non-recursive services blend
            // in content-index hits and re-sort in memory. Falling back
            // to name keeps the SQL page stable.
            _ => "fi.name",
        };
        let order_dir = if criteria.reverse { "DESC" } else { "ASC" };

        // ── Build dynamic WHERE clauses ──
        let mut conditions = Vec::new();
        let mut bind_idx = 2u32; // $1 = caller_id, $2 = root_folder_id

        conditions.push("fi.is_trashed = false".to_string());
        conditions.push(CALLER_CAN_READ_DRIVE.to_string());
        conditions.push(
            "fo.lpath <@ (SELECT lpath FROM storage.folders WHERE id = $2::uuid)".to_string(),
        );

        if let Some(name) = &criteria.name_contains
            && name.len() >= 3
        {
            bind_idx += 1;
            conditions.push(format!("fi.name ILIKE ${bind_idx}"));
        }
        bind_idx = push_criteria_filters(&mut conditions, bind_idx, criteria);

        let where_clause = conditions.join(" AND ");
        let limit_bind = bind_idx + 1;
        let offset_bind = bind_idx + 2;

        // ── Single query with COUNT(*) OVER() + per-row caller flags ──
        // See `search_files_paginated` above for the EXISTS-subquery
        // rationale; same shape applies here.
        let sql = format!(
            "SELECT fi.id, fi.name, fi.folder_id, fo.path, \
                    fi.size, fi.mime_type, \
                    EXTRACT(EPOCH FROM fi.created_at)::bigint, \
                    EXTRACT(EPOCH FROM fi.updated_at)::bigint, \
                    fi.blob_hash, \
                    \
                    fi.created_by, fi.updated_by, \
                    EXISTS ( \
                        SELECT 1 FROM auth.user_favorites uf \
                         WHERE uf.user_id   = $1 \
                           AND uf.item_id   = fi.id::text \
                           AND uf.item_type = 'file' \
                    ) AS is_favorite, \
                    EXISTS ( \
                        SELECT 1 FROM storage.role_grants g \
                         WHERE g.resource_id   = fi.id \
                           AND g.resource_type = 'file' \
                    ) AS is_shared, \
                    COUNT(*) OVER() AS total_count \
               FROM storage.files fi \
               JOIN storage.folders fo ON fo.id = fi.folder_id \
              WHERE {where_clause} \
              ORDER BY {order_column} {order_dir} \
              LIMIT ${limit_bind} OFFSET ${offset_bind}"
        );

        // ── Bind parameters dynamically ──
        let mut query = sqlx::query_as::<
            _,
            (
                Uuid,
                String,
                Option<Uuid>,
                Option<String>,
                i64,
                String,
                i64,
                i64,
                String,
                Option<Uuid>, // created_by (§14)
                Option<Uuid>, // updated_by (§14)
                bool,         // is_favorite
                bool,         // is_shared
                i64,          // total_count
            ),
        >(&sql)
        .bind(caller_id)
        .bind(root_id);

        if let Some(name) = &criteria.name_contains
            && name.len() >= 3
        {
            query = query.bind(super::like_escape(name));
        }
        query = bind_criteria_filters(query, criteria);

        query = query.bind(limit).bind(offset);

        // ── Execute single query ──
        let rows = query.fetch_all(self.pool.as_ref()).await.map_err(|e| {
            DomainError::internal_error("FileBlobRead", format!("subtree search: {e}"))
        })?;

        let total_count = rows.first().map_or(0, |r| r.13) as usize;

        // Pre-size both Vecs, keep caller_flags aligned by index.
        let mut files = Vec::with_capacity(rows.len());
        let mut caller_flags = Vec::with_capacity(rows.len());
        for (id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub, is_fav, is_shr, _total) in
            rows
        {
            files.push(
                Self::row_to_file(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)
                    .map_err(|e| {
                        DomainError::internal_error("FileBlobRead", format!("subtree mapping: {e}"))
                    })?,
            );
            caller_flags.push((is_fav, is_shr));
        }

        Ok((files, caller_flags, total_count))
    }

    #[allow(clippy::type_complexity)]
    async fn suggest_files_by_name(
        &self,
        folder_id: Option<&str>,
        query: &str,
        limit: usize,
        caller_id: Uuid,
    ) -> Result<Vec<File>, DomainError> {
        // Scope by drive membership: `CALLER_CAN_READ_DRIVE` (`$1` =
        // caller_id) restricts the result set to files whose owning drive
        // the caller has any active `role_grants` on — direct or via a
        // transitive group cascade. Pre-fix, the query only filtered on
        // `NOT is_trashed AND name ILIKE $pattern`, exposing names + paths
        // across every tenant on the instance (AuthZ audit finding #1,
        // 2026-07-12).
        let pattern = super::like_escape(query);
        let limit_i64 = limit as i64;

        let rows: Vec<FileRow> = if let Some(fid) = folder_id {
            sqlx::query_as(&format!(
                r#"
                SELECT fi.id, fi.name, fi.folder_id, fo.path,
                       fi.size, fi.mime_type,
                       EXTRACT(EPOCH FROM fi.created_at)::bigint,
                       EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                       fi.blob_hash,

                   fi.created_by, fi.updated_by
                  FROM storage.files fi
                  LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
                 WHERE {CALLER_CAN_READ_DRIVE}
                   AND fi.folder_id = $2::uuid
                   AND NOT fi.is_trashed
                   AND fi.name ILIKE $3
                 ORDER BY CASE
                            WHEN fi.name ILIKE $4 THEN 0
                            WHEN fi.name ILIKE $4 || '%' THEN 1
                            ELSE 2
                          END,
                          fi.name
                 LIMIT $5
                "#
            ))
            .bind(caller_id)
            .bind(fid)
            .bind(&pattern)
            .bind(query)
            .bind(limit_i64)
            .fetch_all(self.pool.as_ref())
            .await
        } else {
            sqlx::query_as(&format!(
                r#"
                SELECT fi.id, fi.name, fi.folder_id, fo.path,
                       fi.size, fi.mime_type,
                       EXTRACT(EPOCH FROM fi.created_at)::bigint,
                       EXTRACT(EPOCH FROM fi.updated_at)::bigint,
                       fi.blob_hash,

                   fi.created_by, fi.updated_by
                  FROM storage.files fi
                  LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
                 WHERE {CALLER_CAN_READ_DRIVE}
                   AND fi.folder_id IS NULL
                   AND NOT fi.is_trashed
                   AND fi.name ILIKE $2
                 ORDER BY CASE
                            WHEN fi.name ILIKE $3 THEN 0
                            WHEN fi.name ILIKE $3 || '%' THEN 1
                            ELSE 2
                          END,
                          fi.name
                 LIMIT $4
                "#
            ))
            .bind(caller_id)
            .bind(&pattern)
            .bind(query)
            .bind(limit_i64)
            .fetch_all(self.pool.as_ref())
            .await
        }
        .map_err(|e| DomainError::internal_error("FileBlobRead", format!("suggest: {e}")))?;

        rows.into_iter()
            .map(
                |(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)| {
                    Self::row_to_file(id, name, fid, fpath, size, mime, ca, ma, blob_hash, cb, ub)
                },
            )
            .collect()
    }
}

#[cfg(feature = "integration_tests")]
#[allow(dead_code)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::common::stubs::StubDedupPort;
    use crate::infrastructure::repositories::pg::folder_db_repository::FolderDbRepository;

    /// Helper: build a `FileBlobReadRepository` without a real PgPool.
    /// Only the moka `hash_cache` is exercised — no SQL is executed.
    fn make_repo() -> FileBlobReadRepository {
        let _folder_repo = Arc::new(FolderDbRepository::new_stub());
        let dedup: Arc<DedupService> = Arc::new(DedupService::new_stub());
        FileBlobReadRepository {
            pool: Arc::new(
                sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                    .max_connections(1)
                    .connect_lazy("postgres://invalid:5432/none")
                    .unwrap(),
            ),
            dedup,
            hash_cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_idle(Duration::from_secs(30))
                .build(),
        }
    }

    #[tokio::test]
    async fn test_cache_insert_and_persist() {
        let repo = make_repo();

        repo.hash_cache
            .insert("file-1".to_string(), "abc123".to_string());

        // First read
        assert_eq!(repo.hash_cache.get("file-1").as_deref(), Some("abc123"));

        // Second read — entry must still be present (no longer one-shot)
        assert_eq!(
            repo.hash_cache.get("file-1").as_deref(),
            Some("abc123"),
            "Entry must persist across multiple reads"
        );
    }

    #[tokio::test]
    async fn test_cache_miss_returns_none() {
        let repo = make_repo();

        assert!(
            repo.hash_cache.get("nonexistent").is_none(),
            "Cache miss must return None"
        );
    }

    #[tokio::test]
    async fn test_cache_multiple_files_independent() {
        let repo = make_repo();

        repo.hash_cache
            .insert("file-a".to_string(), "hash-a".to_string());
        repo.hash_cache
            .insert("file-b".to_string(), "hash-b".to_string());

        // Reading file-a must not affect file-b
        assert_eq!(repo.hash_cache.get("file-a").as_deref(), Some("hash-a"));
        assert_eq!(repo.hash_cache.get("file-a").as_deref(), Some("hash-a"));
        assert_eq!(
            repo.hash_cache.get("file-b").as_deref(),
            Some("hash-b"),
            "Independent entries must not interfere"
        );
    }

    #[tokio::test]
    async fn test_cache_overwrite_updates_value() {
        let repo = make_repo();

        repo.hash_cache
            .insert("file-1".to_string(), "old-hash".to_string());
        repo.hash_cache
            .insert("file-1".to_string(), "new-hash".to_string());

        assert_eq!(
            repo.hash_cache.get("file-1").as_deref(),
            Some("new-hash"),
            "Last insert wins"
        );
    }

    #[tokio::test]
    async fn test_cache_capacity_eviction() {
        // Build a tiny cache to verify eviction behaviour
        let repo = FileBlobReadRepository {
            pool: Arc::new(
                sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                    .max_connections(1)
                    .connect_lazy("postgres://invalid:5432/none")
                    .unwrap(),
            ),
            dedup: Arc::new(DedupService::new_stub()),
            hash_cache: Cache::builder()
                .max_capacity(2) // only 2 entries
                .build(),
        };

        repo.hash_cache.insert("a".to_string(), "ha".to_string());
        repo.hash_cache.insert("b".to_string(), "hb".to_string());
        repo.hash_cache.insert("c".to_string(), "hc".to_string());

        // Force moka to run pending eviction tasks
        repo.hash_cache.run_pending_tasks();

        // At most 2 entries should survive
        let alive = ["a", "b", "c"]
            .iter()
            .filter(|k| repo.hash_cache.get(**k).is_some())
            .count();
        assert!(
            alive <= 2,
            "Cache must evict when capacity is exceeded (alive: {alive})"
        );
    }

    #[tokio::test]
    async fn test_cache_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let repo = Arc::new(make_repo());
        let mut handles = vec![];

        // Spawn 50 threads doing inserts + reads simultaneously
        for i in 0..50 {
            let repo = Arc::clone(&repo);
            handles.push(thread::spawn(move || {
                let key = format!("file-{i}");
                let hash = format!("hash-{i}");
                repo.hash_cache.insert(key.clone(), hash.clone());
                // Read back — should be our value or already evicted, never panic
                let _ = repo.hash_cache.get(&key);
            }));
        }

        for h in handles {
            h.join()
                .expect("Thread must not panic — no poison possible with moka");
        }
    }
}
