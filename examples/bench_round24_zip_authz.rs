//! Round-24 — `download_zip` per-item authz+metadata N+1 → batch, VALIDATED.
//!
//! `BatchOperations::download_zip` authorized + fetched each selected file with
//! a per-file `get_file_with_perms` (= `require_file` authz + `get_file`) — 2
//! serial round-trips per file, before any streaming. AFTER routes the whole
//! multi-select through `FileRetrievalService::get_files_by_ids_with_perms`,
//! which authorizes every id in ONE `check_files_read_batch` and fetches the
//! authorized ids in ONE `get_files_by_ids` (2 round-trips total). The
//! subsequent `add_file_entry_streamed` keeps its own per-file stream-open Read
//! check + Recents recording (now a primed-cache hit), so authorization still
//! happens BEFORE any ZIP entry is written — a denied file never leaks its name.
//!
//! Because this change is authorization-sensitive, the gate is the security
//! property itself: the batch `check_files_read_batch` must make the EXACT same
//! per-file inclusion decision as the shipped-before per-file `require` loop —
//! same **set** AND same **input order** — over a mix of
//!   • files on a drive the caller is granted `editor` on   (INCLUDED)
//!   • files on a drive the caller has NO grant on          (DENIED)
//!   • ids that don't exist at all                          (MISSING)
//! and the batch fetch must return exactly the authorized, existing files.
//! Any divergence `std::process::exit(1)`s.
//!
//! Drives the REAL `PgAclEngine` + `FileBlobReadRepository` (the fresh_engine
//! shape from bench_favorites_authz).
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo run --release --features bench --example bench_round24_zip_authz
//! Tunables (env): BENCH_FILES (200), BENCH_POOL (20).

use std::collections::HashSet;
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
    other: Uuid,
    drive_a: Uuid,
    drive_b: Uuid,
    root_a: Uuid,
    root_b: Uuid,
    blob_hash: String,
    /// The caller's accessible files (drive A) — the expected INCLUDED set.
    owned: Vec<Uuid>,
    /// Files on drive B (no grant to caller) — expected DENIED.
    denied: Vec<Uuid>,
    /// Non-existent ids — expected MISSING.
    missing: Vec<Uuid>,
    /// The full selection, interleaved owned/denied/missing (order matters).
    selection: Vec<Uuid>,
}

async fn seed(pool: &PgPool, n_files: usize) -> Seeded {
    let mut tx = pool.begin().await.expect("begin");
    let caller: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_zipauthz_a', 'bench_zipauthz_a@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed caller");
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_zipauthz_b', 'bench_zipauthz_b@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed other");

    let blob_hash = "benchzipauthz00000000000000000000000000000000000000000000000b24".to_string();
    sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count) VALUES ($1, 1, 1)")
        .bind(&blob_hash)
        .execute(&mut *tx)
        .await
        .expect("seed blob");

    // Two shared drives; `caller` is granted editor on A only, `other` on B.
    async fn drive_with_grant(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        label: &str,
        grantee: Uuid,
    ) -> (Uuid, Uuid) {
        let drive: Uuid =
            sqlx::query_scalar("INSERT INTO storage.drives (kind) VALUES ('shared') RETURNING id")
                .fetch_one(&mut **tx)
                .await
                .expect("seed drive");
        let root: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, path, lpath, drive_id)
             VALUES ($1, $2, 'x', $3) RETURNING id",
        )
        .bind(format!("Bench {label}"))
        .bind(format!("/Bench {label}"))
        .bind(drive)
        .fetch_one(&mut **tx)
        .await
        .expect("seed folder");
        sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
            .bind(root)
            .bind(drive)
            .execute(&mut **tx)
            .await
            .expect("stamp root");
        sqlx::query(
            "INSERT INTO storage.role_grants
                 (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ('user', $1, 'drive', $2, 'editor'::storage.grant_role, $1)",
        )
        .bind(grantee)
        .bind(drive)
        .execute(&mut **tx)
        .await
        .expect("seed grant");
        (drive, root)
    }

    let (drive_a, root_a) = drive_with_grant(&mut tx, "A", caller).await;
    let (drive_b, root_b) = drive_with_grant(&mut tx, "B", other).await;

    let mut owned = Vec::with_capacity(n_files);
    let mut denied = Vec::with_capacity(n_files);
    for i in 0..n_files {
        for (drive, root, sink) in [
            (drive_a, root_a, &mut owned),
            (drive_b, root_b, &mut denied),
        ] {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
                 VALUES ($1, $2, $3, 1, 'text/plain', $4) RETURNING id",
            )
            .bind(format!("bench-{i:04}.txt"))
            .bind(root)
            .bind(&blob_hash)
            .bind(drive)
            .fetch_one(&mut *tx)
            .await
            .expect("seed file");
            sink.push(id);
        }
    }
    tx.commit().await.expect("commit");

    let missing: Vec<Uuid> = (0..n_files).map(|_| Uuid::new_v4()).collect();

    // Interleave owned / denied / missing so the order test is meaningful.
    let mut selection = Vec::with_capacity(n_files * 3);
    for i in 0..n_files {
        selection.push(owned[i]);
        selection.push(denied[i]);
        selection.push(missing[i]);
    }

    Seeded {
        caller,
        other,
        drive_a,
        drive_b,
        root_a,
        root_b,
        blob_hash,
        owned,
        denied,
        missing,
        selection,
    }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
    for d in [s.drive_a, s.drive_b] {
        let _ = sqlx::query("DELETE FROM storage.role_grants WHERE resource_id = $1")
            .bind(d)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM storage.files WHERE drive_id = $1")
            .bind(d)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
            .bind(d)
            .execute(pool)
            .await;
    }
    for f in [s.root_a, s.root_b] {
        let _ = sqlx::query("DELETE FROM storage.folders WHERE id = $1")
            .bind(f)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = $1")
        .bind(&s.blob_hash)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id IN ($1, $2)")
        .bind(s.caller)
        .bind(s.other)
        .execute(pool)
        .await;
}

fn fresh_engine(pool: &Arc<PgPool>) -> (Arc<PgAclEngine>, Arc<FileBlobReadRepository>) {
    let folder_repo = Arc::new(FolderDbRepository::new(pool.clone()));
    let backend = Arc::new(LocalBlobBackend::new(std::path::Path::new(
        "/tmp/bench-zipauthz-blobs",
    )));
    let dedup = Arc::new(DedupService::new(backend, pool.clone(), pool.clone()));
    let file_repo = Arc::new(FileBlobReadRepository::new(
        pool.clone(),
        dedup,
        folder_repo.clone(),
    ));
    let group_repo = Arc::new(SubjectGroupPgRepository::new(pool.clone()));
    let engine = Arc::new(PgAclEngine::new(
        pool.clone(),
        folder_repo,
        file_repo.clone(),
        group_repo,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    ));
    (engine, file_repo)
}

/// BEFORE, verbatim: the per-file `require` filter, preserving input order.
async fn before_included(engine: &PgAclEngine, user: Uuid, sel: &[Uuid]) -> Vec<Uuid> {
    let mut out = Vec::new();
    for id in sel {
        if engine
            .require(Subject::User(user), Permission::Read, Resource::File(*id))
            .await
            .is_ok()
        {
            out.push(*id);
        }
    }
    out
}

/// AFTER: one batch check, then re-associate in input order (the download_zip
/// re-association).
async fn after_included(engine: &PgAclEngine, user: Uuid, sel: &[Uuid]) -> Vec<Uuid> {
    let allowed: HashSet<Uuid> = engine
        .check_files_read_batch(Subject::User(user), sel)
        .await
        .expect("batch check");
    sel.iter()
        .copied()
        .filter(|id| allowed.contains(id))
        .collect()
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

    // Clear any prior fixtures, then seed.
    let _ = sqlx::query(
        "DELETE FROM auth.users WHERE email IN ('bench_zipauthz_a@bench.invalid','bench_zipauthz_b@bench.invalid')",
    )
    .execute(pool.as_ref())
    .await;
    let seeded = seed(&pool, n_files).await;

    // ── Equivalence gate (fresh engines so neither arm rides the other's cache) ──
    let (eng_before, _) = fresh_engine(&pool);
    let (eng_after, file_repo) = fresh_engine(&pool);
    let before = before_included(&eng_before, seeded.caller, &seeded.selection).await;
    let after = after_included(&eng_after, seeded.caller, &seeded.selection).await;

    let owned_set: HashSet<Uuid> = seeded.owned.iter().copied().collect();
    let denied_set: HashSet<Uuid> = seeded.denied.iter().copied().collect();
    let missing_set: HashSet<Uuid> = seeded.missing.iter().copied().collect();

    let mut fail = false;
    if before != after {
        eprintln!("GATE FAIL: batch inclusion set/order != per-file require loop");
        fail = true;
    }
    // The included set must be EXACTLY the caller's owned files, in input order.
    let expected: Vec<Uuid> = seeded
        .selection
        .iter()
        .copied()
        .filter(|id| owned_set.contains(id))
        .collect();
    if after != expected {
        eprintln!("GATE FAIL: included set is not exactly the caller's owned files (in order)");
        fail = true;
    }
    if after.iter().any(|id| denied_set.contains(id)) {
        eprintln!("GATE FAIL: a DENIED (other-drive) file was included — authz regression!");
        fail = true;
    }
    if after.iter().any(|id| missing_set.contains(id)) {
        eprintln!("GATE FAIL: a MISSING id was included");
        fail = true;
    }
    // The batch fetch of the authorized ids must return exactly those files.
    let allowed_ids: Vec<String> = after.iter().map(Uuid::to_string).collect();
    let fetched = file_repo
        .get_files_by_ids(&allowed_ids)
        .await
        .expect("batch fetch");
    let fetched_ids: HashSet<Uuid> = fetched
        .iter()
        .filter_map(|f| Uuid::parse_str(f.id()).ok())
        .collect();
    if fetched_ids != owned_set {
        eprintln!("GATE FAIL: batch fetch of authorized ids != owned files");
        fail = true;
    }
    if fail {
        cleanup(&pool, &seeded).await;
        std::process::exit(1);
    }

    println!("\n#################################################################");
    println!("# download_zip authz+metadata: per-file require loop vs batch");
    println!(
        "# selection = {n} owned + {n} denied + {n} missing (interleaved)",
        n = n_files
    );
    println!("# gate OK: identical inclusion set+order; denied+missing excluded;");
    println!(
        "#          batch fetch returns exactly the {} owned files.",
        seeded.owned.len()
    );
    println!("#################################################################\n");
    println!("| {:<26} | {:>10} | {:>12} |", "arm", "wall ms", "µs/file");

    // Latency: cold engine each run (empty caches — the first-download shape).
    let total = seeded.selection.len();
    for (label, batch) in [
        ("per-file require loop", false),
        ("batch check_files_read", true),
    ] {
        let (engine, _) = fresh_engine(&pool);
        let t = Instant::now();
        let got = if batch {
            after_included(&engine, seeded.caller, &seeded.selection).await
        } else {
            before_included(&engine, seeded.caller, &seeded.selection).await
        };
        let el = t.elapsed();
        assert_eq!(got.len(), seeded.owned.len(), "arm {label} inclusion count");
        println!(
            "| {:<26} | {:>10.2} | {:>12.2} |",
            label,
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / total as f64
        );
    }

    cleanup(&pool, &seeded).await;
    println!("\nAll Round-24 authz-equivalence gates passed.");
}
