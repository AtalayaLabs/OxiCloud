//! Round-11 query-shape pack — BEFORE query shapes vs AFTER (needs Postgres).
//!
//! Sections:
//!   1. Deferred upload registration (the default REST upload path):
//!      3 round-trips (parent drive SELECT → INSERT → parent path SELECT,
//!      the middle two re-reading the SAME folders row) vs the single
//!      `WITH parent AS (…) INSERT … RETURNING` template `persist_file`
//!      already uses. Gate: identical returned (path, drive) + identical
//!      not-found semantics for a missing parent.
//!   2. Calendar/AddressBook/Playlist authz: the only `check()` arms with
//!      no result cache — `role_grants` point query per check vs a moka
//!      `direct_grant_cache` hit. Gate: same verdict + revocation flip
//!      after invalidate.
//!   3. `expand_user` cache miss: `is_external` + recursive groups CTE
//!      awaited serially vs `tokio::join!`. Gate: same result set.
//!   4. Places geo clusters: `min(fm.file_id::text)` (casts every row)
//!      vs `min(fm.file_id)::text` (one cast per cluster). Gate:
//!      identical cluster rows (uuid byte order == canonical text order).
//!   5. Recluster persistence: F sequential `assign_person` UPDATEs vs
//!      one `UPDATE … FROM unnest($1,$2)` batch. Gate: identical final
//!      `person_id` column state.
//!
//! Run (needs Postgres; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_round11_queries
//! Tunables (env): BENCH_PASSES (200)

use std::sync::Arc;
use std::time::Instant;

use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn p50(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

async fn timed<F, Fut, R>(passes: usize, mut f: F) -> (f64, R)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let mut last = f().await;
    let mut samples = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        last = f().await;
        samples.push(t.elapsed().as_secs_f64() * 1e3);
    }
    (p50(samples), last)
}

fn gate(name: &str, ok: bool) {
    if ok {
        println!("    gate[{name}]: OK");
    } else {
        println!("    gate[{name}]: FAILED — DO NOT SHIP THIS SECTION");
    }
}

struct Seed {
    owner: Uuid,
    drive: Uuid,
    root: Uuid,
}

async fn seed_base(pool: &PgPool, tag: &str) -> Seed {
    // Idempotent sweep of leftovers from an aborted earlier run.
    let _ = sqlx::query(
        "DELETE FROM storage.files WHERE drive_id IN
             (SELECT id FROM storage.drives WHERE default_for_user IN
                 (SELECT id FROM auth.users WHERE username = $1))",
    )
    .bind(format!("bench_r11_{tag}"))
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM storage.folders WHERE lpath = $1::ltree")
        .bind(format!("br11{tag}"))
        .execute(pool)
        .await;
    let _ = sqlx::query(
        "DELETE FROM storage.drives WHERE default_for_user IN
             (SELECT id FROM auth.users WHERE username = $1)",
    )
    .bind(format!("bench_r11_{tag}"))
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE username = $1")
        .bind(format!("bench_r11_{tag}"))
        .execute(pool)
        .await;

    let mut tx = pool.begin().await.expect("begin seed tx");
    let owner: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ($1, $2, 'user') RETURNING id",
    )
    .bind(format!("bench_r11_{tag}"))
    .bind(format!("bench_r11_{tag}@bench.invalid"))
    .fetch_one(&mut *tx)
    .await
    .expect("seed owner");
    let drive: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, default_for_user, policies)
         VALUES ('personal', $1, '{\"include_in_photo_index\": true}'::jsonb) RETURNING id",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await
    .expect("seed drive");
    let root: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('Personal', '/Personal', $2::ltree, $1) RETURNING id",
    )
    .bind(drive)
    .bind(format!("br11{tag}"))
    .fetch_one(&mut *tx)
    .await
    .expect("seed root");
    sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
        .bind(root)
        .bind(drive)
        .execute(&mut *tx)
        .await
        .expect("stamp root");
    sqlx::query(
        "INSERT INTO storage.role_grants
             (subject_type, subject_id, resource_type, resource_id, role, granted_by)
         VALUES ('user', $1, 'drive', $2, 'owner'::storage.grant_role, $1)",
    )
    .bind(owner)
    .bind(drive)
    .execute(&mut *tx)
    .await
    .expect("seed owner grant");
    tx.commit().await.expect("commit seed tx");
    Seed { owner, drive, root }
}

async fn cleanup_base(pool: &PgPool, s: &Seed) {
    let _ = sqlx::query("DELETE FROM storage.role_grants WHERE subject_id = $1 OR granted_by = $1")
        .bind(s.owner)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.files WHERE drive_id = $1")
        .bind(s.drive)
        .execute(pool)
        .await;
    let _ = sqlx::query("UPDATE storage.drives SET root_folder_id = NULL WHERE id = $1")
        .bind(s.drive)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.folders WHERE id = $1")
        .bind(s.root)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(s.drive)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(s.owner)
        .execute(pool)
        .await;
}

// ─── §1 deferred upload registration ────────────────────────────────────────

const PLACEHOLDER: &str = "0000000000000000000000000000000000000000000000000000000000000000";

async fn deferred_before(
    pool: &PgPool,
    name: &str,
    folder_id: Uuid,
    caller: Uuid,
) -> (String, String, Uuid) {
    // Q1: resolve_parent_drive
    let drive_id: Uuid =
        sqlx::query_scalar("SELECT drive_id FROM storage.folders WHERE id = $1::uuid")
            .bind(folder_id.to_string())
            .fetch_optional(pool)
            .await
            .expect("q1")
            .expect("parent exists");
    // Q2: INSERT
    let row: (String, i64, i64) = sqlx::query_as(
        r#"
        INSERT INTO storage.files
            (name, folder_id, drive_id, blob_hash, size,
             mime_type, category_order, created_by, updated_by)
        VALUES ($1, $2::uuid, $3, $4, $5, $6, $7, $8, $8)
        RETURNING id::text,
                  EXTRACT(EPOCH FROM created_at)::bigint,
                  EXTRACT(EPOCH FROM updated_at)::bigint
        "#,
    )
    .bind(name)
    .bind(folder_id.to_string())
    .bind(drive_id)
    .bind(PLACEHOLDER)
    .bind(4096i64)
    .bind("application/octet-stream")
    .bind(9999i16)
    .bind(caller)
    .fetch_one(pool)
    .await
    .expect("q2");
    // Q3: lookup_folder_path
    let path: String = sqlx::query_scalar("SELECT path FROM storage.folders WHERE id = $1::uuid")
        .bind(folder_id.to_string())
        .fetch_optional(pool)
        .await
        .expect("q3")
        .expect("parent exists");
    (row.0, path, drive_id)
}

async fn deferred_after(
    pool: &PgPool,
    name: &str,
    folder_id: Uuid,
    caller: Uuid,
) -> Option<(String, String, Uuid)> {
    let row: Option<(String, String, Uuid, i64, i64)> = sqlx::query_as(
        r#"
        WITH parent AS (
            SELECT id, drive_id, path FROM storage.folders WHERE id = $2::uuid
        )
        INSERT INTO storage.files
            (name, folder_id, drive_id, blob_hash, size,
             mime_type, category_order, created_by, updated_by)
        SELECT $1, parent.id, parent.drive_id, $3, $4, $5, $6, $7, $7
          FROM parent
        RETURNING id::text,
                  (SELECT path FROM parent),
                  drive_id,
                  EXTRACT(EPOCH FROM created_at)::bigint,
                  EXTRACT(EPOCH FROM updated_at)::bigint
        "#,
    )
    .bind(name)
    .bind(folder_id.to_string())
    .bind(PLACEHOLDER)
    .bind(4096i64)
    .bind("application/octet-stream")
    .bind(9999i16)
    .bind(caller)
    .fetch_optional(pool)
    .await
    .expect("cte insert");
    row.map(|(id, path, drive, _, _)| (id, path, drive))
}

async fn section_deferred(pool: &PgPool, s: &Seed, passes: usize) {
    println!("  §1 deferred upload registration (per uploaded file)");

    // Gates: identical (path, drive); missing parent → 0 rows (not-found).
    let b = deferred_before(pool, "gate-b.bin", s.root, s.owner).await;
    let a = deferred_after(pool, "gate-a.bin", s.root, s.owner)
        .await
        .expect("row");
    gate("path+drive identical", b.1 == a.1 && b.2 == a.2);
    let missing = deferred_after(pool, "gate-m.bin", Uuid::new_v4(), s.owner).await;
    gate("missing parent → not-found", missing.is_none());
    let _ = sqlx::query("DELETE FROM storage.files WHERE blob_hash = $1")
        .bind(PLACEHOLDER)
        .execute(pool)
        .await;

    let (ms_b, _) = timed(passes, || async {
        let r = deferred_before(pool, "bench-b.bin", s.root, s.owner).await;
        let _ = sqlx::query("DELETE FROM storage.files WHERE id = $1::uuid")
            .bind(&r.0)
            .execute(pool)
            .await;
        r.2
    })
    .await;
    let (ms_a, _) = timed(passes, || async {
        let r = deferred_after(pool, "bench-a.bin", s.root, s.owner)
            .await
            .unwrap();
        let _ = sqlx::query("DELETE FROM storage.files WHERE id = $1::uuid")
            .bind(&r.0)
            .execute(pool)
            .await;
        r.2
    })
    .await;
    // Both arms pay the same cleanup DELETE; the delta is the 3-vs-1 shape.
    println!("    BEFORE 3 round-trips  p50 {ms_b:.3} ms   (incl. cleanup DELETE)");
    println!("    AFTER  1 CTE insert   p50 {ms_a:.3} ms   (incl. cleanup DELETE)");
}

// ─── §2 calendar direct-grant cache ─────────────────────────────────────────

async fn direct_grant_query(pool: &PgPool, subject: Uuid, cal: Uuid) -> bool {
    sqlx::query_scalar::<_, i32>(
        "SELECT 1 FROM storage.role_grants
          WHERE subject_type = ANY($1) AND subject_id = ANY($2)
            AND role = ANY($3::storage.grant_role[])
            AND resource_type = $4 AND resource_id = $5
            AND (expires_at IS NULL OR expires_at > NOW())
          LIMIT 1",
    )
    .bind(vec!["user"])
    .bind(vec![subject])
    .bind(vec![
        "owner",
        "editor",
        "contributor",
        "commenter",
        "viewer",
    ])
    .bind("calendar")
    .bind(cal)
    .fetch_optional(pool)
    .await
    .expect("grant query")
    .is_some()
}

async fn section_grant_cache(pool: &Arc<PgPool>, s: &Seed, passes: usize) {
    println!("  §2 Calendar/AddressBook/Playlist authz check (per DAV request)");
    let cal = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO storage.role_grants
             (subject_type, subject_id, resource_type, resource_id, role, granted_by)
         VALUES ('user', $1, 'calendar', $2, 'owner'::storage.grant_role, $1)",
    )
    .bind(s.owner)
    .bind(cal)
    .execute(pool.as_ref())
    .await
    .expect("seed calendar grant");

    let cache: moka::future::Cache<(Uuid, Uuid), bool> = moka::future::Cache::builder()
        .max_capacity(100_000)
        .time_to_live(std::time::Duration::from_secs(30))
        .build();

    // Gates: identical verdict; revocation + invalidate flips the verdict.
    let v_query = direct_grant_query(pool, s.owner, cal).await;
    let v_cached = {
        let pool2 = pool.clone();
        cache
            .try_get_with((s.owner, cal), async move {
                Ok::<bool, std::convert::Infallible>(direct_grant_query(&pool2, s.owner, cal).await)
            })
            .await
            .unwrap()
    };
    gate("verdict identical", v_query == v_cached && v_query);
    sqlx::query(
        "DELETE FROM storage.role_grants WHERE resource_type = 'calendar' AND resource_id = $1",
    )
    .bind(cal)
    .execute(pool.as_ref())
    .await
    .expect("revoke");
    cache.invalidate_all();
    let v_after_revoke = {
        let pool2 = pool.clone();
        cache
            .try_get_with((s.owner, cal), async move {
                Ok::<bool, std::convert::Infallible>(direct_grant_query(&pool2, s.owner, cal).await)
            })
            .await
            .unwrap()
    };
    gate("revocation flips verdict", !v_after_revoke);
    // Re-seed for the measurement.
    sqlx::query(
        "INSERT INTO storage.role_grants
             (subject_type, subject_id, resource_type, resource_id, role, granted_by)
         VALUES ('user', $1, 'calendar', $2, 'owner'::storage.grant_role, $1)",
    )
    .bind(s.owner)
    .bind(cal)
    .execute(pool.as_ref())
    .await
    .expect("re-seed");
    cache.invalidate_all();

    let (ms_b, _) = timed(passes, || async {
        direct_grant_query(pool, s.owner, cal).await
    })
    .await;
    let (ms_a, _) = timed(passes, || async {
        let pool2 = pool.clone();
        cache
            .try_get_with((s.owner, cal), async move {
                Ok::<bool, std::convert::Infallible>(direct_grant_query(&pool2, s.owner, cal).await)
            })
            .await
            .unwrap()
    })
    .await;
    println!("    BEFORE role_grants query per check  p50 {ms_b:.3} ms");
    println!("    AFTER  moka hit                     p50 {ms_a:.4} ms");

    sqlx::query(
        "DELETE FROM storage.role_grants WHERE resource_type = 'calendar' AND resource_id = $1",
    )
    .bind(cal)
    .execute(pool.as_ref())
    .await
    .expect("cleanup grant");
}

// ─── §3 expand_user serial vs join ──────────────────────────────────────────

async fn q_is_external(pool: &PgPool, user: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT is_external FROM auth.users WHERE id = $1")
        .bind(user)
        .fetch_optional(pool)
        .await
        .expect("is_external")
        .unwrap_or(true)
}

async fn q_groups(pool: &PgPool, user: Uuid) -> Vec<Uuid> {
    sqlx::query(
        "WITH RECURSIVE user_groups AS (
             SELECT group_id
               FROM auth.subject_group_members
              WHERE member_user_id = $1
             UNION
             SELECT m.group_id
               FROM auth.subject_group_members m
               JOIN user_groups ug ON m.member_group_id = ug.group_id
         )
         SELECT group_id FROM user_groups",
    )
    .bind(user)
    .fetch_all(pool)
    .await
    .expect("groups CTE")
    .iter()
    .map(|r| r.get::<Uuid, _>("group_id"))
    .collect()
}

async fn section_expand(pool: &PgPool, s: &Seed, passes: usize) {
    println!("  §3 expand_user cold miss (per user per TTL window)");

    let b = {
        let e = q_is_external(pool, s.owner).await;
        let g = q_groups(pool, s.owner).await;
        (e, g)
    };
    let a = {
        let (e, g) = tokio::join!(q_is_external(pool, s.owner), q_groups(pool, s.owner));
        (e, g)
    };
    gate("expansion identical", b == a);

    let (ms_b, _) = timed(passes, || async {
        let e = q_is_external(pool, s.owner).await;
        let g = q_groups(pool, s.owner).await;
        (e, g.len())
    })
    .await;
    let (ms_a, _) = timed(passes, || async {
        let (e, g) = tokio::join!(q_is_external(pool, s.owner), q_groups(pool, s.owner));
        (e, g.len())
    })
    .await;
    println!("    BEFORE serial 2 queries  p50 {ms_b:.3} ms");
    println!("    AFTER  tokio::join!      p50 {ms_a:.3} ms");
}

// ─── §4 geo clusters min cast ───────────────────────────────────────────────

async fn geo_query(pool: &PgPool, caller: Uuid, min_expr: &str) -> Vec<(i64, f64, f64, String)> {
    sqlx::query_as(&format!(
        r#"
        SELECT count(*)              AS n,
               avg(fm.longitude)     AS clng,
               avg(fm.latitude)      AS clat,
               {min_expr}            AS sample_id
          FROM storage.file_metadata fm
          JOIN storage.files fi ON fi.id = fm.file_id
         WHERE fi.drive_id IN (
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
               )
           AND NOT fi.is_trashed
           AND fm.latitude IS NOT NULL
           AND fm.longitude IS NOT NULL
           AND fm.longitude BETWEEN $2 AND $3
           AND fm.latitude  BETWEEN $4 AND $5
         GROUP BY round(fm.longitude / $6), round(fm.latitude / $6)
        "#
    ))
    .bind(caller)
    .bind(-10.0f64)
    .bind(10.0f64)
    .bind(35.0f64)
    .bind(45.0f64)
    .bind(0.5f64)
    .fetch_all(pool)
    .await
    .expect("geo query")
}

async fn section_geo(pool: &PgPool, s: &Seed, passes: usize) {
    println!("  §4 Places geo clusters (per map viewport, 5k geotagged rows)");
    // Seed 5k geotagged photos across the viewport.
    let mut tx = pool.begin().await.expect("begin geo seed");
    for chunk in 0..10 {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "INSERT INTO storage.files
                 (name, folder_id, drive_id, blob_hash, size, mime_type, category_order)
             SELECT 'geo-' || $4 || '-' || g, $1, $2, $3, 1024, 'image/jpeg', 100
               FROM generate_series(1, 500) g
             RETURNING id",
        )
        .bind(s.root)
        .bind(s.drive)
        .bind(PLACEHOLDER)
        .bind(chunk.to_string())
        .fetch_all(&mut *tx)
        .await
        .expect("seed geo files");
        sqlx::query(
            "INSERT INTO storage.file_metadata (file_id, latitude, longitude)
             SELECT u, 35.0 + (random() * 10.0), -10.0 + (random() * 20.0)
               FROM unnest($1::uuid[]) u",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .expect("seed geo meta");
    }
    tx.commit().await.expect("commit geo seed");

    // REJECTED BY GATE: PostgreSQL has no `min(uuid)` aggregate — the
    // planned `min(fm.file_id)::text` (cast per cluster) fails to parse, so
    // the per-row-cast original stays. Verify the rejection reproducibly
    // and record the BEFORE for the doc.
    let min_uuid_err = sqlx::query("SELECT min(fm.file_id)::text FROM storage.file_metadata fm")
        .fetch_optional(pool)
        .await
        .is_err();
    gate("min(uuid) unsupported → AFTER rejected", min_uuid_err);

    let (ms_b, _) = timed(passes.min(60), || async {
        geo_query(pool, s.owner, "min(fm.file_id::text)")
            .await
            .len()
    })
    .await;
    println!("    BEFORE min(file_id::text)  p50 {ms_b:.3} ms   (AFTER rejected — see gate)");

    let _ = sqlx::query("DELETE FROM storage.files WHERE drive_id = $1 AND name LIKE 'geo-%'")
        .bind(s.drive)
        .execute(pool)
        .await;
}

// ─── §5 recluster assignment batch ──────────────────────────────────────────

async fn section_recluster(pool: &PgPool, s: &Seed, passes: usize) {
    println!("  §5 recluster face assignment (200-face library)");
    // One photo + 200 faces.
    let file: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.files
             (name, folder_id, drive_id, blob_hash, size, mime_type, category_order)
         VALUES ('faces.jpg', $1, $2, $3, 1024, 'image/jpeg', 100) RETURNING id",
    )
    .bind(s.root)
    .bind(s.drive)
    .bind(PLACEHOLDER)
    .fetch_one(pool)
    .await
    .expect("seed face file");
    let face_ids: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO faces.faces (file_id, user_id, bbox, det_score, embedding)
         SELECT $1, $2, ARRAY[0.1,0.1,0.2,0.2]::real[], 0.99, '\\x00'::bytea
           FROM generate_series(1, 200)
         RETURNING id",
    )
    .bind(file)
    .bind(s.owner)
    .fetch_all(pool)
    .await
    .expect("seed faces");
    let person: Uuid =
        sqlx::query_scalar("INSERT INTO faces.persons (user_id) VALUES ($1) RETURNING id")
            .bind(s.owner)
            .fetch_one(pool)
            .await
            .expect("seed person");

    let assignments: Vec<(Uuid, Option<Uuid>)> =
        face_ids.iter().map(|f| (*f, Some(person))).collect();

    async fn reset(pool: &PgPool, ids: &[Uuid]) {
        sqlx::query("UPDATE faces.faces SET person_id = NULL WHERE id = ANY($1)")
            .bind(ids)
            .execute(pool)
            .await
            .expect("reset");
    }
    async fn state(pool: &PgPool, ids: &[Uuid]) -> Vec<(Uuid, Option<Uuid>)> {
        let mut rows: Vec<(Uuid, Option<Uuid>)> =
            sqlx::query_as("SELECT id, person_id FROM faces.faces WHERE id = ANY($1)")
                .bind(ids)
                .fetch_all(pool)
                .await
                .expect("state");
        rows.sort();
        rows
    }

    // BEFORE: one UPDATE per face.
    reset(pool, &face_ids).await;
    for (f, p) in &assignments {
        sqlx::query("UPDATE faces.faces SET person_id = $2 WHERE id = $1")
            .bind(f)
            .bind(p)
            .execute(pool)
            .await
            .expect("assign");
    }
    let st_b = state(pool, &face_ids).await;
    // AFTER: one UNNEST batch.
    reset(pool, &face_ids).await;
    let (fs, ps): (Vec<Uuid>, Vec<Option<Uuid>>) = assignments.iter().cloned().unzip();
    sqlx::query(
        "UPDATE faces.faces f SET person_id = u.pid
           FROM (SELECT unnest($1::uuid[]) AS fid, unnest($2::uuid[]) AS pid) u
          WHERE f.id = u.fid",
    )
    .bind(&fs)
    .bind(&ps)
    .execute(pool)
    .await
    .expect("batch assign");
    let st_a = state(pool, &face_ids).await;
    gate("final person_id state identical", st_a == st_b);

    let it = passes.min(30);
    let (ms_b, _) = timed(it, || async {
        reset(pool, &face_ids).await;
        for (f, p) in &assignments {
            sqlx::query("UPDATE faces.faces SET person_id = $2 WHERE id = $1")
                .bind(f)
                .bind(p)
                .execute(pool)
                .await
                .expect("assign");
        }
        0u32
    })
    .await;
    let (ms_a, _) = timed(it, || async {
        reset(pool, &face_ids).await;
        let (fs, ps): (Vec<Uuid>, Vec<Option<Uuid>>) = assignments.iter().cloned().unzip();
        sqlx::query(
            "UPDATE faces.faces f SET person_id = u.pid
               FROM (SELECT unnest($1::uuid[]) AS fid, unnest($2::uuid[]) AS pid) u
              WHERE f.id = u.fid",
        )
        .bind(&fs)
        .bind(&ps)
        .execute(pool)
        .await
        .expect("batch");
        0u32
    })
    .await;
    println!("    BEFORE 200 sequential UPDATEs  p50 {ms_b:.3} ms  (incl. reset)");
    println!("    AFTER  1 UNNEST batch          p50 {ms_a:.3} ms  (incl. reset)");

    let _ = sqlx::query("DELETE FROM faces.persons WHERE id = $1")
        .bind(person)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.files WHERE id = $1")
        .bind(file)
        .execute(pool)
        .await;
}

// ─── main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required (see .env)");
    let passes: usize = env_or("BENCH_PASSES", 200);
    println!("bench_round11_queries — passes={passes}\n");

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(8)
            .connect(&url)
            .await
            .expect("connect"),
    );
    let seed = seed_base(&pool, "q").await;

    section_deferred(&pool, &seed, passes).await;
    section_grant_cache(&pool, &seed, passes).await;
    section_expand(&pool, &seed, passes).await;
    section_geo(&pool, &seed, passes).await;
    section_recluster(&pool, &seed, passes.min(30)).await;

    cleanup_base(&pool, &seed).await;
    println!("\ndone");
}
