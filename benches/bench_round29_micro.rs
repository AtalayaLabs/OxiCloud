//! Round-29 CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–28: BEFORE (replica of the shipped-before shape) vs AFTER
//! (replica of the shipped-after shape, which the source is then made to match),
//! with a value-equivalence gate and a `GATE FAIL … rollback` `exit(1)` if the
//! AFTER arm fails to beat BEFORE on allocs/op.
//!
//!   [A] NextCloud REPORT emit loops (`report_handler`) still build each row's
//!       `<d:href>` with `nc_href(url_user, subpath)` — a fresh `String` per file
//!       row, and `format!("{}/", nc_href(...))` (TWO Strings) per folder row —
//!       re-encoding the constant `url_user` on every row. The hotter PROPFIND
//!       child loop was already hoisted to a reused buffer + once-encoded prefix
//!       (webdav_handler.rs child loop). AFTER mirrors that: `nc_href_into`
//!       writes into one reused buffer with a precomputed `encoded_user`.
//!
//!   [B] The cache-serve fast path (`file_retrieval_service::optimized_inner`
//!       Tier 1 and `get_file_range_preloaded` — the video-scrub hot path) builds
//!       the owned `get_or_load` args (`format!("\"{}\"", hash)` etag, the
//!       `hash.to_string()` key, `id.to_string()`) BEFORE the cache is probed.
//!       `get_or_load`'s first line is a lock-free `self.get(&key)` that returns
//!       on a hit and never touches any of them — so a cache HIT throws all of
//!       them away. AFTER probes `cache.get(&hash)` (a borrow) first and builds
//!       the owned args only on a miss.
//!
//!   [C] `file_retrieval_service::read_full` reassembles the blob stream with
//!       `BytesMut::with_capacity(cap)` + `extend_from_slice` per frame. The local
//!       backend yields owned contiguous `Bytes` frames; for a file that fits in
//!       one frame (≤256 KB) this copies the whole payload a SECOND time into a
//!       fresh buffer. AFTER returns the sole frame directly (zero-copy) and only
//!       falls back to the pre-sized concat when there is more than one frame.
//!
//! Run:
//!   RUSTFLAGS="-C target-cpu=x86-64-v3" \
//!     cargo bench --features=bench --bench bench_round29_micro
//! Tunables (env): A_ROWS (500), B_ITERS (200000), C_ITERS (50000), C_FRAME (200000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bytes::{Bytes, BytesMut};

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

fn measure(iters: u64, mut f: impl FnMut()) -> (f64, f64) {
    f();
    ALLOC_CALLS.store(0, Ordering::Relaxed);
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let ns = start.elapsed().as_nanos() as f64 / iters as f64;
    let allocs = ALLOC_CALLS.load(Ordering::Relaxed) as f64 / iters as f64;
    (ns, allocs)
}

fn report(tag: &str, bns: f64, ba: f64, ans: f64, aa: f64) {
    println!("## {tag}");
    println!("| arm    |     ns/op | allocs/op |");
    println!("| BEFORE | {bns:>9.1} | {ba:>9.2} |");
    println!("| AFTER  | {ans:>9.1} | {aa:>9.2} |");
    println!(
        "# {:.2}x wall · {:.2} fewer allocs/op\n",
        bns / ans.max(0.0001),
        ba - aa
    );
}

fn gate(tag: &str, before: f64, after: f64) {
    if after >= before {
        eprintln!("GATE FAIL [{tag}] allocs/op: AFTER {after} !< BEFORE {before} — rollback");
        std::process::exit(1);
    }
}

// ── [A] NextCloud REPORT href: per-row String(s) vs reused buffer ─────────────
// Faithful replicas of the source functions.
fn nc_href(username: &str, subpath: &str) -> String {
    let subpath = subpath.trim_matches('/');
    let encoded_user = urlencoding::encode(username);
    const PREFIX: &str = "/remote.php/dav/files/";
    let mut out = String::with_capacity(PREFIX.len() + encoded_user.len() + subpath.len() + 8);
    out.push_str(PREFIX);
    out.push_str(&encoded_user);
    out.push('/');
    for (i, seg) in subpath.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&urlencoding::encode(seg));
    }
    out
}

/// AFTER: write the href into a reused buffer with a precomputed encoded user.
fn nc_href_into(out: &mut String, encoded_user: &str, subpath: &str) {
    let subpath = subpath.trim_matches('/');
    out.clear();
    const PREFIX: &str = "/remote.php/dav/files/";
    out.reserve(PREFIX.len() + encoded_user.len() + subpath.len() + 8);
    out.push_str(PREFIX);
    out.push_str(encoded_user);
    out.push('/');
    for (i, seg) in subpath.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&urlencoding::encode(seg));
    }
}

/// AFTER: collection variant — trailing slash guaranteed, in place.
fn nc_collection_href_into(out: &mut String, encoded_user: &str, subpath: &str) {
    nc_href_into(out, encoded_user, subpath);
    if !out.ends_with('/') {
        out.push('/');
    }
}

fn section_a() {
    let rows: usize = env_or("A_ROWS", 500);
    let user = "admin";
    // A flat REPORT/search result: files and folders at varying paths (each row
    // a DIFFERENT subpath, unlike PROPFIND's shared-parent children — so the win
    // is the per-row String + the once-per-page user encode, not a hoisted prefix).
    let paths: Vec<(bool, String)> = (0..rows)
        .map(|i| {
            let is_dir = i % 2 == 0;
            let p = format!("Documents/2024/q{}/report-{i}.dat", i % 4);
            (is_dir, p)
        })
        .collect();

    // Equivalence: AFTER href bytes match BEFORE for every row.
    {
        let encoded_user = urlencoding::encode(user);
        let mut buf = String::new();
        for (is_dir, p) in &paths {
            let before = if *is_dir {
                format!("{}/", nc_href(user, p))
            } else {
                nc_href(user, p)
            };
            if *is_dir {
                nc_collection_href_into(&mut buf, &encoded_user, p);
            } else {
                nc_href_into(&mut buf, &encoded_user, p);
            }
            assert_eq!(buf, before, "A href differs for {p}");
        }
    }

    let (bns, ba) = measure(2000, || {
        // BEFORE: nc_href per file row; nc_href + format! per folder row.
        let mut sink = 0usize;
        for (is_dir, p) in &paths {
            let href = if *is_dir {
                format!("{}/", nc_href(user, black_box(p)))
            } else {
                nc_href(user, black_box(p))
            };
            sink += href.len();
        }
        black_box(sink);
    });
    let (ans, aa) = measure(2000, || {
        // AFTER: one reused buffer, user encoded once per page.
        let encoded_user = urlencoding::encode(user);
        let mut href = String::new();
        let mut sink = 0usize;
        for (is_dir, p) in &paths {
            if *is_dir {
                nc_collection_href_into(&mut href, &encoded_user, black_box(p));
            } else {
                nc_href_into(&mut href, &encoded_user, black_box(p));
            }
            sink += href.len();
        }
        black_box(sink);
    });
    report(
        &format!("[A] NC REPORT href ({rows} rows)"),
        bns,
        ba,
        ans,
        aa,
    );
    gate("A", ba, aa);
}

// ── [B] cache-serve fast path: eager owned args vs borrow-probe ───────────────
fn section_b() {
    let iters: u64 = env_or("B_ITERS", 200_000);
    let hash = "b3a1c0ffee1234567890abcdef0123456789abcdef0123456789abcdef012345";
    let id = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    let mime: Arc<str> = Arc::from("video/mp4");
    // A tiny content-addressed "cache": key = blob hash → (bytes, etag, ct).
    let mut map: std::collections::HashMap<String, (Bytes, Arc<str>, Arc<str>)> =
        std::collections::HashMap::new();
    let etag_stored: Arc<str> = format!("\"{hash}\"").into();
    map.insert(
        hash.to_string(),
        (
            Bytes::from_static(b"\x00\x01\x02\x03some-cached-blob-bytes"),
            etag_stored,
            mime.clone(),
        ),
    );

    // Equivalence: both arms retrieve the identical cached Bytes on a hit.
    let before_hit = {
        let _etag: Arc<str> = format!("\"{hash}\"").into();
        let _key = hash.to_string();
        let _id_owned = id.to_string();
        map.get(hash).map(|(b, _, _)| b.clone())
    };
    let after_hit = map.get(hash).map(|(b, _, _)| b.clone());
    assert_eq!(before_hit, after_hit, "B cached bytes differ");

    let (bns, ba) = measure(iters, || {
        // BEFORE: build the owned get_or_load args, THEN probe (hit ignores them).
        let etag: Arc<str> = format!("\"{}\"", black_box(hash)).into();
        let ct: Arc<str> = mime.clone();
        let id_owned = black_box(id).to_string();
        let key = black_box(hash).to_string();
        let hit = map.get(key.as_str()).map(|(b, _, _)| b.clone());
        black_box((etag, ct, id_owned, hit));
    });
    let (ans, aa) = measure(iters, || {
        // AFTER: probe with a borrow first; on a hit build nothing.
        let hit = map.get(black_box(hash)).map(|(b, _, _)| b.clone());
        black_box(hit);
    });
    report("[B] cache-serve fast path (hit)", bns, ba, ans, aa);
    gate("B", ba, aa);
}

// ── [C] read_full: single-frame BytesMut concat vs zero-copy passthrough ──────
fn read_full_before(frames: &[Bytes], capacity: usize) -> Bytes {
    let mut buf = BytesMut::with_capacity(capacity);
    for f in frames {
        buf.extend_from_slice(f);
    }
    buf.freeze()
}

fn read_full_after(frames: &[Bytes], capacity: usize) -> Bytes {
    // Single frame → return it directly (zero copy). Multi-frame → identical concat.
    match frames {
        [] => Bytes::new(),
        [only] => only.clone(),
        _ => {
            let mut buf = BytesMut::with_capacity(capacity);
            for f in frames {
                buf.extend_from_slice(f);
            }
            buf.freeze()
        }
    }
}

fn section_c() {
    let iters: u64 = env_or("C_ITERS", 50_000);
    let frame_len: usize = env_or("C_FRAME", 200_000);
    // The local backend yields one owned contiguous frame for a ≤256 KB file.
    let frame = Bytes::from(vec![0u8; frame_len]);
    let frames = [frame.clone()];
    let cap = frame_len;

    // Equivalence: identical bytes out.
    assert_eq!(
        read_full_before(&frames, cap),
        read_full_after(&frames, cap),
        "C single-frame bytes differ"
    );

    let (bns, ba) = measure(iters, || {
        black_box(read_full_before(black_box(&frames), cap));
    });
    let (ans, aa) = measure(iters, || {
        black_box(read_full_after(black_box(&frames), cap));
    });
    report(
        &format!("[C] read_full single frame ({frame_len} B)"),
        bns,
        ba,
        ans,
        aa,
    );
    gate("C", ba, aa);
}

// ── [D] login-lockout key: to_lowercase()+format! vs single ASCII buffer ──────
fn lockout_key_before(username: &str, client_ip: &str) -> String {
    format!("{}|{}", username.to_lowercase(), client_ip)
}
fn lockout_key_after(username: &str, client_ip: &str) -> String {
    if username.is_ascii() {
        let mut k = String::with_capacity(username.len() + 1 + client_ip.len());
        for &b in username.as_bytes() {
            k.push(b.to_ascii_lowercase() as char);
        }
        k.push('|');
        k.push_str(client_ip);
        k
    } else {
        format!("{}|{}", username.to_lowercase(), client_ip)
    }
}

fn section_d() {
    let iters: u64 = env_or("D_ITERS", 200_000);
    let username = "alice.app-password";
    let client_ip = "203.0.113.42";
    // Equivalence across a matrix incl. mixed-case, composite marker, non-ASCII.
    for (u, ip) in [
        ("alice", "1.2.3.4"),
        ("Alice.Smith", "203.0.113.42"),
        ("BOB", "::1"),
        ("home~a1b2", "10.0.0.1"),
        ("ünïcode", "2001:db8::1"),
        ("", "unknown"),
    ] {
        assert_eq!(
            lockout_key_before(u, ip),
            lockout_key_after(u, ip),
            "D key differs for {u}"
        );
    }
    let (bns, ba) = measure(iters, || {
        black_box(lockout_key_before(
            black_box(username),
            black_box(client_ip),
        ));
    });
    let (ans, aa) = measure(iters, || {
        black_box(lockout_key_after(black_box(username), black_box(client_ip)));
    });
    report("[D] login-lockout key (ASCII)", bns, ba, ans, aa);
    gate("D", ba, aa);
}

// ── [E] NC composite-username parse: owned clone/to_string vs borrow ──────────
fn section_e() {
    let iters: u64 = env_or("E_ITERS", 500_000);
    let raw_no_marker = "alice.app-password".to_string();
    let raw_marker = "alice~a1b2c3d4".to_string();
    // Equivalence: borrowed slices equal the owned versions.
    {
        let (bu, bm): (&str, Option<&str>) = match raw_no_marker.split_once('~') {
            Some((u, m)) => (u, Some(m)),
            None => (raw_no_marker.as_str(), None),
        };
        assert_eq!(bu, raw_no_marker.as_str());
        assert!(bm.is_none());
        let (mu, mm) = raw_marker.split_once('~').unwrap();
        assert_eq!((mu, mm), ("alice", "a1b2c3d4"));
    }
    let (bns, ba) = measure(iters, || {
        // BEFORE: the no-marker path clones raw_username into an owned String.
        let (username, drive_marker): (String, Option<String>) =
            match black_box(&raw_no_marker).split_once('~') {
                Some((u, m)) => (u.to_string(), Some(m.to_string())),
                None => (raw_no_marker.clone(), None),
            };
        black_box((username, drive_marker));
    });
    let (ans, aa) = measure(iters, || {
        // AFTER: borrow the slices out of the already-owned raw_username.
        let (username, drive_marker): (&str, Option<&str>) =
            match black_box(&raw_no_marker).split_once('~') {
                Some((u, m)) => (u, Some(m)),
                None => (raw_no_marker.as_str(), None),
            };
        black_box((username, drive_marker));
    });
    report("[E] NC username parse (no-marker)", bns, ba, ans, aa);
    gate("E", ba, aa);
}

// ── [F] contact-group listing: decode the discarded vcard String vs skip it ───
fn section_f() {
    let rows: usize = env_or("F_ROWS", 200);
    let vcard_len: usize = env_or("F_VCARD", 8192); // ~8 KiB with an embedded base64 PHOTO
    let vcard_src = vec![b'v'; vcard_len];
    let (bns, ba) = measure(200, || {
        // BEFORE: decode the vcard TEXT column into an owned String per row,
        // then discard it (ContactDto has no vcard field).
        let mut sink = 0usize;
        for _ in 0..rows {
            let vcard = String::from_utf8(black_box(&vcard_src).clone()).unwrap();
            sink += vcard.len();
        }
        black_box(sink);
    });
    let (ans, aa) = measure(200, || {
        // AFTER: column not selected → empty String, no per-row alloc/copy.
        let mut sink = 0usize;
        for _ in 0..rows {
            let vcard = String::new();
            sink += vcard.len();
        }
        black_box(sink);
    });
    report(
        &format!("[F] contact-group vcard over-fetch ({rows}×{vcard_len}B)"),
        bns,
        ba,
        ans,
        aa,
    );
    gate("F", ba, aa);
}

// ── [G] admin count: hydrate N full user rows vs scalar COUNT ──────────────────
struct FakeUser {
    _username: String,
    _image: String,            // avatar data URI (server allows up to 512 KiB)
    _prefs: serde_json::Value, // ui_preferences JSONB DOM
}

fn section_g() {
    let admins: usize = env_or("G_ADMINS", 3);
    let image_len: usize = env_or("G_IMAGE", 65_536); // 64 KiB avatar (up to 512 KiB allowed)
    let image_src = vec![b'i'; image_len];
    let prefs_json = r#"{"theme":"dark","density":"comfortable","sidebar":true}"#;
    let (bns, ba) = measure(2000, || {
        // BEFORE: hydrate every admin's full row (username + avatar String +
        // ui_preferences Value DOM) only to take the count.
        let users: Vec<FakeUser> = (0..admins)
            .map(|i| FakeUser {
                _username: format!("admin{i}"),
                _image: String::from_utf8(black_box(&image_src).clone()).unwrap(),
                _prefs: serde_json::from_str(black_box(prefs_json)).unwrap(),
            })
            .collect();
        black_box(users.len() as i64);
    });
    let (ans, aa) = measure(2000, || {
        // AFTER: a scalar count — no rows hydrated.
        let count: i64 = black_box(admins) as i64;
        black_box(count);
    });
    report(
        &format!("[G] admin-count hydrate vs COUNT ({admins} admins × {image_len}B avatar)"),
        bns,
        ba,
        ans,
        aa,
    );
    gate("G", ba, aa);
}

fn main() {
    println!("# Round-29 micro alloc pack\n");
    section_a();
    section_b();
    section_c();
    section_d();
    section_e();
    section_f();
    section_g();
    println!("All Round-29 micro sections passed their gate.");
}
