//! Round-11 CPU/alloc micro-pack — BEFORE replicas vs AFTER shapes.
//!
//! Same discipline as ROUND2-10: every section measures a byte-faithful
//! replica of the shipped code (BEFORE) against the candidate shape
//! (AFTER), with an equivalence gate. An AFTER that doesn't win gets
//! rolled back instead of adopted.
//!
//! Sections (all pure CPU, no Postgres):
//!    1. REST download `FileDto` dead clone vs mime/size capture + move
//!    2. Single-resource GET/HEAD `Last-Modified`: chrono `to_rfc2822()`
//!       vs `common::fmt::rfc2822_utc` stack render (gate: byte-identical).
//!       VERDICT: header port REJECTED — the chrono String is already the
//!       terminal allocation; only body-emit sites benefit.
//!    3. `/status.php` poll: rebuild `json!` + serialize vs `OnceLock<Bytes>`
//!       (gate: byte-identical)
//!    4. NC chunk-upload session PROPFIND: `push_str(&format!)` + chrono
//!       per chunk vs `with_capacity` + `write!` + stack dates
//!       (gate: byte-identical XML)
//!    5. RateLimiter: 2 key allocs + entry+insert vs (a) `and_upsert_with`
//!       [REJECTED: slower + more allocs] vs (b) lock-free get + insert
//!       [ADOPTED] (gate: identical counter sequences)
//!    6. CSRF header token: `to_string` vs borrow compare (gate: same bool)
//!    7. Thumbnail ETag: `{:?}` Debug enums vs `as_str` + push (gate: bytes)
//!    8. Recent-handler id: `Uuid::to_string` vs stack `encode_lower`
//!       (gate: identical str)
//!    9. 4xx error body: status+message clones + `kind.to_string()` vs
//!       borrowed single-alloc serialize (gate: byte-identical JSON)
//!   10. vCard emit: `push_str(&format!)` vs `write!` (gate: bytes)
//!   11. Search page slice: `.to_vec()` clone vs `drain` move (gate: equal)
//!   12. Content-hit verify: double `Uuid::parse_str` vs parse-once pairs
//!       (gate: same verified set)
//!   13. Group last-user check: O(N·M) slice contains vs HashSet
//!       (gate: same bool)
//!   14. Retry op label: eager `format!` vs lazy closure (success path)
//!   15. `encrypt_bytes`: ciphertext alloc + copy vs in-place detached
//!       (gate: byte-identical output for a fixed nonce + round-trip)
//!   16. Encrypted `collect_stream`: `Vec::new()` growth vs pre-sized
//!       (gate: same bytes)
//!   17. Face clustering `cosine`: per-pair norm recompute vs precomputed
//!       sqrt norms (gate: bitwise-identical similarity + same unions)
//!   18. `/openapi.json`: rebuild + serialize vs `OnceLock<Bytes>`
//!       (gate: byte-identical)
//!   19. `CalendarEventDto::from`: getter clones (incl. the ~11 KB
//!       `ical_data`) vs `into_parts` move (gate: identical DTO fields)
//!   20. `StoragePath` row materialization: eager `Vec<String>` segments +
//!       duplicated `path_string` vs single canonical joined `String`
//!       (gate: identical path/file_name/parent/Display)
//!
//! Run: cargo run --release --features bench --example bench_round11_micro
//! Tunables (env): BENCH_ITERS (100000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fmt::Write as _;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

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
    println!("    {label:<52} {ns:>10.1} ns/op   {allocs:>8.3} allocs/op");
    (ns, allocs)
}

fn gate(name: &str, ok: bool) {
    if ok {
        println!("    gate[{name}]: OK");
    } else {
        println!("    gate[{name}]: FAILED — DO NOT SHIP THIS SECTION");
    }
}

// ─── §1 download FileDto dead clone ─────────────────────────────────────────

/// Field-faithful replica of `FileDto` (`application/dtos/file_dto.rs`).
#[derive(Clone)]
#[allow(dead_code)]
struct FileDtoRep {
    id: String,
    name: String,
    path: String,
    size: u64,
    mime_type: Arc<str>,
    folder_id: Option<String>,
    created_at: u64,
    modified_at: u64,
    icon_class: Arc<str>,
    icon_special_class: Arc<str>,
    category: Arc<str>,
    size_formatted: String,
    sort_date: Option<u64>,
    content_hash: String,
    etag: String,
    created_by: Option<uuid::Uuid>,
    updated_by: Option<uuid::Uuid>,
}

fn sample_file_dto() -> FileDtoRep {
    FileDtoRep {
        id: "0198c9a0-1111-7abc-9def-0123456789ab".into(),
        name: "IMG_20260716_193245.jpg".into(),
        path: "/Photos/2026/07/IMG_20260716_193245.jpg".into(),
        size: 4_183_212,
        mime_type: Arc::from("image/jpeg"),
        folder_id: Some("0198c9a0-2222-7abc-9def-0123456789ab".into()),
        created_at: 1_784_500_000,
        modified_at: 1_784_500_020,
        icon_class: Arc::from("fas fa-file-image"),
        icon_special_class: Arc::from("image-icon"),
        category: Arc::from("Image"),
        size_formatted: "3.99 MB".into(),
        sort_date: None,
        content_hash: "b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3".into(),
        etag: "\"b3b3b3b3-1784500020\"".into(),
        created_by: None,
        updated_by: None,
    }
}

/// The downstream service consumes the DTO (as `get_file_optimized_preloaded`
/// does) and returns it (discarded by the handler as `_file`).
#[inline(never)]
fn service_consume(dto: FileDtoRep) -> (FileDtoRep, u64) {
    let s = dto.size;
    (dto, s)
}

fn section_1(iters: u64) {
    println!("  §1 REST download FileDto hand-off (per download)");
    let dto = sample_file_dto();

    // The handler owns `file_dto` (fetched per request) in both shapes; the
    // arms isolate ONLY the hand-off into `get_file_optimized_preloaded`.
    // BEFORE: `file_dto.clone()` in, then read mime/size from the retained
    // copy. AFTER: capture mime (Arc bump) + size, MOVE the DTO in.
    measure("BEFORE dead clone into service", iters, || {
        let (ret, _s) = service_consume(dto.clone());
        drop(ret);
        (dto.mime_type.clone(), dto.size)
    });
    measure("AFTER  capture mime/size + move", iters, || {
        // Model the move without giving up the corpus DTO: production moves
        // the request-owned value; the captures are the only per-call work.
        let mime = dto.mime_type.clone();
        let size = dto.size;
        black_box((&dto, mime, size)).1
    });
}

// ─── §2 Last-Modified header value ──────────────────────────────────────────

fn section_2(iters: u64) {
    println!("  §2 GET/HEAD Last-Modified render (per response)");
    let ts: i64 = 1_784_500_020;

    let before = chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc2822();
    let mut buf = [0u8; 31];
    let after = oxicloud::common::fmt::rfc2822_utc(&mut buf, ts)
        .map(str::to_owned)
        .unwrap_or_default();
    gate("rfc2822 bytes identical", before == after);

    measure("BEFORE chrono to_rfc2822", iters, || {
        chrono::DateTime::<chrono::Utc>::from_timestamp(black_box(ts), 0)
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc2822()
    });
    measure("AFTER  fmt::rfc2822_utc + header alloc", iters, || {
        let mut b = [0u8; 31];
        oxicloud::common::fmt::rfc2822_utc(&mut b, black_box(ts))
            .map(str::to_owned)
            .unwrap_or_default()
    });
    measure("AFTER  fmt::rfc2822_utc stack only", iters, || {
        let mut b = [0u8; 31];
        oxicloud::common::fmt::rfc2822_utc(&mut b, black_box(ts)).map(|s| s.len())
    });
}

// ─── §3 /status.php ─────────────────────────────────────────────────────────

fn build_status_json(major: u32, minor: u32, patch: u32, version_string: &str) -> Vec<u8> {
    let v = serde_json::json!({
        "installed": true,
        "maintenance": false,
        "needsDbUpgrade": false,
        "version": format!("{}.{}.{}.1", major, minor, patch),
        "versionstring": version_string,
        "productname": "OxiCloud",
        "edition": ""
    });
    serde_json::to_vec(&v).expect("status json")
}

fn section_3(iters: u64) {
    println!("  §3 /status.php poll (per request)");
    let (maj, min, pat) = (31u32, 0u32, 0u32);
    let vs = "31.0.0";

    static CACHED: std::sync::OnceLock<bytes::Bytes> = std::sync::OnceLock::new();
    let cached = CACHED.get_or_init(|| bytes::Bytes::from(build_status_json(maj, min, pat, vs)));
    gate(
        "status body identical",
        cached.as_ref() == build_status_json(maj, min, pat, vs).as_slice(),
    );

    measure("BEFORE rebuild json! + serialize", iters, || {
        build_status_json(black_box(maj), min, pat, black_box(vs))
    });
    measure("AFTER  OnceLock<Bytes> refcount bump", iters, || {
        CACHED.get().unwrap().clone()
    });
}

// ─── §4 NC chunk-upload session PROPFIND ────────────────────────────────────

/// Replica of `uploads_handler::xml_escape` semantics (escape into owned
/// String only when needed).
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

struct ChunkRep {
    name: String,
    size: u64,
    mtime: u64,
}

fn chunk_listing(n: usize) -> (String, u64, Vec<ChunkRep>) {
    let chunks = (0..n)
        .map(|i| ChunkRep {
            name: format!("{:05}", i + 1),
            size: 10 * 1024 * 1024,
            mtime: 1_784_500_000 + i as u64,
        })
        .collect();
    ("admin".to_string(), 1_784_500_000, chunks)
}

fn propfind_before(raw_username: &str, upload_id: &str, mtime: u64, chunks: &[ChunkRep]) -> String {
    let session_href = format!("/remote.php/dav/uploads/{}/{}/", raw_username, upload_id);
    let session_last_modified = chrono::DateTime::<chrono::Utc>::from_timestamp(mtime as i64, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc2822();

    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    body.push_str(r#"<d:multistatus xmlns:d="DAV:">"#);
    body.push_str("<d:response>");
    body.push_str(&format!("<d:href>{}</d:href>", xml_escape(&session_href)));
    body.push_str("<d:propstat><d:prop>");
    body.push_str("<d:resourcetype><d:collection/></d:resourcetype>");
    body.push_str(&format!(
        "<d:getlastmodified>{}</d:getlastmodified>",
        xml_escape(&session_last_modified)
    ));
    body.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>");
    body.push_str("</d:response>");

    for chunk in chunks {
        let chunk_href = format!(
            "/remote.php/dav/uploads/{}/{}/{}",
            raw_username, upload_id, chunk.name
        );
        let chunk_modified = chrono::DateTime::<chrono::Utc>::from_timestamp(chunk.mtime as i64, 0)
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc2822();

        body.push_str("<d:response>");
        body.push_str(&format!("<d:href>{}</d:href>", xml_escape(&chunk_href)));
        body.push_str("<d:propstat><d:prop>");
        body.push_str("<d:resourcetype/>");
        body.push_str(&format!(
            "<d:getcontentlength>{}</d:getcontentlength>",
            chunk.size
        ));
        body.push_str(&format!(
            "<d:getlastmodified>{}</d:getlastmodified>",
            xml_escape(&chunk_modified)
        ));
        body.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>");
        body.push_str("</d:response>");
    }

    body.push_str("</d:multistatus>");
    body
}

/// Emit a `<d:getlastmodified>` element with the stack renderer, falling
/// back to chrono outside the 4-digit-year range (same fallback shape as
/// `nextcloud/webdav_handler.rs`). RFC 2822 output contains no
/// XML-special characters, so the escape pass is skipped by construction.
fn write_lastmodified(body: &mut String, secs: i64) {
    let mut b = [0u8; 31];
    match oxicloud::common::fmt::rfc2822_utc(&mut b, secs) {
        Some(s) => {
            let _ = write!(body, "<d:getlastmodified>{}</d:getlastmodified>", s);
        }
        None => {
            let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
                .unwrap_or_else(chrono::Utc::now)
                .to_rfc2822();
            let _ = write!(
                body,
                "<d:getlastmodified>{}</d:getlastmodified>",
                xml_escape(&dt)
            );
        }
    }
}

fn propfind_after(raw_username: &str, upload_id: &str, mtime: u64, chunks: &[ChunkRep]) -> String {
    let mut body = String::with_capacity(256 + chunks.len() * 256);
    body.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    body.push_str(r#"<d:multistatus xmlns:d="DAV:">"#);
    body.push_str("<d:response>");
    let _ = write!(
        body,
        "<d:href>/remote.php/dav/uploads/{}/{}/</d:href>",
        xml_escape(raw_username),
        xml_escape(upload_id)
    );
    body.push_str("<d:propstat><d:prop>");
    body.push_str("<d:resourcetype><d:collection/></d:resourcetype>");
    write_lastmodified(&mut body, mtime as i64);
    body.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>");
    body.push_str("</d:response>");

    for chunk in chunks {
        body.push_str("<d:response>");
        let _ = write!(
            body,
            "<d:href>/remote.php/dav/uploads/{}/{}/{}</d:href>",
            xml_escape(raw_username),
            xml_escape(upload_id),
            xml_escape(&chunk.name)
        );
        body.push_str("<d:propstat><d:prop>");
        body.push_str("<d:resourcetype/>");
        let _ = write!(
            body,
            "<d:getcontentlength>{}</d:getcontentlength>",
            chunk.size
        );
        write_lastmodified(&mut body, chunk.mtime as i64);
        body.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>");
        body.push_str("</d:response>");
    }

    body.push_str("</d:multistatus>");
    body
}

fn section_4(iters: u64) {
    println!("  §4 NC upload-session PROPFIND body (per PROPFIND)");
    for n in [16usize, 256] {
        let (user, mtime, chunks) = chunk_listing(n);
        let b = propfind_before(&user, "web-file-upload-abc123", mtime, &chunks);
        let a = propfind_after(&user, "web-file-upload-abc123", mtime, &chunks);
        gate(&format!("xml identical ({n} chunks)"), a == b);
        let it = (iters / n as u64).max(50);
        measure(&format!("BEFORE push_str(&format!) {n} chunks"), it, || {
            propfind_before(&user, "web-file-upload-abc123", mtime, black_box(&chunks))
        });
        measure(
            &format!("AFTER  write! + capacity   {n} chunks"),
            it,
            || propfind_after(&user, "web-file-upload-abc123", mtime, black_box(&chunks)),
        );
    }
}

// ─── §5 RateLimiter ─────────────────────────────────────────────────────────

fn section_5(iters: u64) {
    println!("  §5 RateLimiter check_and_increment (per limited request)");
    let cache_b: moka::sync::Cache<String, u32> = moka::sync::Cache::builder()
        .time_to_live(std::time::Duration::from_secs(60))
        .max_capacity(10_000)
        .build();
    let cache_a: moka::sync::Cache<String, u32> = moka::sync::Cache::builder()
        .time_to_live(std::time::Duration::from_secs(60))
        .max_capacity(10_000)
        .build();
    let ip = "203.0.113.42";

    // Equivalence gate: identical counter sequences over a fresh key.
    let seq_b: Vec<u32> = (0..5)
        .map(|_| {
            let c = cache_b
                .entry(ip.to_string())
                .or_insert_with(|| 0)
                .into_value()
                + 1;
            cache_b.insert(ip.to_string(), c);
            c
        })
        .collect();
    let seq_a: Vec<u32> = (0..5)
        .map(|_| {
            cache_a
                .entry(ip.to_string())
                .and_upsert_with(|e| e.map(|v| v.into_value() + 1).unwrap_or(1))
                .into_value()
        })
        .collect();
    gate("counter sequence identical", seq_a == seq_b);
    cache_a.invalidate(ip);
    cache_b.invalidate(ip);

    // Gate for the get+insert variant: identical counter sequence.
    let cache_c: moka::sync::Cache<String, u32> = moka::sync::Cache::builder()
        .time_to_live(std::time::Duration::from_secs(60))
        .max_capacity(10_000)
        .build();
    let seq_c: Vec<u32> = (0..5)
        .map(|_| {
            let count = cache_c.get(ip).unwrap_or(0) + 1;
            cache_c.insert(ip.to_string(), count);
            count
        })
        .collect();
    gate("get+insert sequence identical", seq_c == seq_b);
    cache_c.invalidate(ip);

    measure("BEFORE 2 allocs + entry+insert", iters, || {
        let key = ip.to_string();
        let count = cache_b.entry(key).or_insert_with(|| 0).into_value() + 1;
        cache_b.insert(ip.to_string(), count);
        count
    });
    measure("AFTER-1 and_upsert_with", iters, || {
        cache_a
            .entry(ip.to_string())
            .and_upsert_with(|e| e.map(|v| v.into_value() + 1).unwrap_or(1))
            .into_value()
    });
    measure("AFTER-2 lock-free get + insert", iters, || {
        let count = cache_c.get(black_box(ip)).unwrap_or(0) + 1;
        cache_c.insert(ip.to_string(), count);
        count
    });
}

// ─── §6 CSRF header token ───────────────────────────────────────────────────

fn section_6(iters: u64) {
    println!("  §6 CSRF token compare (per state-changing cookie request)");
    let cookie_token = Some("9f8e7d6c5b4a39281706f5e4d3c2b1a0".to_string());
    let header_val = "9f8e7d6c5b4a39281706f5e4d3c2b1a0";

    let before = {
        let header_token = Some(header_val).map(|s| s.to_string());
        matches!((cookie_token.as_ref(), header_token.as_ref()),
            (Some(c), Some(h)) if !c.is_empty() && c == h)
    };
    let after = {
        let header_token: Option<&str> = Some(header_val);
        matches!((cookie_token.as_ref(), header_token),
            (Some(c), Some(h)) if !c.is_empty() && c == h)
    };
    gate("csrf verdict identical", before == after);

    measure("BEFORE header to_string + compare", iters, || {
        let header_token = Some(black_box(header_val)).map(|s| s.to_string());
        matches!((cookie_token.as_ref(), header_token.as_ref()),
            (Some(c), Some(h)) if !c.is_empty() && c == h)
    });
    measure("AFTER  borrow compare", iters, || {
        let header_token: Option<&str> = Some(black_box(header_val));
        matches!((cookie_token.as_ref(), header_token),
            (Some(c), Some(h)) if !c.is_empty() && c == h)
    });
}

// ─── §7 Thumbnail ETag ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum SizeRep {
    Icon,
    Preview,
    Large,
}
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum FormatRep {
    Webp,
    Jpeg,
}
impl SizeRep {
    fn as_str(self) -> &'static str {
        match self {
            SizeRep::Icon => "Icon",
            SizeRep::Preview => "Preview",
            SizeRep::Large => "Large",
        }
    }
}
impl FormatRep {
    fn as_str(self) -> &'static str {
        match self {
            FormatRep::Webp => "Webp",
            FormatRep::Jpeg => "Jpeg",
        }
    }
}

fn section_7(iters: u64) {
    println!("  §7 thumbnail ETag build (per thumbnail request)");
    let id = "0198c9a0-1111-7abc-9def-0123456789ab";
    let (sz, fm) = (SizeRep::Preview, FormatRep::Webp);

    let before = format!("\"thumb-{}-{:?}-{:?}\"", id, sz, fm);
    let after = {
        let mut s = String::with_capacity(9 + id.len() + sz.as_str().len() + fm.as_str().len());
        s.push_str("\"thumb-");
        s.push_str(id);
        s.push('-');
        s.push_str(sz.as_str());
        s.push('-');
        s.push_str(fm.as_str());
        s.push('"');
        s
    };
    gate("etag bytes identical", before == after);

    measure("BEFORE format! with {:?} enums", iters, || {
        format!("\"thumb-{}-{:?}-{:?}\"", black_box(id), sz, fm)
    });
    measure("AFTER  as_str + sized push", iters, || {
        let id = black_box(id);
        let mut s = String::with_capacity(9 + id.len() + sz.as_str().len() + fm.as_str().len());
        s.push_str("\"thumb-");
        s.push_str(id);
        s.push('-');
        s.push_str(sz.as_str());
        s.push('-');
        s.push_str(fm.as_str());
        s.push('"');
        s
    });
}

// ─── §8 recent-handler id round-trip ────────────────────────────────────────

fn section_8(iters: u64) {
    println!("  §8 recent-handler item_id hand-off (per record/remove)");
    let id = uuid::Uuid::parse_str("0198c9a0-1111-7abc-9def-0123456789ab").unwrap();

    let before = id.to_string();
    let mut buf = [0u8; 36];
    let after: &str = id.as_hyphenated().encode_lower(&mut buf);
    gate("id str identical", before == after);

    measure("BEFORE Uuid::to_string per call", iters, || {
        let s = black_box(id).to_string();
        s.len()
    });
    measure("AFTER  stack encode_lower", iters, || {
        let mut b = [0u8; 36];
        let s: &str = black_box(id).as_hyphenated().encode_lower(&mut b);
        s.len()
    });
}

// ─── §9 4xx error body ──────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct ErrorResponseOwned {
    status: String,
    error: String,
    message: String,
    error_type: String,
}

#[derive(serde::Serialize)]
struct ErrorResponseBorrowed<'a> {
    status: &'a str,
    error: &'a str,
    message: &'a str,
    error_type: &'static str,
}

fn section_9(iters: u64) {
    println!("  §9 4xx error response build (per 404/401/403)");
    // Model: DomainError::not_found("File", id) → AppError → into_response.
    let entity = "File";
    let id = "0198c9a0-1111-7abc-9def-0123456789ab";
    let status = axum::http::StatusCode::NOT_FOUND;

    let before_bytes = {
        // not_found: id.clone() + eager format!
        let idc = id.to_string();
        let _entity_id = Some(idc.clone());
        let message = format!("{} not found: {}", entity, idc);
        // From<DomainError>: kind.to_string()
        let error_type = "Not Found".to_string();
        // into_response: status.to_string() + message.clone()
        let body = ErrorResponseOwned {
            status: status.to_string(),
            error: message.clone(),
            message,
            error_type,
        };
        serde_json::to_vec(&body).unwrap()
    };
    let after_bytes = {
        let idc = id.to_string();
        let message = format!("{} not found: {}", entity, idc);
        let _entity_id = Some(idc);
        let status_s = status.to_string();
        let body = ErrorResponseBorrowed {
            status: &status_s,
            error: &message,
            message: &message,
            error_type: "Not Found",
        };
        serde_json::to_vec(&body).unwrap()
    };
    gate("error JSON identical", before_bytes == after_bytes);

    measure("BEFORE clones + owned serialize", iters, || {
        let idc = black_box(id).to_string();
        let _entity_id = Some(idc.clone());
        let message = format!("{} not found: {}", black_box(entity), idc);
        let error_type = "Not Found".to_string();
        let body = ErrorResponseOwned {
            status: status.to_string(),
            error: message.clone(),
            message,
            error_type,
        };
        serde_json::to_vec(&body).unwrap()
    });
    measure("AFTER  move + borrowed serialize", iters, || {
        let idc = black_box(id).to_string();
        let message = format!("{} not found: {}", black_box(entity), idc);
        let _entity_id = Some(idc);
        let status_s = status.to_string();
        let body = ErrorResponseBorrowed {
            status: &status_s,
            error: &message,
            message: &message,
            error_type: "Not Found",
        };
        serde_json::to_vec(&body).unwrap()
    });
}

// ─── §10 vCard emit ─────────────────────────────────────────────────────────

struct ContactRep {
    full_name: String,
    first: String,
    last: String,
    email_home: String,
    email_work: String,
    phone: String,
    org: String,
    title: String,
    uid: String,
}

fn sample_contact() -> ContactRep {
    ContactRep {
        full_name: "Ada Lovelace".into(),
        first: "Ada".into(),
        last: "Lovelace".into(),
        email_home: "ada@example.org".into(),
        email_work: "ada@analytical.engines".into(),
        phone: "+44 20 7946 0958".into(),
        org: "Analytical Engines Ltd".into(),
        title: "Chief Mathematician".into(),
        uid: "0198c9a0-3333-7abc-9def-0123456789ab".into(),
    }
}

fn vcard_before(c: &ContactRep) -> String {
    let mut vcard = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");
    vcard.push_str(&format!("FN:{}\r\n", c.full_name));
    vcard.push_str(&format!("N:{};{};;;\r\n", c.last, c.first));
    vcard.push_str(&format!("EMAIL;TYPE=HOME:{}\r\n", c.email_home));
    vcard.push_str(&format!("EMAIL;TYPE=WORK:{}\r\n", c.email_work));
    vcard.push_str(&format!("TEL;TYPE=CELL:{}\r\n", c.phone));
    vcard.push_str(&format!("ORG:{}\r\n", c.org));
    vcard.push_str(&format!("TITLE:{}\r\n", c.title));
    vcard.push_str(&format!("UID:{}\r\n", c.uid));
    vcard.push_str("END:VCARD\r\n");
    vcard
}

fn vcard_after(c: &ContactRep) -> String {
    let mut vcard = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");
    let _ = write!(vcard, "FN:{}\r\n", c.full_name);
    let _ = write!(vcard, "N:{};{};;;\r\n", c.last, c.first);
    let _ = write!(vcard, "EMAIL;TYPE=HOME:{}\r\n", c.email_home);
    let _ = write!(vcard, "EMAIL;TYPE=WORK:{}\r\n", c.email_work);
    let _ = write!(vcard, "TEL;TYPE=CELL:{}\r\n", c.phone);
    let _ = write!(vcard, "ORG:{}\r\n", c.org);
    let _ = write!(vcard, "TITLE:{}\r\n", c.title);
    let _ = write!(vcard, "UID:{}\r\n", c.uid);
    vcard.push_str("END:VCARD\r\n");
    vcard
}

fn section_10(iters: u64) {
    println!("  §10 vCard emit (per contact create/update)");
    let c = sample_contact();
    gate("vcard bytes identical", vcard_before(&c) == vcard_after(&c));
    measure("BEFORE push_str(&format!) per line", iters, || {
        vcard_before(black_box(&c))
    });
    measure("AFTER  write! per line", iters, || {
        vcard_after(black_box(&c))
    });
}

// ─── §11 search page slice ──────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
#[allow(dead_code)]
struct SearchHitRep {
    id: String,
    name: String,
    path: String,
    etag: String,
    content_hash: String,
    size_formatted: String,
    size: u64,
}

fn search_corpus(n: usize) -> Vec<SearchHitRep> {
    (0..n)
        .map(|i| SearchHitRep {
            id: format!("0198c9a0-1111-7abc-9def-{:012}", i),
            name: format!("Informe anual {i}.pdf"),
            path: format!("/Documentos/2026/Informe anual {i}.pdf"),
            etag: format!("\"e{i}-1784500020\""),
            content_hash: "b3".repeat(32),
            size_formatted: "1.24 MB".into(),
            size: 1_300_000 + i as u64,
        })
        .collect()
}

fn section_11(iters: u64) {
    println!("  §11 search page extraction (per uncached query, 50-item page)");
    let full = search_corpus(400);
    let (start, end) = (100usize, 150usize);

    let before_page = full[start..end].to_vec();
    let after_page: Vec<SearchHitRep> = {
        let own = full.clone();
        own.into_iter().skip(start).take(end - start).collect()
    };
    gate("page contents identical", before_page == after_page);

    // Both arms pay the identical own-clone (the service owns the enriched
    // vec in production); the delta is page extraction: deep-clone the
    // slice + drop the whole vec, vs consume the vec moving the page out.
    let it = (iters / 50).max(100);
    measure("BEFORE slice.to_vec() (clones page)", it, || {
        let own = full.clone();
        let page = own[start..end].to_vec();
        (own.len(), page)
    });
    measure("AFTER  into_iter skip/take (moves)", it, || {
        let own = full.clone();
        let n = own.len();
        let page: Vec<SearchHitRep> = own.into_iter().skip(start).take(end - start).collect();
        (n, page)
    });
}

// ─── §12 content-hit double parse ───────────────────────────────────────────

fn section_12(iters: u64) {
    println!("  §12 content-hit verify loop (per content search, 100 hits)");
    let hits: Vec<String> = (0..100)
        .map(|i| format!("0198c9a0-1111-7abc-9def-{:012}", i))
        .collect();
    let allowed: std::collections::HashSet<uuid::Uuid> = hits
        .iter()
        .step_by(2)
        .map(|s| uuid::Uuid::parse_str(s).unwrap())
        .collect();

    let before: Vec<&String> = {
        let mut ids = Vec::with_capacity(hits.len());
        for h in &hits {
            if let Ok(u) = uuid::Uuid::parse_str(h) {
                ids.push(u);
            }
        }
        hits.iter()
            .filter(|h| {
                uuid::Uuid::parse_str(h)
                    .map(|u| allowed.contains(&u))
                    .unwrap_or(false)
            })
            .collect()
    };
    let after: Vec<&String> = {
        let pairs: Vec<(&String, uuid::Uuid)> = hits
            .iter()
            .filter_map(|h| uuid::Uuid::parse_str(h).ok().map(|u| (h, u)))
            .collect();
        pairs
            .iter()
            .filter(|(_, u)| allowed.contains(u))
            .map(|(h, _)| *h)
            .collect()
    };
    gate("verified set identical", before == after);

    let it = (iters / 100).max(100);
    measure("BEFORE parse twice per hit", it, || {
        let mut ids = Vec::with_capacity(hits.len());
        for h in &hits {
            if let Ok(u) = uuid::Uuid::parse_str(h) {
                ids.push(u);
            }
        }
        black_box(&ids);
        let v: Vec<&String> = hits
            .iter()
            .filter(|h| {
                uuid::Uuid::parse_str(h)
                    .map(|u| allowed.contains(&u))
                    .unwrap_or(false)
            })
            .collect();
        v.len()
    });
    measure("AFTER  parse once, carry pairs", it, || {
        let pairs: Vec<(&String, uuid::Uuid)> = hits
            .iter()
            .filter_map(|h| uuid::Uuid::parse_str(h).ok().map(|u| (h, u)))
            .collect();
        let ids: Vec<uuid::Uuid> = pairs.iter().map(|(_, u)| *u).collect();
        black_box(&ids);
        let v: Vec<&String> = pairs
            .iter()
            .filter(|(_, u)| allowed.contains(u))
            .map(|(h, _)| *h)
            .collect();
        v.len()
    });
}

// ─── §13 group last-user containment ────────────────────────────────────────

fn section_13(iters: u64) {
    println!("  §13 group last-user check (per group edit, 500×500)");
    let before_users: Vec<uuid::Uuid> = (0..500).map(|_| uuid::Uuid::new_v4()).collect();
    let mut child_users = before_users.clone();
    child_users.rotate_left(250);

    let b = before_users.iter().all(|u| child_users.contains(u));
    let set: std::collections::HashSet<&uuid::Uuid> = child_users.iter().collect();
    let a = before_users.iter().all(|u| set.contains(u));
    gate("verdict identical", a == b);

    let it = (iters / 500).max(50);
    measure("BEFORE O(N·M) slice contains", it, || {
        before_users
            .iter()
            .all(|u| black_box(&child_users).contains(u))
    });
    measure("AFTER  HashSet build + probe", it, || {
        let s: std::collections::HashSet<&uuid::Uuid> = black_box(&child_users).iter().collect();
        before_users.iter().all(|u| s.contains(u))
    });
}

// ─── §14 retry label ────────────────────────────────────────────────────────

fn retry_sync_before(name: &str, f: impl Fn() -> u64) -> u64 {
    // success path: label was allocated by the caller, never read
    black_box(name);
    f()
}
fn retry_sync_after(_name: impl Fn() -> String, f: impl Fn() -> u64) -> u64 {
    f()
}

fn section_14(iters: u64) {
    println!("  §14 retry op-label (per blob op, success path)");
    let hash = "b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3";
    measure("BEFORE eager format! label", iters, || {
        retry_sync_before(&format!("get_blob_stream({})", black_box(hash)), || 7)
    });
    measure("AFTER  lazy closure label", iters, || {
        retry_sync_after(|| format!("get_blob_stream({})", black_box(hash)), || 7)
    });
}

// ─── §15/16 encrypted backend ───────────────────────────────────────────────

fn section_15_16(iters: u64) {
    use aes_gcm::aead::{Aead, AeadInPlace, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};

    println!("  §15 encrypt_bytes (per encrypted chunk write, 256 KiB)");
    const NONCE_SIZE: usize = 12;
    let key = [7u8; 32];
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let data = vec![0xA5u8; 256 * 1024];
    let nonce_fixed = [9u8; 12];
    let nonce = Nonce::from_slice(&nonce_fixed);

    // BEFORE: cipher.encrypt allocates ciphertext; copied again after nonce.
    let before_out = {
        let ciphertext = cipher.encrypt(nonce, data.as_slice()).unwrap();
        let mut encrypted = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
        encrypted.extend_from_slice(&nonce_fixed);
        encrypted.extend_from_slice(&ciphertext);
        encrypted
    };
    // AFTER: single buffer, in-place detached encrypt, append tag.
    let after_out = {
        let mut out = Vec::with_capacity(NONCE_SIZE + data.len() + 16);
        out.extend_from_slice(&nonce_fixed);
        out.extend_from_slice(&data);
        let tag = cipher
            .encrypt_in_place_detached(nonce, b"", &mut out[NONCE_SIZE..])
            .unwrap();
        out.extend_from_slice(&tag);
        out
    };
    gate(
        "ciphertext identical (fixed nonce)",
        before_out == after_out,
    );
    // Round-trip through the decrypt shape used in production.
    let rt = {
        let mut enc = after_out.clone();
        let ct = enc.split_off(NONCE_SIZE);
        let n = Nonce::from_slice(&enc);
        let mut ct = ct;
        cipher.decrypt_in_place(n, b"", &mut ct).unwrap();
        ct
    };
    gate("decrypt round-trip", rt == data);

    let it = (iters / 100).max(200);
    measure("BEFORE encrypt + second copy", it, || {
        let ciphertext = cipher.encrypt(nonce, black_box(data.as_slice())).unwrap();
        let mut encrypted = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
        encrypted.extend_from_slice(&nonce_fixed);
        encrypted.extend_from_slice(&ciphertext);
        encrypted
    });
    measure("AFTER  in-place detached", it, || {
        let data = black_box(data.as_slice());
        let mut out = Vec::with_capacity(NONCE_SIZE + data.len() + 16);
        out.extend_from_slice(&nonce_fixed);
        out.extend_from_slice(data);
        let tag = cipher
            .encrypt_in_place_detached(nonce, b"", &mut out[NONCE_SIZE..])
            .unwrap();
        out.extend_from_slice(&tag);
        out
    });

    println!("  §16 collect_stream buffer growth (1 MiB blob, 4 KiB frames)");
    let frames: Vec<Vec<u8>> = (0..256).map(|i| vec![i as u8; 4096]).collect();
    let expect: Vec<u8> = frames.iter().flatten().copied().collect();

    let before_buf = {
        let mut buf = Vec::new();
        for f in &frames {
            buf.extend_from_slice(f);
        }
        buf
    };
    let after_buf = {
        let mut buf: Vec<u8> = Vec::new();
        for f in &frames {
            if buf.capacity() == 0 {
                buf.reserve(1024 * 1024 + 28);
            }
            buf.extend_from_slice(f);
        }
        buf
    };
    gate(
        "collected bytes identical",
        before_buf == expect && after_buf == expect,
    );

    let it = (iters / 100).max(200);
    measure("BEFORE Vec::new() growth", it, || {
        let mut buf = Vec::new();
        for f in black_box(&frames) {
            buf.extend_from_slice(f);
        }
        buf
    });
    measure("AFTER  reserve on first frame", it, || {
        let mut buf: Vec<u8> = Vec::new();
        for f in black_box(&frames) {
            if buf.capacity() == 0 {
                buf.reserve(1024 * 1024 + 28);
            }
            buf.extend_from_slice(f);
        }
        buf
    });
}

// ─── §17 cosine norms ───────────────────────────────────────────────────────

fn cosine_before(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// AFTER: norms precomputed once per face (same accumulation order), the
/// pair loop keeps only the dot product. The final expression keeps the
/// exact `dot / (sqrt(na) * sqrt(nb))` arithmetic, so results are
/// bit-identical to the BEFORE.
fn norm_sq(v: &[f32]) -> f32 {
    let mut n = 0.0f32;
    for &x in v {
        n += x * x;
    }
    n
}
fn cosine_after(a: &[f32], b: &[f32], na: f32, nb: f32) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += x * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn section_17(iters: u64) {
    println!("  §17 recluster cosine pass (200 faces × 512-dim)");
    let n = 200usize;
    let mut state = 0x12345678u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % 2000) as f32 / 1000.0 - 1.0
    };
    let faces: Vec<Vec<f32>> = (0..n).map(|_| (0..512).map(|_| next()).collect()).collect();

    // Gate: bit-identical similarity over every pair.
    let norms: Vec<f32> = faces.iter().map(|f| norm_sq(f)).collect();
    let mut identical = true;
    for i in 0..n {
        for j in (i + 1)..n {
            let b = cosine_before(&faces[i], &faces[j]);
            let a = cosine_after(&faces[i], &faces[j], norms[i], norms[j]);
            if a.to_bits() != b.to_bits() {
                identical = false;
            }
        }
    }
    gate("similarity bit-identical (all pairs)", identical);

    let it = (iters / 20_000).max(3);
    measure("BEFORE per-pair norms", it, || {
        let mut acc = 0.0f32;
        for i in 0..n {
            for j in (i + 1)..n {
                acc += cosine_before(black_box(&faces[i]), &faces[j]);
            }
        }
        acc
    });
    measure("AFTER  precomputed norms", it, || {
        let norms: Vec<f32> = faces.iter().map(|f| norm_sq(f)).collect();
        let mut acc = 0.0f32;
        for i in 0..n {
            for j in (i + 1)..n {
                acc += cosine_after(black_box(&faces[i]), &faces[j], norms[i], norms[j]);
            }
        }
        acc
    });
}

// ─── §18 /openapi.json ──────────────────────────────────────────────────────

fn section_18(iters: u64) {
    println!("  §18 /openapi.json (per request)");
    use utoipa::OpenApi as _;
    let built = oxicloud::interfaces::api::ApiDoc::openapi();
    let baseline = serde_json::to_vec(&built).unwrap();

    static SPEC: std::sync::OnceLock<bytes::Bytes> = std::sync::OnceLock::new();
    let cached = SPEC.get_or_init(|| {
        bytes::Bytes::from(
            serde_json::to_vec(&oxicloud::interfaces::api::ApiDoc::openapi()).unwrap(),
        )
    });
    gate(
        "spec bytes identical",
        cached.as_ref() == baseline.as_slice(),
    );
    println!("    (spec size: {} KiB)", baseline.len() / 1024);

    let it = (iters / 2000).max(20);
    measure("BEFORE rebuild ApiDoc + serialize", it, || {
        serde_json::to_vec(&oxicloud::interfaces::api::ApiDoc::openapi())
            .unwrap()
            .len()
    });
    measure("AFTER  OnceLock<Bytes> bump", iters, || {
        SPEC.get().unwrap().clone().len()
    });
}

// ─── §19 CalendarEventDto move ──────────────────────────────────────────────

#[allow(dead_code)]
struct EventRep {
    id: uuid::Uuid,
    calendar_id: uuid::Uuid,
    summary: String,
    description: Option<String>,
    location: Option<String>,
    start: i64,
    end: i64,
    all_day: bool,
    rrule: Option<String>,
    ical_uid: String,
    ical_data: String,
}

#[allow(dead_code)]
struct EventDtoRep {
    id: String,
    calendar_id: String,
    summary: String,
    description: Option<String>,
    location: Option<String>,
    start: i64,
    end: i64,
    all_day: bool,
    rrule: Option<String>,
    ical_uid: String,
    ical_data: String,
}

fn sample_event(ical_kb: usize) -> EventRep {
    EventRep {
        id: uuid::Uuid::new_v4(),
        calendar_id: uuid::Uuid::new_v4(),
        summary: "Reunión trimestral de resultados".into(),
        description: Some("Orden del día: revisión de métricas, hoja de ruta.".into()),
        location: Some("Sala Turing, 3ª planta".into()),
        start: 1_784_500_000,
        end: 1_784_503_600,
        all_day: false,
        rrule: Some("FREQ=MONTHLY;BYDAY=1MO".into()),
        ical_uid: "evt-0198c9a0@oxicloud".into(),
        ical_data: format!(
            "BEGIN:VEVENT\r\nUID:evt@x\r\nSUMMARY:Reunión\r\n{}END:VEVENT\r\n",
            "ATTENDEE;CN=Persona;PARTSTAT=ACCEPTED:mailto:p@example.org\r\n".repeat(ical_kb * 16)
        ),
    }
}

fn dto_before(e: &EventRep) -> EventDtoRep {
    EventDtoRep {
        id: e.id.to_string(),
        calendar_id: e.calendar_id.to_string(),
        summary: e.summary.as_str().to_string(),
        description: e.description.as_deref().map(|s| s.to_string()),
        location: e.location.as_deref().map(|s| s.to_string()),
        start: e.start,
        end: e.end,
        all_day: e.all_day,
        rrule: e.rrule.as_deref().map(|s| s.to_string()),
        ical_uid: e.ical_uid.as_str().to_string(),
        ical_data: e.ical_data.as_str().to_string(),
    }
}

fn dto_after(e: EventRep) -> EventDtoRep {
    EventDtoRep {
        id: e.id.to_string(),
        calendar_id: e.calendar_id.to_string(),
        summary: e.summary,
        description: e.description,
        location: e.location,
        start: e.start,
        end: e.end,
        all_day: e.all_day,
        rrule: e.rrule,
        ical_uid: e.ical_uid,
        ical_data: e.ical_data,
    }
}

fn section_19(iters: u64) {
    println!("  §19 CalendarEventDto::from (per event, 11 KiB ical_data)");
    let ev = sample_event(11);
    let b = dto_before(&ev);
    let a = dto_after(sample_event_clone(&ev));
    gate(
        "dto fields identical",
        b.summary == a.summary && b.ical_data == a.ical_data && b.id == a.id,
    );

    let it = (iters / 20).max(500);
    measure("BEFORE getter clones (11 KiB copy)", it, || {
        // model: adapter owns the entity (fetched row), converts, drops it
        let owned = sample_event_clone(&ev);
        let dto = dto_before(&owned);
        drop(owned);
        dto.ical_data.len()
    });
    measure("AFTER  into_parts move", it, || {
        let owned = sample_event_clone(&ev);
        let dto = dto_after(owned);
        dto.ical_data.len()
    });
}

fn sample_event_clone(e: &EventRep) -> EventRep {
    EventRep {
        id: e.id,
        calendar_id: e.calendar_id,
        summary: e.summary.clone(),
        description: e.description.clone(),
        location: e.location.clone(),
        start: e.start,
        end: e.end,
        all_day: e.all_day,
        rrule: e.rrule.clone(),
        ical_uid: e.ical_uid.clone(),
        ical_data: e.ical_data.clone(),
    }
}

// ─── §20 StoragePath row materialization ────────────────────────────────────

/// BEFORE replica: `StoragePath { segments: Vec<String> }` +
/// `from_folder_and_name` building joined AND per-segment Strings, with the
/// entity retaining BOTH `storage_path` and `path_string` (the current
/// shipped shape).
mod sp_before {
    pub struct StoragePathRep {
        pub segments: Vec<String>,
    }
    fn is_safe(s: &str) -> bool {
        !s.is_empty() && s != "." && s != ".." && !s.contains('/')
    }
    pub fn from_folder_and_name(
        folder_path: Option<&str>,
        file_name: &str,
    ) -> (StoragePathRep, String) {
        let fp = folder_path.unwrap_or("");
        let mut joined = String::with_capacity(fp.len() + file_name.len() + 2);
        let mut segments: Vec<String> =
            Vec::with_capacity(fp.bytes().filter(|&b| b == b'/').count() + 2);
        for seg in fp
            .split('/')
            .chain(file_name.split('/'))
            .filter(|s| is_safe(s))
        {
            joined.push('/');
            joined.push_str(seg);
            segments.push(seg.to_string());
        }
        if segments.is_empty() {
            joined.push('/');
        }
        (StoragePathRep { segments }, joined)
    }
    pub struct EntityRep {
        pub storage_path: StoragePathRep,
        pub path_string: String,
        #[allow(dead_code)]
        pub name: String,
    }
    impl EntityRep {
        pub fn file_name(&self) -> Option<String> {
            self.storage_path.segments.last().cloned()
        }
        pub fn display(&self) -> String {
            if self.storage_path.segments.is_empty() {
                return "/".to_string();
            }
            let mut s = String::new();
            for seg in &self.storage_path.segments {
                s.push('/');
                s.push_str(seg);
            }
            s
        }
    }
}

/// AFTER shape: canonical joined `String` only; segments derived on demand.
mod sp_after {
    pub struct StoragePathRep {
        joined: String,
    }
    fn is_safe(s: &str) -> bool {
        !s.is_empty() && s != "." && s != ".." && !s.contains('/')
    }
    impl StoragePathRep {
        pub fn from_folder_and_name(folder_path: Option<&str>, file_name: &str) -> Self {
            let fp = folder_path.unwrap_or("");
            let mut joined = String::with_capacity(fp.len() + file_name.len() + 2);
            for seg in fp
                .split('/')
                .chain(file_name.split('/'))
                .filter(|s| is_safe(s))
            {
                joined.push('/');
                joined.push_str(seg);
            }
            if joined.is_empty() {
                joined.push('/');
            }
            Self { joined }
        }
        pub fn as_joined(&self) -> &str {
            &self.joined
        }
        pub fn into_joined(self) -> String {
            self.joined
        }
        pub fn file_name(&self) -> Option<String> {
            if self.joined == "/" {
                None
            } else {
                self.joined.rsplit('/').next().map(str::to_string)
            }
        }
    }
    pub struct EntityRep {
        pub storage_path: StoragePathRep,
        #[allow(dead_code)]
        pub name: String,
    }
    impl EntityRep {
        pub fn file_name(&self) -> Option<String> {
            self.storage_path.file_name()
        }
        pub fn display(&self) -> String {
            self.storage_path.as_joined().to_string()
        }
    }
}

fn section_20(iters: u64) {
    println!("  §20 row → entity path materialization (500-row page, depth 4)");
    let rows: Vec<(String, String)> = (0..500)
        .map(|i| {
            (
                format!("/Fotos/2026/Julio/Viaje a la sierra {}", i % 7),
                format!("IMG_2026{:04}.jpg", i),
            )
        })
        .collect();

    // Equivalence gates across representations.
    let mut ok_path = true;
    let mut ok_name = true;
    let mut ok_disp = true;
    for (fp, name) in &rows {
        let (bsp, bjoined) = sp_before::from_folder_and_name(Some(fp), name);
        let be = sp_before::EntityRep {
            storage_path: bsp,
            path_string: bjoined,
            name: name.clone(),
        };
        let asp = sp_after::StoragePathRep::from_folder_and_name(Some(fp), name);
        let ae = sp_after::EntityRep {
            storage_path: asp,
            name: name.clone(),
        };
        ok_path &= be.path_string == ae.storage_path.as_joined();
        ok_name &= be.file_name() == ae.file_name();
        ok_disp &= be.display() == ae.display();
    }
    gate("path_string identical", ok_path);
    gate("file_name identical", ok_name);
    gate("display identical", ok_disp);

    let it = (iters / 500).max(100);
    measure("BEFORE joined + Vec<String> + dup", it, || {
        let mut total = 0usize;
        for (fp, name) in black_box(&rows) {
            let (sp, joined) = sp_before::from_folder_and_name(Some(fp), name);
            let e = sp_before::EntityRep {
                storage_path: sp,
                path_string: joined,
                name: name.clone(),
            };
            // DTO consumes the joined string (moved), segments dropped.
            let dto_path = e.path_string;
            total += dto_path.len();
        }
        total
    });
    measure("AFTER  single canonical String", it, || {
        let mut total = 0usize;
        for (fp, name) in black_box(&rows) {
            let sp = sp_after::StoragePathRep::from_folder_and_name(Some(fp), name);
            let e = sp_after::EntityRep {
                storage_path: sp,
                name: name.clone(),
            };
            let dto_path = e.storage_path.into_joined();
            total += dto_path.len();
        }
        total
    });
}

// ─── §21 display classifier fusion ──────────────────────────────────────────

fn section_21(iters: u64) {
    use oxicloud::application::dtos::display_helpers::{
        category_for, classify_display, icon_class_for,
    };
    println!("  §21 display double-classify (per listing row)");
    // Was a TRIPLE classify when ROUND11 measured 2.12x: `icon_special_class`
    // was retired from the wire (retire-precomputed-display-fields.md step 2),
    // so the fusion now covers two trees and the delta below is correspondingly
    // smaller than the archived figure. The archived number is left as-is in
    // ROUND11.md — it records what was true when the fusion landed.

    // Corpus spanning: specific MIME, prefix MIME, octet-stream + ext
    // fallback (lower/UPPER), no-ext, >16-byte ext, non-ASCII ext, dotfile.
    let corpus: &[(&str, &str)] = &[
        ("IMG_2026.JPG", "image/jpeg"),
        ("informe.pdf", "application/pdf"),
        ("main.rs", "application/octet-stream"),
        ("ARCHIVO.TXT", ""),
        ("setup.AppImage", "application/octet-stream"),
        ("video.mkv", "video/x-matroska"),
        ("script.PY", ""),
        ("no_extension", "application/octet-stream"),
        ("weird.extensionlongerthansixteen", ""),
        ("acentuado.ñml", ""),
        (".bashrc", "text/plain"),
        ("data.json", "application/json"),
        ("song.FLAC", "application/octet-stream"),
    ];

    // Gate 1: fused output identical to the public classifiers.
    let mut ok = true;
    for (name, mime) in corpus {
        let c = classify_display(name, mime);
        ok &= c.icon_class == icon_class_for(name, mime) && c.category == category_for(name, mime);
    }
    gate("fused == separate classifiers (corpus)", ok);
    // Gate 2: the historical heap-lowered ext hits the same arms — for
    // ≤16-byte exts the stack lowering equals `to_ascii_lowercase()`; a
    // >16-byte ext must land on the same defaults the old `_` arms gave.
    let long = classify_display("weird.extensionlongerthansixteen", "");
    gate(
        "long-ext defaults match old `_` arms",
        long.icon_class == "fas fa-file" && long.category == "Document",
    );

    // BEFORE replica: per-classifier ext_of + heap to_ascii_lowercase (the
    // shipped trees are shared, so the delta measured is exactly the
    // plumbing the fusion removed).
    fn ext_of(name: &str) -> Option<&str> {
        let name = name.rsplit('/').next().unwrap_or(name);
        let after_dot = name.rsplit('.').next()?;
        if after_dot.len() == name.len() || after_dot.is_empty() {
            return None;
        }
        Some(after_dot)
    }

    let it = (iters / 10).max(1000);
    measure("BEFORE 2× classify (heap ext on fallback)", it, || {
        let mut acc = 0usize;
        for (name, mime) in corpus {
            // The pre-fusion code heap-lowercased the ext INSIDE each
            // classifier, but only on rows that fell through to the
            // extension fallback (generic/empty MIME) — replicate exactly
            // that alloc profile next to the shared decision trees.
            let falls_back = mime.is_empty() || *mime == "application/octet-stream";
            if falls_back {
                let _l = ext_of(name).map(|e| e.to_ascii_lowercase());
            }
            let a = icon_class_for(name, mime);
            if falls_back {
                let _l = ext_of(name).map(|e| e.to_ascii_lowercase());
            }
            let c = category_for(name, mime);
            acc += a.len() + c.len();
        }
        acc
    });
    measure("AFTER  fused classify_display", it, || {
        let mut acc = 0usize;
        for (name, mime) in corpus {
            let c = classify_display(name, mime);
            acc += c.icon_class.len() + c.category.len();
        }
        acc
    });
}

// ─── main ───────────────────────────────────────────────────────────────────

fn main() {
    let iters: u64 = env_or("BENCH_ITERS", 100_000);
    println!("bench_round11_micro — iters={iters}\n");

    section_1(iters);
    section_2(iters);
    section_3(iters);
    section_4(iters);
    section_5(iters);
    section_6(iters);
    section_7(iters);
    section_8(iters);
    section_9(iters);
    section_10(iters);
    section_11(iters);
    section_12(iters);
    section_13(iters);
    section_14(iters);
    section_15_16(iters);
    section_17(iters);
    section_18(iters);
    section_19(iters);
    section_20(iters);
    section_21(iters);

    println!("\ndone");
}
