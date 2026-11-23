//! WebDAV dead-properties fetch benchmark — per-child N+1 vs batched ANY($1).
//!
//! The streaming PROPFIND walker (`webdav_handler.rs`) fetches dead properties
//! ONE CHILD AT A TIME, sequentially, for every Depth:1 listing page:
//!
//!   for file in &batch { file_deads.push(store.get_all(File(id)).await) }
//!
//! and `DeadPropertyStore::get_all` filters with
//! `folder_id IS NOT DISTINCT FROM $1 AND file_id IS NOT DISTINCT FROM $2`,
//! which PostgreSQL cannot serve from a B-tree index (IS NOT DISTINCT FROM is
//! not an indexable operator) — so each of the N sequential round-trips also
//! degrades to a seq scan as the table grows.
//!
//! This bench isolates exactly the dead-prop portion of a Depth:1 PROPFIND of
//! a folder with N children, comparing the three query shapes:
//!
//!   OLD   — N sequential `IS NOT DISTINCT FROM` queries (production today)
//!   EQ    — N sequential plain `file_id = $1` queries (indexable, still N+1)
//!   BATCH — ⌈N/500⌉ `file_id = ANY($1)` queries (one per PROPFIND page)
//!
//! Two table sizes are measured: the seeded-children-only table and one with
//! extra noise rows (dead props on other resources), which is where the
//! seq-scan cost of OLD shows up.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_dead_props
//! Tunables (env): BENCH_CHILDREN (2000), BENCH_PAGE (500 = PROPFIND_BATCH_SIZE),
//!   BENCH_NOISE_ROWS (20000), BENCH_REPS (5).

use std::env;
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

struct Seeded {
    drive_id: Uuid,
    file_ids: Vec<Uuid>,
}

async fn seed(pool: &PgPool, children: usize, noise: usize) -> Seeded {
    // Drive (kind 'shared' needs no user FK) → root folder → N files → props.
    // The root folder + drive.root_folder_id must land in ONE transaction:
    // trg_no_orphan_root_folder is INITIALLY DEFERRED and checks at commit.
    let mut tx = pool.begin().await.expect("begin seed tx");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, quota_bytes) VALUES ('shared', NULL) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed drive");

    let folder_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('bench_dead_props', '/bench_dead_props', 'bench_dead_props', $1)
         RETURNING id",
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
        .expect("stamp root_folder_id");
    tx.commit().await.expect("commit seed tx");

    // Children of the PROPFIND'd folder, one dead prop each.
    let file_ids: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
         SELECT 'f' || i, $1, 'benchdead000000000000000000000000000000000000000000000000000000',
                1024, 'image/jpeg', $2
           FROM generate_series(1, $3) AS i
         RETURNING id",
    )
    .bind(folder_id)
    .bind(drive_id)
    .bind(children as i32)
    .fetch_all(pool)
    .await
    .expect("seed files");

    sqlx::query(
        "INSERT INTO storage.webdav_dead_properties (file_id, namespace, local_name, value)
         SELECT id, 'urn:bench', 'displayname', 'bench value'
           FROM storage.files WHERE folder_id = $1",
    )
    .bind(folder_id)
    .execute(pool)
    .await
    .expect("seed dead props");

    // Noise: dead props attached to OTHER files (a second folder) so the
    // table has realistic volume — this is what OLD's seq scans pay for.
    if noise > 0 {
        // Child of the main folder — root folders need the deferred
        // four-write dance, children don't.
        let noise_folder: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, path, lpath, drive_id)
             VALUES ('noise', $2, '/bench_dead_props/noise', 'bench_dead_props.noise', $1)
             RETURNING id",
        )
        .bind(drive_id)
        .bind(folder_id)
        .fetch_one(pool)
        .await
        .expect("seed noise folder");
        sqlx::query(
            "WITH f AS (
                INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
                SELECT 'n' || i, $1, 'benchdead000000000000000000000000000000000000000000000000000000',
                       1024, 'image/jpeg', $2
                  FROM generate_series(1, $3) AS i
                RETURNING id
             )
             INSERT INTO storage.webdav_dead_properties (file_id, namespace, local_name, value)
             SELECT id, 'urn:bench', 'noise', 'x' FROM f",
        )
        .bind(noise_folder)
        .bind(drive_id)
        .bind(noise as i32)
        .execute(pool)
        .await
        .expect("seed noise props");
    }

    sqlx::query("ANALYZE storage.webdav_dead_properties")
        .execute(pool)
        .await
        .ok();

    Seeded { drive_id, file_ids }
}

async fn cleanup(pool: &PgPool, drive_id: Uuid) {
    // drives → folders/files → dead props all cascade.
    let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(drive_id)
        .execute(pool)
        .await;
}

/// OLD: production `get_all` shape — sequential, IS NOT DISTINCT FROM.
async fn run_old(pool: &PgPool, ids: &[Uuid]) -> usize {
    let mut rows_seen = 0;
    for id in ids {
        let rows = sqlx::query(
            "SELECT namespace, local_name, value
               FROM storage.webdav_dead_properties
              WHERE folder_id IS NOT DISTINCT FROM $1
                AND file_id   IS NOT DISTINCT FROM $2",
        )
        .bind(Option::<Uuid>::None)
        .bind(Some(*id))
        .fetch_all(pool)
        .await
        .expect("old get_all");
        rows_seen += rows.len();
    }
    rows_seen
}

/// EQ: still N sequential round-trips, but with an indexable `=` predicate.
async fn run_eq(pool: &PgPool, ids: &[Uuid]) -> usize {
    let mut rows_seen = 0;
    for id in ids {
        let rows = sqlx::query(
            "SELECT namespace, local_name, value
               FROM storage.webdav_dead_properties
              WHERE file_id = $1",
        )
        .bind(*id)
        .fetch_all(pool)
        .await
        .expect("eq get_all");
        rows_seen += rows.len();
    }
    rows_seen
}

/// BATCH: one `= ANY($1)` query per PROPFIND page of 500 children.
async fn run_batch(pool: &PgPool, ids: &[Uuid], page: usize) -> usize {
    let mut rows_seen = 0;
    for chunk in ids.chunks(page) {
        let rows = sqlx::query(
            "SELECT file_id, namespace, local_name, value
               FROM storage.webdav_dead_properties
              WHERE file_id = ANY($1)",
        )
        .bind(chunk)
        .fetch_all(pool)
        .await
        .expect("batch get_all");
        // Decode file_id like the real batched store method will (map key).
        for row in &rows {
            let _: Uuid = row.get("file_id");
        }
        rows_seen += rows.len();
    }
    rows_seen
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");

    let children: usize = env_or("BENCH_CHILDREN", 2000);
    let page: usize = env_or("BENCH_PAGE", 500);
    let noise: usize = env_or("BENCH_NOISE_ROWS", 20_000);
    let reps: usize = env_or("BENCH_REPS", 5);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .min_connections(5)
        .connect(&url)
        .await
        .expect("connect Postgres");

    for &with_noise in &[false, true] {
        let n = if with_noise { noise } else { 0 };
        let seeded = seed(&pool, children, n).await;
        let total_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM storage.webdav_dead_properties")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);

        println!("\n== folder with {children} children, dead-props table = {total_rows} rows ==");
        println!(
            "{:<28} {:>10} {:>12} {:>9}",
            "mode", "queries", "total ms", "vs OLD"
        );

        let mut base = None;
        for (label, queries) in [
            ("OLD  seq, IS NOT DISTINCT", children),
            ("EQ   seq, file_id = $1", children),
            ("BATCH file_id = ANY, /page", children.div_ceil(page)),
        ] {
            let mut times = Vec::with_capacity(reps);
            let mut rows = 0;
            for _ in 0..reps {
                let t = Instant::now();
                rows = match label.split_whitespace().next().unwrap() {
                    "OLD" => run_old(&pool, &seeded.file_ids).await,
                    "EQ" => run_eq(&pool, &seeded.file_ids).await,
                    _ => run_batch(&pool, &seeded.file_ids, page).await,
                };
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            assert_eq!(rows, children, "each child has exactly 1 dead prop");
            let ms = median(times);
            let speedup = base
                .map(|b: f64| format!("{:.1}x", b / ms))
                .unwrap_or_else(|| "1.0x".into());
            if base.is_none() {
                base = Some(ms);
            }
            println!("{label:<28} {queries:>10} {ms:>12.2} {speedup:>9}");
        }

        cleanup(&pool, seeded.drive_id).await;
    }

    println!("\n(total ms = the dead-prop portion of one Depth:1 PROPFIND of the folder,");
    println!(" i.e. what the walker adds on top of the file/folder listing queries)");
}
