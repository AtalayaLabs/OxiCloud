//! ACL owner-cache benchmark.
//!
//! Models the owner short-circuit in `PgAclEngine::check` (the common case: a
//! user touching their own files). Before the cache, every authorization check
//! on a folder/file ran one PK query `SELECT user_id FROM storage.folders WHERE
//! id=$1` — a DB round-trip that also occupies a pool connection. After, repeat
//! checks for the same resource hit an in-memory moka cache (owner is immutable).
//!
//! This isolates that exact query vs a moka hit, under concurrency C, against the
//! real dev Postgres. It shows both the latency win and — by NOT touching the
//! pool — the relief it gives the connection pool (ties into the pool benchmark).
//!
//! Run (needs the dev Postgres up with at least one folder; reads DATABASE_URL):
//!   cargo bench --features=bench --bench bench_owner_cache
//! Tunables: BENCH_CONCURRENCY (64), BENCH_POOL_SIZE (20 = prod default),
//!   BENCH_SECONDS (4).

use std::env;
use std::time::{Duration, Instant};

use moka::future::Cache;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Stats {
    reqs: u64,
    tput: f64,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    max_us: f64,
}

fn summarize(mut lats_ns: Vec<u64>, elapsed: f64) -> Stats {
    lats_ns.sort_unstable();
    let n = lats_ns.len();
    let pct = |q: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        let idx = ((q / 100.0) * (n as f64 - 1.0)).round() as usize;
        lats_ns[idx.min(n - 1)] as f64 / 1000.0
    };
    Stats {
        reqs: n as u64,
        tput: n as f64 / elapsed,
        p50_us: pct(50.0),
        p95_us: pct(95.0),
        p99_us: pct(99.0),
        max_us: pct(100.0),
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let concurrency: usize = env_or("BENCH_CONCURRENCY", 64);
    let pool_size: u32 = env_or("BENCH_POOL_SIZE", 20); // production default
    let secs: u64 = env_or("BENCH_SECONDS", 4);

    let pool = PgPoolOptions::new()
        .max_connections(pool_size)
        .min_connections(pool_size)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await
        .expect("connect dev Postgres");

    // A real folder + its owner to check against.
    let Some(row) = sqlx::query("SELECT id, user_id FROM storage.folders LIMIT 1")
        .fetch_optional(&pool)
        .await
        .expect("query folder")
    else {
        eprintln!("No folders in the dev DB — seed some first (`just load-seed`).");
        return;
    };
    let folder_id: Uuid = row.get("id");
    let owner_id: Uuid = row.get("user_id");

    println!("\n###########################################################");
    println!("# ACL owner-cache benchmark (owner short-circuit path)");
    println!("# concurrency: {concurrency}   pool: {pool_size}   window: {secs}s/mode");
    println!("# folder {folder_id}  owner {owner_id}");
    println!("###########################################################\n");

    // ── BEFORE: one PK owner query per check (hits DB + pool) ──────────────
    let uncached = {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(secs);
        let mut handles = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                let mut lats = Vec::with_capacity(16384);
                while Instant::now() < deadline {
                    let t = Instant::now();
                    let _: Uuid =
                        sqlx::query_scalar("SELECT user_id FROM storage.folders WHERE id = $1")
                            .bind(folder_id)
                            .fetch_one(&pool)
                            .await
                            .expect("owner query");
                    lats.push(t.elapsed().as_nanos() as u64);
                }
                lats
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.await.expect("join"));
        }
        let s = summarize(all, start.elapsed().as_secs_f64());
        (s.reqs, s) // reqs == DB queries
    };

    // ── AFTER: moka hit per check (no DB, no pool) ────────────────────────
    let cache: Cache<Uuid, Uuid> = Cache::builder()
        .max_capacity(100_000)
        .time_to_live(Duration::from_secs(300))
        .build();
    cache.insert(folder_id, owner_id).await; // 1 warm-up "query"
    let cached = {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(secs);
        let mut handles = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let cache = cache.clone();
            handles.push(tokio::spawn(async move {
                let mut lats = Vec::with_capacity(65536);
                while Instant::now() < deadline {
                    let t = Instant::now();
                    let owner = cache.get(&folder_id).await.expect("cache hit");
                    std::hint::black_box(owner);
                    lats.push(t.elapsed().as_nanos() as u64);
                }
                lats
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.await.expect("join"));
        }
        summarize(all, start.elapsed().as_secs_f64())
    };
    pool.close().await;

    let (uncached_queries, a) = uncached;
    println!(
        "| {:<16} | {:>10} | {:>11} | {:>9} | {:>9} | {:>9} | {:>10} |",
        "mode", "DB queries", "checks/s", "p50 µs", "p95 µs", "p99 µs", "max µs"
    );
    println!(
        "|{:-<18}|{:-<12}|{:-<13}|{:-<11}|{:-<11}|{:-<11}|{:-<12}|",
        "", "", "", "", "", "", ""
    );
    println!(
        "| {:<16} | {:>10} | {:>11.0} | {:>9.1} | {:>9.1} | {:>9.1} | {:>10.1} |",
        "uncached (before)", uncached_queries, a.tput, a.p50_us, a.p95_us, a.p99_us, a.max_us
    );
    println!(
        "| {:<16} | {:>10} | {:>11.0} | {:>9.3} | {:>9.3} | {:>9.3} | {:>10.3} |",
        "cached (after)",
        1,
        cached.tput,
        cached.p50_us,
        cached.p95_us,
        cached.p99_us,
        cached.max_us
    );

    println!(
        "\nPer authorized action on an owned resource, the cache removes 1 DB query\n\
         + 1 pool-connection occupancy, turning a {:.0} µs round-trip into a {:.3} µs\n\
         memory hit ({:.0}× lower p99). Over a network/loaded DB the absolute saving is\n\
         larger; the freed connections directly relieve the pool (see DB-POOL.md).\n",
        a.p99_us,
        cached.p99_us,
        if cached.p99_us > 0.0 {
            a.p99_us / cached.p99_us
        } else {
            0.0
        }
    );
}
