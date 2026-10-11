//! Share mount lifecycle and resolution (docs/plan/share-mounts.md § Lifecycle, § Resolution).

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::user_lifecycle::{DeletionMode, LogoutReason, UserLifecycleHook};
use crate::application::services::subject_group_service::SubjectGroupService;
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::user::User;
use crate::domain::repositories::drive_repository::{DriveRepository, DriveRepositoryError};
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::domain::services::path_service::validate_storage_name;
use crate::infrastructure::repositories::pg::share_mount_pg_repository::{MountKind, MountRow};
use crate::infrastructure::repositories::pg::{
    DrivePgRepository, FolderDbRepository, ShareMountPgRepository,
};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ResolvedMount {
    pub mount_id: Uuid,
    pub target_id: Uuid,
    pub target_drive_id: Uuid,
    pub kind: MountKind,
    pub name: String,
}

impl From<MountRow> for ResolvedMount {
    fn from(m: MountRow) -> Self {
        Self {
            mount_id: m.mount_id,
            target_id: m.target_id,
            target_drive_id: m.target_drive_id,
            kind: m.kind,
            name: m.name,
        }
    }
}

pub struct ShareMountService {
    pub(crate) repo: Arc<ShareMountPgRepository>,
    drive_repo: Arc<DrivePgRepository>,
    authz: Arc<PgAclEngine>,
    folder_repo: Arc<FolderDbRepository>,
    groups: OnceLock<Arc<SubjectGroupService>>,
    /// Relative folder name under the personal root where new mounts land; `None` = root.
    share_folder: Option<String>,
}

impl ShareMountService {
    pub fn new(
        repo: Arc<ShareMountPgRepository>,
        drive_repo: Arc<DrivePgRepository>,
        authz: Arc<PgAclEngine>,
        folder_repo: Arc<FolderDbRepository>,
        share_folder: Option<String>,
    ) -> Self {
        let share_folder = share_folder
            .map(|s| s.trim_matches('/').to_owned())
            .filter(|s| !s.is_empty() && validate_storage_name(s).is_ok());
        Self {
            repo,
            drive_repo,
            authz,
            folder_repo,
            groups: OnceLock::new(),
            share_folder,
        }
    }

    /// Group expansion for group-subject grants. Set once by DI after the group service exists.
    pub fn set_group_service(&self, groups: Arc<SubjectGroupService>) {
        let _ = self.groups.set(groups);
    }

    /// `(personal_drive_id, root_folder_id)` or `None` for users without a personal drive.
    async fn personal_drive(&self, user_id: Uuid) -> Result<Option<(Uuid, Uuid)>, DomainError> {
        match self.drive_repo.find_default_for_user(user_id).await {
            Ok(d) => Ok(Some((d.drive.id, d.drive.root_folder_id))),
            Err(DriveRepositoryError::NotFound(_)) => Ok(None),
            Err(e) => Err(DomainError::internal_error(
                "ShareMount",
                format!("default drive: {e}"),
            )),
        }
    }

    /// Folder new mounts are created in: the share folder (created on demand) or the root.
    async fn target_parent(&self, user_id: Uuid, root: Uuid) -> Result<Uuid, DomainError> {
        let Some(name) = &self.share_folder else {
            return Ok(root);
        };
        let existing = self
            .folder_repo
            .list_folders(Some(&root.to_string()))
            .await?;
        if let Some(f) = existing.iter().find(|f| f.name() == name) {
            return Uuid::parse_str(f.id())
                .map_err(|_| DomainError::internal_error("ShareMount", "bad folder id"));
        }
        let created = self
            .folder_repo
            .create_folder(name.clone(), Some(root.to_string()), user_id)
            .await?;
        Uuid::parse_str(created.id())
            .map_err(|_| DomainError::internal_error("ShareMount", "bad folder id"))
    }

    /// Makes the user's mounts match their current grants. Idempotent.
    pub async fn reconcile(&self, user_id: Uuid) -> Result<(), DomainError> {
        let Some((drive_id, root)) = self.personal_drive(user_id).await? else {
            return Ok(());
        };
        let wanted: HashSet<Uuid> = self
            .repo
            .mount_worthy_targets(user_id, drive_id)
            .await?
            .into_iter()
            .collect();
        let existing = self.repo.existing_mounts(drive_id).await?;
        let have: HashSet<Uuid> = existing.iter().map(|m| m.target_id).collect();

        for m in existing.iter().filter(|m| !wanted.contains(&m.target_id)) {
            self.repo.delete_mount(m.mount_id).await?;
            tracing::info!(
                target: "audit",
                event = "share_mount.removed",
                reason = "no_longer_granted",
                recipient_id = %user_id,
                mount_id = %m.mount_id,
                target_id = %m.target_id,
                "🔌 share mount removed",
            );
        }
        let missing: Vec<Uuid> = wanted.difference(&have).copied().collect();
        if !missing.is_empty() {
            let parent = self.target_parent(user_id, root).await?;
            for target in missing {
                match self
                    .repo
                    .create_mount(drive_id, parent, target, user_id)
                    .await
                {
                    Ok(m) => tracing::info!(
                        target: "audit",
                        event = "share_mount.created",
                        reason = "reconcile",
                        recipient_id = %user_id,
                        mount_id = %m.mount_id,
                        target_id = %target,
                        kind = m.kind.as_str(),
                        "🔌 share mount created",
                    ),
                    Err(e) if e.kind == ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
        }
        self.repo.touch_reconciled(user_id).await
    }

    async fn users_of(&self, subject: Subject) -> Result<Vec<Uuid>, DomainError> {
        match subject {
            Subject::User(u) => Ok(vec![u]),
            Subject::Group(g) => match self.groups.get() {
                Some(gs) => gs.list_transitive_users(g).await,
                None => Ok(Vec::new()),
            },
            Subject::Token(_) => Ok(Vec::new()),
        }
    }

    async fn reconcile_subject(&self, subject: Subject) -> Result<(), DomainError> {
        for u in self.users_of(subject).await? {
            self.reconcile(u).await?;
        }
        Ok(())
    }

    pub async fn on_folder_granted(
        &self,
        subject: Subject,
        _folder_id: Uuid,
    ) -> Result<(), DomainError> {
        self.reconcile_subject(subject).await
    }

    pub async fn on_drive_member_set(
        &self,
        subject: Subject,
        _drive_id: Uuid,
    ) -> Result<(), DomainError> {
        self.reconcile_subject(subject).await
    }

    pub async fn on_subject_revoked(&self, subject: Subject) -> Result<(), DomainError> {
        self.reconcile_subject(subject).await
    }

    pub async fn is_mount(&self, folder_id: &str) -> Result<bool, DomainError> {
        let Ok(id) = Uuid::parse_str(folder_id) else {
            return Ok(false);
        };
        Ok(self.repo.mount_info(id).await?.is_some())
    }

    /// R0 + R1: `Ok(None)` for a plain folder; `Ok(Some)` when the caller is the recipient and
    /// may Read the target; `NotFound` otherwise.
    pub async fn resolve(
        &self,
        caller: Subject,
        folder_id: &str,
    ) -> Result<Option<ResolvedMount>, DomainError> {
        let Ok(id) = Uuid::parse_str(folder_id) else {
            return Ok(None);
        };
        let Some(m) = self.repo.mount_info(id).await? else {
            return Ok(None);
        };
        let denied = |reason: &'static str| {
            tracing::info!(
                target: "audit",
                event = "share_mount.denied",
                reason,
                caller = ?caller,
                mount_id = %m.mount_id,
                target_id = %m.target_id,
                "👮🏻‍♂️ share mount access denied",
            );
            DomainError::not_found("Folder", folder_id)
        };
        if caller.user_id() != Some(m.recipient_id) {
            return Err(denied("not_recipient"));
        }
        if !self
            .authz
            .check(caller, Permission::Read, Resource::Folder(m.target_id))
            .await?
        {
            return Err(denied("target_unreadable"));
        }
        Ok(Some(m.into()))
    }

    /// Removes a folder-grant mount from the caller's tree and records the decline.
    pub async fn unmount(&self, caller_id: Uuid, mount_id: Uuid) -> Result<(), DomainError> {
        let Some(m) = self.repo.mount_info(mount_id).await? else {
            return Err(DomainError::not_found("Folder", mount_id.to_string()));
        };
        if m.recipient_id != caller_id {
            tracing::info!(
                target: "audit",
                event = "share_mount.denied",
                reason = "not_recipient",
                caller_id = %caller_id,
                mount_id = %mount_id,
                "👮🏻‍♂️ unmount denied",
            );
            return Err(DomainError::not_found("Folder", mount_id.to_string()));
        }
        if m.kind == MountKind::SharedDrive {
            tracing::info!(
                target: "audit",
                event = "share_mount.rejected",
                reason = "drive_mount_not_declinable",
                caller_id = %caller_id,
                mount_id = %mount_id,
                "👮🏻‍♂️ unmount rejected",
            );
            return Err(DomainError::conflict(
                "ShareMount",
                "a shared drive mount cannot be removed",
            ));
        }
        self.repo.insert_decline(caller_id, m.target_id).await?;
        self.repo.delete_mount(mount_id).await?;
        tracing::info!(
            target: "audit",
            event = "share_mount.removed",
            reason = "declined",
            recipient_id = %caller_id,
            mount_id = %mount_id,
            target_id = %m.target_id,
            "🔌 share mount declined",
        );
        Ok(())
    }

    /// Clears a decline and recreates the mount of a target the caller is still granted.
    pub async fn remount(
        &self,
        caller_id: Uuid,
        target_id: Uuid,
    ) -> Result<ResolvedMount, DomainError> {
        let Some((drive_id, root)) = self.personal_drive(caller_id).await? else {
            return Err(DomainError::not_found("Folder", target_id.to_string()));
        };
        self.repo.delete_decline(caller_id, target_id).await?;
        let wanted = self.repo.mount_worthy_targets(caller_id, drive_id).await?;
        if !wanted.contains(&target_id) {
            tracing::info!(
                target: "audit",
                event = "share_mount.rejected",
                reason = "not_mount_worthy",
                caller_id = %caller_id,
                target_id = %target_id,
                "👮🏻‍♂️ remount rejected",
            );
            return Err(DomainError::not_found("Folder", target_id.to_string()));
        }
        if let Some(m) = self
            .repo
            .existing_mounts(drive_id)
            .await?
            .into_iter()
            .find(|m| m.target_id == target_id)
        {
            return Ok(m.into());
        }
        let parent = self.target_parent(caller_id, root).await?;
        let m = self
            .repo
            .create_mount(drive_id, parent, target_id, caller_id)
            .await?;
        tracing::info!(
            target: "audit",
            event = "share_mount.created",
            reason = "remount",
            recipient_id = %caller_id,
            mount_id = %m.mount_id,
            target_id = %target_id,
            "🔌 share mount created",
        );
        Ok(m.into())
    }

    /// R2: before `folder_id` is trashed, moves every mount under it to its own
    /// recipient's share folder / root. The caller may be a non-recipient Owner-grantee.
    pub async fn relocate_mounts_under(
        &self,
        caller_id: Uuid,
        folder_id: Uuid,
    ) -> Result<Vec<ResolvedMount>, DomainError> {
        let mounts = self.repo.mounts_in_subtree(folder_id).await?;
        let mut moved = Vec::new();
        for m in mounts.into_iter().filter(|m| m.mount_id != folder_id) {
            let Some((_drive_id, root)) = self.personal_drive(m.recipient_id).await? else {
                continue;
            };
            let parent = self.target_parent(m.recipient_id, root).await?;
            let mut name = m.name.clone();
            let mut attempt = 1u32;
            loop {
                if name != m.name {
                    self.folder_repo
                        .rename_folder(&m.mount_id.to_string(), name.clone(), m.recipient_id)
                        .await?;
                }
                match self
                    .folder_repo
                    .move_folder(
                        &m.mount_id.to_string(),
                        Some(&parent.to_string()),
                        m.recipient_id,
                    )
                    .await
                {
                    Ok(_) => break,
                    Err(e) if e.kind == ErrorKind::AlreadyExists && attempt < 100 => {
                        attempt += 1;
                        name = format!("{} ({attempt})", m.name);
                    }
                    Err(e) => return Err(e),
                }
            }
            tracing::info!(
                target: "audit",
                event = "share_mount.relocated",
                reason = "ancestor_trashed",
                caller_id = %caller_id,
                recipient_id = %m.recipient_id,
                mount_id = %m.mount_id,
                from = %folder_id,
                to = %parent,
                "🔌 share mount relocated",
            );
            moved.push(ResolvedMount {
                mount_id: m.mount_id,
                target_id: m.target_id,
                target_drive_id: m.target_drive_id,
                kind: m.kind,
                name,
            });
        }
        Ok(moved)
    }

    /// target_id → mount_id for the caller's mounts of `targets`.
    pub async fn mount_ids_for_targets(
        &self,
        caller_id: Uuid,
        targets: &[Uuid],
    ) -> Result<HashMap<Uuid, Uuid>, DomainError> {
        let Some((drive_id, _)) = self.personal_drive(caller_id).await? else {
            return Ok(HashMap::new());
        };
        self.repo.mounts_for_targets(drive_id, targets).await
    }

    pub async fn declined_for(
        &self,
        caller_id: Uuid,
        targets: &[Uuid],
    ) -> Result<HashSet<Uuid>, DomainError> {
        self.repo.declined_targets(caller_id, targets).await
    }
}

/// Reconciles the user's mounts on every login (safety net for missed events).
pub struct ShareMountLoginHook(pub Arc<ShareMountService>);

#[async_trait]
impl UserLifecycleHook for ShareMountLoginHook {
    fn name(&self) -> &'static str {
        "share_mounts"
    }

    async fn on_user_created(&self, _user: &User) -> Result<(), DomainError> {
        Ok(())
    }

    async fn on_user_login(&self, user: &User) -> Result<(), DomainError> {
        self.0.reconcile(user.id()).await
    }

    async fn on_user_logout(&self, _user: &User, _reason: LogoutReason) -> Result<(), DomainError> {
        Ok(())
    }

    async fn on_user_deleted(
        &self,
        _user: &User,
        _mode: DeletionMode,
        _tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

#[cfg(all(test, integration_tests))]
mod it {
    use super::*;
    use crate::application::ports::authorization_ports::AuthorizationEngine;
    use crate::common::errors::ErrorKind;
    use crate::domain::services::authorization::{Resource, Role, Subject};
    use crate::infrastructure::repositories::pg::share_mount_pg_repository::trigger_tests::{
        make_shared_drive, make_user_with_drive, personal_root,
    };
    use crate::infrastructure::repositories::pg::{
        DrivePgRepository, FileBlobReadRepository, FolderDbRepository, ShareMountPgRepository,
        SubjectGroupPgRepository,
    };
    use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
    use crate::mount_it_support::{fresh_db, provision_folder};
    use std::sync::atomic::AtomicBool;

    fn engine(pool: &Arc<sqlx::PgPool>) -> Arc<PgAclEngine> {
        Arc::new(PgAclEngine::new(
            pool.clone(),
            Arc::new(FolderDbRepository::new(pool.clone())),
            Arc::new(FileBlobReadRepository::new_stub()),
            Arc::new(SubjectGroupPgRepository::new(pool.clone())),
            Arc::new(AtomicBool::new(false)),
        ))
    }

    fn service_with(
        pool: &Arc<sqlx::PgPool>,
        authz: Arc<PgAclEngine>,
        share_folder: Option<&str>,
    ) -> ShareMountService {
        ShareMountService::new(
            Arc::new(ShareMountPgRepository::new(pool.clone())),
            Arc::new(DrivePgRepository::new(pool.clone())),
            authz,
            Arc::new(FolderDbRepository::new(pool.clone())),
            share_folder.map(str::to_owned),
        )
    }

    fn service(pool: &Arc<sqlx::PgPool>, share_folder: Option<&str>) -> ShareMountService {
        service_with(pool, engine(pool), share_folder)
    }

    async fn plain_folder(
        pool: &sqlx::PgPool,
        name: &str,
        parent: Uuid,
        drive: Uuid,
        by: Uuid,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ($1, $2, $3, $4, $4) RETURNING id",
        )
        .bind(name)
        .bind(parent)
        .bind(drive)
        .bind(by)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn reconcile_creates_and_removes_mounts() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let authz = engine(&pool);
        authz
            .set_role(
                alice.owner_id,
                Subject::User(bob),
                Role::Viewer,
                Resource::Folder(alice.mount_folder_id),
                None,
            )
            .await
            .unwrap();
        svc.reconcile(bob).await.unwrap();
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let mounts = svc.repo.existing_mounts(bob_drive).await.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target_id, alice.mount_folder_id);
        let parent: Option<Uuid> =
            sqlx::query_scalar("SELECT parent_id FROM storage.folders WHERE id = $1")
                .bind(mounts[0].mount_id)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert_eq!(parent, Some(bob_root));
        authz
            .clear_role(Subject::User(bob), Resource::Folder(alice.mount_folder_id))
            .await
            .unwrap();
        svc.reconcile(bob).await.unwrap();
        assert!(
            svc.repo
                .existing_mounts(bob_drive)
                .await
                .unwrap()
                .is_empty()
        );
        svc.reconcile(bob).await.unwrap();
        let ts: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT mounts_reconciled_at FROM auth.users WHERE id = $1")
                .bind(bob)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert!(ts.is_some());
    }

    #[tokio::test]
    async fn reconcile_uses_share_folder_and_creates_it_once() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, Some("Shared"));
        engine(&pool)
            .set_role(
                alice.owner_id,
                Subject::User(bob),
                Role::Viewer,
                Resource::Folder(alice.mount_folder_id),
                None,
            )
            .await
            .unwrap();
        svc.reconcile(bob).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let shared_dirs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.folders WHERE parent_id = $1 AND name = 'Shared' AND mount_target_id IS NULL",
        )
        .bind(bob_root)
        .fetch_one(&*pool)
        .await
        .unwrap();
        assert_eq!(shared_dirs, 1);
        let m = &svc.repo.existing_mounts(bob_drive).await.unwrap()[0];
        let parent_name: String = sqlx::query_scalar(
            "SELECT p.name FROM storage.folders m JOIN storage.folders p ON p.id = m.parent_id WHERE m.id = $1",
        )
        .bind(m.mount_id)
        .fetch_one(&*pool)
        .await
        .unwrap();
        assert_eq!(parent_name, "Shared");
    }

    #[tokio::test]
    async fn group_grant_mounts_for_members_but_not_granter() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let group: Uuid = sqlx::query_scalar(
            "INSERT INTO auth.subject_groups (name) VALUES ('team') RETURNING id",
        )
        .fetch_one(&*pool)
        .await
        .unwrap();
        for u in [alice.owner_id, bob] {
            sqlx::query(
                "INSERT INTO auth.subject_group_members (group_id, member_user_id, added_by) VALUES ($1, $2, $3)",
            )
            .bind(group)
            .bind(u)
            .bind(alice.owner_id)
            .execute(&*pool)
            .await
            .unwrap();
        }
        let svc = service(&pool, None);
        engine(&pool)
            .set_role(
                alice.owner_id,
                Subject::Group(group),
                Role::Viewer,
                Resource::Folder(alice.mount_folder_id),
                None,
            )
            .await
            .unwrap();
        svc.reconcile(bob).await.unwrap();
        svc.reconcile(alice.owner_id).await.unwrap();
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let (alice_drive, _) = personal_root(&pool, alice.owner_id).await;
        assert_eq!(svc.repo.existing_mounts(bob_drive).await.unwrap().len(), 1);
        assert!(
            svc.repo
                .existing_mounts(alice_drive)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn resolve_only_for_recipient_with_read_on_target() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let carol = make_user_with_drive(&pool, "carol").await;
        let authz = engine(&pool);
        let svc = service_with(&pool, authz.clone(), None);
        authz
            .set_role(
                alice.owner_id,
                Subject::User(bob),
                Role::Viewer,
                Resource::Folder(alice.mount_folder_id),
                None,
            )
            .await
            .unwrap();
        svc.reconcile(bob).await.unwrap();
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let m = svc.repo.existing_mounts(bob_drive).await.unwrap().remove(0);
        assert!(
            svc.resolve(Subject::User(bob), &bob_root.to_string())
                .await
                .unwrap()
                .is_none()
        );
        let r = svc
            .resolve(Subject::User(bob), &m.mount_id.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.target_id, alice.mount_folder_id);
        authz
            .set_role(
                bob,
                Subject::User(carol),
                Role::Viewer,
                Resource::Folder(bob_root),
                None,
            )
            .await
            .unwrap();
        let err = svc
            .resolve(Subject::User(carol), &m.mount_id.to_string())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
        authz
            .clear_role(Subject::User(bob), Resource::Folder(alice.mount_folder_id))
            .await
            .unwrap();
        authz.invalidate_cascade_grant_cache_all().await;
        let err = svc
            .resolve(Subject::User(bob), &m.mount_id.to_string())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn unmount_declines_and_remount_restores_drive_mount_not_declinable() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let authz = engine(&pool);
        authz
            .set_role(
                alice.owner_id,
                Subject::User(bob),
                Role::Viewer,
                Resource::Folder(alice.mount_folder_id),
                None,
            )
            .await
            .unwrap();
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        authz
            .set_role(
                alice.owner_id,
                Subject::User(bob),
                Role::Viewer,
                Resource::Drive(sd),
                None,
            )
            .await
            .unwrap();
        svc.reconcile(bob).await.unwrap();
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let mounts = svc.repo.existing_mounts(bob_drive).await.unwrap();
        let folder_mount = mounts
            .iter()
            .find(|m| m.target_id == alice.mount_folder_id)
            .unwrap()
            .clone();
        let drive_mount = mounts
            .iter()
            .find(|m| m.target_id == sd_root)
            .unwrap()
            .clone();
        svc.unmount(bob, folder_mount.mount_id).await.unwrap();
        svc.reconcile(bob).await.unwrap();
        assert_eq!(
            svc.repo.existing_mounts(bob_drive).await.unwrap().len(),
            1,
            "declined target not recreated"
        );
        let err = svc.unmount(bob, drive_mount.mount_id).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        let r = svc.remount(bob, alice.mount_folder_id).await.unwrap();
        assert_eq!(r.target_id, alice.mount_folder_id);
        assert_eq!(svc.repo.existing_mounts(bob_drive).await.unwrap().len(), 2);
        let carol = make_user_with_drive(&pool, "carol").await;
        let err = svc.unmount(carol, r.mount_id).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn drive_membership_hooks_create_and_remove_mount() {
        use crate::application::services::drive_management_service::DriveManagementService;
        use crate::infrastructure::repositories::pg::UserPgRepository;
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let authz = engine(&pool);
        let sm = Arc::new(service_with(&pool, authz.clone(), None));
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        let dms = DriveManagementService::new(
            Arc::new(DrivePgRepository::new(pool.clone())),
            authz.clone(),
            Arc::new(SubjectGroupPgRepository::new(pool.clone())),
            Arc::new(UserPgRepository::new(pool.clone())),
        )
        .with_share_mounts(sm.clone());
        dms.set_member_role(
            alice.owner_id,
            false,
            sd,
            Subject::User(bob),
            Role::Editor,
            None,
        )
        .await
        .unwrap();
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let mounts = sm.repo.existing_mounts(bob_drive).await.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target_id, sd_root);
        dms.remove_member(alice.owner_id, false, sd, Subject::User(bob))
            .await
            .unwrap();
        assert!(sm.repo.existing_mounts(bob_drive).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn group_membership_hook_reconciles_the_joining_user() {
        use crate::application::services::subject_group_service::SubjectGroupService;
        use crate::domain::entities::subject_group::GroupMember;
        use crate::infrastructure::repositories::pg::UserPgRepository;
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let authz = engine(&pool);
        let sm = Arc::new(service_with(&pool, authz.clone(), None));
        let group: Uuid = sqlx::query_scalar(
            "INSERT INTO auth.subject_groups (name) VALUES ('team') RETURNING id",
        )
        .fetch_one(&*pool)
        .await
        .unwrap();
        authz
            .set_role(
                alice.owner_id,
                Subject::Group(group),
                Role::Viewer,
                Resource::Folder(alice.mount_folder_id),
                None,
            )
            .await
            .unwrap();
        let sgs = SubjectGroupService::new(
            Arc::new(SubjectGroupPgRepository::new(pool.clone())),
            pool.clone(),
            Arc::new(UserPgRepository::new(pool.clone())),
            authz.clone(),
            Arc::new(DrivePgRepository::new(pool.clone())),
        )
        .with_share_mounts(sm.clone());
        // alice (the granter) is a member too, so bob is never the last user.
        sgs.add_member(group, GroupMember::User(alice.owner_id), alice.owner_id)
            .await
            .unwrap();
        sgs.add_member(group, GroupMember::User(bob), alice.owner_id)
            .await
            .unwrap();
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let (alice_drive, _) = personal_root(&pool, alice.owner_id).await;
        assert_eq!(sm.repo.existing_mounts(bob_drive).await.unwrap().len(), 1);
        assert!(
            sm.repo
                .existing_mounts(alice_drive)
                .await
                .unwrap()
                .is_empty()
        );
        sgs.remove_member(group, GroupMember::User(bob), alice.owner_id)
            .await
            .unwrap();
        assert!(sm.repo.existing_mounts(bob_drive).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn relocate_by_non_recipient_moves_mount_to_its_recipients_root() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let carol = make_user_with_drive(&pool, "carol").await;
        let svc = service(&pool, None);
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let projects = plain_folder(&pool, "Projects", bob_root, bob_drive, bob).await;
        let m = svc
            .repo
            .create_mount(bob_drive, projects, alice.mount_folder_id, bob)
            .await
            .unwrap();
        // carol (e.g. an Owner-grantee on Projects) trashes it
        let moved = svc.relocate_mounts_under(carol, projects).await.unwrap();
        assert_eq!(moved.len(), 1);
        let parent: Option<Uuid> =
            sqlx::query_scalar("SELECT parent_id FROM storage.folders WHERE id = $1")
                .bind(m.mount_id)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert_eq!(parent, Some(bob_root));
    }

    #[tokio::test]
    async fn i5_holds_for_trashed_mount_rows() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let m = svc
            .repo
            .create_mount(bob_drive, bob_root, alice.mount_folder_id, bob)
            .await
            .unwrap();
        sqlx::query("UPDATE storage.folders SET is_trashed = TRUE WHERE id = $1")
            .bind(m.mount_id)
            .execute(&*pool)
            .await
            .unwrap();
        let err = svc
            .repo
            .create_mount(bob_drive, bob_root, alice.mount_folder_id, bob)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::AlreadyExists);
    }

    #[tokio::test]
    async fn relocate_moves_mounts_to_root_with_suffix() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let projects = plain_folder(&pool, "Projects", bob_root, bob_drive, bob).await;
        plain_folder(&pool, "Docs", bob_root, bob_drive, bob).await;
        let m = svc
            .repo
            .create_mount(bob_drive, projects, alice.mount_folder_id, bob)
            .await
            .unwrap();
        assert_eq!(m.name, "Docs");
        let moved = svc.relocate_mounts_under(bob, projects).await.unwrap();
        assert_eq!(moved.len(), 1);
        let (parent, name): (Option<Uuid>, String) =
            sqlx::query_as("SELECT parent_id, name FROM storage.folders WHERE id = $1")
                .bind(m.mount_id)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert_eq!(parent, Some(bob_root));
        assert_eq!(name, "Docs (2)");
    }
}
