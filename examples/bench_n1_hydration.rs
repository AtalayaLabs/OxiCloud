//! Grant-listing hydration N+1 benchmark + user-flags herd (ROUND4).
//!
//! [1-3] After `list_incoming_grants`, the CalDAV calendar discovery,
//! CardDAV book discovery and playlist listing each hydrated their K
//! accessible resources with K SERIAL point SELECTs (one
//! `WHERE id = $1` round-trip per resource, awaited in a loop) on every
//! client sync poll / dashboard load. AFTER: one `WHERE id = ANY($1)`
//! round-trip via the new `find_*_by_ids` batch methods — this bench
//! drives the REAL repositories both ways (the single-get methods still
//! exist for point lookups).
//!
//! [4] `get_user_flags` (called by the auth middleware on EVERY
//! authenticated request) used a get→insert cache: on each 30 s TTL
//! expiry, all in-flight requests of that user fired the SELECT
//! concurrently. AFTER: `try_get_with` single-flight. The bench
//! replicates both cache patterns around the real `UserPgRepository`
//! query, herd-style.
//!
//! Equivalence gates: identical id sets from loop vs batch for all
//! three resources; identical flags from every herd caller.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo run --release --features bench --example bench_n1_hydration
//! Tunables (env): BENCH_RESOURCES (15), BENCH_PASSES (200), BENCH_HERD (32).

use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use oxicloud::domain::repositories::address_book_repository::AddressBookRepository;
use oxicloud::domain::repositories::calendar_repository::CalendarRepository;
use oxicloud::domain::repositories::playlist_repository::PlaylistRepository;
use oxicloud::infrastructure::repositories::pg::{
    AddressBookPgRepository, CalendarPgRepository, PlaylistPgRepository, UserPgRepository,
};
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
    calendar_ids: Vec<Uuid>,
    book_ids: Vec<Uuid>,
    playlist_ids: Vec<Uuid>,
}

async fn seed(pool: &PgPool, n: usize) -> Seeded {
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_n1', 'bench_n1@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("seed user");

    let mut calendar_ids = Vec::with_capacity(n);
    let mut book_ids = Vec::with_capacity(n);
    let mut playlist_ids = Vec::with_capacity(n);
    for i in 0..n {
        calendar_ids.push(
            sqlx::query_scalar(
                "INSERT INTO caldav.calendars (id, name, owner_id, color)
                 VALUES (gen_random_uuid(), $1, $2, '#3788d8') RETURNING id",
            )
            .bind(format!("Calendario {i}"))
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("seed calendar"),
        );
        book_ids.push(
            sqlx::query_scalar(
                "INSERT INTO carddav.address_books (id, name, owner_id)
                 VALUES (gen_random_uuid(), $1, $2) RETURNING id",
            )
            .bind(format!("Libreta {i}"))
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("seed book"),
        );
        playlist_ids.push(
            sqlx::query_scalar(
                "INSERT INTO audio.playlists (name, owner_id)
                 VALUES ($1, $2) RETURNING id",
            )
            .bind(format!("Lista {i}"))
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("seed playlist"),
        );
    }
    Seeded {
        user_id,
        calendar_ids,
        book_ids,
        playlist_ids,
    }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
    let _ = sqlx::query("DELETE FROM caldav.calendars WHERE owner_id = $1")
        .bind(s.user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM carddav.address_books WHERE owner_id = $1")
        .bind(s.user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM audio.playlists WHERE owner_id = $1")
        .bind(s.user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(s.user_id)
        .execute(pool)
        .await;
}

fn p50(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

async fn bench_pair<FB, FA, TB, TA>(
    label: &str,
    passes: usize,
    n: usize,
    mut before: FB,
    mut after: FA,
) where
    FB: AsyncFnMut() -> TB,
    FA: AsyncFnMut() -> TA,
{
    let mut lb = Vec::with_capacity(passes);
    let mut la = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t0 = Instant::now();
        std::hint::black_box(before().await);
        lb.push(t0.elapsed().as_secs_f64() * 1e3);
        let t0 = Instant::now();
        std::hint::black_box(after().await);
        la.push(t0.elapsed().as_secs_f64() * 1e3);
    }
    let b = p50(lb);
    let a = p50(la);
    println!("[{label}] ms/listing (p50, K={n})");
    println!("    BEFORE (K point SELECTs)  {b:8.3}   ({n} queries)");
    println!(
        "    AFTER  (1 × = ANY)        {a:8.3}   (1 query)    {:.1}x",
        b / a
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let n: usize = env_or("BENCH_RESOURCES", 15);
    let passes: usize = env_or("BENCH_PASSES", 200);
    let herd: usize = env_or("BENCH_HERD", 32);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(40)
            .min_connections(40)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect Postgres"),
    );

    let seeded = seed(&pool, n).await;

    let cal_repo = CalendarPgRepository::new(pool.clone());
    let book_repo = AddressBookPgRepository::new(pool.clone());
    let pl_repo = PlaylistPgRepository::new(pool.clone());

    println!("bench_n1_hydration — {n} resources/listing, {passes} passes, herd={herd}\n");

    // ── [1] calendars ──
    bench_pair(
        "1 calendars",
        passes,
        n,
        async || {
            let mut out = Vec::with_capacity(n);
            for id in &seeded.calendar_ids {
                if let Ok(c) = cal_repo.find_calendar_by_id(id).await {
                    out.push(c);
                }
            }
            out
        },
        async || {
            cal_repo
                .find_calendars_by_ids(&seeded.calendar_ids)
                .await
                .expect("batch calendars")
        },
    )
    .await;

    // ── [2] address books ──
    bench_pair(
        "2 address books",
        passes,
        n,
        async || {
            let mut out = Vec::with_capacity(n);
            for id in &seeded.book_ids {
                if let Ok(Some(b)) = book_repo.get_address_book_by_id(id).await {
                    out.push(b);
                }
            }
            out
        },
        async || {
            book_repo
                .get_address_books_by_ids(&seeded.book_ids)
                .await
                .expect("batch books")
        },
    )
    .await;

    // ── [3] playlists ──
    bench_pair(
        "3 playlists",
        passes,
        n,
        async || {
            let mut out = Vec::with_capacity(n);
            for id in &seeded.playlist_ids {
                if let Ok(p) = pl_repo.find_playlist_by_id(id).await {
                    out.push(p);
                }
            }
            out
        },
        async || {
            pl_repo
                .find_playlists_by_ids(&seeded.playlist_ids)
                .await
                .expect("batch playlists")
        },
    )
    .await;

    // ── Equivalence gates ──
    let mut ok = true;
    {
        let loop_ids: HashSet<Uuid> = {
            let mut s = HashSet::new();
            for id in &seeded.calendar_ids {
                if let Ok(c) = cal_repo.find_calendar_by_id(id).await {
                    s.insert(*c.id());
                }
            }
            s
        };
        let batch_ids: HashSet<Uuid> = cal_repo
            .find_calendars_by_ids(&seeded.calendar_ids)
            .await
            .expect("batch")
            .iter()
            .map(|c| *c.id())
            .collect();
        if loop_ids != batch_ids {
            eprintln!("GATE FAIL calendars: {loop_ids:?} != {batch_ids:?}");
            ok = false;
        }
        // Missing ids drop out on both sides.
        let with_ghost: Vec<Uuid> = seeded
            .calendar_ids
            .iter()
            .copied()
            .chain([Uuid::new_v4()])
            .collect();
        let ghost_ids: HashSet<Uuid> = cal_repo
            .find_calendars_by_ids(&with_ghost)
            .await
            .expect("batch+ghost")
            .iter()
            .map(|c| *c.id())
            .collect();
        if ghost_ids != batch_ids {
            eprintln!("GATE FAIL calendars: ghost id changed result");
            ok = false;
        }
    }
    {
        let loop_ids: HashSet<Uuid> = {
            let mut s = HashSet::new();
            for id in &seeded.book_ids {
                if let Ok(Some(b)) = book_repo.get_address_book_by_id(id).await {
                    s.insert(*b.id());
                }
            }
            s
        };
        let batch_ids: HashSet<Uuid> = book_repo
            .get_address_books_by_ids(&seeded.book_ids)
            .await
            .expect("batch")
            .iter()
            .map(|b| *b.id())
            .collect();
        if loop_ids != batch_ids {
            eprintln!("GATE FAIL books");
            ok = false;
        }
    }
    {
        let loop_ids: HashSet<Uuid> = {
            let mut s = HashSet::new();
            for id in &seeded.playlist_ids {
                if let Ok(p) = pl_repo.find_playlist_by_id(id).await {
                    s.insert(*p.id());
                }
            }
            s
        };
        let batch_ids: HashSet<Uuid> = pl_repo
            .find_playlists_by_ids(&seeded.playlist_ids)
            .await
            .expect("batch")
            .iter()
            .map(|p| *p.id())
            .collect();
        if loop_ids != batch_ids {
            eprintln!("GATE FAIL playlists");
            ok = false;
        }
    }

    // ── [4] user-flags herd: get→insert vs try_get_with ─────────────────────
    let user_repo = Arc::new(UserPgRepository::new(pool.clone()));
    let queries = Arc::new(AtomicUsize::new(0));

    // BEFORE: sync moka get/insert — every cold caller queries.
    let sync_cache: moka::sync::Cache<Uuid, oxicloud::domain::entities::user::UserFlags> =
        moka::sync::Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(30))
            .build();
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..herd {
        let cache = sync_cache.clone();
        let repo = user_repo.clone();
        let queries = queries.clone();
        let uid = seeded.user_id;
        handles.push(tokio::spawn(async move {
            if let Some(f) = cache.get(&uid) {
                return f;
            }
            queries.fetch_add(1, Ordering::Relaxed);
            let f = repo.get_user_flags(uid).await.expect("flags");
            cache.insert(uid, f.clone());
            f
        }));
    }
    let mut before_flags = Vec::new();
    for h in handles {
        before_flags.push(h.await.unwrap());
    }
    let before_wall = t0.elapsed().as_secs_f64() * 1e3;
    let before_queries = queries.swap(0, Ordering::Relaxed);

    // AFTER: future moka try_get_with — one query per herd.
    let future_cache: moka::future::Cache<Uuid, oxicloud::domain::entities::user::UserFlags> =
        moka::future::Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(30))
            .build();
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..herd {
        let cache = future_cache.clone();
        let repo = user_repo.clone();
        let queries = queries.clone();
        let uid = seeded.user_id;
        handles.push(tokio::spawn(async move {
            cache
                .try_get_with(uid, async {
                    queries.fetch_add(1, Ordering::Relaxed);
                    repo.get_user_flags(uid).await
                })
                .await
                .expect("flags")
        }));
    }
    let mut after_flags = Vec::new();
    for h in handles {
        after_flags.push(h.await.unwrap());
    }
    let after_wall = t0.elapsed().as_secs_f64() * 1e3;
    let after_queries = queries.load(Ordering::Relaxed);

    println!("[4] user-flags cold-cache herd of {herd}");
    println!("    BEFORE (get→insert)    {before_wall:7.2} ms   {before_queries} queries");
    println!("    AFTER  (try_get_with)  {after_wall:7.2} ms   {after_queries} queries");

    for f in before_flags.iter().chain(&after_flags) {
        if *f != before_flags[0] {
            eprintln!("GATE FAIL user flags mismatch");
            ok = false;
        }
    }

    cleanup(&pool, &seeded).await;
    println!(
        "\n[gate] {}",
        if ok {
            "OK (identical result sets)"
        } else {
            "FAILED"
        }
    );
    if !ok {
        std::process::exit(1);
    }
}
