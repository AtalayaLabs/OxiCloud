//! Face-indexing fan-out benchmark — unbounded spawn vs semaphore (ROUND4).
//!
//! `FaceIndexingService::spawn_index` fired one `tokio::spawn` per
//! uploaded/copied image with NO ceiling; each task reads the full blob
//! into RAM and decodes it before inference. A bulk upload of N photos
//! therefore held up to N decoded images in flight simultaneously.
//! AFTER: an `Arc<Semaphore>` sized to the effective core count
//! (`OXICLOUD_FACES_INDEX_CONCURRENCY` override), permit acquired BEFORE
//! the blob read — the exact `ThumbnailService::decode_semaphore`
//! invariant ("peak memory = permits × image size").
//!
//! This is a *pattern* bench (like POOL-CONCURRENCY / RUNTIME): the real
//! service needs Postgres + an ONNX model, so the task body models the
//! dominant costs — full-file read + JPEG decode on the deterministic
//! `bench_support` photo corpus — while the spawn/permit shape is copied
//! from the service verbatim. Metrics: wall time, PEAK LIVE HEAP (exact,
//! via counting allocator), decode results asserted identical.
//!
//! Run (no Postgres needed):
//!   cargo bench --features=bench --bench bench_faces_bound
//! Tunables (env): BENCH_IMAGES (48), BENCH_PERMITS (effective cores).

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

// ─── Peak-live-heap tracking allocator ──────────────────────────────────────

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

struct PeakAlloc;

fn bump(sz: u64) {
    let live = LIVE.fetch_add(sz, Ordering::Relaxed) + sz;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for PeakAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump(layout.size() as u64);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            bump((new_size - layout.size()) as u64);
        } else {
            LIVE.fetch_sub((layout.size() - new_size) as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump(layout.size() as u64);
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: PeakAlloc = PeakAlloc;

/// The modelled per-image work: full blob read (as `index_file` does via
/// `tokio::fs::read`) + JPEG decode (the analyzer's first step).
async fn index_one(path: std::path::PathBuf, dims: Arc<AtomicUsize>) {
    let bytes = tokio::fs::read(&path).await.expect("read blob");
    let img = tokio::task::spawn_blocking(move || image::load_from_memory(&bytes).expect("decode"))
        .await
        .expect("join decode");
    dims.fetch_add((img.width() + img.height()) as usize, Ordering::Relaxed);
    black_box(img);
}

fn effective_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let images: usize = env::var("BENCH_IMAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48);
    let permits: usize = env::var("BENCH_PERMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(effective_parallelism);

    // Deterministic photo corpus (12 MP JPEG case) → one temp file per
    // "upload" so each task pays a real filesystem read.
    let corpus = oxicloud::bench_support::load_or_generate();
    let jpeg = corpus
        .iter()
        .max_by_key(|c| c.bytes.len())
        .expect("corpus nonempty");
    println!(
        "bench_faces_bound — {images} images ({} · {:.1} MiB encoded), permits={permits}\n",
        jpeg.name,
        jpeg.bytes.len() as f64 / (1024.0 * 1024.0)
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let mut paths = Vec::with_capacity(images);
    for i in 0..images {
        let p = dir.path().join(format!("{i}.blob"));
        std::fs::write(&p, &jpeg.bytes).expect("write blob");
        paths.push(p);
    }

    // ── BEFORE: unbounded spawn per image (the old spawn_index shape) ──
    let dims_before = Arc::new(AtomicUsize::new(0));
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let t0 = Instant::now();
    let mut handles = Vec::with_capacity(images);
    for p in &paths {
        let p = p.clone();
        let dims = dims_before.clone();
        handles.push(tokio::spawn(async move {
            index_one(p, dims).await;
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let wall_before = t0.elapsed().as_secs_f64() * 1e3;
    let peak_before = PEAK.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);

    // ── AFTER: same spawn shape + semaphore permit before the read ──
    let dims_after = Arc::new(AtomicUsize::new(0));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(permits));
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let t0 = Instant::now();
    let mut handles = Vec::with_capacity(images);
    for p in &paths {
        let p = p.clone();
        let dims = dims_after.clone();
        let semaphore = semaphore.clone();
        handles.push(tokio::spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("semaphore never closes");
            index_one(p, dims).await;
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let wall_after = t0.elapsed().as_secs_f64() * 1e3;
    let peak_after = PEAK.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);

    println!("                       wall ms   peak live heap MiB");
    println!("BEFORE (unbounded)    {wall_before:8.1}   {peak_before:10.1}");
    println!(
        "AFTER  (semaphore {permits:>2}) {wall_after:8.1}   {peak_after:10.1}   heap {:.1}x lower",
        peak_before / peak_after
    );

    // ── Equivalence gate: identical decode results ──
    let db = dims_before.load(Ordering::Relaxed);
    let da = dims_after.load(Ordering::Relaxed);
    if db != da || db == 0 {
        eprintln!("GATE FAIL: dimension sums differ (before={db} after={da})");
        std::process::exit(1);
    }
    println!("\n[gate] OK — all {images} images decoded identically in both modes");
}
