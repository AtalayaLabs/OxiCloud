//! Shared helpers for `#[cfg(feature = "integration_tests")]` test modules.
//!
//! Compiled only when the build is invoked with `--features integration_tests`.
//! The module exists so the OnceCell that guards pre-suite cleanup is
//! singleton across the whole lib test binary — without it, each test
//! file would have its own cell and module-B's first test could nuke
//! module-A's in-flight rows.

use sqlx::PgPool;

/// Substring the DATABASE_URL must contain for integration tests to
/// run. Both the local docker-compose-test database (`oxicloud_test`
/// on port 5433) and the GitHub Actions postgres service (`oxicloud_test`
/// on the default port) use this name — checking the substring is
/// portable across both. Port-based guards would break CI.
pub const TEST_DB_DISCRIMINATOR: &str = "oxicloud_test";

pub const DEFAULT_TEST_DB: &str =
    "postgres://oxicloud_test:oxicloud_test@localhost:5433/oxicloud_test";

/// Resolve the test DB URL, panicking if it doesn't recognisably point
/// at a test database. Without this guard, a `DATABASE_URL` in `.env`
/// (loaded by `set dotenv-load` in the justfile) would leak into the
/// test run and mutate the real dev DB.
pub fn test_db_url() -> String {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_TEST_DB.to_string());
    assert!(
        url.contains(TEST_DB_DISCRIMINATOR),
        "DATABASE_URL ({url}) does not point to a test database \
         (expected substring '{TEST_DB_DISCRIMINATOR}'). Refusing to \
         run integration tests — they would mutate the real DB. Run \
         via `just test-integration`, or unset DATABASE_URL to use \
         the default test pool at port 5433."
    );
    url
}

/// Once-per-process cleanup of stale test rows from prior runs.
///
/// Naming convention (`rust-test-<slug>-<uuid8>`) keeps this LIKE scan
/// safe — no real group can match. Runs synchronously inside the
/// OnceCell so concurrent test threads block until the first caller
/// finishes; subsequent calls are zero-cost.
///
/// Order matters: `storage.role_grants` rows go first because there's
/// no FK from there to `auth.subject_groups` (the service's `delete`
/// path does this transactionally; here we bypass the service).
static CLEANUP_ONCE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

pub async fn ensure_clean_test_db(pool: &PgPool) {
    CLEANUP_ONCE
        .get_or_init(|| async {
            let _ = sqlx::query(
                "DELETE FROM storage.role_grants
                  WHERE subject_type = 'group'
                    AND subject_id IN (
                        SELECT id FROM auth.subject_groups WHERE name LIKE 'rust-test-%'
                    )",
            )
            .execute(pool)
            .await;
            let _ = sqlx::query("DELETE FROM auth.subject_groups WHERE name LIKE 'rust-test-%'")
                .execute(pool)
                .await;
        })
        .await;
}
