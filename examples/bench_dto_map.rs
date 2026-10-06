//! File/Folder entity → DTO mapping benchmark — per-row allocation churn.
//!
//! Isolates the variables the DTO-mapping change touches:
//!
//!   • `Arc::<str>::from(&'static str)` for the closed-set display fields
//!     (icon class, icon special class, category) — always alloc + copy —
//!     vs interned `Arc<str>` lookups (`intern_display` / `intern_mime`).
//!   • `File::compute_etag` / `Folder::compute_etag` — `chars().take(16)
//!     .collect::<String>()` + `format!` (2 allocs) vs one sized buffer.
//!   • `format_file_size` — two `format!` calls per row vs one buffer.
//!   • `Folder → FolderDto` — per-getter `.to_string()` clones + a
//!     double-allocated etag vs `into_parts()` moves.
//!
//! The OLD mapping logic is copied verbatim into `mod before` so one binary
//! reports BEFORE vs AFTER side by side, and an equivalence gate asserts the
//! two produce byte-identical DTOs for every row (exit 1 on any diff).
//!
//! Sections:
//!   1. File  → FileDto   wall time (p50 ns/row over BENCH_PASSES passes)
//!   2. Folder → FolderDto wall time (same)
//!   3. Alloc calls/row (counting global allocator wrapping System — the
//!      lib crate sets no global allocator; mimalloc lives in main.rs only,
//!      which examples do not link)
//!   4. Equivalence gate: BEFORE output == AFTER output, field by field
//!
//! Run (no Postgres needed):
//!   cargo run --release --features bench --example bench_dto_map
//! Tunables (env):
//!   BENCH_ROWS (10000)   BENCH_PASSES (100)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use oxicloud::application::dtos::file_dto::FileDto;
use oxicloud::application::dtos::folder_dto::FolderDto;
use oxicloud::domain::entities::file::File;
use oxicloud::domain::entities::folder::Folder;
use oxicloud::domain::services::path_service::StoragePath;
use uuid::Uuid;

// ─── Counting allocator (Section 3) ─────────────────────────────────────────

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

// ─── BEFORE: verbatim copy of the pre-optimization mapping logic ────────────

/// Pre-optimization reference implementation. Copied verbatim from the old
/// `From<File> for FileDto` / `From<Folder> for FolderDto` bodies, the old
/// `File::compute_etag` / `Folder::compute_etag` formulas and the old
/// `format_file_size` — kept byte-for-byte in behaviour so the equivalence
/// gate proves the optimized paths change nothing observable.
#[allow(clippy::all)]
mod before {
    use std::sync::Arc;

    use oxicloud::application::dtos::display_helpers::{
        category_for, icon_class_for, icon_special_class_for,
    };
    use oxicloud::application::dtos::file_dto::FileDto;
    use oxicloud::application::dtos::folder_dto::FolderDto;
    use oxicloud::domain::entities::file::File;
    use oxicloud::domain::entities::folder::Folder;

    /// Old `File::compute_etag`: intermediate `collect::<String>()` +
    /// `format!` — 2 allocations for one ~21-char string.
    fn file_compute_etag(blob_hash: &str, modified_at: u64) -> String {
        let prefix: String = blob_hash.chars().take(16).collect();
        format!("{}-{}", prefix, modified_at)
    }

    /// Old `Folder::compute_etag` (same shape as the file formula).
    fn folder_compute_etag(id: &str, tree_modified_at: u64) -> String {
        let prefix: String = id.chars().take(16).collect();
        format!("{}-{}", prefix, tree_modified_at)
    }

    /// Old `format_file_size`: two `format!` calls per row.
    fn format_file_size(bytes: u64) -> String {
        if bytes == 0 {
            return "0 Bytes".to_string();
        }

        const K: f64 = 1024.0;
        const SIZES: [&str; 5] = ["Bytes", "KB", "MB", "GB", "TB"];

        let i = ((bytes as f64).ln() / K.ln()).floor() as usize;
        let i = i.min(SIZES.len() - 1);

        let value = bytes as f64 / K.powi(i as i32);

        let formatted = format!("{:.2}", value);
        let formatted = formatted.trim_end_matches('0').trim_end_matches('.');

        format!("{} {}", formatted, SIZES[i])
    }

    /// Old `From<File> for FileDto` body: `Arc::from(&str)` for the three
    /// display fields and the mime type (alloc + copy each), 2-alloc etag,
    /// 2-format size string.
    pub fn file_to_dto(file: File) -> FileDto {
        let etag = file_compute_etag(file.content_hash(), file.modified_at());
        let content_hash = file.content_hash().to_string();

        let parts = file.into_parts();

        let icon_class: Arc<str> = Arc::from(icon_class_for(&parts.name, &parts.mime_type));
        let icon_special_class: Arc<str> =
            Arc::from(icon_special_class_for(&parts.name, &parts.mime_type));
        let category: Arc<str> = Arc::from(category_for(&parts.name, &parts.mime_type));
        let size_formatted = format_file_size(parts.size);
        let mime_type: Arc<str> = Arc::from(parts.mime_type.as_str());

        FileDto {
            id: parts.id,
            name: parts.name,
            path: parts.storage_path.into_joined(),
            size: parts.size,
            mime_type,
            folder_id: parts.folder_id,
            created_at: parts.created_at,
            modified_at: parts.modified_at,
            icon_class,
            icon_special_class,
            category,
            size_formatted,
            sort_date: None,
            content_hash,
            etag,
            created_by: parts.created_by,
            updated_by: parts.updated_by,
            is_favorite: false,
            is_shared: false,
        }
    }

    /// Old `From<Folder> for FolderDto` body: per-getter `.to_string()`
    /// clones, `folder.etag().to_string()` (etag built then cloned — the
    /// verbatim double alloc) and 3 fresh `Arc::from` constants per row.
    pub fn folder_to_dto(folder: Folder) -> FolderDto {
        let is_root = folder.parent_id().is_none();
        let etag = folder_compute_etag(folder.id(), folder.tree_modified_at()).to_string();

        FolderDto {
            id: folder.id().to_string(),
            name: folder.name().to_string(),
            path: folder.path_string().to_string(),
            parent_id: folder.parent_id().map(String::from),
            drive_id: folder.drive_id(),
            created_at: folder.created_at(),
            modified_at: folder.modified_at(),
            is_root,
            icon_class: Arc::from("fas fa-folder"),
            icon_special_class: Arc::from("folder-icon"),
            category: Arc::from("Folder"),
            etag,
            created_by: folder.created_by(),
            updated_by: folder.updated_by(),
            is_favorite: false,
            is_shared: false,
            mount: None,
        }
    }
}

// ─── Synthetic corpus ────────────────────────────────────────────────────────

/// (extension, mime) matrix: interned common types, generic MIMEs that
/// exercise the extension fallback, and exotic MIMEs that miss the intern
/// table so the fallback `Arc::from` path is measured too.
const KINDS: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("png", "image/png"),
    ("heic", "image/heic"),
    ("mp4", "video/mp4"),
    ("mov", "video/quicktime"),
    ("mp3", "audio/mpeg"),
    ("flac", "audio/flac"),
    ("pdf", "application/pdf"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
    ("csv", "text/csv"),
    ("json", "application/json"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    // Extension fallback: generic MIME, type resolved from the name.
    ("rs", "application/octet-stream"),
    ("py", "application/octet-stream"),
    ("svelte", "application/octet-stream"),
    ("dmg", "application/octet-stream"),
    ("bin", "application/octet-stream"),
    // No extension + empty MIME: full-default path.
    ("", ""),
    // Exotic MIMEs: miss the intern table, fall back to Arc::from.
    ("pdb", "chemical/x-pdb"),
    ("xyz", "application/x-very-exotic-subtype+custom"),
];

const SIZES: &[u64] = &[
    0,
    137,
    500,
    1_024,
    1_536,
    65_536,
    1_048_576,
    3_423_744,
    987_654_321,
    1_073_741_824,
    5_497_558_138_880, // ~5 TB
];

/// Deterministic xorshift64* — fake-but-plausible 64-char lowercase hex
/// BLAKE3 hashes.
fn next_seed(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    seed.wrapping_mul(0x2545F4914F6CDD1D)
}

fn fake_blake3(seed: &mut u64) -> String {
    format!(
        "{:016x}{:016x}{:016x}{:016x}",
        next_seed(seed),
        next_seed(seed),
        next_seed(seed),
        next_seed(seed)
    )
}

fn build_files(rows: usize) -> Vec<File> {
    let mut seed = 0x9E3779B97F4A7C15u64;
    (0..rows)
        .map(|i| {
            let (ext, mime) = KINDS[i % KINDS.len()];
            let name = if ext.is_empty() {
                format!("file_{i:05}")
            } else {
                format!("file_{i:05}.{ext}")
            };
            let path = StoragePath::from_string(&format!("/bench/dir_{}/{}", i % 37, name));
            let folder_id = if i % 3 == 0 {
                None
            } else {
                Some(Uuid::from_u128(1000 + (i % 37) as u128).to_string())
            };
            let created_by = (i % 2 == 0).then(|| Uuid::from_u128(7 + (i % 5) as u128));
            let updated_by = (i % 4 == 0).then(|| Uuid::from_u128(11 + (i % 3) as u128));
            File::with_timestamps_blob_hash_and_provenance(
                Uuid::from_u128(i as u128).to_string(),
                name,
                path,
                SIZES[i % SIZES.len()],
                mime.to_string(),
                folder_id,
                1_600_000_000 + i as u64,
                1_700_000_000 + (i as u64 * 7) % 100_000,
                fake_blake3(&mut seed),
                created_by,
                updated_by,
            )
            .expect("valid synthetic file")
        })
        .collect()
}

fn build_folders(rows: usize) -> Vec<Folder> {
    (0..rows)
        .map(|i| {
            let name = format!("folder_{i:05}");
            let path = StoragePath::from_string(&format!("/bench/parent_{}/{}", i % 37, name));
            let parent_id = if i % 5 == 0 {
                None
            } else {
                Some(Uuid::from_u128(2000 + (i % 37) as u128).to_string())
            };
            let created_by = (i % 2 == 0).then(|| Uuid::from_u128(7 + (i % 5) as u128));
            let updated_by = (i % 4 == 0).then(|| Uuid::from_u128(11 + (i % 3) as u128));
            Folder::with_timestamps_tree_and_provenance(
                Uuid::from_u128(500_000 + i as u128).to_string(),
                name,
                path,
                parent_id,
                Uuid::from_u128(42 + (i % 4) as u128),
                1_600_000_000 + i as u64,
                1_700_000_000 + (i as u64 * 7) % 100_000,
                1_700_000_000 + (i as u64 * 11) % 100_000,
                created_by,
                updated_by,
            )
            .expect("valid synthetic folder")
        })
        .collect()
}

// ─── Measurement helpers ─────────────────────────────────────────────────────

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

/// p50 wall seconds per pass of `f` over `passes` passes.
fn p50_pass_secs(passes: usize, mut f: impl FnMut()) -> f64 {
    f(); // warmup (also initializes LazyLock intern tables)
    let mut xs = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t0 = Instant::now();
        f();
        xs.push(t0.elapsed().as_secs_f64());
    }
    median(xs)
}

/// Allocation calls performed by one run of `f` (deterministic — the
/// mappings do no I/O and touch no shared caches beyond the intern tables,
/// which the warmup run already initialized).
fn allocs_of(mut f: impl FnMut()) -> u64 {
    f(); // warmup so one-time lazy init isn't attributed to the variant
    let start = ALLOC_CALLS.load(Ordering::Relaxed);
    f();
    ALLOC_CALLS.load(Ordering::Relaxed) - start
}

struct Row {
    variant: &'static str,
    ns_per_row: f64,
    allocs_per_row: f64,
}

// ─── Equivalence gate (Section 4) ────────────────────────────────────────────

macro_rules! cmp_field {
    ($diffs:expr, $i:expr, $kind:expr, $b:expr, $a:expr, $field:ident) => {
        if $b.$field != $a.$field {
            $diffs += 1;
            if $diffs <= 20 {
                println!(
                    "  DIFF {} row {}: {} BEFORE={:?} AFTER={:?}",
                    $kind,
                    $i,
                    stringify!($field),
                    $b.$field,
                    $a.$field
                );
            }
        }
    };
}

fn diff_file(i: usize, b: &FileDto, a: &FileDto, diffs: &mut u64) {
    cmp_field!(*diffs, i, "file", b, a, id);
    cmp_field!(*diffs, i, "file", b, a, name);
    cmp_field!(*diffs, i, "file", b, a, path);
    cmp_field!(*diffs, i, "file", b, a, size);
    cmp_field!(*diffs, i, "file", b, a, mime_type);
    cmp_field!(*diffs, i, "file", b, a, folder_id);
    cmp_field!(*diffs, i, "file", b, a, created_at);
    cmp_field!(*diffs, i, "file", b, a, modified_at);
    cmp_field!(*diffs, i, "file", b, a, icon_class);
    cmp_field!(*diffs, i, "file", b, a, icon_special_class);
    cmp_field!(*diffs, i, "file", b, a, category);
    cmp_field!(*diffs, i, "file", b, a, size_formatted);
    cmp_field!(*diffs, i, "file", b, a, sort_date);
    cmp_field!(*diffs, i, "file", b, a, content_hash);
    cmp_field!(*diffs, i, "file", b, a, etag);
    cmp_field!(*diffs, i, "file", b, a, created_by);
    cmp_field!(*diffs, i, "file", b, a, updated_by);
}

fn diff_folder(i: usize, b: &FolderDto, a: &FolderDto, diffs: &mut u64) {
    cmp_field!(*diffs, i, "folder", b, a, id);
    cmp_field!(*diffs, i, "folder", b, a, name);
    cmp_field!(*diffs, i, "folder", b, a, path);
    cmp_field!(*diffs, i, "folder", b, a, parent_id);
    cmp_field!(*diffs, i, "folder", b, a, drive_id);
    cmp_field!(*diffs, i, "folder", b, a, created_at);
    cmp_field!(*diffs, i, "folder", b, a, modified_at);
    cmp_field!(*diffs, i, "folder", b, a, is_root);
    cmp_field!(*diffs, i, "folder", b, a, icon_class);
    cmp_field!(*diffs, i, "folder", b, a, icon_special_class);
    cmp_field!(*diffs, i, "folder", b, a, category);
    cmp_field!(*diffs, i, "folder", b, a, etag);
    cmp_field!(*diffs, i, "folder", b, a, created_by);
    cmp_field!(*diffs, i, "folder", b, a, updated_by);
}

// ─── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let rows: usize = env_or("BENCH_ROWS", 10_000).max(1);
    let passes: usize = env_or("BENCH_PASSES", 100).max(1);

    let files = build_files(rows);
    let folders = build_folders(rows);
    println!(
        "corpus: {rows} files ({} kinds x {} sizes) + {rows} folders, {passes} timed passes",
        KINDS.len(),
        SIZES.len()
    );
    println!(
        "note: each measured pass pays one entity clone per row (mapping consumes the\n\
         entity); the clone-only baseline is measured separately and subtracted.\n"
    );

    // ── Section 1: File → FileDto wall time ─────────────────────────────
    println!("── Section 1: File → FileDto (p50 wall, net of clone) ──");
    let file_base_s = p50_pass_secs(passes, || {
        for f in &files {
            black_box(f.clone());
        }
    });
    let file_before_s = p50_pass_secs(passes, || {
        for f in &files {
            black_box(before::file_to_dto(f.clone()));
        }
    });
    let file_after_s = p50_pass_secs(passes, || {
        for f in &files {
            black_box(FileDto::from(f.clone()));
        }
    });
    let file_base_ns = file_base_s * 1e9 / rows as f64;
    let file_before_ns = (file_before_s - file_base_s) * 1e9 / rows as f64;
    let file_after_ns = (file_after_s - file_base_s) * 1e9 / rows as f64;
    println!("  clone-only baseline: {file_base_ns:8.1} ns/row");
    println!("  BEFORE mapping:      {file_before_ns:8.1} ns/row");
    println!("  AFTER  mapping:      {file_after_ns:8.1} ns/row\n");

    // ── Section 2: Folder → FolderDto wall time ─────────────────────────
    println!("── Section 2: Folder → FolderDto (p50 wall, net of clone) ──");
    let folder_base_s = p50_pass_secs(passes, || {
        for f in &folders {
            black_box(f.clone());
        }
    });
    let folder_before_s = p50_pass_secs(passes, || {
        for f in &folders {
            black_box(before::folder_to_dto(f.clone()));
        }
    });
    let folder_after_s = p50_pass_secs(passes, || {
        for f in &folders {
            black_box(FolderDto::from(f.clone()));
        }
    });
    let folder_base_ns = folder_base_s * 1e9 / rows as f64;
    let folder_before_ns = (folder_before_s - folder_base_s) * 1e9 / rows as f64;
    let folder_after_ns = (folder_after_s - folder_base_s) * 1e9 / rows as f64;
    println!("  clone-only baseline: {folder_base_ns:8.1} ns/row");
    println!("  BEFORE mapping:      {folder_before_ns:8.1} ns/row");
    println!("  AFTER  mapping:      {folder_after_ns:8.1} ns/row\n");

    // ── Section 3: allocation calls per row ─────────────────────────────
    println!("── Section 3: allocator calls per row (net of clone) ──");
    let file_base_a = allocs_of(|| {
        for f in &files {
            black_box(f.clone());
        }
    }) as f64
        / rows as f64;
    let file_before_a = allocs_of(|| {
        for f in &files {
            black_box(before::file_to_dto(f.clone()));
        }
    }) as f64
        / rows as f64
        - file_base_a;
    let file_after_a = allocs_of(|| {
        for f in &files {
            black_box(FileDto::from(f.clone()));
        }
    }) as f64
        / rows as f64
        - file_base_a;
    let folder_base_a = allocs_of(|| {
        for f in &folders {
            black_box(f.clone());
        }
    }) as f64
        / rows as f64;
    let folder_before_a = allocs_of(|| {
        for f in &folders {
            black_box(before::folder_to_dto(f.clone()));
        }
    }) as f64
        / rows as f64
        - folder_base_a;
    let folder_after_a = allocs_of(|| {
        for f in &folders {
            black_box(FolderDto::from(f.clone()));
        }
    }) as f64
        / rows as f64
        - folder_base_a;
    println!("  file clone baseline:   {file_base_a:6.2} allocs/row");
    println!("  file BEFORE mapping:   {file_before_a:6.2} allocs/row");
    println!("  file AFTER  mapping:   {file_after_a:6.2} allocs/row");
    println!("  folder clone baseline: {folder_base_a:6.2} allocs/row");
    println!("  folder BEFORE mapping: {folder_before_a:6.2} allocs/row");
    println!("  folder AFTER  mapping: {folder_after_a:6.2} allocs/row\n");

    // ── Section 4: equivalence gate ─────────────────────────────────────
    println!("── Section 4: equivalence gate (BEFORE == AFTER, field by field) ──");
    let mut diffs: u64 = 0;
    for (i, f) in files.iter().enumerate() {
        let b = before::file_to_dto(f.clone());
        let a = FileDto::from(f.clone());
        diff_file(i, &b, &a, &mut diffs);
    }
    for (i, f) in folders.iter().enumerate() {
        let b = before::folder_to_dto(f.clone());
        let a = FolderDto::from(f.clone());
        diff_folder(i, &b, &a, &mut diffs);
    }
    if diffs > 0 {
        println!("  FAILED: {diffs} field diffs between BEFORE and AFTER mappings");
        std::process::exit(1);
    }
    println!("  PASSED: {rows} files + {rows} folders map byte-identically\n");

    // ── Markdown summary ─────────────────────────────────────────────────
    let table = [
        Row {
            variant: "File→FileDto BEFORE",
            ns_per_row: file_before_ns,
            allocs_per_row: file_before_a,
        },
        Row {
            variant: "File→FileDto AFTER",
            ns_per_row: file_after_ns,
            allocs_per_row: file_after_a,
        },
        Row {
            variant: "Folder→FolderDto BEFORE",
            ns_per_row: folder_before_ns,
            allocs_per_row: folder_before_a,
        },
        Row {
            variant: "Folder→FolderDto AFTER",
            ns_per_row: folder_after_ns,
            allocs_per_row: folder_after_a,
        },
    ];
    println!("| variant | ns/row | allocs/row |");
    println!("|---|---:|---:|");
    for r in &table {
        println!(
            "| {} | {:.1} | {:.2} |",
            r.variant, r.ns_per_row, r.allocs_per_row
        );
    }
}
