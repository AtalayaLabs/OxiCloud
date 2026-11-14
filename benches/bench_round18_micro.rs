//! Round-18 calendar-event edit CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–17: each section is BEFORE (verbatim replica of the
//! shipped-before shape) vs AFTER (verbatim replica of the shipped-after
//! shape), with a byte-for-byte equivalence gate and a `GATE FAIL … rollback`
//! check that exits non-zero if the AFTER arm fails to reduce allocations — the
//! round's roll-back rule encoded into the benchmark.
//!
//!   [C1] `CalendarEvent::update_ical_property` / `remove_ical_property`
//!        rewrote the ENTIRE `ical_data` body with `format!("{}{}{}")` on every
//!        call, and allocated TWO search needles per call (`\nNAME:` and the
//!        redundant `\r\nNAME:` — the CRLF form can never match where the LF
//!        form doesn't, since `\nNAME:` is its suffix). Because
//!        `calendar_storage_adapter::update_event` applies each changed field
//!        independently, a multi-field REST edit paid one full-body (up to
//!        ~11 KB) String allocation PER changed property, plus two needles.
//!        The shipped-after form mutates the body in place (`replace_range` for
//!        an existing property, four `insert`/`insert_str` for a new one) and
//!        builds the single `\nNAME:` needle on the stack — zero heap needle,
//!        no fresh-body allocation. The edited spans are byte-for-byte the same
//!        the `format!` reconstruction produced (`replace_range(a..b, v)` ≡
//!        `before + v + after`), so the emitted body is identical — including
//!        the pre-existing quirk that editing a CRLF line drops its `\r`.
//!
//! Run:
//!   cargo bench --features=bench --bench bench_round18_micro
//! Tunables (env): BENCH_ITERS (200000)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

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
// [C1] calendar-event edit — per-property full-body format! vs in-place edit
// ────────────────────────────────────────────────────────────────────────────

/// An edit step, mirroring the `event.update_*(…)` calls that
/// `calendar_storage_adapter::update_event` fans out into `update_ical_property`
/// (present → replace) and `remove_ical_property` (a cleared `Option`).
enum Op<'a> {
    Update(&'a str, &'a str),
    Remove(&'a str),
}

// ── BEFORE: verbatim replica of the shipped-before methods ──────────────────

fn before_update(ical: &mut String, property_name: &str, value: &str) {
    let search_str = format!("\n{}:", property_name);
    let search_str_alt = format!("\r\n{}:", property_name);

    let pos = ical
        .find(&search_str)
        .or_else(|| ical.find(&search_str_alt));

    if let Some(pos) = pos {
        let value_start = pos + search_str.len();
        let value_end = ical[value_start..]
            .find('\n')
            .map(|p| value_start + p)
            .unwrap_or_else(|| ical.len());
        let before = &ical[..value_start];
        let after = &ical[value_end..];
        *ical = format!("{}{}{}", before, value, after);
    } else {
        let end_pos = ical.find("END:VEVENT").unwrap_or(ical.len());
        let before = &ical[..end_pos];
        let after = &ical[end_pos..];
        *ical = format!("{}{}:{}\n{}", before, property_name, value, after);
    }
}

fn before_remove(ical: &mut String, property_name: &str) {
    let search_str = format!("\n{}:", property_name);
    let search_str_alt = format!("\r\n{}:", property_name);

    let pos = ical
        .find(&search_str)
        .or_else(|| ical.find(&search_str_alt));

    if let Some(pos) = pos {
        let value_end = ical[pos + 1..]
            .find('\n')
            .map(|p| pos + 1 + p)
            .unwrap_or_else(|| ical.len());
        let before = &ical[..pos];
        let after = &ical[value_end..];
        *ical = format!("{}{}", before, after);
    }
}

// ── AFTER: verbatim replica of the shipped-after methods ────────────────────

fn line_needle<'a>(buf: &'a mut [u8; 64], name: &str) -> Option<&'a str> {
    let n = name.len();
    if n + 2 > buf.len() {
        return None;
    }
    buf[0] = b'\n';
    buf[1..1 + n].copy_from_slice(name.as_bytes());
    buf[1 + n] = b':';
    std::str::from_utf8(&buf[..n + 2]).ok()
}

fn after_update(ical: &mut String, property_name: &str, value: &str) {
    let mut buf = [0u8; 64];
    let needle_owned;
    let needle: &str = match line_needle(&mut buf, property_name) {
        Some(n) => n,
        None => {
            needle_owned = format!("\n{property_name}:");
            &needle_owned
        }
    };

    if let Some(pos) = ical.find(needle) {
        let value_start = pos + needle.len();
        let value_end = ical[value_start..]
            .find('\n')
            .map_or(ical.len(), |p| value_start + p);
        ical.replace_range(value_start..value_end, value);
    } else {
        let end_pos = ical.find("END:VEVENT").unwrap_or(ical.len());
        ical.reserve(property_name.len() + value.len() + 2);
        ical.insert(end_pos, '\n');
        ical.insert_str(end_pos, value);
        ical.insert(end_pos, ':');
        ical.insert_str(end_pos, property_name);
    }
}

fn after_remove(ical: &mut String, property_name: &str) {
    let mut buf = [0u8; 64];
    let needle_owned;
    let needle: &str = match line_needle(&mut buf, property_name) {
        Some(n) => n,
        None => {
            needle_owned = format!("\n{property_name}:");
            &needle_owned
        }
    };

    if let Some(pos) = ical.find(needle) {
        let value_end = ical[pos + 1..]
            .find('\n')
            .map_or(ical.len(), |p| pos + 1 + p);
        ical.replace_range(pos..value_end, "");
    }
}

fn apply_before(base: &str, ops: &[Op]) -> String {
    let mut ical = base.to_string();
    for op in ops {
        match op {
            Op::Update(n, v) => before_update(&mut ical, n, v),
            Op::Remove(n) => before_remove(&mut ical, n),
        }
    }
    ical
}

fn apply_after(base: &str, ops: &[Op]) -> String {
    let mut ical = base.to_string();
    for op in ops {
        match op {
            Op::Update(n, v) => after_update(&mut ical, n, v),
            Op::Remove(n) => after_remove(&mut ical, n),
        }
    }
    ical
}

/// A realistic stored VEVENT body (CRLF-terminated, ~1.5 KB with attendees and
/// a VALARM) — the shape `calendar_storage_adapter` hydrates before applying an
/// `UpdateEventDto`.
fn base_body() -> String {
    let mut b = String::from(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n\
         BEGIN:VEVENT\r\nUID:evt-round18@oxicloud.test\r\nDTSTAMP:20260101T100000Z\r\n\
         DTSTART:20260101T120000Z\r\nDTEND:20260101T130000Z\r\n\
         SUMMARY:Original quarterly planning sync\r\n\
         DESCRIPTION:The original description body for the event, moderately long.\r\n\
         LOCATION:Room A, Ground Floor\r\nCATEGORIES:work,planning,quarterly\r\n\
         ORGANIZER;CN=Alice Example:mailto:alice@oxicloud.test\r\n",
    );
    // A handful of attendees + a VALARM to bring the body to a realistic size,
    // so BEFORE's per-property full-body `format!` copies real bytes.
    for i in 0..8 {
        b.push_str(&format!(
            "ATTENDEE;CN=Guest {i};PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:guest{i}@oxicloud.test\r\n"
        ));
    }
    b.push_str(
        "BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Reminder\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n",
    );
    b
}

fn section_calendar_edit() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let base = base_body();

    // A full multi-field REST edit: five existing properties replaced (SUMMARY,
    // DESCRIPTION, LOCATION, DTSTART, DTEND — the last two rewritten twice, as
    // the time-range + all-day updates both do), one new property inserted
    // (RRULE, absent from the body), one cleared (CATEGORIES). Exactly the
    // `update_ical_property` / `remove_ical_property` fan-out of `update_event`.
    let ops = [
        Op::Update("SUMMARY", "Updated quarterly planning sync"),
        Op::Update(
            "DESCRIPTION",
            "A revised, noticeably longer description so the replacement value differs in length from the original and exercises the grow path.",
        ),
        Op::Update("LOCATION", "Conference Room 42, Building B"),
        Op::Update("DTSTART", "20260202T090000Z"),
        Op::Update("DTEND", "20260202T100000Z"),
        Op::Update("DTSTART", "20260202T000000Z"),
        Op::Update("DTEND", "20260202T010000Z"),
        Op::Update("RRULE", "FREQ=WEEKLY;COUNT=10"),
        Op::Remove("CATEGORIES"),
    ];

    // Equivalence gate: the emitted body is byte-for-byte identical.
    let b = apply_before(&base, &ops);
    let a = apply_after(&base, &ops);
    assert_eq!(b, a, "C1 emitted body differs between BEFORE and AFTER");

    let before = measure(iters, || {
        black_box(apply_before(black_box(&base), black_box(&ops)));
    });
    let after = measure(iters, || {
        black_box(apply_after(black_box(&base), black_box(&ops)));
    });

    println!(
        "\n## [C1] calendar-event multi-field edit ({} ops, {}-byte body)",
        ops.len(),
        base.len()
    );
    println!("# both arms pay one identical `base.to_string()` reset per op (constant, shared)");
    header_footer("update_event in-place property rewrite", &before, &after);
    gate_allocs("C1", &before, &after);
}

fn main() {
    println!("#################################################################");
    println!("# Round-18 calendar-event edit CPU/alloc micro-pack");
    println!("#################################################################");

    section_calendar_edit();

    println!("\nGATE PASS (all sections)");
}
