//! Round-15 CPU/alloc micro-pack (no Postgres).
//!
//! Each section is BEFORE (verbatim replica of the shipped-before shape) vs
//! AFTER (the shipped-after shape, or the shipped function itself), with an
//! equivalence gate and a `GATE FAIL … rollback` check that exits non-zero if
//! the AFTER arm fails to beat its BEFORE — the round's roll-back rule encoded
//! into the benchmark.
//!
//!   [B1] exif Make/Model — `display_value().to_string().trim_matches('"')
//!        .trim().to_string()` allocates the display String, then throws it away
//!        to allocate the trimmed copy (2 allocs). The shipped
//!        `exif_service::display_value_trimmed` trims in place on the owned
//!        buffer (`drain` + `truncate`) — 1 alloc. Per ingested photo.
//!   [B2] content-index worker `supports()` — `text_extractor::supports`
//!        (lowercases the MIME + extension, 1–2 allocs) was called TWICE per
//!        file per drain batch: once in the wanted-hashes filter, once in the
//!        records loop. The shipped code classifies each file once into a
//!        `Vec<bool>` and threads it through both. Per reseed batch (every
//!        file in the library).
//!
//! Run:
//!   cargo bench --features=bench --bench bench_round15_micro
//! Tunables (env): BENCH_ITERS (200000), BENCH_BATCH (256)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use oxicloud::infrastructure::services::search_index::text_extractor;

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

struct Measured {
    wall_ns_per_op: f64,
    allocs_per_op: f64,
}

fn measure<F: FnMut()>(iters: usize, mut f: F) -> Measured {
    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let wall = t.elapsed().as_nanos() as f64 / iters as f64;
    let allocs = (ALLOC_CALLS.load(Ordering::Relaxed) - a0) as f64 / iters as f64;
    Measured {
        wall_ns_per_op: wall,
        allocs_per_op: allocs,
    }
}

fn print_row(label: &str, m: &Measured) {
    println!(
        "| {:<42} | {:>12.1} | {:>10.2} |",
        label, m.wall_ns_per_op, m.allocs_per_op
    );
}

fn header_footer(name: &str, before: &Measured, after: &Measured) {
    println!("| arm | ns/op | allocs/op |");
    print_row(&format!("BEFORE {name}"), before);
    print_row(&format!("AFTER  {name}"), after);
    println!(
        "# {:.2}x wall, {:.2} fewer allocs/op",
        before.wall_ns_per_op / after.wall_ns_per_op,
        before.allocs_per_op - after.allocs_per_op
    );
}

// ────────────────────────────────────────────────────────────────────────────
// [B1] exif Make/Model trim — 2 allocs (throwaway display String) vs 1 (in place)
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE: the shipped-before chain. `raw` stands in for the field's rendered
/// display value; `to_string()` mirrors `display_value().to_string()` (the one
/// unavoidable alloc), then `.trim_matches('"').trim().to_string()` allocates a
/// second time for the trimmed copy.
fn trim_before(raw: &str) -> String {
    raw.to_string().trim_matches('"').trim().to_string()
}

/// AFTER: verbatim replica of `exif_service::display_value_trimmed` — trims in
/// place on the already-owned buffer, so only the display String is allocated.
fn trim_after(raw: &str) -> String {
    let mut s = raw.to_string();
    let trimmed = s.trim_matches('"').trim();
    let start = trimmed.as_ptr().addr() - s.as_ptr().addr();
    let len = trimmed.len();
    s.drain(..start);
    s.truncate(len);
    s
}

fn section_exif_trim() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // Representative EXIF Make/Model display values: the widely-seen quoted
    // form, plus a padded one and an already-clean one.
    let samples = ["\"Canon\"", "\"NIKON CORPORATION\"", "  Apple  ", "SONY"];

    // Gate: byte-identical output to the old chain across every shape.
    for s in samples {
        assert_eq!(trim_before(s), trim_after(s), "trim differs for {s:?}");
    }

    let before = measure(iters, || {
        for s in samples {
            black_box(trim_before(black_box(s)));
        }
    });
    let after = measure(iters, || {
        for s in samples {
            black_box(trim_after(black_box(s)));
        }
    });

    println!("\n## [B1] exif Make/Model trim (4 sample values/op)");
    header_footer("exif trim", &before, &after);
    if after.allocs_per_op >= before.allocs_per_op {
        eprintln!("GATE FAIL [B1]: in-place trim did not reduce allocations — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [B2] content-index worker supports() — 2× per file vs 1× (memoized)
// ────────────────────────────────────────────────────────────────────────────

/// One drained file row: (name, mime, size). Mirrors the worker's
/// `FileIndexRow` projection (only the fields `supports` + the size gate read).
struct FileRow {
    name: &'static str,
    mime: &'static str,
    size: i64,
}

fn corpus(n: usize) -> Vec<FileRow> {
    // A realistic reseed mix: text/markdown/pdf/office (supported) interleaved
    // with images/video/binaries (unsupported — the fast reject).
    const MIX: &[(&str, &str, i64)] = &[
        ("notes.txt", "text/plain", 4_000),
        ("readme.md", "text/markdown", 8_000),
        ("report.pdf", "application/pdf", 250_000),
        ("photo.jpg", "image/jpeg", 3_000_000),
        (
            "sheet.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            120_000,
        ),
        ("clip.mp4", "video/mp4", 40_000_000),
        ("data.bin", "application/octet-stream", 1_000),
        ("page.html", "text/html; charset=utf-8", 20_000),
    ];
    (0..n)
        .map(|i| {
            let (name, mime, size) = MIX[i % MIX.len()];
            FileRow { name, mime, size }
        })
        .collect()
}

fn section_supports() {
    let iters: usize = env_or("BENCH_ITERS", 200_000) / 20; // heavier op
    let batch: usize = env_or("BENCH_BATCH", 256);
    let max_bytes: u64 = 10 * 1024 * 1024;
    let files = corpus(batch);

    // BEFORE: `supports` is evaluated in the wanted-hashes filter AND again per
    // file in the records loop — twice per file.
    let run_before = |files: &[FileRow]| -> (usize, usize) {
        let wanted = files
            .iter()
            .filter(|f| text_extractor::supports(f.name, f.mime) && f.size as u64 <= max_bytes)
            .count();
        let mut supported_files = 0;
        for f in files {
            if text_extractor::supports(f.name, f.mime) {
                supported_files += 1;
            }
        }
        (wanted, supported_files)
    };

    // AFTER: classify each file once into a `Vec<bool>`; both the filter and the
    // records loop read the flag.
    let run_after = |files: &[FileRow]| -> (usize, usize) {
        let supported: Vec<bool> = files
            .iter()
            .map(|f| text_extractor::supports(f.name, f.mime))
            .collect();
        let wanted = files
            .iter()
            .zip(&supported)
            .filter(|&(f, s)| *s && f.size as u64 <= max_bytes)
            .count();
        let mut supported_files = 0;
        for (_, &s) in files.iter().zip(&supported) {
            if s {
                supported_files += 1;
            }
        }
        (wanted, supported_files)
    };

    // Gate: identical (wanted, supported) tallies.
    assert_eq!(
        run_before(&files),
        run_after(&files),
        "supports tally differs"
    );

    let before = measure(iters, || {
        black_box(run_before(black_box(&files)));
    });
    let after = measure(iters, || {
        black_box(run_after(black_box(&files)));
    });

    println!("\n## [B2] content-index supports() ({batch} files/batch)");
    header_footer("supports/batch", &before, &after);
    if after.allocs_per_op >= before.allocs_per_op || after.wall_ns_per_op >= before.wall_ns_per_op
    {
        eprintln!("GATE FAIL [B2]: single-classify did not beat the double call — rollback");
        std::process::exit(1);
    }
}

fn main() {
    println!("#################################################################");
    println!("# Round-15 CPU/alloc micro-pack");
    println!("#################################################################");

    section_exif_trim();
    section_supports();

    println!("\nGATE PASS (all sections)");
}
