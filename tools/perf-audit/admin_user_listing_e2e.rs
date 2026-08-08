//! Component-faithful A/B for `GET /api/admin/users`.
//!
//! Historical path: full-row SQL -> full DTO -> count SQL -> direct Serde JSON.
//! Candidate path: hot Moka `get_user_flags` equivalent -> policy check ->
//! summary SQL -> summary DTO -> count SQL -> Serde JSON.
//!
//! The harness uses the exact production column sets and response fields but
//! deliberately stays independent of the OxiCloud crate. That keeps it small
//! enough for fresh-process max-RSS gates while disclosing that router/JWT and
//! socket-level HTTP framing are common work and are not modeled. It also omits
//! the old handler's intermediate `serde_json::Value` materialization, making
//! the historical side optimistic and the accepted speedup conservative.

use chrono::{DateTime, Utc};
use moka::future::Cache;
use serde::Serialize;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::env;
use std::hint::black_box;
use std::time::{Duration, Instant};
use uuid::Uuid;

const USERS: i64 = 100;
const LIMIT: i64 = 100;
const OFFSET: i64 = 0;

#[derive(Clone, Copy, Debug)]
enum Profile {
    Minimal,
    Heavy,
}

impl Profile {
    fn parse(value: &str) -> Self {
        match value {
            "minimal" => Self::Minimal,
            "heavy" => Self::Heavy,
            _ => panic!("profile must be minimal or heavy"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Heavy => "heavy",
        }
    }

    fn is_heavy(self) -> bool {
        matches!(self, Self::Heavy)
    }
}

#[derive(Clone, Copy)]
struct UserFlags {
    admin: bool,
    is_external: bool,
    active: bool,
}

#[derive(Debug, Serialize)]
struct FullUserDto {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    email: String,
    role: String,
    storage_quota_bytes: i64,
    storage_used_bytes: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_login_at: Option<DateTime<Utc>>,
    active: bool,
    federation_issuer: String,
    image: Option<String>,
    can_edit_image: bool,
    is_external: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    given_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    family_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email_verified_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_locale: Option<String>,
    notify_on_share: bool,
    ui_preferences: Value,
}

impl FullUserDto {
    fn summary(&self) -> SummaryUserDto {
        SummaryUserDto {
            id: self.id.clone(),
            username: self.username.clone(),
            email: self.email.clone(),
            role: self.role.clone(),
            storage_quota_bytes: self.storage_quota_bytes,
            storage_used_bytes: self.storage_used_bytes,
            last_login_at: self.last_login_at,
            active: self.active,
            federation_issuer: self.federation_issuer.clone(),
            is_external: self.is_external,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct SummaryUserDto {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    email: String,
    role: String,
    storage_quota_bytes: i64,
    storage_used_bytes: i64,
    last_login_at: Option<DateTime<Utc>>,
    active: bool,
    federation_issuer: String,
    is_external: bool,
}

#[derive(Serialize)]
struct Page<T> {
    users: Vec<T>,
    total: i64,
    limit: i64,
    offset: i64,
}

#[derive(Serialize)]
struct TimingReport {
    profile: &'static str,
    users: i64,
    warmups: usize,
    samples: usize,
    order: &'static str,
    historical_full_samples_ms: Vec<f64>,
    candidate_summary_hot_authz_samples_ms: Vec<f64>,
    historical_full_median_ms: f64,
    candidate_summary_hot_authz_median_ms: f64,
    speedup: f64,
    historical_json_bytes: usize,
    candidate_json_bytes: usize,
    byte_reduction_percent: f64,
    summary_projection_equal: bool,
    total_equal: bool,
}

async fn setup(pool: &PgPool, profile: Profile) {
    sqlx::query(
        "CREATE TEMP TABLE perf_admin_endpoint_users (
            id uuid PRIMARY KEY,
            username text,
            email text NOT NULL,
            password_hash text,
            role text NOT NULL,
            storage_quota_bytes bigint NOT NULL,
            storage_used_bytes bigint NOT NULL,
            created_at timestamptz NOT NULL,
            updated_at timestamptz NOT NULL,
            last_login_at timestamptz,
            active boolean NOT NULL,
            oidc_provider text,
            oidc_subject text,
            image text,
            is_external boolean NOT NULL,
            given_name text,
            family_name text,
            email_verified_at timestamptz,
            preferred_locale text,
            notify_on_share boolean NOT NULL,
            ui_preferences jsonb NOT NULL
        )",
    )
    .execute(pool)
    .await
    .expect("create fixture table");

    sqlx::query(
        "WITH payload AS (
            SELECT string_agg(md5(i::text || ':admin-e2e'), '') AS random_hex
              FROM generate_series(1, 16384) AS i
         )
         INSERT INTO perf_admin_endpoint_users
         SELECT
            gen_random_uuid(),
            'perf-user-' || n,
            'perf-user-' || n || '@example.invalid',
            '$argon2id$v=19$m=19456,t=2,p=1$benchmark-only',
            CASE WHEN n = 1 THEN 'admin' ELSE 'user' END,
            10737418240,
            n::bigint * 1048576,
            timestamptz '2026-01-01 00:00:00+00' + n * interval '1 second',
            timestamptz '2026-01-02 00:00:00+00' + n * interval '1 second',
            timestamptz '2026-01-03 00:00:00+00' + n * interval '1 second',
            true,
            CASE WHEN n % 3 = 0 THEN 'keycloak' END,
            CASE WHEN n % 3 = 0 THEN 'subject-' || n END,
            CASE WHEN $1 THEN 'data:image/webp;base64,' || payload.random_hex END,
            false,
            CASE WHEN $1 THEN 'Given' || n END,
            CASE WHEN $1 THEN 'Family' || n END,
            CASE WHEN $1 THEN timestamptz '2026-01-04 00:00:00+00' END,
            CASE WHEN $1 THEN 'es' END,
            true,
            CASE WHEN $1
                 THEN jsonb_build_object('perf_blob', left(payload.random_hex, 8192))
                 ELSE '{}'::jsonb
            END
         FROM generate_series(1, $2::bigint) AS n
         CROSS JOIN payload",
    )
    .bind(profile.is_heavy())
    .bind(USERS)
    .execute(pool)
    .await
    .expect("seed fixture users");

    sqlx::query(
        "CREATE INDEX perf_admin_endpoint_created_at_idx
            ON perf_admin_endpoint_users (created_at DESC)",
    )
    .execute(pool)
    .await
    .expect("create listing index");
    sqlx::query("ANALYZE perf_admin_endpoint_users")
        .execute(pool)
        .await
        .expect("analyze fixture");
}

async fn load_full(pool: &PgPool) -> Vec<FullUserDto> {
    let rows = sqlx::query(
        "SELECT
            id, username, email, password_hash, role AS role_text,
            storage_quota_bytes, storage_used_bytes, created_at, updated_at,
            last_login_at, active, oidc_provider, oidc_subject, image,
            is_external, given_name, family_name, email_verified_at,
            preferred_locale, notify_on_share, ui_preferences
         FROM perf_admin_endpoint_users
         WHERE ($3 OR is_external = FALSE)
         ORDER BY created_at DESC, id DESC
         LIMIT $1 OFFSET $2",
    )
    .bind(LIMIT)
    .bind(OFFSET)
    .bind(true)
    .fetch_all(pool)
    .await
    .expect("fetch full users");

    rows.into_iter()
        .map(|row| {
            // Decode the two fetched-but-not-serialized fields as production's
            // full User construction does; omitting them would flatter history.
            let _password_hash: Option<String> = row.get("password_hash");
            let _oidc_subject: Option<String> = row.get("oidc_subject");
            let oidc_provider: Option<String> = row.get("oidc_provider");
            let can_edit_image = oidc_provider.is_none();
            FullUserDto {
                id: row.get::<Uuid, _>("id").to_string(),
                username: row.get("username"),
                email: row.get("email"),
                role: row.get("role_text"),
                storage_quota_bytes: row.get("storage_quota_bytes"),
                storage_used_bytes: row.get("storage_used_bytes"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
                last_login_at: row.get("last_login_at"),
                active: row.get("active"),
                federation_issuer: oidc_provider.unwrap_or_else(|| "local".to_owned()),
                image: row.get("image"),
                can_edit_image,
                is_external: row.get("is_external"),
                given_name: row.get("given_name"),
                family_name: row.get("family_name"),
                email_verified_at: row.get("email_verified_at"),
                preferred_locale: row.get("preferred_locale"),
                notify_on_share: row.get("notify_on_share"),
                ui_preferences: row.get("ui_preferences"),
            }
        })
        .collect()
}

async fn load_summary(pool: &PgPool) -> Vec<SummaryUserDto> {
    sqlx::query(
        "SELECT
            id, username, email, role AS role_text,
            storage_quota_bytes, storage_used_bytes,
            last_login_at, active, oidc_provider, is_external
         FROM perf_admin_endpoint_users
         WHERE ($3 OR is_external = FALSE)
         ORDER BY created_at DESC, id DESC
         LIMIT $1 OFFSET $2",
    )
    .bind(LIMIT)
    .bind(OFFSET)
    .bind(true)
    .fetch_all(pool)
    .await
    .expect("fetch summary users")
    .into_iter()
    .map(|row| SummaryUserDto {
        id: row.get::<Uuid, _>("id").to_string(),
        username: row.get("username"),
        email: row.get("email"),
        role: row.get("role_text"),
        storage_quota_bytes: row.get("storage_quota_bytes"),
        storage_used_bytes: row.get("storage_used_bytes"),
        last_login_at: row.get("last_login_at"),
        active: row.get("active"),
        federation_issuer: row
            .get::<Option<String>, _>("oidc_provider")
            .unwrap_or_else(|| "local".to_owned()),
        is_external: row.get("is_external"),
    })
    .collect()
}

async fn count_users(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM perf_admin_endpoint_users")
        .fetch_one(pool)
        .await
        .expect("count fixture users")
}

async fn historical_response(pool: &PgPool) -> Vec<u8> {
    let users = load_full(pool).await;
    let total = count_users(pool).await;
    serde_json::to_vec(&Page {
        users,
        total,
        limit: LIMIT,
        offset: OFFSET,
    })
    .expect("serialize full response")
}

async fn candidate_response(
    pool: &PgPool,
    flags_cache: &Cache<Uuid, UserFlags>,
    admin_id: Uuid,
) -> Vec<u8> {
    let flags = flags_cache
        .try_get_with(admin_id, async {
            Err::<UserFlags, &'static str>("unexpected miss in hot-cache gate")
        })
        .await
        .expect("hot flags cache");
    assert!(flags.admin && !flags.is_external && flags.active);

    let users = load_summary(pool).await;
    let total = count_users(pool).await;
    serde_json::to_vec(&Page {
        users,
        total,
        limit: LIMIT,
        offset: OFFSET,
    })
    .expect("serialize summary response")
}

async fn correctness(pool: &PgPool) {
    let full = load_full(pool).await;
    let summary = load_summary(pool).await;
    let projected: Vec<SummaryUserDto> = full.iter().map(FullUserDto::summary).collect();
    assert_eq!(
        projected, summary,
        "summary projection changed table fields/order"
    );
    assert_eq!(count_users(pool).await, USERS);
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

async fn run_timing(
    pool: &PgPool,
    profile: Profile,
    cache: &Cache<Uuid, UserFlags>,
    admin_id: Uuid,
    samples: usize,
) {
    correctness(pool).await;

    let warmups = 3;
    for warmup in 0..warmups {
        if warmup % 2 == 0 {
            black_box(historical_response(pool).await);
            black_box(candidate_response(pool, cache, admin_id).await);
        } else {
            black_box(candidate_response(pool, cache, admin_id).await);
            black_box(historical_response(pool).await);
        }
    }

    let mut historical = Vec::with_capacity(samples);
    let mut candidate = Vec::with_capacity(samples);
    let mut historical_bytes = 0;
    let mut candidate_bytes = 0;
    for sample in 0..samples {
        if sample % 2 == 0 {
            let start = Instant::now();
            let body = historical_response(pool).await;
            historical.push(elapsed_ms(start));
            historical_bytes = body.len();
            black_box(body);

            let start = Instant::now();
            let body = candidate_response(pool, cache, admin_id).await;
            candidate.push(elapsed_ms(start));
            candidate_bytes = body.len();
            black_box(body);
        } else {
            let start = Instant::now();
            let body = candidate_response(pool, cache, admin_id).await;
            candidate.push(elapsed_ms(start));
            candidate_bytes = body.len();
            black_box(body);

            let start = Instant::now();
            let body = historical_response(pool).await;
            historical.push(elapsed_ms(start));
            historical_bytes = body.len();
            black_box(body);
        }
    }

    let historical_median = median(&historical);
    let candidate_median = median(&candidate);
    let report = TimingReport {
        profile: profile.as_str(),
        users: USERS,
        warmups,
        samples,
        order: "interleaved and alternated",
        historical_full_samples_ms: historical,
        candidate_summary_hot_authz_samples_ms: candidate,
        historical_full_median_ms: historical_median,
        candidate_summary_hot_authz_median_ms: candidate_median,
        speedup: historical_median / candidate_median,
        historical_json_bytes: historical_bytes,
        candidate_json_bytes: candidate_bytes,
        byte_reduction_percent: (1.0 - candidate_bytes as f64 / historical_bytes as f64) * 100.0,
        summary_projection_equal: true,
        total_equal: true,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("serialize timing report")
    );
}

async fn run_memory(
    pool: &PgPool,
    profile: Profile,
    mode: &str,
    cache: &Cache<Uuid, UserFlags>,
    admin_id: Uuid,
) {
    let start = Instant::now();
    let body = match mode {
        "historical" => historical_response(pool).await,
        "candidate" => candidate_response(pool, cache, admin_id).await,
        _ => panic!("memory mode must be historical or candidate"),
    };
    let elapsed = elapsed_ms(start);
    black_box(&body);
    println!(
        "mode={mode} profile={} users={USERS} elapsed_ms={elapsed:.6} json_bytes={}",
        profile.as_str(),
        body.len()
    );
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("timing");
    let profile = Profile::parse(args.get(2).map(String::as_str).unwrap_or("minimal"));
    let samples = args
        .get(3)
        .map(|value| value.parse::<usize>().expect("samples must be an integer"))
        .unwrap_or(21);

    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await
        .expect("connect benchmark database");
    setup(&pool, profile).await;

    let admin_id: Uuid =
        sqlx::query_scalar("SELECT id FROM perf_admin_endpoint_users WHERE role = 'admin' LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("fixture admin id");
    let cache = Cache::builder()
        .max_capacity(10_000)
        .time_to_live(Duration::from_secs(30))
        .build();
    cache
        .insert(
            admin_id,
            UserFlags {
                admin: true,
                is_external: false,
                active: true,
            },
        )
        .await;

    match command {
        "timing" => run_timing(&pool, profile, &cache, admin_id, samples).await,
        "memory-historical" => run_memory(&pool, profile, "historical", &cache, admin_id).await,
        "memory-candidate" => run_memory(&pool, profile, "candidate", &cache, admin_id).await,
        _ => panic!(
            "usage: admin_user_listing_e2e [timing|memory-historical|memory-candidate] [minimal|heavy] [samples]"
        ),
    }
}
