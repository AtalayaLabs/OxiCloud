//! NC chroot / default-drive resolution benchmark — 2 queries/request vs moka.
//!
//! The NextCloud basic-auth middleware wraps EVERY protected NC route and,
//! even with app-password verification fully cached, used to resolve the
//! chroot from scratch per request:
//!
//!   1. `find_default_for_user`  — drives JOIN folders  (drive_pg_repository)
//!   2. `get_folder(root_id)`    — folders by PK
//!
//! The native `/webdav` surface repeats query 1 per request (Mode-B scope
//! resolution), WOPI repeats it per call. The change memoises (1) inside
//! `DrivePgRepository` and (2) in the middleware's `NC_CHROOT_CACHE`
//! (both 30 s TTL). This bench isolates exactly that: the per-request DB
//! cost of the chroot resolution — the two production query shapes vs a
//! moka hit — under sync-storm concurrency against the real pool.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_chroot_cache
//! Tunables (env): BENCH_POOL (20), BENCH_SECONDS (4), BENCH_CONCURRENCIES ("8,64").

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Seeded {
    user_id: Uuid,
}

async fn seed(pool: &PgPool) -> Seeded {
    // user → (drive + root folder + root_folder_id stamp) in one tx —
    // trg_no_orphan_root_folder is INITIALLY DEFERRED and checks at commit.
    let mut tx = pool.begin().await.expect("begin");
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_chroot', 'bench_chroot@bench.invalid', 'user')
         RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed user");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, default_for_user) VALUES ('personal', $1) RETURNING id",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    .expect("seed drive");
    let folder_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('Personal', '/Personal', 'Personal', $1) RETURNING id",
    )
    .bind(drive_id)
    .fetch_one(&mut *tx)
    .await
    .expect("seed folder");
    sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
        .bind(folder_id)
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .expect("stamp root");
    tx.commit().await.expect("commit");
    Seeded { user_id }
}

async fn cleanup(pool: &PgPool, user_id: Uuid) {
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
}

/// The exact production BEFORE: both chroot queries, sequentially (the
/// middleware awaits the drive row to learn root_folder_id first).
async fn one_op_before(pool: &PgPool, user_id: Uuid, queries: &AtomicUsize) {
    let row = sqlx::query(
        r#"
        SELECT d.id, d.kind, d.default_for_user, d.root_folder_id,
               d.quota_bytes, d.used_bytes, d.policies,
               d.created_at, d.updated_at,
               f.name AS root_folder_name
          FROM storage.drives d
          JOIN storage.folders f ON f.id = d.root_folder_id
         WHERE d.default_for_user = $1
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("drive query");
    let root_id: Uuid = row.get("root_folder_id");

    let _folder = sqlx::query(
        "SELECT id, name, parent_id, path, created_at, updated_at
           FROM storage.folders WHERE id = $1",
    )
    .bind(root_id)
    .fetch_one(pool)
    .await
    .expect("folder query");
    queries.fetch_add(2, Ordering::Relaxed);
}

#[derive(Clone)]
#[allow(dead_code)]
struct ChrootValue {
    root_id: Uuid,
    name: String,
    path: String,
}

struct Stats {
    rps: f64,
    p50: f64,
    p95: f64,
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
        p95: pct(0.95),
        p99: pct(0.99),
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");

    let pool_size: u32 = env_or("BENCH_POOL", 20);
    let secs: u64 = env_or("BENCH_SECONDS", 4);
    let concurrencies: Vec<usize> = env::var("BENCH_CONCURRENCIES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![8, 64]);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(pool_size)
            .min_connections(pool_size)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect Postgres"),
    );

    let seeded = seed(&pool).await;
    let user_id = seeded.user_id;

    // AFTER: what the middleware pays on a warm cache — a moka lookup.
    let cache: moka::sync::Cache<Uuid, ChrootValue> = moka::sync::Cache::builder()
        .max_capacity(100_000)
        .time_to_live(Duration::from_secs(30))
        .build();
    cache.insert(
        user_id,
        ChrootValue {
            root_id: Uuid::new_v4(),
            name: "Personal".into(),
            path: "/Personal".into(),
        },
    );

    println!("\n#############################################################");
    println!("# NC chroot resolution: BEFORE (2 queries/req) vs AFTER (moka)");
    println!("# pool={pool_size} window={secs}s/run");
    println!("#############################################################\n");
    println!(
        "| {:>5} | {:<6} | {:>10} | {:>9} | {:>9} | {:>9} | {:>9} |",
        "conc", "mode", "req/s", "p50 µs", "p95 µs", "p99 µs", "queries"
    );

    for &conc in &concurrencies {
        for mode in ["BEFORE", "AFTER"] {
            let queries = Arc::new(AtomicUsize::new(0));
            let deadline = Instant::now() + Duration::from_secs(secs);
            let mut handles = Vec::new();
            for _ in 0..conc {
                let pool = pool.clone();
                let cache = cache.clone();
                let queries = queries.clone();
                let mode = mode.to_string();
                handles.push(tokio::spawn(async move {
                    let mut lats = Vec::new();
                    while Instant::now() < deadline {
                        let t = Instant::now();
                        if mode == "BEFORE" {
                            one_op_before(&pool, user_id, &queries).await;
                        } else {
                            let v = cache.get(&user_id).expect("warm cache");
                            std::hint::black_box(v);
                        }
                        lats.push(t.elapsed().as_secs_f64() * 1_000_000.0);
                        if mode == "AFTER" {
                            // moka hit is ~100 ns; yield so the loop doesn't
                            // monopolise workers and skew the run count.
                            tokio::task::yield_now().await;
                        }
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
                "| {:>5} | {:<6} | {:>10.0} | {:>9.2} | {:>9.2} | {:>9.2} | {:>9} |",
                conc,
                mode,
                s.rps,
                s.p50,
                s.p95,
                s.p99,
                queries.load(Ordering::Relaxed)
            );
        }
    }

    cleanup(&pool, user_id).await;
    println!("\n(BEFORE = the two production chroot queries; AFTER = warm moka hit.");
    println!(" Every NC request pays this before its handler runs.)");
}
