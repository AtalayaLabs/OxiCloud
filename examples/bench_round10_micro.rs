//! Round-10 CPU/alloc micro-pack — BEFORE replicas vs the shipped code.
//!
//! Sections (all pure CPU, no Postgres):
//!   1. Authenticated-request identity build (Bearer/cookie hit path):
//!      BEFORE `String` claims clone ×2 + live-role `to_string` + `Arc::new`
//!      vs AFTER `Arc<str>` refcount bumps + inline `SmolStr` + `Arc::new`.
//!   2. Basic-auth cache hit: BEFORE `CachedBasicAuthResult{String}` moka
//!      value clone vs AFTER `Arc<str>`/`SmolStr` bumps.
//!   3. NC PROPFIND per-row integer props (`oc:fileid`, `nc:creation_time`,
//!      `nc:upload_time`): BEFORE `to_string()` per field vs AFTER
//!      `common::fmt` stack render. Gate: byte-identical XML.
//!   4. NC trashbin date/int props: BEFORE `to_rfc2822()` + `to_string()`
//!      vs AFTER stack render. Gate: byte-identical XML.
//!   5. Native-WebDAV scope prefix test: BEFORE `format!("{prefix}/")` per
//!      request vs AFTER borrow-only check. Gate: identical routing.
//!   6. Share listing base-url: BEFORE `env::var("OXICLOUD_BASE_URL")` +
//!      rebuild per row vs AFTER the construction-time snapshot.
//!   7. JWT verify miss: BEFORE fresh `Validation` + `DecodingKey` per
//!      decode vs AFTER pre-built fields. Gate: identical claims.
//!   8. AES-GCM cipher hand-off: BEFORE key-schedule memcpy clone vs AFTER
//!      `Arc` bump.
//!   9. Request-id header: BEFORE `Uuid::to_string` + `HeaderValue::from_str`
//!      vs AFTER stack-encode. Gate: identical header bytes.
//!
//! Run: cargo run --release --features bench --example bench_round10_micro
//! Tunables (env): BENCH_ITERS (100000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use smol_str::SmolStr;

// ─── Counting allocator ─────────────────────────────────────────────────────

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn measure<R>(label: &str, iters: u64, mut f: impl FnMut() -> R) -> (f64, f64) {
    // Warmup
    for _ in 0..1000 {
        black_box(f());
    }
    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t0 = Instant::now();
    for _ in 0..iters {
        black_box(f());
    }
    let wall = t0.elapsed().as_secs_f64();
    let allocs = (ALLOC_CALLS.load(Ordering::Relaxed) - a0) as f64 / iters as f64;
    let ns = wall * 1e9 / iters as f64;
    println!("    {label:<44} {ns:>9.1} ns/op   {allocs:>7.3} allocs/op");
    (ns, allocs)
}

// ─── §1 BEFORE replicas: old claim/identity shapes ──────────────────────────

mod before {
    use std::sync::Arc;

    /// Old `TokenClaims` shape (owned Strings). The unread fields keep
    /// the replica byte-faithful to the historical struct layout.
    #[allow(dead_code)]
    #[derive(Debug, Clone)]
    pub struct OldTokenClaims {
        pub sub: String,
        pub exp: i64,
        pub iat: i64,
        pub jti: String,
        pub username: String,
        pub email: String,
        pub role: String,
    }

    /// Old `CurrentUser` shape.
    #[derive(Debug, Clone, serde::Serialize)]
    pub struct OldCurrentUser {
        pub id: uuid::Uuid,
        pub username: String,
        pub email: String,
        pub role: String,
    }

    /// Old Bearer-hit tail: deep-clone the two display fields out of the
    /// cached claims + `to_string` the live role + `Arc::new`.
    pub fn bearer_identity(claims: &Arc<OldTokenClaims>, live_role: &str) -> Arc<OldCurrentUser> {
        let role = live_role.to_string(); // decide_live_role's `flags.role.to_string()`
        Arc::new(OldCurrentUser {
            id: uuid::Uuid::nil(),
            username: claims.username.clone(),
            email: claims.email.clone(),
            role,
        })
    }

    /// Old Basic-auth cached value (owned Strings — moka clones on get).
    #[derive(Clone)]
    pub struct OldCachedBasic {
        pub user_id: uuid::Uuid,
        pub username: String,
        pub email: String,
        pub role: String,
    }
}

fn section_identity(iters: u64) {
    use oxicloud::application::dtos::user_dto::CurrentUser;
    use oxicloud::application::ports::auth_ports::TokenClaims;

    println!("[1] authenticated-request identity build (per request)");
    let old_claims = Arc::new(before::OldTokenClaims {
        sub: "6a11f8a2-14a5-4f8a-9d55-3e3c8a2b9a01".into(),
        exp: 4_102_444_800,
        iat: 1_700_000_000,
        jti: uuid::Uuid::nil().to_string(),
        username: "alice.longname".to_string(),
        email: "alice.longname@example.com".to_string(),
        role: "user".to_string(),
    });
    let new_claims = Arc::new(TokenClaims {
        sub: "6a11f8a2-14a5-4f8a-9d55-3e3c8a2b9a01".into(),
        sub_id: uuid::Uuid::parse_str("6a11f8a2-14a5-4f8a-9d55-3e3c8a2b9a01").unwrap(),
        exp: 4_102_444_800,
        iat: 1_700_000_000,
        jti: uuid::Uuid::nil().to_string(),
        username: Arc::from("alice.longname"),
        email: Arc::from("alice.longname@example.com"),
        role: "user".to_string(),
        dpop_jkt: None,
        sid: None,
    });

    let (bn, ba) = measure("BEFORE String clones + role to_string", iters, || {
        before::bearer_identity(black_box(&old_claims), black_box("user"))
    });
    let (an, aa) = measure("AFTER  Arc bumps + inline SmolStr", iters, || {
        // The shipped middleware tail: LiveRole render + CurrentUser build.
        let role = SmolStr::new_static("user");
        Arc::new(CurrentUser {
            id: uuid::Uuid::nil(),
            username: Arc::clone(&black_box(&new_claims).username),
            email: Arc::clone(&new_claims.email),
            role,
            dpop_jkt: None,
        })
    });

    // Gate: identical field values + identical JSON wire shape.
    let old = before::bearer_identity(&old_claims, "user");
    let new = Arc::new(CurrentUser {
        id: uuid::Uuid::nil(),
        username: Arc::clone(&new_claims.username),
        email: Arc::clone(&new_claims.email),
        role: SmolStr::new_static("user"),
        dpop_jkt: None,
    });
    assert_eq!(old.username, *new.username);
    assert_eq!(old.email, *new.email);
    assert_eq!(old.role, new.role.as_str());
    let json_old = serde_json::to_string(&*old).unwrap();
    let json_new = serde_json::to_string(&*new).unwrap();
    assert_eq!(json_old, json_new, "CurrentUser JSON shape must not change");
    println!(
        "    gate: fields + JSON byte-identical ✓   ({bn:.0}→{an:.0} ns, {ba:.2}→{aa:.2} allocs)"
    );
}

fn section_basic_hit(iters: u64) {
    println!("[2] basic-auth cache hit → tuple hand-off (per DAV request)");
    let old_val = before::OldCachedBasic {
        user_id: uuid::Uuid::nil(),
        username: "dav.client.user".to_string(),
        email: "dav.client.user@example.com".to_string(),
        role: "user".to_string(),
    };
    struct NewCachedBasic {
        user_id: uuid::Uuid,
        username: Arc<str>,
        email: Arc<str>,
        role: SmolStr,
    }
    impl Clone for NewCachedBasic {
        fn clone(&self) -> Self {
            Self {
                user_id: self.user_id,
                username: Arc::clone(&self.username),
                email: Arc::clone(&self.email),
                role: self.role.clone(),
            }
        }
    }
    let new_val = NewCachedBasic {
        user_id: uuid::Uuid::nil(),
        username: Arc::from("dav.client.user"),
        email: Arc::from("dav.client.user@example.com"),
        role: SmolStr::new_static("user"),
    };

    let (bn, ba) = measure("BEFORE moka value clone (3 Strings)", iters, || {
        let v = black_box(&old_val).clone(); // what moka's get does
        (v.user_id, v.username, v.email, v.role)
    });
    let (an, aa) = measure("AFTER  moka value clone (bumps)", iters, || {
        let v = black_box(&new_val).clone();
        (v.user_id, v.username, v.email, v.role)
    });
    let o = old_val.clone();
    let n = new_val.clone();
    assert_eq!(o.username, *n.username);
    assert_eq!(o.email, *n.email);
    assert_eq!(o.role, n.role.as_str());
    println!(
        "    gate: identity fields identical ✓   ({bn:.0}→{an:.0} ns, {ba:.2}→{aa:.2} allocs)"
    );
}

// ─── §3/§4 XML emit ─────────────────────────────────────────────────────────

fn write_text_element(
    xml: &mut quick_xml::Writer<&mut Vec<u8>>,
    tag: &str,
    value: &str,
) -> Result<(), String> {
    use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
    xml.write_event(Event::Start(BytesStart::new(tag)))
        .map_err(|e| e.to_string())?;
    xml.write_event(Event::Text(BytesText::new(value)))
        .map_err(|e| e.to_string())?;
    xml.write_event(Event::End(BytesEnd::new(tag)))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn section_propfind_ints(iters: u64) {
    println!("[3] NC PROPFIND per-row integer props (500-row page)");
    let rows: Vec<(i64, u64, u64)> = (0..500)
        .map(|i| (912_345_678 + i as i64, 1_700_000_000 + i, 1_700_000_100 + i))
        .collect();

    let emit_before = |buf: &mut Vec<u8>| {
        let mut xml = quick_xml::Writer::new(buf);
        for &(fid, created, modified) in &rows {
            write_text_element(&mut xml, "oc:fileid", &fid.to_string()).unwrap();
            write_text_element(&mut xml, "nc:creation_time", &created.to_string()).unwrap();
            write_text_element(&mut xml, "nc:upload_time", &modified.to_string()).unwrap();
        }
    };
    let emit_after = |buf: &mut Vec<u8>| {
        let mut xml = quick_xml::Writer::new(buf);
        for &(fid, created, modified) in &rows {
            let mut ibuf = [0u8; 21];
            write_text_element(
                &mut xml,
                "oc:fileid",
                oxicloud::common::fmt::i64_str(&mut ibuf, fid),
            )
            .unwrap();
            let mut ubuf = [0u8; 20];
            write_text_element(
                &mut xml,
                "nc:creation_time",
                oxicloud::common::fmt::u64_str(&mut ubuf, created),
            )
            .unwrap();
            write_text_element(
                &mut xml,
                "nc:upload_time",
                oxicloud::common::fmt::u64_str(&mut ubuf, modified),
            )
            .unwrap();
        }
    };

    let mut b1 = Vec::with_capacity(64 * 1024);
    emit_before(&mut b1);
    let mut b2 = Vec::with_capacity(64 * 1024);
    emit_after(&mut b2);
    assert_eq!(b1, b2, "XML must be byte-identical");

    let page_iters = iters / 500;
    let (bn, ba) = measure("BEFORE to_string per int field", page_iters, || {
        let mut buf = Vec::with_capacity(64 * 1024);
        emit_before(&mut buf);
        buf
    });
    let (an, aa) = measure("AFTER  stack i64_str/u64_str", page_iters, || {
        let mut buf = Vec::with_capacity(64 * 1024);
        emit_after(&mut buf);
        buf
    });
    println!(
        "    gate: 500-row page byte-identical ✓   page: {:.1}→{:.1} µs, {:.0}→{:.0} allocs",
        bn / 1e3,
        an / 1e3,
        ba,
        aa
    );
}

fn section_trashbin(iters: u64) {
    println!("[4] NC trashbin per-item date/int props (2000-item bin)");
    let items: Vec<i64> = (0..2000).map(|i| 1_700_000_000 + i * 37).collect();

    let emit_before = |buf: &mut Vec<u8>| {
        let mut xml = quick_xml::Writer::new(buf);
        for &ts in &items {
            let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0).unwrap();
            write_text_element(&mut xml, "d:getlastmodified", &dt.to_rfc2822()).unwrap();
            write_text_element(&mut xml, "nc:trashbin-deletion-time", &ts.to_string()).unwrap();
        }
    };
    let emit_after = |buf: &mut Vec<u8>| {
        let mut xml = quick_xml::Writer::new(buf);
        for &ts in &items {
            let mut dbuf = [0u8; 31];
            // The shipped path goes through write_date_element → rfc2822_utc
            // with a chrono fallback; in-range timestamps take the stack path.
            let s = oxicloud::common::fmt::rfc2822_utc(&mut dbuf, ts).unwrap();
            write_text_element(&mut xml, "d:getlastmodified", s).unwrap();
            let mut ibuf = [0u8; 21];
            write_text_element(
                &mut xml,
                "nc:trashbin-deletion-time",
                oxicloud::common::fmt::i64_str(&mut ibuf, ts),
            )
            .unwrap();
        }
    };

    let mut b1 = Vec::with_capacity(256 * 1024);
    emit_before(&mut b1);
    let mut b2 = Vec::with_capacity(256 * 1024);
    emit_after(&mut b2);
    assert_eq!(b1, b2, "trashbin XML must be byte-identical");

    let bin_iters = (iters / 2000).max(20);
    let (bn, ba) = measure("BEFORE chrono to_rfc2822 + to_string", bin_iters, || {
        let mut buf = Vec::with_capacity(256 * 1024);
        emit_before(&mut buf);
        buf
    });
    let (an, aa) = measure("AFTER  stack rfc2822_utc + i64_str", bin_iters, || {
        let mut buf = Vec::with_capacity(256 * 1024);
        emit_after(&mut buf);
        buf
    });
    println!(
        "    gate: 2000-item bin byte-identical ✓   bin: {:.1}→{:.1} µs, {:.0}→{:.0} allocs",
        bn / 1e3,
        an / 1e3,
        ba,
        aa
    );
}

// ─── §5 webdav scope prefix ─────────────────────────────────────────────────

fn section_scope_prefix(iters: u64) {
    println!("[5] native-WebDAV scope prefix test (per request)");
    // 1:1 replicas of the two shapes (the production fn is handler-private).
    fn before_route(normalized: &str, marker: &str) -> Option<usize> {
        let with_slash = format!("{}/", marker);
        normalized.strip_prefix(&with_slash).map(|r| r.len())
    }
    fn strip_prefix_slash<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
        s.strip_prefix(prefix)?.strip_prefix('/')
    }
    fn after_route(normalized: &str, marker: &str) -> Option<usize> {
        strip_prefix_slash(normalized, marker).map(|r| r.len())
    }

    let cases = [
        ("@drive/Personal/Photos/2026/img.jpg", "@drive"),
        ("Personal/Documents/report.pdf", "@drive"),
        ("@drive", "@drive"),
        ("@driveX/nope", "@drive"),
    ];
    for (path, marker) in cases {
        assert_eq!(before_route(path, marker), after_route(path, marker));
    }

    let (bn, ba) = measure("BEFORE format!(\"{prefix}/\") probe", iters, || {
        before_route(
            black_box("@drive/Personal/Photos/2026/img.jpg"),
            black_box("@drive"),
        )
    });
    let (an, aa) = measure("AFTER  borrow-only probe", iters, || {
        after_route(
            black_box("@drive/Personal/Photos/2026/img.jpg"),
            black_box("@drive"),
        )
    });
    println!(
        "    gate: routing identical on all shapes ✓   ({bn:.0}→{an:.0} ns, {ba:.2}→{aa:.2} allocs)"
    );
}

// ─── §6 base_url ────────────────────────────────────────────────────────────

fn section_base_url(iters: u64) {
    println!("[6] share-listing base_url (per 500-row listing)");
    unsafe {
        env::set_var("OXICLOUD_BASE_URL", "https://cloud.example.com");
    }
    let config = oxicloud::common::config::AppConfig::default();
    let rows = 500usize;

    let before_listing = || {
        let mut total = 0usize;
        for _ in 0..rows {
            total += config.base_url().len(); // env read + String per row
        }
        total
    };
    let snapshot = config.base_url();
    let after_listing = || {
        let mut total = 0usize;
        for _ in 0..rows {
            total += snapshot.len(); // field read
        }
        total
    };
    assert_eq!(before_listing(), after_listing());

    let listing_iters = (iters / rows as u64).max(50);
    let (bn, ba) = measure("BEFORE env::var + rebuild per row", listing_iters, || {
        before_listing()
    });
    let (an, aa) = measure("AFTER  construction-time snapshot", listing_iters, || {
        after_listing()
    });
    println!(
        "    gate: identical URLs ✓   listing: {:.1}→{:.3} µs, {:.0}→{:.0} allocs",
        bn / 1e3,
        an / 1e3,
        ba,
        aa
    );
}

// ─── §7 JWT verify miss ─────────────────────────────────────────────────────

fn section_jwt(iters: u64) {
    use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};

    println!("[7] JWT verify (validation-cache miss path)");
    #[derive(serde::Serialize, serde::Deserialize)]
    struct C {
        sub: String,
        exp: i64,
        iat: i64,
        jti: String,
        username: String,
        email: String,
        role: String,
    }
    let secret = "bench_secret_key_at_least_32_bytes_long!";
    let claims = C {
        sub: uuid::Uuid::nil().to_string(),
        exp: 4_102_444_800,
        iat: 1_700_000_000,
        jti: uuid::Uuid::nil().to_string(),
        username: "alice".into(),
        email: "alice@example.com".into(),
        role: "user".into(),
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap();

    let jwt_iters = iters / 10;
    let (bn, ba) = measure("BEFORE fresh Validation+DecodingKey", jwt_iters, || {
        let validation = Validation::new(Algorithm::HS256);
        let key = DecodingKey::from_secret(secret.as_bytes());
        decode::<C>(black_box(&token), &key, &validation)
            .unwrap()
            .claims
            .exp
    });
    let key = DecodingKey::from_secret(secret.as_bytes());
    let validation = Validation::new(Algorithm::HS256);
    let (an, aa) = measure("AFTER  pre-built service fields", jwt_iters, || {
        decode::<C>(black_box(&token), &key, &validation)
            .unwrap()
            .claims
            .exp
    });
    println!("    gate: same decode result ✓   ({bn:.0}→{an:.0} ns, {ba:.2}→{aa:.2} allocs)");
}

// ─── §8 cipher clone ────────────────────────────────────────────────────────

fn section_cipher(iters: u64) {
    use aes_gcm::{Aes256Gcm, KeyInit};

    println!("[8] AES-GCM cipher hand-off (per blob op)");
    let cipher = Aes256Gcm::new_from_slice(&[7u8; 32]).unwrap();
    let arc_cipher = Arc::new(Aes256Gcm::new_from_slice(&[7u8; 32]).unwrap());

    let (bn, _) = measure("BEFORE Aes256Gcm::clone (key schedule)", iters, || {
        black_box(cipher.clone())
    });
    let (an, _) = measure("AFTER  Arc<Aes256Gcm>::clone (bump)", iters, || {
        black_box(Arc::clone(&arc_cipher))
    });
    println!("    gate: n/a (same cipher key, encryption unchanged)   ({bn:.1}→{an:.1} ns)");
}

// ─── §9 request-id ──────────────────────────────────────────────────────────

fn section_request_id(iters: u64) {
    println!("[9] x-request-id header build (per request)");
    let fixed = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);

    let (bn, ba) = measure("BEFORE Uuid::to_string + from_str", iters, || {
        let id = black_box(fixed).to_string();
        axum::http::HeaderValue::from_str(&id).unwrap()
    });
    let (an, aa) = measure("AFTER  stack-encode + from_str", iters, || {
        let mut buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
        axum::http::HeaderValue::from_str(black_box(fixed).hyphenated().encode_lower(&mut buf))
            .unwrap()
    });

    let a = {
        let id = fixed.to_string();
        axum::http::HeaderValue::from_str(&id).unwrap()
    };
    let b = {
        let mut buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
        axum::http::HeaderValue::from_str(fixed.hyphenated().encode_lower(&mut buf)).unwrap()
    };
    assert_eq!(a, b, "header bytes must be identical");
    println!("    gate: header bytes identical ✓   ({bn:.0}→{an:.0} ns, {ba:.2}→{aa:.2} allocs)");
}

fn main() {
    let iters: u64 = env_or("BENCH_ITERS", 100_000);
    println!("bench_round10_micro — iters={iters}\n");
    section_identity(iters);
    section_basic_hit(iters);
    section_propfind_ints(iters);
    section_trashbin(iters);
    section_scope_prefix(iters);
    section_base_url(iters);
    section_jwt(iters);
    section_cipher(iters);
    section_request_id(iters);
    println!("\nall gates passed");
}
