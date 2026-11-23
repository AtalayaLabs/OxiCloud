//! PROPFIND folder-listing pagination benchmark — LIMIT/OFFSET vs keyset.
//!
//! The streaming PROPFIND walker pages a folder's children 500 at a time in
//! name order (`list_files_batch`). The old shape was `ORDER BY name LIMIT
//! 500 OFFSET k` with no supporting index — every page bitmap-scanned all N
//! children and top-sorted them, so a full folder walk was O(N²/500) row
//! visits. The change adds `idx_files_folder_name (folder_id, name) WHERE
//! NOT is_trashed` and switches the cursor to keyset (`name > $last`), making
//! each page one O(page) index-range read.
//!
//! Modes (full walk of the folder, all pages):
//!   OFFSET/no-idx — the true BEFORE (index dropped for the run)
//!   OFFSET/idx    — index alone, old query shape
//!   KEYSET/idx    — the AFTER
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_propfind_paging
//! Tunables: BENCH_FILES (20000), BENCH_PAGE (500), BENCH_REPS (3)

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

async fn seed(pool: &PgPool, files: usize) -> (Uuid, Uuid) {
    let mut tx = pool.begin().await.expect("begin");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, quota_bytes) VALUES ('shared', NULL) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("drive");
    let folder_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('bench_paging', '/bench_paging', 'bench_paging', $1) RETURNING id",
    )
    .bind(drive_id)
    .fetch_one(&mut *tx)
    .await
    .expect("folder");
    sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
        .bind(folder_id)
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .expect("stamp");
    tx.commit().await.expect("commit");

    sqlx::query(
        "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
         SELECT 'file_' || LPAD(i::text, 8, '0') || '.jpg', $1,
                'benchpaging00000000000000000000000000000000000000000000000000000',
                1024, 'image/jpeg', $2
           FROM generate_series(1, $3) AS i",
    )
    .bind(folder_id)
    .bind(drive_id)
    .bind(files as i32)
    .execute(pool)
    .await
    .expect("files");
    sqlx::query("ANALYZE storage.files")
        .execute(pool)
        .await
        .ok();
    (drive_id, folder_id)
}

const COLS: &str = "fi.id::text, fi.name, fi.folder_id::text, fo.path, fi.size, fi.mime_type,
                    EXTRACT(EPOCH FROM fi.created_at)::bigint,
                    EXTRACT(EPOCH FROM fi.updated_at)::bigint, fi.blob_hash";

type Row = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    String,
    i64,
    i64,
    String,
);

/// Full folder walk with the old LIMIT/OFFSET shape. Returns rows seen.
async fn walk_offset(pool: &PgPool, folder: Uuid, page: i64) -> usize {
    let mut offset = 0i64;
    let mut seen = 0usize;
    loop {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLS}
               FROM storage.files fi
               LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
              WHERE fi.folder_id = $1 AND NOT fi.is_trashed
              ORDER BY fi.name LIMIT $2 OFFSET $3"
        ))
        .bind(folder)
        .bind(page)
        .bind(offset)
        .fetch_all(pool)
        .await
        .expect("offset page");
        let n = rows.len();
        seen += n;
        if (n as i64) < page {
            break;
        }
        offset += n as i64;
    }
    seen
}

/// Full folder walk with the new keyset shape.
async fn walk_keyset(pool: &PgPool, folder: Uuid, page: i64) -> usize {
    let mut after: Option<String> = None;
    let mut seen = 0usize;
    loop {
        let rows: Vec<Row> = if let Some(a) = &after {
            sqlx::query_as(&format!(
                "SELECT {COLS}
                   FROM storage.files fi
                   LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
                  WHERE fi.folder_id = $1 AND NOT fi.is_trashed AND fi.name > $3
                  ORDER BY fi.name LIMIT $2"
            ))
            .bind(folder)
            .bind(page)
            .bind(a)
            .fetch_all(pool)
            .await
        } else {
            sqlx::query_as(&format!(
                "SELECT {COLS}
                   FROM storage.files fi
                   LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
                  WHERE fi.folder_id = $1 AND NOT fi.is_trashed
                  ORDER BY fi.name LIMIT $2"
            ))
            .bind(folder)
            .bind(page)
            .fetch_all(pool)
            .await
        }
        .expect("keyset page");
        let n = rows.len();
        seen += n;
        if (n as i64) < page {
            break;
        }
        after = rows.last().map(|r| r.1.clone());
    }
    seen
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").expect("set DATABASE_URL");
    let files: usize = env_or("BENCH_FILES", 20_000);
    let page: i64 = env_or("BENCH_PAGE", 500);
    let reps: usize = env_or("BENCH_REPS", 3);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect");
    println!("seeding {files} files (one-time)…");
    let (drive_id, folder_id) = seed(&pool, files).await;

    println!("\n# full PROPFIND walk of a {files}-file folder, {page}/page");
    println!("{:<18} {:>12} {:>9}", "mode", "total ms", "vs OLD");

    let mut base = None;
    for mode in ["OFFSET/no-idx", "OFFSET/idx", "KEYSET/idx"] {
        match mode {
            "OFFSET/no-idx" => {
                sqlx::query("DROP INDEX IF EXISTS storage.idx_files_folder_name")
                    .execute(&pool)
                    .await
                    .ok();
            }
            "OFFSET/idx" => {
                sqlx::query(
                    "CREATE INDEX IF NOT EXISTS idx_files_folder_name
                       ON storage.files (folder_id, name) WHERE NOT is_trashed",
                )
                .execute(&pool)
                .await
                .expect("create index");
            }
            _ => {}
        }
        let mut times = Vec::with_capacity(reps);
        for _ in 0..reps {
            let t = Instant::now();
            let seen = if mode.starts_with("OFFSET") {
                walk_offset(&pool, folder_id, page).await
            } else {
                walk_keyset(&pool, folder_id, page).await
            };
            assert_eq!(seen, files);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let ms = median(times);
        let speedup = base
            .map(|b: f64| format!("{:.1}x", b / ms))
            .unwrap_or_else(|| "1.0x".into());
        if base.is_none() {
            base = Some(ms);
        }
        println!("{mode:<18} {ms:>12.1} {speedup:>9}");
    }

    let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(drive_id)
        .execute(&pool)
        .await;
}
