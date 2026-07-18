//! PostgreSQL implementation of [`DriveRepository`].
//!
//! The repo deals only with the `storage.drives` table itself. Drive
//! membership lives in `storage.role_grants` (`resource_type='drive'`)
//! and is queried through the engine's existing grant paths;
//! `list_readable_by` below resolves `role_grants` → `storage.drives`
//! via a single join.
//!
//! See `migrations/20260802000000_drives_schema_additive.sql` for the
//! schema and `docs/plan/drive.md` §3 / §15 for the locked design.

use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use sqlx::{PgPool, Row, types::Uuid};

use crate::domain::entities::drive::{Drive, DriveKind};
use crate::domain::repositories::drive_repository::{
    DriveRepository, DriveRepositoryError, DriveWithRootName,
};

/// Decode a `d.policies` JSONB column straight into `DrivePolicies` via
/// `sqlx::types::Json<T>` — a single `serde_json::from_slice` over the raw JSONB
/// bytes — instead of fetching a throwaway `serde_json::Value` DOM and walking it
/// once with `DrivePolicies::from_value`. The §J1 pattern (ROUND23) applied to
/// the drive-policy path §J2 left behind (benches/ROUND26.md §P1). The lenient
/// `unwrap_or_default` fallback (a malformed bag decodes to all-false rather than
/// erroring the read) is preserved exactly.
fn policies_from_row(row: &sqlx::postgres::PgRow) -> crate::domain::entities::drive::DrivePolicies {
    row.try_get::<sqlx::types::Json<crate::domain::entities::drive::DrivePolicies>, _>("policies")
        .map(|j| j.0)
        .unwrap_or_default()
}

/// `default_drive_cache` TTL. The default-drive → root-folder binding is
/// nearly immutable (changes only on provisioning / drive deletion /
/// policy edits — all of which invalidate explicitly below), yet it is
/// re-resolved on EVERY NextCloud request (basic-auth chroot), every
/// native `/webdav` request (Mode-B scope resolution) and every WOPI
/// call. 30 s mirrors `drive_role_cache` in `pg_acl_engine.rs`. Root-
/// folder renames — which don't pass through this repository directly
/// — invalidate via the `DriveRepository::invalidate_default_drive_all`
/// trait hook called from `folder_service::rename_folder_with_perms`
/// when `parent_id IS NULL`. Measured in `benches/CHROOT-CACHE.md`.
const DEFAULT_DRIVE_CACHE_TTL: Duration = Duration::from_secs(30);

/// One entry per active user; entries are small (a `Drive` + a name).
const DEFAULT_DRIVE_CACHE_CAPACITY: u64 = 100_000;

pub struct DrivePgRepository {
    pool: Arc<PgPool>,
    /// user_id → default drive (+ root folder name). See
    /// [`DEFAULT_DRIVE_CACHE_TTL`]. Only `Ok` results are cached, so the
    /// provisioning idempotency check (`NotFound` → create) always sees
    /// the live table.
    default_drive_cache: Cache<Uuid, DriveWithRootName>,
    /// caller_id → every drive the caller can read (the full
    /// role_grants ⋈ drives ⋈ folders join of [`list_readable_by`],
    /// including the transitive-group expansion).
    ///
    /// Re-resolved before this cache existed on EVERY native `/webdav`
    /// request that names an explicit drive selector (all verbs; MOVE
    /// and COPY twice), plus per-request in search, trash listing and
    /// the `GET /api/drives` picker — the heaviest per-request query
    /// left on the DAV path after CHROOT-CACHE. Concurrent misses are
    /// coalesced (`try_get_with`), errors are never cached.
    ///
    /// Freshness: every membership/lifecycle mutation that flows
    /// through this repository or `DriveManagementService` invalidates
    /// explicitly (per-user when the subject is a User, whole cache for
    /// Group subjects, whose transitive membership is not resolvable
    /// here). Two further write paths carry a cached field but don't
    /// flow through this repository — a drive-root folder rename
    /// (`FolderService::rename_folder_with_perms`) and a `used_bytes`
    /// reconciliation sweep (`StorageUsageService::invalidate_drive_lookup_caches`,
    /// called after `update_all_drives_storage_usage`) — both reach
    /// back in via the trait's `invalidate_readable_all`/
    /// `invalidate_default_drive_all` hooks rather than waiting out the
    /// TTL. That gap was missed by the perf commit that introduced this
    /// cache (`12dc648c`) and surfaced by `drives_membership.hurl` Step
    /// 23 and `drive_quota.hurl` Step 6. Residual staleness — a grant
    /// written by a path that can't reach this cache at all — is
    /// bounded by the same 30 s TTL the sibling caches accept; actual
    /// permission enforcement is unaffected (the ACL engine re-checks
    /// per operation with its own invalidation).
    readable_cache: Cache<Uuid, Arc<Vec<DriveWithRootName>>>,
}

impl DrivePgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self {
            pool,
            default_drive_cache: Cache::builder()
                .max_capacity(DEFAULT_DRIVE_CACHE_CAPACITY)
                .time_to_live(DEFAULT_DRIVE_CACHE_TTL)
                .build(),
            readable_cache: Cache::builder()
                .max_capacity(DEFAULT_DRIVE_CACHE_CAPACITY)
                .time_to_live(DEFAULT_DRIVE_CACHE_TTL)
                .build(),
        }
    }

    /// Drop the cached readable-drive list for one user (their grant set
    /// changed: membership write, personal-drive provisioning, …).
    pub async fn invalidate_readable_for_user(&self, user_id: Uuid) {
        self.readable_cache.invalidate(&user_id).await;
    }

    /// Drop every cached readable-drive list. Used when the affected
    /// user set is unknown at this layer: group-subject grants, drive
    /// deletion, policy edits. All are admin-rare; repopulation costs
    /// one join per active caller.
    pub fn invalidate_readable_all(&self) {
        self.readable_cache.invalidate_all();
    }

    /// Drop every cached `default_drive_cache` entry. Exposed as a
    /// `pub` sibling of the whole-cache invalidators above so trait
    /// callers holding a `dyn DriveRepository` can trigger the same
    /// cleanup path (e.g. `folder_service` on root-folder rename —
    /// see `impl DriveRepository` below).
    pub fn invalidate_default_drive_all(&self) {
        self.default_drive_cache.invalidate_all();
    }

    fn map_sqlx_err(context: &'static str, e: sqlx::Error) -> DriveRepositoryError {
        if let sqlx::Error::Database(ref dberr) = e
            && let Some(code) = dberr.code()
            && code.as_ref() == "23505"
        {
            // unique_violation. With drives, the only relevant unique is
            // the partial index `idx_drives_default_for_user_unique` —
            // surface the typed variant so the lifecycle hook can detect
            // idempotent re-runs (D0-9 calls create_personal_drive_atomic
            // during user provisioning).
            return DriveRepositoryError::DefaultDriveAlreadyExists(dberr.to_string());
        }
        DriveRepositoryError::StorageError(format!("{context}: {e}"))
    }

    /// Map a row carrying both the drive's columns AND a `root_folder_name`
    /// column (sourced via JOIN with `storage.folders`) into the view-model.
    /// `caller_role` is left `None` — only the listing path (which
    /// JOINs `role_grants` for accessibility) has it in scope; see
    /// `row_to_drive_with_name_and_role`.
    fn row_to_drive_with_name(
        row: &sqlx::postgres::PgRow,
    ) -> Result<DriveWithRootName, DriveRepositoryError> {
        let kind_str: String = row.get("kind");
        let kind = DriveKind::from_sql(&kind_str)?;
        let drive = Drive {
            id: row.get("id"),
            kind,
            default_for_user: row.get("default_for_user"),
            root_folder_id: row.get("root_folder_id"),
            quota_bytes: row.get("quota_bytes"),
            used_bytes: row.get("used_bytes"),
            policies: row.get("policies"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        };
        Ok(DriveWithRootName {
            drive,
            root_folder_name: row.get("root_folder_name"),
            caller_role: None,
        })
    }

    /// Same as `row_to_drive_with_name` but reads `caller_role` from the
    /// listing query — `MIN(g.role)::text`. The `storage.grant_role` ENUM
    /// is declared owner→viewer (strongest→weakest), so `MIN` picks the
    /// strongest of the caller's grants on the drive (direct +
    /// group-mediated collapsed by GROUP BY). Used only by
    /// `list_readable_by`.
    fn row_to_drive_with_name_and_role(
        row: &sqlx::postgres::PgRow,
    ) -> Result<DriveWithRootName, DriveRepositoryError> {
        use crate::domain::services::authorization::Role;
        let mut dwr = Self::row_to_drive_with_name(row)?;
        let role_str: Option<String> = row.try_get("caller_role").ok();
        dwr.caller_role = role_str.as_deref().and_then(Role::parse);
        Ok(dwr)
    }

    /// The uncached grants join behind [`DriveRepository::list_readable_by`].
    ///
    /// Joining role_grants → drives → folders returns every drive the
    /// caller can read, paired with its display name. Group
    /// memberships (direct + transitive) are expanded inline by
    /// `storage.caller_group_ids($caller)` — no Rust-side ceremony.
    ///
    /// ORDER BY puts default drives first (so the picker UI doesn't
    /// need a follow-up sort), then alphabetical by name. GROUP BY
    /// collapses duplicate role_grants on the same drive (direct +
    /// group-mediated) and sidesteps PostgreSQL's "ORDER BY
    /// expression must appear in select list" rule that SELECT
    /// DISTINCT imposes.
    /// `MIN(g.role)` picks the caller's strongest role on each drive:
    /// `storage.grant_role` is declared `owner → viewer` (strongest →
    /// weakest), so MIN returns the strongest. Cast `::text` matches
    /// the codebase convention for reading enum columns into Rust
    /// (see `pg_acl_engine.rs`); `Role::parse` handles the trip back.
    async fn query_readable_by(
        &self,
        caller_id: Uuid,
    ) -> Result<Vec<DriveWithRootName>, DriveRepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT d.id, d.kind, d.default_for_user, d.root_folder_id,
                   d.quota_bytes, d.used_bytes, d.policies,
                   d.created_at, d.updated_at,
                   f.name AS root_folder_name,
                   MIN(g.role)::text AS caller_role
              FROM storage.drives d
              JOIN storage.folders f ON f.id = d.root_folder_id
              JOIN storage.role_grants g
                ON g.resource_type = 'drive'
               AND g.resource_id   = d.id
             WHERE (
                     (g.subject_type = 'user'  AND g.subject_id = $1)
                  OR (g.subject_type = 'group' AND g.subject_id IN
                          (SELECT storage.caller_group_ids($1)))
                   )
               AND (g.expires_at IS NULL OR g.expires_at > NOW())
             GROUP BY d.id, d.kind, d.default_for_user, d.root_folder_id,
                      d.quota_bytes, d.used_bytes, d.policies,
                      d.created_at, d.updated_at, f.name
             ORDER BY (d.default_for_user IS NULL) ASC,
                      LOWER(f.name) ASC
            "#,
        )
        .bind(caller_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("list_readable_by", e))?;

        rows.iter()
            .map(Self::row_to_drive_with_name_and_role)
            .collect()
    }
}

#[async_trait::async_trait]
impl DriveRepository for DrivePgRepository {
    async fn invalidate_readable_for_user(&self, user_id: Uuid) {
        // Delegate to the inherent method — the trait forwarding lets
        // callers holding a `dyn DriveRepository` (e.g. `folder_service`
        // on a root-folder rename) trigger invalidation without knowing
        // about the concrete cache.
        DrivePgRepository::invalidate_readable_for_user(self, user_id).await;
    }

    fn invalidate_readable_all(&self) {
        DrivePgRepository::invalidate_readable_all(self);
    }

    fn invalidate_default_drive_all(&self) {
        DrivePgRepository::invalidate_default_drive_all(self);
    }

    async fn create_personal_drive_atomic(
        &self,
        owner_id: Uuid,
        quota_bytes: Option<i64>,
    ) -> Result<DriveWithRootName, DriveRepositoryError> {
        // Four writes wrapped in a single transaction so either all
        // commit or none does (docs/plan/drive.md §3). A single CTE
        // statement would be cleaner on paper but doesn't work in
        // PostgreSQL: CTE sub-statements share an MVCC snapshot, so
        // `UPDATE storage.drives WHERE id = …` cannot match a row
        // inserted by an earlier CTE branch. We use plain sequential
        // statements inside `pool.begin()` instead — each statement
        // sees the prior ones' writes (transaction-local visibility),
        // and FK constraints are satisfied at insert time because the
        // referenced rows already exist.
        //
        // Rollback semantics: any error before `tx.commit()` (FK
        // violation, unique_violation on `default_for_user`, server
        // crash) discards every partial write. No orphan drive, no
        // folder without a drive, no drive without an owner.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Self::map_sqlx_err("create_personal_drive_atomic.begin", e))?;

        // 1. Drive row (root_folder_id NULL — populated in step 3).
        //
        // Default personal drives are seeded with `include_in_photo_index`
        // + `include_in_music_index` = true so the Photos / Music
        // predicates (§15) can be a single positive rule keyed off the
        // JSONB flag — no per-kind carve-out needed at query time. Any
        // future admin PATCH toggling either flag off shows a confirm
        // dialog in the UI (unusual action; empties the user's Photos
        // timeline / Music library).
        let drive_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO storage.drives
                (kind, default_for_user, quota_bytes, policies)
            VALUES (
                'personal', $1, $2,
                '{"include_in_photo_index": true, "include_in_music_index": true}'::jsonb
            )
            RETURNING id
            "#,
        )
        .bind(owner_id)
        .bind(quota_bytes)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Self::map_sqlx_err("create_personal_drive_atomic.drive", e))?;

        // 2. Root folder. `parent_id IS NULL` makes it a root in the
        //    drive; `drive_id` closes the FK in this direction.
        //
        //    Post-D7: `user_id` omitted from the INSERT column list —
        //    the column is nullable and no longer written to on new
        //    rows. `created_by` / `updated_by` bind to the owner
        //    (§14 provenance).
        let folder_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO storage.folders
                (name, parent_id, drive_id, created_by, updated_by)
            VALUES ('Personal', NULL, $2, $1, $1)
            RETURNING id
            "#,
        )
        .bind(owner_id)
        .bind(drive_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Self::map_sqlx_err("create_personal_drive_atomic.folder", e))?;

        // 3. Close the other side of the circular reference.
        sqlx::query(r#"UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2"#)
            .bind(folder_id)
            .bind(drive_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::map_sqlx_err("create_personal_drive_atomic.wire", e))?;

        // 4. Owner role_grant — the caller becomes the drive's sole
        //    owner (single-user invariant on personal drives, §2).
        sqlx::query(
            r#"
            INSERT INTO storage.role_grants
                (subject_type, subject_id, resource_type, resource_id,
                 role, granted_by)
            VALUES ('user', $1, 'drive', $2, 'owner', $1)
            "#,
        )
        .bind(owner_id)
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| Self::map_sqlx_err("create_personal_drive_atomic.grant", e))?;

        // Fetch the row in its final state so the caller gets a
        // consistent view (including DB-computed defaults like
        // `created_at`, `used_bytes`).
        let row = sqlx::query(
            r#"
            SELECT d.id, d.kind, d.default_for_user, d.root_folder_id,
                   d.quota_bytes, d.used_bytes, d.policies,
                   d.created_at, d.updated_at,
                   f.name AS root_folder_name
              FROM storage.drives d
              JOIN storage.folders f ON f.id = d.root_folder_id
             WHERE d.id = $1
            "#,
        )
        .bind(drive_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Self::map_sqlx_err("create_personal_drive_atomic.read", e))?;

        tx.commit()
            .await
            .map_err(|e| Self::map_sqlx_err("create_personal_drive_atomic.commit", e))?;

        // Drop any cached default-drive resolution for this user (a stale
        // NotFound is never cached, but be explicit about the write path).
        self.default_drive_cache.invalidate(&owner_id).await;
        // The owner gained a drive — their readable list changed too.
        self.invalidate_readable_for_user(owner_id).await;

        Self::row_to_drive_with_name(&row)
    }

    async fn create_shared_drive_atomic(
        &self,
        name: &str,
        owner_subject: crate::domain::services::authorization::Subject,
        quota_bytes: Option<i64>,
        granted_by: Uuid,
    ) -> Result<DriveWithRootName, DriveRepositoryError> {
        // Same four-write transaction shape as `create_personal_drive_atomic`
        // (see that method for the why-not-CTE explanation). Differences:
        //   - `kind='shared'`, `default_for_user=NULL`.
        //   - Root folder name is caller-supplied.
        //   - Owner grant subject is caller-supplied — either a single
        //     User (becomes the sole drive Owner) or a Group (transitive
        //     members inherit Owner via subject expansion).
        //   - `granted_by` is the OxiCloud admin who provisioned the drive;
        //     same value goes onto the folder's `created_by`/`updated_by`
        //     for §14 provenance.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Self::map_sqlx_err("create_shared_drive_atomic.begin", e))?;

        // 1. Drive row (root_folder_id NULL — populated in step 3).
        let drive_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO storage.drives
                (kind, default_for_user, quota_bytes, policies)
            VALUES ('shared', NULL, $1, '{}'::jsonb)
            RETURNING id
            "#,
        )
        .bind(quota_bytes)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Self::map_sqlx_err("create_shared_drive_atomic.drive", e))?;

        // 2. Root folder. Post-D7: `user_id` omitted — the column is
        //    nullable and unused on new rows. `created_by` / `updated_by`
        //    bind to `granted_by` (§14 provenance).
        let folder_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO storage.folders
                (name, parent_id, drive_id, created_by, updated_by)
            VALUES ($1, NULL, $3, $2, $2)
            RETURNING id
            "#,
        )
        .bind(name)
        .bind(granted_by)
        .bind(drive_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Self::map_sqlx_err("create_shared_drive_atomic.folder", e))?;

        // 3. Close the circular reference (drive ↔ root folder).
        sqlx::query(r#"UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2"#)
            .bind(folder_id)
            .bind(drive_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::map_sqlx_err("create_shared_drive_atomic.wire", e))?;

        // 4. Owner role_grant — subject_type chosen from the caller's input.
        //    Group subjects expand transitively via `subject_match_set` so
        //    every member inherits Owner; User subjects are the single
        //    admin case.
        sqlx::query(
            r#"
            INSERT INTO storage.role_grants
                (subject_type, subject_id, resource_type, resource_id,
                 role, granted_by)
            VALUES ($1, $2, 'drive', $3, 'owner', $4)
            "#,
        )
        .bind(owner_subject.type_str())
        .bind(owner_subject.id())
        .bind(drive_id)
        .bind(granted_by)
        .execute(&mut *tx)
        .await
        .map_err(|e| Self::map_sqlx_err("create_shared_drive_atomic.grant", e))?;

        // Fetch final state so the caller sees DB-computed defaults.
        let row = sqlx::query(
            r#"
            SELECT d.id, d.kind, d.default_for_user, d.root_folder_id,
                   d.quota_bytes, d.used_bytes, d.policies,
                   d.created_at, d.updated_at,
                   f.name AS root_folder_name
              FROM storage.drives d
              JOIN storage.folders f ON f.id = d.root_folder_id
             WHERE d.id = $1
            "#,
        )
        .bind(drive_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Self::map_sqlx_err("create_shared_drive_atomic.read", e))?;

        tx.commit()
            .await
            .map_err(|e| Self::map_sqlx_err("create_shared_drive_atomic.commit", e))?;

        // The owner grant written above changes the grantee's readable
        // list. User subjects invalidate precisely; Group subjects fall
        // back to a full clear (transitive members unknown here).
        match owner_subject {
            crate::domain::services::authorization::Subject::User(uid) => {
                self.invalidate_readable_for_user(uid).await;
            }
            _ => self.invalidate_readable_all(),
        }

        Self::row_to_drive_with_name(&row)
    }

    async fn is_empty(&self, drive_id: Uuid) -> Result<bool, DriveRepositoryError> {
        // A "live" non-root folder = any folder with `parent_id IS NOT
        // NULL` (root is the only NULL-parent row per drive) and not in
        // the trash. Trashed items don't count — owners can delete a
        // drive even when its trash bin still holds rows; the trash GC
        // will clean those up after the standard retention window.
        //
        // EXISTS instead of COUNT(*): only emptiness is tested, so the
        // planner stops at the first matching row — a populated drive
        // answers from one index probe instead of aggregating every
        // live file + folder it contains.
        let occupied: (bool,) = sqlx::query_as(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM storage.folders
                 WHERE drive_id = $1 AND parent_id IS NOT NULL AND NOT is_trashed)
              OR EXISTS(
                SELECT 1 FROM storage.files
                 WHERE drive_id = $1 AND NOT is_trashed)
            "#,
        )
        .bind(drive_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("is_empty", e))?;
        Ok(!occupied.0)
    }

    async fn delete_atomic(&self, drive_id: Uuid) -> Result<(), DriveRepositoryError> {
        // Three-statement transaction:
        //   1. Drop every role_grants row scoped to the drive itself
        //      (folder/file grants under it are gone by step 3 cascade).
        //   2. Look up the root folder id (we'll need it to delete the
        //      folder row AFTER the drive row releases its FK).
        //   3. Delete the drive — release the drive→root FK first.
        //   4. Delete the root folder (drive_id FK on folders cascades
        //      from this row going away; only the root remains because
        //      is_empty was true).
        //
        // `drive_id` is bound once per statement; failure at any step
        // rolls back. Caller (`DriveManagementService::delete_drive`)
        // is responsible for the `is_empty` precheck.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Self::map_sqlx_err("delete_atomic.begin", e))?;

        sqlx::query(
            "DELETE FROM storage.role_grants \
             WHERE resource_type = 'drive' AND resource_id = $1",
        )
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| Self::map_sqlx_err("delete_atomic.grants", e))?;

        let root: (Uuid,) =
            sqlx::query_as("SELECT root_folder_id FROM storage.drives WHERE id = $1")
                .bind(drive_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| Self::map_sqlx_err("delete_atomic.lookup_root", e))?
                .ok_or_else(|| DriveRepositoryError::NotFound(drive_id.to_string()))?;

        sqlx::query("DELETE FROM storage.drives WHERE id = $1")
            .bind(drive_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::map_sqlx_err("delete_atomic.drive", e))?;

        sqlx::query("DELETE FROM storage.folders WHERE id = $1")
            .bind(root.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::map_sqlx_err("delete_atomic.root", e))?;

        tx.commit()
            .await
            .map_err(|e| Self::map_sqlx_err("delete_atomic.commit", e))?;
        // We only have the drive id here; the caches are keyed by user.
        // Deletion is rare — clearing them whole is the simple,
        // always-correct move (repopulates at one query per active user).
        self.default_drive_cache.invalidate_all();
        self.invalidate_readable_all();
        Ok(())
    }

    async fn get_by_id(&self, id: Uuid) -> Result<DriveWithRootName, DriveRepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT d.id, d.kind, d.default_for_user, d.root_folder_id,
                   d.quota_bytes, d.used_bytes, d.policies,
                   d.created_at, d.updated_at,
                   f.name AS root_folder_name
              FROM storage.drives d
              JOIN storage.folders f ON f.id = d.root_folder_id
             WHERE d.id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("get_by_id", e))?
        .ok_or_else(|| DriveRepositoryError::NotFound(id.to_string()))?;

        Self::row_to_drive_with_name(&row)
    }

    async fn get_by_ids(
        &self,
        ids: &[Uuid],
    ) -> Result<Vec<DriveWithRootName>, DriveRepositoryError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            SELECT d.id, d.kind, d.default_for_user, d.root_folder_id,
                   d.quota_bytes, d.used_bytes, d.policies,
                   d.created_at, d.updated_at,
                   f.name AS root_folder_name
              FROM storage.drives d
              JOIN storage.folders f ON f.id = d.root_folder_id
             WHERE d.id = ANY($1)
            "#,
        )
        .bind(ids)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("get_by_ids", e))?;

        rows.iter().map(Self::row_to_drive_with_name).collect()
    }

    async fn find_default_for_user(
        &self,
        user_id: Uuid,
    ) -> Result<DriveWithRootName, DriveRepositoryError> {
        if let Some(cached) = self.default_drive_cache.get(&user_id).await {
            return Ok(cached);
        }

        let row = sqlx::query(
            r#"
            SELECT d.id, d.kind, d.default_for_user, d.root_folder_id,
                   d.quota_bytes, d.used_bytes, d.policies,
                   d.created_at, d.updated_at,
                   f.name AS root_folder_name
              FROM storage.drives d
              JOIN storage.folders f ON f.id = d.root_folder_id
             WHERE d.default_for_user = $1
            "#,
        )
        .bind(user_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("find_default_for_user", e))?
        .ok_or_else(|| DriveRepositoryError::NotFound(user_id.to_string()))?;

        let dwr = Self::row_to_drive_with_name(&row)?;
        self.default_drive_cache.insert(user_id, dwr.clone()).await;
        Ok(dwr)
    }

    async fn list_readable_by(
        &self,
        caller_id: Uuid,
    ) -> Result<Arc<Vec<DriveWithRootName>>, DriveRepositoryError> {
        // Serve from the per-user cache; concurrent misses for the same
        // caller are coalesced into one join (`try_get_with`), and errors
        // are never cached. See the `readable_cache` field docs for the
        // freshness/invalidation contract. The Arc is handed to callers
        // directly — a warm hit is a refcount bump, not a deep clone of
        // every row's Strings.
        self.readable_cache
            .try_get_with(caller_id, async move {
                self.query_readable_by(caller_id).await.map(Arc::new)
            })
            .await
            .map_err(|e: Arc<DriveRepositoryError>| {
                Arc::try_unwrap(e)
                    .unwrap_or_else(|shared| DriveRepositoryError::StorageError(shared.to_string()))
            })
    }

    async fn list_all(&self) -> Result<Vec<DriveWithRootName>, DriveRepositoryError> {
        // No subject filter: every drive on the system. The HTTP layer
        // (admin guard on `/api/admin/drives`) is the access control —
        // adding a role filter here would defeat the point of the
        // endpoint (an admin without explicit membership wouldn't see
        // the drives they created for other users).
        let rows = sqlx::query(
            r#"
            SELECT d.id, d.kind, d.default_for_user, d.root_folder_id,
                   d.quota_bytes, d.used_bytes, d.policies,
                   d.created_at, d.updated_at,
                   f.name AS root_folder_name
              FROM storage.drives d
              JOIN storage.folders f ON f.id = d.root_folder_id
             ORDER BY LOWER(f.name) ASC
            "#,
        )
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("list_all", e))?;

        rows.iter().map(Self::row_to_drive_with_name).collect()
    }

    async fn get_policies_for_file(
        &self,
        file_id: Uuid,
    ) -> Result<crate::domain::entities::drive::DrivePolicies, DriveRepositoryError> {
        let row = sqlx::query(
            "SELECT d.policies \
               FROM storage.drives d \
               JOIN storage.files  f ON f.drive_id = d.id \
              WHERE f.id = $1",
        )
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("get_policies_for_file", e))?
        .ok_or_else(|| DriveRepositoryError::NotFound(file_id.to_string()))?;
        Ok(policies_from_row(&row))
    }

    async fn get_policies_for_folder(
        &self,
        folder_id: Uuid,
    ) -> Result<crate::domain::entities::drive::DrivePolicies, DriveRepositoryError> {
        let row = sqlx::query(
            "SELECT d.policies \
               FROM storage.drives  d \
               JOIN storage.folders fo ON fo.drive_id = d.id \
              WHERE fo.id = $1",
        )
        .bind(folder_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("get_policies_for_folder", e))?
        .ok_or_else(|| DriveRepositoryError::NotFound(folder_id.to_string()))?;
        Ok(policies_from_row(&row))
    }

    async fn get_drive_id_and_policies_for_file(
        &self,
        file_id: Uuid,
    ) -> Result<(Uuid, crate::domain::entities::drive::DrivePolicies), DriveRepositoryError> {
        let row = sqlx::query(
            "SELECT d.id, d.policies \
               FROM storage.drives d \
               JOIN storage.files  f ON f.drive_id = d.id \
              WHERE f.id = $1",
        )
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("get_drive_id_and_policies_for_file", e))?
        .ok_or_else(|| DriveRepositoryError::NotFound(file_id.to_string()))?;
        let drive_id: Uuid = row
            .try_get("id")
            .map_err(|e| Self::map_sqlx_err("get_drive_id_and_policies_for_file", e))?;
        Ok((drive_id, policies_from_row(&row)))
    }

    async fn get_drive_id_and_policies_for_folder(
        &self,
        folder_id: Uuid,
    ) -> Result<(Uuid, crate::domain::entities::drive::DrivePolicies), DriveRepositoryError> {
        let row = sqlx::query(
            "SELECT d.id, d.policies \
               FROM storage.drives  d \
               JOIN storage.folders fo ON fo.drive_id = d.id \
              WHERE fo.id = $1",
        )
        .bind(folder_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("get_drive_id_and_policies_for_folder", e))?
        .ok_or_else(|| DriveRepositoryError::NotFound(folder_id.to_string()))?;
        let drive_id: Uuid = row
            .try_get("id")
            .map_err(|e| Self::map_sqlx_err("get_drive_id_and_policies_for_folder", e))?;
        Ok((drive_id, policies_from_row(&row)))
    }

    async fn drive_id_for_folder(&self, folder_id: Uuid) -> Result<Uuid, DriveRepositoryError> {
        let row: Option<(Uuid,)> =
            sqlx::query_as("SELECT drive_id FROM storage.folders WHERE id = $1")
                .bind(folder_id)
                .fetch_optional(self.pool.as_ref())
                .await
                .map_err(|e| Self::map_sqlx_err("drive_id_for_folder", e))?;
        row.map(|(id,)| id)
            .ok_or_else(|| DriveRepositoryError::NotFound(folder_id.to_string()))
    }

    async fn update_policies(
        &self,
        drive_id: Uuid,
        partial: &serde_json::Value,
    ) -> Result<crate::domain::entities::drive::DrivePolicies, DriveRepositoryError> {
        // JSONB-level merge (`||`) keeps unknown keys already on disk —
        // the column remains the canonical bag (see
        // `DrivePolicies::from_value` — typed read is lenient, untyped
        // write is preserving). The caller passes a raw `Value` with
        // ONLY the keys it wants to change (never a full `DrivePolicies`
        // round-trip, which would serialise all-false defaults into the
        // merge and clobber other flags). RETURNING surfaces the
        // post-merge bag so the audit log shows what the row actually
        // carries afterwards.
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "UPDATE storage.drives \
                SET policies   = policies || $2, \
                    updated_at = now() \
              WHERE id = $1 \
              RETURNING policies",
        )
        .bind(drive_id)
        .bind(partial)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("update_policies", e))?;
        let raw = row
            .ok_or_else(|| DriveRepositoryError::NotFound(drive_id.to_string()))?
            .0;
        // Policy edits must not serve a stale `policies` bag from the
        // user-keyed caches (we only have the drive id) — clear both;
        // policy edits are admin-rare.
        self.default_drive_cache.invalidate_all();
        self.invalidate_readable_all();
        Ok(crate::domain::entities::drive::DrivePolicies::from_value(
            &raw,
        ))
    }

    async fn update_quota(
        &self,
        drive_id: Uuid,
        quota_bytes: Option<i64>,
    ) -> Result<Option<i64>, DriveRepositoryError> {
        // RETURNING gives the persisted value so the caller (service
        // layer) has authoritative data for the audit line + API
        // response without a second read.
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            "UPDATE storage.drives \
                SET quota_bytes = $2, \
                    updated_at  = now() \
              WHERE id = $1 \
              RETURNING quota_bytes",
        )
        .bind(drive_id)
        .bind(quota_bytes)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("update_quota", e))?;
        let persisted = row
            .ok_or_else(|| DriveRepositoryError::NotFound(drive_id.to_string()))?
            .0;
        // Same invalidation strategy as `update_policies` — both
        // user-keyed caches (`default_drive_cache`, the readable-drive
        // list) carry the whole DriveWithRootName / DriveDto rows and
        // would serve a stale quota otherwise. Admin-rare mutation,
        // so blowing the whole cache is fine (no per-user pinpointing
        // needed).
        self.default_drive_cache.invalidate_all();
        self.invalidate_readable_all();
        Ok(persisted)
    }
}
