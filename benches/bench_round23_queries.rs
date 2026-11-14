//! Round-23 PostgreSQL query-shape pack — end-to-end latency + equivalence on
//! the live dev Postgres. The deterministic alloc gates for the decode/clone
//! candidates live in `bench_round23_micro.rs`; this harness measures the real
//! round-trip / decode wins against seeded fixtures and asserts identical
//! results (the equivalence gate — a mismatch `std::process::exit(1)`s).
//!
//!   [Q1] Contact JSONB decode on REAL rows (contact_pg §J1): fetch a seeded
//!        address book's contacts once, then decode the `email`/`phone`/`address`
//!        JSONB columns BEFORE (`row.get::<Value>` + `from_value`) vs AFTER
//!        (`row.try_get::<sqlx::types::Json<Vec<Dto>>>`). Gate: identical decode.
//!
//!   [Q4] `get_user_profile` (§P1): two independent point reads of the caller +
//!        target users, BEFORE serial (`await` then `await`) vs AFTER concurrent
//!        (`tokio::join!`). Gate: identical rows.
//!
//!   [Q6] `subject_group::remove_member` (§G1): the child group's transitive
//!        user set (a recursive CTE) BEFORE computed TWICE (the shipped-before
//!        pre-check + `invalidation_targets`) vs AFTER once + reused. Gate:
//!        identical user set.
//!
//! Run (needs the dev Postgres up; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_round23_queries
//! Tunables (env): BENCH_PASSES (200), Q1_CONTACTS (500), Q1_DECODE_PASSES (4000)

use std::env;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn p50(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

fn report(tag: &str, unit: &str, before: f64, after: f64) {
    println!(
        "| {:<44} | {:>12} | {:>12} | {:>7} |",
        tag, "BEFORE", "AFTER", "speedup"
    );
    println!(
        "| {:<44} | {:>12.1} | {:>12.1} | {:>6.2}x |",
        unit,
        before,
        after,
        before / after
    );
}

// ── Verbatim replicas of contact_persistence_dto.rs ──────────────────────────
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EmailDto {
    email: String,
    r#type: String,
    is_primary: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PhoneDto {
    number: String,
    r#type: String,
    is_primary: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AddressDto {
    street: Option<String>,
    city: Option<String>,
    state: Option<String>,
    postal_code: Option<String>,
    country: Option<String>,
    r#type: String,
    is_primary: bool,
}

async fn cleanup(pool: &PgPool) {
    // Idempotent teardown (also clears any fixtures a prior crashed run left).
    // Memberships first (FK to both groups and users), targeted by the bench
    // group names so it catches them whoever `added_by` is.
    let _ = sqlx::query(
        "DELETE FROM auth.subject_group_members WHERE group_id IN
           (SELECT id FROM auth.subject_groups
             WHERE name IN ('bench23parent','bench23child','bench23grand'))",
    )
    .execute(pool)
    .await;
    let _ = sqlx::query(
        "DELETE FROM auth.subject_groups WHERE name IN ('bench23parent','bench23child','bench23grand')",
    )
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM carddav.contacts WHERE uid LIKE 'bench23-%'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM carddav.address_books WHERE name = 'bench23_ab'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE email LIKE 'bench23-%@bench.invalid'")
        .execute(pool)
        .await;
}

async fn seed_user(pool: &PgPool, tag: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ($1, $2, 'user') RETURNING id",
    )
    .bind(format!("bench23_{tag}"))
    .bind(format!("bench23-{tag}@bench.invalid"))
    .fetch_one(pool)
    .await
    .expect("seed user")
}

// ── [Q1] Contact JSONB decode ────────────────────────────────────────────────
async fn section_q1(pool: &PgPool) {
    let n: usize = env_or("Q1_CONTACTS", 500);
    let passes: usize = env_or("Q1_DECODE_PASSES", 4000);

    let owner = seed_user(pool, "q1owner").await;
    let ab: Uuid = sqlx::query_scalar(
        "INSERT INTO carddav.address_books (id, name, owner_id)
         VALUES (gen_random_uuid(), 'bench23_ab', $1) RETURNING id",
    )
    .bind(owner)
    .fetch_one(pool)
    .await
    .expect("seed address book");

    for i in 0..n {
        let emails = serde_json::to_value(vec![
            EmailDto {
                email: format!("user{i}@example.com"),
                r#type: "home".into(),
                is_primary: true,
            },
            EmailDto {
                email: format!("user{i}@work.example.com"),
                r#type: "work".into(),
                is_primary: false,
            },
        ])
        .unwrap();
        let phones = serde_json::to_value(vec![PhoneDto {
            number: format!("+1-555-01{i:04}"),
            r#type: "cell".into(),
            is_primary: true,
        }])
        .unwrap();
        let addrs = serde_json::to_value(vec![AddressDto {
            street: Some(format!("{} Main St", 100 + i)),
            city: Some("Springfield".into()),
            state: Some("IL".into()),
            postal_code: Some("62704".into()),
            country: Some("US".into()),
            r#type: "home".into(),
            is_primary: true,
        }])
        .unwrap();
        sqlx::query(
            "INSERT INTO carddav.contacts (id, address_book_id, uid, full_name, email, phone, address, etag)
             VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(ab)
        .bind(format!("bench23-{i}"))
        .bind(format!("Contact {i}"))
        .bind(&emails)
        .bind(&phones)
        .bind(&addrs)
        .bind(format!("etag-{i}"))
        .execute(pool)
        .await
        .expect("seed contact");
    }

    // Fetch the rows ONCE (the query round-trip is out of the measured window —
    // we isolate the per-row decode, which is what §J1 changes).
    let rows = sqlx::query(
        "SELECT email, phone, address FROM carddav.contacts
         WHERE address_book_id = $1 ORDER BY uid",
    )
    .bind(ab)
    .fetch_all(pool)
    .await
    .expect("fetch contacts");
    assert_eq!(rows.len(), n, "Q1 seeded row count");

    // BEFORE: Value DOM + from_value per column.
    let decode_before =
        |rows: &[sqlx::postgres::PgRow]| -> Vec<(Vec<EmailDto>, Vec<PhoneDto>, Vec<AddressDto>)> {
            rows.iter()
                .map(|r| {
                    let ev: Value = r.get("email");
                    let pv: Value = r.get("phone");
                    let av: Value = r.get("address");
                    (
                        serde_json::from_value::<Vec<EmailDto>>(ev).unwrap_or_default(),
                        serde_json::from_value::<Vec<PhoneDto>>(pv).unwrap_or_default(),
                        serde_json::from_value::<Vec<AddressDto>>(av).unwrap_or_default(),
                    )
                })
                .collect()
        };
    // AFTER: typed Json<T> decode straight from the JSONB bytes.
    let decode_after =
        |rows: &[sqlx::postgres::PgRow]| -> Vec<(Vec<EmailDto>, Vec<PhoneDto>, Vec<AddressDto>)> {
            rows.iter()
                .map(|r| {
                    (
                        r.try_get::<sqlx::types::Json<Vec<EmailDto>>, _>("email")
                            .map(|j| j.0)
                            .unwrap_or_default(),
                        r.try_get::<sqlx::types::Json<Vec<PhoneDto>>, _>("phone")
                            .map(|j| j.0)
                            .unwrap_or_default(),
                        r.try_get::<sqlx::types::Json<Vec<AddressDto>>, _>("address")
                            .map(|j| j.0)
                            .unwrap_or_default(),
                    )
                })
                .collect()
        };

    // Equivalence gate.
    if decode_before(&rows) != decode_after(&rows) {
        eprintln!("GATE FAIL [Q1]: BEFORE/AFTER decode differ — rollback");
        cleanup(pool).await;
        std::process::exit(1);
    }

    let mut b = Vec::with_capacity(passes);
    let mut a = Vec::with_capacity(passes);
    for _ in 0..passes / 20 {
        std::hint::black_box(decode_before(&rows));
        std::hint::black_box(decode_after(&rows));
    }
    for _ in 0..passes {
        let t = Instant::now();
        std::hint::black_box(decode_before(&rows));
        b.push(t.elapsed().as_nanos() as f64 / n as f64);
        let t = Instant::now();
        std::hint::black_box(decode_after(&rows));
        a.push(t.elapsed().as_nanos() as f64 / n as f64);
    }

    println!(
        "\n## [Q1] Contact JSONB decode on real rows ({n} contacts) — gate OK (identical decode)"
    );
    report(
        "Value DOM + from_value vs Json<T>",
        "p50 ns/contact",
        p50(b),
        p50(a),
    );
}

// ── [Q4] get_user_profile: serial vs join! ──────────────────────────────────
async fn section_q4(pool: &PgPool) {
    let passes: usize = env_or("BENCH_PASSES", 200);
    let caller = seed_user(pool, "q4caller").await;
    let target = seed_user(pool, "q4target").await;

    // Capture `pool` (not take it as a param) so the returned future borrows a
    // single concrete lifetime — a closure param `&PgPool` + future return hits
    // the HRTB limitation.
    let read = |id: Uuid| async move {
        sqlx::query("SELECT id, email, role FROM auth.users WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .expect("read user")
            .map(|r| r.get::<Uuid, _>("id"))
    };

    // Equivalence gate: same two ids either way.
    let ser = (read(caller).await, read(target).await);
    let (jc, jt) = tokio::join!(read(caller), read(target));
    if ser != (jc, jt) {
        eprintln!("GATE FAIL [Q4]: serial/join ids differ — rollback");
        cleanup(pool).await;
        std::process::exit(1);
    }

    let mut b = Vec::with_capacity(passes);
    let mut a = Vec::with_capacity(passes);
    for _ in 0..(passes / 20).max(1) {
        let _ = (read(caller).await, read(target).await);
        let _ = tokio::join!(read(caller), read(target));
    }
    for _ in 0..passes {
        let t = Instant::now();
        let _ = std::hint::black_box((read(caller).await, read(target).await));
        b.push(t.elapsed().as_nanos() as f64);
        let t = Instant::now();
        let _ = std::hint::black_box(tokio::join!(read(caller), read(target)));
        a.push(t.elapsed().as_nanos() as f64);
    }

    println!("\n## [Q4] get_user_profile caller+target reads — gate OK (identical ids)");
    report(
        "2 serial reads vs tokio::join!",
        "p50 ns/call",
        p50(b),
        p50(a),
    );
}

// ── [Q6] subject_group child transitive users: 2 CTEs vs 1 ───────────────────
async fn section_q6(pool: &PgPool) {
    let passes: usize = env_or("BENCH_PASSES", 200);
    // Tree: parent → child → {grandchild, u2}; grandchild → u3. u1 direct on parent.
    let u1 = seed_user(pool, "q6u1").await;
    let u2 = seed_user(pool, "q6u2").await;
    let u3 = seed_user(pool, "q6u3").await;
    let mk_group = |name: &'static str| async move {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO auth.subject_groups (name) VALUES ($1) RETURNING id",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("seed group")
    };
    let parent = mk_group("bench23parent").await;
    let child = mk_group("bench23child").await;
    let grand = mk_group("bench23grand").await;
    let add_ug = |g: Uuid, u: Uuid| async move {
        sqlx::query("INSERT INTO auth.subject_group_members (group_id, member_user_id, added_by) VALUES ($1, $2, $3)")
            .bind(g).bind(u).bind(u1).execute(pool).await.expect("add user member");
    };
    let add_gg = |g: Uuid, c: Uuid| async move {
        sqlx::query("INSERT INTO auth.subject_group_members (group_id, member_group_id, added_by) VALUES ($1, $2, $3)")
            .bind(g).bind(c).bind(u1).execute(pool).await.expect("add group member");
    };
    add_ug(parent, u1).await;
    add_gg(parent, child).await;
    add_gg(child, grand).await;
    add_ug(child, u2).await;
    add_ug(grand, u3).await;

    let cte = |gid: Uuid| async move {
        let rows = sqlx::query(
            "WITH RECURSIVE descendants AS (
                 SELECT $1::uuid AS g
                 UNION
                 SELECT m.member_group_id FROM auth.subject_group_members m
                   JOIN descendants d ON m.group_id = d.g WHERE m.member_group_id IS NOT NULL)
             SELECT DISTINCT m.member_user_id AS user_id FROM auth.subject_group_members m
               JOIN descendants d ON m.group_id = d.g WHERE m.member_user_id IS NOT NULL",
        )
        .bind(gid)
        .fetch_all(pool)
        .await
        .expect("cte");
        let mut ids: Vec<Uuid> = rows.iter().map(|r| r.get::<Uuid, _>("user_id")).collect();
        ids.sort();
        ids
    };

    // Equivalence: the child's transitive set is {u2, u3}, and it is IDENTICAL
    // whether computed once or twice (the edge delete above the child cannot
    // change its descendants — the §G1 correctness claim).
    let once = cte(child).await;
    let twice = {
        let _first = cte(child).await;
        cte(child).await
    };
    let mut expected = [u2, u3];
    expected.sort();
    if once != twice || once != expected {
        eprintln!("GATE FAIL [Q6]: child transitive set not stable/expected — rollback");
        cleanup(pool).await;
        std::process::exit(1);
    }

    let mut b = Vec::with_capacity(passes);
    let mut a = Vec::with_capacity(passes);
    for _ in 0..(passes / 20).max(1) {
        let _ = (cte(child).await, cte(child).await);
        let _ = cte(child).await;
    }
    for _ in 0..passes {
        // BEFORE: the child CTE runs TWICE (pre-check + invalidation_targets).
        let t = Instant::now();
        let _ = cte(child).await;
        let _ = std::hint::black_box(cte(child).await);
        b.push(t.elapsed().as_nanos() as f64);
        // AFTER: once, reused.
        let t = Instant::now();
        let _ = std::hint::black_box(cte(child).await);
        a.push(t.elapsed().as_nanos() as f64);
    }

    println!("\n## [Q6] subject_group child transitive users — gate OK (stable set {{u2,u3}})");
    report(
        "2 recursive CTEs vs 1 (reused)",
        "p50 ns/removal",
        p50(b),
        p50(a),
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect Postgres");

    println!("# Round-23 PG query-shape pack — BEFORE/AFTER (live Postgres)");
    println!("# Each section asserts an equivalence gate (mismatch → exit 1) and reports p50.");

    cleanup(&pool).await;
    section_q1(&pool).await;
    section_q4(&pool).await;
    section_q6(&pool).await;
    cleanup(&pool).await;

    println!("\nAll Round-23 query sections passed their equivalence gate.");
}
