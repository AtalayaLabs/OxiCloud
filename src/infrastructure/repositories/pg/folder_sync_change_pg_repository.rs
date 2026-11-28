//! WebDAV sync-collection change log — a `SyncChangeLogSchema` instance
//! over `storage.folder_sync_changes` (see `sync_change_log_pg_repository.rs`
//! for the shared implementation; populated by triggers defined in
//! `migrations/20261126000000_folder_sync_changes.sql`).

use crate::infrastructure::repositories::pg::sync_change_log_pg_repository::{
    SyncChangeLogPgRepository, SyncChangeLogSchema,
};

pub struct FolderSyncChangeSchema;

impl SyncChangeLogSchema for FolderSyncChangeSchema {
    const TABLE: &'static str = "storage.folder_sync_changes";
    const WATERMARK_TABLE: &'static str = "storage.folder_sync_watermark";
    const COLLECTION_ID_COLUMN: &'static str = "collection_folder_id";
    const LABEL_COLUMN: &'static str = "member_href_name";
    const IS_COLLECTION_EXPR: &'static str = "member_type = 'folder'";
    const LOG_NAME: &'static str = "folder_sync_changes";
}

/// Type alias, not a newtype — `FolderSyncChangePgRepository::new(pool)`
/// keeps compiling unchanged at every existing DI/call site.
pub type FolderSyncChangePgRepository = SyncChangeLogPgRepository<FolderSyncChangeSchema>;

#[cfg(feature = "integration_tests")]
// See `session_pg_repository.rs`: CI builds the lib as a plain dependency
// of the `tests/` binaries, where these helpers look unused.
#[allow(dead_code)]
mod integration_tests {
    //! Pins what the `storage.files` / `storage.folders` triggers log for
    //! every kind of write a sync client must learn about. These run the
    //! real triggers against real rows, so a change to either migration
    //! that drops or mislabels a change fails here, not on a client.
    //!
    //! Each test owns a fresh drive and its folders, and reads only its
    //! own collections (AGENTS.md § DB integration tests share one
    //! database).
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::FolderSyncChangePgRepository;
    use crate::domain::entities::sync_token::SyncCursor;
    use crate::domain::repositories::sync_change_log_repository::{
        SyncChangeKind, SyncChangeLogRepository,
    };
    use crate::integration_test_support::test_db_url;

    struct Fixture {
        pool: Arc<PgPool>,
        repo: FolderSyncChangePgRepository,
        owner: Uuid,
        drive: Uuid,
        root: Uuid,
        blob: String,
        blob2: String,
    }

    /// `(label, kind)` pairs of one delta, order-free.
    type Delta = BTreeSet<(String, &'static str)>;

    fn kind(k: SyncChangeKind) -> &'static str {
        match k {
            SyncChangeKind::Created => "created",
            SyncChangeKind::Updated => "updated",
            SyncChangeKind::Deleted => "deleted",
        }
    }

    fn delta(pairs: &[(&str, &'static str)]) -> Delta {
        pairs.iter().map(|(l, k)| ((*l).to_owned(), *k)).collect()
    }

    impl Fixture {
        async fn new() -> Self {
            let pool = Arc::new(
                PgPoolOptions::new()
                    .max_connections(2)
                    .connect(&test_db_url())
                    .await
                    .expect("connect to integration-test PostgreSQL"),
            );
            let owner: Uuid = sqlx::query_scalar("SELECT id FROM auth.users LIMIT 1")
                .fetch_one(&*pool)
                .await
                .expect("test DB must have at least one user");

            // Drive + root folder in one transaction: the
            // no-orphan-root trigger is deferred to COMMIT (mirrors
            // `drives_consistency_service.rs::seed_drift`).
            let mut tx = pool.begin().await.expect("begin");
            let drive: Uuid = sqlx::query_scalar(
                "INSERT INTO storage.drives (kind, default_for_user, quota_bytes, used_bytes) \
                 VALUES ('personal', NULL, NULL, 0) RETURNING id",
            )
            .fetch_one(&mut *tx)
            .await
            .expect("insert drive");
            let root: Uuid = sqlx::query_scalar(
                "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) \
                 VALUES ('sync-trigger-root', NULL, $1, $2, $2) RETURNING id",
            )
            .bind(drive)
            .bind(owner)
            .fetch_one(&mut *tx)
            .await
            .expect("insert root folder");
            sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
                .bind(root)
                .bind(drive)
                .execute(&mut *tx)
                .await
                .expect("wire root folder");
            tx.commit().await.expect("commit drive");

            let mut blobs = Vec::new();
            for _ in 0..2 {
                let hash = format!(
                    "{:032x}{:032x}",
                    Uuid::new_v4().as_u128(),
                    Uuid::new_v4().as_u128()
                );
                sqlx::query(
                    "INSERT INTO storage.blobs (hash, size, ref_count, content_type) \
                     VALUES ($1, 3, 1000, 'text/plain')",
                )
                .bind(&hash)
                .execute(&*pool)
                .await
                .expect("insert blob");
                blobs.push(hash);
            }
            let blob2 = blobs.pop().expect("blob2");
            let blob = blobs.pop().expect("blob");

            Self {
                repo: FolderSyncChangePgRepository::new(pool.clone()),
                pool,
                owner,
                drive,
                root,
                blob,
                blob2,
            }
        }

        async fn folder(&self, parent: Uuid, name: &str) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) \
                 VALUES ($1, $2, $3, $4, $4) RETURNING id",
            )
            .bind(name)
            .bind(parent)
            .bind(self.drive)
            .bind(self.owner)
            .fetch_one(&*self.pool)
            .await
            .expect("insert folder")
        }

        async fn file(&self, folder: Uuid, name: &str) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO storage.files \
                   (name, folder_id, drive_id, blob_hash, size, mime_type, is_trashed, \
                    created_by, updated_by) \
                 VALUES ($1, $2, $3, $4, 3, 'text/plain', false, $5, $5) RETURNING id",
            )
            .bind(name)
            .bind(folder)
            .bind(self.drive)
            .bind(&self.blob)
            .bind(self.owner)
            .fetch_one(&*self.pool)
            .await
            .expect("insert file")
        }

        async fn exec(&self, sql: &str, id: Uuid) {
            sqlx::query(sql)
                .bind(id)
                .execute(&*self.pool)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }

        async fn cursor(&self, collection: Uuid) -> SyncCursor {
            self.repo
                .current_cursor(collection)
                .await
                .expect("current_cursor")
        }

        /// Exactly what a client holding `since` is told about
        /// `collection`, and the cursor to poll with next. Exact: a cursor
        /// never re-reports a change it covered, so no filtering here.
        async fn poll(&self, collection: Uuid, since: SyncCursor) -> (Delta, SyncCursor) {
            let (rows, next) = self
                .repo
                .changes_since(collection, Some(&since))
                .await
                .expect("changes_since");
            (
                rows.into_iter().map(|r| (r.label, kind(r.kind))).collect(),
                next,
            )
        }

        async fn cleanup(self) {
            let folders: Vec<Uuid> =
                sqlx::query_scalar("SELECT id FROM storage.folders WHERE drive_id = $1")
                    .bind(self.drive)
                    .fetch_all(&*self.pool)
                    .await
                    .unwrap_or_default();
            let _ = sqlx::query("DELETE FROM storage.files WHERE drive_id = $1")
                .bind(self.drive)
                .execute(&*self.pool)
                .await;
            let _ = sqlx::query("UPDATE storage.drives SET root_folder_id = NULL WHERE id = $1")
                .bind(self.drive)
                .execute(&*self.pool)
                .await;
            let _ = sqlx::query("DELETE FROM storage.folders WHERE drive_id = $1")
                .bind(self.drive)
                .execute(&*self.pool)
                .await;
            let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
                .bind(self.drive)
                .execute(&*self.pool)
                .await;
            for table in [
                "storage.folder_sync_changes",
                "storage.folder_sync_watermark",
            ] {
                let _ = sqlx::query(&format!(
                    "DELETE FROM {table} WHERE collection_folder_id = ANY($1)"
                ))
                .bind(&folders)
                .execute(&*self.pool)
                .await;
            }
            let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = ANY($1)")
                .bind(vec![self.blob.clone(), self.blob2.clone()])
                .execute(&*self.pool)
                .await;
        }
    }

    #[tokio::test]
    async fn file_create_overwrite_and_hard_delete_are_logged() {
        let fx = Fixture::new().await;
        let dir = fx.folder(fx.root, "dir").await;
        let c0 = fx.cursor(dir).await;

        let f = fx.file(dir, "a.txt").await;
        let (d, c1) = fx.poll(dir, c0).await;
        assert_eq!(d, delta(&[("a.txt", "created")]));

        // Content overwrite: new blob, same href.
        sqlx::query(
            "UPDATE storage.files SET blob_hash = $2, size = 4, updated_at = now() WHERE id = $1",
        )
        .bind(f)
        .bind(&fx.blob2)
        .execute(&*fx.pool)
        .await
        .expect("overwrite");
        let (d, c2) = fx.poll(dir, c1).await;
        assert_eq!(d, delta(&[("a.txt", "updated")]));

        fx.exec("DELETE FROM storage.files WHERE id = $1", f).await;
        let (d, _) = fx.poll(dir, c2).await;
        assert_eq!(d, delta(&[("a.txt", "deleted")]));

        fx.cleanup().await;
    }

    /// Regression: a rename was logged as one `updated` row under the new
    /// name, so the old href was never tombstoned and lived on in every
    /// href-keyed client as a ghost copy.
    #[tokio::test]
    async fn renaming_a_file_tombstones_its_old_href() {
        let fx = Fixture::new().await;
        let dir = fx.folder(fx.root, "dir").await;
        let f = fx.file(dir, "old.txt").await;
        let c0 = fx.cursor(dir).await;

        fx.exec(
            "UPDATE storage.files SET name = 'new.txt', updated_at = now() WHERE id = $1",
            f,
        )
        .await;
        let (d, _) = fx.poll(dir, c0).await;
        assert_eq!(d, delta(&[("old.txt", "deleted"), ("new.txt", "created")]));

        fx.cleanup().await;
    }

    /// Same regression for a folder: the client keeps the whole subtree
    /// under the old name unless the old href is tombstoned.
    #[tokio::test]
    async fn renaming_a_folder_tombstones_its_old_href() {
        let fx = Fixture::new().await;
        let sub = fx.folder(fx.root, "old-dir").await;
        fx.file(sub, "inside.txt").await;
        let c0 = fx.cursor(fx.root).await;

        fx.exec(
            "UPDATE storage.folders SET name = 'new-dir', updated_at = now() WHERE id = $1",
            sub,
        )
        .await;
        let (d, _) = fx.poll(fx.root, c0).await;
        assert_eq!(d, delta(&[("old-dir", "deleted"), ("new-dir", "created")]));

        fx.cleanup().await;
    }

    /// A → B and back within one poll window nets out per collection.
    #[tokio::test]
    async fn moving_a_file_is_a_delete_in_the_source_and_a_create_in_the_target() {
        let fx = Fixture::new().await;
        let a = fx.folder(fx.root, "a").await;
        let b = fx.folder(fx.root, "b").await;
        let f = fx.file(a, "m.txt").await;
        let (ca, cb) = (fx.cursor(a).await, fx.cursor(b).await);

        sqlx::query("UPDATE storage.files SET folder_id = $2, updated_at = now() WHERE id = $1")
            .bind(f)
            .bind(b)
            .execute(&*fx.pool)
            .await
            .expect("move");
        let (da, ca) = fx.poll(a, ca).await;
        let (db, cb) = fx.poll(b, cb).await;
        assert_eq!(da, delta(&[("m.txt", "deleted")]));
        assert_eq!(db, delta(&[("m.txt", "created")]));

        // Move + rename in one statement.
        sqlx::query(
            "UPDATE storage.files SET folder_id = $2, name = 'r.txt', updated_at = now() \
             WHERE id = $1",
        )
        .bind(f)
        .bind(a)
        .execute(&*fx.pool)
        .await
        .expect("move back renamed");
        let (da, _) = fx.poll(a, ca).await;
        let (db, _) = fx.poll(b, cb).await;
        assert_eq!(da, delta(&[("r.txt", "created")]));
        assert_eq!(db, delta(&[("m.txt", "deleted")]));

        fx.cleanup().await;
    }

    /// Trashing must tombstone at once — the member vanishes from the
    /// listing — and restoring must re-create it. Uses the production
    /// shape: one statement whose CTEs update `storage.folders` twice and
    /// `storage.files` once (`folder_db_repository.rs` trash).
    #[tokio::test]
    async fn trashing_a_subtree_and_restoring_it_is_logged_in_every_collection() {
        let fx = Fixture::new().await;
        let sub = fx.folder(fx.root, "sub").await;
        let deep = fx.folder(sub, "deep").await;
        fx.file(sub, "s.txt").await;
        fx.file(deep, "d.txt").await;
        let (cr, cs, cd) = (
            fx.cursor(fx.root).await,
            fx.cursor(sub).await,
            fx.cursor(deep).await,
        );

        sqlx::query(
            r#"
            WITH trash_root AS (
                UPDATE storage.folders
                   SET is_trashed = TRUE, trashed_at = NOW(),
                       original_parent_id = parent_id, updated_at = NOW()
                 WHERE id = $1 AND NOT is_trashed
                RETURNING id, lpath
            ), trash_descendant_folders AS (
                UPDATE storage.folders f
                   SET is_trashed = TRUE, trashed_at = NOW(), updated_at = NOW()
                  FROM trash_root tr
                 WHERE f.lpath <@ tr.lpath AND f.id != tr.id AND NOT f.is_trashed
                RETURNING 1
            ), trash_descendant_files AS (
                UPDATE storage.files fi
                   SET is_trashed = TRUE, trashed_at = NOW(), updated_at = NOW()
                  FROM trash_root tr
                  JOIN storage.folders f ON f.lpath <@ tr.lpath
                 WHERE fi.folder_id = f.id AND NOT fi.is_trashed
                RETURNING 1
            )
            SELECT (SELECT count(*) FROM trash_descendant_folders)
                 + (SELECT count(*) FROM trash_descendant_files)
            "#,
        )
        .bind(sub)
        .execute(&*fx.pool)
        .await
        .expect("trash subtree");

        let (dr, cr) = fx.poll(fx.root, cr).await;
        let (ds, cs) = fx.poll(sub, cs).await;
        let (dd, cd) = fx.poll(deep, cd).await;
        assert_eq!(dr, delta(&[("sub", "deleted")]));
        assert_eq!(ds, delta(&[("deep", "deleted"), ("s.txt", "deleted")]));
        assert_eq!(dd, delta(&[("d.txt", "deleted")]));

        sqlx::query(
            "UPDATE storage.folders SET is_trashed = FALSE, trashed_at = NULL, \
                 original_parent_id = NULL, updated_at = NOW() \
             WHERE drive_id = $1 AND is_trashed",
        )
        .bind(fx.drive)
        .execute(&*fx.pool)
        .await
        .expect("restore folders");
        sqlx::query(
            "UPDATE storage.files SET is_trashed = FALSE, trashed_at = NULL, updated_at = NOW() \
             WHERE drive_id = $1 AND is_trashed",
        )
        .bind(fx.drive)
        .execute(&*fx.pool)
        .await
        .expect("restore files");

        let (dr, _) = fx.poll(fx.root, cr).await;
        let (ds, _) = fx.poll(sub, cs).await;
        let (dd, _) = fx.poll(deep, cd).await;
        assert_eq!(dr, delta(&[("sub", "created")]));
        assert_eq!(ds, delta(&[("deep", "created"), ("s.txt", "created")]));
        assert_eq!(dd, delta(&[("d.txt", "created")]));

        fx.cleanup().await;
    }

    /// Trash then restore inside ONE poll window nets to "present", not a
    /// contradictory delete + create pair.
    #[tokio::test]
    async fn trash_then_restore_within_one_poll_nets_to_created() {
        let fx = Fixture::new().await;
        let dir = fx.folder(fx.root, "dir").await;
        let f = fx.file(dir, "t.txt").await;
        let c0 = fx.cursor(dir).await;

        fx.exec(
            "UPDATE storage.files SET is_trashed = TRUE, trashed_at = now() WHERE id = $1",
            f,
        )
        .await;
        fx.exec(
            "UPDATE storage.files SET is_trashed = FALSE, trashed_at = NULL WHERE id = $1",
            f,
        )
        .await;
        let (d, _) = fx.poll(dir, c0).await;
        assert_eq!(d, delta(&[("t.txt", "created")]));

        fx.cleanup().await;
    }

    /// Columns no DAV client can observe must not churn the log (one row
    /// per EXIF backfill would flood every photo folder's sync).
    #[tokio::test]
    async fn an_unobservable_column_change_is_not_logged() {
        let fx = Fixture::new().await;
        let dir = fx.folder(fx.root, "dir").await;
        let f = fx.file(dir, "p.jpg").await;
        let c0 = fx.cursor(dir).await;

        fx.exec(
            "UPDATE storage.files SET media_sort_date = now() WHERE id = $1",
            f,
        )
        .await;
        let (d, _) = fx.poll(dir, c0).await;
        assert!(d.is_empty(), "{d:?}");

        fx.cleanup().await;
    }

    /// One statement moving files out of several folders logs every row —
    /// a statement-level trigger must not keep only one per statement.
    #[tokio::test]
    async fn a_bulk_move_logs_every_member() {
        let fx = Fixture::new().await;
        let (a, b, t) = (
            fx.folder(fx.root, "a").await,
            fx.folder(fx.root, "b").await,
            fx.folder(fx.root, "t").await,
        );
        for (dir, n) in [(a, "1"), (a, "2"), (b, "3")] {
            fx.file(dir, &format!("{n}.txt")).await;
        }
        let (ca, cb, ct) = (fx.cursor(a).await, fx.cursor(b).await, fx.cursor(t).await);

        sqlx::query(
            "UPDATE storage.files SET folder_id = $1, updated_at = now() \
             WHERE folder_id = ANY($2)",
        )
        .bind(t)
        .bind(vec![a, b])
        .execute(&*fx.pool)
        .await
        .expect("bulk move");

        assert_eq!(
            fx.poll(a, ca).await.0,
            delta(&[("1.txt", "deleted"), ("2.txt", "deleted")])
        );
        assert_eq!(fx.poll(b, cb).await.0, delta(&[("3.txt", "deleted")]));
        assert_eq!(
            fx.poll(t, ct).await.0,
            delta(&[
                ("1.txt", "created"),
                ("2.txt", "created"),
                ("3.txt", "created")
            ])
        );

        fx.cleanup().await;
    }
}
