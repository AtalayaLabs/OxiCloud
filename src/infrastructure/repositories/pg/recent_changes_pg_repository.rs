//! `GET /api/files/recent-changes` — the read side of content provenance.
//!
//! Design: `docs/plan/recently-modified.md` §3 (access filter) and §4 (the
//! two-step query). The write side lives in `file_blob_write_repository`.
//!
//! ## Two statements, not one
//!
//! The `modifier` axis orders lanes by their own recency, which the obvious
//! single query expresses as `MAX(touched_at) OVER (PARTITION BY user_id)`.
//! That cannot be computed incrementally under a `LIMIT`: Postgres must read
//! every qualifying row in the window, sort by the partition key, aggregate,
//! then sort again before the limit applies — so page 7 costs what page 1
//! costs, proportional to changes-in-window across every accessible drive.
//!
//! So: aggregate the lanes on a three-column projection first
//! ([`lanes`]), then fetch the rows for the lanes this page needs
//! ([`rows_for_lanes`]) by index scan. The `changed_at` axis needs none of
//! this and walks `idx_touches_touched` directly.
//!
//! ## The access filter is a UNION, never an OR
//!
//! Three doors lead to a file — the drive, a grant on the file, a cascading
//! grant on an ancestor folder — and `PgAclEngine::check` honours all three.
//! Written as `A OR B OR C` across three tables the planner collapses to a
//! sequential scan on `storage.files`; as a union of three separately
//! indexable selects over file ids it does not. Only `user` and `group`
//! subjects count: a `token` grant is held by a public link, not by a person,
//! and admitting it would put every publicly-linked file in every user's
//! feed.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::application::dtos::recent_changes_dto::{ChangeLane, FileChangeRow};
use crate::common::errors::DomainError;

/// Every file id the caller may see, as a CTE the two queries share.
///
/// `MATERIALIZED` on the drive arm for the same reason the photos listing
/// does it: the planner otherwise inlines the STABLE function per row.
const VISIBLE_FILES_CTE: &str = r#"
    accessible_drives AS MATERIALIZED (
        SELECT drive_id FROM storage.caller_accessible_drives($1, NULL)
    ),
    caller_subjects AS MATERIALIZED (
        SELECT $1::uuid AS id
        UNION
        SELECT storage.caller_group_ids($1)
    ),
    granted_folders AS MATERIALIZED (
        SELECT fo.lpath
          FROM storage.role_grants g
          JOIN storage.folders fo ON fo.id = g.resource_id
         WHERE g.resource_type = 'folder'
           AND g.subject_type IN ('user', 'group')
           AND g.subject_id IN (SELECT id FROM caller_subjects)
           AND (g.expires_at IS NULL OR g.expires_at > NOW())
    ),
    visible_files AS (
        SELECT f.id
          FROM storage.files f
         WHERE NOT f.is_trashed
           AND f.drive_id IN (SELECT drive_id FROM accessible_drives)
        UNION
        SELECT f.id
          FROM storage.files f
          JOIN storage.role_grants g
            ON g.resource_id = f.id
           AND g.resource_type = 'file'
           AND g.subject_type IN ('user', 'group')
           AND g.subject_id IN (SELECT id FROM caller_subjects)
           AND (g.expires_at IS NULL OR g.expires_at > NOW())
         WHERE NOT f.is_trashed
        UNION
        SELECT f.id
          FROM storage.files f
          JOIN storage.folders fo ON fo.id = f.folder_id
         WHERE NOT f.is_trashed
           AND fo.lpath <@ ANY (SELECT lpath FROM granted_folders)
    )
"#;

/// Reads for the recent-changes feed. Caller-scoped: every query takes
/// `caller_id` as `$1` and filters through [`VISIBLE_FILES_CTE`], so there is
/// no shape in which this returns a row the caller could not already open.
pub struct RecentChangesPgRepository {
    pool: Arc<PgPool>,
}

impl RecentChangesPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Step 1 — the lanes, ordered by each person's most recent change.
    ///
    /// `COUNT(*)` is the number of FILES that person changed, exactly: the
    /// touch table holds one row per (file, person), so no `DISTINCT` is
    /// needed and a file saved forty times still counts once. Counted inside
    /// the access filter, so a lane total can never hint at a file the caller
    /// cannot see.
    /// `only` restricts to named people — the `?person=` drill-in, and how the
    /// handler re-reads the totals for a lane a cursor landed inside.
    /// `after` is the lane keyset `(lane_latest, user_id)`, so a page-2
    /// request resumes at the right lane instead of re-aggregating from the
    /// top and paging by offset.
    pub async fn lanes(
        &self,
        caller_id: Uuid,
        days: i32,
        only: Option<&[Uuid]>,
        after: Option<(DateTime<Utc>, Uuid)>,
        limit: i64,
    ) -> Result<Vec<ChangeLane>, DomainError> {
        let sql = format!(
            "WITH {VISIBLE_FILES_CTE}
             SELECT t.user_id,
                    MAX(t.touched_at) AS lane_latest,
                    COUNT(*)::bigint  AS lane_total
               FROM storage.file_content_touches t
              WHERE t.touched_at > NOW() - ($2 || ' days')::interval
                AND t.file_id IN (SELECT id FROM visible_files)
                AND ($3::uuid[] IS NULL OR t.user_id = ANY($3))
              GROUP BY t.user_id
             HAVING $4::timestamptz IS NULL
                 OR MAX(t.touched_at) < $4
                 OR (MAX(t.touched_at) = $4 AND t.user_id > $5)
              ORDER BY lane_latest DESC, t.user_id
              LIMIT $6"
        );
        let rows = sqlx::query(&sql)
            .bind(caller_id)
            .bind(days.to_string())
            .bind(only)
            .bind(after.map(|(ts, _)| ts))
            .bind(after.map(|(_, id)| id))
            .bind(limit)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("RecentChanges", format!("lanes: {e}")))?;

        rows.into_iter()
            .map(|r| {
                Ok(ChangeLane {
                    user_id: r.try_get("user_id").map_err(decode)?,
                    lane_latest: r.try_get("lane_latest").map_err(decode)?,
                    lane_total: r.try_get("lane_total").map_err(decode)?,
                })
            })
            .collect()
    }

    /// Step 2 — the rows.
    ///
    /// `lane_ids` empty means "every lane in one chronological stream", which
    /// is the `changed_at` axis: keyset `(changed_at, file_id)`.
    ///
    /// With lanes, the order is **lane-major**: `array_position` over the
    /// ordered lane list is the leading sort key, so a lane's older file still
    /// sorts above the next lane's newer one. Ordering globally by time would
    /// interleave people, and a client drawing a swimlane break whenever the
    /// person changes would open and close the same lane repeatedly down the
    /// page. The keyset therefore compares `(lane_rank, changed_at, file_id)`
    /// in that precedence, which is why `after_rank` exists — the caller
    /// resolves the cursor's lane to its position in the lane list it just
    /// fetched.
    #[allow(clippy::too_many_arguments)]
    pub async fn rows_for_lanes(
        &self,
        caller_id: Uuid,
        days: i32,
        lane_ids: &[Uuid],
        after_rank: Option<i32>,
        after_ts: Option<DateTime<Utc>>,
        after_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<FileChangeRow>, DomainError> {
        let sql = format!(
            "WITH {VISIBLE_FILES_CTE}
             SELECT f.id                                           AS file_id,
                    f.name                                         AS name,
                    f.folder_id                                    AS folder_id,
                    fo.path                                        AS folder_path,
                    f.size                                         AS size,
                    f.mime_type                                    AS mime_type,
                    f.created_at                                   AS created_at,
                    f.content_modified_at                          AS content_modified_at,
                    f.drive_id                                     AS drive_id,
                    f.blob_hash                                    AS blob_hash,
                    f.created_by                                   AS created_by,
                    f.updated_by                                   AS updated_by,
                    EXISTS (
                        SELECT 1 FROM auth.user_favorites uf
                         WHERE uf.user_id = $1
                           AND uf.item_id = f.id::text
                           AND uf.item_type = 'file'
                    )                                              AS is_favorite,
                    EXISTS (
                        SELECT 1 FROM storage.role_grants g
                         WHERE g.resource_id = f.id
                           AND g.resource_type = 'file'
                    )                                              AS is_shared,
                    t.touched_at                                   AS changed_at,
                    t.user_id                                      AS changed_by
               FROM storage.file_content_touches t
               JOIN storage.files f ON f.id = t.file_id
               LEFT JOIN storage.folders fo ON fo.id = f.folder_id
              WHERE t.touched_at > NOW() - ($2 || ' days')::interval
                AND t.file_id IN (SELECT id FROM visible_files)
                AND ($3::uuid[] IS NULL OR t.user_id = ANY($3))
                AND ($5::timestamptz IS NULL
                     OR ($4::int IS NOT NULL
                         AND array_position($3, t.user_id) > $4)
                     OR (($4::int IS NULL
                          OR array_position($3, t.user_id) = $4)
                         AND (t.touched_at < $5
                              OR (t.touched_at = $5 AND t.file_id < $6))))
              ORDER BY CASE WHEN $3::uuid[] IS NULL THEN 0
                            ELSE array_position($3, t.user_id) END,
                       t.touched_at DESC, t.file_id DESC
              LIMIT $7"
        );
        let lanes: Option<&[Uuid]> = if lane_ids.is_empty() {
            None
        } else {
            Some(lane_ids)
        };
        let rows = sqlx::query(&sql)
            .bind(caller_id)
            .bind(days.to_string())
            .bind(lanes)
            .bind(after_rank)
            .bind(after_ts)
            .bind(after_id)
            .bind(limit)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("RecentChanges", format!("rows: {e}")))?;

        rows.into_iter()
            .map(|r| {
                Ok(FileChangeRow {
                    file_id: r.try_get("file_id").map_err(decode)?,
                    name: r.try_get("name").map_err(decode)?,
                    folder_id: r.try_get("folder_id").map_err(decode)?,
                    folder_path: r.try_get("folder_path").map_err(decode)?,
                    size: r.try_get("size").map_err(decode)?,
                    mime_type: r.try_get("mime_type").map_err(decode)?,
                    created_at: r.try_get("created_at").map_err(decode)?,
                    content_modified_at: r.try_get("content_modified_at").map_err(decode)?,
                    drive_id: r.try_get("drive_id").map_err(decode)?,
                    blob_hash: r.try_get("blob_hash").map_err(decode)?,
                    created_by: r.try_get("created_by").map_err(decode)?,
                    updated_by: r.try_get("updated_by").map_err(decode)?,
                    is_favorite: r.try_get("is_favorite").map_err(decode)?,
                    is_shared: r.try_get("is_shared").map_err(decode)?,
                    changed_at: r.try_get("changed_at").map_err(decode)?,
                    changed_by: r.try_get("changed_by").map_err(decode)?,
                })
            })
            .collect()
    }
}

fn decode(e: sqlx::Error) -> DomainError {
    DomainError::internal_error("RecentChanges", format!("decode: {e}"))
}

// The read side's own invariants, against a real database — the two queries
// are almost entirely SQL, so a unit test could only re-assert the string.
//
// Three of these cannot be written at the wire (`docs/plan/recently-modified.md`
// §7): they need a SECOND write to content that already exists, and REST has
// no content-overwrite endpoint — `POST /api/files/upload` answers 409 on a
// name conflict, and the overwrite path is WebDAV / NextCloud / WOPI PUT or
// the delta protocol, none of which can address another user's file in the
// caller's namespace today. So the "two people on one file" regression, which
// is the whole reason §1b exists, lives here.
//
// Fixtures are prefixed `rust-test-rc-` and deleted by prefix BEFORE each test
// seeds: cleanup at the end does not run when an assertion fails, and a leaked
// row would then fail every later run for an unrelated reason. Every assertion
// filters on this test's own ids — never a count over the whole table, which
// other suites write to concurrently.
#[cfg(all(test, integration_tests))]
mod recent_changes_read_tests {
    use super::*;
    use crate::integration_test_support::{ensure_clean_test_db, test_db_url};
    use chrono::{Duration, SubsecRound};
    use sqlx::postgres::PgPoolOptions;

    const PREFIX: &str = "rust-test-rc-";
    /// `storage.files.blob_hash` has no FK, so a fixture needs no blob.
    const HASH: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    /// Wide enough that every fixture timestamp below is inside the window.
    const DAYS: i32 = 30;

    /// One test's fixtures, under one prefix of its own.
    ///
    /// The per-test scope is not tidiness. `cargo test --workspace --tests`
    /// (what `just test-integration` runs) uses the default thread count, so
    /// the tests in this module run CONCURRENTLY — and a cleanup that wiped
    /// `rust-test-rc-%` would delete a sibling's rows between its seed and
    /// its assertions. Each test therefore owns `rust-test-rc-<tag>-` and
    /// deletes nothing outside it.
    struct Fx {
        pool: Arc<PgPool>,
        scope: String,
        root: Uuid,
        drive: Uuid,
    }

    impl Fx {
        async fn new(tag: &str) -> Fx {
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .connect(&test_db_url())
                .await
                .expect("connect to test DB — run tests/common/spawn-db.sh first");
            ensure_clean_test_db(&pool).await;
            let scope = format!("{PREFIX}{tag}-");

            // Cleanup runs BEFORE seeding, not only after: a failed assertion
            // skips the teardown, and a leaked file would then collide on
            // `idx_files_unique_name_in_folder` on the next run — which is how
            // this feature's write-path suite failed on its second run.
            //
            // `storage.role_grants.subject_id` and `.resource_id` are
            // polymorphic and carry NO foreign key, so grants cascade with
            // neither their subject nor their resource. They go first and
            // explicitly; the rest follows FK order (files, whose touches
            // cascade, then users).
            let like = format!("{scope}%");
            for stmt in [
                "DELETE FROM storage.role_grants
                  WHERE subject_id IN (SELECT id FROM auth.users WHERE username LIKE $1)",
                "DELETE FROM storage.files WHERE name LIKE $1",
                "DELETE FROM auth.users WHERE username LIKE $1",
            ] {
                let _ = sqlx::query(stmt).bind(&like).execute(&pool).await;
            }

            // A drive root to hang fixtures off, borrowed from the seeded
            // instance — these fixture users own no drive of their own.
            let (root, drive): (Uuid, Uuid) = sqlx::query_as(
                "SELECT d.root_folder_id, d.id
                   FROM storage.drives d
                  WHERE d.root_folder_id IS NOT NULL
                  LIMIT 1",
            )
            .fetch_one(&pool)
            .await
            .expect("storage.drives must be seeded (tests/common/init-test-schema.sh)");

            Fx {
                pool: Arc::new(pool),
                scope,
                root,
                drive,
            }
        }

        fn repo(&self) -> RecentChangesPgRepository {
            RecentChangesPgRepository::new(self.pool.clone())
        }

        async fn user(&self, tag: &str) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO auth.users (username, email, password_hash, role)
                 VALUES ($1, $2, 'x', 'user') RETURNING id",
            )
            .bind(format!("{}{tag}", self.scope))
            .bind(format!("{}{tag}@example.test", self.scope))
            .fetch_one(self.pool.as_ref())
            .await
            .expect("seed user")
        }

        /// The caller sees fixtures through a grant on each FILE (branch 2).
        ///
        /// Deliberately not through the drive: these users own nothing, so a
        /// dropped grant empties the feed and every assertion below fails
        /// loudly, rather than passing because the caller could read the
        /// whole seeded drive anyway.
        async fn grant_file(&self, subject: Uuid, file: Uuid) {
            sqlx::query(
                "INSERT INTO storage.role_grants
                     (subject_type, subject_id, resource_type, resource_id, role, granted_by)
                 VALUES ('user', $1, 'file', $2, 'viewer', $1)",
            )
            .bind(subject)
            .bind(file)
            .execute(self.pool.as_ref())
            .await
            .expect("seed grant");
        }

        async fn file_touched_by(&self, author: Uuid, name: &str, at: DateTime<Utc>) -> Uuid {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO storage.files
                     (name, folder_id, drive_id, blob_hash, size, mime_type,
                      created_by, updated_by, content_modified_by, content_modified_at)
                 VALUES ($1, $2, $3, $4, 3, 'text/plain', $5, $5, $5, $6)
                 RETURNING id",
            )
            .bind(format!("{}{name}", self.scope))
            .bind(self.root)
            .bind(self.drive)
            .bind(HASH)
            .bind(author)
            .bind(at)
            .fetch_one(self.pool.as_ref())
            .await
            .expect("seed file");
            self.add_touch(id, author, at).await;
            id
        }

        /// Mirrors the production upsert in `swap_blob_hash`: one row per
        /// (file, person), `GREATEST` so a replay cannot move a lane back.
        async fn add_touch(&self, file: Uuid, who: Uuid, at: DateTime<Utc>) {
            sqlx::query(
                "INSERT INTO storage.file_content_touches (file_id, user_id, touched_at)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (file_id, user_id) DO UPDATE
                    SET touched_at = GREATEST(storage.file_content_touches.touched_at,
                                              EXCLUDED.touched_at)",
            )
            .bind(file)
            .bind(who)
            .bind(at)
            .execute(self.pool.as_ref())
            .await
            .expect("seed touch");
        }
    }

    /// Whole seconds: Postgres stores microseconds and chrono carries
    /// nanoseconds, so an untruncated `Utc::now()` would not survive the
    /// round trip and the timestamp equality assertions would be flaky.
    fn ago(hours: i64) -> DateTime<Utc> {
        (Utc::now() - Duration::hours(hours)).trunc_subsecs(0)
    }

    // ── The regression §1b exists for ────────────────────────────────
    //
    // Bob writes the file on Monday, Carol on Tuesday. The file belongs to
    // BOTH lanes, and Bob's lane still carries it dated Monday — his
    // contribution did not disappear when Carol saved over it.
    //
    // A `content_modified_by` column alone cannot express this: it holds one
    // principal, so the file would sit in Carol's lane and vanish from Bob's.
    // That implementation passes every other pin in this suite.
    #[tokio::test]
    async fn two_people_on_one_file_appear_in_both_lanes() {
        let fx = Fx::new("both").await;
        let viewer = fx.user("viewer").await;
        let bob = fx.user("bob").await;
        let carol = fx.user("carol").await;

        let monday = ago(48);
        let tuesday = ago(24);
        let file = fx.file_touched_by(bob, "both.txt", monday).await;
        fx.add_touch(file, carol, tuesday).await;
        fx.grant_file(viewer, file).await;

        let repo = fx.repo();
        let only = [carol, bob];
        let lanes = repo
            .lanes(viewer, DAYS, Some(&only), None, 10)
            .await
            .expect("lanes");

        assert_eq!(lanes.len(), 2, "one file, two people, two lanes");
        assert_eq!(lanes[0].user_id, carol, "Carol's save is the more recent");
        assert_eq!(lanes[0].lane_latest, tuesday);
        assert_eq!(lanes[1].user_id, bob);
        assert_eq!(
            lanes[1].lane_latest, monday,
            "Bob's lane keeps HIS date, not the file's latest"
        );
        assert_eq!(lanes[0].lane_total, 1);
        assert_eq!(lanes[1].lane_total, 1);

        let rows = repo
            .rows_for_lanes(viewer, DAYS, &[carol, bob], None, None, None, 10)
            .await
            .expect("rows");
        let mine: Vec<_> = rows.iter().filter(|r| r.file_id == file).collect();
        assert_eq!(mine.len(), 2, "the same file is listed in both lanes");
        assert_eq!(mine[0].changed_by, carol);
        assert_eq!(mine[0].changed_at, tuesday);
        assert_eq!(mine[1].changed_by, bob);
        assert_eq!(mine[1].changed_at, monday);
    }

    // ── Repeated saves collapse; the lane counts FILES ───────────────
    //
    // `lane_total` is `COUNT(*)` over the touch table, which is only the
    // number of files because the primary key is (file_id, user_id). Bob
    // saves one file twice and another once: his lane reads 2, not 3, and the
    // twice-saved file carries the LATER timestamp and sorts above the other.
    //
    // A `JOIN` added to either query — to blobs, or to grants without a
    // subquery — would fan this out and inflate both the count and the page.
    #[tokio::test]
    async fn repeated_saves_count_once_and_carry_the_latest_timestamp() {
        let fx = Fx::new("rep").await;
        let viewer = fx.user("viewer").await;
        let bob = fx.user("bob").await;

        let first = ago(72);
        let second = ago(24);
        let other = ago(48);
        let saved_twice = fx.file_touched_by(bob, "twice.txt", first).await;
        fx.add_touch(saved_twice, bob, second).await;
        let saved_once = fx.file_touched_by(bob, "once.txt", other).await;
        fx.grant_file(viewer, saved_twice).await;
        fx.grant_file(viewer, saved_once).await;

        let touch_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.file_content_touches
              WHERE file_id = $1 AND user_id = $2",
        )
        .bind(saved_twice)
        .bind(bob)
        .fetch_one(fx.pool.as_ref())
        .await
        .expect("count touches");
        assert_eq!(touch_rows, 1, "the primary key collapses repeated saves");

        let repo = fx.repo();
        let lanes = repo
            .lanes(viewer, DAYS, Some(&[bob]), None, 10)
            .await
            .expect("lanes");
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].lane_total, 2, "two FILES, three saves");
        assert_eq!(lanes[0].lane_latest, second);

        let rows = repo
            .rows_for_lanes(viewer, DAYS, &[bob], None, None, None, 10)
            .await
            .expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].file_id, saved_twice);
        assert_eq!(
            rows[0].changed_at, second,
            "the re-save moved the row to its later date"
        );
        assert_eq!(rows[1].file_id, saved_once);
    }

    // ── Lane-major order, and the keyset that walks it ───────────────
    //
    // Bob's OLDER file must still sort above Carol's newer one, because the
    // page is ordered by lane first and time second. Ordering globally by
    // time would interleave the two people, and a client drawing a swimlane
    // break whenever the person changes would open and close the same lane
    // repeatedly down one page.
    //
    // Then the walk the handler actually performs (`collect_lane_page`):
    // finish the open lane with the `(changed_at, file_id)` keyset, then take
    // whole lanes after it with the `(lane_latest, user_id)` lane keyset.
    // Every file must appear exactly once.
    #[tokio::test]
    async fn paging_is_lane_major_and_the_keyset_never_repeats_or_skips() {
        let fx = Fx::new("lane").await;
        let viewer = fx.user("viewer").await;
        let bob = fx.user("bob").await;
        let carol = fx.user("carol").await;

        let t_new = ago(1);
        let t_mid = ago(3);
        let t_old = ago(5);
        let bob_new = fx.file_touched_by(bob, "bob-new.txt", t_new).await;
        let bob_old = fx.file_touched_by(bob, "bob-old.txt", t_old).await;
        let carol_mid = fx.file_touched_by(carol, "carol-mid.txt", t_mid).await;
        for f in [bob_new, bob_old, carol_mid] {
            fx.grant_file(viewer, f).await;
        }

        let repo = fx.repo();
        let only = [bob, carol];
        let lanes = repo
            .lanes(viewer, DAYS, Some(&only), None, 10)
            .await
            .expect("lanes");
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes[0].user_id, bob, "Bob's lane is the more recent");
        assert_eq!(lanes[1].user_id, carol);

        let ids: Vec<Uuid> = lanes.iter().map(|l| l.user_id).collect();
        let all = repo
            .rows_for_lanes(viewer, DAYS, &ids, None, None, None, 10)
            .await
            .expect("rows");
        let order: Vec<Uuid> = all.iter().map(|r| r.file_id).collect();
        assert_eq!(
            order,
            vec![bob_new, bob_old, carol_mid],
            "lane-major: Bob's 5-hour-old file outranks Carol's 3-hour-old one"
        );

        // Page 1 of a limit-2 walk, then finish Bob's lane from the cursor.
        let page1 = repo
            .rows_for_lanes(viewer, DAYS, &ids, None, None, None, 2)
            .await
            .expect("page 1");
        assert_eq!(
            page1.iter().map(|r| r.file_id).collect::<Vec<_>>(),
            vec![bob_new, bob_old]
        );
        let rest_of_bob = repo
            .rows_for_lanes(viewer, DAYS, &[bob], None, Some(t_old), Some(bob_old), 10)
            .await
            .expect("rest of Bob's lane");
        assert!(
            rest_of_bob.is_empty(),
            "Bob's lane is exhausted — the keyset must not re-serve the row it ended on"
        );

        // Lanes after Bob's, by the lane keyset, then their rows.
        let next_lanes = repo
            .lanes(
                viewer,
                DAYS,
                Some(&only),
                Some((lanes[0].lane_latest, bob)),
                10,
            )
            .await
            .expect("lanes page 2");
        assert_eq!(next_lanes.len(), 1, "only Carol's lane is left");
        assert_eq!(next_lanes[0].user_id, carol);
        let page2 = repo
            .rows_for_lanes(viewer, DAYS, &[carol], None, None, None, 10)
            .await
            .expect("page 2");
        assert_eq!(
            page2.iter().map(|r| r.file_id).collect::<Vec<_>>(),
            vec![carol_mid],
            "each file exactly once across the walk"
        );

        // `after_rank` is 1-BASED, because `array_position` is. No caller
        // passes it today — `collect_lane_page` finishes the open lane with a
        // single-lane query instead — and this pin is what stops the first one
        // that does from skipping a lane with an off-by-one.
        let after_bob = repo
            .rows_for_lanes(viewer, DAYS, &ids, Some(1), Some(t_old), Some(bob_old), 10)
            .await
            .expect("rank-keyed page");
        assert_eq!(
            after_bob.iter().map(|r| r.file_id).collect::<Vec<_>>(),
            vec![carol_mid],
            "rank 1 is Bob, so the page resumes in Carol's lane"
        );
    }
}
