//! Round-12 query-shape pack (needs the dev Postgres up; reads DATABASE_URL
//! from `.env`).
//!
//! Six sections, each BEFORE (verbatim replica of the shipped query shape)
//! vs AFTER (proposed shape), with equivalence/safety gates:
//!
//!   [1] NC sharee search — the wide 21-column `search_users` row (incl. the
//!       up-to-512 KiB avatar `image`) fetched per match when the handler
//!       only reads `username`, vs a narrow username-only SELECT; plus a
//!       `gin_trgm_ops` index arm for the leading-wildcard ILIKE.
//!   [2] Password login — the redundant full-row `update_user` (17 columns
//!       incl. `image`) that `create_session`'s own `last_login_at` UPDATE
//!       immediately overwrites, vs create_session alone.
//!   [3] Email-verified stamp (magic-link / OIDC JIT) — full-row
//!       `update_user` to set one timestamp vs a narrow conditional UPDATE.
//!   [4] Refresh-token rotation — revoke txn + create txn (2 transactions,
//!       6 statements) vs one fused rotation transaction.
//!   [5] WOPI CheckFileInfo — require(Read) → get_file → check(Update)
//!       serial vs `tokio::join!` (real `PgAclEngine` + real file read repo;
//!       cold and warm arms).
//!   [6] Upload quota pair — user-envelope + drive-cap checks as two serial
//!       point reads vs one fused SELECT (verdict precedence preserved).
//!
//! Run:
//!   cargo run --release --features bench --example bench_round12_queries
//! Tunables (env): BENCH_PASSES (200), BENCH_SHR_USERS (3000),
//!   BENCH_WOPI_FILES (100), BENCH_WARM_ITERS (2000)

use std::env;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use oxicloud::application::ports::authorization_ports::AuthorizationEngine;
use oxicloud::application::ports::storage_ports::FileReadPort;
use oxicloud::domain::services::authorization::{Permission, Resource, Subject};
use oxicloud::infrastructure::repositories::pg::{
    FileBlobReadRepository, FolderDbRepository, SubjectGroupPgRepository,
};
use oxicloud::infrastructure::services::dedup_service::DedupService;
use oxicloud::infrastructure::services::local_blob_backend::LocalBlobBackend;
use oxicloud::infrastructure::services::pg_acl_engine::PgAclEngine;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn stats(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let mean = samples.iter().sum::<f64>() / n as f64;
    let p50 = samples[n / 2];
    let p95 = samples[((n as f64 * 0.95) as usize).min(n - 1)];
    (mean, p50, p95)
}

// ────────────────────────────────────────────────────────────────────────────
// [1] NC sharee search — wide row vs narrow username-only (+ trgm arm)
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE, verbatim `UserPgRepository::search_users` SELECT list.
async fn sharee_before(pool: &PgPool, pattern: &str, limit: i64) -> Vec<Option<String>> {
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
        WHERE (username ILIKE $1 OR email ILIKE $1)
          AND ($3 OR is_external = FALSE)
        ORDER BY username
        LIMIT $2
        "#,
    )
    .bind(pattern)
    .bind(limit)
    .bind(false)
    .fetch_all(pool)
    .await
    .expect("sharee wide");
    rows.into_iter()
        .map(|r| {
            // The handler materializes the whole row (incl. `image`) into a
            // `User`/`UserDto` and then keeps only the username. Touch the
            // wide columns like the entity build does.
            let _image: Option<String> = r.get("image");
            let _email: Option<String> = r.get("email");
            r.get("username")
        })
        .collect()
}

/// AFTER: same WHERE / ORDER / LIMIT, username-only projection.
async fn sharee_after(pool: &PgPool, pattern: &str, limit: i64) -> Vec<Option<String>> {
    let rows = sqlx::query(
        r#"
        SELECT username
        FROM auth.users
        WHERE (username ILIKE $1 OR email ILIKE $1)
          AND ($3 OR is_external = FALSE)
        ORDER BY username
        LIMIT $2
        "#,
    )
    .bind(pattern)
    .bind(limit)
    .bind(false)
    .fetch_all(pool)
    .await
    .expect("sharee narrow");
    rows.into_iter().map(|r| r.get("username")).collect()
}

async fn section_sharee(pool: &PgPool) {
    let n_users: i64 = env_or("BENCH_SHR_USERS", 3000);
    let passes: usize = env_or("BENCH_PASSES", 200);
    let avatared = 600.min(n_users);

    // Seed server-side (no avatar bytes on the wire): first `avatared` users
    // carry a ~256 KiB data-URI image, the rest none.
    sqlx::query(
        r#"
        INSERT INTO auth.users (username, email, role, image)
        SELECT
            'shr_user_' || lpad(i::text, 5, '0'),
            'shr' || i || '@bench.invalid',
            'user',
            CASE WHEN i < $2 THEN 'data:image/png;base64,' || repeat('QUJDRA==', 32768) END
        FROM generate_series(0, $1 - 1) AS g(i)
        "#,
    )
    .bind(n_users)
    .bind(avatared)
    .execute(pool)
    .await
    .expect("seed sharee users");

    // Typing "shr_user_0" — 26-row NC sharee page, all matches avatar-carrying.
    let pattern = "%shr_user_0%";
    let limit = 26i64;

    // Equivalence gate: identical username lists.
    let b = sharee_before(pool, pattern, limit).await;
    let a = sharee_after(pool, pattern, limit).await;
    assert_eq!(b, a, "sharee result lists differ");
    assert_eq!(b.len(), limit as usize, "expected a full page");
    println!("# [1] gate: wide and narrow username lists identical — OK");

    let mut wide = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(sharee_before(pool, pattern, limit).await);
        wide.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut narrow = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(sharee_after(pool, pattern, limit).await);
        narrow.push(t.elapsed().as_secs_f64() * 1e3);
    }

    // trgm arm: the production migration candidate.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS bench_users_username_trgm
         ON auth.users USING gin (username gin_trgm_ops)",
    )
    .execute(pool)
    .await
    .expect("trgm username");
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS bench_users_email_trgm
         ON auth.users USING gin (email gin_trgm_ops)",
    )
    .execute(pool)
    .await
    .expect("trgm email");
    sqlx::query("ANALYZE auth.users")
        .execute(pool)
        .await
        .expect("analyze");
    let a_idx = sharee_after(pool, pattern, limit).await;
    assert_eq!(b, a_idx, "trgm-indexed narrow list differs");
    let mut narrow_idx = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(sharee_after(pool, pattern, limit).await);
        narrow_idx.push(t.elapsed().as_secs_f64() * 1e3);
    }

    let (wm, wp50, wp95) = stats(wide);
    let (nm, np50, np95) = stats(narrow);
    let (im, ip50, ip95) = stats(narrow_idx);
    println!("\n## [1] NC sharee search ({n_users} users, 26-row page, all matches avatared)");
    println!("| arm | mean ms | p50 ms | p95 ms |");
    println!("| BEFORE wide row (incl. image)   | {wm:>8.3} | {wp50:>7.3} | {wp95:>7.3} |");
    println!("| AFTER  narrow username          | {nm:>8.3} | {np50:>7.3} | {np95:>7.3} |");
    println!("| AFTER  narrow + trgm index      | {im:>8.3} | {ip50:>7.3} | {ip95:>7.3} |");
    println!("# narrow speedup {:.2}x; +trgm {:.2}x", wm / nm, wm / im);

    // Cleanup (drop bench indexes; production ones ship via migration only
    // if the arm wins).
    sqlx::query("DROP INDEX IF EXISTS auth.bench_users_username_trgm")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DROP INDEX IF EXISTS auth.bench_users_email_trgm")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM auth.users WHERE username LIKE 'shr\\_user\\_%'")
        .execute(pool)
        .await
        .expect("cleanup sharee users");

    if nm >= wm {
        eprintln!("GATE FAIL [1]: narrow arm not faster — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [2] Login stamp — redundant full-row update_user + create_session
// vs create_session alone. [3] email-verified narrow stamp.
// [4] rotation fused txn. Shared user fixture with a 256 KiB avatar.
// ────────────────────────────────────────────────────────────────────────────

struct AuthFixture {
    user_id: Uuid,
    image: String,
}

async fn seed_auth_user(pool: &PgPool, tag: &str) -> AuthFixture {
    let image = format!("data:image/png;base64,{}", "QUJDRA==".repeat(32 * 1024));
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role, image, storage_quota_bytes)
         VALUES ($1, $2, 'user', $3, 10737418240) RETURNING id",
    )
    .bind(format!("bench12_{tag}"))
    .bind(format!("bench12_{tag}@bench.invalid"))
    .bind(&image)
    .fetch_one(pool)
    .await
    .expect("seed auth user");
    AuthFixture { user_id, image }
}

/// Verbatim replica of `UserPgRepository::update_user`'s statement, executed
/// inside a transaction like `with_transaction` does.
async fn full_row_update_user(pool: &PgPool, f: &AuthFixture, last_login: bool) {
    let now = Utc::now();
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query(
        r#"
        UPDATE auth.users
        SET
            username = $2,
            email = $3,
            password_hash = $4,
            role = $5::auth.userrole,
            storage_quota_bytes = $6,
            storage_used_bytes = $7,
            updated_at = $8,
            last_login_at = $9,
            active = $10,
            image = $11,
            given_name = $12,
            family_name = $13,
            email_verified_at = $14,
            preferred_locale = $15,
            notify_on_share = $16,
            is_external = $17
        WHERE id = $1
        "#,
    )
    .bind(f.user_id)
    .bind("bench12_login")
    .bind("bench12_login@bench.invalid")
    .bind(Option::<String>::None)
    .bind("user")
    .bind(10737418240i64)
    .bind(0i64)
    .bind(now)
    .bind(if last_login { Some(now) } else { None })
    .bind(true)
    .bind(&f.image)
    .bind(Option::<String>::None)
    .bind(Option::<String>::None)
    .bind(if last_login { None } else { Some(now) })
    .bind(Option::<String>::None)
    .bind(true)
    .bind(false)
    .execute(&mut *tx)
    .await
    .expect("full-row update");
    tx.commit().await.expect("commit");
}

/// Verbatim replica of `SessionPgRepository::create_session` (insert + the
/// last_login stamp, one transaction).
async fn create_session_txn(pool: &PgPool, user_id: Uuid) -> Uuid {
    let sid = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query(
        r#"
        INSERT INTO auth.sessions (
            id, user_id, refresh_token, expires_at,
            ip_address, user_agent, created_at, revoked, family_id
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(sid)
    .bind(user_id)
    .bind(format!("rt-{sid}"))
    .bind(Utc::now() + chrono::Duration::days(30))
    .bind(Option::<String>::None)
    .bind(Option::<String>::None)
    .bind(Utc::now())
    .bind(false)
    .bind(Uuid::new_v4())
    .execute(&mut *tx)
    .await
    .expect("insert session");
    sqlx::query("UPDATE auth.users SET last_login_at = NOW(), updated_at = NOW() WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .expect("stamp last_login");
    tx.commit().await.expect("commit");
    sid
}

async fn section_login_stamp(pool: &PgPool) {
    let passes: usize = env_or("BENCH_PASSES", 200);
    let f = seed_auth_user(pool, "login").await;

    // Safety gate: the AFTER flow must leave the same observable row state
    // (last_login_at set, avatar intact, everything else untouched).
    full_row_update_user(pool, &f, true).await;
    create_session_txn(pool, f.user_id).await;
    let before_row: (Option<chrono::DateTime<Utc>>, Option<String>, bool) =
        sqlx::query_as("SELECT last_login_at, image, active FROM auth.users WHERE id = $1")
            .bind(f.user_id)
            .fetch_one(pool)
            .await
            .expect("row");
    sqlx::query("UPDATE auth.users SET last_login_at = NULL WHERE id = $1")
        .bind(f.user_id)
        .execute(pool)
        .await
        .unwrap();
    create_session_txn(pool, f.user_id).await;
    let after_row: (Option<chrono::DateTime<Utc>>, Option<String>, bool) =
        sqlx::query_as("SELECT last_login_at, image, active FROM auth.users WHERE id = $1")
            .bind(f.user_id)
            .fetch_one(pool)
            .await
            .expect("row");
    assert!(before_row.0.is_some() && after_row.0.is_some());
    assert_eq!(before_row.1, after_row.1, "avatar must be untouched");
    assert_eq!(before_row.2, after_row.2);
    println!("# [2] gate: create_session alone leaves identical observable state — OK");

    let mut before = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        full_row_update_user(pool, &f, true).await;
        create_session_txn(pool, f.user_id).await;
        before.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut after = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        create_session_txn(pool, f.user_id).await;
        after.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (bm, bp50, bp95) = stats(before);
    let (am, ap50, ap95) = stats(after);
    println!("\n## [2] Password-login stamp (user with 256 KiB avatar)");
    println!("| arm | mean ms | p50 ms | p95 ms |");
    println!("| BEFORE update_user + create_session | {bm:>7.3} | {bp50:>7.3} | {bp95:>7.3} |");
    println!("| AFTER  create_session only          | {am:>7.3} | {ap50:>7.3} | {ap95:>7.3} |");
    println!(
        "# {:.2}x faster per login; 1 txn + full-row write (incl. avatar) removed",
        bm / am
    );

    sqlx::query("DELETE FROM auth.sessions WHERE user_id = $1")
        .bind(f.user_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(f.user_id)
        .execute(pool)
        .await
        .ok();
    if am >= bm {
        eprintln!("GATE FAIL [2]: AFTER not faster — rollback");
        std::process::exit(1);
    }
}

async fn section_email_stamp(pool: &PgPool) {
    let passes: usize = env_or("BENCH_PASSES", 200);
    let f = seed_auth_user(pool, "email").await;

    // AFTER: the narrow conditional stamp (idempotent in SQL, mirroring the
    // entity guard `if email_verified_at.is_none()`).
    async fn narrow_stamp(pool: &PgPool, id: Uuid) -> u64 {
        sqlx::query(
            "UPDATE auth.users
             SET email_verified_at = NOW(), updated_at = NOW()
             WHERE id = $1 AND email_verified_at IS NULL",
        )
        .bind(id)
        .execute(pool)
        .await
        .expect("narrow stamp")
        .rows_affected()
    }

    // Gates: first call stamps; second call is a no-op (idempotent); value
    // survives; avatar untouched.
    assert_eq!(narrow_stamp(pool, f.user_id).await, 1);
    let first: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT email_verified_at FROM auth.users WHERE id = $1")
            .bind(f.user_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(first.is_some());
    assert_eq!(
        narrow_stamp(pool, f.user_id).await,
        0,
        "second stamp must be a no-op"
    );
    let second: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT email_verified_at FROM auth.users WHERE id = $1")
            .bind(f.user_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(first, second, "timestamp must not move on re-stamp");
    println!("# [3] gate: narrow stamp idempotent, value stable — OK");

    let mut before = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        full_row_update_user(pool, &f, false).await;
        before.push(t.elapsed().as_secs_f64() * 1e3);
    }
    // Reset so the narrow arm measures the write path (not the no-op path).
    let mut after = Vec::with_capacity(passes);
    for _ in 0..passes {
        sqlx::query("UPDATE auth.users SET email_verified_at = NULL WHERE id = $1")
            .bind(f.user_id)
            .execute(pool)
            .await
            .unwrap();
        let t = Instant::now();
        narrow_stamp(pool, f.user_id).await;
        after.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (bm, bp50, bp95) = stats(before);
    let (am, ap50, ap95) = stats(after);

    // [3b] OIDC repeat-login profile sync — the guarded narrow UPDATE in
    // its no-op case (same avatar, already verified). This arm is the
    // evidence for why production ALSO short-circuits app-side: even a
    // 0-row guarded UPDATE ships the ≤512 KiB avatar parameter over the
    // wire just to compare it server-side, so the shipped shape compares
    // against the already-fetched row in memory and issues NO query on
    // the repeat-login common case (the guarded UPDATE remains as the
    // write path when something actually changed, and as a belt-and-
    // braces guard).
    sqlx::query("UPDATE auth.users SET email_verified_at = NOW(), image = $2 WHERE id = $1")
        .bind(f.user_id)
        .bind(&f.image)
        .execute(pool)
        .await
        .unwrap();
    let mut sync_noop = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        let res = sqlx::query(
            "UPDATE auth.users
             SET image = $2,
                 email_verified_at = COALESCE(email_verified_at, NOW()),
                 updated_at = NOW()
             WHERE id = $1
               AND (image IS DISTINCT FROM $2 OR email_verified_at IS NULL)",
        )
        .bind(f.user_id)
        .bind(&f.image)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(res.rows_affected(), 0, "no-op path must not write");
        sync_noop.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (sm, sp50, sp95) = stats(sync_noop);

    println!("\n## [3] Email-verified stamp (user with 256 KiB avatar)");
    println!("| arm | mean ms | p50 ms | p95 ms |");
    println!("| BEFORE full-row update_user | {bm:>7.3} | {bp50:>7.3} | {bp95:>7.3} |");
    println!("| AFTER  narrow conditional   | {am:>7.3} | {ap50:>7.3} | {ap95:>7.3} |");
    println!("| AFTER  oidc guarded no-op   | {sm:>7.3} | {sp50:>7.3} | {sp95:>7.3} |");
    println!(
        "# {:.2}x faster per stamp; guarded no-op still ships the avatar param \
         ({:.2}x) — hence the app-side skip (0 queries) shipped in production",
        bm / am,
        bm / sm
    );

    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(f.user_id)
        .execute(pool)
        .await
        .ok();
    if am >= bm {
        eprintln!("GATE FAIL [3]: AFTER not faster — rollback");
        std::process::exit(1);
    }
}

async fn section_rotation(pool: &PgPool) {
    let passes: usize = env_or("BENCH_PASSES", 200);
    let f = seed_auth_user(pool, "rot").await;

    // BEFORE: revoke txn (verbatim) + create txn (verbatim).
    async fn before_rotate(pool: &PgPool, user_id: Uuid, old: Uuid) -> Uuid {
        let mut tx = pool.begin().await.expect("begin");
        let _row =
            sqlx::query("UPDATE auth.sessions SET revoked = true WHERE id = $1 RETURNING user_id")
                .bind(old)
                .fetch_optional(&mut *tx)
                .await
                .expect("revoke");
        tx.commit().await.expect("commit");
        create_session_txn(pool, user_id).await
    }

    // AFTER: one fused transaction (same three statements, one txn).
    async fn after_rotate(pool: &PgPool, user_id: Uuid, old: Uuid) -> Uuid {
        let sid = Uuid::new_v4();
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query("UPDATE auth.sessions SET revoked = true WHERE id = $1 RETURNING user_id")
            .bind(old)
            .fetch_optional(&mut *tx)
            .await
            .expect("revoke");
        sqlx::query(
            r#"
            INSERT INTO auth.sessions (
                id, user_id, refresh_token, expires_at,
                ip_address, user_agent, created_at, revoked, family_id
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(sid)
        .bind(user_id)
        .bind(format!("rt-{sid}"))
        .bind(Utc::now() + chrono::Duration::days(30))
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .bind(Utc::now())
        .bind(false)
        .bind(Uuid::new_v4())
        .execute(&mut *tx)
        .await
        .expect("insert");
        sqlx::query(
            "UPDATE auth.users SET last_login_at = NOW(), updated_at = NOW() WHERE id = $1",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .expect("stamp");
        tx.commit().await.expect("commit");
        sid
    }

    // Gate: both arms leave old session revoked + new session live.
    let s0 = create_session_txn(pool, f.user_id).await;
    let s1 = before_rotate(pool, f.user_id, s0).await;
    let s2 = after_rotate(pool, f.user_id, s1).await;
    let states: Vec<(Uuid, bool)> =
        sqlx::query_as("SELECT id, revoked FROM auth.sessions WHERE user_id = $1")
            .bind(f.user_id)
            .fetch_all(pool)
            .await
            .unwrap();
    let get = |id: Uuid| states.iter().find(|(s, _)| *s == id).map(|(_, r)| *r);
    assert_eq!(get(s0), Some(true), "s0 revoked");
    assert_eq!(get(s1), Some(true), "s1 revoked by after_rotate");
    assert_eq!(get(s2), Some(false), "s2 live");
    println!("# [4] gate: fused rotation leaves identical session states — OK");

    let mut cur = s2;
    let mut before = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        cur = before_rotate(pool, f.user_id, cur).await;
        before.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut after = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        cur = after_rotate(pool, f.user_id, cur).await;
        after.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (bm, bp50, bp95) = stats(before);
    let (am, ap50, ap95) = stats(after);
    println!("\n## [4] Refresh-token rotation");
    println!("| arm | mean ms | p50 ms | p95 ms |");
    println!("| BEFORE 2 transactions | {bm:>7.3} | {bp50:>7.3} | {bp95:>7.3} |");
    println!("| AFTER  1 transaction  | {am:>7.3} | {ap50:>7.3} | {ap95:>7.3} |");
    println!("# {:.2}x faster per rotation", bm / am);

    sqlx::query("DELETE FROM auth.sessions WHERE user_id = $1")
        .bind(f.user_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(f.user_id)
        .execute(pool)
        .await
        .ok();
    if am >= bm {
        eprintln!("GATE FAIL [4]: AFTER not faster — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [5] WOPI CheckFileInfo triple — serial vs join! (real engine + file repo)
// ────────────────────────────────────────────────────────────────────────────

struct WopiSeed {
    caller: Uuid,
    drive_id: Uuid,
    root_folder: Uuid,
    blob_hash: String,
    file_ids: Vec<Uuid>,
}

async fn wopi_seed(pool: &PgPool, n_files: usize) -> WopiSeed {
    let mut tx = pool.begin().await.expect("begin");
    let caller: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench12_wopi', 'bench12_wopi@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed caller");
    let drive_id: Uuid =
        sqlx::query_scalar("INSERT INTO storage.drives (kind) VALUES ('shared') RETURNING id")
            .fetch_one(&mut *tx)
            .await
            .expect("seed drive");
    let root_folder: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('Bench12 WOPI', '/Bench12 WOPI', 'x', $1) RETURNING id",
    )
    .bind(drive_id)
    .fetch_one(&mut *tx)
    .await
    .expect("seed folder");
    sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
        .bind(root_folder)
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .expect("stamp root");
    sqlx::query(
        "INSERT INTO storage.role_grants
             (subject_type, subject_id, resource_type, resource_id, role, granted_by)
         VALUES ('user', $1, 'drive', $2, 'editor'::storage.grant_role, $1)",
    )
    .bind(caller)
    .bind(drive_id)
    .execute(&mut *tx)
    .await
    .expect("seed grant");
    let blob_hash = "bench12wopi00000000000000000000000000000000000000000000000000b1".to_string();
    sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count) VALUES ($1, 1, 1)")
        .bind(&blob_hash)
        .execute(&mut *tx)
        .await
        .expect("seed blob");
    let mut file_ids = Vec::with_capacity(n_files);
    for i in 0..n_files {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
             VALUES ($1, $2, $3, 1, 'application/vnd.oasis.opendocument.text', $4) RETURNING id",
        )
        .bind(format!("bench12-{i:04}.odt"))
        .bind(root_folder)
        .bind(&blob_hash)
        .bind(drive_id)
        .fetch_one(&mut *tx)
        .await
        .expect("seed file");
        file_ids.push(id);
    }
    tx.commit().await.expect("commit");
    WopiSeed {
        caller,
        drive_id,
        root_folder,
        blob_hash,
        file_ids,
    }
}

async fn wopi_cleanup(pool: &PgPool, s: &WopiSeed) {
    let _ = sqlx::query("DELETE FROM storage.role_grants WHERE resource_id = $1")
        .bind(s.drive_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.files WHERE drive_id = $1")
        .bind(s.drive_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(s.drive_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.folders WHERE id = $1")
        .bind(s.root_folder)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = $1")
        .bind(&s.blob_hash)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(s.caller)
        .execute(pool)
        .await;
}

fn wopi_engine(pool: &Arc<PgPool>) -> (Arc<PgAclEngine>, Arc<FileBlobReadRepository>) {
    let folder_repo = Arc::new(FolderDbRepository::new(pool.clone()));
    let backend = Arc::new(LocalBlobBackend::new(std::path::Path::new(
        "/tmp/bench12-wopi-blobs",
    )));
    let dedup = Arc::new(DedupService::new(backend, pool.clone(), pool.clone()));
    let file_repo = Arc::new(FileBlobReadRepository::new(
        pool.clone(),
        dedup,
        folder_repo.clone(),
    ));
    let group_repo = Arc::new(SubjectGroupPgRepository::new(pool.clone()));
    (
        Arc::new(PgAclEngine::new(
            pool.clone(),
            folder_repo,
            file_repo.clone(),
            group_repo,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )),
        file_repo,
    )
}

/// BEFORE, verbatim handler shape: require(Read) → get_file → check(Update).
async fn wopi_before(
    engine: &Arc<PgAclEngine>,
    files: &Arc<FileBlobReadRepository>,
    caller: Uuid,
    file_id: Uuid,
) -> (String, bool) {
    engine
        .require(
            Subject::User(caller),
            Permission::Read,
            Resource::File(file_id),
        )
        .await
        .expect("read");
    let file = files.get_file(&file_id.to_string()).await.expect("file");
    let can_write = engine
        .check(
            Subject::User(caller),
            Permission::Update,
            Resource::File(file_id),
        )
        .await
        .unwrap_or(false);
    (file.name().to_string(), can_write)
}

/// AFTER: the three independent lookups overlapped.
async fn wopi_after(
    engine: &Arc<PgAclEngine>,
    files: &Arc<FileBlobReadRepository>,
    caller: Uuid,
    file_id: Uuid,
) -> (String, bool) {
    let id_str = file_id.to_string();
    let (read, file, can_write) = tokio::join!(
        engine.require(
            Subject::User(caller),
            Permission::Read,
            Resource::File(file_id)
        ),
        files.get_file(&id_str),
        engine.check(
            Subject::User(caller),
            Permission::Update,
            Resource::File(file_id)
        ),
    );
    read.expect("read");
    let file = file.expect("file");
    (file.name().to_string(), can_write.unwrap_or(false))
}

async fn section_wopi(pool: &Arc<PgPool>) {
    let n_files: usize = env_or("BENCH_WOPI_FILES", 100);
    let warm_iters: usize = env_or("BENCH_WARM_ITERS", 2000);
    let seed = wopi_seed(pool, n_files).await;

    // Equivalence gate (fresh engines so both arms run the same cold path).
    let (e1, f1) = wopi_engine(pool);
    let (e2, f2) = wopi_engine(pool);
    for id in seed.file_ids.iter().take(10) {
        let b = wopi_before(&e1, &f1, seed.caller, *id).await;
        let a = wopi_after(&e2, &f2, seed.caller, *id).await;
        assert_eq!(b, a, "wopi results differ");
    }
    println!("# [5] gate: serial and join! results identical (10 files) — OK");

    // COLD arms: fresh engine, one triple per file (the first CheckFileInfo
    // per file per TTL window).
    let (ec, fc) = wopi_engine(pool);
    let t = Instant::now();
    for id in &seed.file_ids {
        std::hint::black_box(wopi_before(&ec, &fc, seed.caller, *id).await);
    }
    let cold_before = t.elapsed().as_secs_f64() * 1e3 / n_files as f64;
    let (ec2, fc2) = wopi_engine(pool);
    let t = Instant::now();
    for id in &seed.file_ids {
        std::hint::black_box(wopi_after(&ec2, &fc2, seed.caller, *id).await);
    }
    let cold_after = t.elapsed().as_secs_f64() * 1e3 / n_files as f64;

    // WARM arms: same engine, authz caches hot — get_file dominates.
    let (ew, fw) = wopi_engine(pool);
    for id in &seed.file_ids {
        wopi_before(&ew, &fw, seed.caller, *id).await;
    }
    let t = Instant::now();
    for i in 0..warm_iters {
        let id = seed.file_ids[i % n_files];
        std::hint::black_box(wopi_before(&ew, &fw, seed.caller, id).await);
    }
    let warm_before = t.elapsed().as_secs_f64() * 1e3 / warm_iters as f64;
    let t = Instant::now();
    for i in 0..warm_iters {
        let id = seed.file_ids[i % n_files];
        std::hint::black_box(wopi_after(&ew, &fw, seed.caller, id).await);
    }
    let warm_after = t.elapsed().as_secs_f64() * 1e3 / warm_iters as f64;

    println!("\n## [5] WOPI CheckFileInfo triple (real PgAclEngine)");
    println!("| arm | cold ms/call | warm ms/call |");
    println!("| BEFORE serial | {cold_before:>9.3} | {warm_before:>9.3} |");
    println!("| AFTER  join!  | {cold_after:>9.3} | {warm_after:>9.3} |");
    println!(
        "# cold {:.2}x, warm {:.2}x",
        cold_before / cold_after,
        warm_before / warm_after
    );

    wopi_cleanup(pool, &seed).await;
    if cold_after >= cold_before && warm_after >= warm_before {
        eprintln!("GATE FAIL [5]: join! not faster on either arm — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [6] Upload quota pair — two serial point reads vs one fused SELECT
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum QuotaVerdict {
    Ok,
    UserQuotaExceeded,
    DriveQuotaExceeded,
    DriveNotFound,
}

/// BEFORE, verbatim: `check_storage_quota` (narrow user read) then
/// `check_drive_quota` (drive point read), serial.
async fn quota_before(
    pool: &PgPool,
    user_id: Uuid,
    drive_id: Uuid,
    additional: u64,
) -> QuotaVerdict {
    let (used, quota): (i64, i64) = sqlx::query_as(
        "SELECT storage_used_bytes, storage_quota_bytes FROM auth.users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("user quota row");
    if quota > 0 {
        let additional_i = additional as i64;
        if additional_i > quota || used + additional_i > quota {
            return QuotaVerdict::UserQuotaExceeded;
        }
    }
    let row: Option<(i64, Option<i64>)> =
        sqlx::query_as("SELECT used_bytes, quota_bytes FROM storage.drives WHERE id = $1")
            .bind(drive_id)
            .fetch_optional(pool)
            .await
            .expect("drive quota row");
    let Some((dused, dquota)) = row else {
        return QuotaVerdict::DriveNotFound;
    };
    let Some(dquota) = dquota else {
        return QuotaVerdict::Ok;
    };
    if (dused as i128) + (additional as i128) > dquota as i128 {
        return QuotaVerdict::DriveQuotaExceeded;
    }
    QuotaVerdict::Ok
}

/// Fused row: `(user_used, user_quota, drive_used, drive_quota, drive_found)`.
type QuotaPairRow = (i64, i64, Option<i64>, Option<i64>, bool);

/// AFTER: one fused round-trip; verdict precedence identical (user envelope
/// first, then drive existence, then drive cap).
async fn quota_after(
    pool: &PgPool,
    user_id: Uuid,
    drive_id: Uuid,
    additional: u64,
) -> QuotaVerdict {
    let row: Option<QuotaPairRow> = sqlx::query_as(
        r#"
        SELECT u.storage_used_bytes, u.storage_quota_bytes,
               d.used_bytes, d.quota_bytes, (d.id IS NOT NULL) AS drive_found
        FROM auth.users u
        LEFT JOIN storage.drives d ON d.id = $2
        WHERE u.id = $1
        "#,
    )
    .bind(user_id)
    .bind(drive_id)
    .fetch_optional(pool)
    .await
    .expect("fused quota row");
    let Some((used, quota, dused, dquota, drive_found)) = row else {
        // user missing — out of scope here (upload paths resolve the caller
        // first); keep the BEFORE panic semantics.
        panic!("user quota row");
    };
    if quota > 0 {
        let additional_i = additional as i64;
        if additional_i > quota || used + additional_i > quota {
            return QuotaVerdict::UserQuotaExceeded;
        }
    }
    if !drive_found {
        return QuotaVerdict::DriveNotFound;
    }
    match dquota {
        None => QuotaVerdict::Ok,
        Some(dq) => {
            if (dused.unwrap_or(0) as i128) + (additional as i128) > dq as i128 {
                QuotaVerdict::DriveQuotaExceeded
            } else {
                QuotaVerdict::Ok
            }
        }
    }
}

async fn section_quota(pool: &PgPool) {
    let iters: usize = env_or("BENCH_WARM_ITERS", 2000);

    // Fixtures: user 10 GiB quota / 1 GiB used; capped drive; unlimited drive.
    let user_ok: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role, storage_quota_bytes, storage_used_bytes)
         VALUES ('bench12_quota', 'bench12_quota@bench.invalid', 'user', 10737418240, 1073741824)
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("seed quota user");
    let drive_cap: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, quota_bytes, used_bytes)
         VALUES ('shared', 5368709120, 4294967296) RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("seed capped drive");
    let drive_unl: Uuid =
        sqlx::query_scalar("INSERT INTO storage.drives (kind) VALUES ('shared') RETURNING id")
            .fetch_one(pool)
            .await
            .expect("seed unlimited drive");
    let drive_missing = Uuid::new_v4();

    // Verdict-identity gate across the scenario matrix.
    let scenarios: &[(Uuid, Uuid, u64)] = &[
        (user_ok, drive_cap, 1024),                    // ok
        (user_ok, drive_cap, 2 * 1024 * 1024 * 1024),  // drive cap exceeded
        (user_ok, drive_cap, 20 * 1024 * 1024 * 1024), // user envelope exceeded (precedence)
        (user_ok, drive_unl, 8 * 1024 * 1024 * 1024),  // unlimited drive, user ok
        (user_ok, drive_missing, 1024),                // drive missing
    ];
    for (u, d, add) in scenarios {
        let b = quota_before(pool, *u, *d, *add).await;
        let a = quota_after(pool, *u, *d, *add).await;
        assert_eq!(b, a, "verdict differs for add={add}");
    }
    println!("# [6] gate: verdict identity across 5 scenarios (incl. precedence) — OK");

    let t = Instant::now();
    for i in 0..iters {
        std::hint::black_box(quota_before(pool, user_ok, drive_cap, (i % 4096) as u64).await);
    }
    let before_ms = t.elapsed().as_secs_f64() * 1e3 / iters as f64;
    let t = Instant::now();
    for i in 0..iters {
        std::hint::black_box(quota_after(pool, user_ok, drive_cap, (i % 4096) as u64).await);
    }
    let after_ms = t.elapsed().as_secs_f64() * 1e3 / iters as f64;

    println!("\n## [6] Upload quota pair (per NC chunk PUT / upload gate)");
    println!("| arm | ms/check |");
    println!("| BEFORE 2 serial point reads | {before_ms:>7.3} |");
    println!("| AFTER  1 fused read         | {after_ms:>7.3} |");
    println!(
        "# {:.2}x faster, 1 query saved per check",
        before_ms / after_ms
    );

    sqlx::query("DELETE FROM storage.drives WHERE id IN ($1, $2)")
        .bind(drive_cap)
        .bind(drive_unl)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(user_ok)
        .execute(pool)
        .await
        .ok();
    if after_ms >= before_ms {
        eprintln!("GATE FAIL [6]: fused read not faster — rollback");
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
    println!("# Round-12 query-shape pack");
    println!("#################################################################");

    section_sharee(&pool).await;
    section_login_stamp(&pool).await;
    section_email_stamp(&pool).await;
    section_rotation(&pool).await;
    section_wopi(&pool).await;
    section_quota(&pool).await;

    println!("\nGATE PASS (all sections)");
}
