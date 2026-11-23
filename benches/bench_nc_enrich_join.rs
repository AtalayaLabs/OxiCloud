//! NC PROPFIND per-page enrichment — 3 serial round-trips vs `tokio::join!`.
//!
//! Every Depth:1 PROPFIND page on the NextCloud surface enriches its ≤500
//! children with three INDEPENDENT batched reads: favorites
//! (`user_favorites … = ANY`), oc:fileid resolution
//! (`nextcloud_object_ids … = ANY`) and WebDAV dead properties
//! (`webdav_dead_properties … = ANY`). The old code awaited them in
//! sequence — 3×RTT per page; overlapping them costs ~max(RTT).
//!
//! Decide-by-bench (the round-7 deferred "serial pairs" item): round 6
//! showed concurrency can LOSE on local-socket PG (authz `try_join_all`
//! regressed), so this A/B carries an **injected-latency arm** — each
//! round-trip is prefixed with `tokio::time::sleep(L)` to model network
//! RTT at L = 0 / 0.25 / 1 / 5 ms. Adoption rule: `join!` must not
//! regress at L=0 (the local-socket floor) and must win under injected
//! RTT; the L=0 row is the rollback gate.
//!
//! The three queries are the production shapes bound over the same seeded
//! 500-child page; the equivalence gate asserts both arms return
//! identical favorite sets / id maps / dead-prop rows.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_nc_enrich_join
//! Tunables (env): BENCH_CHILDREN (500), BENCH_PASSES (100)

use std::collections::HashSet;
use std::env;
use std::time::{Duration, Instant};

use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Seeded {
    drive_id: Uuid,
    user_id: Uuid,
    file_ids: Vec<Uuid>,
}

async fn seed(pool: &PgPool, children: usize) -> Seeded {
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_enrich', 'bench_enrich@example.com', 'user') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("seed user");

    let mut tx = pool.begin().await.expect("begin");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, quota_bytes) VALUES ('shared', NULL) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("drive");
    let root: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('bench_enrich', '/bench_enrich', 'bench_enrich', $1) RETURNING id",
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

    let file_ids: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
         SELECT 'f' || i, $1,
                'benchenrich0000000000000000000000000000000000000000000000000000',
                1024, 'image/jpeg', $2
           FROM generate_series(1, $3) AS i
         RETURNING id",
    )
    .bind(root)
    .bind(drive_id)
    .bind(children as i32)
    .fetch_all(pool)
    .await
    .expect("seed files");

    // Every 5th file favorited, all files carry an oc:fileid mapping,
    // every 10th file has a dead property — a realistic mixed page.
    sqlx::query(
        "INSERT INTO auth.user_favorites (user_id, item_id, item_type)
         SELECT $1, id::text, 'file' FROM storage.files
          WHERE folder_id = $2 AND (('x' || substr(md5(id::text), 1, 4))::bit(16)::int % 5) = 0",
    )
    .bind(user_id)
    .bind(root)
    .execute(pool)
    .await
    .expect("seed favorites");

    sqlx::query(
        "INSERT INTO storage.nextcloud_object_ids (object_type, object_id)
         SELECT 'file', id FROM storage.files WHERE folder_id = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(root)
    .execute(pool)
    .await
    .expect("seed object ids");

    sqlx::query(
        "INSERT INTO storage.webdav_dead_properties (file_id, namespace, local_name, value)
         SELECT id, 'urn:bench', 'displayname', 'v'
           FROM storage.files
          WHERE folder_id = $1 AND (('x' || substr(md5(id::text), 1, 4))::bit(16)::int % 10) = 0",
    )
    .bind(root)
    .execute(pool)
    .await
    .expect("seed dead props");

    Seeded {
        drive_id,
        user_id,
        file_ids,
    }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
    sqlx::query("DELETE FROM storage.webdav_dead_properties WHERE file_id = ANY($1)")
        .bind(&s.file_ids)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM storage.nextcloud_object_ids WHERE object_id = ANY($1)")
        .bind(&s.file_ids)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM auth.user_favorites WHERE user_id = $1")
        .bind(s.user_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM storage.files WHERE drive_id = $1")
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
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(s.user_id)
        .execute(pool)
        .await
        .ok();
}

// ── The three production-shaped round-trips ─────────────────────────────────

async fn q_favorites(
    pool: &PgPool,
    user_id: Uuid,
    ids: &[String],
    lat: Duration,
) -> HashSet<String> {
    if !lat.is_zero() {
        tokio::time::sleep(lat).await;
    }
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    sqlx::query("SELECT item_id FROM auth.user_favorites WHERE user_id = $1 AND item_id = ANY($2)")
        .bind(user_id)
        .bind(&id_refs)
        .fetch_all(pool)
        .await
        .expect("favorites")
        .into_iter()
        .map(|r| r.get::<String, _>(0))
        .collect()
}

async fn q_object_ids(pool: &PgPool, uuids: &[Uuid], lat: Duration) -> Vec<(i64, Uuid)> {
    if !lat.is_zero() {
        tokio::time::sleep(lat).await;
    }
    let mut rows: Vec<(i64, Uuid)> = sqlx::query(
        "SELECT id, object_id FROM storage.nextcloud_object_ids
          WHERE object_type = 'file' AND object_id = ANY($1::uuid[])",
    )
    .bind(uuids)
    .fetch_all(pool)
    .await
    .expect("object ids")
    .into_iter()
    .map(|r| (r.get::<i64, _>(0), r.get::<Uuid, _>(1)))
    .collect();
    rows.sort_unstable();
    rows
}

async fn q_dead_props(pool: &PgPool, uuids: &[Uuid], lat: Duration) -> Vec<(Uuid, String)> {
    if !lat.is_zero() {
        tokio::time::sleep(lat).await;
    }
    let mut rows: Vec<(Uuid, String)> = sqlx::query(
        "SELECT file_id, local_name FROM storage.webdav_dead_properties
          WHERE file_id = ANY($1)",
    )
    .bind(uuids)
    .fetch_all(pool)
    .await
    .expect("dead props")
    .into_iter()
    .map(|r| (r.get::<Uuid, _>(0), r.get::<String, _>(1)))
    .collect();
    rows.sort_unstable();
    rows
}

type PageResult = (HashSet<String>, Vec<(i64, Uuid)>, Vec<(Uuid, String)>);

/// BEFORE — the old serial shape.
async fn page_serial(
    pool: &PgPool,
    user_id: Uuid,
    ids: &[String],
    uuids: &[Uuid],
    lat: Duration,
) -> PageResult {
    let favs = q_favorites(pool, user_id, ids, lat).await;
    let oc = q_object_ids(pool, uuids, lat).await;
    let dead = q_dead_props(pool, uuids, lat).await;
    (favs, oc, dead)
}

/// AFTER — the production `join!` shape.
async fn page_joined(
    pool: &PgPool,
    user_id: Uuid,
    ids: &[String],
    uuids: &[Uuid],
    lat: Duration,
) -> PageResult {
    let (favs, oc, dead) = tokio::join!(
        q_favorites(pool, user_id, ids, lat),
        q_object_ids(pool, uuids, lat),
        q_dead_props(pool, uuids, lat),
    );
    (favs, oc, dead)
}

fn p50(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").expect("set DATABASE_URL — the dev Postgres URL");
    let children: usize = env_or("BENCH_CHILDREN", 500);
    let passes: usize = env_or("BENCH_PASSES", 100);

    // 4 connections: the production pool always has slack beyond 3.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .min_connections(4)
        .connect(&url)
        .await
        .expect("connect");

    let seeded = seed(&pool, children).await;
    let ids: Vec<String> = seeded.file_ids.iter().map(|u| u.to_string()).collect();
    let uuids = seeded.file_ids.clone();

    // Equivalence gate.
    let a = page_serial(&pool, seeded.user_id, &ids, &uuids, Duration::ZERO).await;
    let b = page_joined(&pool, seeded.user_id, &ids, &uuids, Duration::ZERO).await;
    if a != b {
        eprintln!("EQUIVALENCE GATE FAILED: serial and joined results differ");
        cleanup(&pool, &seeded).await;
        std::process::exit(1);
    }
    assert!(
        !a.0.is_empty() && !a.1.is_empty() && !a.2.is_empty(),
        "seed produced empty enrichment"
    );
    println!(
        "# equivalence gate: identical results (favs={}, oc_ids={}, dead={}) — OK",
        a.0.len(),
        a.1.len(),
        a.2.len()
    );

    for _ in 0..10 {
        std::hint::black_box(
            page_serial(&pool, seeded.user_id, &ids, &uuids, Duration::ZERO).await,
        );
        std::hint::black_box(
            page_joined(&pool, seeded.user_id, &ids, &uuids, Duration::ZERO).await,
        );
    }

    println!("\n#################################################################");
    println!("# NC PROPFIND page enrichment — serial 3×RTT vs tokio::join!");
    println!("# children={children} passes={passes} (interleaved, p50 ms/page)");
    println!("#################################################################\n");
    println!(
        "| {:<14} | {:>12} | {:>12} | {:>8} |",
        "injected RTT", "serial ms", "join! ms", "ratio"
    );

    let mut zero_lat_ratio = 0.0;
    for lat_us in [0u64, 250, 1_000, 5_000] {
        let lat = Duration::from_micros(lat_us);
        let mut serial = Vec::with_capacity(passes);
        let mut joined = Vec::with_capacity(passes);
        for _ in 0..passes {
            let t = Instant::now();
            std::hint::black_box(page_serial(&pool, seeded.user_id, &ids, &uuids, lat).await);
            serial.push(t.elapsed().as_secs_f64() * 1e3);
            let t = Instant::now();
            std::hint::black_box(page_joined(&pool, seeded.user_id, &ids, &uuids, lat).await);
            joined.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let (s, j) = (p50(serial), p50(joined));
        if lat_us == 0 {
            zero_lat_ratio = j / s;
        }
        println!(
            "| {:>11} µs | {:>12.3} | {:>12.3} | {:>7.2}x |",
            lat_us,
            s,
            j,
            s / j
        );
    }

    cleanup(&pool, &seeded).await;

    // Adoption gate: join! must not regress the local-socket floor by >5%
    // (measurement noise band); the injected-RTT rows document the win.
    if zero_lat_ratio > 1.05 {
        eprintln!(
            "\nGATE FAIL: join! is {:.1}% slower at 0 RTT — rollback the overlap",
            (zero_lat_ratio - 1.0) * 100.0
        );
        std::process::exit(1);
    }
    println!("\nGATE PASS: no local-socket regression; overlap wins under injected RTT.");
}
