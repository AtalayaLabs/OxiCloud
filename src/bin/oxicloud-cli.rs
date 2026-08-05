//! `oxicloud-cli` — operator toolbox for the OxiCloud deployment.
//!
//! Single binary with subcommand tree, shipped alongside the `oxicloud`
//! server binary. Replaces the per-task one-off bins (previously
//! `opaque-setup`, and any future `opaque-reset` etc.) with a
//! discoverable `--help`-driven surface so the container ships one
//! toolbox binary rather than N one-off ones.
//!
//! ## Layout
//!
//! ```text
//! oxicloud-cli <domain> <action> [flags]
//!
//! Domains:
//!   opaque    OPAQUE aPAKE substrate management
//!               setup    Print a fresh ServerSetup value for OXICLOUD_AUTH_OPAQUE_SERVER_SETUP
//!               reset    Clear envelope(s) so silent-migration re-mints under current KSF
//! ```
//!
//! Growth pattern: each new domain gets its own module below (e.g.
//! `mod opaque`) with a `#[derive(Subcommand)]` enum for its actions
//! and a `run(args) -> ExitCode` entrypoint. Keep each module
//! self-contained so a future extraction is a file move.
//!
//! ## Environment
//!
//! * `DATABASE_URL` — required by any subcommand that talks to the DB
//!   (`opaque reset`); not needed for pure primitive helpers
//!   (`opaque setup`). Each subcommand documents its own dependencies.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "oxicloud-cli",
    version,
    about = "OxiCloud operator toolbox",
    long_about = "OxiCloud operator toolbox — subcommand entrypoint for operational \
                  tasks that don't belong in the main server binary."
)]
struct Cli {
    #[command(subcommand)]
    domain: Domain,
}

#[derive(Subcommand)]
enum Domain {
    /// OPAQUE aPAKE substrate management (setup, reset).
    Opaque {
        #[command(subcommand)]
        action: opaque::Action,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.domain {
        Domain::Opaque { action } => opaque::run(action).await,
    }
}

// ── opaque domain ──────────────────────────────────────────────────────

mod opaque {
    use std::env;
    use std::process::ExitCode;

    use clap::Subcommand;
    use oxicloud::infrastructure::services::opaque_service::OpaqueService;
    use sqlx::{PgPool, Row};

    #[derive(Subcommand)]
    pub enum Action {
        /// Generate a fresh OPAQUE ServerSetup and print its base64
        /// encoding to stdout. Guidance goes to stderr so shell
        /// pipelines capture cleanly.
        ///
        /// Run ONCE per deployment; persist the printed value as
        /// `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP`. Rotating this value
        /// invalidates every user's OPAQUE registration — treat it
        /// like your JWT secret.
        Setup,

        /// Clear the OPAQUE envelope for one user or all users
        /// WITHOUT touching password or setting force_password_change.
        ///
        /// Use case: KSF rotation. If you change
        /// OXICLOUD_AUTH_OPAQUE_KSF_* values, existing envelopes
        /// become cryptographically incompatible with the newly
        /// published KSF — logins fail with InvalidCredentials.
        /// Nulling the envelope columns forces the SPA's `/lookup`
        /// to report `hasOpaque: false`, which routes the next login
        /// through legacy `/api/auth/login`; silent-migration then
        /// mints a fresh envelope under the CURRENT KSF. Passwords
        /// are unchanged.
        ///
        /// NOT for forgotten-passphrase recovery — use the admin
        /// password-reset endpoint (`PUT /api/admin/users/{id}/password`)
        /// which sets a temp password + force_change flag in one shot.
        Reset {
            /// Email OR username to reset (dispatched on `@` presence,
            /// same rule as `POST /api/auth/login`).
            #[arg(long, conflicts_with = "all")]
            user: Option<String>,

            /// Reset every user with an OPAQUE envelope.
            #[arg(long, conflicts_with = "user")]
            all: bool,

            /// Print what would change without touching the DB.
            #[arg(long)]
            dry_run: bool,
        },
    }

    pub async fn run(action: Action) -> ExitCode {
        match action {
            Action::Setup => run_setup(),
            Action::Reset {
                user,
                all,
                dry_run,
            } => run_reset(user, all, dry_run).await,
        }
    }

    fn run_setup() -> ExitCode {
        // Match the legacy `opaque-setup` bin's contract:
        //   - value on stdout, no trailing commentary (pipeline-safe)
        //   - guidance on stderr
        let b64 = OpaqueService::generate_server_setup_b64();
        println!("{b64}");
        eprintln!();
        eprintln!("=== OPAQUE server setup generated. ===");
        eprintln!("Persist the line above in OXICLOUD_AUTH_OPAQUE_SERVER_SETUP.");
        eprintln!("NEVER rotate: rotating invalidates every user's registration.");
        eprintln!("Treat this value like your JWT secret.");
        ExitCode::from(0)
    }

    async fn run_reset(user: Option<String>, all: bool, dry_run: bool) -> ExitCode {
        // clap enforces `conflicts_with`, but not "at least one of".
        // Belt-and-braces check here so the failure is explicit.
        if user.is_none() && !all {
            eprintln!("opaque reset: pass either --user <id> or --all");
            return ExitCode::from(2);
        }

        let database_url = match env::var("DATABASE_URL") {
            Ok(v) => v,
            Err(_) => {
                eprintln!("opaque reset: DATABASE_URL not set");
                return ExitCode::from(2);
            }
        };
        let pool = match PgPool::connect(&database_url).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("opaque reset: failed to connect to database: {e}");
                return ExitCode::from(1);
            }
        };

        // Preview the affected row set before writing. Doubles as
        // dry-run output and as diagnostics when --user matches nothing.
        // Envelope-presence bool lets the operator see which rows had
        // an envelope vs which only carry a stale migration mark.
        let select_sql = if all {
            r#"
            SELECT id, email, (opaque_envelope IS NOT NULL) AS had_envelope
              FROM auth.users
             WHERE opaque_envelope IS NOT NULL
                OR opaque_migrated_at IS NOT NULL
             ORDER BY email
            "#
        } else {
            r#"
            SELECT id, email, (opaque_envelope IS NOT NULL) AS had_envelope
              FROM auth.users
             WHERE CASE WHEN $1 LIKE '%@%' THEN email = $1 ELSE username = $1 END
            "#
        };
        let rows_result = if all {
            sqlx::query(select_sql).fetch_all(&pool).await
        } else {
            let ident = user.as_deref().unwrap();
            sqlx::query(select_sql).bind(ident).fetch_all(&pool).await
        };
        let rows = match rows_result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("opaque reset: query failed: {e}");
                return ExitCode::from(1);
            }
        };
        if rows.is_empty() {
            if all {
                println!("opaque reset: no users have an OPAQUE envelope — nothing to do.");
                return ExitCode::from(0);
            } else {
                eprintln!(
                    "opaque reset: no user matches --user {} — nothing changed.",
                    user.as_deref().unwrap_or("")
                );
                return ExitCode::from(1);
            }
        }

        println!(
            "opaque reset ({}): {} row(s) to affect",
            if dry_run {
                "DRY RUN — no writes"
            } else {
                "EXECUTING"
            },
            rows.len()
        );
        for row in &rows {
            let id: uuid::Uuid = row.get("id");
            let email: String = row.get("email");
            let had_envelope: bool = row.get("had_envelope");
            println!(
                "  {}  {}  {}",
                id,
                email,
                if had_envelope {
                    "had-envelope"
                } else {
                    "no-envelope-had-migrated-mark"
                }
            );
        }
        if dry_run {
            return ExitCode::from(0);
        }

        // Actual UPDATE. Kept identical in shape to the SELECT above so
        // the planner sees the same query pattern for both. We
        // DELIBERATELY do NOT touch password_hash or
        // force_password_change_at_next_login — this tool is scoped
        // to "the passwords are fine, the envelopes are stale."
        let update_sql_all = r#"
            UPDATE auth.users
               SET opaque_envelope            = NULL,
                   opaque_ciphersuite_version = NULL,
                   opaque_registered_at       = NULL,
                   opaque_migrated_at         = NULL
             WHERE opaque_envelope IS NOT NULL
                OR opaque_migrated_at IS NOT NULL
        "#;
        let update_sql_one = r#"
            UPDATE auth.users
               SET opaque_envelope            = NULL,
                   opaque_ciphersuite_version = NULL,
                   opaque_registered_at       = NULL,
                   opaque_migrated_at         = NULL
             WHERE CASE WHEN $1 LIKE '%@%' THEN email = $1 ELSE username = $1 END
        "#;
        let write_result = if all {
            sqlx::query(update_sql_all).execute(&pool).await
        } else {
            let ident = user.as_deref().unwrap();
            sqlx::query(update_sql_one).bind(ident).execute(&pool).await
        };
        let affected = match write_result {
            Ok(r) => r.rows_affected(),
            Err(e) => {
                eprintln!("opaque reset: update failed: {e}");
                return ExitCode::from(1);
            }
        };
        println!(
            "opaque reset: cleared envelope columns on {affected} row(s). \
             Users log in with their existing password; silent-migration \
             re-mints envelopes under the current KSF on next login."
        );
        ExitCode::from(0)
    }
}
