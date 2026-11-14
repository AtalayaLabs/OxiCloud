//! WebDAV drive-selector resolution benchmark — grants join/request vs moka.
//!
//! Every native `/webdav/<selector>/…` request (all verbs; MOVE and COPY
//! twice) resolved its scope through `lookup_drive_selector` →
//! `DriveRepository::list_readable_by`: a role_grants ⋈ drives ⋈ folders
//! join with inline transitive-group expansion, GROUP BY + MIN(role) +
//! ORDER BY — per request, uncached. The same join also ran per request
//! in search, trash listing and the `GET /api/drives` picker.
//!
//! AFTER wires the per-user `readable_cache` (30 s TTL, single-flight,
//! explicit invalidation on every membership/lifecycle mutation) into
//! `DrivePgRepository` — this bench drives the REAL repository (cache,
//! `try_get_with` and the per-hit `Vec` clone included), not a synthetic
//! lookup, against the verbatim BEFORE query.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_drive_selector
//! Tunables (env): BENCH_POOL (20), BENCH_SECONDS (4), BENCH_CONCURRENCIES ("8,64").

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use oxicloud::domain::repositories::drive_repository::DriveRepository;
use oxicloud::infrastructure::repositories::pg::DrivePgRepository;
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
    user_id: Uuid,
}

/// user → personal drive (default) + two shared drives, each with a
/// role_grant for the user — the shape a typical DAV-syncing member of a
/// small team resolves on every request.
async fn seed(pool: &PgPool) -> Seeded {
    let mut tx = pool.begin().await.expect("begin");
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_drivesel', 'bench_drivesel@bench.invalid', 'user')
         RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed user");

    // (name, kind, default_for_user, role)
    let drives: [(&str, &str, Option<Uuid>, &str); 3] = [
        ("Personal", "personal", Some(user_id), "owner"),
        ("Equipo Diseño", "shared", None, "editor"),
        ("Archivo 2026", "shared", None, "viewer"),
    ];
    for (name, kind, default_for, role) in drives {
        let drive_id: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.drives (kind, default_for_user) VALUES ($1, $2) RETURNING id",
        )
        .bind(kind)
        .bind(default_for)
        .fetch_one(&mut *tx)
        .await
        .expect("seed drive");
        let folder_id: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, path, lpath, drive_id)
             VALUES ($1, '/' || $1, 'x', $2) RETURNING id",
        )
        .bind(name)
        .bind(drive_id)
        .fetch_one(&mut *tx)
        .await
        .expect("seed folder");
        sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
            .bind(folder_id)
            .bind(drive_id)
            .execute(&mut *tx)
            .await
            .expect("stamp root");
        sqlx::query(
            "INSERT INTO storage.role_grants
                 (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'drive', $2, $3::storage.grant_role, $1)",
        )
        .bind(user_id)
        .bind(drive_id)
        .bind(role)
        .execute(&mut *tx)
        .await
        .expect("seed grant");
    }
    tx.commit().await.expect("commit");
    Seeded { user_id }
}

async fn cleanup(pool: &PgPool, user_id: Uuid) {
    // Drives/folders/grants cascade off the user via the grant cleanup
    // trigger + explicit deletes (drives carry no owner FK).
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT resource_id FROM storage.role_grants
          WHERE subject_type = 'user' AND subject_id = $1 AND resource_type = 'drive'",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for id in ids {
        let _ = sqlx::query(
            "DELETE FROM storage.role_grants WHERE resource_type='drive' AND resource_id=$1",
        )
        .bind(id)
        .execute(pool)
        .await;
        let root: Option<Uuid> =
            sqlx::query_scalar("SELECT root_folder_id FROM storage.drives WHERE id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
        let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await;
        if let Some(root) = root {
            let _ = sqlx::query("DELETE FROM storage.folders WHERE id = $1")
                .bind(root)
                .execute(pool)
                .await;
        }
    }
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
}

/// The exact production BEFORE — `list_readable_by`'s query, verbatim.
async fn one_op_before(pool: &PgPool, user_id: Uuid, queries: &AtomicUsize) -> Vec<(Uuid, String)> {
    let rows = sqlx::query(
        r#"
        SELECT d.id, d.kind, d.default_for_user, d.root_folder_id,
               d.quota_bytes, d.used_bytes, d.policies,
               d.created_at, d.updated_at,
               f.name AS root_folder_name,
               MIN(g.role)::text AS caller_role
          FROM storage.drives d
          JOIN storage.folders f ON f.id = d.root_folder_id
          JOIN storage.role_grants g
            ON g.resource_type = 'drive'
           AND g.resource_id   = d.id
         WHERE (
                 (g.subject_type = 'user'  AND g.subject_id = $1)
              OR (g.subject_type = 'group' AND g.subject_id IN
                      (SELECT storage.caller_group_ids($1)))
               )
           AND (g.expires_at IS NULL OR g.expires_at > NOW())
         GROUP BY d.id, d.kind, d.default_for_user, d.root_folder_id,
                  d.quota_bytes, d.used_bytes, d.policies,
                  d.created_at, d.updated_at, f.name
         ORDER BY (d.default_for_user IS NULL) ASC,
                  LOWER(f.name) ASC
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("grants join");
    queries.fetch_add(1, Ordering::Relaxed);
    rows.iter()
        .map(|r| {
            (
                r.get::<Uuid, _>("id"),
                r.get::<String, _>("root_folder_name"),
            )
        })
        .collect()
}

struct Stats {
    rps: f64,
    p50: f64,
    p95: f64,
    p99: f64,
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
        rps: n as f64 / secs as f64,
        p50: pct(0.50),
        p95: pct(0.95),
        p99: pct(0.99),
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");

    let pool_size: u32 = env_or("BENCH_POOL", 20);
    let secs: u64 = env_or("BENCH_SECONDS", 4);
    let concurrencies: Vec<usize> = env::var("BENCH_CONCURRENCIES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![8, 64]);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(pool_size)
            .min_connections(pool_size)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect Postgres"),
    );

    let seeded = seed(&pool).await;
    let user_id = seeded.user_id;

    // AFTER = the real repository with its readable_cache.
    let repo = Arc::new(DrivePgRepository::new(pool.clone()));

    // ── Equivalence gate: BEFORE rows == repo output (cold), == warm hit ──
    let gate_q = AtomicUsize::new(0);
    let before_rows = one_op_before(&pool, user_id, &gate_q).await;
    let cold: Vec<(Uuid, String)> = repo
        .list_readable_by(user_id)
        .await
        .expect("repo list")
        .iter()
        .map(|d| (d.drive.id, d.root_folder_name.clone()))
        .collect();
    let warm: Vec<(Uuid, String)> = repo
        .list_readable_by(user_id)
        .await
        .expect("repo list warm")
        .iter()
        .map(|d| (d.drive.id, d.root_folder_name.clone()))
        .collect();
    if before_rows != cold || cold != warm {
        eprintln!(
            "EQUIVALENCE GATE FAILED:\n before={before_rows:?}\n cold={cold:?}\n warm={warm:?}"
        );
        cleanup(&pool, user_id).await;
        std::process::exit(1);
    }
    if before_rows.len() != 3 {
        eprintln!("seed expected 3 readable drives, got {}", before_rows.len());
        cleanup(&pool, user_id).await;
        std::process::exit(1);
    }

    println!("\n#################################################################");
    println!("# WebDAV drive-selector: BEFORE (grants join/req) vs AFTER (cache)");
    println!("# pool={pool_size} window={secs}s/run  drives/user=3");
    println!("#################################################################\n");
    println!(
        "| {:>5} | {:<6} | {:>10} | {:>9} | {:>9} | {:>9} | {:>9} |",
        "conc", "mode", "req/s", "p50 µs", "p95 µs", "p99 µs", "queries"
    );

    for &conc in &concurrencies {
        for mode in ["BEFORE", "AFTER"] {
            let queries = Arc::new(AtomicUsize::new(0));
            let deadline = Instant::now() + Duration::from_secs(secs);
            let mut handles = Vec::new();
            for _ in 0..conc {
                let pool = pool.clone();
                let repo = repo.clone();
                let queries = queries.clone();
                let mode = mode.to_string();
                handles.push(tokio::spawn(async move {
                    let mut lats = Vec::new();
                    while Instant::now() < deadline {
                        let t = Instant::now();
                        if mode == "BEFORE" {
                            std::hint::black_box(one_op_before(&pool, user_id, &queries).await);
                        } else {
                            let v = repo.list_readable_by(user_id).await.expect("repo list");
                            std::hint::black_box(v);
                        }
                        lats.push(t.elapsed().as_secs_f64() * 1_000_000.0);
                        if mode == "AFTER" {
                            // cache hit is sub-µs; yield so the loop doesn't
                            // monopolise workers and skew the run count.
                            tokio::task::yield_now().await;
                        }
                    }
                    lats
                }));
            }
            let mut all = Vec::new();
            for h in handles {
                all.extend(h.await.unwrap());
            }
            let s = summarize(all, secs);
            println!(
                "| {:>5} | {:<6} | {:>10.0} | {:>9.2} | {:>9.2} | {:>9.2} | {:>9} |",
                conc,
                mode,
                s.rps,
                s.p50,
                s.p95,
                s.p99,
                queries.load(Ordering::Relaxed)
            );
        }
    }

    cleanup(&pool, user_id).await;
    println!("\n(BEFORE = the verbatim list_readable_by join per request; AFTER = the");
    println!(" real DrivePgRepository serving from its per-user readable_cache —");
    println!(" try_get_with single-flight + per-hit Vec clone included. Equivalence");
    println!(" gate asserts identical (id, name) sequences: BEFORE == cold == warm.)");
}
