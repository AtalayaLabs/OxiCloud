//! Subject group application service.
//!
//! Orchestrates CRUD and membership for ReBAC subject groups on top of the
//! `SubjectGroupRepository`. This is where:
//!   - Name validation runs (defence-in-depth alongside the DB CHECK).
//!   - Virtual groups (e.g. `Internal`) are protected from mutation.
//!   - Audit events are emitted via `tracing::info!(target = "audit", ...)`.
//!   - Cascading delete of `storage.role_grants` rows referencing this
//!     group runs in the same transaction as the group delete.
//!
//! See `migrations/20260612000000_subject_groups.sql` for the schema.

use std::collections::HashSet;
use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::subject_group::{
    GroupMember, INTERNAL_GROUP_ID, SubjectGroup, SubjectGroupError,
};
use crate::domain::repositories::subject_group_repository::{
    SubjectGroupRepository, SubjectGroupRepositoryError,
};
use crate::domain::repositories::user_repository::{UserRepository, UserRepositoryError};
use crate::infrastructure::repositories::pg::{SubjectGroupPgRepository, UserPgRepository};

pub struct SubjectGroupService {
    repo: Arc<SubjectGroupPgRepository>,
    pool: Arc<PgPool>,
    /// Looked up by `add_member` to refuse external-user candidates.
    /// External users are grant-only recipients; placing them in a
    /// subject group would let any later group-grant on internal
    /// resources silently leak access to them. `UserPgRepository` rather
    /// than `Arc<dyn UserStoragePort>` because the port's `async fn`s
    /// make it not dyn-compatible (matches the convention used by other
    /// services in this layer).
    user_storage: Arc<UserPgRepository>,
    /// Used by `add_member` / `remove_member` to drop stale
    /// `user_groups_cache` entries for affected users so newly-added
    /// (or newly-removed) group memberships surface in the next
    /// `expand_subject_for_listing` call instead of waiting out the
    /// 30 s TTL. Without this, fresh group-mediated drive grants
    /// don't appear in `/api/drives` for up to 30 s after `add_member`.
    engine: Arc<crate::infrastructure::services::pg_acl_engine::PgAclEngine>,
    /// Same freshness contract for the drive repository's per-user
    /// readable-drives cache: a membership change on a group that holds
    /// drive grants changes every affected user's visible drive list,
    /// so the cached lists drop alongside `user_groups_cache`.
    drive_repo: Arc<crate::infrastructure::repositories::pg::DrivePgRepository>,
    /// Share mounts follow group membership (docs/plan/share-mounts.md § Lifecycle).
    share_mounts: Option<Arc<crate::application::services::share_mount_service::ShareMountService>>,
}

impl SubjectGroupService {
    pub fn new(
        repo: Arc<SubjectGroupPgRepository>,
        pool: Arc<PgPool>,
        user_storage: Arc<UserPgRepository>,
        engine: Arc<crate::infrastructure::services::pg_acl_engine::PgAclEngine>,
        drive_repo: Arc<crate::infrastructure::repositories::pg::DrivePgRepository>,
    ) -> Self {
        Self {
            repo,
            pool,
            user_storage,
            engine,
            drive_repo,
            share_mounts: None,
        }
    }

    pub fn with_share_mounts(
        mut self,
        svc: Arc<crate::application::services::share_mount_service::ShareMountService>,
    ) -> Self {
        self.share_mounts = Some(svc);
        self
    }

    /// Returns the user IDs whose `user_groups_cache` entries need to
    /// drop after a membership change on `member`. For `User` members
    /// it's just that user; for nested `Group` members, every
    /// transitive user under that child group inherits/loses the
    /// parent-group ancestor, so all of them need invalidation.
    async fn invalidation_targets(&self, member: GroupMember) -> Result<Vec<Uuid>, DomainError> {
        match member {
            GroupMember::User(uid) => Ok(vec![uid]),
            GroupMember::Group(child_id) => self
                .repo
                .list_transitive_users(child_id)
                .await
                .map_err(map_repo_err),
        }
    }

    /// Create a new group. Validates the name (RFC 5321 local-part shape)
    /// at the domain layer before the round-trip; the DB CHECK constraint
    /// is the ultimate authority.
    pub async fn create(
        &self,
        name: &str,
        description: Option<String>,
        caller_id: Uuid,
    ) -> Result<SubjectGroup, DomainError> {
        let group = SubjectGroup::new(name, description).map_err(map_entity_err)?;
        let saved = self.repo.create(&group).await.map_err(map_repo_err)?;

        tracing::info!(
            target: "audit",
            event = "group.created",
            group_id = %saved.id,
            name = %saved.name,
            created_by = %caller_id,
        );

        Ok(saved)
    }

    pub async fn get_by_id(&self, id: Uuid) -> Result<SubjectGroup, DomainError> {
        match self.repo.get_by_id(id).await.map_err(map_repo_err)? {
            Some(g) => Ok(g),
            None => Err(DomainError::new(
                ErrorKind::NotFound,
                "SubjectGroup",
                format!("group {} not found", id),
            )),
        }
    }

    pub async fn list(
        &self,
        limit: u32,
        offset: u32,
        name_query: Option<&str>,
    ) -> Result<(Vec<SubjectGroup>, u64), DomainError> {
        self.repo
            .list(limit, offset, name_query)
            .await
            .map_err(map_repo_err)
    }

    /// Same as `list`, with the direct-member count attached to each row.
    /// Used by the management UI; the share-dialog search path stays on the
    /// lighter `search_for_share` which doesn't need counts.
    pub async fn list_with_counts(
        &self,
        limit: u32,
        offset: u32,
        name_query: Option<&str>,
    ) -> Result<(Vec<(SubjectGroup, i64)>, u64), DomainError> {
        self.repo
            .list_with_counts(limit, offset, name_query)
            .await
            .map_err(map_repo_err)
    }

    /// Direct-member count for a single group. Cheap (one `COUNT(*)`); used
    /// by create / get / update endpoints so the response DTO carries the
    /// same `member_count` field as the list view.
    pub async fn count_members(&self, id: Uuid) -> Result<i64, DomainError> {
        self.repo.count_members(id).await.map_err(map_repo_err)
    }

    /// Search by name prefix/substring. Virtual groups (Internal, plus any
    /// future predefined entries) are included so the share-dialog
    /// autocomplete picks them up automatically — no frontend change is
    /// needed when a new virtual group is added server-side. The repository
    /// returns virtual groups first so they're discoverable when the query
    /// is empty / short.
    pub async fn search_for_share(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<SubjectGroup>, DomainError> {
        let (rows, _total) = self
            .repo
            .list(limit, 0, Some(query))
            .await
            .map_err(map_repo_err)?;
        Ok(rows)
    }

    pub async fn rename(
        &self,
        id: Uuid,
        new_name: &str,
        caller_id: Uuid,
    ) -> Result<SubjectGroup, DomainError> {
        // Block mutation on virtual groups (the Internal sentinel).
        let existing = self.get_by_id(id).await?;
        if existing.is_virtual {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "SubjectGroup",
                "virtual groups cannot be modified".to_string(),
            ));
        }

        SubjectGroup::validate_name(new_name).map_err(map_entity_err)?;
        let renamed = self.repo.rename(id, new_name).await.map_err(map_repo_err)?;

        tracing::info!(
            target: "audit",
            event = "group.renamed",
            group_id = %renamed.id,
            old_name = %existing.name,
            new_name = %renamed.name,
            by = %caller_id,
        );

        Ok(renamed)
    }

    /// Delete the group; cascades to:
    ///   - `auth.subject_group_members` rows (FK CASCADE).
    ///   - `storage.role_grants` rows where `subject_type='group'` and
    ///     `subject_id = id` (handled here, no FK exists between
    ///     `role_grants` and `subject_groups`).
    pub async fn delete(&self, id: Uuid, caller_id: Uuid) -> Result<(), DomainError> {
        let existing = self.get_by_id(id).await?;
        if existing.is_virtual {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "SubjectGroup",
                "virtual groups cannot be modified".to_string(),
            ));
        }

        // Refuse if this group is the **sole Owner** of any drive — the
        // cascade-delete below would otherwise wipe the only `owner`
        // grant on that drive and leave it orphaned (no one can ever
        // manage it again). The check is "for every drive where this
        // group holds Owner, does another Owner exist?". A single drive
        // failing the check is enough to refuse.
        //
        // Matching D3a's last-owner-protection rule on `set_member_role`
        // / `remove_member` — they catch the case where the drive's
        // last Owner is *directly* a user or group being demoted /
        // removed via the membership API. This guard catches the same
        // invariant from the group-lifecycle side.
        let orphaning: Option<(Uuid,)> = sqlx::query_as(
            r#"
            WITH group_owned AS (
                SELECT resource_id
                  FROM storage.role_grants
                 WHERE subject_type = 'group'
                   AND subject_id   = $1
                   AND resource_type = 'drive'
                   AND role         = 'owner'
                   AND (expires_at IS NULL OR expires_at > NOW())
            )
            SELECT resource_id
              FROM storage.role_grants
             WHERE resource_type = 'drive'
               AND role         = 'owner'
               AND (expires_at IS NULL OR expires_at > NOW())
               AND resource_id IN (SELECT resource_id FROM group_owned)
             GROUP BY resource_id
            HAVING COUNT(*) = 1
             LIMIT 1
            "#,
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "SubjectGroup",
                format!("sole-owner check: {e}"),
            )
        })?;
        if let Some((drive_id,)) = orphaning {
            tracing::info!(
                target: "audit",
                event = "group_delete.rejected",
                reason = "sole_drive_owner",
                group_id = %id,
                drive_id = %drive_id,
                by = %caller_id,
                "👮🏻‍♂️ refused group delete — sole Owner of drive {drive_id}",
            );
            return Err(DomainError::new(
                ErrorKind::Conflict,
                "SubjectGroup",
                "Group is the sole Owner of at least one shared drive — \
                 promote another Owner first or delete the drive."
                    .to_string(),
            ));
        }

        // Atomically delete grants pointing at this group, then the group
        // itself. If either fails, both roll back.
        let mut tx = self.pool.begin().await.map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "SubjectGroup",
                format!("begin tx: {}", e),
            )
        })?;

        let grants_deleted = sqlx::query(
            "DELETE FROM storage.role_grants
              WHERE subject_type = 'group' AND subject_id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "SubjectGroup",
                format!("cascade-delete grants: {}", e),
            )
        })?
        .rows_affected();

        let removed = sqlx::query("DELETE FROM auth.subject_groups WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "SubjectGroup",
                    format!("delete group: {}", e),
                )
            })?
            .rows_affected();

        if removed == 0 {
            return Err(DomainError::new(
                ErrorKind::NotFound,
                "SubjectGroup",
                format!("group {} not found", id),
            ));
        }

        tx.commit().await.map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "SubjectGroup",
                format!("commit: {}", e),
            )
        })?;

        tracing::info!(
            target: "audit",
            event = "group.deleted",
            group_id = %id,
            name = %existing.name,
            grants_cascade_deleted = grants_deleted,
            by = %caller_id,
        );

        Ok(())
    }

    pub async fn add_member(
        &self,
        group_id: Uuid,
        member: GroupMember,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        if group_id == INTERNAL_GROUP_ID {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "SubjectGroup",
                "Internal group membership is implicit and cannot be edited".to_string(),
            ));
        }

        // Refuse external-user candidates. External users are grant-only
        // recipients; placing one in a subject group would let any later
        // group-grant on an internal resource silently leak access.
        // Mirrors the no-external-admins enforcement style in
        // `User::new(..., is_external = true)`.
        if let GroupMember::User(uid) = member {
            match UserRepository::get_user_by_id(&*self.user_storage, uid).await {
                Ok(user) if user.is_external() => {
                    tracing::info!(
                        target: "audit",
                        event = "group.external_member_rejected",
                        group_id = %group_id,
                        user_id = %uid,
                        by = %caller_id,
                    );
                    return Err(DomainError::new(
                        ErrorKind::AccessDenied,
                        "SubjectGroup",
                        "External users cannot be members of subject groups; share resources with them directly".to_string(),
                    ));
                }
                Ok(_) => { /* internal user — proceed */ }
                Err(UserRepositoryError::NotFound(_)) => {
                    return Err(DomainError::new(
                        ErrorKind::NotFound,
                        "SubjectGroup",
                        format!("user {} not found", uid),
                    ));
                }
                Err(e) => return Err(DomainError::from(e)),
            }
        }

        self.repo
            .add_member(group_id, member, caller_id)
            .await
            .map_err(|e| {
                // Emit a security-relevant audit event on cycle / depth
                // rejections so abusive admin behaviour is captured.
                match &e {
                    SubjectGroupRepositoryError::Cycle(msg) => {
                        tracing::info!(
                            target: "audit",
                            event = "group.cycle_rejected",
                            group_id = %group_id,
                            member = ?member,
                            detail = %msg,
                            by = %caller_id,
                        );
                    }
                    SubjectGroupRepositoryError::DepthExceeded(msg) => {
                        tracing::info!(
                            target: "audit",
                            event = "group.depth_exceeded",
                            group_id = %group_id,
                            member = ?member,
                            detail = %msg,
                            by = %caller_id,
                        );
                    }
                    _ => {}
                }
                map_repo_err(e)
            })?;

        // Drop stale cached subject expansions for every user who just
        // inherited the new group as a transitive ancestor — otherwise
        // group-mediated grants (e.g. shared-drive Owner via this
        // group) wouldn't surface in the next `expand_subject_for_listing`
        // call for up to 30 s.
        for uid in self.invalidation_targets(member).await? {
            self.engine.invalidate_user_groups_cache(uid).await;
            self.drive_repo.invalidate_readable_for_user(uid).await;
            if let Some(svc) = &self.share_mounts
                && let Err(e) = svc.reconcile(uid).await
            {
                tracing::warn!("share mount reconcile after group change failed: {e}");
            }
        }

        tracing::info!(
            target: "audit",
            event = "group.member_added",
            group_id = %group_id,
            member = ?member,
            by = %caller_id,
        );

        Ok(())
    }

    pub async fn remove_member(
        &self,
        group_id: Uuid,
        member: GroupMember,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        if group_id == INTERNAL_GROUP_ID {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "SubjectGroup",
                "Internal group membership is implicit and cannot be edited".to_string(),
            ));
        }

        // Self-defense (D3a follow-up): a group can never drop to 0
        // transitive users once seeded. Without this, an admin could
        // empty a group that's the Owner of a shared drive, leaving the
        // drive with no effective Owner (orphan-owned). Per Ed's
        // conservative-by-default stance: enforce the invariant
        // globally — not just for drive-owning groups — so anything
        // else that grants groups semantic power (delegated permissions,
        // mention targets, ...) is automatically protected.
        //
        // Pre-check is conservative: it computes the transitive user
        // set BEFORE the remove and refuses when the removal would
        // collapse it to 0. The edge case "user reachable via a nested
        // group" is handled by `list_transitive_users` itself — if the
        // user is still reachable via another path after this remove,
        // they stay in the set on the post-state, so the check would
        // pass on the next remove instead.
        // For a nested child-group removal the child's transitive user set is
        // needed twice: by the would-empty pre-check below AND, after the
        // remove, as the cache-invalidation set. The edge delete is ABOVE the
        // child, so it cannot change the child's descendants — compute the
        // recursive CTE ONCE here and reuse it, instead of the identical query
        // running twice (the second was hidden inside `invalidation_targets`).
        // (benches/ROUND23.md §G1)
        let child_users: Option<Vec<uuid::Uuid>> = match member {
            GroupMember::Group(child_id) => Some(
                self.repo
                    .list_transitive_users(child_id)
                    .await
                    .map_err(map_repo_err)?,
            ),
            GroupMember::User(_) => None,
        };

        let users_before = self
            .repo
            .list_transitive_users(group_id)
            .await
            .map_err(map_repo_err)?;
        if !users_before.is_empty() {
            let would_be_empty = match member {
                GroupMember::User(uid) => users_before.len() == 1 && users_before.contains(&uid),
                GroupMember::Group(_) => {
                    // Would removing this child drop the parent's transitive
                    // user set to 0? Reuse the child's transitive users
                    // computed above — if every user in the parent's set comes
                    // through the child, removing the child empties the parent.
                    let child_users = child_users.as_deref().unwrap_or(&[]);
                    // Set probe instead of an O(|before|·|child|) slice scan
                    // (benches/ROUND11.md §13: 5.7x at 500×500).
                    let child_set: std::collections::HashSet<&uuid::Uuid> =
                        child_users.iter().collect();
                    !child_users.is_empty() && users_before.iter().all(|u| child_set.contains(u))
                }
            };
            if would_be_empty {
                tracing::info!(
                    target: "audit",
                    event = "group.member_removed_rejected",
                    reason = "would_empty_seeded_group",
                    group_id = %group_id,
                    member = ?member,
                    by = %caller_id,
                    "👮🏻‍♂️ refused last-user removal from group {group_id} \
                     (groups must never drop to 0 transitive users once seeded — \
                     a drive-owning group going empty would orphan the drive)",
                );
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "SubjectGroup",
                    "A group cannot have less than 1 user once seeded. \
                     Add another user before removing this one."
                        .to_string(),
                ));
            }
        }

        self.repo
            .remove_member(group_id, member)
            .await
            .map_err(map_repo_err)?;

        // Symmetric to add_member: drop the cache for users whose
        // expanded subject set just lost the parent group as an
        // ancestor. Without this, a removed-from-group user keeps
        // appearing as a transitive member in `expand_subject_for_listing`
        // for up to 30 s, surfacing grants they no longer have.
        //
        // Reuse the child's transitive users computed above (unchanged by the
        // edge delete) as the invalidation set — no second recursive CTE. For a
        // `User` member it's just that user. (benches/ROUND23.md §G1)
        let invalidation: Vec<uuid::Uuid> = match member {
            GroupMember::User(uid) => vec![uid],
            GroupMember::Group(_) => child_users.unwrap_or_default(),
        };
        for uid in invalidation {
            self.engine.invalidate_user_groups_cache(uid).await;
            self.drive_repo.invalidate_readable_for_user(uid).await;
            if let Some(svc) = &self.share_mounts
                && let Err(e) = svc.reconcile(uid).await
            {
                tracing::warn!("share mount reconcile after group change failed: {e}");
            }
        }

        tracing::info!(
            target: "audit",
            event = "group.member_removed",
            group_id = %group_id,
            member = ?member,
            by = %caller_id,
        );

        Ok(())
    }

    pub async fn list_direct_members(
        &self,
        group_id: Uuid,
    ) -> Result<Vec<GroupMember>, DomainError> {
        self.repo
            .list_direct_members(group_id)
            .await
            .map_err(map_repo_err)
    }

    pub async fn list_transitive_users(&self, group_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        self.repo
            .list_transitive_users(group_id)
            .await
            .map_err(map_repo_err)
    }

    /// Hot path used by `PgAclEngine::expand_subject`. Returns the set of
    /// groups `user_id` belongs to transitively (excluding the implicit
    /// `INTERNAL_GROUP_ID` — the caller adds that).
    pub async fn groups_for_user(&self, user_id: Uuid) -> Result<HashSet<Uuid>, DomainError> {
        self.repo
            .groups_for_user(user_id)
            .await
            .map_err(map_repo_err)
    }
}

fn map_entity_err(e: SubjectGroupError) -> DomainError {
    let (kind, msg) = match e {
        SubjectGroupError::InvalidName(m) => (ErrorKind::InvalidInput, m),
        SubjectGroupError::CycleDetected(m) => (ErrorKind::InvalidInput, m),
        SubjectGroupError::DepthExceeded(m) => (ErrorKind::InvalidInput, m),
        SubjectGroupError::VirtualImmutable(m) => (ErrorKind::AccessDenied, m),
        SubjectGroupError::ValidationError(m) => (ErrorKind::InvalidInput, m),
    };
    DomainError::new(kind, "SubjectGroup", msg)
}

fn map_repo_err(e: SubjectGroupRepositoryError) -> DomainError {
    let (kind, msg) = match e {
        SubjectGroupRepositoryError::NotFound(m) => (ErrorKind::NotFound, m),
        SubjectGroupRepositoryError::NameAlreadyExists(m) => (ErrorKind::AlreadyExists, m),
        SubjectGroupRepositoryError::InvalidName(m) => (ErrorKind::InvalidInput, m),
        SubjectGroupRepositoryError::Cycle(m) => (ErrorKind::InvalidInput, m),
        SubjectGroupRepositoryError::DepthExceeded(m) => (ErrorKind::InvalidInput, m),
        SubjectGroupRepositoryError::VirtualImmutable(m) => (ErrorKind::AccessDenied, m),
        SubjectGroupRepositoryError::MemberAlreadyPresent => (
            ErrorKind::AlreadyExists,
            "member already in group".to_string(),
        ),
        SubjectGroupRepositoryError::MemberNotPresent => {
            (ErrorKind::NotFound, "member not in group".to_string())
        }
        SubjectGroupRepositoryError::StorageError(m) => (ErrorKind::InternalError, m),
    };
    DomainError::new(kind, "SubjectGroup", msg)
}

// ────────────────────────────────────────────────────────────────────────────
// Integration tests — service layer behaviours that need a live DB.
//
// How to run:
//   bash tests/common/spawn-db.sh
//   RUSTFLAGS='--cfg integration_tests' cargo test \
//       -p oxicloud --lib subject_group_service::integration_tests
// ────────────────────────────────────────────────────────────────────────────
#[cfg(integration_tests)]
#[allow(dead_code)]
mod integration_tests {
    use super::*;
    // INTERNAL_GROUP_ID is already in scope via `super::*` (re-exported
    // through the file's top-level `use crate::domain::entities::subject_group::…`).
    use sqlx::Row;
    use sqlx::postgres::PgPoolOptions;

    use crate::integration_test_support::{ensure_clean_test_db, test_db_url};

    async fn make_service() -> SubjectGroupService {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&test_db_url())
            .await
            .expect("connect to test DB — run tests/common/spawn-db.sh first");
        ensure_clean_test_db(&pool).await;
        let pool = Arc::new(pool);
        let repo = Arc::new(SubjectGroupPgRepository::new(pool.clone()));
        let user_storage = Arc::new(UserPgRepository::new(pool.clone()));
        // The engine is wired so `add_member` / `remove_member` can invalidate
        // their stale `user_groups_cache` entries (the production path).
        // A stub engine is enough — these tests never trigger an authz SQL
        // round-trip, only the in-memory cache invalidation calls. The stub's
        // lazy invalid pool would panic if reached, surfacing any drift if a
        // future test starts exercising real authz lookups.
        let engine =
            Arc::new(crate::infrastructure::services::pg_acl_engine::PgAclEngine::new_stub());
        let drive_repo =
            Arc::new(crate::infrastructure::repositories::pg::DrivePgRepository::new(pool.clone()));
        SubjectGroupService::new(repo, pool, user_storage, engine, drive_repo)
    }

    async fn first_admin(pool: &sqlx::PgPool) -> Uuid {
        let row = sqlx::query("SELECT id FROM auth.users LIMIT 1")
            .fetch_optional(pool)
            .await
            .expect("query")
            .expect("seed an admin user before running these tests");
        row.get::<Uuid, _>("id")
    }

    fn rand_name(test: &str) -> String {
        format!(
            "rust-test-svc-{}-{}",
            test,
            &Uuid::new_v4().to_string()[..8]
        )
    }

    // ── 9. Virtual group cannot be deleted ─────────────────────────────────
    #[tokio::test]
    async fn test_virtual_group_cannot_be_deleted() {
        let svc = make_service().await;
        let admin = first_admin(&svc.pool).await;

        let err = svc
            .delete(INTERNAL_GROUP_ID, admin)
            .await
            .expect_err("delete on Internal must be rejected");
        assert_eq!(err.kind, ErrorKind::AccessDenied);
    }

    #[tokio::test]
    async fn test_virtual_group_cannot_be_renamed() {
        let svc = make_service().await;
        let admin = first_admin(&svc.pool).await;

        let err = svc
            .rename(INTERNAL_GROUP_ID, "renamed", admin)
            .await
            .expect_err("rename on Internal must be rejected");
        assert_eq!(err.kind, ErrorKind::AccessDenied);
    }

    #[tokio::test]
    async fn test_virtual_group_cannot_add_member() {
        let svc = make_service().await;
        let admin = first_admin(&svc.pool).await;

        let err = svc
            .add_member(INTERNAL_GROUP_ID, GroupMember::User(admin), admin)
            .await
            .expect_err("add_member on Internal must be rejected");
        assert_eq!(err.kind, ErrorKind::AccessDenied);
    }

    // ── 13. Grants are revoked atomically when a group is deleted ──────────
    //
    // There is no FK between `storage.role_grants` and `auth.subject_groups`
    // (different schemas); the cascade is handled by the service's
    // transactional DELETE. This test pins that behaviour.
    #[tokio::test]
    async fn test_grants_revoked_when_group_deleted() {
        let svc = make_service().await;
        let admin = first_admin(&svc.pool).await;

        // Create a group and a fake grant referencing it.
        let group = svc
            .create(&rand_name("cleanup"), None, admin)
            .await
            .unwrap();
        let resource_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO storage.role_grants \
             (subject_type, subject_id, resource_type, resource_id, \
              role, granted_by) \
             VALUES ('group', $1, 'folder', $2, 'viewer', $3)",
        )
        .bind(group.id)
        .bind(resource_id)
        .bind(admin)
        .execute(svc.pool.as_ref())
        .await
        .expect("insert role_grants row");

        // Sanity: the grant exists.
        let pre: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.role_grants \
              WHERE subject_type = 'group' AND subject_id = $1",
        )
        .bind(group.id)
        .fetch_one(svc.pool.as_ref())
        .await
        .unwrap();
        assert_eq!(pre, 1);

        // Delete the group — the same transaction nukes the grant.
        svc.delete(group.id, admin).await.unwrap();

        let post: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.role_grants \
              WHERE subject_type = 'group' AND subject_id = $1",
        )
        .bind(group.id)
        .fetch_one(svc.pool.as_ref())
        .await
        .unwrap();
        assert_eq!(post, 0, "grants must be revoked atomically with the group");
    }

    // ── External users cannot be added as subject group members ─────────────
    //
    // Defense-in-depth gap #1 closed in PR 6: external users (grant-only
    // recipients) must never appear inside a subject group, because the
    // group could later be granted access to internal resources.
    #[tokio::test]
    async fn test_external_user_cannot_be_added_as_member() {
        let svc = make_service().await;
        let admin = first_admin(&svc.pool).await;

        // Insert an external user directly (no public test helper for this yet).
        let external_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO auth.users (
                 id, username, email, password_hash, role,
                 storage_quota_bytes, storage_used_bytes,
                 created_at, updated_at, active, is_external
             ) VALUES ($1, NULL, $2, NULL, 'user'::auth.userrole,
                       0, 0, NOW(), NOW(), TRUE, TRUE)",
        )
        .bind(external_id)
        .bind(format!("ext-{}@example.com", &external_id.to_string()[..8]))
        .execute(svc.pool.as_ref())
        .await
        .expect("seed external user");

        let group = svc
            .create(&rand_name("ext-reject"), None, admin)
            .await
            .unwrap();

        let err = svc
            .add_member(group.id, GroupMember::User(external_id), admin)
            .await
            .expect_err("external user must be rejected as a group member");
        assert_eq!(err.kind, ErrorKind::AccessDenied);
        assert!(
            err.message.contains("External users"),
            "error message should explain the rejection; got: {}",
            err.message
        );

        // Verify the membership did NOT land in the table.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth.subject_group_members
              WHERE group_id = $1 AND member_user_id = $2",
        )
        .bind(group.id)
        .bind(external_id)
        .fetch_one(svc.pool.as_ref())
        .await
        .unwrap();
        assert_eq!(count, 0, "external user must not appear in members table");
    }

    // Bonus: service-layer name validation runs before the DB round-trip.
    #[tokio::test]
    async fn test_service_rejects_invalid_name_locally() {
        let svc = make_service().await;
        let admin = first_admin(&svc.pool).await;

        let err = svc
            .create("name with space", None, admin)
            .await
            .expect_err("space must be rejected");
        assert_eq!(err.kind, ErrorKind::InvalidInput);
    }
}
