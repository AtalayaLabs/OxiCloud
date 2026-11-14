//! `Drive::is_empty` benchmark — full-drive `COUNT(*)` sum vs short-circuit
//! `EXISTS OR EXISTS`.
//!
//! The drive-deletion precheck only needs a boolean, but the old query
//! aggregated every live folder AND file in the drive (two full index/heap
//! scans) to compare the sum with 0. `EXISTS` stops at the first matching
//! row, so a populated drive answers from one probe.
//!
//! Both query shapes run against the same seeded data; the equivalence
//! gate asserts identical booleans for a populated and an empty drive.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_drive_is_empty
//! Tunables (env): BENCH_FILES (100000), BENCH_REPS (25)

use std::env;
use std::time::Instant;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn seed_drive(pool: &PgPool, files: usize) -> Uuid {
    // Drive + root folder must commit together (deferred root-folder trigger).
    let mut tx = pool.begin().await.expect("begin");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, quota_bytes) VALUES ('shared', NULL) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("drive");
    let root: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('bench_is_empty', '/bench_is_empty', 'bench_is_empty', $1)
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

    if files > 0 {
        sqlx::query(
            "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
             SELECT 'f' || i, $1,
                    'benchempty00000000000000000000000000000000000000000000000000000',
                    1024, 'image/jpeg', $2
               FROM generate_series(1, $3) AS i",
        )
        .bind(root)
        .bind(drive_id)
        .bind(files as i32)
        .execute(pool)
        .await
        .expect("seed files");
    }
    drive_id
}

async fn cleanup(pool: &PgPool, drive_id: Uuid) {
    sqlx::query("DELETE FROM storage.files WHERE drive_id = $1")
        .bind(drive_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("UPDATE storage.drives SET root_folder_id = NULL WHERE id = $1")
        .bind(drive_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM storage.folders WHERE drive_id = $1")
        .bind(drive_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(drive_id)
        .execute(pool)
        .await
        .ok();
}

/// BEFORE — verbatim old query shape.
async fn is_empty_count(pool: &PgPool, drive_id: Uuid) -> bool {
    let count: (i64,) = sqlx::query_as(
        r#"
        SELECT (
            (SELECT COUNT(*) FROM storage.folders
              WHERE drive_id = $1 AND parent_id IS NOT NULL AND NOT is_trashed)
          + (SELECT COUNT(*) FROM storage.files
              WHERE drive_id = $1 AND NOT is_trashed)
        )
        "#,
    )
    .bind(drive_id)
    .fetch_one(pool)
    .await
    .expect("count query");
    count.0 == 0
}

/// AFTER — the production EXISTS shape.
async fn is_empty_exists(pool: &PgPool, drive_id: Uuid) -> bool {
    let occupied: (bool,) = sqlx::query_as(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM storage.folders
             WHERE drive_id = $1 AND parent_id IS NOT NULL AND NOT is_trashed)
          OR EXISTS(
            SELECT 1 FROM storage.files
             WHERE drive_id = $1 AND NOT is_trashed)
        "#,
    )
    .bind(drive_id)
    .fetch_one(pool)
    .await
    .expect("exists query");
    !occupied.0
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").expect("set DATABASE_URL — the dev Postgres URL");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect");

    let files: usize = env_or("BENCH_FILES", 100_000);
    let reps: usize = env_or("BENCH_REPS", 25);

    let populated = seed_drive(&pool, files).await;
    let empty = seed_drive(&pool, 0).await;

    // Equivalence gate on both data shapes.
    assert_eq!(
        is_empty_count(&pool, populated).await,
        is_empty_exists(&pool, populated).await,
        "populated drive verdict differs"
    );
    assert_eq!(
        is_empty_count(&pool, empty).await,
        is_empty_exists(&pool, empty).await,
        "empty drive verdict differs"
    );
    assert!(!is_empty_exists(&pool, populated).await);
    assert!(is_empty_exists(&pool, empty).await);
    println!("# equivalence gate: identical booleans on populated + empty drives — OK");

    // Warm both shapes.
    for _ in 0..3 {
        is_empty_count(&pool, populated).await;
        is_empty_exists(&pool, populated).await;
    }

    let mut rows = Vec::new();
    for (label, drive) in [("populated (100k files)", populated), ("empty", empty)] {
        let t = Instant::now();
        for _ in 0..reps {
            std::hint::black_box(is_empty_count(&pool, drive).await);
        }
        let before_ms = t.elapsed().as_secs_f64() * 1e3 / reps as f64;

        let t = Instant::now();
        for _ in 0..reps {
            std::hint::black_box(is_empty_exists(&pool, drive).await);
        }
        let after_ms = t.elapsed().as_secs_f64() * 1e3 / reps as f64;
        rows.push((label, before_ms, after_ms));
    }

    println!("\n#################################################################");
    println!("# Drive::is_empty — COUNT(*) sum vs EXISTS OR EXISTS");
    println!("# files={files} reps={reps} (ms per call)");
    println!("#################################################################\n");
    println!(
        "| {:<24} | {:>14} | {:>14} | {:>8} |",
        "drive", "BEFORE ms", "AFTER ms", "speedup"
    );
    let mut populated_gain = 0.0;
    for (label, before_ms, after_ms) in &rows {
        println!(
            "| {:<24} | {:>14.3} | {:>14.3} | {:>7.1}x |",
            label,
            before_ms,
            after_ms,
            before_ms / after_ms
        );
        if label.starts_with("populated") {
            populated_gain = before_ms / after_ms;
        }
    }

    cleanup(&pool, populated).await;
    cleanup(&pool, empty).await;

    if populated_gain <= 1.0 {
        eprintln!("\nGATE FAIL: EXISTS not faster on the populated drive — rollback");
        std::process::exit(1);
    }
    println!("\nGATE PASS: identical verdicts, populated drive {populated_gain:.1}x faster.");
}
