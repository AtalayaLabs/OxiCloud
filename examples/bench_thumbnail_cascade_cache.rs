//! Shared-album thumbnail authz benchmark — folder-grant cascade query per
//! thumbnail vs the `cascade_grant_cache`.
//!
//! A recipient of a shared folder (a grant on the album folder, NOT drive
//! membership) fails the drive-role precheck in `PgAclEngine::check_inner` and
//! falls through to `file_cascade_grant_exists` — an ltree folder-ancestor
//! grant query — for EVERY file. `get_thumbnail_impl` runs that Read check on
//! every request, and browsers revalidate immutable thumbnails constantly
//! (`If-None-Match`), so the same `(recipient, file, Read)` decision is
//! recomputed again and again: ~one grant query per thumbnail per view.
//!
//! Round 8 memoises that decision in `cascade_grant_cache` (30 s TTL, flushed
//! on any File/Folder grant write). The check still runs on every request —
//! it is never skipped — but after the first query it resolves in-memory.
//!
//! Round 9 additionally decomposes the FILE decision: parent point-read
//! (memoised) → the FOLDER cascade decision (one ltree query per folder,
//! shared by every sibling) → direct-file-grant fallback. A shared album's
//! COLD first view drops from one ltree UNION query per file to one ltree
//! query per FOLDER plus cheap PK reads. The `ROUND8 cold` arm below runs
//! the historical UNION verbatim per file for comparison.
//!
//! Safety gates (hard asserts, exit 1 on failure):
//!   1. the folder-grant recipient is allowed; an outsider is denied;
//!   2. REVOCATION — after a warm cache serves `allowed`, `clear_role` on the
//!      shared folder makes the very next check DENY (proves the grant-write
//!      invalidation flushes the cache; without it the stale `true` would
//!      still serve);
//!   3. DIRECT-GRANT SIBLING (round 9) — a caller holding ONLY a direct
//!      grant on one file is allowed that file and denied its siblings,
//!      proving the folder-level decomposition neither shadows direct file
//!      grants nor leaks a file decision to siblings.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo run --release --features bench --example bench_thumbnail_cascade_cache
//! Tunables (env): BENCH_THUMBS (100), BENCH_POOL (8).

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxicloud::application::ports::authorization_ports::AuthorizationEngine;
use oxicloud::domain::services::authorization::{
    Permission, Resource, Role, Subject, roles_implying,
};
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
    owner: Uuid,
    recipient: Uuid,
    outsider: Uuid,
    drive_id: Uuid,
    root_folder: Uuid,
    album_folder: Uuid,
    blob_hash: String,
    files: Vec<Uuid>,
}

async fn seed(pool: &PgPool, n_thumbs: usize) -> Seeded {
    let mut tx = pool.begin().await.expect("begin");
    let owner: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_thumbowner', 'bench_thumbowner@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed owner");
    let recipient: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_thumbrecip', 'bench_thumbrecip@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed recipient");
    let outsider: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_thumbout', 'bench_thumbout@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("seed outsider");

    // Owner's personal drive with a root and an album subfolder. The recipient
    // is NOT a drive member — only granted the album folder below, so their
    // File checks fall through the drive precheck to the folder cascade.
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, default_for_user) VALUES ('personal', $1) RETURNING id",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await
    .expect("seed drive");
    let root_folder: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('Personal', '/Personal', 'benchthumbroot', $1) RETURNING id",
    )
    .bind(drive_id)
    .fetch_one(&mut *tx)
    .await
    .expect("seed root");
    sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
        .bind(root_folder)
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .expect("stamp root");
    let album_folder: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id, parent_id)
         VALUES ('Album', '/Personal/Album', 'benchthumbroot.album', $1, $2) RETURNING id",
    )
    .bind(drive_id)
    .bind(root_folder)
    .fetch_one(&mut *tx)
    .await
    .expect("seed album");
    // Owner grant on the drive (personal-drive owner floor), and the recipient
    // grant on the ALBUM FOLDER only — the shared-album shape.
    sqlx::query(
        "INSERT INTO storage.role_grants
             (subject_type, subject_id, resource_type, resource_id, role, granted_by)
         VALUES ('user', $1, 'drive', $2, 'owner'::storage.grant_role, $1)",
    )
    .bind(owner)
    .bind(drive_id)
    .execute(&mut *tx)
    .await
    .expect("seed owner grant");
    sqlx::query(
        "INSERT INTO storage.role_grants
             (subject_type, subject_id, resource_type, resource_id, role, granted_by)
         VALUES ('user', $1, 'folder', $2, 'viewer'::storage.grant_role, $3)",
    )
    .bind(recipient)
    .bind(album_folder)
    .bind(owner)
    .execute(&mut *tx)
    .await
    .expect("seed recipient folder grant");

    let blob_hash = "benchthumbcascade00000000000000000000000000000000000000000000b4".to_string();
    sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count) VALUES ($1, 4096, 1)")
        .bind(&blob_hash)
        .execute(&mut *tx)
        .await
        .expect("seed blob");
    let mut files = Vec::with_capacity(n_thumbs);
    for i in 0..n_thumbs {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
             VALUES ($1, $2, $3, 4096, 'image/jpeg', $4) RETURNING id",
        )
        .bind(format!("photo-{i:04}.jpg"))
        .bind(album_folder)
        .bind(&blob_hash)
        .bind(drive_id)
        .fetch_one(&mut *tx)
        .await
        .expect("seed file");
        files.push(id);
    }
    tx.commit().await.expect("commit");
    Seeded {
        owner,
        recipient,
        outsider,
        drive_id,
        root_folder,
        album_folder,
        blob_hash,
        files,
    }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
    let _ = sqlx::query(
        "DELETE FROM storage.role_grants WHERE resource_id IN ($1, $2) OR resource_id = ANY($3)",
    )
    .bind(s.drive_id)
    .bind(s.album_folder)
    .bind(&s.files)
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
    let _ = sqlx::query("DELETE FROM storage.folders WHERE id IN ($1, $2)")
        .bind(s.album_folder)
        .bind(s.root_folder)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = $1")
        .bind(&s.blob_hash)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id IN ($1, $2, $3)")
        .bind(s.owner)
        .bind(s.recipient)
        .bind(s.outsider)
        .execute(pool)
        .await;
}

fn fresh_engine(pool: &Arc<PgPool>) -> Arc<PgAclEngine> {
    let folder_repo = Arc::new(FolderDbRepository::new(pool.clone()));
    let backend = Arc::new(LocalBlobBackend::new(std::path::Path::new(
        "/tmp/bench-thumbcascade-blobs",
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

async fn allowed(engine: &Arc<PgAclEngine>, caller: Uuid, file: Uuid) -> bool {
    engine
        .require(
            Subject::User(caller),
            Permission::Read,
            Resource::File(file),
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
    let thumbs: usize = env_or("BENCH_THUMBS", 100);
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

    let s = seed(&pool, thumbs).await;

    // ── Safety gate 1: recipient allowed on every file, outsider denied ──
    {
        let engine = fresh_engine(&pool);
        for &f in &s.files {
            if !allowed(&engine, s.recipient, f).await {
                eprintln!("SAFETY GATE FAILED: folder-grant recipient denied a file in the album");
                cleanup(&pool, &s).await;
                std::process::exit(1);
            }
        }
        if allowed(&engine, s.outsider, s.files[0]).await {
            eprintln!("SAFETY GATE FAILED: outsider was allowed");
            cleanup(&pool, &s).await;
            std::process::exit(1);
        }
    }

    // ── Safety gate 2: revocation flushes the cache (immediate deny) ──
    {
        let engine = fresh_engine(&pool);
        // Warm: caches (recipient, File[0], Read) → true.
        assert!(allowed(&engine, s.recipient, s.files[0]).await);
        // Revoke the album share through the real grant-write path.
        engine
            .clear_role(Subject::User(s.recipient), Resource::Folder(s.album_folder))
            .await
            .expect("clear_role");
        // Next check MUST deny — a stale cached `true` here would be a hole.
        if allowed(&engine, s.recipient, s.files[0]).await {
            eprintln!(
                "SAFETY GATE FAILED: recipient still allowed after clear_role — \
                 cascade cache was not invalidated on grant revoke"
            );
            cleanup(&pool, &s).await;
            std::process::exit(1);
        }
        // Re-grant for the perf run below.
        engine
            .set_role(
                s.owner,
                Subject::User(s.recipient),
                Role::Viewer,
                Resource::Folder(s.album_folder),
                None,
            )
            .await
            .expect("re-grant");
    }

    // ── Safety gate 3 (round 9): direct-grant sibling isolation ──
    // The outsider gets a DIRECT grant on file[0] only (no folder/drive
    // grant): they must be allowed file[0] — the folder half of the
    // decomposition denies, the direct half matches — and denied file[1]
    // even immediately after the allowed check (no sibling leak through
    // the folder-level cache).
    {
        let engine = fresh_engine(&pool);
        engine
            .set_role(
                s.owner,
                Subject::User(s.outsider),
                Role::Viewer,
                Resource::File(s.files[0]),
                None,
            )
            .await
            .expect("direct file grant");
        if !allowed(&engine, s.outsider, s.files[0]).await {
            eprintln!(
                "SAFETY GATE FAILED: direct file grant denied — the folder-level \
                 decomposition shadowed the direct-grant branch"
            );
            cleanup(&pool, &s).await;
            std::process::exit(1);
        }
        if allowed(&engine, s.outsider, s.files[1]).await {
            eprintln!(
                "SAFETY GATE FAILED: direct grant on file[0] leaked to a sibling — \
                 a file decision must never authorize other files"
            );
            cleanup(&pool, &s).await;
            std::process::exit(1);
        }
        engine
            .clear_role(Subject::User(s.outsider), Resource::File(s.files[0]))
            .await
            .expect("clear direct grant");
    }

    println!("\n#################################################################");
    println!("# shared-album thumbnail authz: folder-cascade query/thumb vs cache");
    println!("# thumbs={thumbs}  (recipient holds a folder grant, no drive membership)");
    println!("#################################################################\n");
    println!("| {:<28} | {:>10} | {:>12} |", "arm", "wall ms", "µs/thumb");

    // BEFORE: no cache — a fresh engine per thumbnail forces the cascade query
    // every time (models the pre-round-8 per-request behaviour).
    {
        let t = Instant::now();
        for &f in &s.files {
            let engine = fresh_engine(&pool);
            std::hint::black_box(allowed(&engine, s.recipient, f).await);
        }
        let el = t.elapsed();
        println!(
            "| {:<28} | {:>10.2} | {:>12.2} |",
            "BEFORE (query/thumb)",
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / thumbs as f64
        );
    }

    // ROUND8 cold: the historical per-file UNION (direct grant ∨ ltree
    // ancestor join) run verbatim once per file — what a cold first view
    // cost before the round-9 folder-level decomposition.
    {
        let subject_types: Vec<&str> = vec!["user", "group"];
        let subject_ids = vec![s.recipient];
        let roles: Vec<&str> = roles_implying(Permission::Read)
            .iter()
            .map(|r| r.as_str())
            .collect();
        let t = Instant::now();
        for &f in &s.files {
            let exists: Option<i32> = sqlx::query_scalar(
                r#"
                SELECT 1
                  FROM (
                    SELECT 1
                      FROM storage.role_grants
                     WHERE subject_type = ANY($1)
                       AND subject_id   = ANY($2)
                       AND role         = ANY($3::storage.grant_role[])
                       AND resource_type = 'file' AND resource_id = $4
                       AND (expires_at IS NULL OR expires_at > NOW())
                    UNION ALL
                    SELECT 1
                      FROM storage.role_grants g
                      JOIN storage.folders gf     ON gf.id = g.resource_id
                      JOIN storage.files target_f ON target_f.id = $4
                     WHERE g.subject_type  = ANY($1)
                       AND g.subject_id    = ANY($2)
                       AND g.role          = ANY($3::storage.grant_role[])
                       AND g.resource_type = 'folder'
                       AND (g.expires_at IS NULL OR g.expires_at > NOW())
                       AND target_f.folder_id IS NOT NULL
                       AND gf.lpath @> (SELECT lpath FROM storage.folders
                                         WHERE id = target_f.folder_id)
                  ) any_match
                 LIMIT 1
                "#,
            )
            .bind(&subject_types)
            .bind(&subject_ids)
            .bind(&roles)
            .bind(f)
            .fetch_optional(pool.as_ref())
            .await
            .expect("round8 union query");
            assert!(exists.is_some(), "ROUND8 arm: recipient must be allowed");
        }
        let el = t.elapsed();
        println!(
            "| {:<28} | {:>10.2} | {:>12.2} |",
            "ROUND8 cold (union/file)",
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / thumbs as f64
        );
    }

    // AFTER cold: one persistent engine — the first grid view resolves each
    // file's parent (PK read) and shares ONE folder-cascade decision.
    let engine = fresh_engine(&pool);
    {
        let t = Instant::now();
        for &f in &s.files {
            std::hint::black_box(allowed(&engine, s.recipient, f).await);
        }
        let el = t.elapsed();
        println!(
            "| {:<28} | {:>10.2} | {:>12.2} |",
            "AFTER cold (first view)",
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / thumbs as f64
        );
    }

    // AFTER warm: revalidation re-checks the same files — all cache hits, the
    // "navigate away and back" / constant If-None-Match revalidation case.
    {
        let t = Instant::now();
        for &f in &s.files {
            std::hint::black_box(allowed(&engine, s.recipient, f).await);
        }
        let el = t.elapsed();
        println!(
            "| {:<28} | {:>10.2} | {:>12.2} |",
            "AFTER warm (revalidation)",
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / thumbs as f64
        );
    }

    // ── ROUND10: the CONCURRENT cold herd ────────────────────────────
    // A browser grid fires its thumbnail requests near-simultaneously, so
    // the real cold first view is K in-flight checks, not a sequential
    // loop. BEFORE (round-9 shape): every request pays its own parent
    // point read — replicated below as K concurrent `SELECT folder_id`
    // probes + the shared folder decision. AFTER: the engine's parent
    // batcher drains the herd into ~2 queries.
    {
        // BEFORE replica: K concurrent point reads (the R9 per-request work).
        let t = Instant::now();
        let probes = s.files.iter().map(|&f| {
            let pool = pool.clone();
            async move {
                let parent: Option<Option<Uuid>> =
                    sqlx::query_scalar("SELECT folder_id FROM storage.files WHERE id = $1")
                        .bind(f)
                        .fetch_optional(pool.as_ref())
                        .await
                        .expect("point parent read");
                parent.flatten()
            }
        });
        let before_parents = futures::future::join_all(probes).await;
        let el = t.elapsed();
        println!(
            "| {:<28} | {:>10.2} | {:>12.2} |",
            "R9 herd (point read/file)",
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / thumbs as f64
        );

        // AFTER: fresh engine, all K checks in flight at once.
        let herd_engine = fresh_engine(&pool);
        let t = Instant::now();
        let checks = s
            .files
            .iter()
            .map(|&f| allowed(&herd_engine, s.recipient, f));
        let results = futures::future::join_all(checks).await;
        let el = t.elapsed();
        let parent_queries = herd_engine.parent_query_count();
        println!(
            "| {:<28} | {:>10.2} | {:>12.2} |",
            "AFTER herd (batched)",
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e6 / thumbs as f64
        );
        println!(
            "|   parent queries for the {thumbs}-thumb herd: {parent_queries} (was {thumbs})            |"
        );

        // Gates: every check allowed; the herd collapsed (≤8 queries for a
        // 100-wide herd would already be a pass; typical is 2-3); and the
        // batcher's answers match the point reads exactly.
        if results.iter().any(|ok| !ok) {
            eprintln!("SAFETY GATE FAILED: batched herd denied an allowed thumbnail");
            cleanup(&pool, &s).await;
            std::process::exit(1);
        }
        if parent_queries as usize >= thumbs / 4 {
            eprintln!(
                "PERF GATE FAILED: parent batcher issued {parent_queries} queries for a {thumbs}-thumb herd"
            );
            cleanup(&pool, &s).await;
            std::process::exit(1);
        }
        for (i, &f) in s.files.iter().enumerate() {
            let via_engine: Option<Option<Uuid>> =
                sqlx::query_scalar("SELECT folder_id FROM storage.files WHERE id = $1")
                    .bind(f)
                    .fetch_optional(pool.as_ref())
                    .await
                    .expect("verify parent");
            assert_eq!(
                via_engine.flatten(),
                before_parents[i],
                "parent resolution must be identical"
            );
        }
    }

    cleanup(&pool, &s).await;
    println!("\n(The check is never skipped — authz still runs on every thumbnail; only");
    println!(" the folder-cascade DECISION is memoised. BEFORE re-queries per request;");
    println!(" AFTER warm serves revalidations from memory. Safety gates verified:");
    println!(" recipient allowed, outsider denied, and a clear_role revoke denies");
    println!(" immediately — the grant write flushed the cache.)");
}
