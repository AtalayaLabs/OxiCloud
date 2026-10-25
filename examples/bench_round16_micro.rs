//! Round-16 CPU/alloc micro-pack (no Postgres).
//!
//! Each section is BEFORE (verbatim replica of the shipped-before shape) vs
//! AFTER (the shipped function itself where it is reachable, else a verbatim
//! replica of the shipped-after shape), with a byte/-value equivalence gate and
//! a `GATE FAIL … rollback` check that exits non-zero if the AFTER arm fails to
//! reduce allocations — the round's roll-back rule encoded into the benchmark.
//!
//!   [M1] REMOVED — it measured `intern_display`, which no longer exists.
//!        See the note where `section_intern` used to be.
//!   [M2] `build_content_disposition` — every download and every Range seek
//!        built an `encoded` String, an `ascii_safe` String, and the `format!`
//!        result: 3 allocs. The shipped fast path (all-attr-char name) and the
//!        single in-place buffer (slow path) do it in 1.
//!   [M3] `nc_href` — every NC PROPFIND/REPORT href allocated a per-segment
//!        `Vec<Cow>`, a joined String and the `format!` result. The shipped
//!        single pre-sized buffer keeps `urlencoding::encode` (identical bytes)
//!        and drops the Vec + join + format.
//!   [M4] NC preview `fileId` — the handler `collect()`ed the digit prefix into
//!        a String only to reparse it to `i64`. The shipped code parses the
//!        borrowed digit-prefix slice — 0 allocs.
//!
//! Run:
//!   cargo run --release --features bench --example bench_round16_micro
//! Tunables (env): BENCH_ITERS (200000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use oxicloud::interfaces::nextcloud::webdav_handler::nc_href;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

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
        "| {:<44} | {:>12.1} | {:>10.2} |",
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

fn gate_allocs(tag: &str, before: &Measured, after: &Measured) {
    if after.allocs_per_op >= before.allocs_per_op {
        eprintln!("GATE FAIL [{tag}]: AFTER did not reduce allocations — rollback");
        std::process::exit(1);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// [M1] Folder display constants — 3 `Arc::from` allocs vs 0 (interned clone)
// ────────────────────────────────────────────────────────────────────────────

// [M1] `section_intern` lived here. It measured `intern_display` against a
// per-row `Arc::from`, and that function no longer exists: the `category`
// field it interned is now the `Copy` one-byte `ResourceCategory` enum, so
// there is no allocation left to remove and nothing to compare. Deleted
// rather than left to rot against a function it cannot call — the same
// treatment `bench_round11_micro`'s §21 got when `DisplayClass` went.

// ────────────────────────────────────────────────────────────────────────────
// [M2] build_content_disposition — 3 allocs vs 1
// ────────────────────────────────────────────────────────────────────────────

const RFC5987_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

fn disposition_of(mime: &str, force_inline: bool) -> &'static str {
    if force_inline
        || mime.starts_with("image/")
        || mime == "application/pdf"
        || mime.starts_with("video/")
        || mime.starts_with("audio/")
    {
        "inline"
    } else {
        "attachment"
    }
}

/// BEFORE: verbatim replica of the shipped-before body — three allocations.
fn cd_before(name: &str, mime: &str, force_inline: bool) -> String {
    let disposition = disposition_of(mime, force_inline);
    let encoded = utf8_percent_encode(name, RFC5987_SET).to_string();
    let ascii_safe: String = name
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .map(|c| match c {
            '"' | '\\' => '_',
            _ => c,
        })
        .collect();
    format!("{disposition}; filename=\"{ascii_safe}\"; filename*=UTF-8''{encoded}")
}

/// AFTER: verbatim replica of the shipped `build_content_disposition`.
fn cd_after(name: &str, mime: &str, force_inline: bool) -> String {
    let disposition = disposition_of(mime, force_inline);
    let all_attr_char = name.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
    });
    if all_attr_char {
        return format!("{disposition}; filename=\"{name}\"; filename*=UTF-8''{name}");
    }
    let mut out = String::with_capacity(disposition.len() + name.len() * 4 + 32);
    out.push_str(disposition);
    out.push_str("; filename=\"");
    for c in name.chars().filter(|c| c.is_ascii_graphic() || *c == ' ') {
        out.push(match c {
            '"' | '\\' => '_',
            _ => c,
        });
    }
    out.push_str("\"; filename*=UTF-8''");
    for chunk in utf8_percent_encode(name, RFC5987_SET) {
        out.push_str(chunk);
    }
    out
}

fn section_content_disposition() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // Fast-path (all-attr-char) and slow-path (space / unicode / quote+backslash)
    // names, inline and attachment.
    let samples: &[(&str, &str, bool)] = &[
        ("report.pdf", "application/pdf", false),
        ("photo.jpg", "image/jpeg", false),
        ("My Holiday Photo.png", "image/png", false),
        ("résumé final.docx", "application/octet-stream", false),
        ("weird\"na\\me.txt", "text/plain", false),
    ];

    // Gate: byte-identical output to the old chain across every shape.
    for &(n, m, f) in samples {
        assert_eq!(
            cd_before(n, m, f),
            cd_after(n, m, f),
            "content-disposition differs for {n:?}"
        );
    }

    let before = measure(iters, || {
        for &(n, m, f) in samples {
            black_box(cd_before(black_box(n), m, f));
        }
    });
    let after = measure(iters, || {
        for &(n, m, f) in samples {
            black_box(cd_after(black_box(n), m, f));
        }
    });

    println!(
        "\n## [M2] build_content_disposition ({} names/op)",
        samples.len()
    );
    header_footer("content-disposition", &before, &after);
    gate_allocs("M2", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [M3] nc_href — Vec<Cow> + join + format! vs one pre-sized buffer
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE: verbatim replica of the shipped-before `nc_href`.
fn nc_href_before(username: &str, subpath: &str) -> String {
    let subpath = subpath.trim_matches('/');
    let encoded_user = urlencoding::encode(username);
    if subpath.is_empty() {
        format!("/remote.php/dav/files/{}/", encoded_user)
    } else {
        let encoded_segments: Vec<_> = subpath
            .split('/')
            .map(|seg| urlencoding::encode(seg))
            .collect();
        format!(
            "/remote.php/dav/files/{}/{}",
            encoded_user,
            encoded_segments.join("/")
        )
    }
}

fn section_nc_href() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let samples: &[(&str, &str)] = &[
        ("alice", ""),
        ("alice", "Documents/report.pdf"),
        ("alice", "Photos/2026/My Holiday.jpg"),
        ("bob smith", "Résumés/final draft.docx"),
        ("carol", "a/deeply/nested/folder/tree/file.txt"),
    ];

    // Gate: the shipped `nc_href` is byte-identical to the old shape.
    for &(u, sp) in samples {
        assert_eq!(
            nc_href_before(u, sp),
            nc_href(u, sp),
            "nc_href differs for {u:?}/{sp:?}"
        );
    }

    let before = measure(iters, || {
        for &(u, sp) in samples {
            black_box(nc_href_before(black_box(u), black_box(sp)));
        }
    });
    let after = measure(iters, || {
        for &(u, sp) in samples {
            black_box(nc_href(black_box(u), black_box(sp)));
        }
    });

    println!("\n## [M3] nc_href ({} hrefs/op)", samples.len());
    header_footer("nc_href", &before, &after);
    gate_allocs("M3", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [M4] NC preview fileId — collect-into-String-then-parse vs borrow-slice parse
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE: verbatim replica — allocate the digit prefix, then reparse it.
fn parse_before(file_id: &str) -> Result<i64, ()> {
    let numeric_part: String = file_id.chars().take_while(|c| c.is_ascii_digit()).collect();
    numeric_part.parse().map_err(|_| ())
}

/// AFTER: verbatim replica of the shipped code — parse the borrowed prefix.
fn parse_after(file_id: &str) -> Result<i64, ()> {
    let end = file_id
        .as_bytes()
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(file_id.len());
    file_id[..end].parse().map_err(|_| ())
}

fn section_preview_parse() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // The NC app appends an instance suffix; plus all-digit, non-digit and empty.
    let samples = ["00000326ocnca", "123456789", "42abc", "notanid", ""];

    // Gate: identical parse outcome across every shape.
    for s in samples {
        assert_eq!(
            parse_before(s),
            parse_after(s),
            "preview parse differs for {s:?}"
        );
    }

    let before = measure(iters, || {
        for s in samples {
            let _ = black_box(parse_before(black_box(s)));
        }
    });
    let after = measure(iters, || {
        for s in samples {
            let _ = black_box(parse_after(black_box(s)));
        }
    });

    println!(
        "\n## [M4] NC preview fileId parse ({} ids/op)",
        samples.len()
    );
    header_footer("preview fileId parse", &before, &after);
    gate_allocs("M4", &before, &after);
}

fn main() {
    println!("#################################################################");
    println!("# Round-16 CPU/alloc micro-pack");
    println!("#################################################################");

    section_content_disposition();
    section_nc_href();
    section_preview_parse();

    println!("\nGATE PASS (all sections)");
}
