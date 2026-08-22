//! Round-14 query-shape pack (needs the dev Postgres up; reads DATABASE_URL
//! from `.env`).
//!
//! Each section is BEFORE (verbatim replica of the shipped query shape) vs
//! AFTER (proposed shape), with an equivalence/safety gate and a `GATE FAIL`
//! rollback check — an AFTER that doesn't beat its BEFORE exits non-zero.
//!
//!   [Q1] Lightbox face boxes — `faces_for_file`'s 10-column row (incl. the
//!        2,048-byte `embedding` BYTEA, decoded into a `Vec<f32>` per face)
//!        hydrated for a group photo, then filtered `user_id == caller` in
//!        Rust, vs a narrow `SELECT id, person_id, bbox … WHERE file_id = $1
//!        AND user_id = $2` (embedding + 6 unused columns dropped; the caller
//!        filter pushed into SQL). The only consumer, `people_service::
//!        faces_for_file`, builds `FaceBoxDto { id, person_id, x,y,w,h }`.
//!
//! Run:
//!   cargo run --release --features bench --example bench_round14_queries
//! Tunables (env): BENCH_PASSES (200), BENCH_FACES_PER_FILE (15)

use std::env;
use std::time::Instant;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn stats(mut s: Vec<f64>) -> (f64, f64, f64) {
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    (
        s.iter().sum::<f64>() / n as f64,
        s[n / 2],
        s[((n as f64 * 0.95) as usize).min(n - 1)],
    )
}

/// Mirror of `face_pg_repository::bytes_to_embedding` — the per-face
/// `Vec<f32>` decode the BEFORE path pays for a column it never reads.
/// Kept byte-for-byte identical to the shipped path so the benchmark
/// measures apples-to-apples; `as_chunks` swap matches the source.
fn bytes_to_embedding(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// [Q1] Lightbox face boxes — wide row (incl. embedding) vs narrow projection
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE, verbatim `faces_for_file` + `people_service::faces_for_file`:
/// hydrate the full 10-column row (decoding the 2 KiB embedding like
/// `row_to_face`), then filter `user_id == caller` in Rust and keep only
/// `(id, person_id, bbox)`.
async fn boxes_before(
    pool: &PgPool,
    file_id: Uuid,
    caller: Uuid,
) -> Vec<(Uuid, Option<Uuid>, Vec<f32>)> {
    let rows = sqlx::query(
        "SELECT id, file_id, user_id, person_id, bbox, det_score, quality, embedding, blob_hash, created_at
           FROM faces.faces WHERE file_id = $1",
    )
    .bind(file_id)
    .fetch_all(pool)
    .await
    .expect("faces wide");
    rows.into_iter()
        .filter_map(|r| {
            let user_id: Uuid = r.get("user_id");
            // Decode the embedding exactly as `row_to_face` does (the cost the
            // BEFORE path pays even though `FaceBoxDto` never reads it).
            let emb_bytes: Vec<u8> = r.get("embedding");
            let _embedding = bytes_to_embedding(&emb_bytes);
            if user_id != caller {
                return None;
            }
            let bbox: Vec<f32> = r.get("bbox");
            Some((r.get("id"), r.get("person_id"), bbox))
        })
        .collect()
}

/// AFTER: narrow projection, caller filter in SQL.
async fn boxes_after(
    pool: &PgPool,
    file_id: Uuid,
    caller: Uuid,
) -> Vec<(Uuid, Option<Uuid>, Vec<f32>)> {
    let rows = sqlx::query(
        "SELECT id, person_id, bbox FROM faces.faces WHERE file_id = $1 AND user_id = $2",
    )
    .bind(file_id)
    .bind(caller)
    .fetch_all(pool)
    .await
    .expect("faces narrow");
    rows.into_iter()
        .map(|r| {
            let bbox: Vec<f32> = r.get("bbox");
            (r.get("id"), r.get("person_id"), bbox)
        })
        .collect()
}

async fn section_face_boxes(pool: &PgPool) {
    let n: usize = env_or("BENCH_FACES_PER_FILE", 15);
    let passes: usize = env_or("BENCH_PASSES", 200);

    // Seed: user + drive + folder + one photo file + N faces on it.
    let mut tx = pool.begin().await.expect("begin");
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench14_faces', 'bench14_faces@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("user");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, default_for_user) VALUES ('personal', $1) RETURNING id",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    .expect("drive");
    let folder_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('bench14', '/bench14', 'bench14', $1) RETURNING id",
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
        .expect("stamp root");
    let file_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
         VALUES ('group.jpg', $1, 'bench14blob00000000000000000000000000000000000000000000000000', 1024, 'image/jpeg', $2)
         RETURNING id",
    )
    .bind(folder_id)
    .bind(drive_id)
    .fetch_one(&mut *tx)
    .await
    .expect("file");
    tx.commit().await.expect("commit");

    let person_id: Uuid = sqlx::query_scalar(
        "INSERT INTO faces.persons (user_id, display_name) VALUES ($1, 'P') RETURNING id",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("person");

    let embedding = vec![7u8; 2048]; // 512 × f32, like the real thing
    for i in 0..n {
        // Half the faces are named, half unassigned — exercises Option<Uuid>.
        let pid = if i % 2 == 0 { Some(person_id) } else { None };
        sqlx::query(
            "INSERT INTO faces.faces
                 (file_id, user_id, person_id, bbox, det_score, quality, embedding, blob_hash)
             VALUES ($1, $2, $3, ARRAY[0.1,0.2,0.3,0.4]::real[], 0.99, 0.9, $4, NULL)",
        )
        .bind(file_id)
        .bind(user_id)
        .bind(pid)
        .bind(&embedding)
        .execute(pool)
        .await
        .expect("face");
    }
    sqlx::query("ANALYZE faces.faces").execute(pool).await.ok();

    // Equivalence gate: same (id, person_id, bbox) set both ways, all N present.
    let mut b = boxes_before(pool, file_id, user_id).await;
    let mut a = boxes_after(pool, file_id, user_id).await;
    b.sort_by_key(|x| x.0);
    a.sort_by_key(|x| x.0);
    assert_eq!(b, a, "face-box projections differ");
    assert_eq!(a.len(), n, "expected all faces");
    println!("# [Q1] gate: wide/narrow face-box sets identical ({n} faces) — OK");

    let mut wide = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(boxes_before(pool, file_id, user_id).await);
        wide.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut narrow = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(boxes_after(pool, file_id, user_id).await);
        narrow.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (wm, wp50, wp95) = stats(wide);
    let (nm, np50, np95) = stats(narrow);
    let wire_before = n * (2048 + 16 * 4 + 24); // embedding + uuids/bbox + row overhead
    let wire_after = n * (16 + 16 + 16 + 8);
    println!("\n## [Q1] Lightbox face boxes — group photo, {n} faces");
    println!("| arm | mean ms | p50 ms | p95 ms | ~bytes/req |");
    println!(
        "| BEFORE wide row (incl. embedding) | {wm:>7.3} | {wp50:>6.3} | {wp95:>6.3} | {wire_before:>9} |"
    );
    println!(
        "| AFTER  narrow (id,person,bbox)    | {nm:>7.3} | {np50:>6.3} | {np95:>6.3} | {wire_after:>9} |"
    );
    println!(
        "# {:.2}x faster; ~{} KiB embedding/columns off the wire per lightbox open (scales with face count)",
        wm / nm,
        (wire_before - wire_after) / 1024
    );

    // Cleanup (cascades faces + persons via FKs on drive/user delete).
    sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(drive_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();

    if nm >= wm {
        eprintln!("GATE FAIL [Q1]: narrow projection not faster — rollback");
        std::process::exit(1);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let _ = dotenvy::dotenv();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required (see .env)");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect");

    println!("#################################################################");
    println!("# Round-14 query-shape pack");
    println!("#################################################################");

    section_face_boxes(&pool).await;

    println!("\nGATE PASS (all sections)");
}
