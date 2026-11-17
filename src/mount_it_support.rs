//! Shared testcontainers harness for external-mount integration tests.
//!
//! Compiled only under `#[cfg(all(test, feature = "integration_tests"))]` (the
//! `testcontainers` dev-dependency is linked into test targets only). Each
//! `fresh_db()` spins up an ephemeral Postgres, applies every migration, and
//! returns a pool — so the DB-backed tests are self-contained and need only
//! docker, not the external `spawn-db.sh` compose harness.

use std::sync::Arc;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

use crate::domain::repositories::drive_repository::DriveRepository;
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::infrastructure::repositories::pg::{DrivePgRepository, FolderDbRepository};

/// Postgres image tag — must match production (PG13+): the schema uses
/// `CREATE OR REPLACE TRIGGER` (PG14+) and the `pg_trgm` / `ltree` contrib
/// extensions. The `testcontainers` module default (`11-alpine`) is too old.
pub const PG_IMAGE_TAG: &str = "17-alpine";

/// A provisioned mount: a user with a personal drive, a child folder serving as
/// the mount root, and (when created via [`provision_mount`]) an
/// `external_mounts` row.
pub struct Provisioned {
    pub owner_id: Uuid,
    pub drive_id: Uuid,
    pub mount_folder_id: Uuid,
}

/// Bring up an ephemeral Postgres, apply every migration, return a pool. Keep
/// the returned container handle alive for the test's duration.
pub async fn fresh_db() -> (ContainerAsync<Postgres>, Arc<PgPool>) {
    let container = Postgres::default()
        .with_tag(PG_IMAGE_TAG)
        .start()
        .await
        .expect("start postgres testcontainer (is docker running?)");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to ephemeral postgres");
    sqlx::migrate!().run(&pool).await.expect("apply migrations");
    (container, Arc::new(pool))
}

/// Insert a minimal `user` role account, returning its id.
pub async fn make_user(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO auth.users (username, email, password_hash, role)
         VALUES ($1, $2, 'x', 'user') RETURNING id",
    )
    .bind(name)
    .bind(format!("{name}@example.test"))
    .fetch_one(pool)
    .await
    .expect("insert user")
}

/// Provision user → personal drive → a child folder named `folder_name` that
/// will act as the mount root. Does NOT insert an `external_mounts` row.
pub async fn provision_folder(
    pool: &Arc<PgPool>,
    user_name: &str,
    folder_name: &str,
) -> Provisioned {
    let owner_id = make_user(pool, user_name).await;
    let drive_repo = DrivePgRepository::new(pool.clone());
    let drive = drive_repo
        .create_personal_drive_atomic(owner_id, None)
        .await
        .expect("create personal drive");
    let root_folder_id = drive.drive.root_folder_id;

    let folder_repo = FolderDbRepository::new(pool.clone());
    let folder = folder_repo
        .create_folder(
            folder_name.to_string(),
            Some(root_folder_id.to_string()),
            owner_id,
        )
        .await
        .expect("create mount-root folder");
    let mount_folder_id = Uuid::parse_str(folder.id()).expect("uuid");

    Provisioned {
        owner_id,
        drive_id: drive.drive.id,
        mount_folder_id,
    }
}

/// Insert an `external_mounts` row for `mount_folder_id` with a `local_fs`
/// provider pointed at `host_path`.
pub async fn insert_mount(pool: &PgPool, p: &Provisioned, host_path: &str) {
    sqlx::query(
        "INSERT INTO storage.external_mounts
            (mount_folder_id, kind, config, name, owner_id, read_only)
         VALUES ($1, 'local_fs', $2, 'Media', $3, false)",
    )
    .bind(p.mount_folder_id)
    .bind(serde_json::json!({ "path": host_path }))
    .bind(p.owner_id)
    .execute(pool)
    .await
    .expect("insert external mount");
}
