//! DB connection-pool tail-latency benchmark.
//!
//! Isolates the variable under test — `max_connections` — from the HTTP/auth
//! stack. Builds a real `sqlx` Postgres pool of size P and drives it with `C`
//! concurrent workers, each looping `SELECT pg_sleep($query_ms)` (a query of
//! known duration). The measured per-request latency is **acquire-wait + query**
//! — exactly the pool-exhaustion mechanism: when in-flight queries exceed P, the
//! surplus queues on `acquire()`, inflating p95/p99.
//!
//! `pg_sleep` is a faithful stand-in for "a query that occupies a connection for
//! T ms" — real listing/auth queries take a few ms each. We hold P constant per
//! run and sweep it, so the *shape* of tail-latency-vs-pool-size is what matters.
//!
//! Run (needs the dev Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_db_pool
//! Tunables (env): BENCH_CONCURRENCY (default 96), BENCH_QUERY_MS (3),
//!   BENCH_SECONDS (4), BENCH_POOL_SIZES ("10,20,40,70").

use std::env;
use std::time::{Duration, Instant};

use sqlx::postgres::PgPoolOptions;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL (or OXICLOUD_DB_CONNECTION_STRING) — the dev Postgres URL");

    let concurrency: usize = env_or("BENCH_CONCURRENCY", 96);
    let query_ms: u64 = env_or("BENCH_QUERY_MS", 3);
    let secs: u64 = env_or("BENCH_SECONDS", 4);
    let pool_sizes: Vec<u32> = env::var("BENCH_POOL_SIZES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![10, 20, 40, 70]);

    println!("\n###########################################################");
    println!("# DB pool tail-latency benchmark");
    println!("# concurrency (in-flight requests): {concurrency}");
    println!("# query duration: pg_sleep({query_ms} ms)   window: {secs}s/pool");
    println!("# latency = acquire-wait + query (the pool-queue effect)");
    println!("###########################################################\n");
    println!(
        "| {:>4} | {:>9} | {:>10} | {:>8} | {:>8} | {:>8} | {:>9} | {:>6} |",
        "pool", "requests", "req/s", "p50 ms", "p95 ms", "p99 ms", "max ms", "errors"
    );
    println!(
        "|{:-<6}|{:-<11}|{:-<12}|{:-<10}|{:-<10}|{:-<10}|{:-<11}|{:-<8}|",
        "", "", "", "", "", "", "", ""
    );

    let qsec = query_ms as f64 / 1000.0;

    for &pool_size in &pool_sizes {
        let pool = PgPoolOptions::new()
            .max_connections(pool_size)
            .min_connections(pool_size) // pre-warm so we don't time connection setup
            .acquire_timeout(Duration::from_secs(10)) // matches prod connect_timeout default
            .connect(&url)
            .await
            .unwrap_or_else(|e| panic!("connect pool={pool_size}: {e}"));

        // Warm-up burst (discarded).
        {
            let mut warm = Vec::new();
            for _ in 0..concurrency {
                let pool = pool.clone();
                warm.push(tokio::spawn(async move {
                    let _ = sqlx::query("SELECT pg_sleep($1)")
                        .bind(qsec)
                        .execute(&pool)
                        .await;
                }));
            }
            for h in warm {
                let _ = h.await;
            }
        }

        let start = Instant::now();
        let deadline = start + Duration::from_secs(secs);
        let mut handles = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                let mut lats_us: Vec<u32> = Vec::with_capacity(8192);
                let mut errors: u64 = 0;
                while Instant::now() < deadline {
                    let t = Instant::now();
                    match sqlx::query("SELECT pg_sleep($1)")
                        .bind(qsec)
                        .execute(&pool)
                        .await
                    {
                        Ok(_) => lats_us.push(t.elapsed().as_micros() as u32),
                        Err(_) => errors += 1,
                    }
                }
                (lats_us, errors)
            }));
        }

        let mut all: Vec<u32> = Vec::new();
        let mut errors: u64 = 0;
        for h in handles {
            let (l, e) = h.await.expect("join worker");
            all.extend(l);
            errors += e;
        }
        let elapsed = start.elapsed().as_secs_f64();
        pool.close().await;

        all.sort_unstable();
        let n = all.len();
        let pct = |q: f64| -> f64 {
            if n == 0 {
                return 0.0;
            }
            let idx = ((q / 100.0) * (n as f64 - 1.0)).round() as usize;
            all[idx.min(n - 1)] as f64 / 1000.0
        };
        let tput = n as f64 / elapsed;

        println!(
            "| {:>4} | {:>9} | {:>10.0} | {:>8.2} | {:>8.2} | {:>8.2} | {:>9.2} | {:>6} |",
            pool_size,
            n,
            tput,
            pct(50.0),
            pct(95.0),
            pct(99.0),
            pct(100.0),
            errors,
        );
    }

    println!(
        "\nNote: pg_sleep models query DURATION (connection occupancy), not CPU.\n\
         At fixed concurrency, raising the pool cuts queue-wait until pool ≈ concurrency,\n\
         then plateaus — the tail-latency shape that tells you the right size for your load.\n"
    );
}
