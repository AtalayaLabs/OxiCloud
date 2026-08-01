//! Range-seek per-request authz duplication benchmark.
//!
//! `download_file_impl` calls `get_file_with_perms` once (authz + access
//! notify + metadata) and THEN, in the Range branch, called
//! `get_file_range_preloaded_with_perms` — which re-ran `require_file`
//! (authz) + `notify_file_accessed` per request. Media players and PDF
//! viewers fetch a file *exclusively* through Range requests: a `bytes=0-`
//! probe then one request per seek. So every seek in a scrub re-authorized a
//! file the request-level gate had already cleared.
//!
//! Round 7 drops the range branch to the non-perms `get_file_range_preloaded`
//! (the share-landing and WebDAV range paths already do exactly this). This
//! bench isolates the per-seek `require` that AFTER eliminates, driving the
//! REAL `PgAclEngine`:
//!   - WARM: the cache the initial `get_file_with_perms` warmed — each removed
//!     seek-check was a moka hit + uuid parse (pure CPU/alloc).
//!   - COLD: a shared-drive recipient whose drive-role cache expired mid-scrub
//!     (30 s TTL) — each removed seek-check was a full drive-resolve query.
//!
//! Safety gate: the surviving request-level gate still authorizes correctly —
//! the member is granted, a non-member is denied — so removing the per-seek
//! re-check bypasses nothing.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo run --release --features bench --example bench_range_seek_authz
//! Tunables (env): BENCH_SEEKS (200), BENCH_POOL (8).

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
    member: Uuid,
    outsider: Uuid,
    drive_id: Uuid,
    root_folder: Uuid,
    blob_hash: String,
    file_id: Uuid,
}

async fn seed(pool: &PgPool) -> Seeded {
    let mut tx = pool.begin().await.expect("begin");
    let member: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_rangeseek', 'bench_rangeseek@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed member");
    let outsider: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_rangeseek_out', 'bench_rangeseek_out@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed outsider");

    let drive_id: Uuid =
        sqlx::query_scalar("INSERT INTO storage.drives (kind) VALUES ('shared') RETURNING id")
            .fetch_one(&mut *tx)
            .await
            .expect("seed drive");
    let root_folder: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('Bench Seek', '/Bench Seek', 'x', $1) RETURNING id",
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
         VALUES ('user', $1, 'drive', $2, 'viewer'::storage.grant_role, $1)",
    )
    .bind(member)
    .bind(drive_id)
    .execute(&mut *tx)
    .await
    .expect("seed grant");

    let blob_hash = "benchrangeseek00000000000000000000000000000000000000000000000b3".to_string();
    sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count) VALUES ($1, 1048576, 1)")
        .bind(&blob_hash)
        .execute(&mut *tx)
        .await
        .expect("seed blob");
    let file_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
         VALUES ('clip.mp4', $1, $2, 1048576, 'video/mp4', $3) RETURNING id",
    )
    .bind(root_folder)
    .bind(&blob_hash)
    .bind(drive_id)
    .fetch_one(&mut *tx)
    .await
    .expect("seed file");
    tx.commit().await.expect("commit");
    Seeded {
        member,
        outsider,
        drive_id,
        root_folder,
        blob_hash,
        file_id,
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
        .bind(s.member)
        .bind(s.outsider)
        .execute(pool)
        .await;
}

fn fresh_engine(pool: &Arc<PgPool>) -> Arc<PgAclEngine> {
    let folder_repo = Arc::new(FolderDbRepository::new(pool.clone()));
    let backend = Arc::new(LocalBlobBackend::new(std::path::Path::new(
        "/tmp/bench-rangeseek-blobs",
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

/// The per-seek check the range branch used to run (verbatim: uuid parse +
/// `authz.require`, exactly `require_file`'s body).
async fn seek_require(engine: &Arc<PgAclEngine>, caller: Uuid, file_id: Uuid) -> bool {
    engine
        .require(
            Subject::User(caller),
            Permission::Read,
            Resource::File(file_id),
        )
        .await
        .is_ok()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let seeks: usize = env_or("BENCH_SEEKS", 200);
    let pool_size: u32 = env_or("BENCH_POOL", 8);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(pool_size)
            .min_connections(pool_size)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect Postgres"),
    );

    let s = seed(&pool).await;

    // ── Safety gate: the surviving request-level gate authorizes correctly ──
    let gate = fresh_engine(&pool);
    let member_ok = seek_require(&gate, s.member, s.file_id).await;
    let outsider_denied = !seek_require(&gate, s.outsider, s.file_id).await;
    if !member_ok || !outsider_denied {
        eprintln!(
            "SAFETY GATE FAILED: member_ok={member_ok} outsider_denied={outsider_denied} \
             (the single request-level authz must still grant the member and deny the outsider)"
        );
        cleanup(&pool, &s).await;
        std::process::exit(1);
    }

    println!("\n#################################################################");
    println!("# range-seek authz duplication: per-seek require (BEFORE) vs 0 (AFTER)");
    println!("# seeks/scrub={seeks}  (member of a shared drive, viewer grant)");
    println!("#################################################################\n");
    println!("| {:<26} | {:>10} | {:>12} |", "arm", "wall ms", "µs/seek");

    // WARM: one require warms owner_cache + drive_role_cache (as the handler's
    // get_file_with_perms does), then the scrub's per-seek re-checks are moka
    // hits — pure CPU/alloc the AFTER path removes.
    {
        let engine = fresh_engine(&pool);
        seek_require(&engine, s.member, s.file_id).await; // warm
        let t = Instant::now();
        for _ in 0..seeks {
            std::hint::black_box(seek_require(&engine, s.member, s.file_id).await);
        }
        let el = t.elapsed();
        println!(
            "| {:<26} | {:>10.2} | {:>12.2} |",
            "BEFORE per-seek (WARM)",
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / seeks as f64
        );
    }

    // COLD: a fresh engine per seek models a cross-drive recipient or a
    // drive-role-cache entry that expired mid-scrub (30 s TTL) — each removed
    // re-check was a full grant-cascade drive-resolve query.
    {
        let t = Instant::now();
        for _ in 0..seeks {
            let engine = fresh_engine(&pool);
            std::hint::black_box(seek_require(&engine, s.member, s.file_id).await);
        }
        let el = t.elapsed();
        println!(
            "| {:<26} | {:>10.2} | {:>12.2} |",
            "BEFORE per-seek (COLD)",
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / seeks as f64
        );
    }

    println!(
        "| {:<26} | {:>10.2} | {:>12.2} |",
        "AFTER per-seek (removed)", 0.0, 0.0
    );

    cleanup(&pool, &s).await;
    println!("\n(AFTER runs zero per-seek authz: the request-level get_file_with_perms");
    println!(" already authorized + recorded the access. WARM = the moka/CPU cost removed");
    println!(" per seek; COLD = the drive-resolve query removed per seek when the cache");
    println!(" isn't warm. notify_file_accessed (a throttled hook call) is likewise");
    println!(" removed per seek. Safety gate: member granted, outsider denied.)");
}
