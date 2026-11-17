//! Postgres implementation of `SubjectGroupRepository`.
//!
//! Two queries are non-trivial and deserve a read pass:
//!   - **Cycle check** (write-time, inside `add_member` when adding a
//!     group-member): walks child-edges from the candidate; if the parent
//!     appears in the descendants, reject.
//!   - **Transitive expansion** (`groups_for_user`): hot path on every
//!     authz cache miss; walks parent-edges from the user's direct
//!     memberships upward through nested groups.
//!
//! Depth-cap (`MAX_GROUP_DEPTH = 8`) is enforced at write time inside the
//! same transaction as the membership insert.
//!
//! See `migrations/20260612000000_subject_groups.sql` for the schema.

use std::collections::HashSet;
use std::sync::Arc;

use sqlx::{PgPool, Row, types::Uuid};

use super::like_escape;
use crate::domain::entities::subject_group::{GroupMember, MAX_GROUP_DEPTH, SubjectGroup};
use crate::domain::repositories::subject_group_repository::{
    SubjectGroupRepository, SubjectGroupRepositoryError,
};

pub struct SubjectGroupPgRepository {
    pool: Arc<PgPool>,
}

impl SubjectGroupPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    fn map_sqlx_err(context: &'static str, e: sqlx::Error) -> SubjectGroupRepositoryError {
        // Recognise common Postgres errors and translate to typed variants.
        if let sqlx::Error::Database(ref dberr) = e
            && let Some(code) = dberr.code()
        {
            match code.as_ref() {
                // unique_violation — name collision (or duplicate member, but
                // the caller already handles that case via UNIQUE indexes
                // returning the same code).
                "23505" => {
                    return SubjectGroupRepositoryError::NameAlreadyExists(dberr.to_string());
                }
                // check_violation — RFC 5321 regex CHECK failed.
                "23514" => return SubjectGroupRepositoryError::InvalidName(dberr.to_string()),
                _ => {}
            }
        }
        SubjectGroupRepositoryError::StorageError(format!("{}: {}", context, e))
    }

    fn row_to_group(row: &sqlx::postgres::PgRow) -> SubjectGroup {
        SubjectGroup {
            id: row.get::<Uuid, _>("id"),
            name: row.get::<String, _>("name"),
            description: row.get::<Option<String>, _>("description"),
            is_virtual: row.get::<bool, _>("is_virtual"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }
    }
}

impl SubjectGroupRepository for SubjectGroupPgRepository {
    async fn create(
        &self,
        group: &SubjectGroup,
    ) -> Result<SubjectGroup, SubjectGroupRepositoryError> {
        let row = sqlx::query(
            "INSERT INTO auth.subject_groups (id, name, description, is_virtual, created_at, updated_at)
             VALUES ($1, $2, $3, false, $4, $5)
             RETURNING id, name, description, is_virtual, created_at, updated_at",
        )
        .bind(group.id)
        .bind(&group.name)
        .bind(&group.description)
        .bind(group.created_at)
        .bind(group.updated_at)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("create subject_group", e))?;

        Ok(Self::row_to_group(&row))
    }

    async fn get_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<SubjectGroup>, SubjectGroupRepositoryError> {
        let row = sqlx::query(
            "SELECT id, name, description, is_virtual, created_at, updated_at
             FROM auth.subject_groups WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("get_by_id", e))?;

        Ok(row.as_ref().map(Self::row_to_group))
    }

    async fn get_by_name(
        &self,
        name: &str,
    ) -> Result<Option<SubjectGroup>, SubjectGroupRepositoryError> {
        // CITEXT matches case-insensitively — no need for LOWER() here.
        let row = sqlx::query(
            "SELECT id, name, description, is_virtual, created_at, updated_at
             FROM auth.subject_groups WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("get_by_name", e))?;

        Ok(row.as_ref().map(Self::row_to_group))
    }

    async fn list(
        &self,
        limit: u32,
        offset: u32,
        name_query: Option<&str>,
    ) -> Result<(Vec<SubjectGroup>, u64), SubjectGroupRepositoryError> {
        // Single query: the page plus `COUNT(*) OVER()` for the total matching
        // count, folding what used to be a separate COUNT round-trip into one.
        let (sql_page, pattern) = match name_query {
            Some(q) => (
                "SELECT id, name, description, is_virtual, created_at, updated_at,
                        COUNT(*) OVER() AS total_count
                 FROM auth.subject_groups
                 WHERE name ILIKE $1
                 ORDER BY is_virtual DESC, name
                 LIMIT $2 OFFSET $3"
                    .to_string(),
                Some(like_escape(q)),
            ),
            None => (
                "SELECT id, name, description, is_virtual, created_at, updated_at,
                        COUNT(*) OVER() AS total_count
                 FROM auth.subject_groups
                 ORDER BY is_virtual DESC, name
                 LIMIT $1 OFFSET $2"
                    .to_string(),
                None,
            ),
        };

        let rows = if let Some(ref p) = pattern {
            sqlx::query(&sql_page)
                .bind(p)
                .bind(limit as i64)
                .bind(offset as i64)
                .fetch_all(self.pool.as_ref())
                .await
        } else {
            sqlx::query(&sql_page)
                .bind(limit as i64)
                .bind(offset as i64)
                .fetch_all(self.pool.as_ref())
                .await
        }
        .map_err(|e| Self::map_sqlx_err("list page", e))?;

        // total_count is identical in every row; 0 when the page is empty.
        let total = rows.first().map_or(0, |r| r.get::<i64, _>("total_count")) as u64;
        Ok((rows.iter().map(Self::row_to_group).collect(), total))
    }

    async fn list_with_counts(
        &self,
        limit: u32,
        offset: u32,
        name_query: Option<&str>,
    ) -> Result<(Vec<(SubjectGroup, i64)>, u64), SubjectGroupRepositoryError> {
        // Single SQL: groups + per-group member COUNT via LEFT JOIN on
        // `auth.subject_group_members`, plus `COUNT(*) OVER()` for the total
        // group count. No N+1 and no separate COUNT round-trip — one query.
        let (sql_page, pattern) = match name_query {
            Some(q) => (
                "SELECT g.id, g.name, g.description, g.is_virtual,
                        g.created_at, g.updated_at,
                        COUNT(m.group_id) AS member_count,
                        COUNT(*) OVER() AS total_count
                 FROM auth.subject_groups g
                 LEFT JOIN auth.subject_group_members m ON m.group_id = g.id
                 WHERE g.name ILIKE $1
                 GROUP BY g.id
                 ORDER BY g.is_virtual DESC, g.name
                 LIMIT $2 OFFSET $3"
                    .to_string(),
                Some(like_escape(q)),
            ),
            None => (
                "SELECT g.id, g.name, g.description, g.is_virtual,
                        g.created_at, g.updated_at,
                        COUNT(m.group_id) AS member_count,
                        COUNT(*) OVER() AS total_count
                 FROM auth.subject_groups g
                 LEFT JOIN auth.subject_group_members m ON m.group_id = g.id
                 GROUP BY g.id
                 ORDER BY g.is_virtual DESC, g.name
                 LIMIT $1 OFFSET $2"
                    .to_string(),
                None,
            ),
        };

        let rows = if let Some(ref p) = pattern {
            sqlx::query(&sql_page)
                .bind(p)
                .bind(limit as i64)
                .bind(offset as i64)
                .fetch_all(self.pool.as_ref())
                .await
        } else {
            sqlx::query(&sql_page)
                .bind(limit as i64)
                .bind(offset as i64)
                .fetch_all(self.pool.as_ref())
                .await
        }
        .map_err(|e| Self::map_sqlx_err("list_with_counts page", e))?;

        // total_count is identical in every row; 0 when the page is empty.
        let total = rows.first().map_or(0, |r| r.get::<i64, _>("total_count")) as u64;
        let items = rows
            .iter()
            .map(|r| (Self::row_to_group(r), r.get::<i64, _>("member_count")))
            .collect();

        Ok((items, total))
    }

    async fn count_members(&self, id: Uuid) -> Result<i64, SubjectGroupRepositoryError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth.subject_group_members WHERE group_id = $1",
        )
        .bind(id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("count_members", e))?;
        Ok(count)
    }

    async fn rename(
        &self,
        id: Uuid,
        new_name: &str,
    ) -> Result<SubjectGroup, SubjectGroupRepositoryError> {
        let row = sqlx::query(
            "UPDATE auth.subject_groups
             SET name = $2, updated_at = now()
             WHERE id = $1
             RETURNING id, name, description, is_virtual, created_at, updated_at",
        )
        .bind(id)
        .bind(new_name)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("rename", e))?;

        match row {
            Some(r) => Ok(Self::row_to_group(&r)),
            None => Err(SubjectGroupRepositoryError::NotFound(id.to_string())),
        }
    }

    async fn delete(&self, id: Uuid) -> Result<(), SubjectGroupRepositoryError> {
        // The application service is responsible for clearing related
        // `storage.role_grants` rows in the same transaction (there's no
        // FK between role_grants and subject_groups). The subject_group_members
        // rows cascade automatically via FK.
        let result = sqlx::query("DELETE FROM auth.subject_groups WHERE id = $1")
            .bind(id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| Self::map_sqlx_err("delete", e))?;

        if result.rows_affected() == 0 {
            return Err(SubjectGroupRepositoryError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn add_member(
        &self,
        group_id: Uuid,
        member: GroupMember,
        added_by: Uuid,
    ) -> Result<(), SubjectGroupRepositoryError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Self::map_sqlx_err("add_member: begin tx", e))?;

        // Lock the parent row to prevent racing concurrent adds from each
        // squeezing under the cycle/depth limits.
        let exists: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM auth.subject_groups WHERE id = $1 FOR UPDATE")
                .bind(group_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| Self::map_sqlx_err("add_member: lock parent", e))?;
        if exists.is_none() {
            return Err(SubjectGroupRepositoryError::NotFound(group_id.to_string()));
        }

        match member {
            GroupMember::User(user_id) => {
                // Plain insert. Unique index catches duplicates.
                let res = sqlx::query(
                    "INSERT INTO auth.subject_group_members
                       (group_id, member_user_id, added_by)
                     VALUES ($1, $2, $3)
                     ON CONFLICT DO NOTHING",
                )
                .bind(group_id)
                .bind(user_id)
                .bind(added_by)
                .execute(&mut *tx)
                .await
                .map_err(|e| Self::map_sqlx_err("add_member: insert user", e))?;

                if res.rows_affected() == 0 {
                    return Err(SubjectGroupRepositoryError::MemberAlreadyPresent);
                }
            }
            GroupMember::Group(member_group_id) => {
                if member_group_id == group_id {
                    return Err(SubjectGroupRepositoryError::Cycle(
                        "group cannot contain itself".to_string(),
                    ));
                }

                // ── Cycle check ─────────────────────────────────────────
                // Adding member_group_id=$child to group_id=$parent creates
                // a cycle iff $parent is reachable by walking child-edges
                // from $child. Use a bounded recursion (UNION de-dups).
                let cycle: Option<(i32,)> = sqlx::query_as(
                    "WITH RECURSIVE descendants AS (
                         SELECT member_group_id AS g
                           FROM auth.subject_group_members
                          WHERE group_id = $1 AND member_group_id IS NOT NULL
                         UNION
                         SELECT m.member_group_id
                           FROM auth.subject_group_members m
                           JOIN descendants d ON m.group_id = d.g
                          WHERE m.member_group_id IS NOT NULL
                     )
                     SELECT 1 FROM descendants WHERE g = $2 LIMIT 1",
                )
                .bind(member_group_id)
                .bind(group_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| Self::map_sqlx_err("add_member: cycle check", e))?;
                if cycle.is_some() {
                    return Err(SubjectGroupRepositoryError::Cycle(format!(
                        "{} → {}",
                        group_id, member_group_id
                    )));
                }

                // ── Depth check ─────────────────────────────────────────
                // The longest path from $parent after the mutation =
                // max(longest path from existing descendants, 1 + longest
                // path under $child). Compute both with the same CTE,
                // pretending the new edge already exists.
                let depth: Option<(i32,)> = sqlx::query_as(
                    "WITH RECURSIVE path AS (
                         -- existing depth from this group downward
                         SELECT member_group_id AS g, 1 AS depth
                           FROM auth.subject_group_members
                          WHERE group_id = $1 AND member_group_id IS NOT NULL
                         UNION ALL
                         -- proposed new edge
                         SELECT $2::uuid AS g, 1 AS depth
                         UNION ALL
                         SELECT m.member_group_id, p.depth + 1
                           FROM auth.subject_group_members m
                           JOIN path p ON m.group_id = p.g
                          WHERE m.member_group_id IS NOT NULL
                     )
                     SELECT MAX(depth) FROM path",
                )
                .bind(group_id)
                .bind(member_group_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| Self::map_sqlx_err("add_member: depth check", e))?;

                let max_depth = depth.map(|d| d.0).unwrap_or(0);
                if (max_depth as u8) > MAX_GROUP_DEPTH {
                    return Err(SubjectGroupRepositoryError::DepthExceeded(format!(
                        "would reach depth {} (max {})",
                        max_depth, MAX_GROUP_DEPTH
                    )));
                }

                // ── Insert ──────────────────────────────────────────────
                let res = sqlx::query(
                    "INSERT INTO auth.subject_group_members
                       (group_id, member_group_id, added_by)
                     VALUES ($1, $2, $3)
                     ON CONFLICT DO NOTHING",
                )
                .bind(group_id)
                .bind(member_group_id)
                .bind(added_by)
                .execute(&mut *tx)
                .await
                .map_err(|e| Self::map_sqlx_err("add_member: insert group", e))?;

                if res.rows_affected() == 0 {
                    return Err(SubjectGroupRepositoryError::MemberAlreadyPresent);
                }
            }
        }

        tx.commit()
            .await
            .map_err(|e| Self::map_sqlx_err("add_member: commit", e))?;
        Ok(())
    }

    async fn remove_member(
        &self,
        group_id: Uuid,
        member: GroupMember,
    ) -> Result<(), SubjectGroupRepositoryError> {
        let res = match member {
            GroupMember::User(uid) => sqlx::query(
                "DELETE FROM auth.subject_group_members
                  WHERE group_id = $1 AND member_user_id = $2",
            )
            .bind(group_id)
            .bind(uid),
            GroupMember::Group(gid) => sqlx::query(
                "DELETE FROM auth.subject_group_members
                  WHERE group_id = $1 AND member_group_id = $2",
            )
            .bind(group_id)
            .bind(gid),
        }
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("remove_member", e))?;

        if res.rows_affected() == 0 {
            return Err(SubjectGroupRepositoryError::MemberNotPresent);
        }
        Ok(())
    }

    async fn list_direct_members(
        &self,
        group_id: Uuid,
    ) -> Result<Vec<GroupMember>, SubjectGroupRepositoryError> {
        let rows = sqlx::query(
            "SELECT member_user_id, member_group_id
               FROM auth.subject_group_members
              WHERE group_id = $1",
        )
        .bind(group_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("list_direct_members", e))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let user_id: Option<Uuid> = row.get("member_user_id");
            let group_id: Option<Uuid> = row.get("member_group_id");
            match (user_id, group_id) {
                (Some(uid), None) => out.push(GroupMember::User(uid)),
                (None, Some(gid)) => out.push(GroupMember::Group(gid)),
                _ => {
                    // XOR check at the schema level guarantees we never hit
                    // this branch — log defensively if we do.
                    tracing::warn!(
                        "subject_group_members row violates XOR invariant (user={:?}, group={:?})",
                        user_id,
                        group_id
                    );
                }
            }
        }
        Ok(out)
    }

    async fn list_transitive_users(
        &self,
        group_id: Uuid,
    ) -> Result<Vec<Uuid>, SubjectGroupRepositoryError> {
        // Walk child-edges from `group_id` to find every user transitively
        // a member. Used by debug / audit endpoints.
        let rows = sqlx::query(
            "WITH RECURSIVE descendants AS (
                 SELECT $1::uuid AS g
                 UNION
                 SELECT m.member_group_id
                   FROM auth.subject_group_members m
                   JOIN descendants d ON m.group_id = d.g
                  WHERE m.member_group_id IS NOT NULL
             )
             SELECT DISTINCT m.member_user_id AS user_id
               FROM auth.subject_group_members m
               JOIN descendants d ON m.group_id = d.g
              WHERE m.member_user_id IS NOT NULL",
        )
        .bind(group_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("list_transitive_users", e))?;

        Ok(rows.iter().map(|r| r.get::<Uuid, _>("user_id")).collect())
    }

    async fn groups_for_user(
        &self,
        user_id: Uuid,
    ) -> Result<HashSet<Uuid>, SubjectGroupRepositoryError> {
        // The hot path. PgAclEngine::expand_subject calls this on every
        // cache miss; result is memoised in the Moka cache for ~30s.
        let rows = sqlx::query(
            "WITH RECURSIVE user_groups AS (
                 SELECT group_id
                   FROM auth.subject_group_members
                  WHERE member_user_id = $1
                 UNION
                 SELECT m.group_id
                   FROM auth.subject_group_members m
                   JOIN user_groups ug ON m.member_group_id = ug.group_id
             )
             SELECT group_id FROM user_groups",
        )
        .bind(user_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| Self::map_sqlx_err("groups_for_user", e))?;

        Ok(rows.iter().map(|r| r.get::<Uuid, _>("group_id")).collect())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Integration tests — DB-dependent. Gated on `--features integration_tests` so
// they don't break the default `cargo test` run.
//
// How to run:
//   bash tests/common/spawn-db.sh                          # one-time
//   sqlx migrate run --database-url $TEST_DB               # if needed
//   cargo test --features integration_tests \
//       -p oxicloud --lib subject_group_pg_repository::integration_tests
//
// `TEST_DB` defaults to `postgres://oxicloud_test:oxicloud_test@localhost:5433/
// oxicloud_test` — the same DB used by `tests/api/run.sh`.
//
// Each test uses uniquely-suffixed group names (`rust-test-<uuid8>`) so
// concurrent runs and re-runs don't collide on the CITEXT unique constraint.
// Cleanup is by-id at the end of each test.
// ────────────────────────────────────────────────────────────────────────────
#[cfg(feature = "integration_tests")]
#[allow(dead_code)]
mod integration_tests {
    use super::*;
    use crate::integration_test_support::{ensure_clean_test_db, test_db_url};
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Arc<PgPool> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&test_db_url())
            .await
            .expect("connect to test DB — run tests/common/spawn-db.sh first");
        ensure_clean_test_db(&pool).await;
        Arc::new(pool)
    }

    async fn make_repo() -> SubjectGroupPgRepository {
        SubjectGroupPgRepository::new(test_pool().await)
    }

    /// Find any existing admin or create a throw-away one, so memberships'
    /// `added_by` FK is satisfied. Returns the admin's UUID.
    async fn ensure_admin(pool: &PgPool) -> Uuid {
        if let Ok(Some(row)) = sqlx::query("SELECT id FROM auth.users LIMIT 1")
            .fetch_optional(pool)
            .await
        {
            return row.get::<Uuid, _>("id");
        }
        // Fallback: build a minimal user. Schema permitting — if the test DB
        // is fresh, the operator should have run the server once to seed.
        panic!(
            "no rows in auth.users — start the server against the test DB \
             once (cargo run with DATABASE_URL=…) to seed the schema and \
             create the initial admin, then re-run."
        );
    }

    /// Unique name scoped to a single test invocation.
    fn rand_name(test: &str) -> String {
        let id = Uuid::new_v4();
        format!("rust-test-{}-{}", test, &id.to_string()[..8])
    }

    /// Idempotent cleanup of a group by id (cascades to members via FK).
    async fn drop_group(pool: &PgPool, id: Uuid) {
        let _ = sqlx::query("DELETE FROM auth.subject_groups WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await;
    }

    // ── 1. CITEXT unique enforcement ────────────────────────────────────────
    #[tokio::test]
    async fn test_group_name_unique_case_insensitive() {
        let repo = make_repo().await;
        let name_lower = rand_name("citext-lower");
        let name_upper = name_lower.to_uppercase();

        let g1 = SubjectGroup::new(&name_lower, None).expect("valid name");
        let created = repo.create(&g1).await.expect("first create succeeds");

        // Second create with same name in different case must collide.
        let g2 = SubjectGroup::new(&name_upper, None).expect("valid name shape");
        let err = repo
            .create(&g2)
            .await
            .expect_err("CITEXT must collide on different case");
        assert!(
            matches!(err, SubjectGroupRepositoryError::NameAlreadyExists(_)),
            "expected NameAlreadyExists, got {:?}",
            err
        );

        drop_group(repo.pool.as_ref(), created.id).await;
    }

    // ── 2. XOR constraint on members table ──────────────────────────────────
    #[tokio::test]
    async fn test_member_xor_check_at_db_level() {
        let repo = make_repo().await;
        let admin = ensure_admin(repo.pool.as_ref()).await;

        let g = SubjectGroup::new(&rand_name("xor"), None).unwrap();
        let group = repo.create(&g).await.unwrap();
        let some_uuid = Uuid::new_v4();

        // Both columns NULL → CHECK violation.
        let res = sqlx::query(
            "INSERT INTO auth.subject_group_members \
             (group_id, member_user_id, member_group_id, added_by) \
             VALUES ($1, NULL, NULL, $2)",
        )
        .bind(group.id)
        .bind(admin)
        .execute(repo.pool.as_ref())
        .await;
        assert!(res.is_err(), "both-null insert must fail XOR check");

        // Both columns set → CHECK violation.
        let res = sqlx::query(
            "INSERT INTO auth.subject_group_members \
             (group_id, member_user_id, member_group_id, added_by) \
             VALUES ($1, $2, $3, $2)",
        )
        .bind(group.id)
        .bind(admin)
        .bind(some_uuid)
        .execute(repo.pool.as_ref())
        .await;
        assert!(res.is_err(), "both-set insert must fail XOR check");

        drop_group(repo.pool.as_ref(), group.id).await;
    }

    // ── 3. Direct loop: A∋A rejected ────────────────────────────────────────
    #[tokio::test]
    async fn test_cycle_check_rejects_direct_loop() {
        let repo = make_repo().await;
        let admin = ensure_admin(repo.pool.as_ref()).await;

        let g = SubjectGroup::new(&rand_name("cycle-self"), None).unwrap();
        let group = repo.create(&g).await.unwrap();

        let err = repo
            .add_member(group.id, GroupMember::Group(group.id), admin)
            .await
            .expect_err("self-add must be rejected");
        // The `no_self` DB CHECK is the row-level guard for the degenerate
        // case; it surfaces here as a StorageError. Longer cycles take the
        // CTE/`Cycle` path. Accept either flavour.
        assert!(
            matches!(
                err,
                SubjectGroupRepositoryError::Cycle(_)
                    | SubjectGroupRepositoryError::StorageError(_)
            ),
            "expected cycle/storage rejection, got {:?}",
            err
        );

        drop_group(repo.pool.as_ref(), group.id).await;
    }

    // ── 4. Two-step loop: A∋B, B∋C, attempted C∋A rejected ──────────────────
    #[tokio::test]
    async fn test_cycle_check_rejects_two_step_loop() {
        let repo = make_repo().await;
        let admin = ensure_admin(repo.pool.as_ref()).await;

        let a = repo
            .create(&SubjectGroup::new(&rand_name("cyc2-a"), None).unwrap())
            .await
            .unwrap();
        let b = repo
            .create(&SubjectGroup::new(&rand_name("cyc2-b"), None).unwrap())
            .await
            .unwrap();
        let c = repo
            .create(&SubjectGroup::new(&rand_name("cyc2-c"), None).unwrap())
            .await
            .unwrap();

        repo.add_member(a.id, GroupMember::Group(b.id), admin)
            .await
            .unwrap();
        repo.add_member(b.id, GroupMember::Group(c.id), admin)
            .await
            .unwrap();

        // C∋A would close the loop A→B→C→A.
        let err = repo
            .add_member(c.id, GroupMember::Group(a.id), admin)
            .await
            .expect_err("two-step cycle must be rejected");
        assert!(matches!(err, SubjectGroupRepositoryError::Cycle(_)));

        for id in [c.id, b.id, a.id] {
            drop_group(repo.pool.as_ref(), id).await;
        }
    }

    // ── 5. Long-chain cycle: chain of 8 + closing edge rejected ─────────────
    #[tokio::test]
    async fn test_cycle_check_rejects_eight_step_loop() {
        let repo = make_repo().await;
        let admin = ensure_admin(repo.pool.as_ref()).await;

        let mut ids = Vec::with_capacity(8);
        for i in 0..8 {
            let g = repo
                .create(&SubjectGroup::new(&rand_name(&format!("cyc8-{i}")), None).unwrap())
                .await
                .unwrap();
            ids.push(g.id);
        }
        // Build the chain 0→1→2→…→7.
        for i in 0..7 {
            repo.add_member(ids[i], GroupMember::Group(ids[i + 1]), admin)
                .await
                .unwrap();
        }
        // Closing edge 7→0 should be rejected as a cycle.
        let err = repo
            .add_member(ids[7], GroupMember::Group(ids[0]), admin)
            .await
            .expect_err("eight-step cycle must be rejected");
        assert!(
            matches!(
                err,
                SubjectGroupRepositoryError::Cycle(_)
                    | SubjectGroupRepositoryError::DepthExceeded(_)
            ),
            "expected cycle/depth rejection, got {:?}",
            err
        );

        for id in ids.into_iter().rev() {
            drop_group(repo.pool.as_ref(), id).await;
        }
    }

    // ── 6. Depth cap at 8 ───────────────────────────────────────────────────
    //
    // Depth is enforced **per mutation, on the subtree under the parent being
    // mutated** — not as a global chain-length invariant. A new edge is
    // rejected when its proposed subtree under the parent would reach
    // depth > MAX_GROUP_DEPTH. Top-down chain construction can therefore grow
    // arbitrarily deep one edge at a time; the rejection fires when an
    // existing deep subtree is *lifted* under a new outer parent.
    //
    // This test pins that behaviour:
    //   1. Build a chain g[0] → g[1] → … → g[8] (9 nodes, 8 edges, max
    //      subtree depth 8 — exactly at the cap, still allowed).
    //   2. Create an outer group `h`.
    //   3. Attempt to add g[0] as a member of `h` — the subtree under `h`
    //      would now be 9 deep → DepthExceeded.
    #[tokio::test]
    async fn test_depth_cap_at_8() {
        let repo = make_repo().await;
        let admin = ensure_admin(repo.pool.as_ref()).await;

        let len = (MAX_GROUP_DEPTH as usize) + 1;
        let mut ids = Vec::with_capacity(len);
        for i in 0..len {
            let g = repo
                .create(&SubjectGroup::new(&rand_name(&format!("depth-{i}")), None).unwrap())
                .await
                .unwrap();
            ids.push(g.id);
        }
        // Build top-down: each insert only adds depth 1 under its parent, so
        // every edge is allowed by the per-mutation depth check.
        for i in 0..(len - 1) {
            repo.add_member(ids[i], GroupMember::Group(ids[i + 1]), admin)
                .await
                .unwrap_or_else(|e| panic!("edge {i} should fit in the depth budget: {:?}", e));
        }

        // Lift the whole chain under a new outer group → subtree depth 9.
        let outer = repo
            .create(&SubjectGroup::new(&rand_name("depth-outer"), None).unwrap())
            .await
            .unwrap();
        let err = repo
            .add_member(outer.id, GroupMember::Group(ids[0]), admin)
            .await
            .expect_err("depth-9 subtree must be rejected");
        assert!(
            matches!(err, SubjectGroupRepositoryError::DepthExceeded(_)),
            "expected DepthExceeded, got {:?}",
            err
        );

        drop_group(repo.pool.as_ref(), outer.id).await;
        for id in ids.into_iter().rev() {
            drop_group(repo.pool.as_ref(), id).await;
        }
    }

    // ── 7. Transitive expansion: A∋B, B∋C, U∈C → groups_for_user(U) ⊇ {A,B,C}
    #[tokio::test]
    async fn test_transitive_expansion_includes_indirect_groups() {
        let repo = make_repo().await;
        let admin = ensure_admin(repo.pool.as_ref()).await;

        let a = repo
            .create(&SubjectGroup::new(&rand_name("tx-a"), None).unwrap())
            .await
            .unwrap();
        let b = repo
            .create(&SubjectGroup::new(&rand_name("tx-b"), None).unwrap())
            .await
            .unwrap();
        let c = repo
            .create(&SubjectGroup::new(&rand_name("tx-c"), None).unwrap())
            .await
            .unwrap();

        repo.add_member(a.id, GroupMember::Group(b.id), admin)
            .await
            .unwrap();
        repo.add_member(b.id, GroupMember::Group(c.id), admin)
            .await
            .unwrap();
        repo.add_member(c.id, GroupMember::User(admin), admin)
            .await
            .unwrap();

        let expanded = repo.groups_for_user(admin).await.unwrap();
        assert!(expanded.contains(&a.id), "expansion must include outer A");
        assert!(expanded.contains(&b.id), "expansion must include middle B");
        assert!(
            expanded.contains(&c.id),
            "expansion must include direct parent C"
        );

        drop_group(repo.pool.as_ref(), c.id).await;
        drop_group(repo.pool.as_ref(), b.id).await;
        drop_group(repo.pool.as_ref(), a.id).await;
    }

    // ── 8. Internal virtual group is seeded with the well-known UUID ────────
    #[tokio::test]
    async fn test_internal_group_is_seeded() {
        use crate::domain::entities::subject_group::INTERNAL_GROUP_ID;

        let repo = make_repo().await;
        let row = repo
            .get_by_id(INTERNAL_GROUP_ID)
            .await
            .expect("query OK")
            .expect("Internal group row must exist");
        assert!(row.is_virtual, "Internal must be flagged virtual");
        assert_eq!(row.name, "Internal");
    }
}
