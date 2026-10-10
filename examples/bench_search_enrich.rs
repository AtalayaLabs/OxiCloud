//! Search-result enrichment benchmark — borrow+clone+reclassify vs consume.
//!
//! `SearchService::enrich_file` took `&FileDto`, cloned every owned `String`
//! out of it (id/name/path/folder_id/content_hash), allocated fresh `String`s
//! for `mime_type` + the three display fields, and RE-RAN the three display
//! classifiers (`icon_class_for` / `icon_special_class_for` / `category_for`)
//! whose results the `FileDto` already carried interned (`Arc<str>`, computed
//! once in `FileDto::from`). The recursive search branch runs this map over
//! the ENTIRE pre-pagination match set, so a subtree query matching thousands
//! of files paid ~11 allocs + 3 classifier passes per row. `enrich_folder`
//! cloned its 4 strings the same way, and the NC REPORT conversion
//! (`file_dto_from_search`) re-ran all three classifiers a SECOND time per
//! emitted row.
//!
//! Round 9 changes `SearchFileResultDto.{mime_type,icon_class,
//! icon_special_class,category}` to `Arc<str>`, makes both enrichers consume
//! their DTO (strings move, interned fields transfer as refcount bumps), and
//! has the NC conversion reuse the carried values.
//!
//! `mod before` holds the pre-round-9 logic verbatim (old struct shape
//! included); the equivalence gate asserts field-by-field identical output
//! for every row, and the NC-conversion gate asserts the reused display
//! fields byte-equal a fresh classifier run.
//!
//! Sections:
//!   1. enrich_file   — ns/row + allocs/row, BEFORE vs AFTER
//!   2. enrich_folder — ns/row + allocs/row, BEFORE vs AFTER
//!   3. NC REPORT search→FileDto conversion — allocs/row, BEFORE vs AFTER
//!
//! Run (no Postgres needed):
//!   cargo run --release --features bench --example bench_search_enrich
//! Tunables (env): BENCH_ROWS (10000), BENCH_PASSES (50)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use oxicloud::application::dtos::file_dto::FileDto;
use oxicloud::application::dtos::folder_dto::FolderDto;
use oxicloud::application::services::search_service::SearchService;

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

// ─── BEFORE: verbatim pre-round-9 logic ─────────────────────────────────────

#[allow(clippy::all)]
mod before {
    use oxicloud::application::dtos::display_helpers::{
        category_for, format_file_size, icon_class_for, icon_special_class_for,
    };
    use oxicloud::application::dtos::file_dto::FileDto;
    use oxicloud::application::dtos::folder_dto::FolderDto;
    use oxicloud::domain::entities::file::File;

    /// Old `SearchFileResultDto` shape — all-String display fields.
    pub struct OldSearchFileResultDto {
        pub id: String,
        pub name: String,
        pub path: String,
        pub size: u64,
        pub mime_type: String,
        pub folder_id: Option<String>,
        pub created_at: u64,
        pub modified_at: u64,
        pub relevance_score: u32,
        pub size_formatted: String,
        pub icon_class: String,
        pub icon_special_class: String,
        pub category: String,
        pub blob_hash: String,
        pub snippet: Option<String>,
        pub match_source: Option<String>,
    }

    pub struct OldSearchFolderResultDto {
        pub id: String,
        pub name: String,
        pub path: String,
        pub parent_id: Option<String>,
        pub drive_id: uuid::Uuid,
        pub created_at: u64,
        pub modified_at: u64,
        pub is_root: bool,
        pub relevance_score: u32,
    }

    // Verbatim copies of the old private helpers.
    fn get_icon_class(name: &str, mime: &str) -> String {
        icon_class_for(name, mime).to_string()
    }
    fn get_icon_special_class(name: &str, mime: &str) -> String {
        icon_special_class_for(name, mime).to_string()
    }
    fn get_category(name: &str, mime: &str) -> String {
        category_for(name, mime).to_string()
    }

    /// Verbatim copy of the service's private `format_bytes` (unchanged by
    /// round 9; the equivalence gate asserts it still matches production).
    pub fn format_bytes(bytes: u64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
        if bytes == 0 {
            return "0 B".to_string();
        }
        let exp = (bytes as f64).log(1024.0).floor() as usize;
        let exp = exp.min(UNITS.len() - 1);
        let value = bytes as f64 / 1024_f64.powi(exp as i32);
        if exp == 0 {
            format!("{} B", bytes)
        } else {
            format!("{:.1} {}", value, UNITS[exp])
        }
    }

    /// Verbatim copy of the service's private `compute_relevance` (unchanged
    /// by round 9; the equivalence gate asserts it still matches production).
    pub fn compute_relevance(name: &str, query_lower: &str) -> u32 {
        let name_lower = name.to_lowercase();

        if name_lower == query_lower {
            100
        } else if name_lower.starts_with(query_lower) {
            80
        } else if name_lower.contains(query_lower) {
            // Bonus for shorter names (more specific match)
            let ratio = query_lower.len() as f64 / name_lower.len() as f64;
            50 + (ratio * 20.0) as u32
        } else {
            0
        }
    }

    /// Verbatim old `enrich_file` (borrowing, cloning, re-classifying).
    pub fn enrich_file(file: &FileDto, query_lower: &str) -> OldSearchFileResultDto {
        let relevance = if query_lower.is_empty() {
            50
        } else {
            compute_relevance(&file.name, query_lower)
        };

        OldSearchFileResultDto {
            id: file.id.clone(),
            name: file.name.clone(),
            path: file.path.clone(),
            size: file.size,
            mime_type: file.mime_type.to_string(),
            folder_id: file.folder_id.clone(),
            created_at: file.created_at,
            modified_at: file.modified_at,
            relevance_score: relevance,
            size_formatted: format_bytes(file.size),
            icon_class: get_icon_class(&file.name, &file.mime_type),
            icon_special_class: get_icon_special_class(&file.name, &file.mime_type),
            category: get_category(&file.name, &file.mime_type),
            blob_hash: file.content_hash.clone(),
            snippet: None,
            match_source: (!query_lower.is_empty() && relevance > 0).then(|| "name".to_string()),
        }
    }

    /// Verbatim old `enrich_folder`.
    pub fn enrich_folder(folder: &FolderDto, query_lower: &str) -> OldSearchFolderResultDto {
        let relevance = if query_lower.is_empty() {
            50
        } else {
            compute_relevance(&folder.name, query_lower)
        };

        OldSearchFolderResultDto {
            id: folder.id.clone(),
            name: folder.name.clone(),
            path: folder.path.clone(),
            parent_id: folder.parent_id.clone(),
            drive_id: folder.drive_id,
            created_at: folder.created_at,
            modified_at: folder.modified_at,
            is_root: folder.is_root,
            relevance_score: relevance,
        }
    }

    /// Verbatim old NC REPORT `file_dto_from_search` body (String-field
    /// input shape) — re-runs all three classifiers per converted row.
    pub fn file_dto_from_search(fr: &OldSearchFileResultDto) -> FileDto {
        let etag = if fr.blob_hash.is_empty() {
            String::new()
        } else {
            File::compute_etag(&fr.blob_hash, fr.modified_at)
        };
        FileDto {
            id: fr.id.clone(),
            name: fr.name.clone(),
            path: fr.path.clone(),
            size: fr.size,
            mime_type: fr.mime_type.clone().into(),
            folder_id: fr.folder_id.clone(),
            created_at: fr.created_at,
            modified_at: fr.modified_at,
            icon_class: icon_class_for(&fr.name, &fr.mime_type).to_string().into(),
            icon_special_class: icon_special_class_for(&fr.name, &fr.mime_type)
                .to_string()
                .into(),
            category: category_for(&fr.name, &fr.mime_type).to_string().into(),
            size_formatted: format_file_size(fr.size),
            content_hash: fr.blob_hash.clone(),
            etag,
            created_by: None,
            updated_by: None,
            is_favorite: false,
            is_shared: false,
        }
    }
}

// ─── Fixture ────────────────────────────────────────────────────────────────

const NAMES: [(&str, &str); 5] = [
    ("report-{i}.pdf", "application/pdf"),
    ("photo-{i}.jpg", "image/jpeg"),
    ("notes-{i}.txt", "text/plain"),
    ("track-{i}.mp3", "audio/mpeg"),
    ("data-{i}.bin", "application/octet-stream"),
];

fn file_dtos(n: usize) -> Vec<FileDto> {
    (0..n)
        .map(|i| {
            let (name_t, mime) = NAMES[i % NAMES.len()];
            let name = name_t.replace("{i}", &format!("{i:05}"));
            let file = oxicloud::domain::entities::file::File::from_materialized_row(
                uuid::Uuid::new_v4().to_string(),
                name,
                Some("Documents/Work"),
                4096 + i as u64,
                mime.to_string(),
                Some(uuid::Uuid::new_v4().to_string()),
                1_700_000_000,
                1_700_000_100,
                "a".repeat(64),
                None,
                None,
            )
            .expect("fixture file");
            FileDto::from(file)
        })
        .collect()
}

fn folder_dtos(n: usize) -> Vec<FolderDto> {
    (0..n)
        .map(|i| FolderDto {
            id: uuid::Uuid::new_v4().to_string(),
            name: format!("Folder {i:05}"),
            path: format!("Documents/Folder-{i:05}"),
            parent_id: Some(uuid::Uuid::new_v4().to_string()),
            drive_id: uuid::Uuid::new_v4(),
            created_at: 1_700_000_000,
            modified_at: 1_700_000_100,
            is_root: false,
            etag: format!("{i:032x}"),
            icon_class: Arc::from("fas fa-folder"),
            icon_special_class: Arc::from("folder-icon"),
            category: Arc::from("Folder"),
            created_by: None,
            updated_by: None,
            is_favorite: false,
            is_shared: false,
            mount: None,
        })
        .collect()
}

fn p50(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let n: usize = env_or("BENCH_ROWS", 10_000);
    let passes: usize = env_or("BENCH_PASSES", 50);
    let query_lower = "report";

    // ── Equivalence gate: field-by-field identical enrichment ───────────────
    {
        let dtos = file_dtos(500);
        for dto in &dtos {
            let old = before::enrich_file(dto, query_lower);
            let new = SearchService::enrich_file_for_bench(dto.clone(), query_lower);
            let same = old.id == new.id
                && old.name == new.name
                && old.path == new.path
                && old.size == new.size
                && old.mime_type == *new.mime_type
                && old.folder_id == new.folder_id
                && old.created_at == new.created_at
                && old.modified_at == new.modified_at
                && old.relevance_score == new.relevance_score
                && old.size_formatted == new.size_formatted
                && old.icon_class == *new.icon_class
                && old.icon_special_class == *new.icon_special_class
                && old.category == *new.category
                && old.blob_hash == new.blob_hash
                && old.snippet == new.snippet
                && old.match_source == new.match_source;
            if !same {
                eprintln!("EQUIVALENCE GATE FAILED (file): {} differs", old.name);
                std::process::exit(1);
            }
        }
        let folders = folder_dtos(500);
        for dto in &folders {
            let old = before::enrich_folder(dto, query_lower);
            let new = SearchService::enrich_folder_for_bench(dto.clone(), query_lower);
            let same = old.id == new.id
                && old.name == new.name
                && old.path == new.path
                && old.parent_id == new.parent_id
                && old.drive_id == new.drive_id
                && old.created_at == new.created_at
                && old.modified_at == new.modified_at
                && old.is_root == new.is_root
                && old.relevance_score == new.relevance_score;
            if !same {
                eprintln!("EQUIVALENCE GATE FAILED (folder): {} differs", old.name);
                std::process::exit(1);
            }
        }
        println!("# equivalence gate: 500 files + 500 folders field-identical — OK");
    }

    // ── NC REPORT conversion gate: carried display fields == fresh run ──────
    {
        let dtos = file_dtos(500);
        for dto in dtos {
            let old_row = before::enrich_file(&dto, "");
            let new_row = SearchService::enrich_file_for_bench(dto, "");
            let old_conv = before::file_dto_from_search(&old_row);
            let new_conv =
                oxicloud::interfaces::nextcloud::report_handler::file_dto_from_search_for_bench(
                    &new_row,
                );
            let same = old_conv.id == new_conv.id
                && old_conv.name == new_conv.name
                && old_conv.mime_type == new_conv.mime_type
                && old_conv.icon_class == new_conv.icon_class
                && old_conv.icon_special_class == new_conv.icon_special_class
                && old_conv.category == new_conv.category
                && old_conv.size_formatted == new_conv.size_formatted
                && old_conv.etag == new_conv.etag
                && old_conv.content_hash == new_conv.content_hash;
            if !same {
                eprintln!("NC CONVERSION GATE FAILED: {} differs", old_conv.name);
                std::process::exit(1);
            }
        }
        println!("# NC REPORT conversion gate: 500 rows field-identical — OK");
    }

    // ── Section 1: enrich_file wall + allocs ────────────────────────────────
    let mut before_wall = Vec::with_capacity(passes);
    let mut after_wall = Vec::with_capacity(passes);
    let mut before_allocs = 0u64;
    let mut after_allocs = 0u64;

    for pass in 0..passes {
        // BEFORE consumes borrowed rows: reuse one input set per pass, built
        // outside the measured window (both arms see identical inputs).
        let input = file_dtos(n);

        let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
        let t = Instant::now();
        let out: Vec<_> = input
            .iter()
            .map(|f| before::enrich_file(f, query_lower))
            .collect();
        before_wall.push(t.elapsed().as_secs_f64() * 1e9 / n as f64);
        if pass == 0 {
            before_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;
        }
        black_box(&out);
        drop(out);

        let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
        let t = Instant::now();
        let out: Vec<_> = input
            .into_iter()
            .map(|f| SearchService::enrich_file_for_bench(f, query_lower))
            .collect();
        after_wall.push(t.elapsed().as_secs_f64() * 1e9 / n as f64);
        if pass == 0 {
            after_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;
        }
        black_box(&out);
    }

    println!("\n#################################################################");
    println!("# [1] enrich_file — borrow+clone+reclassify vs consume");
    println!("# rows={n} passes={passes} (p50 of per-pass ns/row; allocs from pass 0)");
    println!("#################################################################\n");
    println!(
        "| {:<22} | {:>10} | {:>12} | {:>12} |",
        "arm", "ns/row", "allocs", "allocs/row"
    );
    println!(
        "| {:<22} | {:>10.1} | {:>12} | {:>12.3} |",
        "BEFORE (borrow+clone)",
        p50(before_wall.clone()),
        before_allocs,
        before_allocs as f64 / n as f64
    );
    println!(
        "| {:<22} | {:>10.1} | {:>12} | {:>12.3} |",
        "AFTER (consume)",
        p50(after_wall.clone()),
        after_allocs,
        after_allocs as f64 / n as f64
    );
    let s1_ok = after_allocs < before_allocs;

    // ── Section 2: enrich_folder ────────────────────────────────────────────
    let mut fb_wall = Vec::with_capacity(passes);
    let mut fa_wall = Vec::with_capacity(passes);
    let mut fb_allocs = 0u64;
    let mut fa_allocs = 0u64;
    for pass in 0..passes {
        let input = folder_dtos(n);

        let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
        let t = Instant::now();
        let out: Vec<_> = input
            .iter()
            .map(|f| before::enrich_folder(f, query_lower))
            .collect();
        fb_wall.push(t.elapsed().as_secs_f64() * 1e9 / n as f64);
        if pass == 0 {
            fb_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;
        }
        black_box(&out);
        drop(out);

        let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
        let t = Instant::now();
        let out: Vec<_> = input
            .into_iter()
            .map(|f| SearchService::enrich_folder_for_bench(f, query_lower))
            .collect();
        fa_wall.push(t.elapsed().as_secs_f64() * 1e9 / n as f64);
        if pass == 0 {
            fa_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;
        }
        black_box(&out);
    }

    println!("\n#################################################################");
    println!("# [2] enrich_folder — borrow+clone vs consume");
    println!("#################################################################\n");
    println!(
        "| {:<22} | {:>10} | {:>12} | {:>12} |",
        "arm", "ns/row", "allocs", "allocs/row"
    );
    println!(
        "| {:<22} | {:>10.1} | {:>12} | {:>12.3} |",
        "BEFORE (borrow+clone)",
        p50(fb_wall.clone()),
        fb_allocs,
        fb_allocs as f64 / n as f64
    );
    println!(
        "| {:<22} | {:>10.1} | {:>12} | {:>12.3} |",
        "AFTER (consume)",
        p50(fa_wall.clone()),
        fa_allocs,
        fa_allocs as f64 / n as f64
    );
    let s2_ok = fa_allocs < fb_allocs;

    // ── Section 3: NC REPORT conversion ─────────────────────────────────────
    let conv_n = n.min(5_000);
    let old_rows: Vec<_> = file_dtos(conv_n)
        .iter()
        .map(|f| before::enrich_file(f, ""))
        .collect();
    let new_rows: Vec<_> = file_dtos(conv_n)
        .into_iter()
        .map(|f| SearchService::enrich_file_for_bench(f, ""))
        .collect();

    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    let out: Vec<_> = old_rows.iter().map(before::file_dto_from_search).collect();
    let conv_before_ms = t.elapsed().as_secs_f64() * 1e3;
    let conv_before_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;
    black_box(&out);
    drop(out);

    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    let out: Vec<_> = new_rows
        .iter()
        .map(oxicloud::interfaces::nextcloud::report_handler::file_dto_from_search_for_bench)
        .collect();
    let conv_after_ms = t.elapsed().as_secs_f64() * 1e3;
    let conv_after_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;
    black_box(&out);

    println!("\n#################################################################");
    println!("# [3] NC REPORT search→FileDto conversion — reclassify vs carry");
    println!("# rows={conv_n}");
    println!("#################################################################\n");
    println!(
        "| {:<22} | {:>10} | {:>12} | {:>12} |",
        "arm", "wall ms", "allocs", "allocs/row"
    );
    println!(
        "| {:<22} | {:>10.3} | {:>12} | {:>12.3} |",
        "BEFORE (reclassify)",
        conv_before_ms,
        conv_before_allocs,
        conv_before_allocs as f64 / conv_n as f64
    );
    println!(
        "| {:<22} | {:>10.3} | {:>12} | {:>12.3} |",
        "AFTER (carry Arc)",
        conv_after_ms,
        conv_after_allocs,
        conv_after_allocs as f64 / conv_n as f64
    );
    let s3_ok = conv_after_allocs < conv_before_allocs;

    if !(s1_ok && s2_ok && s3_ok) {
        eprintln!("\nGATE FAIL: allocs not reduced (s1={s1_ok} s2={s2_ok} s3={s3_ok}) — rollback");
        std::process::exit(1);
    }
    println!("\nGATE PASS: allocs reduced in all three sections; outputs field-identical.");
}
