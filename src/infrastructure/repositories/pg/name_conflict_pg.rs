//! PostgreSQL helpers for unified name-conflict resolution (`on_conflict = rename`).
//!
//! Used by file/folder creation on conflict, and designed so share-link
//! mount placement (#791) and trash-restore disambiguation can share the exact
//! same atomic `SAVEPOINT` + cross-table check loop.

use futures::future::BoxFuture;
use sqlx::{Acquire, PgConnection, PgPool};
use tracing::warn;
use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::services::name_conflict::{
    MAX_CONFLICT_ATTEMPTS, NameKind, conflict_candidates,
};

use super::transaction_utils::is_deadlock;

/// True when `err` is a PostgreSQL `unique_violation` (`23505`).
pub fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

/// Check whether `name` is already taken in the *sibling* table under
/// `parent_folder_id` (a live file when creating a folder, or a live folder
/// when creating a file).
///
/// PostgreSQL enforces uniqueness per table (`storage.files` vs
/// `storage.folders`) rather than across both; WebDAV and Nextcloud paths
/// treat both as children of the same collection, so cross-table collisions
/// must be stepped over too (matches PR #791 `ShareMountPgRepository::share_folder_on`).
pub async fn sibling_name_exists_in_other_table(
    conn: &mut PgConnection,
    parent_folder_id: Uuid,
    name: &str,
    kind: NameKind,
) -> Result<bool, sqlx::Error> {
    let sql = match kind {
        NameKind::Folder => {
            "SELECT EXISTS (SELECT 1 FROM storage.files WHERE folder_id = $1 AND name = $2 AND NOT is_trashed)"
        }
        NameKind::File => {
            "SELECT EXISTS (SELECT 1 FROM storage.folders WHERE parent_id = $1 AND name = $2 AND NOT is_trashed)"
        }
    };
    sqlx::query_scalar(sql)
        .bind(parent_folder_id)
        .bind(name)
        .fetch_one(conn)
        .await
}

/// Outcome expected from each `try_insert` savepoint attempt in [`elect_and_insert_in_tx`].
pub enum CandidateInsertError {
    /// Target parent folder does not exist (stop immediately → `NotFound`).
    ParentNotFound,
    /// Underlying SQLx error (`23505` triggers the next candidate; `40P01`
    /// triggers a transaction deadlock retry; anything else fails immediately).
    Sqlx(sqlx::Error),
}

impl From<sqlx::Error> for CandidateInsertError {
    fn from(value: sqlx::Error) -> Self {
        Self::Sqlx(value)
    }
}

/// Atomically try `base`, `base (2)`, …, `base (MAX_CONFLICT_ATTEMPTS)` inside
/// a single transaction using one `SAVEPOINT` per candidate.
///
/// - Checks [`sibling_name_exists_in_other_table`] first so files and folders
///   never shadow each other inside the same parent.
/// - Rolls back the savepoint and advances to the next candidate on `23505`.
/// - Retries the whole transaction up to 3 times on deadlock (`40P01`).
pub async fn elect_and_insert_in_tx<T, F>(
    pool: &PgPool,
    operation_name: &str,
    entity_type: &'static str,
    parent_folder_id: Uuid,
    base_name: &str,
    kind: NameKind,
    mut try_insert: F,
) -> Result<T, DomainError>
where
    F: for<'c> FnMut(
        &'c mut PgConnection,
        String,
    ) -> BoxFuture<'c, Result<T, CandidateInsertError>>,
{
    const BACKOFF_MS: [u64; 3] = [10, 50, 150];
    let mut dl_attempt = 0;

    loop {
        match elect_once(
            pool,
            entity_type,
            parent_folder_id,
            base_name,
            kind,
            &mut try_insert,
        )
        .await
        {
            Ok(val) => return Ok(val),
            Err(ElectFailure::Sqlx(e)) if dl_attempt < BACKOFF_MS.len() && is_deadlock(&e) => {
                warn!(
                    "Deadlock on {} (attempt {}/{}), backing off {}ms",
                    operation_name,
                    dl_attempt + 1,
                    BACKOFF_MS.len() + 1,
                    BACKOFF_MS[dl_attempt]
                );
                tokio::time::sleep(std::time::Duration::from_millis(BACKOFF_MS[dl_attempt])).await;
                dl_attempt += 1;
            }
            Err(ElectFailure::Sqlx(e)) => {
                return Err(DomainError::internal_error(
                    entity_type,
                    format!("{operation_name}: {e}"),
                ));
            }
            Err(ElectFailure::Domain(d)) => return Err(d),
        }
    }
}

enum ElectFailure {
    Sqlx(sqlx::Error),
    Domain(DomainError),
}

async fn elect_once<T, F>(
    pool: &PgPool,
    entity_type: &'static str,
    parent_folder_id: Uuid,
    base_name: &str,
    kind: NameKind,
    try_insert: &mut F,
) -> Result<T, ElectFailure>
where
    F: for<'c> FnMut(
        &'c mut PgConnection,
        String,
    ) -> BoxFuture<'c, Result<T, CandidateInsertError>>,
{
    let mut tx = pool.begin().await.map_err(ElectFailure::Sqlx)?;

    for candidate in conflict_candidates(base_name, kind, MAX_CONFLICT_ATTEMPTS) {
        if sibling_name_exists_in_other_table(&mut tx, parent_folder_id, &candidate, kind)
            .await
            .map_err(ElectFailure::Sqlx)?
        {
            continue;
        }

        let mut sp = tx.begin().await.map_err(ElectFailure::Sqlx)?;
        match try_insert(&mut sp, candidate).await {
            Ok(val) => {
                sp.commit().await.map_err(ElectFailure::Sqlx)?;
                tx.commit().await.map_err(ElectFailure::Sqlx)?;
                return Ok(val);
            }
            Err(CandidateInsertError::ParentNotFound) => {
                let _ = sp.rollback().await;
                let _ = tx.rollback().await;
                return Err(ElectFailure::Domain(DomainError::not_found(
                    "Folder",
                    parent_folder_id.to_string(),
                )));
            }
            Err(CandidateInsertError::Sqlx(e)) if is_unique_violation(&e) => {
                sp.rollback().await.map_err(ElectFailure::Sqlx)?;
            }
            Err(CandidateInsertError::Sqlx(e)) => {
                let _ = sp.rollback().await;
                let _ = tx.rollback().await;
                return Err(ElectFailure::Sqlx(e));
            }
        }
    }

    let _ = tx.rollback().await;
    Err(ElectFailure::Domain(DomainError::new(
        crate::domain::errors::ErrorKind::Conflict,
        entity_type,
        format!("no free name after {MAX_CONFLICT_ATTEMPTS} attempts"),
    )))
}
