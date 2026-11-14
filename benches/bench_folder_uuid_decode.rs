//! Folder-listing UUID decode benchmark — `id::text`/`parent_id::text`
//! server casts vs binary `Uuid` decode + one app-side render.
//!
//! Round 6 adopted binary decode for the FILE listing rows
//! (`row_to_file`, benches/ROUND6.md §10: 1.17x on 500-row pages) and
//! queued "other repos with the same shape" — `FolderDbRepository` never
//! got the port. Its rows (`list_folders`, `list_folders_batch` — every
//! Depth:1 PROPFIND subfolder page — descendants, suggest) still shipped
//! two `::text` casts per row: 36+36 B on the wire instead of 16+16 and
//! a server-side cast per column.
//!
//! Same methodology as `bench_uuid_text_cast` (the round-6 A/B this
//! ports): seeded page, equivalence gate on identical `(id, parent_id,
//! name, path)` string tuples, warm-up, interleaved passes.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_folder_uuid_decode
//! Tunables (env): BENCH_ROWS (500), BENCH_PASSES (200)

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

struct Seeded {
    drive_id: Uuid,
    parent_id: Uuid,
}

async fn seed(pool: &PgPool, rows: usize) -> Seeded {
    let mut tx = pool.begin().await.expect("begin");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, quota_bytes) VALUES ('shared', NULL) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("drive");
    let root: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('bench_uuid_folders', '/bench_uuid_folders', 'bench_uuid_folders', $1)
         RETURNING id",
    )
    .bind(drive_id)
    .fetch_one(&mut *tx)
    .await
    .expect("root");
    sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
        .bind(root)
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .expect("stamp root");
    tx.commit().await.expect("commit");

    sqlx::query(
        "INSERT INTO storage.folders (name, parent_id, path, lpath, drive_id)
         SELECT 'sub' || i, $1, '/bench_uuid_folders/sub' || i,
                ('bench_uuid_folders.sub' || i)::ltree, $2
           FROM generate_series(1, $3) AS i",
    )
    .bind(root)
    .bind(drive_id)
    .bind(rows as i32)
    .execute(pool)
    .await
    .expect("seed subfolders");

    Seeded {
        drive_id,
        parent_id: root,
    }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
    sqlx::query("DELETE FROM storage.folders WHERE drive_id = $1 AND parent_id IS NOT NULL")
        .bind(s.drive_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("UPDATE storage.drives SET root_folder_id = NULL WHERE id = $1")
        .bind(s.drive_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM storage.folders WHERE drive_id = $1")
        .bind(s.drive_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(s.drive_id)
        .execute(pool)
        .await
        .ok();
}

/// Materialized tuple both arms must produce identically.
type FolderTuple = (String, String, String, Option<String>);

/// BEFORE — verbatim old query shape: two server-side `::text` casts,
/// decode as String.
async fn fetch_text_cast(pool: &PgPool, parent_id: Uuid) -> Vec<FolderTuple> {
    sqlx::query_as::<_, (String, String, String, Option<String>)>(
        r#"
        SELECT id::text, name, path, parent_id::text
          FROM storage.folders
         WHERE parent_id = $1 AND NOT is_trashed
         ORDER BY name
        "#,
    )
    .bind(parent_id)
    .fetch_all(pool)
    .await
    .expect("text-cast fetch")
}

/// AFTER — the production shape: binary decode, one `to_string` app-side
/// (exactly what `row_to_folder` does now).
async fn fetch_binary_uuid(pool: &PgPool, parent_id: Uuid) -> Vec<FolderTuple> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, Option<Uuid>)>(
        r#"
        SELECT id, name, path, parent_id
          FROM storage.folders
         WHERE parent_id = $1 AND NOT is_trashed
         ORDER BY name
        "#,
    )
    .bind(parent_id)
    .fetch_all(pool)
    .await
    .expect("binary fetch");
    rows.into_iter()
        .map(|(id, name, path, pid)| (id.to_string(), name, path, pid.map(|u| u.to_string())))
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

    // Equivalence gate: identical string tuples in identical order.
    let a = fetch_text_cast(&pool, seeded.parent_id).await;
    let b = fetch_binary_uuid(&pool, seeded.parent_id).await;
    if a != b || a.len() != rows {
        eprintln!(
            "EQUIVALENCE GATE FAILED: rows differ (a={}, b={})",
            a.len(),
            b.len()
        );
        cleanup(&pool, &seeded).await;
        std::process::exit(1);
    }
    println!("# equivalence gate: {rows} identical (id, name, path, parent_id) tuples — OK");

    for _ in 0..10 {
        std::hint::black_box(fetch_text_cast(&pool, seeded.parent_id).await);
        std::hint::black_box(fetch_binary_uuid(&pool, seeded.parent_id).await);
    }

    // Interleaved A/B passes so drift (autovacuum, CPU governor) hits both.
    let mut lat_a = Vec::with_capacity(passes);
    let mut lat_b = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(fetch_text_cast(&pool, seeded.parent_id).await);
        lat_a.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        std::hint::black_box(fetch_binary_uuid(&pool, seeded.parent_id).await);
        lat_b.push(t.elapsed().as_secs_f64() * 1e3);
    }

    let sa = summarize(lat_a);
    let sb = summarize(lat_b);

    println!("\n#################################################################");
    println!("# folder page: `::text` casts vs binary UUID decode + app fmt");
    println!("# rows/page={rows} passes={passes} (interleaved)");
    println!("#################################################################\n");
    println!(
        "| {:<22} | {:>9} | {:>9} | {:>9} |",
        "arm", "mean ms", "p50 ms", "p95 ms"
    );
    println!(
        "| {:<22} | {:>9.3} | {:>9.3} | {:>9.3} |",
        "A ::text (before)", sa.mean_ms, sa.p50_ms, sa.p95_ms
    );
    println!(
        "| {:<22} | {:>9.3} | {:>9.3} | {:>9.3} |",
        "B binary (after)", sb.mean_ms, sb.p50_ms, sb.p95_ms
    );
    println!(
        "\nB/A mean ratio: {:.3} ({:.2}x)",
        sb.mean_ms / sa.mean_ms,
        sa.mean_ms / sb.mean_ms
    );

    cleanup(&pool, &seeded).await;

    if sb.mean_ms >= sa.mean_ms {
        eprintln!("GATE FAIL: binary decode not faster than ::text — rollback");
        std::process::exit(1);
    }
    println!("GATE PASS");
}
