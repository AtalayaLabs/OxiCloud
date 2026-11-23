//! PROPFIND subfolder-paging benchmark — LIMIT/OFFSET + COUNT(*) OVER() vs
//! keyset, mirroring the files-side PROPFIND-PAGING fix.
//!
//! The streaming PROPFIND walkers (native WebDAV + NC-DAV) page a folder's
//! subfolders via `list_folders_paginated`, whose query is
//! `COUNT(*) OVER() … ORDER BY name LIMIT $2 OFFSET $3` — every page
//! window-aggregates and rescans ALL N subfolders (the total is only used
//! for has_next), so a full walk is O(N²/page) row visits.
//!
//! The AFTER shape is the same keyset used for files: `name > $last ORDER BY
//! name LIMIT k`, served by the existing UNIQUE index
//! `idx_folders_unique_name (parent_id, name, drive_id) WHERE NOT is_trashed
//! AND parent_id IS NOT NULL` — no migration needed. has_next falls out of
//! `rows.len() == limit`.
//!
//! Equivalence gate: the drained name sequence must be identical.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_folder_keyset
//! Tunables: BENCH_DIRS (5000), BENCH_PAGE (500), BENCH_REPS (5)

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

async fn seed(pool: &PgPool, dirs: usize) -> (Uuid, Uuid) {
    let mut tx = pool.begin().await.expect("begin");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, quota_bytes) VALUES ('shared', NULL) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("drive");
    let folder_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('bench_folder_keyset', '/bench_folder_keyset', 'bench_folder_keyset', $1)
         RETURNING id",
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
        "INSERT INTO storage.folders (name, path, lpath, parent_id, drive_id)
         SELECT 'Dir_' || LPAD(i::text, 6, '0'),
                '/bench_folder_keyset/Dir_' || LPAD(i::text, 6, '0'),
                ('bench_folder_keyset.d' || i)::ltree,
                $1, $2
           FROM generate_series(1, $3) AS i",
    )
    .bind(folder_id)
    .bind(drive_id)
    .bind(dirs as i32)
    .execute(pool)
    .await
    .expect("dirs");
    sqlx::query("ANALYZE storage.folders")
        .execute(pool)
        .await
        .ok();
    (drive_id, folder_id)
}

const COLS: &str = "id::text, name, path, parent_id::text, drive_id,
                    EXTRACT(EPOCH FROM created_at)::bigint,
                    EXTRACT(EPOCH FROM updated_at)::bigint,
                    EXTRACT(EPOCH FROM tree_modified_at)::bigint,
                    created_by, updated_by";

type Row = (
    String,
    String,
    String,
    Option<String>,
    Uuid,
    i64,
    i64,
    i64,
    Option<Uuid>,
    Option<Uuid>,
);
type RowWithTotal = (
    String,
    String,
    String,
    Option<String>,
    Uuid,
    i64,
    i64,
    i64,
    Option<Uuid>,
    Option<Uuid>,
    i64,
);

/// OLD: production `list_folders_paginated` shape — window total + OFFSET.
async fn walk_offset(pool: &PgPool, parent: Uuid, page: i64) -> (Vec<String>, Vec<f64>) {
    let mut offset = 0i64;
    let mut names = Vec::new();
    let mut times = Vec::new();
    loop {
        let t = Instant::now();
        let rows: Vec<RowWithTotal> = sqlx::query_as(&format!(
            "SELECT {COLS}, COUNT(*) OVER() AS total_count
               FROM storage.folders
              WHERE parent_id = $1::uuid AND NOT is_trashed
              ORDER BY name
              LIMIT $2 OFFSET $3"
        ))
        .bind(parent)
        .bind(page)
        .bind(offset)
        .fetch_all(pool)
        .await
        .expect("offset page");
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let n = rows.len();
        names.extend(rows.into_iter().map(|r| r.1));
        if (n as i64) < page {
            break;
        }
        offset += n as i64;
    }
    (names, times)
}

/// NEW: keyset on the existing unique index; has_next = rows.len() == limit.
async fn walk_keyset(pool: &PgPool, parent: Uuid, page: i64) -> (Vec<String>, Vec<f64>) {
    let mut after: Option<String> = None;
    let mut names = Vec::new();
    let mut times = Vec::new();
    loop {
        let t = Instant::now();
        let rows: Vec<Row> = if let Some(a) = &after {
            sqlx::query_as(&format!(
                "SELECT {COLS}
                   FROM storage.folders
                  WHERE parent_id = $1::uuid AND NOT is_trashed AND name > $3
                  ORDER BY name
                  LIMIT $2"
            ))
            .bind(parent)
            .bind(page)
            .bind(a)
            .fetch_all(pool)
            .await
        } else {
            sqlx::query_as(&format!(
                "SELECT {COLS}
                   FROM storage.folders
                  WHERE parent_id = $1::uuid AND NOT is_trashed
                  ORDER BY name
                  LIMIT $2"
            ))
            .bind(parent)
            .bind(page)
            .fetch_all(pool)
            .await
        }
        .expect("keyset page");
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let n = rows.len();
        after = rows.last().map(|r| r.1.clone());
        names.extend(rows.into_iter().map(|r| r.1));
        if (n as i64) < page {
            break;
        }
    }
    (names, times)
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").expect("set DATABASE_URL");
    let dirs: usize = env_or("BENCH_DIRS", 5_000);
    let page: i64 = env_or("BENCH_PAGE", 500);
    let reps: usize = env_or("BENCH_REPS", 5);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect");
    println!("seeding {dirs} subfolders (one-time)…");
    let (drive_id, folder_id) = seed(&pool, dirs).await;

    let (ref_names, _) = walk_offset(&pool, folder_id, page).await;
    assert_eq!(ref_names.len(), dirs, "reference drain size");

    println!("\n# full PROPFIND subfolder walk of a {dirs}-dir parent, {page}/page");
    println!(
        "{:<12} {:>11} {:>11} {:>8}",
        "mode", "total ms", "p50 ms/pg", "vs OLD"
    );

    let mut failures = 0usize;
    let mut base: Option<f64> = None;
    for mode in ["OFFSET", "KEYSET"] {
        let mut totals = Vec::with_capacity(reps);
        let mut per_page: Vec<f64> = Vec::new();
        for _ in 0..reps {
            let t = Instant::now();
            let (names, times) = if mode == "OFFSET" {
                walk_offset(&pool, folder_id, page).await
            } else {
                walk_keyset(&pool, folder_id, page).await
            };
            totals.push(t.elapsed().as_secs_f64() * 1000.0);
            if names != ref_names {
                eprintln!("EQUIVALENCE FAILURE: {mode} drained a different sequence");
                failures += 1;
            }
            per_page = times;
        }
        let ms = median(totals);
        let speedup = base
            .map(|b| format!("{:.1}x", b / ms))
            .unwrap_or_else(|| "1.0x".into());
        if base.is_none() {
            base = Some(ms);
        }
        println!(
            "{:<12} {:>11.1} {:>11.2} {:>8}",
            mode,
            ms,
            median(per_page.clone()),
            speedup
        );
    }

    let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(drive_id)
        .execute(&pool)
        .await;

    if failures > 0 {
        eprintln!("\n{failures} equivalence failures — the NEW shape is NOT safe to adopt");
        std::process::exit(1);
    }
}
