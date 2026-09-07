//! `opaque` subcommand domain — OPAQUE aPAKE substrate management.
//!
//! Two actions today:
//! * `setup` — mint a fresh ServerSetup for
//!   `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP`. Deployment-time one-off.
//! * `reset` — clear envelope columns so silent-migration re-mints them
//!   under the current KSF. Used after KSF rotation.
//!
//! Previously lived in `src/bin/oxicloud-cli.rs::mod opaque` before the
//! v0.9.0 CLI/server merge — see docs/plan/bundled-binary.md § 1b.
//! Behaviour is identical; the only change is the invocation form
//! (`oxicloud opaque <action>` instead of `oxicloud-cli opaque <action>`).

use std::env;

use clap::Subcommand;
use sqlx::{PgPool, Row};

use crate::infrastructure::services::opaque_service::OpaqueService;

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

pub async fn run(action: Action) -> u8 {
    match action {
        Action::Setup => run_setup(),
        Action::Reset { user, all, dry_run } => run_reset(user, all, dry_run).await,
    }
}

fn run_setup() -> u8 {
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
    0
}

async fn run_reset(user: Option<String>, all: bool, dry_run: bool) -> u8 {
    // clap enforces `conflicts_with`, but not "at least one of".
    // Belt-and-braces check here so the failure is explicit.
    if user.is_none() && !all {
        eprintln!("opaque reset: pass either --user <id> or --all");
        return 2;
    }

    let database_url = match env::var("DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("opaque reset: DATABASE_URL not set");
            return 2;
        }
    };
    let pool = match PgPool::connect(&database_url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("opaque reset: failed to connect to database: {e}");
            return 1;
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
            return 1;
        }
    };
    if rows.is_empty() {
        if all {
            println!("opaque reset: no users have an OPAQUE envelope — nothing to do.");
            return 0;
        } else {
            eprintln!(
                "opaque reset: no user matches --user {} — nothing changed.",
                user.as_deref().unwrap_or("")
            );
            return 1;
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
        return 0;
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
            return 1;
        }
    };
    println!(
        "opaque reset: cleared envelope columns on {affected} row(s). \
         Users log in with their existing password; silent-migration \
         re-mints envelopes under the current KSF on next login."
    );
    0
}
