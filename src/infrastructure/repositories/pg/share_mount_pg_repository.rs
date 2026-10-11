//! PostgreSQL access for share mounts (docs/plan/share-mounts.md).

use crate::common::errors::DomainError;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MountKind {
    SharedFolder,
    SharedDrive,
}

impl MountKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MountKind::SharedFolder => "shared_folder",
            MountKind::SharedDrive => "shared_drive",
        }
    }
}

#[derive(Clone, Debug)]
pub struct MountRow {
    pub mount_id: Uuid,
    pub mount_drive_id: Uuid,
    pub recipient_id: Uuid,
    pub target_id: Uuid,
    pub target_drive_id: Uuid,
    pub kind: MountKind,
    pub name: String,
}

type MountTuple = (Uuid, Uuid, Uuid, Uuid, Uuid, bool, String);

fn tuple_to_row(t: MountTuple) -> MountRow {
    MountRow {
        mount_id: t.0,
        mount_drive_id: t.1,
        recipient_id: t.2,
        target_id: t.3,
        target_drive_id: t.4,
        kind: if t.5 {
            MountKind::SharedDrive
        } else {
            MountKind::SharedFolder
        },
        name: t.6,
    }
}

/// Columns of one mount row; `m` = mount row, `t` = target, `d` = mount's drive.
const MOUNT_SELECT: &str = "SELECT m.id, m.drive_id, d.default_for_user, t.id, t.drive_id, \
            (t.parent_id IS NULL) AS is_drive_root, m.name \
       FROM storage.folders m \
       JOIN storage.drives  d ON d.id = m.drive_id \
       JOIN storage.folders t ON t.id = m.mount_target_id ";

/// Caller `$1` may Read folder `t` through a folder grant on an ancestor or drive membership.
const CALLER_READS_T: &str = "( \
        EXISTS (SELECT 1 FROM storage.role_grants g \
                  JOIN storage.folders a ON a.id = g.resource_id \
                 WHERE g.resource_type = 'folder' AND a.lpath @> t.lpath \
                   AND (g.expires_at IS NULL OR g.expires_at > NOW()) \
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1) \
                     OR (g.subject_type = 'group' AND g.subject_id IN (SELECT storage.caller_group_ids($1))))) \
     OR EXISTS (SELECT 1 FROM storage.role_grants g \
                 WHERE g.resource_type = 'drive' AND g.resource_id = t.drive_id \
                   AND (g.expires_at IS NULL OR g.expires_at > NOW()) \
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1) \
                     OR (g.subject_type = 'group' AND g.subject_id IN (SELECT storage.caller_group_ids($1))))) \
    )";

pub struct ShareMountPgRepository {
    pool: Arc<PgPool>,
}

impl ShareMountPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    fn err(op: &str, e: sqlx::Error) -> DomainError {
        DomainError::internal_error("ShareMountDb", format!("{op}: {e}"))
    }

    pub async fn mount_info(&self, folder_id: Uuid) -> Result<Option<MountRow>, DomainError> {
        let sql = format!("{MOUNT_SELECT} WHERE m.id = $1 AND NOT m.is_trashed");
        sqlx::query_as::<_, MountTuple>(&sql)
            .bind(folder_id)
            .fetch_optional(&*self.pool)
            .await
            .map(|o| o.map(tuple_to_row))
            .map_err(|e| Self::err("mount_info", e))
    }

    pub async fn existing_mounts(
        &self,
        personal_drive_id: Uuid,
    ) -> Result<Vec<MountRow>, DomainError> {
        let sql = format!("{MOUNT_SELECT} WHERE m.drive_id = $1 AND NOT m.is_trashed");
        sqlx::query_as::<_, MountTuple>(&sql)
            .bind(personal_drive_id)
            .fetch_all(&*self.pool)
            .await
            .map(|v| v.into_iter().map(tuple_to_row).collect())
            .map_err(|e| Self::err("existing_mounts", e))
    }

    pub async fn mounts_in_subtree(&self, folder_id: Uuid) -> Result<Vec<MountRow>, DomainError> {
        let sql = format!(
            "{MOUNT_SELECT} WHERE NOT m.is_trashed \
               AND m.lpath <@ (SELECT lpath FROM storage.folders WHERE id = $1)"
        );
        sqlx::query_as::<_, MountTuple>(&sql)
            .bind(folder_id)
            .fetch_all(&*self.pool)
            .await
            .map(|v| v.into_iter().map(tuple_to_row).collect())
            .map_err(|e| Self::err("mounts_in_subtree", e))
    }

    /// target_id → mount_id for the caller's mounts of `target_ids`.
    pub async fn mounts_for_targets(
        &self,
        personal_drive_id: Uuid,
        target_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Uuid>, DomainError> {
        if target_ids.is_empty() {
            return Ok(HashMap::new());
        }
        sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT mount_target_id, id FROM storage.folders \
              WHERE drive_id = $1 AND mount_target_id = ANY($2) AND NOT is_trashed",
        )
        .bind(personal_drive_id)
        .bind(target_ids)
        .fetch_all(&*self.pool)
        .await
        .map(|v| v.into_iter().collect())
        .map_err(|e| Self::err("mounts_for_targets", e))
    }

    /// Targets the user should have a mount for: shared drives they hold a role on,
    /// plus top-level incoming folder grants outside their own drive and outside any
    /// drive they are a member of, minus declined targets.
    pub async fn mount_worthy_targets(
        &self,
        user_id: Uuid,
        personal_drive_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            WITH groups AS (
                SELECT ARRAY(SELECT storage.caller_group_ids($1)) AS ids
            ),
            my_drives AS (
                SELECT DISTINCT g.resource_id AS drive_id
                  FROM storage.role_grants g, groups
                 WHERE g.resource_type = 'drive'
                   AND (g.expires_at IS NULL OR g.expires_at > NOW())
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1)
                     OR (g.subject_type = 'group' AND g.subject_id = ANY(groups.ids)))
            ),
            drive_targets AS (
                SELECT d.root_folder_id AS target_id
                  FROM storage.drives d
                  JOIN my_drives md ON md.drive_id = d.id
                 WHERE d.kind = 'shared' AND d.root_folder_id IS NOT NULL
            ),
            folder_grants AS (
                SELECT DISTINCT f.id AS target_id, f.lpath, f.drive_id
                  FROM storage.role_grants g
                  CROSS JOIN groups
                  JOIN storage.folders f ON f.id = g.resource_id
                 WHERE g.resource_type = 'folder'
                   AND (g.expires_at IS NULL OR g.expires_at > NOW())
                   AND g.granted_by <> $1
                   AND NOT f.is_trashed
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1)
                     OR (g.subject_type = 'group' AND g.subject_id = ANY(groups.ids)))
            ),
            folder_targets AS (
                SELECT fg.target_id
                  FROM folder_grants fg
                 WHERE fg.drive_id <> $2
                   AND fg.drive_id NOT IN (SELECT drive_id FROM my_drives)
                   AND NOT EXISTS (
                       SELECT 1 FROM folder_grants anc
                        WHERE anc.target_id <> fg.target_id AND anc.lpath @> fg.lpath
                   )
            )
            SELECT target_id FROM drive_targets
            UNION
            SELECT target_id FROM folder_targets
            EXCEPT
            SELECT target_folder_id FROM storage.share_mount_declines WHERE recipient_id = $1
            ORDER BY 1
            "#,
        )
        .bind(user_id)
        .bind(personal_drive_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::err("mount_worthy_targets", e))
    }

    /// Creates the mount row named after the target, retrying with " (2)", " (3)", … on a
    /// sibling-name collision. `already_exists` when a mount of this target exists.
    pub async fn create_mount(
        &self,
        personal_drive_id: Uuid,
        parent_id: Uuid,
        target_id: Uuid,
        user_id: Uuid,
    ) -> Result<MountRow, DomainError> {
        let base: String = sqlx::query_scalar("SELECT name FROM storage.folders WHERE id = $1")
            .bind(target_id)
            .fetch_optional(&*self.pool)
            .await
            .map_err(|e| Self::err("create_mount/name", e))?
            .ok_or_else(|| DomainError::not_found("Folder", target_id.to_string()))?;

        for attempt in 1..=100u32 {
            let name = if attempt == 1 {
                base.clone()
            } else {
                format!("{base} ({attempt})")
            };
            let res = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by, mount_target_id) \
                 VALUES ($1, $2, $3, $4, $4, $5) RETURNING id",
            )
            .bind(&name)
            .bind(parent_id)
            .bind(personal_drive_id)
            .bind(user_id)
            .bind(target_id)
            .fetch_one(&*self.pool)
            .await;
            match res {
                Ok(id) => {
                    return self.mount_info(id).await?.ok_or_else(|| {
                        DomainError::internal_error("ShareMountDb", "mount vanished after insert")
                    });
                }
                Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505") => {
                    if db.constraint() == Some("idx_folders_one_mount_per_target") {
                        return Err(DomainError::already_exists(
                            "ShareMount",
                            target_id.to_string(),
                        ));
                    }
                    continue;
                }
                Err(e) => return Err(Self::err("create_mount", e)),
            }
        }
        Err(DomainError::conflict(
            "ShareMount",
            "no free name after 100 attempts",
        ))
    }

    pub async fn delete_mount(&self, mount_id: Uuid) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM storage.folders WHERE id = $1 AND mount_target_id IS NOT NULL")
            .bind(mount_id)
            .execute(&*self.pool)
            .await
            .map(|_| ())
            .map_err(|e| Self::err("delete_mount", e))
    }

    pub async fn insert_decline(
        &self,
        recipient_id: Uuid,
        target_id: Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "INSERT INTO storage.share_mount_declines (recipient_id, target_folder_id) \
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(recipient_id)
        .bind(target_id)
        .execute(&*self.pool)
        .await
        .map(|_| ())
        .map_err(|e| Self::err("insert_decline", e))
    }

    pub async fn delete_decline(
        &self,
        recipient_id: Uuid,
        target_id: Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "DELETE FROM storage.share_mount_declines WHERE recipient_id = $1 AND target_folder_id = $2",
        )
        .bind(recipient_id)
        .bind(target_id)
        .execute(&*self.pool)
        .await
        .map(|_| ())
        .map_err(|e| Self::err("delete_decline", e))
    }

    pub async fn declined_targets(
        &self,
        recipient_id: Uuid,
        target_ids: &[Uuid],
    ) -> Result<HashSet<Uuid>, DomainError> {
        if target_ids.is_empty() {
            return Ok(HashSet::new());
        }
        sqlx::query_scalar::<_, Uuid>(
            "SELECT target_folder_id FROM storage.share_mount_declines \
              WHERE recipient_id = $1 AND target_folder_id = ANY($2)",
        )
        .bind(recipient_id)
        .bind(target_ids)
        .fetch_all(&*self.pool)
        .await
        .map(|v| v.into_iter().collect())
        .map_err(|e| Self::err("declined_targets", e))
    }

    pub async fn caller_can_read_target(
        &self,
        user_id: Uuid,
        target_id: Uuid,
    ) -> Result<bool, DomainError> {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM storage.folders t WHERE t.id = $2 AND NOT t.is_trashed AND {CALLER_READS_T})"
        );
        sqlx::query_scalar::<_, bool>(&sql)
            .bind(user_id)
            .bind(target_id)
            .fetch_one(&*self.pool)
            .await
            .map_err(|e| Self::err("caller_can_read_target", e))
    }

    pub async fn touch_reconciled(&self, user_id: Uuid) -> Result<(), DomainError> {
        sqlx::query("UPDATE auth.users SET mounts_reconciled_at = NOW() WHERE id = $1")
            .bind(user_id)
            .execute(&*self.pool)
            .await
            .map(|_| ())
            .map_err(|e| Self::err("touch_reconciled", e))
    }
}

#[cfg(all(test, integration_tests))]
pub(crate) mod trigger_tests {
    use crate::domain::repositories::drive_repository::DriveRepository;
    use crate::infrastructure::repositories::pg::DrivePgRepository;
    use crate::mount_it_support::{fresh_db, make_user, provision_folder};
    use sqlx::PgPool;
    use std::sync::Arc;
    use uuid::Uuid;

    /// A user with a provisioned default personal drive.
    pub(crate) async fn make_user_with_drive(pool: &Arc<PgPool>, name: &str) -> Uuid {
        let uid = make_user(pool, name).await;
        DrivePgRepository::new(pool.clone())
            .create_personal_drive_atomic(uid, None)
            .await
            .expect("create personal drive");
        uid
    }

    /// Creates a shared drive owned by `owner`. Returns (drive_id, root_folder_id).
    pub(crate) async fn make_shared_drive(
        pool: &Arc<PgPool>,
        name: &str,
        owner: Uuid,
    ) -> (Uuid, Uuid) {
        let d = DrivePgRepository::new(pool.clone())
            .create_shared_drive_atomic(
                name,
                crate::domain::services::authorization::Subject::User(owner),
                None,
                owner,
            )
            .await
            .expect("create shared drive");
        (d.drive.id, d.drive.root_folder_id)
    }

    /// (personal_drive_id, root_folder_id) of the user's default drive.
    pub(crate) async fn personal_root(pool: &PgPool, user: Uuid) -> (Uuid, Uuid) {
        sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT id, root_folder_id FROM storage.drives WHERE default_for_user = $1",
        )
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn insert_mount(
        pool: &PgPool,
        parent: Uuid,
        drive: Uuid,
        target: Uuid,
        by: Uuid,
        name: &str,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by, mount_target_id)
             VALUES ($1, $2, $3, $4, $4, $5) RETURNING id",
        )
        .bind(name)
        .bind(parent)
        .bind(drive)
        .bind(by)
        .bind(target)
        .fetch_one(pool)
        .await
    }

    #[tokio::test]
    async fn mount_row_accepted_in_personal_drive() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let id = insert_mount(
            &pool,
            bob_root,
            bob_drive,
            alice.mount_folder_id,
            bob,
            "Docs",
        )
        .await;
        assert!(id.is_ok(), "{id:?}");
    }

    #[tokio::test]
    async fn i1_mount_in_shared_drive_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (sd, sd_root) = make_shared_drive(&pool, "Team", bob).await;
        let err = insert_mount(&pool, sd_root, sd, alice.mount_folder_id, bob, "Docs")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("personal drive"), "{err}");
    }

    #[tokio::test]
    async fn i2_child_under_mount_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let mount = insert_mount(
            &pool,
            bob_root,
            bob_drive,
            alice.mount_folder_id,
            bob,
            "Docs",
        )
        .await
        .unwrap();
        let err = sqlx::query(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ('x', $1, $2, $3, $3)",
        )
        .bind(mount)
        .bind(bob_drive)
        .bind(bob)
        .execute(&*pool)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cannot have children"), "{err}");
    }

    #[tokio::test]
    async fn i3_mount_of_mount_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let carol = make_user_with_drive(&pool, "carol").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let (carol_drive, carol_root) = personal_root(&pool, carol).await;
        let bob_mount = insert_mount(
            &pool,
            bob_root,
            bob_drive,
            alice.mount_folder_id,
            bob,
            "Docs",
        )
        .await
        .unwrap();
        let err = insert_mount(&pool, carol_root, carol_drive, bob_mount, carol, "Docs")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("itself be a mount"), "{err}");
    }

    #[tokio::test]
    async fn i4_target_in_same_drive_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let (alice_drive, alice_root) = personal_root(&pool, alice.owner_id).await;
        let err = insert_mount(
            &pool,
            alice_root,
            alice_drive,
            alice.mount_folder_id,
            alice.owner_id,
            "Docs2",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("another drive"), "{err}");
    }

    #[tokio::test]
    async fn i5_second_mount_of_same_target_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        insert_mount(
            &pool,
            bob_root,
            bob_drive,
            alice.mount_folder_id,
            bob,
            "Docs",
        )
        .await
        .unwrap();
        let err = insert_mount(
            &pool,
            bob_root,
            bob_drive,
            alice.mount_folder_id,
            bob,
            "Docs (2)",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("idx_folders_one_mount_per_target"),
            "{err}"
        );
    }
}

#[cfg(all(test, integration_tests))]
mod repo_tests {
    use super::trigger_tests::{make_shared_drive, make_user_with_drive, personal_root};
    use super::*;
    use crate::mount_it_support::{fresh_db, provision_folder};

    async fn grant_folder(
        pool: &sqlx::PgPool,
        subject_type: &str,
        subject: Uuid,
        folder: Uuid,
        by: Uuid,
    ) {
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ($1, $2, 'folder', $3, 'viewer', $4)",
        )
        .bind(subject_type)
        .bind(subject)
        .bind(folder)
        .bind(by)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn grant_drive(pool: &sqlx::PgPool, subject: Uuid, drive: Uuid, by: Uuid) {
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'drive', $2, 'editor', $3)",
        )
        .bind(subject)
        .bind(drive)
        .bind(by)
        .execute(pool)
        .await
        .unwrap();
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
    async fn worthy_targets_direct_grant_and_shared_drive() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, _) = personal_root(&pool, bob).await;
        grant_folder(&pool, "user", bob, alice.mount_folder_id, alice.owner_id).await;
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        grant_drive(&pool, bob, sd, alice.owner_id).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        let mut got = repo.mount_worthy_targets(bob, bob_drive).await.unwrap();
        got.sort();
        let mut want = vec![alice.mount_folder_id, sd_root];
        want.sort();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn worthy_targets_skip_nested_grant_and_own_drive_and_granter() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let sub = plain_folder(
            &pool,
            "Sub",
            alice.mount_folder_id,
            alice.drive_id,
            alice.owner_id,
        )
        .await;
        grant_folder(&pool, "user", bob, alice.mount_folder_id, alice.owner_id).await;
        grant_folder(&pool, "user", bob, sub, alice.owner_id).await;
        grant_folder(&pool, "user", bob, bob_root, alice.owner_id).await;
        grant_folder(
            &pool,
            "user",
            alice.owner_id,
            alice.mount_folder_id,
            alice.owner_id,
        )
        .await;
        let repo = ShareMountPgRepository::new(pool.clone());
        assert_eq!(
            repo.mount_worthy_targets(bob, bob_drive).await.unwrap(),
            vec![alice.mount_folder_id]
        );
        let (alice_drive, _) = personal_root(&pool, alice.owner_id).await;
        assert!(
            repo.mount_worthy_targets(alice.owner_id, alice_drive)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn worthy_targets_skip_folder_grant_inside_member_drive_and_declined() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, _) = personal_root(&pool, bob).await;
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        let inside = plain_folder(&pool, "Inside", sd_root, sd, alice.owner_id).await;
        grant_drive(&pool, bob, sd, alice.owner_id).await;
        grant_folder(&pool, "user", bob, inside, alice.owner_id).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        assert_eq!(
            repo.mount_worthy_targets(bob, bob_drive).await.unwrap(),
            vec![sd_root]
        );
        repo.insert_decline(bob, sd_root).await.unwrap();
        assert!(
            repo.mount_worthy_targets(bob, bob_drive)
                .await
                .unwrap()
                .is_empty()
        );
        repo.delete_decline(bob, sd_root).await.unwrap();
        assert_eq!(
            repo.mount_worthy_targets(bob, bob_drive).await.unwrap(),
            vec![sd_root]
        );
    }

    #[tokio::test]
    async fn create_mount_suffixes_on_collision_and_keeps_name_after_rename() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let own = plain_folder(&pool, "Docs", bob_root, bob_drive, bob).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        let m = repo
            .create_mount(bob_drive, bob_root, alice.mount_folder_id, bob)
            .await
            .unwrap();
        assert_eq!(m.name, "Docs (2)");
        assert_eq!(m.kind, MountKind::SharedFolder);
        assert_eq!(m.recipient_id, bob);
        assert_eq!(m.target_drive_id, alice.drive_id);
        sqlx::query("UPDATE storage.folders SET name = 'Other' WHERE id = $1")
            .bind(own)
            .execute(&*pool)
            .await
            .unwrap();
        let again = repo.mount_info(m.mount_id).await.unwrap().unwrap();
        assert_eq!(again.name, "Docs (2)");
        let dup = repo
            .create_mount(bob_drive, bob_root, alice.mount_folder_id, bob)
            .await
            .unwrap_err();
        assert_eq!(dup.kind, crate::common::errors::ErrorKind::AlreadyExists);
    }

    #[tokio::test]
    async fn create_mount_of_shared_drive_root_has_drive_kind() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let (_sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        let m = repo
            .create_mount(bob_drive, bob_root, sd_root, bob)
            .await
            .unwrap();
        assert_eq!(m.kind, MountKind::SharedDrive);
        assert_eq!(m.name, "Team");
    }

    #[tokio::test]
    async fn mounts_in_subtree_and_delete() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let parent = plain_folder(&pool, "Projects", bob_root, bob_drive, bob).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        let m = repo
            .create_mount(bob_drive, parent, alice.mount_folder_id, bob)
            .await
            .unwrap();
        let found = repo.mounts_in_subtree(bob_root).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mount_id, m.mount_id);
        let by_target = repo
            .mounts_for_targets(bob_drive, &[alice.mount_folder_id])
            .await
            .unwrap();
        assert_eq!(by_target.get(&alice.mount_folder_id), Some(&m.mount_id));
        repo.delete_mount(m.mount_id).await.unwrap();
        assert!(repo.mount_info(m.mount_id).await.unwrap().is_none());
        assert!(repo.mounts_in_subtree(bob_root).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn copy_folder_tree_skips_mount_rows() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let projects = plain_folder(&pool, "Projects", bob_root, bob_drive, bob).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        repo.create_mount(bob_drive, projects, alice.mount_folder_id, bob)
            .await
            .unwrap();
        let (new_root, folders_copied, _files): (String, i64, i64) = sqlx::query_as(
            "SELECT new_root_id, folders_copied, files_copied FROM storage.copy_folder_tree($1, $2, $3)",
        )
        .bind(projects)
        .bind(bob_root)
        .bind("Projects copy")
        .fetch_one(&*pool)
        .await
        .unwrap();
        assert_eq!(folders_copied, 1, "the mount row is not copied");
        let copied_mounts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.folders WHERE parent_id = $1::uuid AND mount_target_id IS NOT NULL",
        )
        .bind(Uuid::parse_str(&new_root).unwrap())
        .fetch_one(&*pool)
        .await
        .unwrap();
        assert_eq!(copied_mounts, 0);
    }

    #[tokio::test]
    async fn caller_can_read_target_follows_grants() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let repo = ShareMountPgRepository::new(pool.clone());
        assert!(
            !repo
                .caller_can_read_target(bob, alice.mount_folder_id)
                .await
                .unwrap()
        );
        grant_folder(&pool, "user", bob, alice.mount_folder_id, alice.owner_id).await;
        assert!(
            repo.caller_can_read_target(bob, alice.mount_folder_id)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE storage.folders SET is_trashed = TRUE WHERE id = $1")
            .bind(alice.mount_folder_id)
            .execute(&*pool)
            .await
            .unwrap();
        assert!(
            !repo
                .caller_can_read_target(bob, alice.mount_folder_id)
                .await
                .unwrap()
        );
        repo.touch_reconciled(bob).await.unwrap();
        let ts: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT mounts_reconciled_at FROM auth.users WHERE id = $1")
                .bind(bob)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert!(ts.is_some());
    }
}
