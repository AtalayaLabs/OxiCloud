//! Quota-path benchmark — full `auth.users` row vs narrow 2-column read.
//!
//! `check_storage_quota` (every upload) and `get_user_storage_info` (every
//! quota-reporting PROPFIND) used to call `get_user_by_id`, whose SELECT
//! drags the whole user row — including `image`, an avatar data URI of up
//! to 512 KiB — across the wire to read two i64s. The change reads only
//! `(storage_used_bytes, storage_quota_bytes)`
//! (`UserPgRepository::get_storage_usage`). Companion change measured here
//! as "SKIP": PROPFINDs whose prop list never names a quota prop now skip
//! the resolution entirely (`PropFindRequest::wants_quota`).
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_quota_path
//! Tunables: BENCH_SECONDS (4), BENCH_CONCURRENCIES ("8,64"), BENCH_IMAGE_KB (512)

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn seed(pool: &PgPool, image_kb: usize) -> Uuid {
    // Realistic worst-ish case: an avatar data URI at the documented cap.
    let image = format!("data:image/png;base64,{}", "A".repeat(image_kb * 1024 - 22));
    sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role, image)
         VALUES ('bench_quota', 'bench_quota@bench.invalid', 'user', $1)
         RETURNING id",
    )
    .bind(&image)
    .fetch_one(pool)
    .await
    .expect("seed user")
}

async fn cleanup(pool: &PgPool, user_id: Uuid) {
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
}

/// BEFORE: the full-row SELECT `get_user_by_id` runs (same column list).
async fn one_op_full(pool: &PgPool, id: Uuid) {
    let _row = sqlx::query(
        r#"
        SELECT
            id, username, email, password_hash, role::text as role_text,
            storage_quota_bytes, storage_used_bytes,
            created_at, updated_at, last_login_at, active,
            oidc_provider, oidc_subject, image, is_external,
            given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
            ui_preferences
        FROM auth.users
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("full row");
}

/// AFTER: the narrow `get_storage_usage` SELECT.
async fn one_op_narrow(pool: &PgPool, id: Uuid) {
    let _row: (i64, i64) = sqlx::query_as(
        "SELECT storage_used_bytes, storage_quota_bytes FROM auth.users WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("narrow row");
}

struct Stats {
    rps: f64,
    p50: f64,
    p99: f64,
}

fn summarize(mut lats: Vec<f64>, secs: u64) -> Stats {
    lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = lats.len();
    let pct = |p: f64| {
        if n == 0 {
            0.0
        } else {
            lats[((n as f64 * p) as usize).min(n - 1)]
        }
    };
    Stats {
        rps: n as f64 / secs as f64,
        p50: pct(0.50),
        p99: pct(0.99),
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").expect("set DATABASE_URL");
    let secs: u64 = env_or("BENCH_SECONDS", 4);
    let image_kb: usize = env_or("BENCH_IMAGE_KB", 512);
    let concurrencies: Vec<usize> = env::var("BENCH_CONCURRENCIES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![8, 64]);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(20)
            .min_connections(20)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect"),
    );
    let user_id = seed(&pool, image_kb).await;

    println!("\n# quota lookup: full user row (incl. {image_kb} KiB avatar) vs 2-column read");
    println!(
        "| {:>5} | {:<7} | {:>10} | {:>9} | {:>9} |",
        "conc", "mode", "ops/s", "p50 µs", "p99 µs"
    );
    for &conc in &concurrencies {
        for mode in ["FULL", "NARROW"] {
            let deadline = Instant::now() + Duration::from_secs(secs);
            let mut handles = Vec::new();
            for _ in 0..conc {
                let pool = pool.clone();
                let mode = mode.to_string();
                handles.push(tokio::spawn(async move {
                    let mut lats = Vec::new();
                    while Instant::now() < deadline {
                        let t = Instant::now();
                        if mode == "FULL" {
                            one_op_full(&pool, user_id).await;
                        } else {
                            one_op_narrow(&pool, user_id).await;
                        }
                        lats.push(t.elapsed().as_secs_f64() * 1e6);
                    }
                    lats
                }));
            }
            let mut all = Vec::new();
            for h in handles {
                all.extend(h.await.unwrap());
            }
            let s = summarize(all, secs);
            println!(
                "| {:>5} | {:<7} | {:>10.0} | {:>9.1} | {:>9.1} |",
                conc, mode, s.rps, s.p50, s.p99
            );
        }
    }
    println!("\n(SKIP: PROPFINDs not naming quota props now issue NEITHER query — 0 round-trips.)");

    cleanup(&pool, user_id).await;
}
