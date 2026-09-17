//! `user` subcommand domain — account administration that needs no
//! running server.
//!
//! One action today: `promote-to-owner`, the upgrade path for the server
//! ownership introduced in docs/plan/role-hierarchy-owner.md.
//!
//! **Why a CLI action and not an endpoint.** An install upgraded from
//! before the Owner role has no owner, and the endpoints that could create
//! one require being the owner already. That is deliberate rather than
//! circular: auto-promoting the earliest admin would guess, and on a
//! long-lived instance the guess is often wrong — the first admin may have
//! left the company. Naming the owner is an operator decision, and the
//! operator proves the right to make it by having database access.
//!
//! A *fresh* install never needs this: setup creates its first user as the
//! owner, since whoever runs setup is by definition standing the instance
//! up and there is exactly one candidate.

use std::env;

use clap::Subcommand;
use sqlx::{PgPool, Row};

#[derive(Subcommand)]
pub enum Action {
    /// Designate a user as the server owner — the account that other
    /// administrators cannot demote, deactivate, delete, or take over by
    /// resetting its password.
    ///
    /// For UPGRADED installs, which have no owner until someone names
    /// one. Until then the instance runs normally but refuses
    /// admin-of-admin operations.
    ///
    /// Refuses if an owner already exists: use the transfer-ownership
    /// endpoint (`POST /api/admin/transfer-ownership`) to move it, which
    /// demotes the outgoing owner in the same transaction. Two owners is
    /// not a state this command can create — the database will not hold
    /// it — so the check is for a clear message, not for safety.
    PromoteToOwner {
        /// Email OR username (dispatched on `@` presence, the same rule
        /// as `POST /api/auth/login`).
        user: String,

        /// Print what would change without touching the database.
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn run(action: Action) -> u8 {
    match action {
        Action::PromoteToOwner { user, dry_run } => run_promote_to_owner(user, dry_run).await,
    }
}

async fn run_promote_to_owner(identifier: String, dry_run: bool) -> u8 {
    let database_url = match env::var("DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("user promote-to-owner: DATABASE_URL not set");
            return 2;
        }
    };
    let pool = match PgPool::connect(&database_url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("user promote-to-owner: failed to connect to database: {e}");
            return 1;
        }
    };

    // Report an existing owner before looking the target up, so the
    // message names the actual obstacle rather than whatever the target
    // turns out to be.
    match sqlx::query("SELECT email FROM auth.users WHERE role = 'owner'::auth.userrole")
        .fetch_optional(&pool)
        .await
    {
        Ok(Some(row)) => {
            let email: String = row.get("email");
            eprintln!("user promote-to-owner: this instance already has an owner ({email}).");
            eprintln!(
                "  To move ownership, sign in as them and use \
                 POST /api/admin/transfer-ownership — it demotes the current \
                 owner and promotes the new one in one transaction."
            );
            return 2;
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("user promote-to-owner: failed to check for an existing owner: {e}");
            return 1;
        }
    }

    // Same `@`-presence dispatch as login. Normalised before binding so
    // the CLI accepts any case, matching how the accounts were stored.
    let normalized = identifier.trim().to_lowercase();
    let row = match sqlx::query(
        r#"
        SELECT id, email, username, role::text AS role_text, is_external, active
          FROM auth.users
         WHERE CASE WHEN $1 LIKE '%@%' THEN email = $1 ELSE username = $1 END
        "#,
    )
    .bind(&normalized)
    .fetch_optional(&pool)
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => {
            eprintln!("user promote-to-owner: no user matches {identifier:?}");
            return 2;
        }
        Err(e) => {
            eprintln!("user promote-to-owner: lookup failed: {e}");
            return 1;
        }
    };

    let id: uuid::Uuid = row.get("id");
    let email: String = row.get("email");
    let username: Option<String> = row.get("username");
    let role: String = row.get("role_text");
    let is_external: bool = row.get("is_external");
    let active: bool = row.get("active");

    // Each refusal below describes an account that could hold the role but
    // could not exercise it — ownership that cannot be used is worse than
    // no owner, because the grace state at least has a documented remedy.
    if is_external {
        eprintln!(
            "user promote-to-owner: {email} is an external (grant-only) account. \
             Federated identities cannot hold a privileged role — the \
             users_external_not_privileged constraint would reject this. \
             Promote them to internal first."
        );
        return 2;
    }
    if !active {
        eprintln!("user promote-to-owner: {email} is deactivated. Reactivate them first.");
        return 2;
    }
    if username.is_none() {
        eprintln!(
            "user promote-to-owner: {email} has no username, so they cannot complete \
             every sign-in flow. Set one first."
        );
        return 2;
    }

    println!("user promote-to-owner: {email} ({id})");
    println!("  role: {role} -> owner");

    if dry_run {
        println!("  (dry run — nothing written)");
        return 0;
    }

    match sqlx::query(
        "UPDATE auth.users SET role = 'owner'::auth.userrole, updated_at = NOW() WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    {
        Ok(result) if result.rows_affected() == 1 => {
            println!("  done.");
            // The running server caches role flags for up to 30s
            // (USER_FLAGS_CACHE_TTL) and this process cannot reach into
            // it, so say so rather than let the operator think the write
            // failed when the UI does not change instantly.
            println!(
                "  A running server may take up to 30s to observe this \
                 (its user-flags cache TTL)."
            );
            0
        }
        Ok(_) => {
            eprintln!("user promote-to-owner: the user disappeared mid-update; nothing changed");
            1
        }
        Err(e) => {
            eprintln!("user promote-to-owner: update failed: {e}");
            1
        }
    }
}
