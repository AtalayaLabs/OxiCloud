//! Round-27 CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–26: BEFORE (replica of the shipped-before shape) vs AFTER
//! (replica of the shipped-after shape, which the source is then made to match),
//! with a value-equivalence gate and a `GATE FAIL … rollback` `exit(1)` if the
//! AFTER arm fails to beat BEFORE.
//!
//!   [H1] The NextCloud PROPFIND page loops build `oc:id` as a fresh `String`
//!        per child (`format_oc_id(id, svc)` = `format!("{:08}{}", id, instance)`),
//!        then pass `oc_id.as_deref()` into `write_{file,folder}_response`. The
//!        sibling per-row costs (href, etag, dates) were already reduced to a
//!        reused buffer / borrowed events (ROUND19/20); oc:id was the last
//!        per-row String. AFTER computes it into one `oc_buf` reused across the
//!        page via `format_oc_id_into` — 1 String/row → 0 (amortized).
//!
//!   [P2] `contact_pg_repository::{create,update}_contact` build a throwaway
//!        `serde_json::Value` per JSONB column (`serde_json::to_value(&dtos)`)
//!        and bind that — the Value tree is serialized to JSONB bytes at encode
//!        time and dropped. AFTER binds `sqlx::types::Json(&dtos)`, whose
//!        `Encode` runs `serde_json::to_writer` straight into the JSONB buffer,
//!        skipping the intermediate DOM (the write-side twin of ROUND23 §J1).
//!
//! Run:
//!   RUSTFLAGS="-C target-cpu=x86-64-v3" \
//!     cargo bench --features=bench --bench bench_round27_micro
//! Tunables (env): H1_ROWS (500), P2_ITERS (100000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fmt::Write as _;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;

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

// ── [H1] oc:id per-row String vs reused buffer ───────────────────────────────
fn format_oc_id(id: i64, instance: &str) -> String {
    format!("{id:08}{instance}")
}
fn format_oc_id_into(out: &mut String, id: i64, instance: &str) {
    out.clear();
    let _ = write!(out, "{id:08}");
    out.push_str(instance);
}

fn section_h1() {
    let rows: usize = env_or("H1_ROWS", 500);
    let instance = "ocnca";

    // Equivalence: the reused-buffer output matches the per-row String byte-for-byte.
    for id in [0i64, 7, 12345, 99_999_999] {
        let mut buf = String::new();
        format_oc_id_into(&mut buf, id, instance);
        assert_eq!(buf, format_oc_id(id, instance), "H1 oc:id differs");
    }

    let (bns, ba) = measure(2000, || {
        // BEFORE: one String per row.
        let mut sink = 0usize;
        for i in 0..rows {
            let s = format_oc_id(black_box(i as i64), instance);
            sink += s.len();
        }
        black_box(sink);
    });
    let (ans, aa) = measure(2000, || {
        // AFTER: one buffer reused across the page.
        let mut oc_buf = String::new();
        let mut sink = 0usize;
        for i in 0..rows {
            format_oc_id_into(&mut oc_buf, black_box(i as i64), instance);
            sink += oc_buf.len();
        }
        black_box(sink);
    });
    report(
        &format!("[H1] PROPFIND oc:id ({rows} rows)"),
        bns,
        ba,
        ans,
        aa,
    );
    gate("H1", ba, aa);
}

// ── [P2] contact JSONB write: to_value DOM vs direct serialize (Json<T>) ──────
#[derive(Serialize, serde::Deserialize, Clone, PartialEq, Debug)]
struct EmailDto {
    email: String,
    r#type: String,
    is_primary: bool,
}

fn section_p2() {
    let iters: u64 = env_or("P2_ITERS", 100_000);
    let dtos: Vec<EmailDto> = (0..3)
        .map(|i| EmailDto {
            email: format!("user{i}@example.com"),
            r#type: "home".into(),
            is_primary: i == 0,
        })
        .collect();

    // Equivalence: the two serializations differ only in key ORDER —
    // `serde_json::to_value` builds a (sorted) Map, direct serialize keeps struct
    // order — but Postgres normalizes JSONB key order, so the STORED value and
    // the read-back DTOs are identical (verified via psql:
    // `'{...alpha...}'::jsonb = '{...struct...}'::jsonb` → t). Assert the
    // semantic equivalence: both decode back to the same DTOs.
    let via_dom = serde_json::to_vec(&serde_json::to_value(&dtos).unwrap()).unwrap();
    let direct = serde_json::to_vec(&dtos).unwrap();
    let from_dom: Vec<EmailDto> = serde_json::from_slice(&via_dom).unwrap();
    let from_direct: Vec<EmailDto> = serde_json::from_slice(&direct).unwrap();
    assert_eq!(from_dom, from_direct, "P2 decoded DTOs differ");

    let (bns, ba) = measure(iters, || {
        // BEFORE: build a serde_json::Value DOM, then serialize it (what
        // `to_value(&dtos)` + binding the Value does).
        let v = serde_json::to_value(black_box(&dtos)).unwrap();
        black_box(serde_json::to_vec(&v).unwrap());
    });
    let (ans, aa) = measure(iters, || {
        // AFTER: serialize the DTOs straight to JSONB bytes (what
        // `Json(&dtos)`'s Encode does via to_writer) — no intermediate DOM.
        black_box(serde_json::to_vec(black_box(&dtos)).unwrap());
    });
    report(
        "[P2] contact JSONB write (Value DOM vs direct serialize)",
        bns,
        ba,
        ans,
        aa,
    );
    gate("P2", ba, aa);
}

fn main() {
    println!("# Round-27 micro alloc pack\n");
    section_h1();
    section_p2();
    println!("All Round-27 micro sections passed their gate.");
}
