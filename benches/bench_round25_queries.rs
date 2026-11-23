//! Round-25 PostgreSQL query-shape pack — end-to-end round-trips + wall on the
//! live dev Postgres, with an equivalence gate (mismatch → `exit(1)`) mirroring
//! ROUND23's methodology.
//!
//!   [Q1] `music_storage_adapter::list_public_playlists` is 1 + N round-trips:
//!        one listing SELECT then one `SELECT COUNT(*) FROM audio.playlist_items`
//!        per returned playlist (up to 101 at limit=100). AFTER folds the count
//!        into the listing with a `LEFT JOIN … GROUP BY` — one round-trip.
//!        Gate: AFTER wall < BEFORE wall AND identical (playlist → track_count).
//!
//!   [Q2] The three REST contact listings `SELECT … vcard …` — the multi-KB
//!        vCard TEXT (may embed a base64 PHOTO) — but every caller maps
//!        Contact → ContactDto, which has NO vcard field, so it is fetched,
//!        shipped over the wire, decoded into a String and dropped. AFTER omits
//!        the vcard column (a lite mapper passes an empty string). Gate: AFTER
//!        wall < BEFORE wall AND identical (id, full_name, photo_url) DTO fields.
//!
//! Run (needs the dev Postgres up; reads DATABASE_URL from .env):
//!   RUSTFLAGS="-C target-cpu=x86-64-v3" \
//!     cargo bench --features=bench --bench bench_round25_queries
//! Tunables (env): Q1_PLAYLISTS (100), Q1_PASSES (30),
//!                 Q2_CONTACTS (1000), Q2_PASSES (20), Q2_VCARD_KB (8)

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

fn p50(mut s: Vec<f64>) -> f64 {
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}

fn report(tag: &str, unit: &str, before: f64, after: f64, stmts_before: usize, stmts_after: usize) {
    println!("## {tag}");
    println!("| arm    | {unit:>16} | statements |");
    println!("| BEFORE | {before:>16.3} | {stmts_before:>10} |");
    println!("| AFTER  | {after:>16.3} | {stmts_after:>10} |");
    println!(
        "# {:.2}x wall · {} → {} round-trips\n",
        before / after.max(1e-9),
        stmts_before,
        stmts_after
    );
}

fn gate(tag: &str, metric: &str, before: f64, after: f64) {
    if after >= before {
        eprintln!("GATE FAIL [{tag}] {metric}: AFTER {after} !< BEFORE {before} — rollback");
        std::process::exit(1);
    }
}

async fn cleanup(pool: &PgPool) {
    // Idempotent teardown (also clears fixtures a prior crashed run left).
    let _ = sqlx::query("SET session_replication_role = default")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM audio.playlist_items WHERE playlist_id IN (SELECT id FROM audio.playlists WHERE name LIKE 'bench25_pl_%')").execute(pool).await;
    let _ = sqlx::query("DELETE FROM audio.playlists WHERE name LIKE 'bench25_pl_%'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM carddav.contacts WHERE uid LIKE 'bench25-%'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM carddav.address_books WHERE name = 'bench25_ab'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE email LIKE 'bench25-%@bench.invalid'")
        .execute(pool)
        .await;
}

async fn seed_user(pool: &PgPool, tag: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role) VALUES ($1, $2, 'user') RETURNING id",
    )
    .bind(format!("bench25_{tag}"))
    .bind(format!("bench25-{tag}@bench.invalid"))
    .fetch_one(pool)
    .await
    .expect("seed user")
}

// ── [Q1] Public-playlist listing: 1 + N COUNT vs one LEFT JOIN GROUP BY ───────
async fn section_q1(pool: &PgPool) {
    let n: usize = env_or("Q1_PLAYLISTS", 100);
    let passes: usize = env_or("Q1_PASSES", 30);
    let owner = seed_user(pool, "q1owner").await;

    // Seed N public playlists, playlist i carrying (i % 10) + 1 items. The
    // playlist_items.file_id FK to storage.files is bypassed with replica role
    // (superuser) so the query SHAPE can be isolated without a files fixture.
    let mut conn = pool.acquire().await.expect("acquire");
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *conn)
        .await
        .unwrap();
    let mut ids: Vec<Uuid> = Vec::with_capacity(n);
    for i in 0..n {
        let pid: Uuid = sqlx::query_scalar(
            "INSERT INTO audio.playlists (id, name, owner_id, is_public)
             VALUES (gen_random_uuid(), $1, $2, TRUE) RETURNING id",
        )
        .bind(format!("bench25_pl_{i}"))
        .bind(owner)
        .fetch_one(&mut *conn)
        .await
        .expect("seed playlist");
        ids.push(pid);
        for j in 0..((i % 10) + 1) {
            sqlx::query(
                "INSERT INTO audio.playlist_items (id, playlist_id, file_id, position)
                 VALUES (gen_random_uuid(), $1, gen_random_uuid(), $2)",
            )
            .bind(pid)
            .bind(j as i32)
            .execute(&mut *conn)
            .await
            .expect("seed item");
        }
    }
    sqlx::query("SET session_replication_role = default")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    let limit = n as i64;

    // BEFORE: list (1) then one COUNT per playlist (N) → 1 + N round-trips.
    let before_counts = {
        let rows = sqlx::query(
            "SELECT id FROM audio.playlists WHERE is_public = TRUE ORDER BY updated_at DESC LIMIT $1 OFFSET 0",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
        .unwrap();
        let mut m: Vec<(Uuid, i64)> = Vec::with_capacity(rows.len());
        for r in &rows {
            let pid: Uuid = r.get(0);
            let c: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM audio.playlist_items WHERE playlist_id = $1")
                    .bind(pid)
                    .fetch_one(pool)
                    .await
                    .unwrap();
            m.push((pid, c.0));
        }
        m.sort();
        m
    };

    // AFTER: one LEFT JOIN + GROUP BY → 1 round-trip.
    let after_counts = {
        let rows = sqlx::query(
            "SELECT p.id, COUNT(pi.id) AS track_count
             FROM audio.playlists p
             LEFT JOIN audio.playlist_items pi ON pi.playlist_id = p.id
             WHERE p.is_public = TRUE
             GROUP BY p.id
             ORDER BY p.updated_at DESC LIMIT $1 OFFSET 0",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
        .unwrap();
        let mut m: Vec<(Uuid, i64)> = rows
            .iter()
            .map(|r| (r.get::<Uuid, _>(0), r.get::<i64, _>(1)))
            .collect();
        m.sort();
        m
    };

    assert_eq!(
        before_counts, after_counts,
        "Q1 track_count mismatch BEFORE vs AFTER"
    );

    // Timed passes.
    let mut before_ms = Vec::new();
    let mut after_ms = Vec::new();
    for _ in 0..passes {
        let t = Instant::now();
        let rows = sqlx::query("SELECT id FROM audio.playlists WHERE is_public = TRUE ORDER BY updated_at DESC LIMIT $1 OFFSET 0").bind(limit).fetch_all(pool).await.unwrap();
        for r in &rows {
            let pid: Uuid = r.get(0);
            let _c: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM audio.playlist_items WHERE playlist_id = $1")
                    .bind(pid)
                    .fetch_one(pool)
                    .await
                    .unwrap();
        }
        before_ms.push(t.elapsed().as_secs_f64() * 1e3);

        let t = Instant::now();
        let _rows = sqlx::query("SELECT p.id, COUNT(pi.id) FROM audio.playlists p LEFT JOIN audio.playlist_items pi ON pi.playlist_id = p.id WHERE p.is_public = TRUE GROUP BY p.id ORDER BY p.updated_at DESC LIMIT $1 OFFSET 0").bind(limit).fetch_all(pool).await.unwrap();
        after_ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let b = p50(before_ms);
    let a = p50(after_ms);
    report(
        &format!("[Q1] public-playlist listing ({n} playlists)"),
        "p50 ms",
        b,
        a,
        1 + n,
        1,
    );
    gate("Q1", "p50 ms", b, a);
}

// ── [Q2] Contact listing: over-fetch vcard TEXT vs lite (no vcard) ────────────
async fn section_q2(pool: &PgPool) {
    let n: usize = env_or("Q2_CONTACTS", 1000);
    let passes: usize = env_or("Q2_PASSES", 20);
    let vcard_kb: usize = env_or("Q2_VCARD_KB", 8);
    let owner = seed_user(pool, "q2owner").await;
    let ab: Uuid = sqlx::query_scalar(
        "INSERT INTO carddav.address_books (id, name, owner_id) VALUES (gen_random_uuid(), 'bench25_ab', $1) RETURNING id",
    )
    .bind(owner)
    .fetch_one(pool)
    .await
    .expect("seed address book");

    // A realistic vCard body with an embedded base64 PHOTO of ~vcard_kb KiB.
    let photo_blob = "A".repeat(vcard_kb * 1024);
    for i in 0..n {
        let vcard = format!(
            "BEGIN:VCARD\nVERSION:3.0\nFN:Contact {i}\nEMAIL:c{i}@example.com\nPHOTO;ENCODING=b;TYPE=JPEG:{photo_blob}\nEND:VCARD"
        );
        sqlx::query(
            "INSERT INTO carddav.contacts (id, address_book_id, uid, full_name, photo_url, email, phone, address, vcard, etag)
             VALUES (gen_random_uuid(), $1, $2, $3, $4, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5, $6)",
        )
        .bind(ab)
        .bind(format!("bench25-{i}"))
        .bind(format!("Contact {i}"))
        .bind(format!("https://example.com/p/{i}.jpg"))
        .bind(&vcard)
        .bind(format!("etag{i}"))
        .execute(pool)
        .await
        .expect("seed contact");
    }

    // Lite DTO shape the REST listing actually keeps.
    #[derive(PartialEq, Debug)]
    struct LiteDto {
        id: Uuid,
        full_name: Option<String>,
        photo_url: Option<String>,
    }

    let before_select = "SELECT id, full_name, photo_url, vcard FROM carddav.contacts WHERE address_book_id = $1 ORDER BY full_name LIMIT $2";
    let after_select = "SELECT id, full_name, photo_url FROM carddav.contacts WHERE address_book_id = $1 ORDER BY full_name LIMIT $2";
    let limit = n as i64;

    // Equivalence: the kept DTO fields are identical whether or not vcard is read.
    let before_dtos: Vec<LiteDto> = {
        let rows = sqlx::query(before_select)
            .bind(ab)
            .bind(limit)
            .fetch_all(pool)
            .await
            .unwrap();
        rows.iter()
            .map(|r| {
                let _vcard: Option<String> = r.get("vcard"); // fetched + decoded, then dropped
                LiteDto {
                    id: r.get("id"),
                    full_name: r.get("full_name"),
                    photo_url: r.get("photo_url"),
                }
            })
            .collect()
    };
    let after_dtos: Vec<LiteDto> = {
        let rows = sqlx::query(after_select)
            .bind(ab)
            .bind(limit)
            .fetch_all(pool)
            .await
            .unwrap();
        rows.iter()
            .map(|r| LiteDto {
                id: r.get("id"),
                full_name: r.get("full_name"),
                photo_url: r.get("photo_url"),
            })
            .collect()
    };
    assert_eq!(
        before_dtos, after_dtos,
        "Q2 DTO fields mismatch BEFORE vs AFTER"
    );

    let mut before_ms = Vec::new();
    let mut after_ms = Vec::new();
    for _ in 0..passes {
        let t = Instant::now();
        let rows = sqlx::query(before_select)
            .bind(ab)
            .bind(limit)
            .fetch_all(pool)
            .await
            .unwrap();
        let mut sink = 0usize;
        for r in &rows {
            let v: Option<String> = r.get("vcard");
            sink += v.map(|s| s.len()).unwrap_or(0);
            let _d = LiteDto {
                id: r.get("id"),
                full_name: r.get("full_name"),
                photo_url: r.get("photo_url"),
            };
        }
        std::hint::black_box(sink);
        before_ms.push(t.elapsed().as_secs_f64() * 1e3);

        let t = Instant::now();
        let rows = sqlx::query(after_select)
            .bind(ab)
            .bind(limit)
            .fetch_all(pool)
            .await
            .unwrap();
        for r in &rows {
            let _d = LiteDto {
                id: r.get("id"),
                full_name: r.get("full_name"),
                photo_url: r.get("photo_url"),
            };
        }
        after_ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let b = p50(before_ms);
    let a = p50(after_ms);
    report(
        &format!("[Q2] contact listing over-fetch vcard ({n} contacts, {vcard_kb} KiB vcard)"),
        "p50 ms",
        b,
        a,
        1,
        1,
    );
    gate("Q2", "p50 ms", b, a);
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect Postgres");

    println!("# Round-25 PG query-shape pack — BEFORE/AFTER (live Postgres)\n");
    cleanup(&pool).await;
    section_q1(&pool).await;
    section_q2(&pool).await;
    cleanup(&pool).await;
    println!("All Round-25 query sections passed their gate.");
}
