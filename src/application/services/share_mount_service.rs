//! Share mount lifecycle and resolution (docs/plan/share-mounts.md § Lifecycle, § Resolution).

use crate::application::dtos::folder_dto::FolderDto;
use crate::application::dtos::grant_dto::LinkStateDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::services::notification_application_service::NotificationApplicationService;
use crate::application::services::subject_group_service::SubjectGroupService;
use crate::common::config::LinksMode;
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::folder::Folder;
use crate::domain::entities::notification::{
    LinkTargetRenamedPayload, NewNotification, kind as notification_kind,
};
use crate::domain::repositories::drive_repository::{DriveRepository, DriveRepositoryError};
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::domain::services::path_service::{normalize_storage_name_owned, validate_storage_name};
use crate::infrastructure::repositories::pg::ShareMountPgRepository as Repo;
use crate::infrastructure::repositories::pg::share_mount_pg_repository::{
    LinkTargetMtime, MountKind, MountRow,
};
use crate::infrastructure::repositories::pg::{
    DrivePgRepository, FolderDbRepository, ShareMountPgRepository,
};
use crate::infrastructure::scheduler::{JobRegistry, JobRunArgs};
use crate::infrastructure::services::links_settle_job::LINKS_SETTLE_JOB_NAME;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use sqlx::PgConnection;
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

/// Folder etags derived across links (docs/plan/share-mounts.md § ETag propagation across mounts).
#[derive(Debug, Default)]
pub struct LinkEtags(HashMap<Uuid, LinkTargetMtime>);

impl LinkEtags {
    pub fn apply(&self, folder: &mut FolderDto) {
        let Some(l) = Uuid::parse_str(&folder.id)
            .ok()
            .and_then(|id| self.0.get(&id))
        else {
            return;
        };
        let mtime = l.mtime.max(0) as u64;
        folder.etag = match l.target_id {
            Some(target) => Folder::compute_etag(&target.to_string(), mtime),
            None => Folder::compute_etag(&folder.id, mtime),
        };
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SettleOutcome {
    Created(Uuid),
    Deleted,
    Unchanged,
    Skipped(&'static str),
    NameExhausted,
}

enum Settled {
    Created(MountRow),
    NameExhausted(Uuid),
    Deleted,
    Unchanged,
}

async fn commit(tx: sqlx::Transaction<'static, sqlx::Postgres>) -> Result<(), DomainError> {
    tx.commit()
        .await
        .map_err(|e| DomainError::internal_error("ShareMount", format!("commit: {e}")))
}

pub struct ShareMountService {
    pub(crate) repo: Arc<ShareMountPgRepository>,
    drive_repo: Arc<DrivePgRepository>,
    authz: Arc<PgAclEngine>,
    folder_repo: Arc<FolderDbRepository>,
    groups: OnceLock<Arc<SubjectGroupService>>,
    jobs: OnceLock<Arc<JobRegistry>>,
    notifications: OnceLock<Arc<NotificationApplicationService>>,
    /// Relative folder name under the personal root where new mounts land; `None` = root.
    share_folder: Option<String>,
    mode: LinksMode,
}

impl ShareMountService {
    pub fn new(
        repo: Arc<ShareMountPgRepository>,
        drive_repo: Arc<DrivePgRepository>,
        authz: Arc<PgAclEngine>,
        folder_repo: Arc<FolderDbRepository>,
        share_folder: Option<String>,
        mode: LinksMode,
    ) -> Self {
        let share_folder = share_folder
            .map(|s| normalize_storage_name_owned(s.trim_matches('/').to_owned()))
            .filter(|s| !s.is_empty() && validate_storage_name(s).is_ok());
        Self {
            repo,
            drive_repo,
            authz,
            folder_repo,
            groups: OnceLock::new(),
            jobs: OnceLock::new(),
            notifications: OnceLock::new(),
            share_folder,
            mode,
        }
    }

    pub fn mode(&self) -> LinksMode {
        self.mode
    }

    pub fn links_enabled(&self) -> bool {
        self.mode == LinksMode::Auto
    }

    /// Group expansion for group-subject grants. Set once by DI after the group service exists.
    pub fn set_group_service(&self, groups: Arc<SubjectGroupService>) {
        let _ = self.groups.set(groups);
    }

    pub fn set_notifications(&self, svc: Arc<NotificationApplicationService>) {
        let _ = self.notifications.set(svc);
    }

    /// Runs `notify_target_renamed` in the background; failures are logged, never returned.
    pub fn notify_target_renamed_detached(
        self: &Arc<Self>,
        target_id: Uuid,
        old_name: String,
        new_name: String,
        renamed_by: Uuid,
    ) {
        let svc = self.clone();
        tokio::spawn(async move {
            if let Err(e) = svc
                .notify_target_renamed(target_id, &old_name, &new_name, renamed_by)
                .await
            {
                tracing::warn!(%target_id, "link rename notification failed: {e}");
            }
        });
    }

    /// One notification per link whose name differs from `new_name`; declined recipients have no link row, so none.
    pub async fn notify_target_renamed(
        &self,
        target_id: Uuid,
        old_name: &str,
        new_name: &str,
        renamed_by: Uuid,
    ) -> Result<u32, DomainError> {
        let Some(notifications) = self.notifications.get() else {
            return Ok(0);
        };
        let links = self
            .repo
            .links_differing_from(target_id, new_name, renamed_by)
            .await?;
        let mut sent = 0;
        for (link_id, recipient, link_name) in links {
            let payload = LinkTargetRenamedPayload {
                link_id,
                target_id,
                old_name: old_name.to_owned(),
                new_name: new_name.to_owned(),
                link_name,
                renamed_by,
            };
            let payload = match serde_json::to_value(&payload) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(%recipient, %link_id, "link_target_renamed payload: {e}");
                    continue;
                }
            };
            let created = notifications
                .create(NewNotification {
                    user_id: recipient,
                    kind: notification_kind::LINK_TARGET_RENAMED.to_owned(),
                    payload,
                })
                .await;
            match created {
                Ok(_) => sent += 1,
                Err(e) => {
                    tracing::warn!(%recipient, %link_id, "link_target_renamed notification failed: {e}")
                }
            }
        }
        Ok(sent)
    }

    pub fn set_job_registry(&self, reg: Arc<JobRegistry>) {
        let _ = self.jobs.set(reg);
    }

    /// Starts a `links_settle` run in the background; never blocks or fails the caller.
    pub fn poke(&self) {
        if let Some(reg) = self.jobs.get() {
            let reg = reg.clone();
            tokio::spawn(async move {
                if !reg.is_running(LINKS_SETTLE_JOB_NAME).await {
                    reg.trigger(LINKS_SETTLE_JOB_NAME, &JobRunArgs::default())
                        .await;
                }
            });
        }
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
    async fn target_parent(
        &self,
        user_id: Uuid,
        drive_id: Uuid,
        root: Uuid,
    ) -> Result<Uuid, DomainError> {
        match &self.share_folder {
            None => Ok(root),
            Some(name) => self.repo.share_folder(drive_id, root, name, user_id).await,
        }
    }

    async fn target_parent_on(
        &self,
        conn: &mut PgConnection,
        user_id: Uuid,
        drive_id: Uuid,
        root: Uuid,
    ) -> Result<Uuid, DomainError> {
        match &self.share_folder {
            None => Ok(root),
            Some(name) => Repo::share_folder_on(conn, drive_id, root, name, user_id).await,
        }
    }

    pub async fn users_of(&self, subject: Subject) -> Result<Vec<Uuid>, DomainError> {
        match subject {
            Subject::User(u) => Ok(vec![u]),
            Subject::Group(g) => match self.groups.get() {
                Some(gs) => gs.list_transitive_users(g).await,
                None => Err(DomainError::internal_error(
                    "ShareMount",
                    "group service not wired",
                )),
            },
            Subject::Token(_) => Ok(Vec::new()),
        }
    }

    /// Makes the (recipient, target) link match current grants (docs/plan/links.md § Settling a pair).
    pub async fn settle(
        &self,
        recipient: Uuid,
        target: Uuid,
    ) -> Result<SettleOutcome, DomainError> {
        let Some((drive_id, root)) = self.personal_drive(recipient).await? else {
            return Ok(SettleOutcome::Skipped("no_personal_drive"));
        };
        let mut tx = self.repo.begin_pair(recipient, target).await?;
        let conn = &mut *tx;
        let own_drive = Repo::drive_of_folder_on(conn, target).await? == Some(drive_id);
        let link = Repo::mount_of_on(conn, drive_id, target).await?;
        if own_drive && link.is_none() {
            return Ok(SettleOutcome::Skipped("own_drive"));
        }
        let reads = !own_drive && Repo::reads_target_on(conn, recipient, target).await?;
        let declined = Repo::is_declined_on(conn, recipient, target).await?;
        let outcome = match (reads, &link, declined) {
            (true, None, false) if self.links_enabled() => {
                match self.target_parent_on(conn, recipient, drive_id, root).await {
                    Ok(parent) => {
                        match Repo::create_mount_on(conn, drive_id, parent, target, recipient).await
                        {
                            Ok(m) => Settled::Created(m),
                            Err(e) if e.kind == ErrorKind::AlreadyExists => Settled::Unchanged,
                            Err(e) if e.kind == ErrorKind::Conflict => {
                                Settled::NameExhausted(parent)
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Err(e) if e.kind == ErrorKind::Conflict => Settled::NameExhausted(root),
                    Err(e) => return Err(e),
                }
            }
            (false, link, declined) if link.is_some() || declined => {
                if let Some(m) = link {
                    Repo::delete_mount_on(conn, m.mount_id).await?;
                }
                Repo::delete_decline_on(conn, recipient, target).await?;
                Settled::Deleted
            }
            _ => Settled::Unchanged,
        };
        commit(tx).await?;
        Ok(match outcome {
            Settled::Created(m) => {
                tracing::info!(
                    target: "audit",
                    event = "share_mount.created",
                    reason = "granted",
                    recipient_id = %recipient,
                    mount_id = %m.mount_id,
                    target_id = %target,
                    kind = m.kind.as_str(),
                    "🔌 share mount created",
                );
                SettleOutcome::Created(m.mount_id)
            }
            Settled::NameExhausted(parent) => {
                tracing::info!(
                    target: "audit",
                    event = "link.create_failed",
                    reason = "name_exhausted",
                    recipient_id = %recipient,
                    target_id = %target,
                    parent_id = %parent,
                    "🔌 link not created: no free name",
                );
                SettleOutcome::NameExhausted
            }
            Settled::Deleted => {
                tracing::info!(
                    target: "audit",
                    event = "share_mount.removed",
                    reason = if own_drive { "own_drive" } else { "no_longer_granted" },
                    recipient_id = %recipient,
                    mount_id = ?link.map(|m| m.mount_id),
                    target_id = %target,
                    declined,
                    "🔌 share mount removed",
                );
                SettleOutcome::Deleted
            }
            Settled::Unchanged => SettleOutcome::Unchanged,
        })
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
        if !self.links_enabled() {
            return Err(denied("links_off"));
        }
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
        if m.target_trashed {
            return Err(denied("target_trashed"));
        }
        Ok(Some(m.into()))
    }

    /// Removes a link from the caller's tree and records the decline.
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
        let mut tx = self.repo.begin_pair(caller_id, m.target_id).await?;
        let conn = &mut *tx;
        if Repo::mount_info_on(conn, mount_id).await?.is_none() {
            tracing::info!(
                target: "audit",
                event = "share_mount.denied",
                reason = "link_gone",
                caller_id = %caller_id,
                mount_id = %mount_id,
                target_id = %m.target_id,
                "👮🏻‍♂️ unmount of a link removed meanwhile",
            );
            return Err(DomainError::not_found("Folder", mount_id.to_string()));
        }
        Repo::insert_decline_on(conn, caller_id, m.target_id).await?;
        Repo::delete_mount_on(conn, mount_id).await?;
        commit(tx).await?;
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

    /// Etags of `folder_ids` as seen through the caller's readable links; empty when links are off.
    pub async fn link_etags<'a>(
        &self,
        caller_id: Uuid,
        folder_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<LinkEtags, DomainError> {
        if !self.links_enabled() {
            return Ok(LinkEtags::default());
        }
        let ids: Vec<Uuid> = folder_ids
            .into_iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect();
        if ids.is_empty() {
            return Ok(LinkEtags::default());
        }
        Ok(LinkEtags(
            self.repo.max_link_target_mtime(caller_id, &ids).await?,
        ))
    }

    /// Link state per target for *Shared with me*; empty when links are off.
    pub async fn link_states(
        &self,
        caller_id: Uuid,
        target_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, LinkStateDto>, DomainError> {
        if !self.links_enabled() || target_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let Some((drive_id, _)) = self.personal_drive(caller_id).await? else {
            return Ok(HashMap::new());
        };
        self.repo
            .link_states_for(caller_id, drive_id, target_ids)
            .await
    }

    /// Clears a decline and recreates the link of a target the caller still reads.
    pub async fn remount(
        &self,
        caller_id: Uuid,
        target_id: Uuid,
    ) -> Result<ResolvedMount, DomainError> {
        let rejected = |reason: &'static str| {
            tracing::info!(
                target: "audit",
                event = "share_mount.rejected",
                reason,
                caller_id = %caller_id,
                target_id = %target_id,
                "👮🏻‍♂️ remount rejected",
            );
            DomainError::not_found("Folder", target_id.to_string())
        };
        if !self.links_enabled() {
            return Err(rejected("links_off"));
        }
        let Some((drive_id, root)) = self.personal_drive(caller_id).await? else {
            return Err(rejected("no_personal_drive"));
        };
        let mut tx = self.repo.begin_pair(caller_id, target_id).await?;
        let conn = &mut *tx;
        if Repo::drive_of_folder_on(conn, target_id).await? == Some(drive_id) {
            return Err(rejected("own_drive"));
        }
        if !Repo::reads_target_on(conn, caller_id, target_id).await? {
            return Err(rejected("not_mount_worthy"));
        }
        Repo::delete_decline_on(conn, caller_id, target_id).await?;
        if let Some(m) = Repo::mount_of_on(conn, drive_id, target_id).await? {
            commit(tx).await?;
            return Ok(m.into());
        }
        let parent = self
            .target_parent_on(conn, caller_id, drive_id, root)
            .await?;
        let m = Repo::create_mount_on(conn, drive_id, parent, target_id, caller_id).await?;
        commit(tx).await?;
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
            let Some((drive_id, root)) = self.personal_drive(m.recipient_id).await? else {
                continue;
            };
            let parent = match self.target_parent(m.recipient_id, drive_id, root).await? {
                p if p == folder_id => root,
                p => p,
            };
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

#[cfg(all(test, integration_tests))]
pub(crate) mod it {
    use super::*;
    use crate::application::ports::authorization_ports::AuthorizationEngine;
    use crate::common::errors::ErrorKind;
    use crate::domain::services::authorization::{Resource, Role, Subject};
    use crate::infrastructure::repositories::pg::share_mount_pg_repository::trigger_tests::{
        make_secondary_personal_drive, make_shared_drive, make_user_with_drive, personal_root,
    };
    use crate::infrastructure::repositories::pg::{
        DrivePgRepository, FileBlobReadRepository, FolderDbRepository, ShareMountPgRepository,
        SubjectGroupPgRepository,
    };
    use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
    use crate::mount_it_support::{fresh_db, make_user, provision_folder};
    use std::sync::atomic::AtomicBool;

    pub(crate) fn engine(pool: &Arc<sqlx::PgPool>) -> Arc<PgAclEngine> {
        Arc::new(PgAclEngine::new(
            pool.clone(),
            Arc::new(FolderDbRepository::new(pool.clone())),
            Arc::new(FileBlobReadRepository::new_stub()),
            Arc::new(SubjectGroupPgRepository::new(pool.clone())),
            Arc::new(AtomicBool::new(false)),
        ))
    }

    pub(crate) fn service_with_mode(
        pool: &Arc<sqlx::PgPool>,
        authz: Arc<PgAclEngine>,
        share_folder: Option<&str>,
        mode: LinksMode,
    ) -> ShareMountService {
        ShareMountService::new(
            Arc::new(ShareMountPgRepository::new(pool.clone())),
            Arc::new(DrivePgRepository::new(pool.clone())),
            authz,
            Arc::new(FolderDbRepository::new(pool.clone())),
            share_folder.map(str::to_owned),
            mode,
        )
    }

    fn service_with(
        pool: &Arc<sqlx::PgPool>,
        authz: Arc<PgAclEngine>,
        share_folder: Option<&str>,
    ) -> ShareMountService {
        service_with_mode(pool, authz, share_folder, LinksMode::Auto)
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
    async fn off_mode_hides_link_rows_from_resolve() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let auto = service(&pool, None);
        grant_viewer(&pool, alice.owner_id, bob, alice.mount_folder_id).await;
        auto.settle(bob, alice.mount_folder_id).await.unwrap();
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let link_id = auto.repo.existing_mounts(bob_drive).await.unwrap()[0].mount_id;
        assert!(
            auto.resolve(Subject::User(bob), &link_id.to_string())
                .await
                .unwrap()
                .is_some()
        );
        let off = service_with_mode(&pool, engine(&pool), None, LinksMode::Off);
        let r = off.resolve(Subject::User(bob), &link_id.to_string()).await;
        assert_eq!(r.unwrap_err().kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn settle_uses_share_folder_and_creates_it_once() {
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
        svc.settle(bob, alice.mount_folder_id).await.unwrap();
        svc.settle(bob, alice.mount_folder_id).await.unwrap();
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

    async fn grant_viewer(pool: &Arc<sqlx::PgPool>, owner: Uuid, to: Uuid, folder: Uuid) {
        engine(pool)
            .set_role(
                owner,
                Subject::User(to),
                Role::Viewer,
                Resource::Folder(folder),
                None,
            )
            .await
            .unwrap();
    }

    async fn mount_parent_name(pool: &sqlx::PgPool, target: Uuid) -> String {
        sqlx::query_scalar(
            "SELECT p.name FROM storage.folders m JOIN storage.folders p ON p.id = m.parent_id WHERE m.mount_target_id = $1",
        )
        .bind(target)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn share_folder_steps_over_a_mount_with_its_name() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Shares").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        grant_viewer(&pool, alice.owner_id, bob, alice.mount_folder_id).await;
        service(&pool, None)
            .settle(bob, alice.mount_folder_id)
            .await
            .unwrap();

        let (alice_drive, alice_root) = personal_root(&pool, alice.owner_id).await;
        let docs = plain_folder(&pool, "Docs", alice_root, alice_drive, alice.owner_id).await;
        grant_viewer(&pool, alice.owner_id, bob, docs).await;
        let svc = service(&pool, Some("Shares"));
        svc.settle(bob, docs).await.unwrap();
        svc.settle(bob, docs).await.unwrap();

        assert_eq!(mount_parent_name(&pool, docs).await, "Shares (2)");
        let (_, bob_root) = personal_root(&pool, bob).await;
        let first_parent: Option<Uuid> =
            sqlx::query_scalar("SELECT parent_id FROM storage.folders WHERE mount_target_id = $1")
                .bind(alice.mount_folder_id)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert_eq!(first_parent, Some(bob_root));
    }

    #[tokio::test]
    async fn share_folder_steps_over_a_file_with_its_name() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let hash = format!("{:064x}", Uuid::new_v4().as_u128());
        sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count, content_type) VALUES ($1, 1, 1, 'text/plain')")
            .bind(&hash)
            .execute(&*pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO storage.files (name, folder_id, drive_id, blob_hash, size, mime_type, is_trashed, created_by, updated_by) \
             VALUES ('Shares', $1, $2, $3, 1, 'text/plain', false, $4, $4)",
        )
        .bind(bob_root)
        .bind(bob_drive)
        .bind(&hash)
        .bind(bob)
        .execute(&*pool)
        .await
        .unwrap();
        grant_viewer(&pool, alice.owner_id, bob, alice.mount_folder_id).await;
        service(&pool, Some("Shares"))
            .settle(bob, alice.mount_folder_id)
            .await
            .unwrap();

        assert_eq!(
            mount_parent_name(&pool, alice.mount_folder_id).await,
            "Shares (2)"
        );
    }

    #[tokio::test]
    async fn settle_links_group_grant_for_member_not_granter() {
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
        let f = alice.mount_folder_id;
        assert!(matches!(
            svc.settle(bob, f).await.unwrap(),
            SettleOutcome::Created(_)
        ));
        assert_eq!(
            svc.settle(alice.owner_id, f).await.unwrap(),
            SettleOutcome::Skipped("own_drive")
        );
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
        svc.settle(bob, alice.mount_folder_id).await.unwrap();
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
    async fn rename_notifies_only_recipients_whose_link_name_differs() {
        use crate::application::ports::message_bus_ports::NoopReplicator;
        use crate::application::services::notification_application_service::NotificationApplicationService;
        use crate::domain::repositories::notification_repository::NotificationListFilter;
        use crate::infrastructure::repositories::pg::NotificationPgRepository;
        use crate::infrastructure::services::in_process_message_bus::InProcessMessageBus;
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let svc = service(&pool, None);
        let notifs = Arc::new(NotificationApplicationService::new(
            Arc::new(NotificationPgRepository::new(pool.clone())),
            InProcessMessageBus::with_replicator(Arc::new(NoopReplicator)),
        ));
        svc.set_notifications(notifs.clone());
        let bob = make_user_with_drive(&pool, "bob").await;
        let carol = make_user_with_drive(&pool, "carol").await;
        let dave = make_user_with_drive(&pool, "dave").await;
        for u in [bob, carol, dave] {
            grant_viewer(&pool, alice.owner_id, u, alice.mount_folder_id).await;
            svc.settle(u, alice.mount_folder_id).await.unwrap();
        }
        let (dave_drive, _) = personal_root(&pool, dave).await;
        let dave_link = svc.repo.existing_mounts(dave_drive).await.unwrap()[0].mount_id;
        svc.unmount(dave, dave_link).await.unwrap();
        sqlx::query(
            "UPDATE storage.folders m SET name = 'Docs 2026' FROM storage.drives d \
              WHERE d.id = m.drive_id AND d.default_for_user = $1 AND m.mount_target_id IS NOT NULL",
        )
        .bind(carol)
        .execute(&*pool)
        .await
        .unwrap();
        sqlx::query("UPDATE storage.folders SET name = 'Docs 2026' WHERE id = $1")
            .bind(alice.mount_folder_id)
            .execute(&*pool)
            .await
            .unwrap();
        let n = svc
            .notify_target_renamed(alice.mount_folder_id, "Docs", "Docs 2026", alice.owner_id)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let rows = notifs
            .list_for_user(bob, NotificationListFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "link_target_renamed");
        assert_eq!(rows[0].payload["link_name"], "Docs");
        assert_eq!(rows[0].payload["old_name"], "Docs");
        assert_eq!(rows[0].payload["new_name"], "Docs 2026");
        for u in [carol, dave, alice.owner_id] {
            let rows = notifs
                .list_for_user(u, NotificationListFilter::default())
                .await
                .unwrap();
            assert!(rows.is_empty());
        }
    }

    #[tokio::test]
    async fn unmount_declines_and_remount_restores_folder_mount() {
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
        svc.settle(bob, alice.mount_folder_id).await.unwrap();
        svc.settle(bob, sd_root).await.unwrap();
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
        assert_eq!(
            svc.settle(bob, alice.mount_folder_id).await.unwrap(),
            SettleOutcome::Unchanged
        );
        assert_eq!(
            svc.repo.existing_mounts(bob_drive).await.unwrap().len(),
            1,
            "declined target not recreated"
        );
        svc.unmount(bob, drive_mount.mount_id).await.unwrap();
        let r = svc.remount(bob, alice.mount_folder_id).await.unwrap();
        assert_eq!(r.target_id, alice.mount_folder_id);
        assert_eq!(svc.repo.existing_mounts(bob_drive).await.unwrap().len(), 1);
        let carol = make_user_with_drive(&pool, "carol").await;
        let err = svc.unmount(carol, r.mount_id).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
        let err = svc.remount(carol, alice.mount_folder_id).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound, "no grant path");
        let off = service_with_mode(&pool, engine(&pool), None, LinksMode::Off);
        svc.unmount(bob, r.mount_id).await.unwrap();
        let err = off.remount(bob, alice.mount_folder_id).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound, "links off");
        assert!(
            svc.repo
                .is_declined(bob, alice.mount_folder_id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn drive_link_is_declinable_and_restorable() {
        let (_c, pool) = fresh_db().await;
        let alice = make_user_with_drive(&pool, "alice").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice).await;
        engine(&pool)
            .set_role(
                alice,
                Subject::User(bob),
                Role::Viewer,
                Resource::Drive(sd),
                None,
            )
            .await
            .unwrap();
        svc.settle(bob, sd_root).await.unwrap();
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let l = svc
            .repo
            .mount_of(bob_drive, sd_root)
            .await
            .unwrap()
            .unwrap();
        svc.unmount(bob, l.mount_id).await.unwrap();
        assert!(
            svc.repo
                .mount_of(bob_drive, sd_root)
                .await
                .unwrap()
                .is_none()
        );
        let m = svc.remount(bob, sd_root).await.unwrap();
        assert_eq!(m.target_id, sd_root);
    }

    #[tokio::test]
    async fn link_states_report_link_and_decline_per_target() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        grant_viewer(&pool, alice.owner_id, bob, alice.mount_folder_id).await;
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        engine(&pool)
            .set_role(
                alice.owner_id,
                Subject::User(bob),
                Role::Viewer,
                Resource::Drive(sd),
                None,
            )
            .await
            .unwrap();
        let svc = service(&pool, None);
        svc.settle(bob, alice.mount_folder_id).await.unwrap();
        svc.settle(bob, sd_root).await.unwrap();
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let l = svc
            .repo
            .mount_of(bob_drive, sd_root)
            .await
            .unwrap()
            .unwrap();
        svc.unmount(bob, l.mount_id).await.unwrap();

        let ids = [alice.mount_folder_id, sd_root, Uuid::new_v4()];
        let st = svc.link_states(bob, &ids).await.unwrap();
        let f = &st[&alice.mount_folder_id];
        assert!(f.link_id.is_some() && f.link_name.is_some() && !f.declined);
        let d = &st[&sd_root];
        assert!(d.link_id.is_none() && d.declined);
        assert!(!st[&ids[2]].declined && st[&ids[2]].link_id.is_none());

        let off = service_with_mode(&pool, engine(&pool), None, LinksMode::Off);
        assert!(off.link_states(bob, &ids).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn trashing_the_share_folder_relocates_its_mounts_to_root() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        grant_viewer(&pool, alice.owner_id, bob, alice.mount_folder_id).await;
        let svc = service(&pool, Some("Shares"));
        svc.settle(bob, alice.mount_folder_id).await.unwrap();
        let (_, bob_root) = personal_root(&pool, bob).await;
        let shares: Uuid = sqlx::query_scalar(
            "SELECT id FROM storage.folders WHERE parent_id = $1 AND name = 'Shares'",
        )
        .bind(bob_root)
        .fetch_one(&*pool)
        .await
        .unwrap();

        svc.relocate_mounts_under(bob, shares).await.unwrap();

        let parent: Option<Uuid> =
            sqlx::query_scalar("SELECT parent_id FROM storage.folders WHERE mount_target_id = $1")
                .bind(alice.mount_folder_id)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert_eq!(parent, Some(bob_root));
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

    async fn set_role(pool: &Arc<sqlx::PgPool>, by: Uuid, to: Uuid, role: Role, r: Resource) {
        engine(pool)
            .set_role(by, Subject::User(to), role, r, None)
            .await
            .unwrap();
    }

    async fn trash(pool: &sqlx::PgPool, folder: Uuid) {
        sqlx::query("UPDATE storage.folders SET is_trashed = TRUE WHERE id = $1")
            .bind(folder)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn settle_creates_once_then_unchanged() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, f).await;
        let SettleOutcome::Created(link) = svc.settle(bob, f).await.unwrap() else {
            panic!("expected Created");
        };
        assert_eq!(svc.settle(bob, f).await.unwrap(), SettleOutcome::Unchanged);
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let m = svc.repo.mount_of(bob_drive, f).await.unwrap().unwrap();
        assert_eq!(m.mount_id, link);
        let parent: Option<Uuid> =
            sqlx::query_scalar("SELECT parent_id FROM storage.folders WHERE id = $1")
                .bind(link)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert_eq!(parent, Some(bob_root));
    }

    #[tokio::test]
    async fn settle_deletes_link_and_decline_when_no_grant_path() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let authz = engine(&pool);
        let svc = service_with(&pool, authz.clone(), None);
        let f = alice.mount_folder_id;
        let (a_drive, a_root) = personal_root(&pool, alice.owner_id).await;
        let g = plain_folder(&pool, "Only declined", a_root, a_drive, alice.owner_id).await;
        grant_viewer(&pool, alice.owner_id, bob, f).await;
        svc.settle(bob, f).await.unwrap();
        svc.repo.insert_decline(bob, f).await.unwrap();
        svc.repo.insert_decline(bob, g).await.unwrap();
        authz
            .clear_role(Subject::User(bob), Resource::Folder(f))
            .await
            .unwrap();
        assert_eq!(svc.settle(bob, f).await.unwrap(), SettleOutcome::Deleted);
        let (bob_drive, _) = personal_root(&pool, bob).await;
        assert!(svc.repo.mount_of(bob_drive, f).await.unwrap().is_none());
        assert!(!svc.repo.is_declined(bob, f).await.unwrap());
        assert_eq!(svc.settle(bob, g).await.unwrap(), SettleOutcome::Deleted);
        assert!(!svc.repo.is_declined(bob, g).await.unwrap());
        assert_eq!(svc.settle(bob, f).await.unwrap(), SettleOutcome::Unchanged);
        let target_left: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM storage.folders WHERE id = $1)")
                .bind(f)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert!(target_left, "the target folder is never deleted");
    }

    #[tokio::test]
    async fn settle_respects_decline_while_access_stands() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, f).await;
        svc.repo.insert_decline(bob, f).await.unwrap();
        assert_eq!(svc.settle(bob, f).await.unwrap(), SettleOutcome::Unchanged);
        let (bob_drive, _) = personal_root(&pool, bob).await;
        assert!(svc.repo.mount_of(bob_drive, f).await.unwrap().is_none());
        assert!(svc.repo.is_declined(bob, f).await.unwrap());
    }

    #[tokio::test]
    async fn settle_keeps_link_of_trashed_target() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, f).await;
        let SettleOutcome::Created(link) = svc.settle(bob, f).await.unwrap() else {
            panic!("expected Created");
        };
        trash(&pool, f).await;
        assert_eq!(svc.settle(bob, f).await.unwrap(), SettleOutcome::Unchanged);
        let (bob_drive, _) = personal_root(&pool, bob).await;
        assert_eq!(
            svc.repo
                .mount_of(bob_drive, f)
                .await
                .unwrap()
                .unwrap()
                .mount_id,
            link
        );
    }

    #[tokio::test]
    async fn resolve_hides_link_of_trashed_target_until_restored() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, f).await;
        let SettleOutcome::Created(link) = svc.settle(bob, f).await.unwrap() else {
            panic!("expected Created");
        };
        trash(&pool, f).await;
        let err = svc
            .resolve(Subject::User(bob), &link.to_string())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
        sqlx::query("UPDATE storage.folders SET is_trashed = FALSE WHERE id = $1")
            .bind(f)
            .execute(&*pool)
            .await
            .unwrap();
        let r = svc
            .resolve(Subject::User(bob), &link.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.target_id, f);
    }

    #[tokio::test]
    async fn settle_skips_own_drive_and_user_without_drive() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let nodrive = make_user(&pool, "nodrive").await;
        let svc = service(&pool, None);
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, nodrive, f).await;
        let (_, a_root) = personal_root(&pool, alice.owner_id).await;
        let bob = make_user_with_drive(&pool, "bob").await;
        set_role(
            &pool,
            alice.owner_id,
            bob,
            Role::Editor,
            Resource::Folder(f),
        )
        .await;
        let (b_drive, b_root) = personal_root(&pool, bob).await;
        let in_bob = plain_folder(&pool, "Mine", b_root, b_drive, bob).await;
        set_role(
            &pool,
            bob,
            alice.owner_id,
            Role::Viewer,
            Resource::Folder(in_bob),
        )
        .await;
        assert_eq!(
            svc.settle(alice.owner_id, f).await.unwrap(),
            SettleOutcome::Skipped("own_drive")
        );
        assert_eq!(
            svc.settle(alice.owner_id, a_root).await.unwrap(),
            SettleOutcome::Skipped("own_drive")
        );
        assert_eq!(
            svc.settle(nodrive, f).await.unwrap(),
            SettleOutcome::Skipped("no_personal_drive")
        );
        assert!(matches!(
            svc.settle(alice.owner_id, in_bob).await.unwrap(),
            SettleOutcome::Created(_)
        ));
    }

    #[tokio::test]
    async fn settle_links_nested_and_member_drive_folder_grants() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "F1").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let (d, r) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        let f = plain_folder(&pool, "Docs", r, d, alice.owner_id).await;
        let f1 = alice.mount_folder_id;
        let f2 = plain_folder(&pool, "F2", f1, alice.drive_id, alice.owner_id).await;
        set_role(&pool, alice.owner_id, bob, Role::Viewer, Resource::Drive(d)).await;
        set_role(
            &pool,
            alice.owner_id,
            bob,
            Role::Editor,
            Resource::Folder(f),
        )
        .await;
        grant_viewer(&pool, alice.owner_id, bob, f1).await;
        grant_viewer(&pool, alice.owner_id, bob, f2).await;
        for t in [r, f, f1, f2] {
            assert!(
                matches!(svc.settle(bob, t).await.unwrap(), SettleOutcome::Created(_)),
                "{t}"
            );
        }
        let (bob_drive, _) = personal_root(&pool, bob).await;
        assert_eq!(svc.repo.existing_mounts(bob_drive).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn settle_in_off_mode_deletes_but_never_creates() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let authz = engine(&pool);
        let auto = service_with(&pool, authz.clone(), None);
        let off = service_with_mode(&pool, authz.clone(), None, LinksMode::Off);
        let f = alice.mount_folder_id;
        let (bob_drive, _) = personal_root(&pool, bob).await;
        grant_viewer(&pool, alice.owner_id, bob, f).await;
        assert_eq!(off.settle(bob, f).await.unwrap(), SettleOutcome::Unchanged);
        assert!(off.repo.mount_of(bob_drive, f).await.unwrap().is_none());
        auto.settle(bob, f).await.unwrap();
        authz
            .clear_role(Subject::User(bob), Resource::Folder(f))
            .await
            .unwrap();
        assert_eq!(off.settle(bob, f).await.unwrap(), SettleOutcome::Deleted);
        assert!(off.repo.mount_of(bob_drive, f).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn settle_name_exhausted_is_an_outcome() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "X").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        plain_folder(&pool, "X", bob_root, bob_drive, bob).await;
        for n in 2..=100 {
            plain_folder(&pool, &format!("X ({n})"), bob_root, bob_drive, bob).await;
        }
        let x = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, x).await;
        assert_eq!(
            svc.settle(bob, x).await.unwrap(),
            SettleOutcome::NameExhausted
        );
        assert!(svc.repo.mount_of(bob_drive, x).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn settle_name_exhausted_share_folder_is_an_outcome() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let hash = format!("{:064x}", Uuid::new_v4().as_u128());
        sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count, content_type) VALUES ($1, 1, 100, 'text/plain')")
            .bind(&hash)
            .execute(&*pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO storage.files (name, folder_id, drive_id, blob_hash, size, mime_type, is_trashed, created_by, updated_by) \
             SELECT CASE WHEN n = 1 THEN 'Shares' ELSE 'Shares (' || n || ')' END, $1, $2, $3, 1, 'text/plain', false, $4, $4 \
               FROM generate_series(1, 100) n",
        )
        .bind(bob_root)
        .bind(bob_drive)
        .bind(&hash)
        .bind(bob)
        .execute(&*pool)
        .await
        .unwrap();
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, f).await;
        let svc = service(&pool, Some("Shares"));
        assert_eq!(
            svc.settle(bob, f).await.unwrap(),
            SettleOutcome::NameExhausted
        );
        assert!(svc.repo.mount_of(bob_drive, f).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn settle_deletes_link_whose_target_moved_into_the_recipients_drive() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = service(&pool, None);
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, f).await;
        assert!(matches!(
            svc.settle(bob, f).await.unwrap(),
            SettleOutcome::Created(_)
        ));
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let mine = plain_folder(&pool, "Mine", bob_root, bob_drive, bob).await;
        sqlx::query("UPDATE storage.folders SET drive_id = $1, parent_id = $2 WHERE id = $3")
            .bind(bob_drive)
            .bind(mine)
            .bind(f)
            .execute(&*pool)
            .await
            .unwrap();
        assert_eq!(svc.settle(bob, f).await.unwrap(), SettleOutcome::Deleted);
        assert!(svc.repo.mount_of(bob_drive, f).await.unwrap().is_none());
        assert_eq!(
            svc.settle(bob, f).await.unwrap(),
            SettleOutcome::Skipped("own_drive")
        );
    }

    #[tokio::test]
    async fn unmount_waits_for_the_pair_lock_and_skips_a_link_settle_removed() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = Arc::new(service(&pool, None));
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, f).await;
        let SettleOutcome::Created(link) = svc.settle(bob, f).await.unwrap() else {
            panic!("expected Created");
        };

        let mut held = hold_pair_lock(&pool, bob, f).await;
        let s = svc.clone();
        let mut pending = tokio::spawn(async move { s.unmount(bob, link).await });
        let early = tokio::time::timeout(std::time::Duration::from_millis(300), &mut pending).await;
        assert!(early.is_err(), "unmount must wait for the pair lock");
        ShareMountPgRepository::delete_mount_on(&mut held, link)
            .await
            .unwrap();
        held.commit().await.unwrap();

        let err = pending.await.unwrap().unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound);
        assert!(!svc.repo.is_declined(bob, f).await.unwrap());
    }

    #[tokio::test]
    async fn settle_does_not_link_own_secondary_personal_drive() {
        let (_c, pool) = fresh_db().await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (_, second_root) = make_secondary_personal_drive(&pool, bob).await;
        let svc = service(&pool, None);
        assert_eq!(
            svc.settle(bob, second_root).await.unwrap(),
            SettleOutcome::Unchanged
        );
        let (bob_drive, _) = personal_root(&pool, bob).await;
        assert!(
            svc.repo
                .existing_mounts(bob_drive)
                .await
                .unwrap()
                .is_empty()
        );
    }

    async fn hold_pair_lock(
        pool: &sqlx::PgPool,
        recipient: Uuid,
        target: Uuid,
    ) -> sqlx::Transaction<'static, sqlx::Postgres> {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || $2::text, 0))")
            .bind(recipient)
            .bind(target)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx
    }

    #[tokio::test]
    async fn settle_waits_for_the_pair_lock_and_sees_the_revoke() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let authz = engine(&pool);
        let svc = Arc::new(service_with(&pool, authz.clone(), None));
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, f).await;

        let held = hold_pair_lock(&pool, bob, f).await;
        let s = svc.clone();
        let mut pending = tokio::spawn(async move { s.settle(bob, f).await });
        let early = tokio::time::timeout(std::time::Duration::from_millis(300), &mut pending).await;
        assert!(early.is_err(), "settle must wait for the pair lock");
        authz
            .clear_role(Subject::User(bob), Resource::Folder(f))
            .await
            .unwrap();
        held.commit().await.unwrap();

        assert_eq!(pending.await.unwrap().unwrap(), SettleOutcome::Unchanged);
        let (bob_drive, _) = personal_root(&pool, bob).await;
        assert!(svc.repo.mount_of(bob_drive, f).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn remount_waits_for_the_pair_lock() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let svc = Arc::new(service(&pool, None));
        let f = alice.mount_folder_id;
        grant_viewer(&pool, alice.owner_id, bob, f).await;

        let held = hold_pair_lock(&pool, bob, f).await;
        let s = svc.clone();
        let mut pending = tokio::spawn(async move { s.remount(bob, f).await });
        let early = tokio::time::timeout(std::time::Duration::from_millis(300), &mut pending).await;
        assert!(early.is_err(), "remount must wait for the pair lock");
        held.commit().await.unwrap();
        assert_eq!(pending.await.unwrap().unwrap().target_id, f);
    }

    async fn dto(pool: &Arc<sqlx::PgPool>, id: Uuid) -> FolderDto {
        FolderDto::from(
            FolderDbRepository::new(pool.clone())
                .get_folder(&id.to_string())
                .await
                .unwrap(),
        )
    }

    async fn seen(svc: &ShareMountService, caller: Uuid, folder: FolderDto) -> String {
        let mut folder = folder;
        svc.link_etags(caller, [folder.id.as_str()])
            .await
            .unwrap()
            .apply(&mut folder);
        folder.etag
    }

    async fn seen_id(
        svc: &ShareMountService,
        pool: &Arc<sqlx::PgPool>,
        caller: Uuid,
        id: Uuid,
    ) -> String {
        seen(svc, caller, dto(pool, id).await).await
    }

    async fn bump_tree(pool: &sqlx::PgPool, id: Uuid) {
        sqlx::query(
            "UPDATE storage.folders SET tree_modified_at = tree_modified_at + interval '10 seconds' WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn linked_reports(
        pool: &Arc<sqlx::PgPool>,
        svc: &ShareMountService,
    ) -> (Uuid, Uuid, Uuid, Uuid, Uuid) {
        let alice = provision_folder(pool, "alice", "Reports").await;
        let bob = make_user_with_drive(pool, "bob").await;
        grant_viewer(pool, alice.owner_id, bob, alice.mount_folder_id).await;
        svc.settle(bob, alice.mount_folder_id).await.unwrap();
        let (bob_drive, bob_root) = personal_root(pool, bob).await;
        let link = svc.repo.existing_mounts(bob_drive).await.unwrap()[0].mount_id;
        let shares: Uuid =
            sqlx::query_scalar("SELECT parent_id FROM storage.folders WHERE id = $1")
                .bind(link)
                .fetch_one(&**pool)
                .await
                .unwrap();
        (alice.mount_folder_id, bob, bob_root, shares, link)
    }

    #[tokio::test]
    async fn etags_of_folders_holding_links_follow_their_targets() {
        let (_c, pool) = fresh_db().await;
        let svc = service(&pool, Some("Shares"));
        let (reports, bob, bob_root, shares, link) = linked_reports(&pool, &svc).await;
        let carol = make_user_with_drive(&pool, "carol").await;
        let (_, carol_root) = personal_root(&pool, carol).await;

        let root_before = seen_id(&svc, &pool, bob, bob_root).await;
        let shares_before = seen_id(&svc, &pool, bob, shares).await;
        let carol_before = seen_id(&svc, &pool, carol, carol_root).await;
        assert_eq!(
            seen_id(&svc, &pool, bob, link).await,
            dto(&pool, reports).await.etag
        );

        bump_tree(&pool, reports).await;

        assert_ne!(seen_id(&svc, &pool, bob, bob_root).await, root_before);
        assert_ne!(seen_id(&svc, &pool, bob, shares).await, shares_before);
        assert_eq!(
            seen_id(&svc, &pool, bob, link).await,
            dto(&pool, reports).await.etag
        );
        assert_eq!(seen_id(&svc, &pool, carol, carol_root).await, carol_before);
        assert_eq!(
            seen_id(&svc, &pool, carol, bob_root).await,
            dto(&pool, bob_root).await.etag,
            "another caller's view of bob's folder ignores bob's links"
        );
    }

    #[tokio::test]
    async fn hidden_links_do_not_contribute_to_etags() {
        let (_c, pool) = fresh_db().await;
        let svc = service(&pool, Some("Shares"));
        let (reports, bob, bob_root, _shares, link) = linked_reports(&pool, &svc).await;
        bump_tree(&pool, reports).await;
        assert_ne!(
            seen_id(&svc, &pool, bob, bob_root).await,
            dto(&pool, bob_root).await.etag
        );

        let off = service_with_mode(&pool, engine(&pool), None, LinksMode::Off);
        assert_eq!(
            seen_id(&off, &pool, bob, bob_root).await,
            dto(&pool, bob_root).await.etag
        );
        assert_eq!(
            seen_id(&off, &pool, bob, link).await,
            dto(&pool, link).await.etag
        );

        sqlx::query(
            "UPDATE storage.role_grants SET expires_at = NOW() - interval '1 second' WHERE resource_id = $1",
        )
        .bind(reports)
        .execute(&*pool)
        .await
        .unwrap();
        assert_eq!(
            seen_id(&svc, &pool, bob, bob_root).await,
            dto(&pool, bob_root).await.etag
        );
        assert_eq!(
            seen_id(&svc, &pool, bob, link).await,
            dto(&pool, link).await.etag
        );
    }
}
