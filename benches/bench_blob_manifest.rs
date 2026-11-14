//! Blob-manifest read benchmark — `read_blob_bytes` manifest round-trips.
//!
//! `read_blob_bytes` used to read the same `storage.chunk_manifests` PK row
//! TWICE per full-blob read: once via `blob_size` (`SELECT total_size`) and once
//! via `read_blob_stream` (`SELECT chunk_hashes`). On the thumbnail cold path
//! that is 2N manifest queries for an N-image gallery load. The change folds both
//! into ONE query (`SELECT chunk_hashes, total_size`).
//!
//! This isolates exactly that change — the two manifest lookups vs the one — and
//! leaves the (unchanged) chunk streaming out, so the signal is the DB
//! round-trip(s) per blob read. Runs OLD (2 queries) vs NEW (1 query) against the
//! real dev Postgres, at low contention (raw per-op latency) and high contention
//! (concurrency > pool, where holding a connection ~2× longer inflates the tail).
//!
//! Run (needs the dev Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_blob_manifest
//! Tunables (env): BENCH_POOL (20), BENCH_SECONDS (4), BENCH_CHUNKS (64),
//!   BENCH_CONCURRENCIES ("4,64").

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Synthetic manifest key: exactly 64 chars (the VARCHAR(64) PK width), and the
/// non-hex letters ('n','s','h') guarantee it can never collide with a real
/// BLAKE3 blob hash (always lowercase hex). 8 × "bench000".
const FILE_HASH: &str = "bench000bench000bench000bench000bench000bench000bench000bench000";

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Clone, Copy)]
enum Mode {
    /// Two separate manifest lookups (the old blob_size + read_blob_stream).
    Old,
    /// One combined manifest lookup (the new read_blob_bytes).
    New,
}

async fn seed(pool: &PgPool, n_chunks: usize) {
    let chunk_hashes: Vec<String> = (0..n_chunks).map(|i| format!("{i:064x}")).collect();
    let chunk_sizes: Vec<i64> = vec![65_536; n_chunks];
    let total: i64 = chunk_sizes.iter().sum();
    sqlx::query(
        "INSERT INTO storage.chunk_manifests
             (file_hash, chunk_hashes, chunk_sizes, total_size, chunk_count)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (file_hash) DO UPDATE
             SET chunk_hashes = $2, chunk_sizes = $3, total_size = $4, chunk_count = $5",
    )
    .bind(FILE_HASH)
    .bind(&chunk_hashes)
    .bind(&chunk_sizes)
    .bind(total)
    .bind(n_chunks as i32)
    .execute(pool)
    .await
    .expect("seed chunk_manifests row");
}

async fn cleanup(pool: &PgPool) {
    let _ = sqlx::query("DELETE FROM storage.chunk_manifests WHERE file_hash = $1")
        .bind(FILE_HASH)
        .execute(pool)
        .await;
}

/// One blob "manifest read" — exactly the queries the production code issues.
async fn one_op(pool: &PgPool, mode: Mode) {
    match mode {
        Mode::Old => {
            let _total: i64 = sqlx::query_scalar(
                "SELECT total_size FROM storage.chunk_manifests WHERE file_hash = $1",
            )
            .bind(FILE_HASH)
            .fetch_one(pool)
            .await
            .expect("old total_size query");
            let _chunks: Vec<String> = sqlx::query_scalar(
                "SELECT chunk_hashes FROM storage.chunk_manifests WHERE file_hash = $1",
            )
            .bind(FILE_HASH)
            .fetch_one(pool)
            .await
            .expect("old chunk_hashes query");
        }
        Mode::New => {
            let _row: (Vec<String>, i64) = sqlx::query_as(
                "SELECT chunk_hashes, total_size FROM storage.chunk_manifests WHERE file_hash = $1",
            )
            .bind(FILE_HASH)
            .fetch_one(pool)
            .await
            .expect("new combined query");
        }
    }
}

struct Stats {
    count: usize,
    rps: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
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
        count: n,
        rps: n as f64 / secs as f64,
        p50: pct(0.50),
        p95: pct(0.95),
        p99: pct(0.99),
        max: lats.last().copied().unwrap_or(0.0),
    }
}

async fn run_window(pool: Arc<PgPool>, concurrency: usize, secs: u64, mode: Mode) -> Stats {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let mut lats = Vec::new();
            while Instant::now() < deadline {
                let t = Instant::now();
                one_op(&pool, mode).await;
                lats.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            lats
        }));
    }
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.await.unwrap());
    }
    summarize(all, secs)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL (or OXICLOUD_DB_CONNECTION_STRING) — the dev Postgres URL");

    let pool_size: u32 = env_or("BENCH_POOL", 20);
    let secs: u64 = env_or("BENCH_SECONDS", 4);
    let n_chunks: usize = env_or("BENCH_CHUNKS", 64);
    let concurrencies: Vec<usize> = env::var("BENCH_CONCURRENCIES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![4, 64]);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(pool_size)
            .min_connections(pool_size) // pre-warm: don't time connection setup
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect dev Postgres"),
    );

    seed(&pool, n_chunks).await;

    println!("\n###########################################################");
    println!("# read_blob_bytes manifest round-trips: OLD (2 queries) vs NEW (1)");
    println!("# pool={pool_size}  window={secs}s/run  chunks/manifest={n_chunks}");
    println!("# latency = acquire-wait + manifest query/queries per blob read");
    println!("###########################################################\n");
    println!(
        "| {:>5} | {:<4} | {:>9} | {:>9} | {:>7} | {:>7} | {:>7} | {:>7} |",
        "conc", "mode", "ops", "ops/s", "p50 ms", "p95 ms", "p99 ms", "max ms"
    );
    println!(
        "|{:-<7}|{:-<6}|{:-<11}|{:-<11}|{:-<9}|{:-<9}|{:-<9}|{:-<9}|",
        "", "", "", "", "", "", "", ""
    );

    for &conc in &concurrencies {
        // Warm-up (discarded) so the first real window isn't skewed.
        let _ = run_window(pool.clone(), conc, 1, Mode::New).await;

        let old = run_window(pool.clone(), conc, secs, Mode::Old).await;
        let new = run_window(pool.clone(), conc, secs, Mode::New).await;
        let row = |label: &str, s: &Stats| {
            println!(
                "| {:>5} | {:<4} | {:>9} | {:>9.0} | {:>7.3} | {:>7.3} | {:>7.3} | {:>7.3} |",
                conc, label, s.count, s.rps, s.p50, s.p95, s.p99, s.max
            );
        };
        row("OLD", &old);
        row("NEW", &new);
        let thr = if old.rps > 0.0 {
            new.rps / old.rps
        } else {
            0.0
        };
        let p99 = if new.p99 > 0.0 {
            old.p99 / new.p99
        } else {
            0.0
        };
        println!(
            "|       | →    | {:>9} | {:>7.2}× | {:>7} | {:>7} | {:>6.2}× | {:>7} |",
            "throughput", thr, "", "", p99, ""
        );
    }

    cleanup(&pool).await;
    println!("\n(ops = blob-manifest reads completed; NEW issues 1 query/op, OLD issues 2.)");
}
