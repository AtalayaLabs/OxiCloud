//! `/api/folders/{id}/resources` row→DTO mapping micro-alloc benchmark.
//!
//! The listing maps each `FolderResourceRow` into a `FolderResourceItemDto`.
//! BEFORE cloned `row.name` into the DTO (`name: row.name.clone()`) even
//! though the row is owned by the mapping closure — one avoidable `String`
//! heap alloc per listed folder/file. AFTER computes the name-derived icon /
//! category classes first (they borrow `&row.name`), then MOVES `row.name`
//! into the DTO — the same output, one fewer alloc per row.
//!
//! Section 2 (round 9): the SAME clone-vs-move port applied to the
//! favorites/recents listings (`/api/favorites/resources`,
//! `/api/recent/resources`), which the round-7 rewrite never reached. Their
//! per-row mapping additionally cloned `row.path` (owner rows) and
//! `row.blob_hash` (file rows), so the saving is up to 3 allocs per file row.
//! The two handlers share one mapping shape (only the `favorited_at` /
//! `accessed_at` passthrough differs), so the favorites row stands for both.
//!
//! Run:
//!   cargo run --release --features bench --example bench_resource_row_map
//! Tunables (env): BENCH_ROWS (500).

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::{DateTime, TimeZone, Utc};
use oxicloud::application::dtos::display_helpers::{
    category_for, format_file_size, icon_class_for, icon_special_class_for, intern_display,
    intern_mime,
};
use oxicloud::application::dtos::favorites_dto::FavoriteResourceRow;
use oxicloud::application::dtos::file_dto::FileDto;
use oxicloud::application::dtos::folder_dto::{FolderDto, FolderResourceRow};
use oxicloud::domain::entities::file::File;
use uuid::Uuid;

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

fn rows(n: usize) -> Vec<FolderResourceRow> {
    let ts: DateTime<Utc> = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    (0..n)
        .map(|i| {
            let is_folder = i % 4 == 0;
            FolderResourceRow {
                resource_type: if is_folder { "folder" } else { "file" }.to_string(),
                id: Uuid::new_v4(),
                name: if is_folder {
                    format!("Folder {i:05}")
                } else {
                    format!("document-{i:05}.pdf")
                },
                parent_id: Some(Uuid::new_v4()),
                mime_type: if is_folder {
                    None
                } else {
                    Some("application/pdf".to_string())
                },
                size: if is_folder { -1 } else { 4096 },
                created_at: ts,
                modified_at: ts,
                drive_id: Uuid::new_v4(),
                blob_hash: if is_folder {
                    None
                } else {
                    Some("a".repeat(64))
                },
                created_by: Some(Uuid::new_v4()),
                updated_by: Some(Uuid::new_v4()),
                is_favorite: false,
                is_shared: false,
                sort_str: format!("row {i}"),
                type_order: 0,
                folder_first: if is_folder { 0 } else { 1 },
                mount_target_id: None,
                mount_target_drive_id: None,
                mount_kind: None,
            }
        })
        .collect()
}

/// (name, icon_class, category) triple extracted from each produced DTO — the
/// fields the move-vs-clone touches. Used for the equivalence gate.
type Probe = (String, std::sync::Arc<str>, std::sync::Arc<str>);

/// BEFORE — verbatim: `name: row.name.clone()` in both branches.
fn map_before(rows: Vec<FolderResourceRow>) -> Vec<Probe> {
    rows.into_iter()
        .map(|row| {
            if row.resource_type == "folder" {
                let resource_id = row.id.to_string();
                let dto = FolderDto {
                    etag: resource_id.clone(),
                    id: resource_id,
                    name: row.name.clone(),
                    path: String::new(),
                    parent_id: row.parent_id.map(|u| u.to_string()),
                    drive_id: row.drive_id,
                    created_at: row.created_at.timestamp() as u64,
                    modified_at: row.modified_at.timestamp() as u64,
                    is_root: false,
                    icon_class: intern_display("fas fa-folder"),
                    icon_special_class: intern_display("folder-icon"),
                    category: intern_display("Folder"),
                    created_by: None,
                    updated_by: None,
                    is_favorite: false,
                    is_shared: false,
                    mount: None,
                };
                (dto.name, dto.icon_class, dto.category)
            } else {
                let mime = row
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                let size_bytes = row.size.max(0) as u64;
                let modified_at_u = row.modified_at.timestamp() as u64;
                let content_hash = row.blob_hash.clone().unwrap_or_default();
                let etag = if content_hash.is_empty() {
                    String::new()
                } else {
                    File::compute_etag(&content_hash, modified_at_u)
                };
                let dto = FileDto {
                    id: row.id.to_string(),
                    name: row.name.clone(),
                    path: String::new(),
                    size: size_bytes,
                    mime_type: intern_mime(mime),
                    folder_id: row.parent_id.map(|u| u.to_string()),
                    created_at: row.created_at.timestamp() as u64,
                    modified_at: row.modified_at.timestamp() as u64,
                    icon_class: intern_display(icon_class_for(&row.name, mime)),
                    icon_special_class: intern_display(icon_special_class_for(&row.name, mime)),
                    category: intern_display(category_for(&row.name, mime)),
                    size_formatted: format_file_size(size_bytes),
                    content_hash,
                    etag,
                    created_by: None,
                    updated_by: None,
                    is_favorite: false,
                    is_shared: false,
                };
                (dto.name, dto.icon_class, dto.category)
            }
        })
        .collect()
}

/// AFTER — icons/category first (borrow `&row.name`), then move `row.name`.
fn map_after(rows: Vec<FolderResourceRow>) -> Vec<Probe> {
    rows.into_iter()
        .map(|row| {
            if row.resource_type == "folder" {
                let resource_id = row.id.to_string();
                let dto = FolderDto {
                    etag: resource_id.clone(),
                    id: resource_id,
                    name: row.name,
                    path: String::new(),
                    parent_id: row.parent_id.map(|u| u.to_string()),
                    drive_id: row.drive_id,
                    created_at: row.created_at.timestamp() as u64,
                    modified_at: row.modified_at.timestamp() as u64,
                    is_root: false,
                    icon_class: intern_display("fas fa-folder"),
                    icon_special_class: intern_display("folder-icon"),
                    category: intern_display("Folder"),
                    created_by: None,
                    updated_by: None,
                    is_favorite: false,
                    is_shared: false,
                    mount: None,
                };
                (dto.name, dto.icon_class, dto.category)
            } else {
                let mime = row
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                let size_bytes = row.size.max(0) as u64;
                let modified_at_u = row.modified_at.timestamp() as u64;
                let content_hash = row.blob_hash.clone().unwrap_or_default();
                let etag = if content_hash.is_empty() {
                    String::new()
                } else {
                    File::compute_etag(&content_hash, modified_at_u)
                };
                let icon_class = intern_display(icon_class_for(&row.name, mime));
                let icon_special_class = intern_display(icon_special_class_for(&row.name, mime));
                let category = intern_display(category_for(&row.name, mime));
                let dto = FileDto {
                    id: row.id.to_string(),
                    name: row.name,
                    path: String::new(),
                    size: size_bytes,
                    mime_type: intern_mime(mime),
                    folder_id: row.parent_id.map(|u| u.to_string()),
                    created_at: row.created_at.timestamp() as u64,
                    modified_at: row.modified_at.timestamp() as u64,
                    icon_class,
                    icon_special_class,
                    category,
                    size_formatted: format_file_size(size_bytes),
                    content_hash,
                    etag,
                    created_by: None,
                    updated_by: None,
                    is_favorite: false,
                    is_shared: false,
                };
                (dto.name, dto.icon_class, dto.category)
            }
        })
        .collect()
}

// ── Section 2: favorites/recents row→DTO mapping (round 9 port) ─────────────

fn fav_rows(n: usize) -> Vec<FavoriteResourceRow> {
    let ts: DateTime<Utc> = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    (0..n)
        .map(|i| {
            let is_folder = i % 4 == 0;
            FavoriteResourceRow {
                resource_type: if is_folder { "folder" } else { "file" }.to_string(),
                resource_id: Uuid::new_v4(),
                name: if is_folder {
                    format!("Folder {i:05}")
                } else {
                    format!("document-{i:05}.pdf")
                },
                parent_id: Some(Uuid::new_v4()),
                mime_type: if is_folder {
                    None
                } else {
                    Some("application/pdf".to_string())
                },
                size: if is_folder { -1 } else { 4096 },
                resource_created_at: ts,
                modified_at: ts,
                drive_id: Uuid::new_v4(),
                blob_hash: if is_folder {
                    None
                } else {
                    Some("a".repeat(64))
                },
                created_by: Some(Uuid::new_v4()),
                updated_by: Some(Uuid::new_v4()),
                is_favorite: true,
                is_shared: false,
                is_owner: true,
                favorited_at: ts,
                path: Some(format!("Documents/Work/item-{i:05}")),
                sort_str: Some(format!("row {i}")),
                sort_int: None,
                sort_ts: None,
            }
        })
        .collect()
}

/// (name, path, content_hash, icon_class, category) — every field the
/// clone→move rewrite touches on the favorites/recents mapping.
type FavProbe = (
    String,
    String,
    String,
    std::sync::Arc<str>,
    std::sync::Arc<str>,
);

/// BEFORE — verbatim favorites/recents mapping: `row.path.clone()`,
/// `row.name.clone()` (both branches) and `row.blob_hash.clone()`.
fn fav_map_before(rows: Vec<FavoriteResourceRow>) -> Vec<FavProbe> {
    rows.into_iter()
        .map(|row| {
            let path = if row.is_owner {
                row.path.clone().unwrap_or_default()
            } else {
                String::new()
            };
            if row.resource_type == "folder" {
                let resource_id = row.resource_id.to_string();
                let dto = FolderDto {
                    etag: resource_id.clone(),
                    id: resource_id,
                    name: row.name.clone(),
                    path,
                    parent_id: row.parent_id.map(|u| u.to_string()),
                    drive_id: row.drive_id,
                    created_at: row.resource_created_at.timestamp() as u64,
                    modified_at: row.modified_at.timestamp() as u64,
                    is_root: false,
                    icon_class: intern_display("fas fa-folder"),
                    icon_special_class: intern_display("folder-icon"),
                    category: intern_display("Folder"),
                    created_by: None,
                    updated_by: None,
                    is_favorite: false,
                    is_shared: false,
                    mount: None,
                };
                (
                    dto.name,
                    dto.path,
                    String::new(),
                    dto.icon_class,
                    dto.category,
                )
            } else {
                let mime = row
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                let size_bytes = row.size.max(0) as u64;
                let modified_at_u = row.modified_at.timestamp() as u64;
                let content_hash = row.blob_hash.clone().unwrap_or_default();
                let etag = if content_hash.is_empty() {
                    String::new()
                } else {
                    File::compute_etag(&content_hash, modified_at_u)
                };
                let dto = FileDto {
                    id: row.resource_id.to_string(),
                    name: row.name.clone(),
                    path,
                    size: size_bytes,
                    mime_type: intern_mime(mime),
                    folder_id: row.parent_id.map(|u| u.to_string()),
                    created_at: row.resource_created_at.timestamp() as u64,
                    modified_at: modified_at_u,
                    icon_class: intern_display(icon_class_for(&row.name, mime)),
                    icon_special_class: intern_display(icon_special_class_for(&row.name, mime)),
                    category: intern_display(category_for(&row.name, mime)),
                    size_formatted: format_file_size(size_bytes),
                    content_hash,
                    etag,
                    created_by: None,
                    updated_by: None,
                    is_favorite: false,
                    is_shared: false,
                };
                (
                    dto.name,
                    dto.path,
                    dto.content_hash,
                    dto.icon_class,
                    dto.category,
                )
            }
        })
        .collect()
}

/// AFTER — the round-9 handler code: `path`/`blob_hash` moved, classes
/// computed before `row.name` moves.
fn fav_map_after(rows: Vec<FavoriteResourceRow>) -> Vec<FavProbe> {
    rows.into_iter()
        .map(|row| {
            let path = if row.is_owner {
                row.path.unwrap_or_default()
            } else {
                String::new()
            };
            if row.resource_type == "folder" {
                let resource_id = row.resource_id.to_string();
                let dto = FolderDto {
                    etag: resource_id.clone(),
                    id: resource_id,
                    name: row.name,
                    path,
                    parent_id: row.parent_id.map(|u| u.to_string()),
                    drive_id: row.drive_id,
                    created_at: row.resource_created_at.timestamp() as u64,
                    modified_at: row.modified_at.timestamp() as u64,
                    is_root: false,
                    icon_class: intern_display("fas fa-folder"),
                    icon_special_class: intern_display("folder-icon"),
                    category: intern_display("Folder"),
                    created_by: None,
                    updated_by: None,
                    is_favorite: false,
                    is_shared: false,
                    mount: None,
                };
                (
                    dto.name,
                    dto.path,
                    String::new(),
                    dto.icon_class,
                    dto.category,
                )
            } else {
                let mime = row
                    .mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                let size_bytes = row.size.max(0) as u64;
                let modified_at_u = row.modified_at.timestamp() as u64;
                let content_hash = row.blob_hash.unwrap_or_default();
                let etag = if content_hash.is_empty() {
                    String::new()
                } else {
                    File::compute_etag(&content_hash, modified_at_u)
                };
                let icon_class = intern_display(icon_class_for(&row.name, mime));
                let icon_special_class = intern_display(icon_special_class_for(&row.name, mime));
                let category = intern_display(category_for(&row.name, mime));
                let dto = FileDto {
                    id: row.resource_id.to_string(),
                    name: row.name,
                    path,
                    size: size_bytes,
                    mime_type: intern_mime(mime),
                    folder_id: row.parent_id.map(|u| u.to_string()),
                    created_at: row.resource_created_at.timestamp() as u64,
                    modified_at: modified_at_u,
                    icon_class,
                    icon_special_class,
                    category,
                    size_formatted: format_file_size(size_bytes),
                    content_hash,
                    etag,
                    created_by: None,
                    updated_by: None,
                    is_favorite: false,
                    is_shared: false,
                };
                (
                    dto.name,
                    dto.path,
                    dto.content_hash,
                    dto.icon_class,
                    dto.category,
                )
            }
        })
        .collect()
}

fn main() {
    let n: usize = env_or("BENCH_ROWS", 500);

    // Equivalence gate: identical (name, icon_class, category) for every row.
    if map_before(rows(n)) != map_after(rows(n)) {
        eprintln!("EQUIVALENCE GATE FAILED: mapping output differs");
        std::process::exit(1);
    }

    // Warm the string interner so its first-sight allocs sit outside the
    // measured windows (they're identical for both arms anyway).
    std::hint::black_box(map_before(rows(n)));
    std::hint::black_box(map_after(rows(n)));

    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    std::hint::black_box(map_before(rows(n)));
    let before_ms = t.elapsed().as_secs_f64() * 1e3;
    let before_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;

    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    std::hint::black_box(map_after(rows(n)));
    let after_ms = t.elapsed().as_secs_f64() * 1e3;
    let after_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;

    // Both arms build the same `rows(n)` input inside the timed window, so the
    // input allocs are equal and cancel in the delta; the difference is the
    // per-row name clone the AFTER path avoids.
    println!("\n#################################################################");
    println!("# resources row→DTO mapping: clone name vs move name");
    println!("# rows={n}");
    println!("#################################################################\n");
    println!(
        "| {:<20} | {:>12} | {:>10} | {:>14} |",
        "arm", "allocs", "wall ms", "allocs/row"
    );
    println!(
        "| {:<20} | {:>12} | {:>10.3} | {:>14.3} |",
        "BEFORE (clone)",
        before_allocs,
        before_ms,
        before_allocs as f64 / n as f64
    );
    println!(
        "| {:<20} | {:>12} | {:>10.3} | {:>14.3} |",
        "AFTER (move)",
        after_allocs,
        after_ms,
        after_allocs as f64 / n as f64
    );
    println!(
        "\nSaved {} allocs ({:.2}/row) — the per-row name clone removed.",
        before_allocs.saturating_sub(after_allocs),
        (before_allocs.saturating_sub(after_allocs)) as f64 / n as f64
    );

    // ── Section 2: favorites/recents mapping (round-9 port) ────────────────
    if fav_map_before(fav_rows(n)) != fav_map_after(fav_rows(n)) {
        eprintln!("EQUIVALENCE GATE FAILED: favorites mapping output differs");
        std::process::exit(1);
    }
    std::hint::black_box(fav_map_before(fav_rows(n)));
    std::hint::black_box(fav_map_after(fav_rows(n)));

    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    std::hint::black_box(fav_map_before(fav_rows(n)));
    let fb_ms = t.elapsed().as_secs_f64() * 1e3;
    let fb_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a0;

    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    std::hint::black_box(fav_map_after(fav_rows(n)));
    let fa_ms = t.elapsed().as_secs_f64() * 1e3;
    let fa_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - a1;

    println!("\n#################################################################");
    println!("# [2] favorites/recents row→DTO mapping: clone path+name+hash vs move");
    println!("# rows={n} (same mapping shape in both handlers)");
    println!("#################################################################\n");
    println!(
        "| {:<20} | {:>12} | {:>10} | {:>14} |",
        "arm", "allocs", "wall ms", "allocs/row"
    );
    println!(
        "| {:<20} | {:>12} | {:>10.3} | {:>14.3} |",
        "BEFORE (clone)",
        fb_allocs,
        fb_ms,
        fb_allocs as f64 / n as f64
    );
    println!(
        "| {:<20} | {:>12} | {:>10.3} | {:>14.3} |",
        "AFTER (move)",
        fa_allocs,
        fa_ms,
        fa_allocs as f64 / n as f64
    );
    println!(
        "\nSaved {} allocs ({:.2}/row) — path + name + blob_hash clones removed.",
        fb_allocs.saturating_sub(fa_allocs),
        (fb_allocs.saturating_sub(fa_allocs)) as f64 / n as f64
    );
    if fa_allocs >= fb_allocs {
        eprintln!("GATE FAIL: AFTER allocs not below BEFORE — rollback");
        std::process::exit(1);
    }
}
