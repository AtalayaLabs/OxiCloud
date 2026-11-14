//! People-tab benchmark — full faces scan (embeddings included) vs grouped COUNT.
//!
//! `PeopleService::list_people` used to call `faces_for_user`, dragging every
//! face row — each with a 2,048-byte embedding BYTEA — across the wire and
//! decoding it into a fresh `Vec<f32>`, only to (a) count faces per person and
//! (b) resolve ~a-handful of cover faces to file ids. The change replaces it
//! with `person_face_stats` (grouped COUNT) + `file_ids_for_faces` (one
//! `= ANY` over just the cover ids).
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_people_list
//! Tunables: BENCH_FACES (10000), BENCH_PERSONS (20), BENCH_REPS (5)

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

struct Seeded {
    user_id: Uuid,
    drive_id: Uuid,
    cover_ids: Vec<Uuid>,
}

async fn seed(pool: &PgPool, faces: usize, persons: usize) -> Seeded {
    let mut tx = pool.begin().await.expect("begin");
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_people', 'bench_people@bench.invalid', 'user') RETURNING id",
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
         VALUES ('bench_people', '/bench_people', 'bench_people', $1) RETURNING id",
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
    tx.commit().await.expect("commit");

    // Photo files the faces point at.
    let file_ids: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
         SELECT 'p' || i, $1, 'benchpeople0000000000000000000000000000000000000000000000000000',
                1024, 'image/jpeg', $2
           FROM generate_series(1, $3) AS i
         RETURNING id",
    )
    .bind(folder_id)
    .bind(drive_id)
    .bind(faces as i32)
    .fetch_all(pool)
    .await
    .expect("files");

    // Persons + faces (2 KiB embedding each, like the real 512×f32).
    let mut person_ids = Vec::with_capacity(persons);
    for i in 0..persons {
        let pid: Uuid = sqlx::query_scalar(
            "INSERT INTO faces.persons (user_id, display_name) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(format!("Person {i}"))
        .fetch_one(pool)
        .await
        .expect("person");
        person_ids.push(pid);
    }

    let embedding = vec![0u8; 2048];
    let mut cover_ids = Vec::with_capacity(persons);
    for (i, file_id) in file_ids.iter().enumerate() {
        let pid = person_ids[i % persons];
        let face_id: Uuid = sqlx::query_scalar(
            "INSERT INTO faces.faces
                 (file_id, user_id, person_id, bbox, det_score, quality, embedding, blob_hash)
             VALUES ($1, $2, $3, ARRAY[0.1,0.1,0.2,0.2]::real[], 0.99, 0.9, $4,
                     'benchpeople0000000000000000000000000000000000000000000000000000')
             RETURNING id",
        )
        .bind(file_id)
        .bind(user_id)
        .bind(pid)
        .bind(&embedding)
        .fetch_one(pool)
        .await
        .expect("face");
        if i < persons {
            cover_ids.push(face_id);
        }
    }
    sqlx::query("ANALYZE faces.faces").execute(pool).await.ok();

    Seeded {
        user_id,
        drive_id,
        cover_ids,
    }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
    let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(s.drive_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(s.user_id)
        .execute(pool)
        .await;
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").expect("set DATABASE_URL");
    let faces: usize = env_or("BENCH_FACES", 10_000);
    let persons: usize = env_or("BENCH_PERSONS", 20);
    let reps: usize = env_or("BENCH_REPS", 5);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect");
    println!("seeding {faces} faces / {persons} persons (one-time)…");
    let seeded = seed(&pool, faces, persons).await;

    println!(
        "\n# GET /api/people data fetch: BEFORE (full face rows) vs AFTER (COUNT + cover ANY)"
    );
    println!("{:<28} {:>12} {:>14}", "mode", "total ms", "bytes moved");

    let mut base = None;
    for mode in ["BEFORE full-rows", "AFTER  count+covers"] {
        let mut times = Vec::with_capacity(reps);
        let mut bytes = 0usize;
        for _ in 0..reps {
            let t = Instant::now();
            if mode.starts_with("BEFORE") {
                // faces_for_user shape: every column incl. embedding.
                let rows: Vec<(Uuid, Uuid, Option<Uuid>, Vec<u8>)> = sqlx::query_as(
                    "SELECT id, file_id, person_id, embedding FROM faces.faces WHERE user_id = $1",
                )
                .bind(seeded.user_id)
                .fetch_all(&pool)
                .await
                .expect("full rows");
                bytes = rows.iter().map(|r| r.3.len() + 48).sum();
                assert_eq!(rows.len(), faces);
            } else {
                let stats: Vec<(Uuid, i64)> = sqlx::query_as(
                    "SELECT person_id, COUNT(*) FROM faces.faces
                      WHERE user_id = $1 AND person_id IS NOT NULL GROUP BY person_id",
                )
                .bind(seeded.user_id)
                .fetch_all(&pool)
                .await
                .expect("stats");
                let covers: Vec<(Uuid, Uuid)> = sqlx::query_as(
                    "SELECT id, file_id FROM faces.faces WHERE user_id = $1 AND id = ANY($2)",
                )
                .bind(seeded.user_id)
                .bind(&seeded.cover_ids)
                .fetch_all(&pool)
                .await
                .expect("covers");
                bytes = (stats.len() + covers.len()) * 32;
                assert_eq!(stats.len(), persons);
            }
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let ms = median(times);
        let speedup = base
            .map(|b: f64| format!("({:.1}x)", b / ms))
            .unwrap_or_default();
        println!("{mode:<28} {ms:>12.2} {bytes:>14} {speedup}");
        if base.is_none() {
            base = Some(ms);
        }
    }

    cleanup(&pool, &seeded).await;
}
