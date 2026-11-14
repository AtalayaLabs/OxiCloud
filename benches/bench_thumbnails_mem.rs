//! Phase 0 — Task 0.3: peak-RAM and saturated-throughput baseline.
//!
//! Two measurements criterion does not give us:
//!   1. **Peak heap per decode** — via a counting global allocator wrapping the
//!      system allocator. We snapshot the high-water mark of bytes allocated
//!      around a single `render_all` call, i.e. the transient decode/resize/
//!      encode footprint. This is the number the `Semaphore` (`cpus/2`) exists
//!      to bound, and the one shrink-on-load (Task 1.1) should collapse.
//!   2. **Throughput under saturation** — N = cores threads hammering the same
//!      image for a fixed window → photos/sec and effective ms/photo, mirroring
//!      a burst of hundreds of uploads.
//!
//! Heap bytes here are *logical allocation* (what the program requested), not
//! RSS. For a Max-RSS cross-check on macOS run the binary under:
//!   `/usr/bin/time -l ./target/release/benches/bench_thumbnails_mem`
//!
//! Run: `cargo bench --features=bench --bench bench_thumbnails_mem`

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use oxicloud::application::ports::thumbnail_ports::ThumbnailFormat;
use oxicloud::bench_support::{self, CorpusCase};
use oxicloud::infrastructure::services::thumbnail_service::{ThumbnailService, ThumbnailSize};

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

struct TrackingAlloc;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let delta = new_size - layout.size();
                let now = CURRENT.fetch_add(delta, Ordering::Relaxed) + delta;
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static GLOBAL: TrackingAlloc = TrackingAlloc;

fn current() -> usize {
    CURRENT.load(Ordering::Relaxed)
}
fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}
fn reset_peak_to_current() {
    PEAK.store(CURRENT.load(Ordering::Relaxed), Ordering::Relaxed);
}

const MB: f64 = 1024.0 * 1024.0;

// ---------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------

struct PeakRow {
    name: &'static str,
    format: &'static str,
    width: u32,
    height: u32,
    megapixels: f64,
    input_bytes: usize,
    output_bytes: usize,
    single_ms: f64,
    peak_heap_bytes: usize,
}

/// Single-thread: peak transient heap + latency for one `render_all`.
fn measure_peak(case: &CorpusCase) -> PeakRow {
    // Warm up (let any one-time lazy allocations settle) and discard.
    let _ = ThumbnailService::bench_render_all(&case.bytes).expect("render_all warmup");

    let mut best_peak = 0usize;
    let mut best_ms = f64::INFINITY;
    let mut output_bytes = 0usize;
    for _ in 0..3 {
        reset_peak_to_current();
        let base = current();
        let t = Instant::now();
        let out = ThumbnailService::bench_render_all(&case.bytes).expect("render_all");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let pk = peak().saturating_sub(base);
        output_bytes = out.iter().map(|(_, n)| *n).sum();
        black_box(&out);
        best_peak = best_peak.max(pk);
        best_ms = best_ms.min(ms);
    }

    PeakRow {
        name: case.name,
        format: case.format,
        width: case.width,
        height: case.height,
        megapixels: case.megapixels(),
        input_bytes: case.bytes.len(),
        output_bytes,
        single_ms: best_ms,
        peak_heap_bytes: best_peak,
    }
}

struct ThroughputRow {
    name: &'static str,
    width: u32,
    height: u32,
    megapixels: f64,
    threads: usize,
    seconds: f64,
    photos: u64,
    photos_per_sec: f64,
    eff_ms_per_photo: f64,
}

/// N=threads workers render the same image until the window elapses.
fn measure_throughput(case: &CorpusCase, threads: usize, window: Duration) -> ThroughputRow {
    let counter = AtomicU64::new(0);
    let start = Instant::now();
    let deadline = start + window;

    thread::scope(|s| {
        for _ in 0..threads {
            let counter = &counter;
            let bytes = &case.bytes;
            s.spawn(move || {
                while Instant::now() < deadline {
                    let out = ThumbnailService::bench_render_all(bytes).expect("render_all");
                    black_box(out.len());
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let elapsed = start.elapsed().as_secs_f64();
    let photos = counter.load(Ordering::Relaxed);
    let pps = photos as f64 / elapsed;
    ThroughputRow {
        name: case.name,
        width: case.width,
        height: case.height,
        megapixels: case.megapixels(),
        threads,
        seconds: elapsed,
        photos,
        photos_per_sec: pps,
        eff_ms_per_photo: if pps > 0.0 { 1000.0 / pps } else { 0.0 },
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn main() {
    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let corpus = bench_support::load_or_generate();
    assert!(!corpus.is_empty(), "corpus is empty — generation failed");

    println!("\n###########################################################");
    println!("# Thumbnail render harness — current working tree");
    println!("# cores (available_parallelism): {threads}");
    println!("# corpus dir: {}", bench_support::corpus_dir().display());
    println!("# heap = logical allocation high-water mark (not RSS)");
    println!("###########################################################\n");

    // --- Table A: peak RAM + single-thread latency + output size ---
    println!("== A. Per-image: peak heap, single-thread latency, output size ==");
    println!(
        "| {:<17} | {:<5} | {:>11} | {:>6} | {:>8} | {:>9} | {:>13} | {:>11} |",
        "case", "fmt", "source", "MP", "input KB", "out KB", "render_all ms", "peak heap MB"
    );
    println!(
        "|{:-<19}|{:-<7}|{:-<13}|{:-<8}|{:-<10}|{:-<11}|{:-<15}|{:-<13}|",
        "", "", "", "", "", "", "", ""
    );
    let mut peak_rows = Vec::new();
    for case in &corpus {
        let row = measure_peak(case);
        println!(
            "| {:<17} | {:<5} | {:>5}×{:<5} | {:>6.1} | {:>8} | {:>9} | {:>13.2} | {:>11.1} |",
            row.name,
            row.format,
            row.width,
            row.height,
            row.megapixels,
            row.input_bytes / 1024,
            row.output_bytes / 1024,
            row.single_ms,
            row.peak_heap_bytes as f64 / MB,
        );
        peak_rows.push(row);
    }

    // --- Table B: saturated throughput (JPEG photo sizes only) ---
    println!("\n== B. Saturated throughput ({threads} threads, 3s window) ==");
    println!(
        "| {:<17} | {:>11} | {:>6} | {:>7} | {:>8} | {:>11} | {:>14} |",
        "case", "source", "MP", "threads", "photos", "photos/sec", "eff ms/photo"
    );
    println!(
        "|{:-<19}|{:-<13}|{:-<8}|{:-<9}|{:-<10}|{:-<13}|{:-<16}|",
        "", "", "", "", "", "", ""
    );
    let mut tp_rows = Vec::new();
    for case in corpus.iter().filter(|c| is_throughput_case(c.name)) {
        let row = measure_throughput(case, threads, Duration::from_secs(3));
        println!(
            "| {:<17} | {:>5}×{:<5} | {:>6.1} | {:>7} | {:>8} | {:>11.1} | {:>14.2} |",
            row.name,
            row.width,
            row.height,
            row.megapixels,
            row.threads,
            row.photos,
            row.photos_per_sec,
            row.eff_ms_per_photo,
        );
        tp_rows.push(row);
    }

    // --- Table C: quality — shrink-on-load vs full decode (Task 1.1 gate) ---
    println!("\n== C. Quality: shrink-on-load vs full-decode, Preview 400px ==");
    println!(
        "| {:<17} | {:>11} | {:>7} | {:>9} |",
        "case", "thumb dims", "SSIM", "PSNR dB"
    );
    println!("|{:-<19}|{:-<13}|{:-<9}|{:-<11}|", "", "", "", "");
    for case in corpus.iter().filter(|c| {
        matches!(
            c.name,
            "jpeg_12mp" | "jpeg_24mp" | "jpeg_48mp" | "small_300"
        )
    }) {
        let new_jpeg =
            ThumbnailService::bench_render_thumbnail(&case.bytes, ThumbnailSize::Preview)
                .expect("shrink-on-load render");
        let ref_jpeg = reference_render_full_decode(&case.bytes, 400);
        let (a, aw, ah) = decode_to_luma(&new_jpeg);
        let (b, bw, bh) = decode_to_luma(&ref_jpeg);
        if (aw, ah) != (bw, bh) {
            println!(
                "| {:<17} | {:>4}×{:<4} ⚠ ref {}×{} (dim mismatch) |",
                case.name, aw, ah, bw, bh
            );
            continue;
        }
        let ssim = block_ssim(&a, &b, aw, ah);
        let psnr = psnr(&a, &b);
        let flag = if ssim >= 0.98 { "" } else { "  ⚠ below 0.98" };
        println!(
            "| {:<17} | {:>4}×{:<4} | {:>7.4} | {:>9.2} |{}",
            case.name, aw, ah, ssim, psnr, flag
        );
    }

    // --- Table D: semaphore-bounded throughput (Task 1.5) ---
    // Mirrors the real service path (Semaphore + spawn_blocking) so we can see
    // the effect of the decode-concurrency cap. cpus/2 was the old default;
    // cpus is the new default; cpus*2 checks for diminishing returns.
    let half = (threads / 2).max(2);
    let permit_levels = [half, threads, threads * 2];
    println!("\n== D. Semaphore-bounded throughput (real path: Semaphore+spawn_blocking, 3s) ==");
    println!(
        "| {:<17} | {:>7} | {:>11} | {:<22} |",
        "case", "permits", "photos/sec", "note"
    );
    println!("|{:-<19}|{:-<9}|{:-<13}|{:-<24}|", "", "", "", "");
    for case in corpus
        .iter()
        .filter(|c| matches!(c.name, "jpeg_12mp" | "jpeg_24mp"))
    {
        for &permits in &permit_levels {
            let pps = measure_semaphore_throughput(case, permits, Duration::from_secs(3));
            let note = if permits == half {
                "cpus/2 (old default)"
            } else if permits == threads {
                "cpus (new default)"
            } else {
                "cpus*2 (oversubscribed)"
            };
            println!(
                "| {:<17} | {:>7} | {:>11.1} | {:<22} |",
                case.name, permits, pps, note
            );
        }
    }

    // --- Table E1: WebP quality sweep vs JPEG q80 (find the equal-SSIM q) ---
    println!(
        "\n== E1. WebP quality sweep vs JPEG q80 — Preview/400px, SSIM vs uncompressed source =="
    );
    println!(
        "   (the equal-quality bandwidth win = save% at the lowest WebP q whose ssim ≥ JPEG's)"
    );
    let sweep_q = [78.0_f32, 82.0, 86.0, 90.0];
    for case in corpus
        .iter()
        .filter(|c| matches!(c.name, "jpeg_12mp" | "jpeg_24mp" | "jpeg_48mp"))
    {
        let jpeg = ThumbnailService::bench_render_thumbnail_fmt(
            &case.bytes,
            ThumbnailSize::Preview,
            ThumbnailFormat::Jpeg,
        )
        .expect("jpeg");
        let (lj, wj, hj) = decode_to_luma(&jpeg);
        let refl = reference_luma_at(&case.bytes, wj, hj);
        let jpeg_ssim = block_ssim(&lj, &refl, wj, hj);
        println!(
            "  {:<10} JPEG q80: {:>6} B   ssim {:.4}",
            case.name,
            jpeg.len(),
            jpeg_ssim
        );
        for &q in &sweep_q {
            let webp =
                ThumbnailService::bench_render_webp_at(&case.bytes, ThumbnailSize::Preview, q)
                    .expect("webp");
            let (lw, ww, hw) = decode_to_luma(&webp);
            let ssim_w = if (ww, hw) == (wj, hj) {
                block_ssim(&lw, &refl, wj, hj)
            } else {
                f64::NAN
            };
            let save = 100.0 * (1.0 - webp.len() as f64 / jpeg.len() as f64);
            let flag = if ssim_w >= jpeg_ssim {
                "  ← ≥ JPEG"
            } else {
                ""
            };
            println!(
                "       webp q{:>3.0}: {:>6} B   ssim {:.4}   {:>5.1}% smaller{}",
                q,
                webp.len(),
                ssim_w,
                save,
                flag
            );
        }
    }

    // --- Table E2: codec comparison at the production WEBP_QUALITY const ---
    println!(
        "\n== E2. Production codec (WEBP_QUALITY const): JPEG vs WebP bytes + SSIM vs source =="
    );
    println!(
        "| {:<13} | {:<8} | {:>8} | {:>8} | {:>6} | {:>9} | {:>9} |",
        "case", "size", "jpeg B", "webp B", "save%", "ssim jpg", "ssim webp"
    );
    println!(
        "|{:-<15}|{:-<10}|{:-<10}|{:-<10}|{:-<8}|{:-<11}|{:-<11}|",
        "", "", "", "", "", "", ""
    );
    let mut jpeg_total = 0u64;
    let mut webp_total = 0u64;
    for case in corpus
        .iter()
        .filter(|c| matches!(c.name, "jpeg_12mp" | "jpeg_24mp" | "jpeg_48mp"))
    {
        for &size in &[
            ThumbnailSize::Icon,
            ThumbnailSize::Preview,
            ThumbnailSize::Large,
        ] {
            let jpeg = ThumbnailService::bench_render_thumbnail_fmt(
                &case.bytes,
                size,
                ThumbnailFormat::Jpeg,
            )
            .expect("jpeg encode");
            let webp = ThumbnailService::bench_render_thumbnail_fmt(
                &case.bytes,
                size,
                ThumbnailFormat::Webp,
            )
            .expect("webp encode");
            jpeg_total += jpeg.len() as u64;
            webp_total += webp.len() as u64;
            let save = 100.0 * (1.0 - webp.len() as f64 / jpeg.len() as f64);

            // SSIM of each codec vs the uncompressed full-decode source at the
            // thumbnail's exact dims — proves WebP is equal/better quality.
            let (lj, wj, hj) = decode_to_luma(&jpeg);
            let (lw, ww, hw) = decode_to_luma(&webp);
            let (ssim_j, ssim_w) = if (wj, hj) == (ww, hw) {
                let refl = reference_luma_at(&case.bytes, wj, hj);
                (
                    block_ssim(&lj, &refl, wj, hj),
                    block_ssim(&lw, &refl, wj, hj),
                )
            } else {
                (f64::NAN, f64::NAN)
            };
            println!(
                "| {:<13} | {:<8} | {:>8} | {:>8} | {:>5.1}% | {:>9.4} | {:>9.4} |",
                case.name,
                format!("{size:?}"),
                jpeg.len(),
                webp.len(),
                save,
                ssim_j,
                ssim_w
            );
        }
    }
    let total_save = 100.0 * (1.0 - webp_total as f64 / jpeg_total as f64);
    println!(
        "  ── all 3 sizes × 3 photos: JPEG {} B → WebP {} B = {:.1}% smaller ──",
        jpeg_total, webp_total, total_save
    );

    // Encode time, full pipeline (decode+resize+encode), Preview/12MP, best of N.
    if let Some(c) = corpus.iter().find(|c| c.name == "jpeg_12mp") {
        let n = 50u32;
        let mut tj = f64::INFINITY;
        let mut tw = f64::INFINITY;
        for _ in 0..3 {
            let t = Instant::now();
            for _ in 0..n {
                let _ = ThumbnailService::bench_render_thumbnail_fmt(
                    &c.bytes,
                    ThumbnailSize::Preview,
                    ThumbnailFormat::Jpeg,
                );
            }
            tj = tj.min(t.elapsed().as_secs_f64() * 1000.0 / n as f64);
            let t = Instant::now();
            for _ in 0..n {
                let _ = ThumbnailService::bench_render_thumbnail_fmt(
                    &c.bytes,
                    ThumbnailSize::Preview,
                    ThumbnailFormat::Webp,
                );
            }
            tw = tw.min(t.elapsed().as_secs_f64() * 1000.0 / n as f64);
        }
        println!(
            "  encode (Preview/12MP, full pipeline incl. shared decode): JPEG {tj:.2} ms vs WebP {tw:.2} ms"
        );
    }

    write_json(threads, &peak_rows, &tp_rows);

    println!(
        "\nWrote machine-readable baseline → {}",
        json_path().display()
    );
    println!(
        "For Max RSS / CPU time cross-check, re-run under:\n  /usr/bin/time -l ./target/release/benches/bench_thumbnails_mem\n"
    );
}

/// Throughput is only meaningful on the realistic upload load — the JPEG photo
/// sizes. (Tiny / GIF / WebP cases stay in the per-image table.)
fn is_throughput_case(name: &str) -> bool {
    matches!(name, "jpeg_12mp" | "jpeg_24mp" | "jpeg_48mp")
}

fn json_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("bench-baseline-fase0.json")
}

fn write_json(threads: usize, peak_rows: &[PeakRow], tp_rows: &[ThroughputRow]) {
    let per_image: Vec<_> = peak_rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "case": r.name,
                "format": r.format,
                "width": r.width,
                "height": r.height,
                "megapixels": r.megapixels,
                "input_bytes": r.input_bytes,
                "output_bytes": r.output_bytes,
                "single_ms": r.single_ms,
                "peak_heap_bytes": r.peak_heap_bytes,
            })
        })
        .collect();

    let throughput: Vec<_> = tp_rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "case": r.name,
                "width": r.width,
                "height": r.height,
                "megapixels": r.megapixels,
                "threads": r.threads,
                "seconds": r.seconds,
                "photos": r.photos,
                "photos_per_sec": r.photos_per_sec,
                "eff_ms_per_photo": r.eff_ms_per_photo,
            })
        })
        .collect();

    let doc = serde_json::json!({
        "phase": 0,
        "label": "baseline-image-crate",
        "cores": threads,
        "per_image": per_image,
        "throughput": throughput,
    });

    if let Err(e) = std::fs::write(
        json_path(),
        serde_json::to_string_pretty(&doc).unwrap_or_default(),
    ) {
        eprintln!("could not write {}: {e}", json_path().display());
    }
}

/// Throughput of the real service path under a decode-concurrency cap: many
/// concurrent "requests" compete for `permits` slots, each holding its permit
/// while the CPU-bound render runs on the blocking pool (exactly how
/// `generate_all_sizes_background` + `decode_semaphore` behave). Returns
/// photos/sec over `window`.
fn measure_semaphore_throughput(case: &CorpusCase, permits: usize, window: Duration) -> f64 {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let bytes = Arc::new(case.bytes.clone());
    rt.block_on(async move {
        let sem = Arc::new(tokio::sync::Semaphore::new(permits));
        let counter = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        let deadline = start + window;

        // Oversupply concurrent requests so the semaphore — not the task count —
        // is the limiter, mirroring a burst of hundreds of uploads.
        let workers = (permits * 3).max(48);
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let sem = sem.clone();
            let counter = counter.clone();
            let bytes = bytes.clone();
            handles.push(tokio::spawn(async move {
                while Instant::now() < deadline {
                    let permit = sem.clone().acquire_owned().await.expect("permit");
                    let b = bytes.clone();
                    let res =
                        tokio::task::spawn_blocking(move || ThumbnailService::bench_render_all(&b))
                            .await;
                    drop(permit); // release only after the render finishes
                    if matches!(res, Ok(Ok(_))) {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        let elapsed = start.elapsed().as_secs_f64();
        counter.load(Ordering::Relaxed) as f64 / elapsed
    })
}

// ---------------------------------------------------------------------------
// Quality verification helpers (Table C)
// ---------------------------------------------------------------------------

/// Reference thumbnail: a **full-resolution decode** + high-quality CatmullRom
/// resample + q80 JPEG encode — the original (pre-optimisation) quality target.
/// Comparing the optimised output against this gauges whether shrink-on-load +
/// SIMD resizing degrades quality. Uses the same exact target dims as production
/// (`resize_exact`) so the comparison is apples-to-apples, never a dim mismatch.
/// EXIF orientation is not applied, so only run it on orientation=1 cases.
fn reference_render_full_decode(bytes: &[u8], max_dim: u32) -> Vec<u8> {
    let img = image::load_from_memory(bytes).expect("ref full decode");
    let (ow, oh) = (img.width(), img.height());
    // Same fit-to-longest-side dims production computes (see fit_dims()).
    let (nw, nh) = if ow > oh {
        (
            max_dim,
            ((oh as f32 * (max_dim as f32 / ow as f32)) as u32).max(1),
        )
    } else {
        (
            ((ow as f32 * (max_dim as f32 / oh as f32)) as u32).max(1),
            max_dim,
        )
    };
    let rgb = img
        .resize_exact(nw, nh, image::imageops::FilterType::CatmullRom)
        .to_rgb8();
    let mut buf = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80);
    rgb.write_with_encoder(enc).expect("ref encode");
    buf
}

/// Decode a JPEG thumbnail back to an 8-bit luma plane for comparison.
fn decode_to_luma(jpeg: &[u8]) -> (Vec<u8>, u32, u32) {
    let img = image::load_from_memory(jpeg).expect("decode thumbnail");
    let luma = img.to_luma8();
    let (w, h) = (luma.width(), luma.height());
    (luma.into_raw(), w, h)
}

/// Uncompressed ground truth: full-decode the source and resize to the exact
/// thumbnail dims (CatmullRom), returning luma. Comparing each codec's decoded
/// thumbnail against this isolates codec quality (no second lossy step).
fn reference_luma_at(bytes: &[u8], w: u32, h: u32) -> Vec<u8> {
    image::load_from_memory(bytes)
        .expect("ref decode")
        .resize_exact(w, h, image::imageops::FilterType::CatmullRom)
        .to_luma8()
        .into_raw()
}

/// Mean SSIM over non-overlapping 8×8 blocks (luma). 1.0 = identical.
fn block_ssim(a: &[u8], b: &[u8], w: u32, h: u32) -> f64 {
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let (w, h) = (w as usize, h as usize);
    let bs = 8usize;
    let mut acc = 0.0;
    let mut blocks = 0.0;
    let mut by = 0;
    while by < h {
        let mut bx = 0;
        while bx < w {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab, mut n) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            for y in by..(by + bs).min(h) {
                for x in bx..(bx + bs).min(w) {
                    let ia = a[y * w + x] as f64;
                    let ib = b[y * w + x] as f64;
                    sa += ia;
                    sb += ib;
                    saa += ia * ia;
                    sbb += ib * ib;
                    sab += ia * ib;
                    n += 1.0;
                }
            }
            let (ma, mb) = (sa / n, sb / n);
            let va = (saa / n - ma * ma).max(0.0);
            let vb = (sbb / n - mb * mb).max(0.0);
            let cov = sab / n - ma * mb;
            let s = ((2.0 * ma * mb + C1) * (2.0 * cov + C2))
                / ((ma * ma + mb * mb + C1) * (va + vb + C2));
            acc += s;
            blocks += 1.0;
            bx += bs;
        }
        by += bs;
    }
    if blocks > 0.0 { acc / blocks } else { 1.0 }
}

/// Peak signal-to-noise ratio (luma). ∞ for identical inputs.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return f64::INFINITY;
    }
    let mse: f64 = a
        .iter()
        .zip(b.iter())
        .take(n)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / n as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}
