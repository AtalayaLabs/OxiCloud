//! CalDAV whole-calendar response benchmark — buffered vs streamed (ROUND5).
//!
//! The REPORT path (no-range calendar-query, sync-collection) and the
//! collection `.ics` GET used to (a) materialise EVERY event DTO of the
//! calendar in one Vec (owned `ical_data` per row), then (b) render the
//! complete multistatus / VCALENDAR into a second in-RAM buffer — the
//! calendar resident twice, TTFB = full generation. AFTER streams ONE
//! window-ordered scan (`MIN(start_time) OVER (PARTITION BY ical_uid)`)
//! through a PG cursor and cuts pages at UID boundaries — same-UID rows
//! never split, bundle order equals the buffered first-appearance
//! order, and only a page of rows is resident. (A first keyset-paged
//! shape re-aggregated per page — 3-4x wall — and a per-uid ANY
//! hydration paid ~20 µs per index descent — both measured and
//! discarded; see ROUND5.md.)
//!
//! This bench drives the REAL repository methods + adapter writers both
//! ways at the repo layer (authz gates are identical constants on both
//! sides and excluded). BEFORE uses the surviving buffered generator
//! (byte-stable refactor of the old monolith) + a verbatim copy of the
//! removed `generate_full_calendar_ical`. Gates: streamed concatenation
//! byte-identical to the buffered output for BOTH the multistatus and
//! the ICS body (seeded with strictly distinct start times so ordering
//! is deterministic).
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo run --release --features bench --example bench_caldav_stream
//! Tunables (env): BENCH_EVENTS (4000), BENCH_PAGE (500), BENCH_PASSES (9).

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use oxicloud::application::adapters::caldav_adapter::{
    CalDavAdapter, CalDavReportType, bench as caldav_bench,
};
use oxicloud::application::dtos::calendar_dto::CalendarEventDto;
use oxicloud::domain::repositories::calendar_event_repository::CalendarEventRepository;
use oxicloud::infrastructure::repositories::pg::CalendarEventPgRepository;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

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

// ─── BEFORE: verbatim copy of the removed whole-calendar ICS builder ────────

#[allow(clippy::all)]
mod before {
    use super::*;

    /// Verbatim copy of the removed `generate_full_calendar_ical`.
    pub fn generate_full_calendar_ical(calendar_name: &str, events: &[CalendarEventDto]) -> String {
        let mut buf = String::with_capacity(256 + events.len() * 320);
        let _ = write!(
            buf,
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\nX-WR-CALNAME:{}\r\n",
            calendar_name
        );
        for group in caldav_bench::group_events_by_uid(events) {
            for event in group {
                if let Some(chunk) = caldav_bench::extract_vevent_chunk(&event.ical_data) {
                    buf.push_str(chunk);
                    if !buf.ends_with('\n') {
                        buf.push_str("\r\n");
                    }
                }
            }
        }
        buf.push_str("END:VCALENDAR\r\n");
        buf
    }
}

// ─── Seed ───────────────────────────────────────────────────────────────────

fn vevent_body(uid: &str, start: DateTime<Utc>, exception: bool) -> String {
    let dt = start.format("%Y%m%dT%H%M%SZ");
    let dtend = (start + chrono::Duration::minutes(45)).format("%Y%m%dT%H%M%SZ");
    let mut v = String::with_capacity(640);
    v.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n");
    v.push_str("BEGIN:VEVENT\r\n");
    let _ = write!(v, "UID:{uid}\r\nDTSTAMP:20260701T120000Z\r\n");
    let _ = write!(v, "DTSTART:{dt}\r\nDTEND:{dtend}\r\n");
    if exception {
        let _ = write!(v, "RECURRENCE-ID:{dt}\r\n");
    } else {
        v.push_str("RRULE:FREQ=WEEKLY;BYDAY=WE\r\n");
    }
    let _ = write!(v, "SUMMARY:Reunión {uid}\r\n");
    v.push_str("LOCATION:Sala 3\r\nSTATUS:CONFIRMED\r\n");
    v.push_str("BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\n");
    v.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
    v
}

struct Seeded {
    calendar_id: Uuid,
    owner_id: Uuid,
}

async fn seed(pool: &PgPool, n: usize) -> Seeded {
    let owner_id: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_calstream', 'bench_calstream@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("seed user");
    let calendar_id: Uuid = sqlx::query_scalar(
        "INSERT INTO caldav.calendars (id, name, owner_id)
         VALUES (gen_random_uuid(), 'Agenda grande', $1) RETURNING id",
    )
    .bind(owner_id)
    .fetch_one(pool)
    .await
    .expect("seed calendar");

    let base = Utc.with_ymd_and_hms(2026, 1, 5, 8, 0, 0).unwrap();
    let mut tx = pool.begin().await.expect("begin");
    for i in 0..n {
        // 20% of rows are exception overrides sharing the previous
        // master's UID; every start_time is strictly distinct so the
        // response ordering is deterministic (byte-identity gate).
        let exception = i % 5 == 4;
        let master = if exception { i - 1 } else { i };
        let uid = format!("evt-{master:06}@oxicloud.bench");
        let start = base + chrono::Duration::seconds((i as i64) * 137);
        let recurrence: Option<DateTime<Utc>> = exception.then_some(start);
        sqlx::query(
            "INSERT INTO caldav.calendar_events
                 (id, calendar_id, summary, start_time, end_time, all_day,
                  rrule, ical_uid, ical_data, recurrence_id)
             VALUES (gen_random_uuid(), $1, $2, $3, $4, false, $5, $6, $7, $8)",
        )
        .bind(calendar_id)
        .bind(format!("Reunión {i}"))
        .bind(start)
        .bind(start + chrono::Duration::minutes(45))
        .bind((!exception).then_some("FREQ=WEEKLY;BYDAY=WE"))
        .bind(&uid)
        .bind(vevent_body(&uid, start, exception))
        .bind(recurrence)
        .execute(&mut *tx)
        .await
        .expect("seed event");
    }
    tx.commit().await.expect("commit");
    Seeded {
        calendar_id,
        owner_id,
    }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
    let _ = sqlx::query("DELETE FROM caldav.calendar_events WHERE calendar_id = $1")
        .bind(s.calendar_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM caldav.calendars WHERE id = $1")
        .bind(s.calendar_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(s.owner_id)
        .execute(pool)
        .await;
}

// ─── Pipelines ──────────────────────────────────────────────────────────────

fn report_shape() -> CalDavReportType {
    CalDavReportType::CalendarQuery {
        props: vec![],
        time_range: None,
    }
}

/// BEFORE: the buffered pipeline — full entity fetch → full DTO Vec →
/// one whole-response buffer. Returns (ttfb_ms, wall_ms, bytes).
async fn buffered_report(
    repo: &CalendarEventPgRepository,
    calendar_id: &Uuid,
    base_href: &str,
) -> (f64, f64, Vec<u8>) {
    let t0 = Instant::now();
    let events: Vec<CalendarEventDto> = repo
        .list_events_by_calendar(calendar_id)
        .await
        .expect("list events")
        .into_iter()
        .map(CalendarEventDto::from)
        .collect();
    let mut out = Vec::with_capacity(events.len() * 1024);
    CalDavAdapter::generate_calendar_events_response(
        &mut out,
        &events,
        &report_shape(),
        base_href,
        &[],
        None,
    )
    .expect("generate");
    let wall = t0.elapsed().as_secs_f64() * 1e3;
    // Buffered: the first byte is only available when everything is.
    (wall, wall, out)
}

/// AFTER: the streaming pipeline — uid-keyset pages, per-page hydration,
/// header/page/footer chunks (the handler's loop over the same public
/// pieces). Returns (ttfb_ms, wall_ms, concatenated bytes).
async fn streamed_report(
    repo: &CalendarEventPgRepository,
    calendar_id: &Uuid,
    base_href: &str,
    page_uids: usize,
) -> (f64, f64, Vec<u8>) {
    let t0 = Instant::now();
    let mut ttfb = None;
    let mut all = Vec::new();
    let report = report_shape();

    let mut chunk = Vec::with_capacity(256);
    {
        let mut w = quick_xml::Writer::new(&mut chunk);
        CalDavAdapter::write_caldav_multistatus_start(&mut w).expect("start");
    }
    all.extend_from_slice(&chunk);

    {
        use futures::TryStreamExt;
        let mut rows = repo.stream_events_uid_order(*calendar_id);
        let mut page: Vec<CalendarEventDto> = Vec::with_capacity(page_uids + 32);
        loop {
            let next = rows
                .try_next()
                .await
                .expect("stream row")
                .map(CalendarEventDto::from);
            let flush = match &next {
                Some(ev) => {
                    page.len() >= page_uids
                        && page.last().is_some_and(|p| p.ical_uid != ev.ical_uid)
                }
                None => !page.is_empty(),
            };
            if flush {
                let mut chunk = Vec::with_capacity(page.len() * 1024 + 128);
                {
                    let mut w = quick_xml::Writer::new(&mut chunk);
                    CalDavAdapter::write_report_page(&mut w, &page, &report, base_href)
                        .expect("page");
                }
                if ttfb.is_none() && !all.is_empty() {
                    // header already emitted; first data page complete
                }
                page.clear();
                all.extend_from_slice(&chunk);
                ttfb.get_or_insert_with(|| t0.elapsed().as_secs_f64() * 1e3);
            }
            match next {
                Some(ev) => page.push(ev),
                None => break,
            }
        }
    }

    let mut chunk = Vec::with_capacity(32);
    {
        let mut w = quick_xml::Writer::new(&mut chunk);
        CalDavAdapter::write_caldav_multistatus_end(&mut w).expect("end");
    }
    all.extend_from_slice(&chunk);
    (
        ttfb.unwrap_or(f64::NAN),
        t0.elapsed().as_secs_f64() * 1e3,
        all,
    )
}

/// TTFB for the streaming path measured honestly: time until the FIRST
/// PAGE chunk (header + one hydrated page) exists — the moment real
/// bytes could hit the socket.
async fn streamed_report_ttfb(
    repo: &CalendarEventPgRepository,
    calendar_id: &Uuid,
    base_href: &str,
    page_uids: usize,
) -> f64 {
    use futures::TryStreamExt;
    let t0 = Instant::now();
    let mut rows = repo.stream_events_uid_order(*calendar_id);
    let mut page: Vec<CalendarEventDto> = Vec::with_capacity(page_uids + 32);
    while let Some(ev) = rows.try_next().await.expect("stream row") {
        let ev = CalendarEventDto::from(ev);
        if page.len() >= page_uids && page.last().is_some_and(|p| p.ical_uid != ev.ical_uid) {
            break;
        }
        page.push(ev);
    }
    let mut chunk = Vec::with_capacity(page.len() * 1024 + 256);
    {
        let mut w = quick_xml::Writer::new(&mut chunk);
        CalDavAdapter::write_caldav_multistatus_start(&mut w).expect("start");
        CalDavAdapter::write_report_page(&mut w, &page, &report_shape(), base_href).expect("page");
    }
    std::hint::black_box(&chunk);
    t0.elapsed().as_secs_f64() * 1e3
}

fn p50(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn peak_mib() -> f64 {
    PEAK.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let n: usize = env::var("BENCH_EVENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000);
    let page_uids: usize = env::var("BENCH_PAGE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let passes: usize = env::var("BENCH_PASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(10)
            .min_connections(10)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect Postgres"),
    );

    let seeded = seed(&pool, n).await;
    let repo = CalendarEventPgRepository::new(pool.clone());
    let base_href = format!("/caldav/{}/", seeded.calendar_id);

    println!(
        "bench_caldav_stream — {n} events (20% exceptions), page={page_uids} uids, {passes} passes\n"
    );

    // ── [1] REPORT (multistatus) ────────────────────────────────────────────
    // Warm-up + equivalence gate first.
    let (_, _, before_bytes) = buffered_report(&repo, &seeded.calendar_id, &base_href).await;
    let (_, _, after_bytes) =
        streamed_report(&repo, &seeded.calendar_id, &base_href, page_uids).await;
    let gate_report = before_bytes == after_bytes;

    let mut b_wall = Vec::new();
    let mut a_wall = Vec::new();
    let mut a_ttfb = Vec::new();
    for _ in 0..passes {
        let (_, w, out) = buffered_report(&repo, &seeded.calendar_id, &base_href).await;
        std::hint::black_box(out);
        b_wall.push(w);
        let (_, w, out) = streamed_report(&repo, &seeded.calendar_id, &base_href, page_uids).await;
        std::hint::black_box(out);
        a_wall.push(w);
        a_ttfb.push(streamed_report_ttfb(&repo, &seeded.calendar_id, &base_href, page_uids).await);
    }
    // Peak-heap arms, measured in isolation.
    reset_peak();
    let (_, _, out) = buffered_report(&repo, &seeded.calendar_id, &base_href).await;
    drop(out);
    let peak_before = peak_mib();
    reset_peak();
    // Streamed peak: emulate the socket by dropping each chunk — reuse
    // the pipeline but without accumulating (accumulation would charge
    // the response size to the streaming arm).
    {
        use futures::TryStreamExt;
        let t0 = Instant::now();
        let report = report_shape();
        let mut rows = repo.stream_events_uid_order(seeded.calendar_id);
        let mut page: Vec<CalendarEventDto> = Vec::with_capacity(page_uids + 32);
        loop {
            let next = rows
                .try_next()
                .await
                .expect("stream row")
                .map(CalendarEventDto::from);
            let flush = match &next {
                Some(ev) => {
                    page.len() >= page_uids
                        && page.last().is_some_and(|p| p.ical_uid != ev.ical_uid)
                }
                None => !page.is_empty(),
            };
            if flush {
                let mut chunk = Vec::with_capacity(page.len() * 1024 + 128);
                {
                    let mut w = quick_xml::Writer::new(&mut chunk);
                    CalDavAdapter::write_report_page(&mut w, &page, &report, &base_href)
                        .expect("page");
                }
                std::hint::black_box(&chunk);
                page.clear();
            }
            match next {
                Some(ev) => page.push(ev),
                None => break,
            }
        }
        std::hint::black_box(t0.elapsed());
    }
    let peak_after = peak_mib();

    let bw = p50(b_wall);
    let aw = p50(a_wall);
    let at = p50(a_ttfb);
    println!("[1] REPORT calendar-query (no range)   TTFB ms   wall ms   peak heap MiB");
    println!("    BEFORE (buffered)                 {bw:8.1}  {bw:8.1}   {peak_before:10.1}");
    println!(
        "    AFTER  (streamed)                 {at:8.1}  {aw:8.1}   {peak_after:10.1}   TTFB {:.1}x, heap {:.1}x lower",
        bw / at,
        peak_before / peak_after
    );

    // ── [2] Collection GET (.ics) ───────────────────────────────────────────
    let events_all: Vec<CalendarEventDto> = repo
        .list_events_by_calendar(&seeded.calendar_id)
        .await
        .expect("list")
        .into_iter()
        .map(CalendarEventDto::from)
        .collect();
    let before_ics = before::generate_full_calendar_ical("Agenda grande", &events_all);
    drop(events_all);
    // Streamed ICS: header + per-page chunks + footer (the handler loop).
    let mut after_ics = String::from(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\nX-WR-CALNAME:Agenda grande\r\n",
    );
    let ics_pages: Vec<Vec<CalendarEventDto>> = {
        use futures::TryStreamExt;
        let mut rows = repo.stream_events_uid_order(seeded.calendar_id);
        let mut pages = Vec::new();
        let mut page: Vec<CalendarEventDto> = Vec::with_capacity(page_uids + 32);
        while let Some(ev) = rows.try_next().await.expect("stream row") {
            let ev = CalendarEventDto::from(ev);
            if page.len() >= page_uids && page.last().is_some_and(|p| p.ical_uid != ev.ical_uid) {
                pages.push(std::mem::take(&mut page));
            }
            page.push(ev);
        }
        if !page.is_empty() {
            pages.push(page);
        }
        pages
    };
    for events in &ics_pages {
        let events = &events[..];
        let mut chunk = String::with_capacity(events.len() * 384);
        for group in caldav_bench::group_events_by_uid(events) {
            for event in group {
                if let Some(vevent) = caldav_bench::extract_vevent_chunk(&event.ical_data) {
                    chunk.push_str(vevent);
                    if !chunk.ends_with('\n') {
                        chunk.push_str("\r\n");
                    }
                }
            }
        }
        after_ics.push_str(&chunk);
    }
    after_ics.push_str("END:VCALENDAR\r\n");
    let gate_ics = before_ics == after_ics;
    println!(
        "[2] collection GET .ics: {} bytes, streamed == buffered: {}",
        before_ics.len(),
        if gate_ics { "OK" } else { "MISMATCH" }
    );

    cleanup(&pool, &seeded).await;

    println!(
        "\n[gate] multistatus byte-identical: {} · ICS byte-identical: {}",
        if gate_report { "OK" } else { "FAILED" },
        if gate_ics { "OK" } else { "FAILED" }
    );
    if !gate_report || !gate_ics {
        std::process::exit(1);
    }
}
