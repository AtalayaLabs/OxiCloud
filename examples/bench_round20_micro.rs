//! Round-20 CPU/alloc micro-pack (no Postgres).
//!
//! Same rule as ROUND2–19: each section is BEFORE (verbatim replica of the
//! shipped-before shape) vs AFTER (verbatim replica of the shipped-after shape,
//! which the source is then made to match), with a byte/-value equivalence gate
//! and a `GATE FAIL … rollback` check that `std::process::exit(1)`s if the AFTER
//! arm fails to beat its BEFORE — the round's roll-back rule encoded into the
//! benchmark. An AFTER that doesn't win is never applied to the source.
//!
//!   [A1] `CalendarEvent::prop_with_params` builds a throwaway
//!        `HashMap<String, Vec<String>>` (uppercased keys + cloned value Vecs)
//!        per DTSTART/DTEND/RECURRENCE-ID on every CalDAV PUT / iCal import,
//!        though the 5 production call sites only read `.get("VALUE")` (all-day
//!        detect) or discard the map entirely. AFTER scans `prop.params` for a
//!        case-insensitive `VALUE=DATE` directly — same bool, zero map.
//!
//!   [A2] `UserDto::from(User)` takes the `User` BY VALUE yet clones every
//!        field through its accessors — including `image` (a data URI up to
//!        512 KiB) and `ui_preferences` (a full `serde_json::Value` tree) — on
//!        every `/api/auth/me` and admin user listing. AFTER moves the owned
//!        fields out (the `into_parts` treatment File/Folder/Contact already
//!        have), keeping the DTO byte-identical.
//!
//!   [A3] `ContactService::parse_vcard` collects `vcard_data.lines()` into a
//!        `Vec` it only iterates, and runs `line.to_ascii_uppercase()` — a full
//!        per-line `String` copy — per EMAIL/TEL/ADR line just to `.contains`
//!        a `TYPE=` token, on every CardDAV PUT / vCard import. AFTER iterates
//!        `lines()` directly and matches with the allocation-free
//!        `common::text::ascii_ci_contains` (the CalDAV parse path already uses
//!        this shape).
//!
//!   [A4] `CalendarDto::from(Calendar)` / `AddressBookDto::from(AddressBook)`
//!        consume the entity yet clone `name`/`description`/`color` and (for
//!        calendars) the whole `custom_properties` `HashMap<String,String>`, on
//!        every CalDAV/CardDAV discovery listing. AFTER moves them.
//!
//!   [I1] The listing repositories map rows with
//!        `.collect::<Result<Vec<T>, E>>()`, whose `Result`-shunt reports
//!        `size_hint().0 == 0` — so the `Vec` grows from capacity 0 with
//!        ~⌈log₂N⌉ reallocations, memcpy-ing the accumulated (File-sized)
//!        elements each grow. AFTER pre-sizes with `Vec::with_capacity(rows.len())`
//!        and pushes with `?` (the exact pattern `list_media_files` already uses).
//!
//!   [I4] `encrypted_blob_backend::plaintext_stream` `.collect()`s every
//!        emit-slice into a `Vec` before `stream::iter` — an eager container of
//!        ⌈len/64 KiB⌉ entries per encrypted read. AFTER hands the lazy `map`
//!        iterator to `stream::iter` directly (same slice sequence, no Vec).
//!
//! Run:
//!   cargo run --release --features bench --example bench_round20_micro
//! Tunables (env): BENCH_ITERS (200000), I1_ROWS (500)

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bytes::Bytes;
use quick_xml::Writer;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use serde_json::json;
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

struct Measured {
    wall_ns_per_op: f64,
    allocs_per_op: f64,
}

fn measure<F: FnMut()>(iters: usize, mut f: F) -> Measured {
    // Warm up (grow any reused buffers, prime caches) so the measured window
    // reflects steady state, not first-touch growth.
    for _ in 0..(iters / 20).max(1) {
        f();
    }
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
        "| {:<50} | {:>12.1} | {:>10.2} |",
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
// [A1] CalendarEvent::prop_with_params — throwaway HashMap vs direct VALUE scan
// ────────────────────────────────────────────────────────────────────────────

/// The `ical` crate's parameter shape: `Option<Vec<(name, values)>>`.
type Params = Option<Vec<(String, Vec<String>)>>;

/// BEFORE: build the full uppercased `HashMap` (as the shipped `prop_with_params`
/// does), then read `.get("VALUE")` — the only thing 3 of the 5 call sites want.
fn a1_before(
    dtstart_params: &Params,
    dtstart_val: &str,
    dtend_val: &str,
) -> (bool, String, String) {
    fn prop_with_params(value: &str, params: &Params) -> (String, HashMap<String, Vec<String>>) {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(list) = params {
            for (name, values) in list {
                map.insert(name.to_ascii_uppercase(), values.clone());
            }
        }
        (value.trim().to_string(), map)
    }
    // DTSTART: needs value + the all-day flag off the map.
    let (start, start_map) = prop_with_params(dtstart_val, dtstart_params);
    let all_day = start_map
        .get("VALUE")
        .map(|vs| vs.iter().any(|v| v.eq_ignore_ascii_case("DATE")))
        .unwrap_or(false);
    // DTEND: only the value is used; the map is discarded (`_dtend_params`).
    let (end, _end_map) = prop_with_params(dtend_val, &None);
    (all_day, start, end)
}

/// AFTER: scan the params directly for a case-insensitive `VALUE=DATE`; DTEND
/// takes the plain trimmed value with no map at all.
fn a1_after(dtstart_params: &Params, dtstart_val: &str, dtend_val: &str) -> (bool, String, String) {
    let all_day = dtstart_params
        .as_ref()
        .and_then(|p| {
            p.iter()
                .rev()
                .find(|(n, _)| n.eq_ignore_ascii_case("VALUE"))
        })
        .map(|(_, vs)| vs.iter().any(|v| v.eq_ignore_ascii_case("DATE")))
        .unwrap_or(false);
    (
        all_day,
        dtstart_val.trim().to_string(),
        dtend_val.trim().to_string(),
    )
}

fn section_a1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // A timed event: DTSTART;TZID=…, DTEND;TZID=… — the common shape.
    let dtstart_params: Params = Some(vec![(
        "TZID".to_string(),
        vec!["America/New_York".to_string()],
    )]);
    let dtstart_val = "20260717T114714";
    let dtend_val = "20260717T124714";

    assert_eq!(
        a1_before(&dtstart_params, dtstart_val, dtend_val),
        a1_after(&dtstart_params, dtstart_val, dtend_val),
        "A1 extracted (all_day, start, end) differs"
    );
    // And an all-day event (VALUE=DATE) — the flag must still be detected.
    let ad: Params = Some(vec![("VALUE".to_string(), vec!["DATE".to_string()])]);
    assert!(a1_before(&ad, "20260717", "20260718").0);
    assert!(a1_after(&ad, "20260717", "20260718").0);

    let before = measure(iters, || {
        black_box(a1_before(
            black_box(&dtstart_params),
            dtstart_val,
            dtend_val,
        ));
    });
    let after = measure(iters, || {
        black_box(a1_after(black_box(&dtstart_params), dtstart_val, dtend_val));
    });

    println!("\n## [A1] CalendarEvent prop_with_params (per timed event)");
    header_footer("DTSTART/DTEND all-day extract", &before, &after);
    gate_allocs("A1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [A2] UserDto::from — clone-every-field vs move (into_parts)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct BenchUser {
    id: Uuid,
    username: Option<String>,
    email: String,
    image: Option<String>,
    oidc_provider: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    preferred_locale: Option<String>,
    ui_preferences: serde_json::Value,
}

#[allow(dead_code)]
struct BenchUserDto {
    id: String,
    username: Option<String>,
    email: String,
    federation_issuer: String,
    image: Option<String>,
    can_edit_image: bool,
    given_name: Option<String>,
    family_name: Option<String>,
    preferred_locale: Option<String>,
    ui_preferences: serde_json::Value,
}

/// BEFORE: the shipped `From<User>` shape — clone through accessors even though
/// `user` is owned and dropped immediately (image ≤512 KiB memcpy + JSON clone).
fn a2_before(user: &BenchUser) -> BenchUserDto {
    BenchUserDto {
        id: user.id.to_string(),
        username: user.username.as_deref().map(str::to_string),
        email: user.email.clone(),
        federation_issuer: user.oidc_provider.as_deref().unwrap_or("local").to_string(),
        image: user.image.as_deref().map(|s| s.to_string()),
        can_edit_image: user.oidc_provider.is_none(),
        given_name: user.given_name.as_deref().map(str::to_string),
        family_name: user.family_name.as_deref().map(str::to_string),
        preferred_locale: user.preferred_locale.as_deref().map(str::to_string),
        ui_preferences: user.ui_preferences.clone(),
    }
}

/// AFTER: compute the derived bool first, then move every owned field out.
fn a2_after(user: BenchUser) -> BenchUserDto {
    let can_edit_image = user.oidc_provider.is_none();
    BenchUserDto {
        id: user.id.to_string(),
        username: user.username,
        email: user.email,
        federation_issuer: user.oidc_provider.unwrap_or_else(|| "local".to_string()),
        image: user.image,
        can_edit_image,
        given_name: user.given_name,
        family_name: user.family_name,
        preferred_locale: user.preferred_locale,
        ui_preferences: user.ui_preferences,
    }
}

fn section_a2() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // A realistic /api/auth/me user: OIDC-provisioned, a ~48 KiB avatar data
    // URI, a small preferences bag. (512 KiB is the ceiling; 48 KiB keeps the
    // bench fast while still crossing the "large image" boundary.)
    let image = format!("data:image/png;base64,{}", "A".repeat(48 * 1024));
    let user = BenchUser {
        id: Uuid::from_u128(0x1234_5678_9abc_def0_1122_3344_5566_7788),
        username: Some("benchuser".to_string()),
        email: "bench@example.com".to_string(),
        image: Some(image.clone()),
        oidc_provider: Some("google".to_string()),
        given_name: Some("Bench".to_string()),
        family_name: Some("User".to_string()),
        preferred_locale: Some("en".to_string()),
        ui_preferences: json!({"hideDotfiles": true, "viewMode": "grid", "sidebar": "collapsed"}),
    };

    // Equivalence: BEFORE and AFTER produce byte-identical DTO fields.
    let b = a2_before(&user);
    let a = a2_after(user.clone());
    assert_eq!(b.image, a.image, "A2 image differs");
    assert_eq!(b.email, a.email, "A2 email differs");
    assert_eq!(b.federation_issuer, a.federation_issuer, "A2 federation_issuer differs");
    assert_eq!(
        b.can_edit_image, a.can_edit_image,
        "A2 can_edit_image differs"
    );
    assert_eq!(
        b.ui_preferences, a.ui_preferences,
        "A2 ui_preferences differs"
    );

    // `a2_after` consumes its input, so each op must materialize one owned
    // `User` (a clone). BEFORE pays the same source-clone so the measured delta
    // isolates BEFORE's extra per-field clones vs AFTER's field moves — not the
    // shared source clone.
    let before = measure(iters, || {
        let u = black_box(user.clone());
        black_box(a2_before(black_box(&u)));
    });
    let after = measure(iters, || {
        black_box(a2_after(black_box(user.clone())));
    });

    println!("\n## [A2] UserDto::from (OIDC user, 48 KiB image + prefs)");
    header_footer("clone-every-field vs move", &before, &after);
    gate_allocs("A2", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [A3] parse_vcard per-line — lines Vec + to_ascii_uppercase vs direct + CI
// ────────────────────────────────────────────────────────────────────────────

/// Allocation-free ASCII case-insensitive substring test (replica of the
/// shipped `common::text::ascii_ci_contains` the AFTER source will call).
fn bench_ascii_ci_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// BEFORE: collect lines into a Vec, then uppercase each EMAIL/TEL/ADR line to
/// classify its TYPE. Returns the classification labels (observable result).
fn a3_before(vcard: &str) -> Vec<&'static str> {
    let lines: Vec<&str> = vcard.lines().collect();
    let mut out = Vec::new();
    for line in &lines {
        let line = line.trim();
        if line.starts_with("EMAIL") {
            let up = line.to_ascii_uppercase();
            out.push(if up.contains("TYPE=HOME") {
                "home"
            } else if up.contains("TYPE=WORK") {
                "work"
            } else {
                "other"
            });
        } else if line.starts_with("TEL") {
            let up = line.to_ascii_uppercase();
            out.push(if up.contains("TYPE=CELL") || up.contains("TYPE=MOBILE") {
                "mobile"
            } else if up.contains("TYPE=HOME") {
                "home"
            } else {
                "other"
            });
        } else if line.starts_with("ADR") {
            let up = line.to_ascii_uppercase();
            out.push(if up.contains("TYPE=HOME") {
                "home"
            } else if up.contains("TYPE=WORK") {
                "work"
            } else {
                "other"
            });
        }
    }
    out
}

/// AFTER: iterate lines() directly; classify with allocation-free CI contains.
fn a3_after(vcard: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    for line in vcard.lines() {
        let line = line.trim();
        let b = line.as_bytes();
        if line.starts_with("EMAIL") {
            out.push(if bench_ascii_ci_contains(b, b"TYPE=HOME") {
                "home"
            } else if bench_ascii_ci_contains(b, b"TYPE=WORK") {
                "work"
            } else {
                "other"
            });
        } else if line.starts_with("TEL") {
            out.push(
                if bench_ascii_ci_contains(b, b"TYPE=CELL")
                    || bench_ascii_ci_contains(b, b"TYPE=MOBILE")
                {
                    "mobile"
                } else if bench_ascii_ci_contains(b, b"TYPE=HOME") {
                    "home"
                } else {
                    "other"
                },
            );
        } else if line.starts_with("ADR") {
            out.push(if bench_ascii_ci_contains(b, b"TYPE=HOME") {
                "home"
            } else if bench_ascii_ci_contains(b, b"TYPE=WORK") {
                "work"
            } else {
                "other"
            });
        }
    }
    out
}

fn section_a3() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let vcard = "BEGIN:VCARD\r\n\
        VERSION:3.0\r\n\
        FN:Bench User\r\n\
        N:User;Bench;;;\r\n\
        EMAIL;TYPE=HOME:home@example.com\r\n\
        EMAIL;TYPE=WORK:work@example.com\r\n\
        TEL;TYPE=CELL:+15551234567\r\n\
        ADR;TYPE=HOME:;;123 Main St;Town;CA;90210;USA\r\n\
        END:VCARD\r\n";

    assert_eq!(
        a3_before(vcard),
        a3_after(vcard),
        "A3 classification differs"
    );

    let before = measure(iters, || {
        black_box(a3_before(black_box(vcard)));
    });
    let after = measure(iters, || {
        black_box(a3_after(black_box(vcard)));
    });

    println!("\n## [A3] parse_vcard type classify (2 email / 1 tel / 1 adr)");
    header_footer("lines-Vec + uppercase vs direct + CI", &before, &after);
    gate_allocs("A3", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [A4] CalendarDto::from — clone name/desc/color/custom_properties vs move
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct BenchCalendar {
    id: Uuid,
    owner_id: Uuid,
    name: String,
    description: Option<String>,
    color: Option<String>,
    custom_properties: HashMap<String, String>,
}

#[allow(dead_code)]
struct BenchCalendarDto {
    id: String,
    owner_id: String,
    name: String,
    description: Option<String>,
    color: Option<String>,
    custom_properties: HashMap<String, String>,
}

fn a4_before(c: &BenchCalendar) -> BenchCalendarDto {
    BenchCalendarDto {
        id: c.id.to_string(),
        owner_id: c.owner_id.to_string(),
        name: c.name.clone(),
        description: c.description.clone(),
        color: c.color.clone(),
        custom_properties: c.custom_properties.clone(),
    }
}

fn a4_after(c: BenchCalendar) -> BenchCalendarDto {
    BenchCalendarDto {
        id: c.id.to_string(),
        owner_id: c.owner_id.to_string(),
        name: c.name,
        description: c.description,
        color: c.color,
        custom_properties: c.custom_properties,
    }
}

fn section_a4() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let mut custom = HashMap::new();
    custom.insert("X-APPLE-CALENDAR-COLOR".to_string(), "#FF2968".to_string());
    custom.insert("CALSCALE".to_string(), "GREGORIAN".to_string());
    let cal = BenchCalendar {
        id: Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888),
        owner_id: Uuid::from_u128(0x9999_aaaa_bbbb_cccc_dddd_eeee_ffff_0000),
        name: "Personal".to_string(),
        description: Some("My personal calendar".to_string()),
        color: Some("#FF2968".to_string()),
        custom_properties: custom,
    };

    let b = a4_before(&cal);
    let a = a4_after(cal.clone());
    assert_eq!(b.name, a.name);
    assert_eq!(
        b.custom_properties, a.custom_properties,
        "A4 custom_properties differ"
    );

    let before = measure(iters, || {
        let c = black_box(cal.clone());
        black_box(a4_before(black_box(&c)));
    });
    let after = measure(iters, || {
        black_box(a4_after(black_box(cal.clone())));
    });

    println!("\n## [A4] CalendarDto::from (2 custom properties)");
    header_footer("clone name/desc/color/props vs move", &before, &after);
    gate_allocs("A4", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [I1] Result-collect never pre-sizes — collect vs Vec::with_capacity + push
// ────────────────────────────────────────────────────────────────────────────

/// A File-sized (~128 B) element so the container-realloc memcpy cost is
/// realistic. The per-element mapper allocates nothing in either arm, so the
/// measured alloc delta is exactly the container growth.
type Row = [u8; 128];

fn i1_before(rows: &[Row]) -> Result<Vec<Row>, ()> {
    rows.iter()
        .map(|r| Ok::<Row, ()>(*r))
        .collect::<Result<Vec<_>, _>>()
}

fn i1_after(rows: &[Row]) -> Result<Vec<Row>, ()> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(*r);
    }
    Ok(out)
}

fn section_i1() {
    let n: usize = env_or("I1_ROWS", 500);
    let iters: usize = env_or("BENCH_ITERS", 200_000) / 20; // heavier op
    let rows: Vec<Row> = (0..n).map(|i| [i as u8; 128]).collect();

    assert_eq!(
        i1_before(&rows).unwrap().len(),
        i1_after(&rows).unwrap().len()
    );

    let before = measure(iters, || {
        black_box(i1_before(black_box(&rows)).unwrap());
    });
    let after = measure(iters, || {
        black_box(i1_after(black_box(&rows)).unwrap());
    });

    println!("\n## [I1] Result-collect vs with_capacity ({n} File-sized rows)");
    header_footer(
        "collect::<Result<Vec>> vs with_capacity+push",
        &before,
        &after,
    );
    gate_allocs("I1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [I4] plaintext_stream — eager Vec collect vs lazy iterator
// ────────────────────────────────────────────────────────────────────────────

const PLAINTEXT_EMIT_SIZE: usize = 64 * 1024;

type BenchStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

fn i4_before(data: Bytes) -> BenchStream {
    let len = data.len();
    let slices: Vec<Result<Bytes, std::io::Error>> = (0..len)
        .step_by(PLAINTEXT_EMIT_SIZE)
        .map(|off| Ok(data.slice(off..len.min(off + PLAINTEXT_EMIT_SIZE))))
        .collect();
    Box::pin(futures::stream::iter(slices))
}

fn i4_after(data: Bytes) -> BenchStream {
    let len = data.len();
    Box::pin(futures::stream::iter(
        (0..len)
            .step_by(PLAINTEXT_EMIT_SIZE)
            .map(move |off| Ok(data.slice(off..len.min(off + PLAINTEXT_EMIT_SIZE)))),
    ))
}

fn section_i4() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    // 4 MiB decrypted payload → 64 emit-slices.
    let data = Bytes::from(vec![0u8; 4 * 1024 * 1024]);

    let before = measure(iters, || {
        // Constructing the stream is the measured work (the Vec vs no-Vec); the
        // stream is dropped unpolled, so bind to `_` to quiet the must-use lint.
        let _ = black_box(i4_before(black_box(data.clone())));
    });
    let after = measure(iters, || {
        let _ = black_box(i4_after(black_box(data.clone())));
    });

    println!("\n## [I4] plaintext_stream (4 MiB → 64 slices)");
    header_footer("collect Vec + stream::iter vs lazy iter", &before, &after);
    gate_allocs("I4", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [C1] NC write_etag_element — quoted String + escape vs borrowed pre-escaped
// ────────────────────────────────────────────────────────────────────────────

/// BEFORE: build a `"…"`-quoted `String`, then write it as an auto-escaped text
/// element — `quick_xml` escapes the `"` → `&quot;`, re-allocating an owned Cow.
fn c1_before(buf: &mut Vec<u8>, tag: &str, etag: &str) {
    let mut w = Writer::new(&mut *buf);
    let mut quoted = String::with_capacity(etag.len() + 2);
    quoted.push('"');
    quoted.push_str(etag);
    quoted.push('"');
    w.write_event(Event::Start(BytesStart::new(tag))).unwrap();
    w.write_event(Event::Text(BytesText::new(&quoted))).unwrap();
    w.write_event(Event::End(BytesEnd::new(tag))).unwrap();
}

/// AFTER: emit the pre-escaped `&quot;` quote literals as borrowed text events
/// around the escaped etag body — byte-identical output, zero owned strings.
fn c1_after(buf: &mut Vec<u8>, tag: &str, etag: &str) {
    let mut w = Writer::new(&mut *buf);
    w.write_event(Event::Start(BytesStart::new(tag))).unwrap();
    w.write_event(Event::Text(BytesText::from_escaped("&quot;")))
        .unwrap();
    w.write_event(Event::Text(BytesText::new(etag))).unwrap();
    w.write_event(Event::Text(BytesText::from_escaped("&quot;")))
        .unwrap();
    w.write_event(Event::End(BytesEnd::new(tag))).unwrap();
}

fn section_c1() {
    let iters: usize = env_or("BENCH_ITERS", 200_000);
    let tag = "d:getetag";
    let etag = "a1b2c3d4e5f6-1719792000"; // realistic NC etag

    // Equivalence: byte-identical output, incl. an etag with XML-special chars.
    let (mut b1, mut b2) = (Vec::new(), Vec::new());
    c1_before(&mut b1, tag, etag);
    c1_after(&mut b2, tag, etag);
    assert_eq!(b1, b2, "C1 emitted bytes differ (hex etag)");
    let (mut s1, mut s2) = (Vec::new(), Vec::new());
    c1_before(&mut s1, tag, "abc&def<x\"y");
    c1_after(&mut s2, tag, "abc&def<x\"y");
    assert_eq!(s1, s2, "C1 emitted bytes differ (special chars)");

    let mut buf = Vec::with_capacity(64);
    let before = measure(iters, || {
        buf.clear();
        c1_before(black_box(&mut buf), tag, black_box(etag));
    });
    let after = measure(iters, || {
        buf.clear();
        c1_after(black_box(&mut buf), tag, black_box(etag));
    });

    println!("\n## [C1] NC write_etag_element (per file+folder PROPFIND row)");
    header_footer("quoted String + escape vs borrowed events", &before, &after);
    gate_allocs("C1", &before, &after);
}

// ────────────────────────────────────────────────────────────────────────────
// [C3] favorites REPORT — map.get().clone() vs map.remove() (move)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
#[allow(dead_code)] // fields model FileDto's owned Strings for realistic clone cost
struct BenchFileDto {
    id: String,
    name: String,
    path: String,
    folder_id: String,
    size_formatted: String,
    content_hash: String,
    etag: String,
}

/// BEFORE: clone the whole DTO out of a map that is dropped at fn end.
fn c3_before(favorites: &[String], map: &HashMap<String, BenchFileDto>) -> Vec<BenchFileDto> {
    let mut out = Vec::with_capacity(favorites.len());
    for id in favorites {
        if let Some(f) = map.get(id) {
            out.push(f.clone());
        }
    }
    out
}

/// AFTER: move the DTO out — the map is consumed anyway.
fn c3_after(favorites: &[String], mut map: HashMap<String, BenchFileDto>) -> Vec<BenchFileDto> {
    let mut out = Vec::with_capacity(favorites.len());
    for id in favorites {
        if let Some(f) = map.remove(id) {
            out.push(f);
        }
    }
    out
}

fn section_c3() {
    let iters: usize = env_or("BENCH_ITERS", 200_000) / 10; // heavier op
    let n = 20usize;
    let favorites: Vec<String> = (0..n).map(|i| format!("id-{i:04}")).collect();
    let mut map: HashMap<String, BenchFileDto> = HashMap::with_capacity(n);
    for (i, id) in favorites.iter().enumerate() {
        map.insert(
            id.clone(),
            BenchFileDto {
                id: id.clone(),
                name: format!("file-{i}.txt"),
                path: format!("/drive/folder/file-{i}.txt"),
                folder_id: "0e72efc0-0d1c-45a1-b434-52336643b3f7".to_string(),
                size_formatted: "1.2 MB".to_string(),
                content_hash: "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4".to_string(),
                etag: "18abf-1719792000".to_string(),
            },
        );
    }

    // Equivalence: same items in favorites order.
    let ids_b: Vec<String> = c3_before(&favorites, &map)
        .into_iter()
        .map(|f| f.id)
        .collect();
    let ids_a: Vec<String> = c3_after(&favorites, map.clone())
        .into_iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(ids_b, ids_a, "C3 selected items differ");

    let before = measure(iters, || {
        let m = black_box(map.clone());
        black_box(c3_before(black_box(&favorites), &m));
    });
    let after = measure(iters, || {
        black_box(c3_after(black_box(&favorites), black_box(map.clone())));
    });

    println!("\n## [C3] favorites REPORT map hydrate ({n} favorites)");
    header_footer("get().clone() vs remove() move", &before, &after);
    gate_allocs("C3", &before, &after);
}

fn main() {
    println!("# Round-20 micro-pack — BEFORE/AFTER (counting allocator, release)");
    println!("# allocs/op is the deterministic gate; a non-winning AFTER exits 1 (rollback).");
    section_a1();
    section_a2();
    section_a3();
    section_a4();
    section_i1();
    section_i4();
    section_c1();
    section_c3();
    println!("\nAll Round-20 sections passed their allocation gate.");
}
