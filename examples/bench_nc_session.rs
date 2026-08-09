//! NextCloud per-request session benchmark — deep-clone vs `Arc` end-to-end.
//!
//! Every authenticated NC request (all six DAV dispatchers + OCS) extracts
//! the session. The old pipeline paid, per request:
//!
//!   • extractor: `(**arc).clone()` — a DEEP clone of `NcSession`
//!     (`CurrentUser` 3 Strings + `raw_username` + chroot `FolderDto`
//!     ~5 Strings ≈ 8-9 heap allocs) despite the doc claiming "one Arc
//!     increment";
//!   • chroot cache hit: moka `get` clones the stored `FolderDto` by value
//!     (~5 more allocs) on the markerless (default-drive) branch;
//!   • session build: `CurrentUser` built then cloned for the extension,
//!     `raw_username` cloned, `user_id.to_string()` for the span.
//!
//! Round 9 stores `Arc<FolderDto>` in the cache, shares one
//! `Arc<CurrentUser>` between the extension and the session, and extracts
//! `SharedNcSession` (an `Arc` handle that derefs to `NcSession`).
//!
//! `mod before` replicates the old struct shapes + clone flows verbatim;
//! equivalence gates assert every field consumed by handlers is identical.
//!
//! Sections:
//!   1. Extractor — allocs/extract + ns/extract (BEFORE deep clone vs
//!      AFTER production `SharedNcSession::from_request_parts`)
//!   2. Chroot-cache hit — allocs/hit (FolderDto-by-value vs Arc)
//!   3. Session build — allocs/build (double CurrentUser + clones vs
//!      single shared Arc + moves)
//!
//! Run (no Postgres needed):
//!   cargo run --release --features bench --example bench_nc_session
//! Tunables (env): BENCH_REQS (100000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::extract::FromRequestParts;
use oxicloud::application::dtos::folder_dto::FolderDto;
use oxicloud::interfaces::middleware::auth::CurrentUser;
use oxicloud::interfaces::nextcloud::session::{NcSession, SharedNcSession};

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

// ─── BEFORE replicas (verbatim old shapes) ──────────────────────────────────

mod before {
    use super::*;

    /// Old `NcSession` shape: owned `CurrentUser`, chroot by value.
    #[derive(Debug, Clone)]
    pub struct OldNcSession {
        pub user: CurrentUser,
        pub raw_username: String,
        pub chroot: Option<FolderDto>,
    }

    /// Old extractor body: deep clone out of the shared Arc.
    pub fn extract(arc: &Arc<OldNcSession>) -> OldNcSession {
        (**arc).clone()
    }
}

fn fixture_folder() -> FolderDto {
    FolderDto {
        id: uuid::Uuid::new_v4().to_string(),
        name: "Personal".to_string(),
        path: "Personal".to_string(),
        parent_id: None,
        drive_id: uuid::Uuid::new_v4(),
        created_at: 1_700_000_000,
        modified_at: 1_700_000_100,
        is_root: true,
        etag: "8f2e5a1c9b3d4e6f".to_string(),
        icon_class: Arc::from("fas fa-folder"),
        icon_special_class: Arc::from("folder-icon"),
        category: Arc::from("Folder"),
        created_by: None,
        updated_by: None,
        is_favorite: false,
        is_shared: false,
    }
}

fn fixture_user(id: uuid::Uuid) -> CurrentUser {
    CurrentUser {
        id,
        username: Arc::from("alice.longname"),
        email: Arc::from("alice.longname@example.com"),
        role: smol_str::SmolStr::new_static("user"),
        dpop_jkt: None,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let reqs: usize = env_or("BENCH_REQS", 100_000);
    let user_id = uuid::Uuid::new_v4();

    // ── Section 1: extractor ────────────────────────────────────────────────
    let old_session = Arc::new(before::OldNcSession {
        user: fixture_user(user_id),
        raw_username: "alice.longname".to_string(),
        chroot: Some(fixture_folder()),
    });
    let new_session = Arc::new(NcSession {
        user: Arc::new(fixture_user(user_id)),
        raw_username: "alice.longname".to_string(),
        chroot: Some(Arc::new(fixture_folder())),
    });

    // Equivalence gate: every field handlers consume is identical.
    {
        let old = before::extract(&old_session);
        let (mut parts, _) = axum::http::Request::builder()
            .uri("/ocs/v2.php/cloud/user")
            .extension(Arc::clone(&new_session))
            .body(())
            .expect("request")
            .into_parts();
        let new = SharedNcSession::from_request_parts(&mut parts, &())
            .await
            .expect("extract");
        assert_eq!(old.user.id, new.user.id);
        assert_eq!(old.user.username, new.user.username);
        assert_eq!(old.user.email, new.user.email);
        assert_eq!(old.user.role, new.user.role);
        assert_eq!(old.raw_username, new.raw_username);
        let (oc, nc) = (old.chroot.as_ref().unwrap(), new.require_chroot().unwrap());
        assert_eq!(oc.name, nc.name);
        assert_eq!(oc.path, nc.path);
        assert_eq!(oc.etag, nc.etag);
        println!("# equivalence gate: extracted session fields identical — OK");
    }

    // The URL cross-check runs in both arms' request flow; the BEFORE arm
    // replicates only the clone (its cross-check was identical string
    // compare — unchanged by round 9), so both arms time the same work
    // minus the measured clone-vs-bump difference.
    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..reqs {
        black_box(before::extract(black_box(&old_session)));
    }
    let before_ms = t.elapsed().as_secs_f64() * 1e3;
    let before_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;

    let (mut parts, _) = axum::http::Request::builder()
        .uri("/ocs/v2.php/cloud/user")
        .extension(Arc::clone(&new_session))
        .body(())
        .expect("request")
        .into_parts();
    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..reqs {
        let s = SharedNcSession::from_request_parts(black_box(&mut parts), &())
            .await
            .expect("extract");
        black_box(&s);
    }
    let after_ms = t.elapsed().as_secs_f64() * 1e3;
    let after_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;

    println!("\n#################################################################");
    println!("# [1] NC session extractor — deep clone vs Arc handle");
    println!("# extracts={reqs}");
    println!("#################################################################\n");
    println!(
        "| {:<26} | {:>10} | {:>12} | {:>14} |",
        "arm", "wall ms", "allocs", "allocs/extract"
    );
    println!(
        "| {:<26} | {:>10.1} | {:>12} | {:>14.3} |",
        "BEFORE (deep clone)",
        before_ms,
        before_allocs,
        before_allocs as f64 / reqs as f64
    );
    println!(
        "| {:<26} | {:>10.1} | {:>12} | {:>14.3} |",
        "AFTER  (SharedNcSession)",
        after_ms,
        after_allocs,
        after_allocs as f64 / reqs as f64
    );
    let s1_ok = after_allocs < before_allocs && after_ms < before_ms;

    // ── Section 2: chroot-cache hit ─────────────────────────────────────────
    let by_value: moka::sync::Cache<uuid::Uuid, FolderDto> = moka::sync::Cache::new(100);
    let by_arc: moka::sync::Cache<uuid::Uuid, Arc<FolderDto>> = moka::sync::Cache::new(100);
    let root_id = uuid::Uuid::new_v4();
    by_value.insert(root_id, fixture_folder());
    by_arc.insert(root_id, Arc::new(fixture_folder()));

    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..reqs {
        black_box(by_value.get(black_box(&root_id)));
    }
    let bv_ms = t.elapsed().as_secs_f64() * 1e3;
    let bv_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;

    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..reqs {
        black_box(by_arc.get(black_box(&root_id)));
    }
    let ba_ms = t.elapsed().as_secs_f64() * 1e3;
    let ba_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;

    println!("\n#################################################################");
    println!("# [2] chroot-cache hit — FolderDto by value vs Arc<FolderDto>");
    println!("# hits={reqs}");
    println!("#################################################################\n");
    println!(
        "| {:<26} | {:>10} | {:>12} | {:>12} |",
        "arm", "wall ms", "allocs", "allocs/hit"
    );
    println!(
        "| {:<26} | {:>10.1} | {:>12} | {:>12.3} |",
        "BEFORE (by value)",
        bv_ms,
        bv_allocs,
        bv_allocs as f64 / reqs as f64
    );
    println!(
        "| {:<26} | {:>10.1} | {:>12} | {:>12.3} |",
        "AFTER  (Arc)",
        ba_ms,
        ba_allocs,
        ba_allocs as f64 / reqs as f64
    );
    let s2_ok = ba_allocs < bv_allocs;

    // ── Section 3: session build ────────────────────────────────────────────
    // BEFORE: build CurrentUser, clone it for the extension Arc, clone
    // raw_username, `to_string` the span value. AFTER: one Arc shared by
    // extension + session, raw_username moved, span rendered lazily (the
    // lazy render costs nothing here; the removed `to_string` did).
    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..reqs {
        let raw_username = String::from("alice.longname");
        let span_value = user_id.to_string();
        let current_user = fixture_user(user_id);
        let ext = Arc::new(current_user.clone());
        let session = Arc::new(before::OldNcSession {
            user: current_user,
            raw_username: raw_username.clone(),
            chroot: None,
        });
        black_box((&span_value, &ext, &session));
    }
    let sb_ms = t.elapsed().as_secs_f64() * 1e3;
    let sb_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;

    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..reqs {
        let raw_username = String::from("alice.longname");
        let current_user = Arc::new(fixture_user(user_id));
        let ext = Arc::clone(&current_user);
        let session = Arc::new(NcSession {
            user: current_user,
            raw_username,
            chroot: None,
        });
        black_box((&ext, &session));
    }
    let sa_ms = t.elapsed().as_secs_f64() * 1e3;
    let sa_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;

    println!("\n#################################################################");
    println!("# [3] session build — double CurrentUser + clones vs shared Arc");
    println!("# builds={reqs}");
    println!("#################################################################\n");
    println!(
        "| {:<26} | {:>10} | {:>12} | {:>12} |",
        "arm", "wall ms", "allocs", "allocs/build"
    );
    println!(
        "| {:<26} | {:>10.1} | {:>12} | {:>12.3} |",
        "BEFORE (clone x2 + span)",
        sb_ms,
        sb_allocs,
        sb_allocs as f64 / reqs as f64
    );
    println!(
        "| {:<26} | {:>10.1} | {:>12} | {:>12.3} |",
        "AFTER  (shared Arc)",
        sa_ms,
        sa_allocs,
        sa_allocs as f64 / reqs as f64
    );
    let s3_ok = sa_allocs < sb_allocs;

    if !(s1_ok && s2_ok && s3_ok) {
        eprintln!("\nGATE FAIL: (extractor={s1_ok} cache={s2_ok} build={s3_ok}) — rollback");
        std::process::exit(1);
    }
    println!("\nGATE PASS: all three session stages allocate less with identical fields.");
}
