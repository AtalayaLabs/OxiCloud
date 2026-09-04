//! PostgreSQL persistence for external mount configuration.
//!
//! `list_all` (re)builds the in-memory registry; `create`/`delete` back the
//! admin CRUD endpoints.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::application::ports::external_mount_ports::{
    ExternalMountRecord, ExternalMountRepositoryPort, NewExternalMount,
};
use crate::domain::errors::DomainError;
use crate::domain::services::path_service::normalize_storage_name;

/// PostgreSQL implementation of [`ExternalMountRepositoryPort`].
pub struct ExternalMountPgRepository {
    pool: Arc<PgPool>,
}

impl ExternalMountPgRepository {
    /// Construct over a connection pool.
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ExternalMountRepositoryPort for ExternalMountPgRepository {
    async fn list_all(&self) -> Result<Vec<ExternalMountRecord>, DomainError> {
        // Join each mount to its (non-trashed) root folder to pick up the
        // drive scope and the materialized path needed for path resolution.
        let rows = sqlx::query(
            r#"
            SELECT
                m.mount_folder_id AS mount_folder_id,
                m.kind            AS kind,
                m.config          AS config,
                m.name            AS name,
                m.owner_id        AS owner_id,
                m.read_only       AS read_only,
                f.drive_id        AS drive_id,
                f.path            AS mount_path
            FROM storage.external_mounts m
            JOIN storage.folders f ON f.id = m.mount_folder_id
            WHERE NOT f.is_trashed
            "#,
        )
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::database_error(format!("failed to list external mounts: {e}")))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let drive_id: Option<Uuid> = row
                .try_get("drive_id")
                .map_err(|e| DomainError::database_error(format!("external mount row: {e}")))?;
            let Some(drive_id) = drive_id else {
                // A mount root without a drive shouldn't exist post-D0; skip safely.
                tracing::warn!(
                    target: "oxicloud::external_mounts",
                    "skipping external mount with NULL drive_id"
                );
                continue;
            };
            out.push(ExternalMountRecord {
                mount_folder_id: row
                    .try_get("mount_folder_id")
                    .map_err(|e| DomainError::database_error(format!("external mount row: {e}")))?,
                kind: row
                    .try_get("kind")
                    .map_err(|e| DomainError::database_error(format!("external mount row: {e}")))?,
                config: row
                    .try_get("config")
                    .map_err(|e| DomainError::database_error(format!("external mount row: {e}")))?,
                name: row
                    .try_get("name")
                    .map_err(|e| DomainError::database_error(format!("external mount row: {e}")))?,
                owner_id: row
                    .try_get("owner_id")
                    .map_err(|e| DomainError::database_error(format!("external mount row: {e}")))?,
                read_only: row
                    .try_get("read_only")
                    .map_err(|e| DomainError::database_error(format!("external mount row: {e}")))?,
                drive_id,
                mount_path: row
                    .try_get("mount_path")
                    .map_err(|e| DomainError::database_error(format!("external mount row: {e}")))?,
            });
        }
        Ok(out)
    }

    async fn create(&self, mount: &NewExternalMount) -> Result<(), DomainError> {
        // NFC-normalize the admin-supplied display label at the last
        // touch before bind. Admin-facing (not end-user drag-drop) so
        // NFD is unlikely, but the invariant matches the storage.*
        // pattern — the sibling folder row (created via
        // `folder_db_repository::create_folder`, which already
        // normalizes) and this admin label should stay byte-consistent
        // on any table.
        let normalized_name = normalize_storage_name(&mount.name);
        sqlx::query(
            "INSERT INTO storage.external_mounts
                (mount_folder_id, kind, config, name, owner_id, read_only)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(mount.mount_folder_id)
        .bind(&mount.kind)
        .bind(&mount.config)
        .bind(&normalized_name)
        .bind(mount.owner_id)
        .bind(mount.read_only)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::database_error(format!("failed to create external mount: {e}"))
        })?;
        Ok(())
    }

    async fn delete(&self, mount_folder_id: Uuid) -> Result<bool, DomainError> {
        let res = sqlx::query("DELETE FROM storage.external_mounts WHERE mount_folder_id = $1")
            .bind(mount_folder_id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::database_error(format!("failed to delete external mount: {e}"))
            })?;
        Ok(res.rows_affected() > 0)
    }
}

// Gated on `test` too: the module uses the `testcontainers` dev-dependency,
// which is only linked into test targets — a plain `--cfg integration_tests`
// lib build (e.g. clippy's lib pass) must not try to compile it.
#[cfg(all(test, integration_tests))]
mod integration_tests {
    use super::*;
    use crate::mount_it_support::{fresh_db, insert_mount, provision_folder};

    #[tokio::test]
    async fn list_all_returns_mount_joined_with_folder() {
        let (_c, pool) = fresh_db().await;
        let p = provision_folder(&pool, "mountowner", "Media").await;
        insert_mount(&pool, &p, "/srv/media").await;

        let repo = ExternalMountPgRepository::new(pool.clone());
        let mounts = repo.list_all().await.expect("list_all");

        assert_eq!(mounts.len(), 1);
        let m = &mounts[0];
        assert_eq!(m.mount_folder_id, p.mount_folder_id);
        assert_eq!(m.kind, "local_fs");
        assert_eq!(m.owner_id, p.owner_id);
        assert_eq!(m.drive_id, p.drive_id);
        assert!(!m.read_only);
        // The joined folder path (drive-scoped materialized path) contains the
        // mount folder's name.
        assert!(
            m.mount_path.contains("Media"),
            "mount_path was {:?}",
            m.mount_path
        );
        assert_eq!(m.config["path"], "/srv/media");
    }

    #[tokio::test]
    async fn list_all_skips_trashed_mount_folder() {
        let (_c, pool) = fresh_db().await;
        let p = provision_folder(&pool, "mountowner", "Media").await;
        insert_mount(&pool, &p, "/srv/media").await;

        // Soft-delete the mount-root folder; the join filters NOT is_trashed.
        sqlx::query("UPDATE storage.folders SET is_trashed = true WHERE id = $1")
            .bind(p.mount_folder_id)
            .execute(pool.as_ref())
            .await
            .unwrap();

        let repo = ExternalMountPgRepository::new(pool.clone());
        let mounts = repo.list_all().await.expect("list_all");
        assert!(mounts.is_empty());
    }

    #[tokio::test]
    async fn list_all_empty_when_no_mounts() {
        let (_c, pool) = fresh_db().await;
        let repo = ExternalMountPgRepository::new(pool.clone());
        assert!(repo.list_all().await.expect("list_all").is_empty());
    }

    #[tokio::test]
    async fn create_and_delete_round_trip() {
        use crate::application::ports::external_mount_ports::NewExternalMount;
        let (_c, pool) = fresh_db().await;
        let p = provision_folder(&pool, "owner", "Media").await;
        let repo = ExternalMountPgRepository::new(pool.clone());

        repo.create(&NewExternalMount {
            mount_folder_id: p.mount_folder_id,
            kind: "local_fs".to_string(),
            config: serde_json::json!({ "path": "/srv/x" }),
            name: "Media".to_string(),
            owner_id: p.owner_id,
            read_only: true,
        })
        .await
        .expect("create");

        let mounts = repo.list_all().await.expect("list");
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].read_only);
        assert_eq!(mounts[0].mount_folder_id, p.mount_folder_id);

        assert!(repo.delete(p.mount_folder_id).await.expect("delete"));
        assert!(repo.list_all().await.expect("list").is_empty());
        // Deleting a non-existent mount returns false.
        assert!(!repo.delete(p.mount_folder_id).await.expect("delete again"));
    }
}
