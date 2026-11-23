//! Web-UI folder listing benchmark — whole-folder rescan vs keyset pushdown.
//!
//! `list_resources_paged` (folder_db_repository.rs) pages the SPA files view
//! with a UNION-ALL CTE (folders + files) and applies the keyset cursor
//! OUTSIDE the CTE on computed columns (`sort_str = LOWER(name)`,
//! `folder_first`). Postgres therefore scans every remaining row of the
//! folder and top-N-sorts it on EVERY page — a 20k-file folder pays a full
//! rescan per 200-row page.
//!
//! The AFTER shape pushes the cursor into each branch as a sargable
//! row-value comparison (`(LOWER(name), id) > ($str, $id)`), gives each
//! branch its own `ORDER BY … LIMIT`, and adds two expression indexes:
//!   idx_files_folder_lname   (folder_id, LOWER(name), id) WHERE NOT is_trashed
//!   idx_folders_parent_lname (parent_id, LOWER(name), id) WHERE NOT is_trashed
//! The outer query then merges ≤ 2·limit pre-sorted rows.
//!
//! Modes (full drain of the folder in default "name" order, plus a
//! modified_at parity check):
//!   OLD/no-idx — the true BEFORE
//!   OLD/idx    — new indexes alone, old query shape
//!   NEW/idx    — the AFTER
//!
//! Equivalence gate: the drained (type, id) sequence must be identical
//! across all modes; a mismatch aborts with exit(1).
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_listing_keyset
//! Tunables: BENCH_FILES (20000), BENCH_DIRS (300), BENCH_PAGE (200),
//!           BENCH_REPS (3)

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

async fn seed(pool: &PgPool, files: usize, dirs: usize) -> (Uuid, Uuid) {
    let mut tx = pool.begin().await.expect("begin");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, quota_bytes) VALUES ('shared', NULL) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("drive");
    let folder_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('bench_listing', '/bench_listing', 'bench_listing', $1) RETURNING id",
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

    // Mixed-case names so LOWER() actually differs from the raw column.
    sqlx::query(
        "INSERT INTO storage.folders (name, path, lpath, parent_id, drive_id)
         SELECT 'Dir_' || LPAD(i::text, 6, '0'),
                '/bench_listing/Dir_' || LPAD(i::text, 6, '0'),
                ('bench_listing.d' || i)::ltree,
                $1, $2
           FROM generate_series(1, $3) AS i",
    )
    .bind(folder_id)
    .bind(drive_id)
    .bind(dirs as i32)
    .execute(pool)
    .await
    .expect("dirs");
    sqlx::query(
        "INSERT INTO storage.files
                (name, folder_id, blob_hash, size, mime_type, drive_id,
                 updated_at, category_order)
         SELECT 'File_' || LPAD(i::text, 8, '0') || '.JPG', $1,
                'benchlisting0000000000000000000000000000000000000000000000000000',
                1024 + i, 'image/jpeg', $2,
                NOW() - (i || ' seconds')::interval,
                3
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
    sqlx::query("ANALYZE storage.folders")
        .execute(pool)
        .await
        .ok();
    (drive_id, folder_id)
}

const FOLDER_BRANCH: &str = r#"
    SELECT
        'folder'::text            AS resource_type,
        f.id,
        f.name,
        f.parent_id               AS folder_id,
        NULL::text                AS mime_type,
        -1::bigint                AS size,
        f.created_at,
        f.updated_at              AS modified_at,
        f.drive_id,
        NULL::text                AS blob_hash,
        LOWER(f.name)             AS sort_str,
        0::bigint                 AS type_order,
        0::int                    AS folder_first
    FROM storage.folders f
    WHERE f.parent_id = $1::uuid AND NOT f.is_trashed
"#;

const FILE_BRANCH: &str = r#"
    SELECT
        'file'::text              AS resource_type,
        fm.id,
        fm.name,
        fm.folder_id,
        fm.mime_type,
        fm.size::bigint,
        fm.created_at,
        fm.updated_at             AS modified_at,
        fm.drive_id,
        fm.blob_hash,
        LOWER(fm.name)            AS sort_str,
        fm.category_order::bigint AS type_order,
        1::int                    AS folder_first
    FROM storage.files fm
    WHERE fm.folder_id = $1::uuid AND NOT fm.is_trashed
"#;

const COLS: &str = "resource_type, id, name, folder_id, mime_type, size, \
                    created_at, modified_at, drive_id, blob_hash, \
                    sort_str, type_order, folder_first";

type Row = (
    String,
    Uuid,
    String,
    Option<Uuid>,
    Option<String>,
    i64,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
    Uuid,
    Option<String>,
    String,
    i64,
    i32,
);

/// Cursor state for the walks: (folder_first, sort_str, modified_at, id).
#[derive(Clone)]
struct Cur {
    ff: i64,
    sort_str: String,
    ts: chrono::DateTime<chrono::Utc>,
    id: Uuid,
}

/// OLD shape, "name" order — production SQL verbatim: cursor OUTSIDE the CTE.
async fn old_page_name(pool: &PgPool, parent: Uuid, cur: Option<&Cur>, limit: i64) -> Vec<Row> {
    let sql = format!(
        "WITH resources AS ({FOLDER_BRANCH} UNION ALL {FILE_BRANCH}) \
         SELECT {COLS} FROM resources \
         WHERE ($3::bigint IS NULL) \
            OR (folder_first::bigint > $3) \
            OR (folder_first::bigint = $3 AND sort_str > $2) \
            OR (folder_first::bigint = $3 AND sort_str = $2 AND id > $5::uuid) \
         ORDER BY folder_first ASC, sort_str ASC, id ASC \
         LIMIT $6"
    );
    sqlx::query_as(&sql)
        .bind(parent)
        .bind(cur.map(|c| c.sort_str.clone()))
        .bind(cur.map(|c| c.ff))
        .bind(cur.map(|c| c.ts))
        .bind(cur.map(|c| c.id))
        .bind(limit)
        .fetch_all(pool)
        .await
        .expect("old name page")
}

/// NEW shape, "name" order — cursor pushed into each branch as a sargable
/// row-value comparison; each branch pre-sorts and pre-limits.
async fn new_page_name(pool: &PgPool, parent: Uuid, cur: Option<&Cur>, limit: i64) -> Vec<Row> {
    match cur {
        None => {
            let sql = format!(
                "SELECT {COLS} FROM ( \
                   (SELECT * FROM ({FOLDER_BRANCH}) fb \
                     ORDER BY sort_str ASC, id ASC LIMIT $2) \
                   UNION ALL \
                   (SELECT * FROM ({FILE_BRANCH}) lb \
                     ORDER BY sort_str ASC, id ASC LIMIT $2) \
                 ) r ORDER BY folder_first ASC, sort_str ASC, id ASC LIMIT $2"
            );
            sqlx::query_as(&sql)
                .bind(parent)
                .bind(limit)
                .fetch_all(pool)
                .await
                .expect("new name page (first)")
        }
        Some(c) if c.ff == 0 => {
            // Cursor sits in the folder group: folders continue after the
            // row-value cursor; ALL files still follow.
            let sql = format!(
                "SELECT {COLS} FROM ( \
                   (SELECT * FROM ({FOLDER_BRANCH} \
                       AND (LOWER(f.name), f.id) > ($3, $4::uuid)) fb \
                     ORDER BY sort_str ASC, id ASC LIMIT $2) \
                   UNION ALL \
                   (SELECT * FROM ({FILE_BRANCH}) lb \
                     ORDER BY sort_str ASC, id ASC LIMIT $2) \
                 ) r ORDER BY folder_first ASC, sort_str ASC, id ASC LIMIT $2"
            );
            sqlx::query_as(&sql)
                .bind(parent)
                .bind(limit)
                .bind(&c.sort_str)
                .bind(c.id)
                .fetch_all(pool)
                .await
                .expect("new name page (folder cursor)")
        }
        Some(c) => {
            // Cursor sits in the file group: the folder branch is exhausted.
            let sql = format!(
                "SELECT {COLS} FROM ( \
                   SELECT * FROM ({FILE_BRANCH} \
                       AND (LOWER(fm.name), fm.id) > ($3, $4::uuid)) lb \
                    ORDER BY sort_str ASC, id ASC LIMIT $2 \
                 ) r ORDER BY folder_first ASC, sort_str ASC, id ASC LIMIT $2"
            );
            sqlx::query_as(&sql)
                .bind(parent)
                .bind(limit)
                .bind(&c.sort_str)
                .bind(c.id)
                .fetch_all(pool)
                .await
                .expect("new name page (file cursor)")
        }
    }
}

/// OLD shape, "modified_at" order (newest first) — production SQL verbatim.
async fn old_page_modified(pool: &PgPool, parent: Uuid, cur: Option<&Cur>, limit: i64) -> Vec<Row> {
    let sql = format!(
        "WITH resources AS ({FOLDER_BRANCH} UNION ALL {FILE_BRANCH}) \
         SELECT {COLS} FROM resources \
         WHERE ($4::timestamptz IS NULL) \
            OR (modified_at < $4) \
            OR (modified_at = $4 AND id < $5::uuid) \
         ORDER BY modified_at DESC, id DESC \
         LIMIT $6"
    );
    sqlx::query_as(&sql)
        .bind(parent)
        .bind(cur.map(|c| c.sort_str.clone()))
        .bind(cur.map(|c| c.ff))
        .bind(cur.map(|c| c.ts))
        .bind(cur.map(|c| c.id))
        .bind(limit)
        .fetch_all(pool)
        .await
        .expect("old modified page")
}

/// NEW shape, "modified_at" order — per-branch row-value cursor + LIMIT.
async fn new_page_modified(pool: &PgPool, parent: Uuid, cur: Option<&Cur>, limit: i64) -> Vec<Row> {
    match cur {
        None => {
            let sql = format!(
                "SELECT {COLS} FROM ( \
                   (SELECT * FROM ({FOLDER_BRANCH}) fb \
                     ORDER BY modified_at DESC, id DESC LIMIT $2) \
                   UNION ALL \
                   (SELECT * FROM ({FILE_BRANCH}) lb \
                     ORDER BY modified_at DESC, id DESC LIMIT $2) \
                 ) r ORDER BY modified_at DESC, id DESC LIMIT $2"
            );
            sqlx::query_as(&sql)
                .bind(parent)
                .bind(limit)
                .fetch_all(pool)
                .await
                .expect("new modified page (first)")
        }
        Some(c) => {
            let sql = format!(
                "SELECT {COLS} FROM ( \
                   (SELECT * FROM ({FOLDER_BRANCH} \
                       AND (f.updated_at, f.id) < ($3, $4::uuid)) fb \
                     ORDER BY modified_at DESC, id DESC LIMIT $2) \
                   UNION ALL \
                   (SELECT * FROM ({FILE_BRANCH} \
                       AND (fm.updated_at, fm.id) < ($3, $4::uuid)) lb \
                     ORDER BY modified_at DESC, id DESC LIMIT $2) \
                 ) r ORDER BY modified_at DESC, id DESC LIMIT $2"
            );
            sqlx::query_as(&sql)
                .bind(parent)
                .bind(limit)
                .bind(c.ts)
                .bind(c.id)
                .fetch_all(pool)
                .await
                .expect("new modified page (cursor)")
        }
    }
}

/// Drain the whole folder; returns ((type, id) sequence, per-page ms).
async fn drain(
    pool: &PgPool,
    parent: Uuid,
    limit: i64,
    new_shape: bool,
    by_modified: bool,
) -> (Vec<(String, Uuid)>, Vec<f64>) {
    let mut cur: Option<Cur> = None;
    let mut seq = Vec::new();
    let mut page_ms = Vec::new();
    loop {
        let t = Instant::now();
        let rows = match (new_shape, by_modified) {
            (false, false) => old_page_name(pool, parent, cur.as_ref(), limit).await,
            (true, false) => new_page_name(pool, parent, cur.as_ref(), limit).await,
            (false, true) => old_page_modified(pool, parent, cur.as_ref(), limit).await,
            (true, true) => new_page_modified(pool, parent, cur.as_ref(), limit).await,
        };
        page_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let n = rows.len();
        if let Some(last) = rows.last() {
            cur = Some(Cur {
                ff: last.12 as i64,
                sort_str: last.10.clone(),
                ts: last.7,
                id: last.1,
            });
        }
        seq.extend(rows.into_iter().map(|r| (r.0, r.1)));
        if (n as i64) < limit {
            break;
        }
    }
    (seq, page_ms)
}

async fn set_indexes(pool: &PgPool, on: bool) {
    if on {
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_files_folder_lname
               ON storage.files (folder_id, LOWER(name), id) WHERE NOT is_trashed",
        )
        .execute(pool)
        .await
        .expect("files idx");
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_folders_parent_lname
               ON storage.folders (parent_id, LOWER(name), id) WHERE NOT is_trashed",
        )
        .execute(pool)
        .await
        .expect("folders idx");
    } else {
        sqlx::query("DROP INDEX IF EXISTS storage.idx_files_folder_lname")
            .execute(pool)
            .await
            .ok();
        sqlx::query("DROP INDEX IF EXISTS storage.idx_folders_parent_lname")
            .execute(pool)
            .await
            .ok();
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn p99(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[(xs.len() as f64 * 0.99) as usize % xs.len()]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").expect("set DATABASE_URL");
    let files: usize = env_or("BENCH_FILES", 20_000);
    let dirs: usize = env_or("BENCH_DIRS", 300);
    let page: i64 = env_or("BENCH_PAGE", 200);
    let reps: usize = env_or("BENCH_REPS", 3);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect");
    println!("seeding {files} files + {dirs} dirs (one-time)…");
    let (drive_id, folder_id) = seed(&pool, files, dirs).await;
    let total = files + dirs;

    // Reference sequences for the equivalence gate (computed once per mode).
    set_indexes(&pool, false).await;
    let (ref_name, _) = drain(&pool, folder_id, page, false, false).await;
    let (ref_modified, _) = drain(&pool, folder_id, page, false, true).await;
    assert_eq!(ref_name.len(), total, "name drain row count");
    assert_eq!(ref_modified.len(), total, "modified drain row count");

    println!("\n# full SPA-listing drain of a {files}-file/{dirs}-dir folder, {page}/page");
    println!(
        "{:<28} {:>11} {:>11} {:>11} {:>8}",
        "mode", "total ms", "p50 ms/pg", "p99 ms/pg", "vs OLD"
    );

    let mut failures = 0usize;
    for by_modified in [false, true] {
        let label = if by_modified { "modified_at" } else { "name" };
        let reference = if by_modified {
            &ref_modified
        } else {
            &ref_name
        };
        let mut base: Option<f64> = None;
        for (mode, new_shape, idx) in [
            ("OLD/no-idx", false, false),
            ("OLD/idx", false, true),
            ("NEW/idx", true, true),
        ] {
            set_indexes(&pool, idx).await;
            let mut totals = Vec::with_capacity(reps);
            let mut pages: Vec<f64> = Vec::new();
            for _ in 0..reps {
                let t = Instant::now();
                let (seq, page_ms) = drain(&pool, folder_id, page, new_shape, by_modified).await;
                totals.push(t.elapsed().as_secs_f64() * 1000.0);
                if &seq != reference {
                    eprintln!("EQUIVALENCE FAILURE: {label}/{mode} drained a different sequence");
                    failures += 1;
                }
                pages = page_ms;
            }
            let ms = median(totals);
            let speedup = base
                .map(|b| format!("{:.1}x", b / ms))
                .unwrap_or_else(|| "1.0x".into());
            if base.is_none() {
                base = Some(ms);
            }
            println!(
                "{:<28} {:>11.1} {:>11.2} {:>11.2} {:>8}",
                format!("{label} {mode}"),
                ms,
                median(pages.clone()),
                p99(pages.clone()),
                speedup
            );
        }
    }

    let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(drive_id)
        .execute(&pool)
        .await;
    // Leave the new indexes in place (they are the production migration).

    if failures > 0 {
        eprintln!("\n{failures} equivalence failures — the NEW shape is NOT safe to adopt");
        std::process::exit(1);
    }
}
