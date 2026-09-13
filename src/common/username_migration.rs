//! Username-lowercase boot flow: verifier, auto-rename, shared collision helper.
//!
//! The plan (`docs/plan/username-lowercase.md`) makes usernames
//! case-insensitive by canonicalising to lowercase on ingest. Three
//! pieces of infrastructure live here:
//!
//! 1. [`verify_all_usernames_lowercase`] — a **read-only** check that
//!    runs after `sqlx::migrate!()` at boot. Classifies every active
//!    mixed-case row into one of three outcomes:
//!
//!    - [`UsernameCaseCheck::Clean`] — nothing to do.
//!    - [`UsernameCaseCheck::AutoRenamable`] — mixed-case rows exist
//!      but each `LOWER(username)` form is unique in the active-user
//!      set. Safe to lowercase in one atomic transaction; boot proceeds.
//!    - [`UsernameCaseCheck::Collisions`] — at least one group has
//!      two or more active rows sharing a `LOWER(username)` (e.g.
//!      `Alice` + `alice`). Tiebreak requires human judgement; the
//!      server refuses to start and prints the CLI command.
//!
//!    Follows [[feedback_no_silent_auto_repair]] in spirit: silent
//!    action is limited to cases where there is exactly one correct
//!    move (rename the sole mixed-case row to its lowercase form).
//!    Anywhere ambiguity exists (which of `Alice` and `alice` keeps
//!    the canonical name?), boot refuses and defers to `oxicloud
//!    migrate lowercase-usernames`.
//!
//! 2. [`apply_auto_renames`] — the one-transaction UPDATE loop that
//!    performs the auto-rename path. Emits a structured audit line
//!    per row (`user.username_lowercased_on_boot`). All-or-nothing:
//!    a mid-tx failure aborts the transaction and boot fails, so the
//!    DB is never left in a half-renamed state.
//!
//! 3. [`find_free_username_suffix`] — the shared collision-resolution
//!    helper. Called by the migration CLI when it lowercases a name
//!    that would clash with an existing row, AND by the un-soft-delete
//!    API when it re-normalises a mixed-case account whose lowercase
//!    form is now taken by someone else.
//!
//! `NULL` usernames (OPAQUE-migrated accounts) are always skipped — the
//! SQL `WHERE username <> LOWER(username)` predicate is NULL-safe by
//! semantics (`NULL <> anything` yields `NULL`, which `WHERE` excludes).
//! Soft-deleted / disabled accounts (`active = false`) are also skipped:
//! they can't serve traffic anyway.

use sqlx::{PgPool, Row};

/// One mixed-case account row. Used both for the auto-rename list and
/// for reporting collision-group members.
#[derive(Debug, Clone)]
pub struct MixedCaseAccount {
    pub id: uuid::Uuid,
    pub username: String,
    pub last_login_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A `LOWER(username)` group with two or more active members. At least
/// one member is mixed-case (that's what made the group visible to the
/// verifier); the other member(s) may be already-lowercase (e.g.
/// `Alice` + `alice`) or also mixed-case (`Alice` + `ALICE`).
#[derive(Debug, Clone)]
pub struct CollisionGroup {
    /// The lowercase form shared by every member.
    pub canonical: String,
    /// Members, ordered by the tiebreak that the migration CLI
    /// applies: `last_login_at DESC NULLS LAST, created_at ASC`.
    pub members: Vec<MixedCaseAccount>,
}

/// The three outcomes of the boot-time verifier.
#[derive(Debug, Clone)]
pub enum UsernameCaseCheck {
    /// Every active username is already lowercase (or `NULL`). Boot
    /// proceeds unmodified.
    Clean,
    /// Mixed-case rows exist, but each `LOWER(username)` form is
    /// unique among active users. Safe to lowercase atomically at
    /// boot; the caller runs [`apply_auto_renames`].
    AutoRenamable(Vec<MixedCaseAccount>),
    /// At least one `LOWER(username)` group has two or more active
    /// members. Tiebreak requires human judgement; the caller formats
    /// a refusal message via [`format_refusal_message_collisions`] and
    /// aborts boot.
    Collisions(Vec<CollisionGroup>),
}

/// Boot-time verifier. Runs AFTER `sqlx::migrate!()` and BEFORE
/// `AppState` is assembled. Read-only: never mutates `auth.users`.
///
/// Returns [`UsernameCaseCheck`] describing what (if anything) the
/// caller should do. Errors are limited to DB failures — semantic
/// outcomes are all `Ok(_)` variants.
pub async fn verify_all_usernames_lowercase(pool: &PgPool) -> Result<UsernameCaseCheck, String> {
    // First pass: mixed-case rows that have NO other active row
    // sharing their LOWER form. These are safe to auto-rename.
    let auto_rows = sqlx::query(
        r#"
        SELECT u.id, u.username, u.last_login_at
          FROM auth.users u
         WHERE u.active = true
           AND u.username <> LOWER(u.username)
           AND NOT EXISTS (
               SELECT 1
                 FROM auth.users u2
                WHERE u2.active = true
                  AND u2.id <> u.id
                  AND LOWER(u2.username) = LOWER(u.username)
           )
         ORDER BY LOWER(u.username)
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("username lowercase verifier: singleton query failed: {e}"))?;

    // Second pass: every active row that belongs to a colliding
    // group — a `LOWER(username)` shared by two or more active rows
    // where at least one member is mixed-case. Result includes
    // already-lowercase members so the refusal report shows the full
    // context of each collision.
    let collision_rows = sqlx::query(
        r#"
        WITH colliding_lowers AS (
            SELECT LOWER(username) AS canonical
              FROM auth.users
             WHERE active = true
             GROUP BY LOWER(username)
            HAVING COUNT(*) > 1
               AND SUM(CASE WHEN username <> LOWER(username) THEN 1 ELSE 0 END) >= 1
        )
        SELECT id, username, last_login_at, LOWER(username) AS canonical
          FROM auth.users
         WHERE active = true
           AND LOWER(username) IN (SELECT canonical FROM colliding_lowers)
         ORDER BY LOWER(username),
                  (last_login_at IS NULL),
                  last_login_at DESC NULLS LAST,
                  created_at ASC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("username lowercase verifier: collision query failed: {e}"))?;

    if !collision_rows.is_empty() {
        // Group by canonical. Rows are already ordered by canonical
        // then by tiebreak, so a fold is enough.
        let mut groups: Vec<CollisionGroup> = Vec::new();
        for r in collision_rows {
            let canonical: String = r.get("canonical");
            let member = MixedCaseAccount {
                id: r.get::<uuid::Uuid, _>("id"),
                username: r.get::<String, _>("username"),
                last_login_at: r
                    .try_get::<chrono::DateTime<chrono::Utc>, _>("last_login_at")
                    .ok(),
            };
            match groups.last_mut() {
                Some(g) if g.canonical == canonical => g.members.push(member),
                _ => groups.push(CollisionGroup {
                    canonical,
                    members: vec![member],
                }),
            }
        }
        return Ok(UsernameCaseCheck::Collisions(groups));
    }

    if auto_rows.is_empty() {
        return Ok(UsernameCaseCheck::Clean);
    }

    let accounts = auto_rows
        .into_iter()
        .map(|r| MixedCaseAccount {
            id: r.get::<uuid::Uuid, _>("id"),
            username: r.get::<String, _>("username"),
            last_login_at: r
                .try_get::<chrono::DateTime<chrono::Utc>, _>("last_login_at")
                .ok(),
        })
        .collect();
    Ok(UsernameCaseCheck::AutoRenamable(accounts))
}

/// Apply the atomic auto-rename transaction. All UPDATEs succeed
/// together or all roll back — the DB is never left in a half-renamed
/// state. Each successful rename emits a structured audit line.
///
/// The `WHERE id = $1 AND username = $3` guard defends against a
/// concurrent rename between the SELECT and this UPDATE. If some
/// other process renamed the row in that window, the UPDATE affects
/// zero rows and we log a warning but do not fail the transaction —
/// the row is already lowercase (that's why the guard didn't match),
/// so the invariant still holds.
pub async fn apply_auto_renames(
    pool: &PgPool,
    accounts: &[MixedCaseAccount],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for acc in accounts {
        let new_username = acc.username.to_ascii_lowercase();
        let res = sqlx::query(
            r#"
            UPDATE auth.users
               SET username = $2
             WHERE id = $1
               AND username = $3
            "#,
        )
        .bind(acc.id)
        .bind(&new_username)
        .bind(&acc.username)
        .execute(&mut *tx)
        .await?;

        if res.rows_affected() == 0 {
            tracing::warn!(
                target: "audit",
                event = "user.username_lowercase_skipped_on_boot",
                reason = "row_changed_between_verify_and_apply",
                user_id = %acc.id,
                expected_username = %acc.username,
                "👮🏻‍♂️ skipped auto-lowercase: row was modified after verifier ran",
            );
            continue;
        }

        tracing::info!(
            target: "audit",
            event = "user.username_lowercased_on_boot",
            reason = "unique_lowercase_group",
            user_id = %acc.id,
            old_username = %acc.username,
            new_username = %new_username,
            "👮🏻‍♂️ auto-lowercased username at boot",
        );
    }
    tx.commit().await?;
    Ok(())
}

/// Format the FATAL error string shown when boot refuses to proceed
/// because at least one `LOWER(username)` group has multiple active
/// members. Self-sufficient — an operator at 3 AM shouldn't need to
/// consult docs to know what to do.
pub fn format_refusal_message_collisions(groups: &[CollisionGroup]) -> String {
    use std::fmt::Write;
    let total_members: usize = groups.iter().map(|g| g.members.len()).sum();
    let mut out = String::new();
    let _ = write!(
        &mut out,
        "\nFATAL: cannot start — {} colliding username group(s) \
         ({} affected account(s) in total).\n\n\
         Non-colliding mixed-case rows are auto-renamed at boot. \
         These groups can't be resolved automatically because two or \
         more active accounts share the same lowercase form, and only \
         a human can decide who keeps the canonical name.\n\n\
         Run the migration:\n\n  \
             oxicloud migrate lowercase-usernames --dry-run     # preview the tiebreak\n  \
             oxicloud migrate lowercase-usernames               # apply\n\n\
         The tiebreak rule is `last_login_at DESC NULLS LAST, \
         created_at ASC` — the most recently active member keeps the \
         canonical lowercase name; the losers get `-2`, `-3`, … as a \
         suffix. Sessions and grants survive the rename (they key on \
         user_id, not username).\n\n\
         Collision groups (up to 10 shown):\n",
        groups.len(),
        total_members
    );
    for g in groups.iter().take(10) {
        let _ = writeln!(&mut out, "\n  Canonical form: {}", g.canonical);
        for m in &g.members {
            let last = m
                .last_login_at
                .map(|t| t.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "never".to_string());
            let _ = writeln!(
                &mut out,
                "    {}   (id: {}  last_login: {})",
                m.username, m.id, last
            );
        }
    }
    if groups.len() > 10 {
        let _ = writeln!(
            &mut out,
            "\n  ... and {} more group(s). Run --dry-run for the full list.",
            groups.len() - 10
        );
    }
    out
}

/// Cap on the suffix-probe loop. If we ever need `<base>-10000` there's
/// something very wrong with the account universe — collisions in the
/// wild are 2-3 accounts, not 10 K. The loud abort IS the detection.
/// See [`docs/plan/username-lowercase.md § 3. Suffix-collision robustness`].
const SUFFIX_PROBE_CAP: i32 = 10_000;

/// Find the next free `<base>-<N>` suffix for a colliding username.
///
/// Starts at `<base>-2` and increments until an unused suffix is
/// found. Robust against pre-existing rows already occupying some
/// suffixes (the probe steps past them).
///
/// Called by:
/// - The migration CLI when a `LOWER(username)` group has multiple
///   members and the tiebreak winner keeps the canonical name; the
///   losers get `<base>-2`, `-3`, … from this helper.
/// - The un-soft-delete API when re-normalising a mixed-case
///   account whose lowercase form is now taken by an active row.
///
/// Both callers reach for this single function so the two paths
/// agree by construction — no drift risk between the migration and
/// runtime un-soft-delete.
pub async fn find_free_username_suffix(pool: &PgPool, base: &str) -> Result<String, sqlx::Error> {
    for n in 2..=SUFFIX_PROBE_CAP {
        let candidate = format!("{base}-{n}");
        let exists: (bool,) =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM auth.users WHERE username = $1)")
                .bind(&candidate)
                .fetch_one(pool)
                .await?;
        if !exists.0 {
            return Ok(candidate);
        }
    }
    // If we get here, something is very wrong. Loud panic beats
    // silent truncation to whatever the caller's fallback is.
    panic!(
        "find_free_username_suffix: exhausted {SUFFIX_PROBE_CAP} suffix probes for base '{base}'; \
         the account universe likely has an anomaly worth investigating"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(name: &str) -> MixedCaseAccount {
        MixedCaseAccount {
            id: uuid::Uuid::nil(),
            username: name.into(),
            last_login_at: None,
        }
    }

    #[test]
    fn refusal_message_lists_groups_and_cli() {
        let groups = vec![CollisionGroup {
            canonical: "alice".into(),
            members: vec![acc("Alice"), acc("alice")],
        }];
        let msg = format_refusal_message_collisions(&groups);
        assert!(msg.contains("1 colliding username group(s)"));
        assert!(msg.contains("2 affected account(s)"));
        assert!(msg.contains("oxicloud migrate lowercase-usernames"));
        assert!(msg.contains("Canonical form: alice"));
        assert!(msg.contains("Alice"));
        assert!(msg.contains("last_login: never"));
    }

    #[test]
    fn refusal_message_caps_group_display_and_notes_overflow() {
        let groups: Vec<_> = (0..15)
            .map(|i| CollisionGroup {
                canonical: format!("user{i:02}"),
                members: vec![acc(&format!("User{i:02}")), acc(&format!("user{i:02}"))],
            })
            .collect();
        let msg = format_refusal_message_collisions(&groups);
        // First 10 groups shown by canonical name.
        assert!(msg.contains("Canonical form: user00"));
        assert!(msg.contains("Canonical form: user09"));
        // Overflow tail names how many are hidden.
        assert!(msg.contains("and 5 more group(s)"));
    }

    #[test]
    fn refusal_message_reports_total_across_all_groups() {
        // Two groups with different sizes — 2 + 3 = 5 members total.
        let groups = vec![
            CollisionGroup {
                canonical: "alice".into(),
                members: vec![acc("Alice"), acc("alice")],
            },
            CollisionGroup {
                canonical: "bob".into(),
                members: vec![acc("Bob"), acc("BOB"), acc("bob")],
            },
        ];
        let msg = format_refusal_message_collisions(&groups);
        assert!(msg.contains("2 colliding username group(s)"));
        assert!(msg.contains("5 affected account(s)"));
    }
}
