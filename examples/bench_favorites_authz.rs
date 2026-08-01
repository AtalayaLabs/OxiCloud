//! Batch-favorites AuthZ fan-out benchmark — serial `require` loop vs
//! `try_join_all`.
//!
//! VERDICT (round 6): the fan-out measured WORSE on both the cold and the
//! warm path against local-socket Postgres (see benches/ROUND6.md), so the
//! production loop stays serial. This example is kept as the reproducible
//! evidence for that rejection — re-run it if the DB ever moves behind real
//! network latency, where the answer could flip.
//!
//! `FavoritesService::batch_add_to_favorites` pre-checks `Permission::Read`
//! on every referenced resource. BEFORE awaited the checks one-by-one: for a
//! "select all → add to favorites" over N items whose drive-lookup isn't
//! cached yet, that is N sequential point-SELECT round-trips
//! (`drive_of` per distinct file) before the batched insert even starts.
//! AFTER fans the same checks out with `futures::future::try_join_all`
//! (fail-fast on any denial preserved).
//!
//! This bench drives the REAL `PgAclEngine` (owner/drive-role caches
//! included) against a seeded shared drive:
//!   caller ──editor grant──▶ drive ─▶ root folder ─▶ N files
//!
//! Arms: cold engine (empty caches — the first-grid-load shape) and warm
//! repeat (all moka — parity check, both arms should collapse).
//!
//! Equivalence gates: every check grants for the member on both arms, and
//! both arms deny a control user with no grant.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo run --release --features bench --example bench_favorites_authz
//! Tunables (env): BENCH_FILES (200), BENCH_POOL (20).

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxicloud::application::ports::authorization_ports::AuthorizationEngine;
use oxicloud::domain::services::authorization::{Permission, Resource, Subject};
use oxicloud::infrastructure::repositories::pg::{
    FileBlobReadRepository, FolderDbRepository, SubjectGroupPgRepository,
};
use oxicloud::infrastructure::services::dedup_service::DedupService;
use oxicloud::infrastructure::services::local_blob_backend::LocalBlobBackend;
use oxicloud::infrastructure::services::pg_acl_engine::PgAclEngine;
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
    caller: Uuid,
    control: Uuid,
    drive_id: Uuid,
    root_folder: Uuid,
    blob_hash: String,
    file_ids: Vec<Uuid>,
}

async fn seed(pool: &PgPool, n_files: usize) -> Seeded {
    let mut tx = pool.begin().await.expect("begin");
    let caller: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_favauthz', 'bench_favauthz@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed caller");
    let control: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_favauthz_ctl', 'bench_favauthz_ctl@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed control");

    let drive_id: Uuid =
        sqlx::query_scalar("INSERT INTO storage.drives (kind) VALUES ('shared') RETURNING id")
            .fetch_one(&mut *tx)
            .await
            .expect("seed drive");
    let root_folder: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('Bench Shared', '/Bench Shared', 'x', $1) RETURNING id",
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
    sqlx::query(
        "INSERT INTO storage.role_grants
             (subject_type, subject_id, resource_type, resource_id, role, granted_by)
         VALUES ('user', $1, 'drive', $2, 'editor'::storage.grant_role, $1)",
    )
    .bind(caller)
    .bind(drive_id)
    .execute(&mut *tx)
    .await
    .expect("seed grant");

    let blob_hash = "benchfavauthz0000000000000000000000000000000000000000000000000b1".to_string();
    sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count) VALUES ($1, 1, 1)")
        .bind(&blob_hash)
        .execute(&mut *tx)
        .await
        .expect("seed blob");

    let mut file_ids = Vec::with_capacity(n_files);
    for i in 0..n_files {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
             VALUES ($1, $2, $3, 1, 'text/plain', $4) RETURNING id",
        )
        .bind(format!("bench-{i:04}.txt"))
        .bind(root_folder)
        .bind(&blob_hash)
        .bind(drive_id)
        .fetch_one(&mut *tx)
        .await
        .expect("seed file");
        file_ids.push(id);
    }
    tx.commit().await.expect("commit");
    Seeded {
        caller,
        control,
        drive_id,
        root_folder,
        blob_hash,
        file_ids,
    }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
    let _ = sqlx::query("DELETE FROM storage.role_grants WHERE resource_id = $1")
        .bind(s.drive_id)
        .execute(pool)
        .await;
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
    let _ = sqlx::query("DELETE FROM auth.users WHERE id IN ($1, $2)")
        .bind(s.caller)
        .bind(s.control)
        .execute(pool)
        .await;
}

fn fresh_engine(pool: &Arc<PgPool>) -> Arc<PgAclEngine> {
    let folder_repo = Arc::new(FolderDbRepository::new(pool.clone()));
    let backend = Arc::new(LocalBlobBackend::new(std::path::Path::new(
        "/tmp/bench-favauthz-blobs",
    )));
    let dedup = Arc::new(DedupService::new(backend, pool.clone(), pool.clone()));
    let file_repo = Arc::new(FileBlobReadRepository::new(
        pool.clone(),
        dedup,
        folder_repo.clone(),
    ));
    let group_repo = Arc::new(SubjectGroupPgRepository::new(pool.clone()));
    Arc::new(PgAclEngine::new(
        pool.clone(),
        folder_repo,
        file_repo,
        group_repo,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    ))
}

/// BEFORE, verbatim shape: one awaited `require` per item.
async fn serial_checks(engine: &Arc<PgAclEngine>, user: Uuid, files: &[Uuid]) -> Result<(), ()> {
    for id in files {
        engine
            .require(Subject::User(user), Permission::Read, Resource::File(*id))
            .await
            .map_err(|_| ())?;
    }
    Ok(())
}

/// AFTER: the same checks, fanned out with fail-fast join.
async fn joined_checks(engine: &Arc<PgAclEngine>, user: Uuid, files: &[Uuid]) -> Result<(), ()> {
    futures::future::try_join_all(
        files
            .iter()
            .map(|id| engine.require(Subject::User(user), Permission::Read, Resource::File(*id))),
    )
    .await
    .map(|_| ())
    .map_err(|_| ())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let n_files: usize = env_or("BENCH_FILES", 200);
    let pool_size: u32 = env_or("BENCH_POOL", 20);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(pool_size)
            .min_connections(pool_size)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect Postgres"),
    );

    let seeded = seed(&pool, n_files).await;

    // ── Equivalence gates ────────────────────────────────────────────────
    // Grant path: both arms must authorize every file for the member.
    let gate_engine = fresh_engine(&pool);
    if serial_checks(&gate_engine, seeded.caller, &seeded.file_ids)
        .await
        .is_err()
        || joined_checks(&gate_engine, seeded.caller, &seeded.file_ids)
            .await
            .is_err()
    {
        eprintln!("EQUIVALENCE GATE FAILED: member was denied");
        cleanup(&pool, &seeded).await;
        std::process::exit(1);
    }
    // Denial path: both arms must reject the control user (fresh engines so
    // the joined arm can't ride the serial arm's caches).
    let deny_a = fresh_engine(&pool);
    let deny_b = fresh_engine(&pool);
    if serial_checks(&deny_a, seeded.control, &seeded.file_ids)
        .await
        .is_ok()
        || joined_checks(&deny_b, seeded.control, &seeded.file_ids)
            .await
            .is_ok()
    {
        eprintln!("EQUIVALENCE GATE FAILED: control user was granted");
        cleanup(&pool, &seeded).await;
        std::process::exit(1);
    }

    println!("\n#################################################################");
    println!("# batch-favorites authz: serial require loop vs try_join_all");
    println!("# files={n_files} pool={pool_size}  (shared-drive member, editor grant)");
    println!("#################################################################\n");
    println!("| {:<18} | {:>10} | {:>12} |", "arm", "wall ms", "µs/item");

    for (label, joined, warm) in [
        ("serial COLD", false, false),
        ("join    COLD", true, false),
        ("serial WARM", false, true),
        ("join    WARM", true, true),
    ] {
        // COLD: fresh engine per run (empty moka). WARM: prime, then measure.
        let engine = fresh_engine(&pool);
        if warm {
            serial_checks(&engine, seeded.caller, &seeded.file_ids)
                .await
                .expect("prime");
        }
        let t = Instant::now();
        let r = if joined {
            joined_checks(&engine, seeded.caller, &seeded.file_ids).await
        } else {
            serial_checks(&engine, seeded.caller, &seeded.file_ids).await
        };
        let el = t.elapsed();
        r.expect("granted");
        println!(
            "| {:<18} | {:>10.2} | {:>12.2} |",
            label,
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / n_files as f64
        );
    }

    cleanup(&pool, &seeded).await;
    println!("\n(COLD = empty caches: N distinct `drive_of` point-SELECTs — the arm");
    println!(" under test. WARM = all-moka parity check. Fail-fast denial semantics");
    println!(" verified by the control-user gate on both arms.)");
}
