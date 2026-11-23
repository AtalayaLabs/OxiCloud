//! Photos timeline benchmark — full-library scan vs per-drive LATERAL top-N.
//!
//! `list_media_files` (file_blob_read_repository.rs) filters by
//! `fi.drive_id IN (<grants subquery>)`, joins folders + file_metadata, and
//! sorts globally by `media_sort_date DESC LIMIT k`. The doc comment claims
//! `idx_files_media_timeline_by_drive` lets LIMIT stop the scan early, but
//! the plan is a Nested Loop over the drive set feeding EVERY media row
//! through a Hash Left Join into a top-N heapsort ABOVE the join — the
//! index is drained to exhaustion on every page, so each timeline page
//! costs O(library), not O(page).
//!
//! The AFTER shape materialises the accessible drive ids once, then does a
//! `CROSS JOIN LATERAL (… ORDER BY media_sort_date DESC LIMIT k)` per drive
//! — each LATERAL is one bounded index scan — and merges `drives × k` rows.
//! The folders/file_metadata joins move OUTSIDE the top-N so only the k
//! emitted rows pay them.
//!
//! Equivalence gate: page-by-page id sequences must be identical (the seed
//! uses strictly distinct capture dates so ties cannot mask reordering).
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_photos_timeline
//! Tunables: BENCH_MEDIA (50000), BENCH_DRIVES (3), BENCH_PAGE (100),
//!           BENCH_PAGES (10), BENCH_REPS (3)

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

async fn seed(pool: &PgPool, media: usize, drives: usize) -> (Uuid, Vec<Uuid>) {
    let caller = Uuid::new_v4();
    let mut drive_ids = Vec::with_capacity(drives);
    for d in 0..drives {
        let mut tx = pool.begin().await.expect("begin");
        let drive_id: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.drives (kind, quota_bytes, policies)
             VALUES ('shared', NULL, '{\"include_in_photo_index\": true}'::jsonb)
             RETURNING id",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("drive");
        let folder_id: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, path, lpath, drive_id)
             VALUES ($1, $2, $3::ltree, $4) RETURNING id",
        )
        .bind(format!("bench_photos_{d}"))
        .bind(format!("/bench_photos_{d}"))
        .bind(format!("bench_photos_{d}"))
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
        sqlx::query(
            "INSERT INTO storage.role_grants
                    (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'drive', $2, 'viewer', $1)",
        )
        .bind(caller)
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .expect("grant");
        tx.commit().await.expect("commit");

        // Strictly distinct capture dates (offset per drive) so the
        // equivalence gate cannot be masked by tie reordering.
        let per_drive = media / drives;
        sqlx::query(
            "INSERT INTO storage.files
                    (name, folder_id, blob_hash, size, mime_type, drive_id, media_sort_date)
             SELECT 'IMG_' || LPAD(i::text, 8, '0') || '.jpg', $1,
                    'benchphotos00000000000000000000000000000000000000000000000000000',
                    2048, 'image/jpeg', $2,
                    TIMESTAMPTZ '2026-01-01 00:00:00Z' - ((i * $4 + $5) || ' seconds')::interval
               FROM generate_series(1, $3) AS i",
        )
        .bind(folder_id)
        .bind(drive_id)
        .bind(per_drive as i32)
        .bind(drives as i32)
        .bind(d as i32)
        .execute(pool)
        .await
        .expect("files");
        drive_ids.push(drive_id);
    }
    sqlx::query("ANALYZE storage.files")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ANALYZE storage.role_grants")
        .execute(pool)
        .await
        .ok();
    (caller, drive_ids)
}

type MediaRow = (
    String,         // id::text
    String,         // name
    Option<String>, // folder_id::text
    Option<String>, // fo.path
    i64,            // size
    String,         // mime_type
    i64,            // created_at epoch
    i64,            // updated_at epoch
    String,         // blob_hash
    Option<Uuid>,   // created_by
    Option<Uuid>,   // updated_by
    i64,            // sort_date epoch
    Option<i32>,    // width
    Option<i32>,    // height
);

const GRANTS_SUBQ: &str = r#"
    SELECT d.id
      FROM storage.drives d
      JOIN storage.role_grants g
        ON g.resource_type = 'drive'
       AND g.resource_id   = d.id
     WHERE (
             (g.subject_type = 'user'  AND g.subject_id = $1)
          OR (g.subject_type = 'group' AND g.subject_id IN
                  (SELECT storage.caller_group_ids($1)))
           )
       AND (g.expires_at IS NULL OR g.expires_at > NOW())
       AND (d.policies->>'include_in_photo_index')::boolean = true
"#;

/// OLD shape — production SQL verbatim.
async fn old_page(
    pool: &PgPool,
    caller: Uuid,
    before: Option<chrono::DateTime<chrono::Utc>>,
    limit: i64,
) -> Vec<MediaRow> {
    let cursor_pred = if before.is_some() {
        "AND fi.media_sort_date < $2"
    } else {
        "AND $2::timestamptz IS NULL"
    };
    let sql = format!(
        r#"
        SELECT fi.id::text, fi.name, fi.folder_id::text, fo.path,
               fi.size, fi.mime_type,
               EXTRACT(EPOCH FROM fi.created_at)::bigint,
               EXTRACT(EPOCH FROM fi.updated_at)::bigint,
               fi.blob_hash,
               fi.created_by, fi.updated_by,
               EXTRACT(EPOCH FROM fi.media_sort_date)::bigint AS sort_date,
               fm.width, fm.height
          FROM storage.files fi
          LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
          LEFT JOIN storage.file_metadata fm ON fm.file_id = fi.id
         WHERE fi.drive_id IN ({GRANTS_SUBQ})
           AND NOT fi.is_trashed
           AND (fi.mime_type LIKE 'image/%' OR fi.mime_type LIKE 'video/%')
           {cursor_pred}
         ORDER BY fi.media_sort_date DESC
         LIMIT $3
        "#
    );
    sqlx::query_as(&sql)
        .bind(caller)
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await
        .expect("old page")
}

/// NEW shape — accessible drives materialised once, per-drive LATERAL top-N
/// on the timeline index, folders/metadata joined only on the emitted rows.
async fn new_page(
    pool: &PgPool,
    caller: Uuid,
    before: Option<chrono::DateTime<chrono::Utc>>,
    limit: i64,
) -> Vec<MediaRow> {
    let cursor_pred = if before.is_some() {
        "AND fi.media_sort_date < $2"
    } else {
        "AND $2::timestamptz IS NULL"
    };
    let sql = format!(
        r#"
        WITH accessible AS MATERIALIZED ({GRANTS_SUBQ})
        SELECT top.id::text, top.name, top.folder_id::text, fo.path,
               top.size, top.mime_type,
               EXTRACT(EPOCH FROM top.created_at)::bigint,
               EXTRACT(EPOCH FROM top.updated_at)::bigint,
               top.blob_hash,
               top.created_by, top.updated_by,
               EXTRACT(EPOCH FROM top.media_sort_date)::bigint AS sort_date,
               fm.width, fm.height
          FROM (
            SELECT fi.*
              FROM accessible a
             CROSS JOIN LATERAL (
                SELECT fi.*
                  FROM storage.files fi
                 WHERE fi.drive_id = a.id
                   AND NOT fi.is_trashed
                   AND (fi.mime_type LIKE 'image/%' OR fi.mime_type LIKE 'video/%')
                   {cursor_pred}
                 ORDER BY fi.media_sort_date DESC
                 LIMIT $3
             ) fi
             ORDER BY fi.media_sort_date DESC
             LIMIT $3
          ) top
          LEFT JOIN storage.folders fo ON fo.id = top.folder_id
          LEFT JOIN storage.file_metadata fm ON fm.file_id = top.id
         ORDER BY top.media_sort_date DESC
        "#
    );
    sqlx::query_as(&sql)
        .bind(caller)
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await
        .expect("new page")
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

/// Walk `pages` cursor pages; returns (id sequence, per-page ms).
async fn walk(
    pool: &PgPool,
    caller: Uuid,
    page: i64,
    pages: usize,
    new_shape: bool,
) -> (Vec<String>, Vec<f64>) {
    let mut before: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut ids = Vec::new();
    let mut times = Vec::new();
    for _ in 0..pages {
        let t = Instant::now();
        let rows = if new_shape {
            new_page(pool, caller, before, page).await
        } else {
            old_page(pool, caller, before, page).await
        };
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        if rows.is_empty() {
            break;
        }
        // Cursor semantics mirror production: whole-second epoch of the last
        // row (list_media_files hands the epoch back to the client).
        let last_epoch = rows.last().unwrap().11;
        before = chrono::DateTime::from_timestamp(last_epoch, 0);
        ids.extend(rows.into_iter().map(|r| r.0));
    }
    (ids, times)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").expect("set DATABASE_URL");
    let media: usize = env_or("BENCH_MEDIA", 50_000);
    let drives: usize = env_or("BENCH_DRIVES", 3);
    let page: i64 = env_or("BENCH_PAGE", 100);
    let pages: usize = env_or("BENCH_PAGES", 10);
    let reps: usize = env_or("BENCH_REPS", 3);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect");
    println!("seeding {media} media rows across {drives} drives (one-time)…");
    let (caller, drive_ids) = seed(&pool, media, drives).await;

    let (ref_ids, _) = walk(&pool, caller, page, pages, false).await;
    assert_eq!(
        ref_ids.len(),
        (page as usize) * pages,
        "reference walk size"
    );

    println!("\n# {pages} timeline pages of {page} over a {media}-photo library ({drives} drives)");
    println!(
        "{:<8} {:>11} {:>11} {:>8}",
        "mode", "total ms", "p50 ms/pg", "vs OLD"
    );

    let mut failures = 0usize;
    let mut base: Option<f64> = None;
    for (mode, new_shape) in [("OLD", false), ("NEW", true)] {
        let mut totals = Vec::with_capacity(reps);
        let mut per_page: Vec<f64> = Vec::new();
        for _ in 0..reps {
            let t = Instant::now();
            let (ids, times) = walk(&pool, caller, page, pages, new_shape).await;
            totals.push(t.elapsed().as_secs_f64() * 1000.0);
            if ids != ref_ids {
                eprintln!("EQUIVALENCE FAILURE: {mode} walk drained different ids");
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
            "{:<8} {:>11.1} {:>11.2} {:>8}",
            mode,
            ms,
            median(per_page.clone()),
            speedup
        );
    }

    for d in drive_ids {
        let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
            .bind(d)
            .execute(&pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM storage.role_grants WHERE subject_id = $1")
        .bind(caller)
        .execute(&pool)
        .await;

    if failures > 0 {
        eprintln!("\n{failures} equivalence failures — the NEW shape is NOT safe to adopt");
        std::process::exit(1);
    }
}
