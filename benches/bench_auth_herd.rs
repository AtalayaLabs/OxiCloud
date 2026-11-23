//! Basic-auth thundering-herd benchmark — K concurrent cache misses.
//!
//! Every WebDAV/CalDAV/CardDAV/NextCloud request authenticates through
//! `AppPasswordService::verify_basic_auth`. The cache (TTL 300 s) used to be
//! a plain get/insert: when a sync client holding K parallel connections hit
//! an expired entry, all K in-flight requests missed simultaneously and each
//! ran the full slow path — an Argon2id verification at ~64 MiB / t=3 / p=2
//! apiece (100-300 ms CPU each). `try_get_with` now coalesces concurrent
//! misses into ONE verification; failed verifications stay uncached.
//!
//! Sections:
//!   BEFORE (emulated) — K concurrent bare Argon2id verifications, the exact
//!                       work the old code fanned out per herd
//!   AFTER             — K concurrent verify_basic_auth on a cold cache
//!                       (single-flight: 1 verification, K-1 waiters)
//!   warm-hit          — p50 of the cached path
//!
//! Gate: AFTER's process-CPU delta must be ~1 verification (< 2x a single
//! verify), while BEFORE burns ~K of them. All K results must be Ok and
//! identical.
//!
//! Run (needs Postgres up; reads DATABASE_URL / OXICLOUD_DB_CONNECTION_STRING
//! from .env):
//!   cargo bench --features=bench --bench bench_auth_herd
//! Tunables: BENCH_HERD (8)

use std::env;
use std::sync::Arc;
use std::time::Instant;

use oxicloud::application::services::app_password_service::AppPasswordService;
use oxicloud::infrastructure::repositories::pg::{AppPasswordPgRepository, UserPgRepository};
use oxicloud::infrastructure::services::password_hasher::Argon2PasswordHasher;
use sqlx::postgres::PgPoolOptions;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

use oxicloud::bench_support::process_metrics::cpu_seconds;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL");
    let herd: usize = env_or("BENCH_HERD", 8);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(10)
            .connect(&url)
            .await
            .expect("connect"),
    );

    // ── Seed: user + NC-format app password (production Argon2 params) ──
    let username = format!("bench_herd_{}", std::process::id());
    let user_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, password_hash, role)
         VALUES ($1, $2, '', 'user') RETURNING id",
    )
    .bind(&username)
    .bind(format!("{username}@bench.invalid"))
    .fetch_one(pool.as_ref())
    .await
    .expect("seed user");

    // Production defaults: m=64 MiB, t=3, p=2 (config.rs auth defaults).
    let hasher = Arc::new(Argon2PasswordHasher::new(65536, 3, 2));
    let svc = Arc::new(AppPasswordService::new(
        Arc::new(AppPasswordPgRepository::new(pool.clone())),
        hasher.clone(),
        Arc::new(UserPgRepository::new(pool.clone())),
        "http://localhost".into(),
        // require_verified_email — the bench measures the Argon2 herd,
        // not the admission policy.
        false,
    ));
    let (_ap_id, plain) = svc.create_nc(user_id, "bench").await.expect("create_nc");

    // ── Single-verify baseline (what one Argon2id run costs here) ──────
    use oxicloud::application::ports::auth_ports::PasswordHasherPort;
    let ref_hash = hasher.hash_password("benchpw").await.expect("hash");
    let t = Instant::now();
    let c = cpu_seconds();
    assert!(
        hasher
            .verify_password("benchpw", &ref_hash)
            .await
            .expect("verify")
    );
    let one_wall = t.elapsed().as_secs_f64();
    let one_cpu = cpu_seconds() - c;
    println!(
        "single Argon2id verify: {:.0} ms wall, {:.0} ms CPU",
        one_wall * 1000.0,
        one_cpu * 1000.0
    );

    // ── BEFORE (emulated): K concurrent bare verifications ─────────────
    let t = Instant::now();
    let c = cpu_seconds();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..herd {
        let h = hasher.clone();
        let rh = ref_hash.clone();
        set.spawn(async move { h.verify_password("benchpw", &rh).await.expect("verify") });
    }
    while let Some(r) = set.join_next().await {
        assert!(r.expect("join"));
    }
    let before_wall = t.elapsed().as_secs_f64();
    let before_cpu = cpu_seconds() - c;

    // ── AFTER: K concurrent verify_basic_auth on a cold cache ──────────
    let t = Instant::now();
    let c = cpu_seconds();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..herd {
        let s = svc.clone();
        let u = username.clone();
        let p = plain.clone();
        set.spawn(async move { s.verify_basic_auth(&u, &p).await });
    }
    let mut ids = Vec::new();
    while let Some(r) = set.join_next().await {
        let (uid, uname, _) = r.expect("join").expect("verify_basic_auth");
        assert_eq!(&*uname, username.as_str());
        ids.push(uid);
    }
    assert!(ids.iter().all(|&u| u == user_id));
    let after_wall = t.elapsed().as_secs_f64();
    let after_cpu = cpu_seconds() - c;

    // ── Warm hit p50 ────────────────────────────────────────────────────
    let mut lat = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let t = Instant::now();
        let _ = svc
            .verify_basic_auth(&username, &plain)
            .await
            .expect("warm hit");
        lat.push(t.elapsed().as_secs_f64() * 1e6);
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let warm_p50 = lat[lat.len() / 2];

    println!("\n# herd of {herd} concurrent Basic Auth verifications, cold cache");
    println!(
        "{:<22} {:>10} {:>10} {:>14}",
        "variant", "wall ms", "CPU ms", "verifications"
    );
    println!(
        "{:<22} {:>10.0} {:>10.0} {:>14.1}",
        "BEFORE (per-caller)",
        before_wall * 1000.0,
        before_cpu * 1000.0,
        before_cpu / one_cpu
    );
    println!(
        "{:<22} {:>10.0} {:>10.0} {:>14.1}",
        "AFTER (single-flight)",
        after_wall * 1000.0,
        after_cpu * 1000.0,
        after_cpu / one_cpu
    );
    println!("warm cache hit p50: {warm_p50:.1} us");

    // ── Cleanup ─────────────────────────────────────────────────────────
    let _ = sqlx::query("DELETE FROM auth.app_passwords WHERE user_id = $1")
        .bind(user_id)
        .execute(pool.as_ref())
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(user_id)
        .execute(pool.as_ref())
        .await;

    // ── Gate ────────────────────────────────────────────────────────────
    // AFTER must coalesce to ~1 verification's CPU; 2x headroom for
    // scheduler noise. BEFORE must show the herd actually fanned out.
    if after_cpu > one_cpu * 2.0 {
        eprintln!(
            "GATE FAIL: single-flight AFTER burned {:.1} verifications of CPU (expected ~1)",
            after_cpu / one_cpu
        );
        std::process::exit(1);
    }
    if before_cpu < one_cpu * (herd as f64) * 0.6 {
        eprintln!(
            "GATE WARN: BEFORE emulation did not saturate ({:.1} verifs)",
            before_cpu / one_cpu
        );
    }
    println!("\nGATE PASS: cold-cache herd coalesced to ~1 Argon2id run");
}
