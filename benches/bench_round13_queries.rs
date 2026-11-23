//! Round-13 query-shape pack (needs the dev Postgres up; reads DATABASE_URL
//! from `.env`).
//!
//! Three sections, each BEFORE (verbatim replica of the shipped query shape)
//! vs AFTER (proposed shape), with equivalence/safety gates:
//!
//!   [Q1] Group-notification recipient expansion — `get_users_by_ids`'s
//!        21-column row (incl. the ≤512 KiB avatar `image` + `ui_preferences`
//!        JSONB) hydrated per member vs the notification-only projection
//!        (drops both heavy columns; the caller reads only email/eligibility
//!        fields).
//!   [Q2] Login provisioning idempotency — `list_calendars_by_owner(..)
//!        .is_empty()` / `get_address_books_by_owner(..).is_empty()` (hydrate
//!        every owned row) vs `SELECT EXISTS(...)`.
//!   [Q3] Recent-access recording — unconditional upsert + prune (2
//!        round-trips) vs upsert-`RETURNING (xmax=0)` + prune-only-on-insert.
//!
//! Run:
//!   cargo bench --features=bench --bench bench_round13_queries
//! Tunables (env): BENCH_PASSES (200), BENCH_GROUP (30), BENCH_CALS (4),
//!   BENCH_RECENT_CAP (50)

use std::env;
use std::sync::Arc;
use std::time::Instant;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn stats(mut s: Vec<f64>) -> (f64, f64, f64) {
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    (
        s.iter().sum::<f64>() / n as f64,
        s[n / 2],
        s[((n as f64 * 0.95) as usize).min(n - 1)],
    )
}

// ────────────────────────────────────────────────────────────────────────────
// [Q1] Notification recipient expansion — wide row vs narrow projection
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE, verbatim `get_users_by_ids` projection: 21 columns incl. `image`
/// and `ui_preferences`. Touch the heavy columns like `User::from_data_full`
/// does (materialize them) so the detoast/parse cost is counted.
async fn recipients_before(pool: &PgPool, ids: &[Uuid]) -> Vec<(Uuid, String, bool)> {
    let rows = sqlx::query(
        r#"
        SELECT
            id, username, email, password_hash, role::text as role_text,
            storage_quota_bytes, storage_used_bytes,
            created_at, updated_at, last_login_at, active,
            oidc_provider, oidc_subject, image, is_external,
            given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
            ui_preferences
        FROM auth.users
        WHERE id = ANY($1)
        "#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .expect("recipients wide");
    rows.into_iter()
        .map(|r| {
            let _image: Option<String> = r.get("image");
            let _prefs: serde_json::Value = r.get("ui_preferences");
            (r.get("id"), r.get("email"), r.get("notify_on_share"))
        })
        .collect()
}

/// AFTER: the shipped narrow projection (image + ui_preferences dropped).
async fn recipients_after(pool: &PgPool, ids: &[Uuid]) -> Vec<(Uuid, String, bool)> {
    let rows = sqlx::query(
        r#"
        SELECT
            id, username, email, password_hash, role::text as role_text,
            storage_quota_bytes, storage_used_bytes,
            created_at, updated_at, last_login_at, active,
            oidc_provider, oidc_subject, is_external,
            given_name, family_name, email_verified_at, preferred_locale, notify_on_share
        FROM auth.users
        WHERE id = ANY($1)
        "#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .expect("recipients narrow");
    rows.into_iter()
        .map(|r| (r.get("id"), r.get("email"), r.get("notify_on_share")))
        .collect()
}

async fn section_recipients(pool: &PgPool) {
    let group: usize = env_or("BENCH_GROUP", 30);
    let passes: usize = env_or("BENCH_PASSES", 200);

    // Seed a group of avatared users (256 KiB data-URI each).
    let mut ids = Vec::with_capacity(group);
    for i in 0..group {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO auth.users (username, email, role, image, notify_on_share)
             VALUES ($1, $2, 'user', $3, true) RETURNING id",
        )
        .bind(format!("bench13_rcpt_{i:04}"))
        .bind(format!("bench13_rcpt_{i:04}@bench.invalid"))
        .bind(format!(
            "data:image/png;base64,{}",
            "QUJDRA==".repeat(32 * 1024)
        ))
        .fetch_one(pool)
        .await
        .expect("seed recipient");
        ids.push(id);
    }

    // Equivalence gate: same (id, email, notify) set either way.
    let mut b = recipients_before(pool, &ids).await;
    let mut a = recipients_after(pool, &ids).await;
    b.sort();
    a.sort();
    assert_eq!(b, a, "recipient projections differ");
    assert_eq!(a.len(), group, "expected all members");
    println!("# [Q1] gate: wide/narrow recipient sets identical ({group} members) — OK");

    let mut wide = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(recipients_before(pool, &ids).await);
        wide.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut narrow = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(recipients_after(pool, &ids).await);
        narrow.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (wm, wp50, wp95) = stats(wide);
    let (nm, np50, np95) = stats(narrow);
    println!("\n## [Q1] Group-notification recipient expansion ({group} avatared members)");
    println!("| arm | mean ms | p50 ms | p95 ms |");
    println!("| BEFORE wide row (incl. image)   | {wm:>8.3} | {wp50:>7.3} | {wp95:>7.3} |");
    println!("| AFTER  narrow (email fields)    | {nm:>8.3} | {np50:>7.3} | {np95:>7.3} |");
    println!(
        "# {:.2}x faster, ~{} KiB avatar/ui_prefs off the wire per fan-out",
        wm / nm,
        group * 256
    );

    sqlx::query("DELETE FROM auth.users WHERE username LIKE 'bench13\\_rcpt\\_%'")
        .execute(pool)
        .await
        .expect("cleanup recipients");
    if nm >= wm {
        eprintln!("GATE FAIL [Q1]: narrow not faster — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [Q2] Login provisioning idempotency — hydrate-all vs EXISTS
// ────────────────────────────────────────────────────────────────────────────

async fn section_provisioning(pool: &PgPool) {
    let cals: usize = env_or("BENCH_CALS", 4);
    let passes: usize = env_or("BENCH_PASSES", 200);

    let owner: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench13_prov', 'bench13_prov@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("seed owner");
    for i in 0..cals {
        sqlx::query(
            "INSERT INTO caldav.calendars (id, name, owner_id, description, color)
             VALUES (gen_random_uuid(), $1, $2, $3, '#3b82f6')",
        )
        .bind(format!("Cal {i}"))
        .bind(owner)
        .bind("A reasonably long calendar description to make the hydrated row wider")
        .execute(pool)
        .await
        .expect("seed calendar");
    }

    async fn before_is_empty(pool: &PgPool, owner: Uuid) -> bool {
        // Verbatim: hydrate every owned calendar row, then `.is_empty()`.
        let rows = sqlx::query(
            "SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
             FROM caldav.calendars WHERE owner_id = $1 ORDER BY name",
        )
        .bind(owner)
        .fetch_all(pool)
        .await
        .expect("list calendars");
        !rows.is_empty()
    }
    async fn after_exists(pool: &PgPool, owner: Uuid) -> bool {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM caldav.calendars WHERE owner_id = $1)")
            .bind(owner)
            .fetch_one(pool)
            .await
            .expect("exists")
    }

    // Gate: identical verdict, present and absent.
    assert!(before_is_empty(pool, owner).await);
    assert!(after_exists(pool, owner).await);
    let ghost = Uuid::new_v4();
    assert_eq!(
        before_is_empty(pool, ghost).await,
        after_exists(pool, ghost).await
    );
    println!("# [Q2] gate: hydrate-all and EXISTS agree (present + absent) — OK");

    let mut before = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(before_is_empty(pool, owner).await);
        before.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut after = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(after_exists(pool, owner).await);
        after.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (bm, bp50, bp95) = stats(before);
    let (am, ap50, ap95) = stats(after);
    println!("\n## [Q2] Login provisioning idempotency probe ({cals} owned calendars)");
    println!("| arm | mean ms | p50 ms | p95 ms |");
    println!("| BEFORE list+hydrate .is_empty() | {bm:>7.3} | {bp50:>7.3} | {bp95:>7.3} |");
    println!("| AFTER  SELECT EXISTS            | {am:>7.3} | {ap50:>7.3} | {ap95:>7.3} |");
    println!(
        "# {:.2}x faster per login probe (×2: calendar + address book)",
        bm / am
    );

    sqlx::query("DELETE FROM caldav.calendars WHERE owner_id = $1")
        .bind(owner)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(owner)
        .execute(pool)
        .await
        .ok();
    if am >= bm {
        eprintln!("GATE FAIL [Q2]: EXISTS not faster — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [Q3] Recent-access recording — upsert+prune (2 RTT) vs prune-on-insert
// ────────────────────────────────────────────────────────────────────────────

async fn section_recent(pool: &PgPool) {
    let cap: i32 = env_or("BENCH_RECENT_CAP", 50);
    let passes: usize = env_or("BENCH_PASSES", 200);

    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench13_recent', 'bench13_recent@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("seed recent user");

    async fn upsert_before(pool: &PgPool, user: Uuid, item: &str) {
        sqlx::query(
            "INSERT INTO auth.user_recent_files (user_id, item_id, item_type, accessed_at)
             VALUES ($1, $2, 'file', CURRENT_TIMESTAMP)
             ON CONFLICT (user_id, item_id, item_type)
             DO UPDATE SET accessed_at = CURRENT_TIMESTAMP",
        )
        .bind(user)
        .bind(item)
        .execute(pool)
        .await
        .expect("upsert");
    }
    async fn prune(pool: &PgPool, user: Uuid, cap: i32) {
        sqlx::query(
            "DELETE FROM auth.user_recent_files
             WHERE id IN (SELECT id FROM auth.user_recent_files
                          WHERE user_id = $1 ORDER BY accessed_at DESC OFFSET $2)",
        )
        .bind(user)
        .bind(cap)
        .execute(pool)
        .await
        .expect("prune");
    }
    async fn upsert_after(pool: &PgPool, user: Uuid, item: &str) -> bool {
        sqlx::query_scalar(
            "INSERT INTO auth.user_recent_files (user_id, item_id, item_type, accessed_at)
             VALUES ($1, $2, 'file', CURRENT_TIMESTAMP)
             ON CONFLICT (user_id, item_id, item_type)
             DO UPDATE SET accessed_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(user)
        .bind(item)
        .fetch_one(pool)
        .await
        .expect("upsert returning")
    }

    // Fill to the cap so the set is at steady state.
    for i in 0..cap {
        upsert_before(pool, user, &format!("seed-{i:04}")).await;
    }

    // Gate: the AFTER path must keep the row count at the cap AND flag
    // insert-vs-update correctly. Re-access an existing item → update (no
    // prune); a brand-new item → insert (prune keeps count == cap).
    let existing = "seed-0000";
    assert!(
        !upsert_after(pool, user, existing).await,
        "re-access must be an UPDATE"
    );
    let fresh = "gate-new-item";
    assert!(
        upsert_after(pool, user, fresh).await,
        "new item must be an INSERT"
    );
    prune(pool, user, cap).await;
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth.user_recent_files WHERE user_id = $1")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(count, cap as i64, "prune-on-insert keeps the cap");
    println!("# [Q3] gate: xmax flags insert/update, count stays at cap — OK");

    // BEFORE: every record = upsert + prune (2 round-trips). Model the
    // common case — re-accessing items already in the set (all UPDATEs).
    let mut before = Vec::with_capacity(passes);
    for i in 0..passes {
        let item = format!("seed-{:04}", i % cap as usize);
        let t = Instant::now();
        upsert_before(pool, user, &item).await;
        prune(pool, user, cap).await;
        before.push(t.elapsed().as_secs_f64() * 1e3);
    }
    // AFTER: upsert RETURNING; prune only when inserted (never, here).
    let mut after = Vec::with_capacity(passes);
    for i in 0..passes {
        let item = format!("seed-{:04}", i % cap as usize);
        let t = Instant::now();
        let inserted = upsert_after(pool, user, &item).await;
        if inserted {
            prune(pool, user, cap).await;
        }
        after.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (bm, bp50, bp95) = stats(before);
    let (am, ap50, ap95) = stats(after);
    println!("\n## [Q3] Recent-access recording (re-access = UPDATE, common path)");
    println!("| arm | mean ms | p50 ms | p95 ms |");
    println!("| BEFORE upsert + prune (2 RTT)   | {bm:>7.3} | {bp50:>7.3} | {bp95:>7.3} |");
    println!("| AFTER  upsert; prune-on-insert  | {am:>7.3} | {ap50:>7.3} | {ap95:>7.3} |");
    println!(
        "# {:.2}x faster on re-access; prune round-trip skipped",
        bm / am
    );

    sqlx::query("DELETE FROM auth.user_recent_files WHERE user_id = $1")
        .bind(user)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(user)
        .execute(pool)
        .await
        .ok();
    if am >= bm {
        eprintln!("GATE FAIL [Q3]: prune-on-insert not faster — rollback");
        std::process::exit(1);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let _ = dotenvy::dotenv();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required (see .env)");
    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(8)
            .connect(&url)
            .await
            .expect("connect"),
    );

    println!("#################################################################");
    println!("# Round-13 query-shape pack");
    println!("#################################################################");

    section_recipients(&pool).await;
    section_provisioning(&pool).await;
    section_recent(&pool).await;

    println!("\nGATE PASS (all sections)");
}
