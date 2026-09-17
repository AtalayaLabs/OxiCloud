use crate::common::config::AppConfig;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

/// Database initialization error.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DbError(String);

type Result<T> = std::result::Result<T, DbError>;

/// Segmented database pools.
///
/// `primary` is used for all user-facing request paths (REST, WebDAV, CalDAV,
/// CardDAV).  `maintenance` is a smaller, isolated pool reserved for
/// background / batch operations (verify_integrity, garbage_collect,
/// update_all_users_storage_usage, trash cleanup) so they can never starve
/// interactive requests.
pub struct DbPools {
    /// Pool for user-facing request paths.
    pub primary: PgPool,
    /// Pool for background / batch maintenance tasks.
    pub maintenance: PgPool,
}

/// Connection string with the password replaced by `redacted` for
/// logging. The username stays visible — it is not a secret, and
/// telling apart "wrong user" from "wrong password" is exactly what
/// this log line is for; the marker still shows a password WAS set.
///
/// Parsed with the same `url` crate sqlx uses for `PgConnectOptions`,
/// so every accepted connection string round-trips here; anything the
/// parser rejects (e.g. a raw `/` in the password — libpq requires
/// %2F) is replaced wholesale, never echoed.
/// The previous version only PREPENDED a `[user]:[pass]@` marker after
/// the scheme, leaving the real credentials in the log line.
fn redact_db_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut parsed) => {
            if parsed.password().is_some() {
                // Cannot fail: a password was parsed, so the URL has an
                // authority.
                let _ = parsed.set_password(Some("redacted"));
            }
            parsed.to_string()
        }
        Err(_) => "[unparseable database URL]".to_string(),
    }
}

/// Create both the primary and maintenance database pools.
///
/// Pending migrations are applied via the primary pool on startup.
/// The maintenance pool shares the same connection string but has its
/// own, smaller budget.
pub async fn create_database_pools(config: &AppConfig) -> Result<DbPools> {
    tracing::info!(
        "Initializing PostgreSQL connections with URL: {}",
        redact_db_url(&config.database.connection_string)
    );

    // --- primary pool ---
    let primary = create_pool_with_retries(
        &config.database.connection_string,
        config.database.max_connections,
        config.database.min_connections,
        config.database.connect_timeout_secs,
        config.database.idle_timeout_secs,
        config.database.max_lifetime_secs,
        config.database.statement_timeout_secs,
        "primary",
    )
    .await?;

    // Run pending migrations (idempotent, tracked in _sqlx_migrations table)
    tracing::info!("Running database migrations...");
    if let Err(e) = run_migrations(&primary).await {
        return Err(DbError(format!(
            "Database migrations failed: {}. \
             Check the migrations/ directory for issues.",
            e
        )));
    }
    tracing::info!("Database migrations complete");

    // Username-lowercase verifier — three outcomes:
    //   * Clean          → nothing to do.
    //   * AutoRenamable  → non-colliding mixed-case rows exist; lowercase
    //                      them in one transaction and continue. Silent
    //                      action is bounded to the case where there is
    //                      exactly one correct move ([[feedback_no_silent_auto_repair]]
    //                      in spirit — ambiguity → refusal, unique fix → apply).
    //                      Each rename emits an audit line.
    //   * Collisions     → two or more active rows share a LOWER(username)
    //                      form (e.g. `Alice` + `alice`); tiebreak needs a
    //                      human, refuse to boot and print the CLI command.
    // See `common::username_migration::verify_all_usernames_lowercase`.
    use crate::common::username_migration::{
        UsernameCaseCheck, apply_auto_renames, format_refusal_message_collisions,
        verify_all_usernames_lowercase,
    };
    match verify_all_usernames_lowercase(&primary).await {
        Ok(UsernameCaseCheck::Clean) => {}
        Ok(UsernameCaseCheck::AutoRenamable(accounts)) => {
            let count = accounts.len();
            if let Err(e) = apply_auto_renames(&primary, &accounts).await {
                return Err(DbError(format!(
                    "username lowercase auto-rename failed at boot: {e}. \
                     Run `oxicloud migrate lowercase-usernames --dry-run` to \
                     inspect the current state, then apply manually."
                )));
            }
            tracing::info!(
                target: "audit",
                event = "user.usernames_lowercased_on_boot_summary",
                renamed = count,
                "auto-lowercased {count} non-colliding mixed-case username(s) at boot",
            );
        }
        Ok(UsernameCaseCheck::Collisions(groups)) => {
            return Err(DbError(format_refusal_message_collisions(&groups)));
        }
        Err(msg) => return Err(DbError(msg)),
    }

    // --- maintenance pool ---
    let maintenance = create_pool_with_retries(
        &config.database.connection_string,
        config.database.maintenance_max_connections,
        config.database.maintenance_min_connections,
        config.database.connect_timeout_secs,
        config.database.idle_timeout_secs,
        config.database.max_lifetime_secs,
        // Maintenance pool is exempt: integrity scans / GC may run long.
        0,
        "maintenance",
    )
    .await?;

    tracing::info!(
        "Database pools ready — primary: {} max / {} min, maintenance: {} max / {} min",
        config.database.max_connections,
        config.database.min_connections,
        config.database.maintenance_max_connections,
        config.database.maintenance_min_connections,
    );

    Ok(DbPools {
        primary,
        maintenance,
    })
}

/// Internal helper: create a single pool with retry logic.
#[allow(clippy::too_many_arguments)]
async fn create_pool_with_retries(
    connection_string: &str,
    max_connections: u32,
    min_connections: u32,
    connect_timeout_secs: u64,
    idle_timeout_secs: u64,
    max_lifetime_secs: u64,
    statement_timeout_secs: u64,
    label: &str,
) -> Result<PgPool> {
    let mut attempt = 0;
    const MAX_ATTEMPTS: usize = 5;

    while attempt < MAX_ATTEMPTS {
        attempt += 1;
        tracing::info!(
            "PostgreSQL {} pool connection attempt #{}/{}",
            label,
            attempt,
            MAX_ATTEMPTS
        );

        let mut opts = PgPoolOptions::new()
            .max_connections(max_connections)
            .min_connections(min_connections)
            .acquire_timeout(Duration::from_secs(connect_timeout_secs))
            .idle_timeout(Duration::from_secs(idle_timeout_secs))
            .max_lifetime(Duration::from_secs(max_lifetime_secs))
            // Skip the liveness ping sqlx issues on every acquire() (on by
            // default): with warm min_connections and a bounded max_lifetime,
            // that extra round-trip per checkout costs more than the rare dead
            // connection it catches. A stale socket surfaces as a query error
            // and the pool recycles it either way.
            .test_before_acquire(false);

        // Bound the worst-case query: `SET statement_timeout` on every new
        // connection caps how long any single statement may run, so a runaway
        // query can't pin a pool slot and starve interactive requests. `0`
        // disables it (maintenance pool). statement_timeout's integer value is
        // milliseconds.
        if statement_timeout_secs > 0 {
            use sqlx::Executor;
            let stmt_ms = statement_timeout_secs.saturating_mul(1000);
            opts = opts.after_connect(move |conn, _meta| {
                Box::pin(async move {
                    conn.execute(format!("SET statement_timeout = {stmt_ms}").as_str())
                        .await?;
                    Ok(())
                })
            });
        }

        match opts.connect(connection_string).await {
            Ok(pool) => match sqlx::query("SELECT 1").execute(&pool).await {
                Ok(_) => {
                    tracing::info!("PostgreSQL {} pool established successfully", label);
                    return Ok(pool);
                }
                Err(e) => {
                    tracing::error!("Error verifying {} pool connection: {}", label, e);
                    if attempt >= MAX_ATTEMPTS {
                        return Err(DbError(format!(
                            "Error verifying PostgreSQL {} pool connection: {}",
                            label, e
                        )));
                    }
                }
            },
            Err(e) => {
                tracing::error!(
                    "Error connecting to PostgreSQL {} pool (attempt {}/{}): {}",
                    label,
                    attempt,
                    MAX_ATTEMPTS,
                    e
                );
                if attempt >= MAX_ATTEMPTS {
                    return Err(DbError(format!(
                        "Error in PostgreSQL {} pool connection: {}",
                        label, e
                    )));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    Err(DbError(format!(
        "Could not establish PostgreSQL {} pool connection after {} attempts",
        label, MAX_ATTEMPTS
    )))
}

/// Run pending migrations from the `migrations/` directory.
///
/// Uses sqlx's built-in migration system which tracks applied migrations
/// in a `_sqlx_migrations` table. Each migration runs in its own transaction.
/// Migration files are embedded at compile time via `sqlx::migrate!()`.
async fn run_migrations(pool: &PgPool) -> Result<()> {
    // ── One-time pre-flight cleanup for the 20260625000000 collision ──
    //
    // Two migrations landed on the same day from parallel branches with
    // the same version prefix:
    //   - 20260625000000_files_user_size_index.sql    (Dio)
    //   - 20260625000000_folder_tree_modified_at.sql  (Ed)
    // They were renamed to ...0001 and ...0002 (disjoint versions), and
    // both bodies were made idempotent so they re-run safely against
    // databases that already applied either original under the shared
    // version. However sqlx 0.8's default strict mode errors on boot
    // when `_sqlx_migrations` contains a row whose version no longer
    // maps to a source file ("previously applied but is missing in the
    // resolved migrations") — which is exactly the state of every
    // contributor DB that booted before the rename.
    //
    // This DELETE silently clears that stale bookkeeping row. The
    // schema effects of whichever original ran are preserved
    // (idempotent re-application via ...0001 / ...0002 is a no-op on
    // already-modified schemas). On fresh databases the table doesn't
    // exist yet, the query errors, and the `let _` swallows it —
    // sqlx::migrate!() then creates the table cleanly on its first
    // pass.
    //
    // Sunset: drop this block once the contributor base has rolled
    // past the affected commit window. Suggested review date 2026-12.
    let _ = sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 20260625000000")
        .execute(pool)
        .await;

    match sqlx::migrate!().run(pool).await {
        Ok(()) => Ok(()),
        Err(e) => Err(DbError(format_error_chain("Migration error", &e))),
    }
}

/// Format an error and every wrapped `source()` cause on a single line.
///
/// sqlx's `MigrateError::Execute` wraps the underlying `sqlx::Error::Database`
/// which in turn carries the PG `DETAIL` (e.g. `Key (version)=(20260803000000)`
/// for a duplicate-key on `_sqlx_migrations_pkey`). The default `Display`
/// only renders the outermost layer, so the operationally-critical hint
/// gets buried. Walking the chain surfaces it without needing to bump
/// `RUST_LOG` to debug.
fn format_error_chain(prefix: &str, e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = format!("{prefix}: {e}");
    let mut cur = e.source();
    while let Some(c) = cur {
        out.push_str(" -> ");
        out.push_str(&c.to_string());
        cur = c.source();
    }
    out
}

#[cfg(test)]
mod redact_tests {
    use super::redact_db_url;

    #[test]
    fn password_is_replaced_username_stays() {
        assert_eq!(
            redact_db_url("postgres://oxicloud:s3cr3t@127.0.0.1:5435/oxicloud"),
            "postgres://oxicloud:redacted@127.0.0.1:5435/oxicloud"
        );
    }

    #[test]
    fn username_without_password_is_unchanged() {
        assert_eq!(
            redact_db_url("postgres://oxicloud@127.0.0.1/db"),
            "postgres://oxicloud@127.0.0.1/db"
        );
    }

    #[test]
    fn percent_encoded_password_is_replaced() {
        assert_eq!(
            redact_db_url("postgres://u:p%2Fa%40ss@host/db"),
            "postgres://u:redacted@host/db"
        );
    }

    #[test]
    fn url_without_credentials_is_unchanged() {
        assert_eq!(
            redact_db_url("postgres://127.0.0.1:5435/oxicloud"),
            "postgres://127.0.0.1:5435/oxicloud"
        );
    }

    #[test]
    fn at_sign_in_query_does_not_confuse_redaction() {
        assert_eq!(
            redact_db_url("postgres://host/db?options=-c%20app=a@b"),
            "postgres://host/db?options=-c%20app=a@b"
        );
    }

    /// A raw `/` in the password is not a valid URI (libpq's connection
    /// URI spec requires %2F) and the parser — the same one sqlx uses —
    /// rejects it. A log redactor must NEVER echo input it could not
    /// parse, or the very string it failed on leaks.
    #[test]
    fn unparseable_input_never_echoes_the_value() {
        assert_eq!(
            redact_db_url("postgres://u:pa/ss@host/db"),
            "[unparseable database URL]"
        );
        assert_eq!(redact_db_url("not a url"), "[unparseable database URL]");
    }
}
