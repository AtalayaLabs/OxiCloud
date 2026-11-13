//! PostgreSQL access for share mounts (docs/plan/share-mounts.md).

use crate::application::dtos::grant_dto::LinkStateDto;
use crate::common::errors::DomainError;
use sqlx::{Connection, PgConnection, PgPool, Postgres, Transaction};
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
    pub target_trashed: bool,
}

type MountTuple = (Uuid, Uuid, Uuid, Uuid, Uuid, bool, String, bool);

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
        target_trashed: t.7,
    }
}

/// Columns of one mount row; `m` = mount row, `t` = target, `d` = mount's drive.
const MOUNT_SELECT: &str = "SELECT m.id, m.drive_id, d.default_for_user, t.id, t.drive_id, \
            (t.parent_id IS NULL) AS is_drive_root, m.name, t.is_trashed \
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

/// `mtime`: a link row's target `tree_modified_at`, else the greater of the folder's own and its links' targets'.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkTargetMtime {
    pub mtime: i64,
    pub target_id: Option<Uuid>,
}

/// A folder, file or shared-drive root the user can reach through a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRoot {
    pub id: Uuid,
    pub name: String,
    pub is_folder: bool,
    pub modified_at: i64,
    pub tree_modified_at: Option<i64>,
    pub size: Option<i64>,
    pub mime_type: Option<String>,
    pub blob_hash: Option<String>,
}

type SharedRootTuple = (
    Uuid,
    String,
    bool,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

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

    async fn conn(&self) -> Result<sqlx::pool::PoolConnection<Postgres>, DomainError> {
        self.pool
            .acquire()
            .await
            .map_err(|e| Self::err("acquire", e))
    }

    /// Opens a transaction holding the (recipient, target) advisory lock until it ends.
    pub async fn begin_pair(
        &self,
        recipient_id: Uuid,
        target_id: Uuid,
    ) -> Result<Transaction<'static, Postgres>, DomainError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Self::err("begin_pair", e))?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || $2::text, 0))")
            .bind(recipient_id)
            .bind(target_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| Self::err("begin_pair/lock", e))?;
        Ok(tx)
    }

    pub async fn mount_info(&self, folder_id: Uuid) -> Result<Option<MountRow>, DomainError> {
        Self::mount_info_on(&mut *self.conn().await?, folder_id).await
    }

    pub async fn mount_info_on(
        conn: &mut PgConnection,
        folder_id: Uuid,
    ) -> Result<Option<MountRow>, DomainError> {
        let sql = format!("{MOUNT_SELECT} WHERE m.id = $1 AND NOT m.is_trashed");
        sqlx::query_as::<_, MountTuple>(&sql)
            .bind(folder_id)
            .fetch_optional(conn)
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

    /// Non-expired grant path from user to target: a folder grant on the target itself (user or caller_group_ids, granted_by <> user) or a drive grant on the shared drive whose root is the target. Trash state ignored.
    pub async fn reads_target(&self, user_id: Uuid, target_id: Uuid) -> Result<bool, DomainError> {
        Self::reads_target_on(&mut *self.conn().await?, user_id, target_id).await
    }

    pub async fn reads_target_on(
        conn: &mut PgConnection,
        user_id: Uuid,
        target_id: Uuid,
    ) -> Result<bool, DomainError> {
        sqlx::query_scalar::<_, bool>(
            r#"
            WITH groups AS (
                SELECT ARRAY(SELECT storage.caller_group_ids($1)) AS ids
            )
            SELECT EXISTS (
                SELECT 1 FROM storage.role_grants g, groups
                 WHERE g.resource_type = 'folder' AND g.resource_id = $2
                   AND g.granted_by <> $1
                   AND (g.expires_at IS NULL OR g.expires_at > NOW())
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1)
                     OR (g.subject_type = 'group' AND g.subject_id = ANY(groups.ids)))
            ) OR EXISTS (
                SELECT 1 FROM storage.folders t
                  JOIN storage.drives d
                    ON d.id = t.drive_id AND d.root_folder_id = t.id AND d.kind = 'shared'
                  JOIN storage.role_grants g
                    ON g.resource_type = 'drive' AND g.resource_id = d.id,
                       groups
                 WHERE t.id = $2
                   AND (g.expires_at IS NULL OR g.expires_at > NOW())
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1)
                     OR (g.subject_type = 'group' AND g.subject_id = ANY(groups.ids)))
            )
            "#,
        )
        .bind(user_id)
        .bind(target_id)
        .fetch_one(conn)
        .await
        .map_err(|e| Self::err("reads_target", e))
    }

    pub async fn drive_of_folder_on(
        conn: &mut PgConnection,
        folder_id: Uuid,
    ) -> Result<Option<Uuid>, DomainError> {
        sqlx::query_scalar::<_, Uuid>("SELECT drive_id FROM storage.folders WHERE id = $1")
            .bind(folder_id)
            .fetch_optional(conn)
            .await
            .map_err(|e| Self::err("drive_of_folder", e))
    }

    pub async fn mount_of(
        &self,
        personal_drive_id: Uuid,
        target_id: Uuid,
    ) -> Result<Option<MountRow>, DomainError> {
        Self::mount_of_on(&mut *self.conn().await?, personal_drive_id, target_id).await
    }

    pub async fn mount_of_on(
        conn: &mut PgConnection,
        personal_drive_id: Uuid,
        target_id: Uuid,
    ) -> Result<Option<MountRow>, DomainError> {
        let sql = format!(
            "{MOUNT_SELECT} WHERE m.drive_id = $1 AND m.mount_target_id = $2 AND NOT m.is_trashed"
        );
        sqlx::query_as::<_, MountTuple>(&sql)
            .bind(personal_drive_id)
            .bind(target_id)
            .fetch_optional(conn)
            .await
            .map(|o| o.map(tuple_to_row))
            .map_err(|e| Self::err("mount_of", e))
    }

    /// Live links to `target_id` held by someone other than `except_user`, whose name differs from `new_name`: (link id, recipient, link name).
    pub async fn links_differing_from(
        &self,
        target_id: Uuid,
        new_name: &str,
        except_user: Uuid,
    ) -> Result<Vec<(Uuid, Uuid, String)>, DomainError> {
        sqlx::query_as(
            "SELECT m.id, d.default_for_user, m.name \
               FROM storage.folders m \
               JOIN storage.drives d ON d.id = m.drive_id \
              WHERE m.mount_target_id = $1 AND NOT m.is_trashed \
                AND d.default_for_user IS NOT NULL AND d.default_for_user <> $3 AND m.name <> $2",
        )
        .bind(target_id)
        .bind(new_name)
        .bind(except_user)
        .fetch_all(&mut *self.conn().await?)
        .await
        .map_err(|e| Self::err("links_differing_from", e))
    }

    /// One entry per `target_ids`: the recipient's live link (if any) and whether the target is declined.
    pub async fn link_states_for(
        &self,
        recipient_id: Uuid,
        personal_drive_id: Uuid,
        target_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, LinkStateDto>, DomainError> {
        if target_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(Uuid, Option<Uuid>, Option<String>, bool)> = sqlx::query_as(
            "SELECT t.id, m.id, m.name, \
                    EXISTS (SELECT 1 FROM storage.share_mount_declines x \
                             WHERE x.recipient_id = $1 AND x.target_folder_id = t.id) \
               FROM UNNEST($3::uuid[]) AS t(id) \
               LEFT JOIN storage.folders m \
                 ON m.drive_id = $2 AND m.mount_target_id = t.id AND NOT m.is_trashed",
        )
        .bind(recipient_id)
        .bind(personal_drive_id)
        .bind(target_ids)
        .fetch_all(&mut *self.conn().await?)
        .await
        .map_err(|e| Self::err("link_states_for", e))?;
        Ok(rows
            .into_iter()
            .map(|(target, link_id, link_name, declined)| {
                (
                    target,
                    LinkStateDto {
                        link_id,
                        link_name,
                        declined,
                    },
                )
            })
            .collect())
    }

    pub async fn is_declined(
        &self,
        recipient_id: Uuid,
        target_id: Uuid,
    ) -> Result<bool, DomainError> {
        Self::is_declined_on(&mut *self.conn().await?, recipient_id, target_id).await
    }

    pub async fn is_declined_on(
        conn: &mut PgConnection,
        recipient_id: Uuid,
        target_id: Uuid,
    ) -> Result<bool, DomainError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM storage.share_mount_declines \
              WHERE recipient_id = $1 AND target_folder_id = $2)",
        )
        .bind(recipient_id)
        .bind(target_id)
        .fetch_one(conn)
        .await
        .map_err(|e| Self::err("is_declined", e))
    }

    /// folder id for a folder grant; root_folder_id for a drive grant; None when the drive has no root or the row is gone
    pub async fn target_of(
        &self,
        resource_type: &str,
        resource_id: Uuid,
    ) -> Result<Option<Uuid>, DomainError> {
        let sql = match resource_type {
            "folder" => "SELECT id FROM storage.folders WHERE id = $1",
            "drive" => "SELECT root_folder_id FROM storage.drives WHERE id = $1",
            _ => return Ok(None),
        };
        sqlx::query_scalar::<_, Option<Uuid>>(sql)
            .bind(resource_id)
            .fetch_optional(&*self.pool)
            .await
            .map(Option::flatten)
            .map_err(|e| Self::err("target_of", e))
    }

    /// group_id plus every group that (transitively) contains it
    pub async fn group_and_ancestors(&self, group_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            WITH RECURSIVE anc AS (
                SELECT $1::uuid AS id
                UNION
                SELECT m.group_id
                  FROM auth.subject_group_members m
                  JOIN anc ON m.member_group_id = anc.id
            )
            SELECT id FROM anc
            "#,
        )
        .bind(group_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::err("group_and_ancestors", e))
    }

    /// targets (folder ids, drive roots) of non-expired grants held by any of the groups
    pub async fn grant_targets_of_groups(
        &self,
        group_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, DomainError> {
        if group_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_scalar::<_, Uuid>(
            r#"
            WITH grants AS (
                SELECT resource_type, resource_id FROM storage.role_grants
                 WHERE subject_type = 'group' AND subject_id = ANY($1)
                   AND (expires_at IS NULL OR expires_at > NOW())
            )
            SELECT f.id FROM storage.folders f
             WHERE f.id IN (SELECT resource_id FROM grants WHERE resource_type = 'folder')
            UNION
            SELECT d.root_folder_id FROM storage.drives d
             WHERE d.root_folder_id IS NOT NULL
               AND d.id IN (SELECT resource_id FROM grants WHERE resource_type = 'drive')
            "#,
        )
        .bind(group_ids)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::err("grant_targets_of_groups", e))
    }

    /// targets of the user's links in their default drive plus the targets they declined
    pub async fn link_and_decline_targets(&self, user_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT f.mount_target_id FROM storage.folders f
              JOIN storage.drives d ON d.id = f.drive_id
             WHERE d.default_for_user = $1 AND f.mount_target_id IS NOT NULL
            UNION
            SELECT target_folder_id FROM storage.share_mount_declines WHERE recipient_id = $1
            "#,
        )
        .bind(user_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| Self::err("link_and_decline_targets", e))
    }

    /// Folder, file and shared-drive roots the user holds a live grant on (declined included).
    pub async fn shared_roots(&self, user_id: Uuid) -> Result<Vec<SharedRoot>, DomainError> {
        sqlx::query_as::<_, SharedRootTuple>(
            r#"
            WITH groups AS (
                SELECT ARRAY(SELECT storage.caller_group_ids($1)) AS ids
            ),
            grants AS (
                SELECT g.resource_type, g.resource_id, g.granted_by
                  FROM storage.role_grants g, groups
                 WHERE (g.expires_at IS NULL OR g.expires_at > NOW())
                   AND ((g.subject_type = 'user'  AND g.subject_id = $1)
                     OR (g.subject_type = 'group' AND g.subject_id = ANY(groups.ids)))
            )
            SELECT f.id, f.name, TRUE,
                   EXTRACT(EPOCH FROM f.content_modified_at)::bigint,
                   EXTRACT(EPOCH FROM f.tree_modified_at)::bigint,
                   NULL::bigint, NULL::text, NULL::text
              FROM storage.folders f
             WHERE NOT f.is_trashed
               AND f.id IN (SELECT resource_id FROM grants
                             WHERE resource_type = 'folder' AND granted_by <> $1)
            UNION
            SELECT f.id, f.name, TRUE,
                   EXTRACT(EPOCH FROM f.content_modified_at)::bigint,
                   EXTRACT(EPOCH FROM f.tree_modified_at)::bigint,
                   NULL::bigint, NULL::text, NULL::text
              FROM storage.drives d
              JOIN storage.folders f ON f.id = d.root_folder_id
             WHERE d.kind = 'shared' AND NOT f.is_trashed
               AND d.id IN (SELECT resource_id FROM grants WHERE resource_type = 'drive')
            UNION
            SELECT fi.id, fi.name, FALSE,
                   EXTRACT(EPOCH FROM fi.content_modified_at)::bigint,
                   NULL::bigint, fi.size, fi.mime_type, fi.blob_hash::text
              FROM storage.files fi
             WHERE NOT fi.is_trashed
               AND fi.id IN (SELECT resource_id FROM grants
                              WHERE resource_type = 'file' AND granted_by <> $1)
            ORDER BY 1
            "#,
        )
        .bind(user_id)
        .fetch_all(&*self.pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(
                    |(
                        id,
                        name,
                        is_folder,
                        modified_at,
                        tree_modified_at,
                        size,
                        mime_type,
                        blob_hash,
                    )| {
                        SharedRoot {
                            id,
                            name,
                            is_folder,
                            modified_at,
                            tree_modified_at,
                            size,
                            mime_type,
                            blob_hash,
                        }
                    },
                )
                .collect()
        })
        .map_err(|e| Self::err("shared_roots", e))
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
        Self::create_mount_on(
            &mut *self.conn().await?,
            personal_drive_id,
            parent_id,
            target_id,
            user_id,
        )
        .await
    }

    /// Each insert attempt runs in its own savepoint, so a name collision does not abort an enclosing transaction.
    pub async fn create_mount_on(
        conn: &mut PgConnection,
        personal_drive_id: Uuid,
        parent_id: Uuid,
        target_id: Uuid,
        user_id: Uuid,
    ) -> Result<MountRow, DomainError> {
        let base: String = sqlx::query_scalar("SELECT name FROM storage.folders WHERE id = $1")
            .bind(target_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| Self::err("create_mount/name", e))?
            .ok_or_else(|| DomainError::not_found("Folder", target_id.to_string()))?;

        for attempt in 1..=100u32 {
            let name = if attempt == 1 {
                base.clone()
            } else {
                format!("{base} ({attempt})")
            };
            let mut sp = conn
                .begin()
                .await
                .map_err(|e| Self::err("create_mount/savepoint", e))?;
            let res = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by, mount_target_id) \
                 VALUES ($1, $2, $3, $4, $4, $5) RETURNING id",
            )
            .bind(&name)
            .bind(parent_id)
            .bind(personal_drive_id)
            .bind(user_id)
            .bind(target_id)
            .fetch_one(&mut *sp)
            .await;
            match res {
                Ok(id) => {
                    sp.commit()
                        .await
                        .map_err(|e| Self::err("create_mount/release", e))?;
                    return Self::mount_info_on(conn, id).await?.ok_or_else(|| {
                        DomainError::internal_error("ShareMountDb", "mount vanished after insert")
                    });
                }
                Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505") => {
                    sp.rollback()
                        .await
                        .map_err(|e| Self::err("create_mount/rollback", e))?;
                    if db.constraint() == Some("idx_folders_one_mount_per_target") {
                        return Err(DomainError::already_exists(
                            "ShareMount",
                            target_id.to_string(),
                        ));
                    }
                }
                Err(e) => return Err(Self::err("create_mount", e)),
            }
        }
        Err(DomainError::conflict(
            "ShareMount",
            "no free name after 100 attempts",
        ))
    }

    /// Plain folder `name` under `root`, created on demand; mounts and same-named files are stepped over with ` (2)`, ` (3)`, ….
    pub async fn share_folder(
        &self,
        drive_id: Uuid,
        root: Uuid,
        name: &str,
        user_id: Uuid,
    ) -> Result<Uuid, DomainError> {
        Self::share_folder_on(&mut *self.conn().await?, drive_id, root, name, user_id).await
    }

    pub async fn share_folder_on(
        conn: &mut PgConnection,
        drive_id: Uuid,
        root: Uuid,
        name: &str,
        user_id: Uuid,
    ) -> Result<Uuid, DomainError> {
        for attempt in 1..=100u32 {
            let candidate = if attempt == 1 {
                name.to_owned()
            } else {
                format!("{name} ({attempt})")
            };
            if let Some(id) = Self::plain_child_folder(&mut *conn, root, &candidate).await? {
                return Ok(id);
            }
            let file_taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM storage.files WHERE folder_id = $1 AND name = $2 AND NOT is_trashed)",
            )
            .bind(root)
            .bind(&candidate)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| Self::err("share_folder/file", e))?;
            if file_taken {
                continue;
            }
            let mut sp = conn
                .begin()
                .await
                .map_err(|e| Self::err("share_folder/savepoint", e))?;
            let res = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) \
                 VALUES ($1, $2, $3, $4, $4) RETURNING id",
            )
            .bind(&candidate)
            .bind(root)
            .bind(drive_id)
            .bind(user_id)
            .fetch_one(&mut *sp)
            .await;
            match res {
                Ok(id) => {
                    sp.commit()
                        .await
                        .map_err(|e| Self::err("share_folder/release", e))?;
                    return Ok(id);
                }
                Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505") => {
                    sp.rollback()
                        .await
                        .map_err(|e| Self::err("share_folder/rollback", e))?;
                    if let Some(id) = Self::plain_child_folder(&mut *conn, root, &candidate).await?
                    {
                        return Ok(id);
                    }
                }
                Err(e) => return Err(Self::err("share_folder", e)),
            }
        }
        Err(DomainError::conflict(
            "ShareMount",
            "no free share folder name after 100 attempts",
        ))
    }

    async fn plain_child_folder(
        conn: &mut PgConnection,
        parent: Uuid,
        name: &str,
    ) -> Result<Option<Uuid>, DomainError> {
        sqlx::query_scalar(
            "SELECT id FROM storage.folders WHERE parent_id = $1 AND name = $2 AND NOT is_trashed AND mount_target_id IS NULL",
        )
        .bind(parent)
        .bind(name)
        .fetch_optional(conn)
        .await
        .map_err(|e| Self::err("share_folder/lookup", e))
    }

    pub async fn delete_mount(&self, mount_id: Uuid) -> Result<(), DomainError> {
        Self::delete_mount_on(&mut *self.conn().await?, mount_id).await
    }

    pub async fn delete_mount_on(
        conn: &mut PgConnection,
        mount_id: Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM storage.folders WHERE id = $1 AND mount_target_id IS NOT NULL")
            .bind(mount_id)
            .execute(conn)
            .await
            .map(|_| ())
            .map_err(|e| Self::err("delete_mount", e))
    }

    /// Deletes every link row and every decline in one transaction; returns (links, declines).
    pub async fn drop_all_links_and_declines(&self) -> Result<(u64, u64), DomainError> {
        let e = |ex| Self::err("drop_all_links_and_declines", ex);
        let mut tx = self.pool.begin().await.map_err(e)?;
        let links = sqlx::query("DELETE FROM storage.folders WHERE mount_target_id IS NOT NULL")
            .execute(&mut *tx)
            .await
            .map_err(e)?
            .rows_affected();
        let declines = sqlx::query("DELETE FROM storage.share_mount_declines")
            .execute(&mut *tx)
            .await
            .map_err(e)?
            .rows_affected();
        tx.commit().await.map_err(e)?;
        Ok((links, declines))
    }

    pub async fn insert_decline(
        &self,
        recipient_id: Uuid,
        target_id: Uuid,
    ) -> Result<(), DomainError> {
        Self::insert_decline_on(&mut *self.conn().await?, recipient_id, target_id).await
    }

    pub async fn insert_decline_on(
        conn: &mut PgConnection,
        recipient_id: Uuid,
        target_id: Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "INSERT INTO storage.share_mount_declines (recipient_id, target_folder_id) \
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(recipient_id)
        .bind(target_id)
        .execute(conn)
        .await
        .map(|_| ())
        .map_err(|e| Self::err("insert_decline", e))
    }

    pub async fn delete_decline(
        &self,
        recipient_id: Uuid,
        target_id: Uuid,
    ) -> Result<(), DomainError> {
        Self::delete_decline_on(&mut *self.conn().await?, recipient_id, target_id).await
    }

    pub async fn delete_decline_on(
        conn: &mut PgConnection,
        recipient_id: Uuid,
        target_id: Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "DELETE FROM storage.share_mount_declines WHERE recipient_id = $1 AND target_folder_id = $2",
        )
        .bind(recipient_id)
        .bind(target_id)
        .execute(conn)
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

    /// Per given folder holding readable links of `caller_id` in its subtree: the effective etag mtime and, on a link row, its target.
    pub async fn max_link_target_mtime(
        &self,
        caller_id: Uuid,
        folder_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, LinkTargetMtime>, DomainError> {
        if folder_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = format!(
            "WITH links AS ( \
                SELECT m.id, m.drive_id, m.lpath, t.id AS target_id, \
                       EXTRACT(EPOCH FROM t.tree_modified_at)::bigint AS mtime \
                  FROM storage.drives d \
                  JOIN storage.folders m ON m.drive_id = d.id \
                       AND m.mount_target_id IS NOT NULL AND NOT m.is_trashed \
                  JOIN storage.folders t ON t.id = m.mount_target_id AND NOT t.is_trashed \
                 WHERE d.default_for_user = $1 AND {CALLER_READS_T} \
             ) \
             SELECT f.id, \
                    GREATEST(MAX(l.mtime), CASE WHEN f.mount_target_id IS NULL \
                        THEN EXTRACT(EPOCH FROM f.tree_modified_at)::bigint END), \
                    (ARRAY_AGG(l.target_id) FILTER (WHERE l.id = f.id))[1] \
               FROM storage.folders f \
               JOIN links l ON l.drive_id = f.drive_id AND l.lpath <@ f.lpath \
              WHERE f.id = ANY($2) \
              GROUP BY f.id, f.mount_target_id, f.tree_modified_at"
        );
        sqlx::query_as::<_, (Uuid, i64, Option<Uuid>)>(&sql)
            .bind(caller_id)
            .bind(folder_ids)
            .fetch_all(&*self.pool)
            .await
            .map(|v| {
                v.into_iter()
                    .map(|(id, mtime, target_id)| (id, LinkTargetMtime { mtime, target_id }))
                    .collect()
            })
            .map_err(|e| Self::err("max_link_target_mtime", e))
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

    /// A non-default personal drive of `owner`, with the owner grant its creation writes. Returns (drive_id, root_folder_id).
    pub(crate) async fn make_secondary_personal_drive(pool: &PgPool, owner: Uuid) -> (Uuid, Uuid) {
        let mut tx = pool.begin().await.unwrap();
        let drive: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.drives (kind, default_for_user, policies) VALUES ('personal', NULL, '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let root: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ('Second', NULL, $1, $2, $2) RETURNING id",
        )
        .bind(drive)
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
            .bind(root)
            .bind(drive)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'drive', $2, 'owner', $1)",
        )
        .bind(owner)
        .bind(drive)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (drive, root)
    }

    pub(crate) async fn insert_mount(
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
        assert!(err.to_string().contains("default personal drive"), "{err}");
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
    use super::trigger_tests::{
        insert_mount, make_secondary_personal_drive, make_shared_drive, make_user_with_drive,
        personal_root,
    };
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

    async fn plain_file(
        pool: &sqlx::PgPool,
        name: &str,
        parent: Uuid,
        drive: Uuid,
        by: Uuid,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO storage.files (name, folder_id, drive_id, created_by, updated_by, blob_hash)
             VALUES ($1, $2, $3, $4, $4, repeat('a', 64)) RETURNING id",
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
    async fn shared_roots_lists_folder_file_and_drive_root_not_own_or_trashed() {
        let (_c, pool) = fresh_db().await;
        let alice = make_user_with_drive(&pool, "alice").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (a_drive, a_root) = personal_root(&pool, alice).await;
        let (b_drive, b_root) = personal_root(&pool, bob).await;
        let f = plain_folder(&pool, "F", a_root, a_drive, alice).await;
        let t = plain_folder(&pool, "T", a_root, a_drive, alice).await;
        let x = plain_file(&pool, "X.txt", a_root, a_drive, alice).await;
        let o = plain_folder(&pool, "O", b_root, b_drive, bob).await;
        grant_folder(&pool, "user", bob, f, alice).await;
        grant_folder(&pool, "user", bob, t, alice).await;
        grant_folder(&pool, "user", alice, o, bob).await;
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'file', $2, 'viewer', $3)",
        )
        .bind(bob)
        .bind(x)
        .bind(alice)
        .execute(&*pool)
        .await
        .unwrap();
        sqlx::query("UPDATE storage.folders SET is_trashed = TRUE WHERE id = $1")
            .bind(t)
            .execute(&*pool)
            .await
            .unwrap();
        let (d, d_root) = make_shared_drive(&pool, "Team", alice).await;
        grant_drive(&pool, bob, d, alice).await;

        let repo = ShareMountPgRepository::new(pool.clone());
        let roots = repo.shared_roots(bob).await.unwrap();
        let ids: Vec<Uuid> = roots.iter().map(|r| r.id).collect();
        assert!(ids.contains(&f) && ids.contains(&x) && ids.contains(&d_root));
        assert!(!ids.contains(&t) && !ids.contains(&o));
        let file_row = roots.iter().find(|r| r.id == x).unwrap();
        assert!(!file_row.is_folder && file_row.blob_hash.is_some() && file_row.size.is_some());
        assert!(roots.iter().find(|r| r.id == f).unwrap().is_folder);
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[tokio::test]
    async fn shared_roots_grant_filters_self_group_expiry_and_declines() {
        let (_c, pool) = fresh_db().await;
        let alice = make_user_with_drive(&pool, "alice").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (a_drive, a_root) = personal_root(&pool, alice).await;
        let self_folder = plain_folder(&pool, "SelfF", a_root, a_drive, alice).await;
        let self_file = plain_file(&pool, "self.txt", a_root, a_drive, alice).await;
        let group_folder = plain_folder(&pool, "GroupF", a_root, a_drive, alice).await;
        let expired = plain_folder(&pool, "Expired", a_root, a_drive, alice).await;
        let declined = plain_folder(&pool, "Declined", a_root, a_drive, alice).await;
        grant_folder(&pool, "user", bob, self_folder, bob).await;
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'file', $2, 'viewer', $1)",
        )
        .bind(bob)
        .bind(self_file)
        .execute(&*pool)
        .await
        .unwrap();
        let group: Uuid = sqlx::query_scalar(
            "INSERT INTO auth.subject_groups (name, description, is_virtual) VALUES ('g', '', false) RETURNING id",
        )
        .fetch_one(&*pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auth.subject_group_members (group_id, member_user_id, added_by) VALUES ($1, $2, $3)",
        )
        .bind(group)
        .bind(bob)
        .bind(alice)
        .execute(&*pool)
        .await
        .unwrap();
        grant_folder(&pool, "group", group, group_folder, alice).await;
        grant_folder(&pool, "user", bob, expired, alice).await;
        sqlx::query(
            "UPDATE storage.role_grants SET expires_at = NOW() - interval '1 hour' WHERE resource_id = $1",
        )
        .bind(expired)
        .execute(&*pool)
        .await
        .unwrap();
        grant_folder(&pool, "user", bob, declined, alice).await;
        sqlx::query(
            "INSERT INTO storage.share_mount_declines (recipient_id, target_folder_id) VALUES ($1, $2)",
        )
        .bind(bob)
        .bind(declined)
        .execute(&*pool)
        .await
        .unwrap();
        let (_own_drive, own_drive_root) = make_shared_drive(&pool, "Mine", bob).await;
        let (granted_drive, granted_root) = make_shared_drive(&pool, "SelfGranted", alice).await;
        grant_drive(&pool, bob, granted_drive, bob).await;

        let repo = ShareMountPgRepository::new(pool.clone());
        let ids: Vec<Uuid> = repo
            .shared_roots(bob)
            .await
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert!(!ids.contains(&self_folder), "folder self-grant excluded");
        assert!(!ids.contains(&self_file), "file self-grant excluded");
        assert!(!ids.contains(&expired), "expired grant excluded");
        assert!(ids.contains(&group_folder), "group grant included");
        assert!(ids.contains(&declined), "declined still listed");
        assert!(
            ids.contains(&granted_root),
            "drive grant with granted_by = bob included"
        );
        assert!(
            ids.contains(&own_drive_root),
            "drive bob created (self-granted owner) included"
        );
    }

    async fn expire_grants_on(pool: &sqlx::PgPool, resource: Uuid) {
        sqlx::query(
            "UPDATE storage.role_grants SET expires_at = NOW() - interval '1 hour' WHERE resource_id = $1",
        )
        .bind(resource)
        .execute(pool)
        .await
        .unwrap();
    }

    fn sorted(mut v: Vec<Uuid>) -> Vec<Uuid> {
        v.sort();
        v
    }

    #[tokio::test]
    async fn reads_target_direct_group_drive_and_not_grantor() {
        let (_c, pool) = fresh_db().await;
        let alice = make_user_with_drive(&pool, "alice").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let owner = make_user_with_drive(&pool, "owner").await;
        let (a_drive, a_root) = personal_root(&pool, alice).await;
        let f_direct = plain_folder(&pool, "Direct", a_root, a_drive, alice).await;
        let f_via_group = plain_folder(&pool, "ViaGroup", a_root, a_drive, alice).await;
        let f_expired = plain_folder(&pool, "Expired", a_root, a_drive, alice).await;
        let f_trashed = plain_folder(&pool, "Trashed", a_root, a_drive, alice).await;
        let f_ungranted = plain_folder(&pool, "None", a_root, a_drive, alice).await;
        grant_folder(&pool, "user", bob, f_direct, alice).await;
        let g = make_group(&pool, "g").await;
        add_member(&pool, g, Some(bob), None, alice).await;
        grant_folder(&pool, "group", g, f_via_group, alice).await;
        grant_folder(&pool, "user", bob, f_expired, alice).await;
        expire_grants_on(&pool, f_expired).await;
        grant_folder(&pool, "user", bob, f_trashed, alice).await;
        sqlx::query("UPDATE storage.folders SET is_trashed = TRUE WHERE id = $1")
            .bind(f_trashed)
            .execute(&*pool)
            .await
            .unwrap();
        grant_folder(&pool, "user", alice, f_direct, alice).await;
        let (d, d_root) = make_shared_drive(&pool, "Team", alice).await;
        grant_drive(&pool, bob, d, alice).await;
        let (_own, own_drive_root) = make_shared_drive(&pool, "Mine", owner).await;

        let repo = ShareMountPgRepository::new(pool.clone());
        assert!(
            repo.reads_target(bob, f_direct).await.unwrap(),
            "direct grant"
        );
        assert!(
            repo.reads_target(bob, f_via_group).await.unwrap(),
            "group grant"
        );
        assert!(
            repo.reads_target(bob, d_root).await.unwrap(),
            "drive member"
        );
        assert!(!repo.reads_target(bob, f_expired).await.unwrap(), "expired");
        assert!(
            !repo.reads_target(alice, f_direct).await.unwrap(),
            "grantor"
        );
        assert!(
            repo.reads_target(bob, f_trashed).await.unwrap(),
            "trash ignored"
        );
        assert!(
            !repo.reads_target(bob, f_ungranted).await.unwrap(),
            "no grant"
        );
        assert!(
            repo.reads_target(owner, own_drive_root).await.unwrap(),
            "drive creator"
        );
    }

    #[tokio::test]
    async fn reads_target_false_for_root_of_a_personal_drive() {
        let (_c, pool) = fresh_db().await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (_, second_root) = make_secondary_personal_drive(&pool, bob).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        assert!(!repo.reads_target(bob, second_root).await.unwrap());
    }

    #[tokio::test]
    async fn mount_of_target_of_and_is_declined() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let (sd, sd_root) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        let repo = ShareMountPgRepository::new(pool.clone());
        let f = alice.mount_folder_id;
        assert!(repo.mount_of(bob_drive, f).await.unwrap().is_none());
        let id = insert_mount(&pool, bob_root, bob_drive, f, bob, "Docs")
            .await
            .unwrap();
        assert_eq!(
            repo.mount_of(bob_drive, f).await.unwrap().unwrap().mount_id,
            id
        );
        assert!(!repo.is_declined(bob, f).await.unwrap());
        repo.insert_decline(bob, f).await.unwrap();
        assert!(repo.is_declined(bob, f).await.unwrap());
        assert_eq!(repo.target_of("folder", f).await.unwrap(), Some(f));
        assert_eq!(repo.target_of("drive", sd).await.unwrap(), Some(sd_root));
        assert_eq!(repo.target_of("drive", Uuid::new_v4()).await.unwrap(), None);
        assert_eq!(
            repo.target_of("folder", Uuid::new_v4()).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn group_and_ancestors_and_grant_targets() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let by = alice.owner_id;
        let h = make_group(&pool, "h").await;
        let g = make_group(&pool, "g").await;
        let top = make_group(&pool, "top").await;
        let other = make_group(&pool, "other").await;
        add_member(&pool, g, None, Some(h), by).await;
        add_member(&pool, top, None, Some(g), by).await;
        let f = alice.mount_folder_id;
        grant_folder(&pool, "group", g, f, by).await;
        let (d, d_root) = make_shared_drive(&pool, "Team", by).await;
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('group', $1, 'drive', $2, 'viewer', $3)",
        )
        .bind(top)
        .bind(d)
        .bind(by)
        .execute(&*pool)
        .await
        .unwrap();
        let (a_drive, a_root) = personal_root(&pool, by).await;
        let stale = plain_folder(&pool, "Stale", a_root, a_drive, by).await;
        grant_folder(&pool, "group", g, stale, by).await;
        expire_grants_on(&pool, stale).await;
        let elsewhere = plain_folder(&pool, "Elsewhere", a_root, a_drive, by).await;
        grant_folder(&pool, "group", other, elsewhere, by).await;

        let repo = ShareMountPgRepository::new(pool.clone());
        assert_eq!(
            sorted(repo.group_and_ancestors(h).await.unwrap()),
            sorted(vec![h, g, top])
        );
        assert_eq!(
            sorted(repo.grant_targets_of_groups(&[h, g, top]).await.unwrap()),
            sorted(vec![f, d_root])
        );
        assert!(repo.grant_targets_of_groups(&[]).await.unwrap().is_empty());
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
    }

    type EventRow = (String, Uuid, String, Uuid);

    async fn events(pool: &sqlx::PgPool) -> Vec<EventRow> {
        sqlx::query_as(
            "SELECT subject_type, subject_id, resource_type, resource_id FROM storage.link_events ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn clear_events(pool: &sqlx::PgPool) {
        sqlx::query("DELETE FROM storage.link_events")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn make_group(pool: &sqlx::PgPool, name: &str) -> Uuid {
        sqlx::query_scalar("INSERT INTO auth.subject_groups (name) VALUES ($1) RETURNING id")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn add_member(
        pool: &sqlx::PgPool,
        group: Uuid,
        user: Option<Uuid>,
        child: Option<Uuid>,
        by: Uuid,
    ) {
        sqlx::query(
            "INSERT INTO auth.subject_group_members (group_id, member_user_id, member_group_id, added_by) VALUES ($1, $2, $3, $4)",
        )
        .bind(group)
        .bind(user)
        .bind(child)
        .bind(by)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn grant_insert_delete_and_expiry_update_enqueue_events() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let f = alice.mount_folder_id;
        clear_events(&pool).await;
        grant_folder(&pool, "user", bob, f, alice.owner_id).await;
        sqlx::query("UPDATE storage.role_grants SET expires_at = now() + interval '1 day' WHERE resource_id = $1")
            .bind(f)
            .execute(&*pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM storage.role_grants WHERE resource_id = $1")
            .bind(f)
            .execute(&*pool)
            .await
            .unwrap();
        let row = ("user".to_string(), bob, "folder".to_string(), f);
        assert_eq!(events(&pool).await, vec![row.clone(), row.clone(), row]);
    }

    #[tokio::test]
    async fn token_and_file_grants_enqueue_nothing() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        clear_events(&pool).await;
        grant_folder(
            &pool,
            "token",
            Uuid::new_v4(),
            alice.mount_folder_id,
            alice.owner_id,
        )
        .await;
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'file', $2, 'viewer', $3)",
        )
        .bind(bob)
        .bind(Uuid::new_v4())
        .bind(alice.owner_id)
        .execute(&*pool)
        .await
        .unwrap();
        assert!(events(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn membership_insert_and_delete_enqueue_group_events() {
        let (_c, pool) = fresh_db().await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let g = make_group(&pool, "g").await;
        let h = make_group(&pool, "h").await;
        clear_events(&pool).await;
        add_member(&pool, g, Some(bob), None, bob).await;
        add_member(&pool, g, None, Some(h), bob).await;
        sqlx::query("DELETE FROM auth.subject_group_members WHERE member_user_id = $1")
            .bind(bob)
            .execute(&*pool)
            .await
            .unwrap();
        let t = |s: &str, id, r: &str, rid| (s.to_string(), id, r.to_string(), rid);
        assert_eq!(
            events(&pool).await,
            vec![
                t("user", bob, "group", g),
                t("group", h, "group", g),
                t("user", bob, "group", g)
            ]
        );
    }

    #[tokio::test]
    async fn event_row_rolls_back_with_the_grant() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        clear_events(&pool).await;
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'folder', $2, 'viewer', $3)",
        )
        .bind(bob)
        .bind(alice.mount_folder_id)
        .bind(alice.owner_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        assert!(events(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn i1_link_outside_default_drive_rejected() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Docs").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let second: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.drives (kind, default_for_user, policies) VALUES ('personal', NULL, '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&*pool)
        .await
        .unwrap();
        let (_, bob_root) = personal_root(&pool, bob).await;
        let err = insert_mount(&pool, bob_root, second, alice.mount_folder_id, bob, "Docs")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("default personal drive"), "{err}");
    }
}
