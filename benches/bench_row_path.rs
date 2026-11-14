//! PG row → entity path materialization benchmark — the per-listing-row
//! `make_file_path` split→rejoin + NFC-copy chain (ROUND3 follow-up).
//!
//! Every listing row (PROPFIND batches, photos timeline, search pages,
//! by-ids enrichment, subtree ZIP streams) used to pay this chain:
//!
//!   • files:   `format!("{fp}/{name}")` temp → `StoragePath::from_string`
//!     split (one `String` per segment + `Vec`) → constructor NFC-copies
//!     the already-NFC name → `Display`/`join` re-joins the segments it
//!     just split into `path_string` (join temp + unsized `to_string`).
//!   • folders: same minus the format temp — the materialized `path`
//!     column arrives owned, is split, dropped, and re-joined into an
//!     identical `String`.
//!
//! The optimized path builds segments + joined string in ONE pass
//! (`StoragePath::from_folder_and_name` / `from_joined`, the latter
//! reusing the owned input when canonical) and normalizes the owned name
//! without the always-copy (`normalize_storage_name_owned`).
//!
//! The OLD logic is copied verbatim into `mod before` so one binary
//! reports BEFORE vs AFTER side by side; an equivalence gate asserts
//! byte-identical (name, path_string, segments) triples — including
//! adversarial non-canonical inputs — and error parity for invalid
//! names (exit 1 on any diff).
//!
//! Sections:
//!   1. File row    wall time (p50 ns/row over BENCH_PASSES passes)
//!   2. Folder row  wall time (same)
//!   3. Alloc calls/row (counting allocator wrapping System — the lib
//!      crate sets no global allocator; mimalloc lives in main.rs only)
//!   4. Equivalence gate (realistic corpus + adversarial set)
//!
//! Run (no Postgres needed):
//!   cargo bench --features=bench --bench bench_row_path
//! Tunables (env):
//!   BENCH_ROWS (10000)   BENCH_PASSES (100)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use oxicloud::domain::entities::file::File;
use oxicloud::domain::entities::folder::Folder;
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

// ─── BEFORE: verbatim copy of the pre-optimization chain ────────────────────

/// Pre-optimization reference implementation. `OldStoragePath` +
/// `normalize_storage_name` + `make_file_path` + the constructor bodies
/// are copied byte-for-byte from the old `path_service.rs` /
/// `file.rs` / `folder.rs` / repository code so the equivalence gate
/// proves the optimized paths change nothing observable.
#[allow(clippy::all)]
mod before {
    use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};
    use uuid::Uuid;

    /// Old borrowing normalize — allocates a copy even on the NFC fast path.
    fn normalize_storage_name(name: &str) -> String {
        if is_nfc_quick(name.chars()) == IsNormalized::Yes {
            return name.to_string();
        }
        name.nfc().collect()
    }

    fn validate_storage_name(name: &str) -> Result<(), &'static str> {
        if name.is_empty() {
            return Err("name cannot be empty");
        }
        if name.contains('/') || name.contains('\\') {
            return Err("name must not contain '/' or '\\'");
        }
        if name.contains('\0') {
            return Err("name must not contain null bytes");
        }
        if name == "." || name == ".." {
            return Err("'.' and '..' are not valid names");
        }
        Ok(())
    }

    pub struct OldStoragePath {
        pub segments: Vec<String>,
    }

    impl OldStoragePath {
        fn is_safe_segment(s: &str) -> bool {
            !s.is_empty() && s != "." && s != ".." && !s.contains('/')
        }

        fn from_string(path: &str) -> Self {
            let segments = path
                .split('/')
                .filter(|s| Self::is_safe_segment(s))
                .map(|s| s.to_string())
                .collect();
            Self { segments }
        }
    }

    /// Old `Display` impl (join temp) driven through the std `ToString`
    /// blanket — the exact `storage_path.to_string()` the constructors ran.
    impl std::fmt::Display for OldStoragePath {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if self.segments.is_empty() {
                write!(f, "/")
            } else {
                write!(f, "/{}", self.segments.join("/"))
            }
        }
    }

    /// Old repository helper (identical copies lived in the read + write
    /// file repositories).
    fn make_file_path(folder_path: Option<&str>, file_name: &str) -> OldStoragePath {
        match folder_path {
            Some(fp) if !fp.is_empty() => OldStoragePath::from_string(&format!("{fp}/{file_name}")),
            _ => OldStoragePath::from_string(file_name),
        }
    }

    /// Entity-shaped product so BEFORE pays the same field moves the real
    /// constructors pay; only the path/name chain differs from AFTER.
    /// Fields exist to be *built* (cost parity), not read.
    #[allow(dead_code)]
    pub struct BeforeFile {
        pub id: String,
        pub name: String,
        pub storage_path: OldStoragePath,
        pub path_string: String,
        pub size: u64,
        pub mime_type: String,
        pub folder_id: Option<String>,
        pub created_at: u64,
        pub modified_at: u64,
        pub blob_hash: String,
        pub created_by: Option<Uuid>,
        pub updated_by: Option<Uuid>,
    }

    /// Old `row_to_file` + `File::with_timestamps_blob_hash_and_provenance`.
    #[allow(clippy::too_many_arguments)]
    pub fn file_row(
        id: String,
        name: String,
        folder_path: Option<&str>,
        size: u64,
        mime_type: String,
        folder_id: Option<String>,
        created_at: u64,
        modified_at: u64,
        blob_hash: String,
        created_by: Option<Uuid>,
        updated_by: Option<Uuid>,
    ) -> Result<BeforeFile, String> {
        let storage_path = make_file_path(folder_path, &name);

        let name = normalize_storage_name(&name);
        if let Err(reason) = validate_storage_name(&name) {
            return Err(format!("{name}: {reason}"));
        }

        // Store the path string for serialization compatibility
        let path_string = storage_path.to_string();

        Ok(BeforeFile {
            id,
            name,
            storage_path,
            path_string,
            size,
            mime_type,
            folder_id,
            created_at,
            modified_at,
            blob_hash,
            created_by,
            updated_by,
        })
    }

    #[allow(dead_code)]
    pub struct BeforeFolder {
        pub id: String,
        pub name: String,
        pub storage_path: OldStoragePath,
        pub path_string: String,
        pub parent_id: Option<String>,
        pub drive_id: Uuid,
        pub created_at: u64,
        pub modified_at: u64,
        pub tree_modified_at: u64,
        pub created_by: Option<Uuid>,
        pub updated_by: Option<Uuid>,
    }

    /// Old `row_to_folder` + `Folder::with_timestamps_tree_and_provenance`.
    #[allow(clippy::too_many_arguments)]
    pub fn folder_row(
        id: String,
        name: String,
        path: String,
        parent_id: Option<String>,
        drive_id: Uuid,
        created_at: u64,
        modified_at: u64,
        tree_modified_at: u64,
        created_by: Option<Uuid>,
        updated_by: Option<Uuid>,
    ) -> Result<BeforeFolder, String> {
        let storage_path = OldStoragePath::from_string(&path);

        let name = normalize_storage_name(&name);
        if let Err(reason) = validate_storage_name(&name) {
            return Err(format!("{name}: {reason}"));
        }

        let path_string = storage_path.to_string();

        Ok(BeforeFolder {
            id,
            name,
            storage_path,
            path_string,
            parent_id,
            drive_id,
            created_at,
            modified_at,
            tree_modified_at,
            created_by,
            updated_by,
        })
    }
}

// ─── Corpus ─────────────────────────────────────────────────────────────────

struct Row {
    id: String,
    name: String,
    folder_path: Option<String>,
    mime: String,
}

/// Deterministic LCG so runs are reproducible.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() as usize) % xs.len()]
    }
}

const SEGMENTS: &[&str] = &[
    "Personal",
    "Projects",
    "2026",
    "Q3 Reports",
    "Fotos de familia",
    "Archive",
    "Contabilidad",
    "src",
    "Diseño gráfico",
    "backup-2026-07",
];

const NAMES: &[&str] = &[
    "informe-final.pdf",
    "IMG_20260714_183042.jpg",
    "Presupuesto Q3 2026.xlsx",
    "Capture d\u{2019}\u{00E9}cran.png", // NFC accents — the common Unicode case
    "notes.md",
    "vacaciones-c\u{00F3}rdoba.mp4",
    "main.rs",
    "espa\u{00F1}ol.txt",
];

fn build_corpus(rows: usize) -> Vec<Row> {
    let mut rng = Lcg(0x0c1_f00d);
    (0..rows)
        .map(|i| {
            let depth = (rng.next() % 6) as usize; // 0..=5
            let folder_path = if depth == 0 {
                None
            } else {
                let mut p = String::new();
                for _ in 0..depth {
                    p.push('/');
                    p.push_str(rng.pick(SEGMENTS));
                }
                Some(p)
            };
            Row {
                id: Uuid::from_u128(i as u128).to_string(),
                name: format!("{}-{}", i, rng.pick(NAMES)),
                folder_path,
                mime: "application/octet-stream".to_string(),
            }
        })
        .collect()
}

fn p50(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

// ─── Runners ────────────────────────────────────────────────────────────────

fn run_file_before(corpus: &[Row]) -> before::BeforeFile {
    let mut last = None;
    for r in corpus {
        let f = before::file_row(
            r.id.clone(),
            r.name.clone(),
            r.folder_path.as_deref(),
            1234,
            r.mime.clone(),
            Some(r.id.clone()),
            1_700_000_000,
            1_750_000_000,
            "aabbccddeeff00112233445566778899".to_string(),
            None,
            None,
        )
        .expect("valid row");
        last = Some(f);
    }
    last.unwrap()
}

fn run_file_after(corpus: &[Row]) -> File {
    let mut last = None;
    for r in corpus {
        let f = File::from_materialized_row(
            r.id.clone(),
            r.name.clone(),
            r.folder_path.as_deref(),
            1234,
            r.mime.clone(),
            Some(r.id.clone()),
            1_700_000_000,
            1_750_000_000,
            "aabbccddeeff00112233445566778899".to_string(),
            None,
            None,
        )
        .expect("valid row");
        last = Some(f);
    }
    last.unwrap()
}

fn folder_full_path(r: &Row) -> String {
    match &r.folder_path {
        Some(p) => format!("{}/{}", p, r.name),
        None => format!("/{}", r.name),
    }
}

fn run_folder_before(corpus: &[Row]) -> before::BeforeFolder {
    let mut last = None;
    for r in corpus {
        let f = before::folder_row(
            r.id.clone(),
            r.name.clone(),
            folder_full_path(r),
            Some(r.id.clone()),
            Uuid::nil(),
            1_700_000_000,
            1_750_000_000,
            1_750_000_000,
            None,
            None,
        )
        .expect("valid row");
        last = Some(f);
    }
    last.unwrap()
}

fn run_folder_after(corpus: &[Row]) -> Folder {
    let mut last = None;
    for r in corpus {
        let f = Folder::from_materialized_row(
            r.id.clone(),
            r.name.clone(),
            folder_full_path(r),
            Some(r.id.clone()),
            Uuid::nil(),
            1_700_000_000,
            1_750_000_000,
            1_750_000_000,
            None,
            None,
        )
        .expect("valid row");
        last = Some(f);
    }
    last.unwrap()
}

fn time_ns_per_row<T>(passes: usize, rows: usize, mut f: impl FnMut() -> T) -> f64 {
    let mut per_pass = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t0 = Instant::now();
        black_box(f());
        per_pass.push(t0.elapsed().as_nanos() as f64 / rows as f64);
    }
    p50(per_pass)
}

fn allocs_per_row<T>(rows: usize, mut f: impl FnMut() -> T) -> f64 {
    let start = ALLOC_CALLS.load(Ordering::Relaxed);
    black_box(f());
    (ALLOC_CALLS.load(Ordering::Relaxed) - start) as f64 / rows as f64
}

// ─── Equivalence gate ───────────────────────────────────────────────────────

fn gate_file(name: &str, folder_path: Option<&str>) -> bool {
    let b = before::file_row(
        "id".into(),
        name.to_string(),
        folder_path,
        0,
        "m".into(),
        None,
        0,
        0,
        String::new(),
        None,
        None,
    );
    let a = File::from_materialized_row(
        "id".into(),
        name.to_string(),
        folder_path,
        0,
        "m".into(),
        None,
        0,
        0,
        String::new(),
        None,
        None,
    );
    match (b, a) {
        (Ok(b), Ok(a)) => {
            let seg_a: Vec<String> = a.storage_path().segments().map(str::to_string).collect();
            if b.name != a.name()
                || b.path_string != a.path_string()
                || b.storage_path.segments != seg_a
            {
                eprintln!(
                    "GATE FAIL file name={name:?} fp={folder_path:?}\n  BEFORE name={:?} path={:?} segs={:?}\n  AFTER  name={:?} path={:?} segs={:?}",
                    b.name,
                    b.path_string,
                    b.storage_path.segments,
                    a.name(),
                    a.path_string(),
                    seg_a
                );
                return false;
            }
            true
        }
        (Err(_), Err(_)) => true, // error parity
        (b, a) => {
            eprintln!(
                "GATE FAIL file name={name:?} fp={folder_path:?}: error parity broke (before_ok={} after_ok={})",
                b.is_ok(),
                a.is_ok()
            );
            false
        }
    }
}

fn gate_folder(name: &str, path: &str) -> bool {
    let b = before::folder_row(
        "id".into(),
        name.to_string(),
        path.to_string(),
        None,
        Uuid::nil(),
        0,
        0,
        0,
        None,
        None,
    );
    let a = Folder::from_materialized_row(
        "id".into(),
        name.to_string(),
        path.to_string(),
        None,
        Uuid::nil(),
        0,
        0,
        0,
        None,
        None,
    );
    match (b, a) {
        (Ok(b), Ok(a)) => {
            let seg_a: Vec<String> = a.storage_path().segments().map(str::to_string).collect();
            if b.name != a.name()
                || b.path_string != a.path_string()
                || b.storage_path.segments != seg_a
            {
                eprintln!(
                    "GATE FAIL folder name={name:?} path={path:?}\n  BEFORE name={:?} path={:?} segs={:?}\n  AFTER  name={:?} path={:?} segs={:?}",
                    b.name,
                    b.path_string,
                    b.storage_path.segments,
                    a.name(),
                    a.path_string(),
                    seg_a
                );
                return false;
            }
            true
        }
        (Err(_), Err(_)) => true,
        (b, a) => {
            eprintln!(
                "GATE FAIL folder name={name:?} path={path:?}: error parity broke (before_ok={} after_ok={})",
                b.is_ok(),
                a.is_ok()
            );
            false
        }
    }
}

fn main() {
    let rows: usize = env::var("BENCH_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let passes: usize = env::var("BENCH_PASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let corpus = build_corpus(rows);

    println!("bench_row_path — {rows} rows, {passes} passes (p50 ns/row)");
    println!();

    // Warm-up
    black_box(run_file_before(&corpus));
    black_box(run_file_after(&corpus));
    black_box(run_folder_before(&corpus));
    black_box(run_folder_after(&corpus));

    // [1] file rows
    let f_before = time_ns_per_row(passes, rows, || run_file_before(&corpus));
    let f_after = time_ns_per_row(passes, rows, || run_file_after(&corpus));
    println!("[1] File row (path chain + entity build)");
    println!("    BEFORE  {f_before:8.1} ns/row");
    println!(
        "    AFTER   {f_after:8.1} ns/row   {:.2}x",
        f_before / f_after
    );

    // [2] folder rows
    let d_before = time_ns_per_row(passes, rows, || run_folder_before(&corpus));
    let d_after = time_ns_per_row(passes, rows, || run_folder_after(&corpus));
    println!("[2] Folder row (path chain + entity build)");
    println!("    BEFORE  {d_before:8.1} ns/row");
    println!(
        "    AFTER   {d_after:8.1} ns/row   {:.2}x",
        d_before / d_after
    );

    // [3] allocs/row
    let fa_before = allocs_per_row(rows, || run_file_before(&corpus));
    let fa_after = allocs_per_row(rows, || run_file_after(&corpus));
    let da_before = allocs_per_row(rows, || run_folder_before(&corpus));
    let da_after = allocs_per_row(rows, || run_folder_after(&corpus));
    println!("[3] Alloc calls/row");
    println!("    File    BEFORE {fa_before:6.2}   AFTER {fa_after:6.2}");
    println!("    Folder  BEFORE {da_before:6.2}   AFTER {da_after:6.2}");

    // [4] equivalence gate — realistic corpus + adversarial inputs
    let mut ok = true;
    for r in &corpus {
        ok &= gate_file(&r.name, r.folder_path.as_deref());
        ok &= gate_folder(&r.name, &folder_full_path(r));
    }
    // Adversarial: non-canonical paths, traversal, NFD names, empties.
    let adversarial_files: &[(&str, Option<&str>)] = &[
        ("file.txt", None),
        ("file.txt", Some("")),
        ("file.txt", Some("/")),
        ("file.txt", Some("a//b")),
        ("file.txt", Some("/a/b/")),
        ("file.txt", Some("../etc")),
        ("file.txt", Some("a/./b")),
        ("file.txt", Some("//")),
        // NFD name (decomposed é): DB rows are NFC by invariant, but the
        // chain must stay byte-identical even for un-normalized input.
        ("cafe\u{0301}.txt", Some("/a")),
        ("", Some("/a")),           // error parity
        ("..", Some("/a")),         // error parity
        ("nul\0l.txt", Some("/a")), // error parity
        ("a\\b.txt", Some("/a")),   // error parity
    ];
    for (n, fp) in adversarial_files {
        ok &= gate_file(n, *fp);
    }
    let adversarial_folders: &[(&str, &str)] = &[
        ("Docs", "/Docs"),
        ("Docs", "Docs"),
        ("Docs", "/a//Docs"),
        ("Docs", "/a/Docs/"),
        ("Docs", "/"),
        ("Docs", ""),
        ("Docs", "/../Docs"),
        ("Doc\u{0301}s", "/a/Doc\u{0301}s"), // NFD in both
    ];
    for (n, p) in adversarial_folders {
        ok &= gate_folder(n, p);
    }
    println!(
        "[4] Equivalence gate: {}",
        if ok { "OK (byte-identical)" } else { "FAILED" }
    );

    if !ok {
        std::process::exit(1);
    }
}
