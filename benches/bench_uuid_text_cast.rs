//! A/B: `id::text` server-side casts vs binary UUID decode + app-side format.
//!
//! `file_blob_read_repository.rs` (and friends) SELECT UUID columns as
//! `id::text` and decode `String`s directly. The alternative is to decode the
//! wire-native binary `Uuid` (16 bytes vs 36 on the wire) and render the
//! string app-side with `Uuid::to_string`. This bench decides ROUND6 task
//! "::text casts A/B" empirically: whichever loses is documented, only a
//! winner ships.
//!
//! Arms fetch the same 500-row page from a seeded `storage.files` subtree,
//! interleaved A/B to cancel drift; the equivalence gate asserts identical
//! `(id, folder_id, name)` string triples.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_uuid_text_cast
//! Tunables (env): BENCH_ROWS (500), BENCH_PASSES (200).

use std::env;
use std::sync::Arc;
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
    drive_id: Uuid,
    root_folder: Uuid,
    blob_hash: String,
}

async fn seed(pool: &PgPool, rows: usize) -> Seeded {
    let mut tx = pool.begin().await.expect("begin");
    let drive_id: Uuid =
        sqlx::query_scalar("INSERT INTO storage.drives (kind) VALUES ('shared') RETURNING id")
            .fetch_one(&mut *tx)
            .await
            .expect("seed drive");
    let root_folder: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('Bench Cast', '/Bench Cast', 'x', $1) RETURNING id",
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
    let blob_hash = "benchuuidcast000000000000000000000000000000000000000000000000b2".to_string();
    sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count) VALUES ($1, 1, 1)")
        .bind(&blob_hash)
        .execute(&mut *tx)
        .await
        .expect("seed blob");
    for i in 0..rows {
        sqlx::query(
            "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
             VALUES ($1, $2, $3, 1, 'text/plain', $4)",
        )
        .bind(format!("cast-{i:05}.txt"))
        .bind(root_folder)
        .bind(&blob_hash)
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .expect("seed file");
    }
    tx.commit().await.expect("commit");
    Seeded {
        drive_id,
        root_folder,
        blob_hash,
    }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
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
}

type Triple = (String, Option<String>, String);

/// Arm A — the current production shape: server-side `::text` casts.
async fn fetch_text_cast(pool: &PgPool, drive_id: Uuid) -> Vec<Triple> {
    sqlx::query(
        "SELECT id::text AS id, folder_id::text AS folder_id, name
           FROM storage.files WHERE drive_id = $1 ORDER BY name",
    )
    .bind(drive_id)
    .fetch_all(pool)
    .await
    .expect("text-cast fetch")
    .iter()
    .map(|r| {
        (
            r.get::<String, _>("id"),
            r.get::<Option<String>, _>("folder_id"),
            r.get::<String, _>("name"),
        )
    })
    .collect()
}

/// Arm B — binary `Uuid` decode + app-side `to_string`.
async fn fetch_binary_uuid(pool: &PgPool, drive_id: Uuid) -> Vec<Triple> {
    sqlx::query(
        "SELECT id, folder_id, name
           FROM storage.files WHERE drive_id = $1 ORDER BY name",
    )
    .bind(drive_id)
    .fetch_all(pool)
    .await
    .expect("binary fetch")
    .iter()
    .map(|r| {
        (
            r.get::<Uuid, _>("id").to_string(),
            r.get::<Option<Uuid>, _>("folder_id").map(|u| u.to_string()),
            r.get::<String, _>("name"),
        )
    })
    .collect()
}

struct Stats {
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
}

fn summarize(mut xs: Vec<f64>) -> Stats {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    Stats {
        mean_ms: xs.iter().sum::<f64>() / n as f64,
        p50_ms: xs[n / 2],
        p95_ms: xs[((n as f64 * 0.95) as usize).min(n - 1)],
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let rows: usize = env_or("BENCH_ROWS", 500);
    let passes: usize = env_or("BENCH_PASSES", 200);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(4)
            .min_connections(4)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect Postgres"),
    );

    let seeded = seed(&pool, rows).await;

    // ── Equivalence gate: identical string triples ───────────────────────
    let a = fetch_text_cast(&pool, seeded.drive_id).await;
    let b = fetch_binary_uuid(&pool, seeded.drive_id).await;
    if a != b || a.len() != rows {
        eprintln!(
            "EQUIVALENCE GATE FAILED: rows differ (a={}, b={})",
            a.len(),
            b.len()
        );
        cleanup(&pool, &seeded).await;
        std::process::exit(1);
    }

    // Warm-up both shapes (plan cache, buffer cache).
    for _ in 0..10 {
        std::hint::black_box(fetch_text_cast(&pool, seeded.drive_id).await);
        std::hint::black_box(fetch_binary_uuid(&pool, seeded.drive_id).await);
    }

    // Interleaved A/B passes so drift (autovacuum, CPU governor) hits both.
    let mut lat_a = Vec::with_capacity(passes);
    let mut lat_b = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(fetch_text_cast(&pool, seeded.drive_id).await);
        lat_a.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        std::hint::black_box(fetch_binary_uuid(&pool, seeded.drive_id).await);
        lat_b.push(t.elapsed().as_secs_f64() * 1e3);
    }

    let sa = summarize(lat_a);
    let sb = summarize(lat_b);

    println!("\n#################################################################");
    println!("# UUID columns: `id::text` server cast vs binary decode + app fmt");
    println!("# rows/page={rows} passes={passes} (interleaved)");
    println!("#################################################################\n");
    println!(
        "| {:<22} | {:>9} | {:>9} | {:>9} |",
        "arm", "mean ms", "p50 ms", "p95 ms"
    );
    println!(
        "| {:<22} | {:>9.3} | {:>9.3} | {:>9.3} |",
        "A ::text (current)", sa.mean_ms, sa.p50_ms, sa.p95_ms
    );
    println!(
        "| {:<22} | {:>9.3} | {:>9.3} | {:>9.3} |",
        "B binary + to_string", sb.mean_ms, sb.p50_ms, sb.p95_ms
    );
    println!(
        "\nB/A mean ratio: {:.3} ({})",
        sb.mean_ms / sa.mean_ms,
        if sb.mean_ms < sa.mean_ms {
            "binary decode wins"
        } else {
            "::text cast wins"
        }
    );

    cleanup(&pool, &seeded).await;
}
