//! CalDAV parse-path benchmark — the write-side 8×-reparse and the
//! read-side per-event copies (ROUND4).
//!
//! What changed:
//!
//!   • `CalendarEvent::from_ical` funnelled each of its 8 property
//!     lookups through an extractor that re-ran the full `IcalParser`
//!     (line unfolding + component tree) over the whole body — 8
//!     complete parses per VEVENT on every CalDAV PUT, `8·(M+1)` on a
//!     master+M-exceptions PUT, `8·N` on an N-event import. Now: one
//!     parse, all lookups on the parsed component (value-only lookups
//!     also skip the parameter-map build).
//!   • `split_vevents` uppercased EVERY line into a fresh String.
//!     Now: allocation-free case-insensitive prefix tests.
//!   • `extract_vevent_chunk` (read side: every REPORT/GET, per event)
//!     allocated a full uppercase copy of the stored body just to find
//!     two tags. Now: memchr fast path + alloc-free CI scan fallback.
//!   • `group_events_by_uid` (read side, per REPORT) cloned every
//!     event's UID String. Now: borrowed keys.
//!
//! The OLD logic is copied verbatim into `mod before`; equivalence
//! gates assert byte-identical parsed fields / chunk slices / grouping
//! across a corpus incl. folded lines, params, VALARM, all-day,
//! exceptions and mixed-case tags (exit 1 on any diff).
//!
//! Run (no Postgres needed):
//!   cargo run --release --features bench --example bench_caldav_parse
//! Tunables (env):
//!   BENCH_EVENTS (200)  BENCH_PASSES (30)  BENCH_GROUP_N (5000)

use std::env;
use std::hint::black_box;
use std::time::Instant;

use chrono::{DateTime, TimeZone, Utc};
use oxicloud::application::adapters::caldav_adapter::bench as caldav_bench;
use oxicloud::application::dtos::calendar_dto::CalendarEventDto;
use oxicloud::domain::entities::calendar_event::CalendarEvent;
use uuid::Uuid;

// ─── BEFORE: verbatim copies of the pre-optimization logic ──────────────────

#[allow(clippy::all)]
mod before {
    use std::collections::HashMap;

    /// Old `parse_first_vevent` — fresh parser per call.
    pub fn parse_first_vevent(ical_data: &str) -> Option<ical::parser::ical::component::IcalEvent> {
        use std::io::BufReader;
        let reader = BufReader::new(ical_data.as_bytes());
        let parser = ical::IcalParser::new(reader);
        for cal in parser {
            let Ok(cal) = cal else { continue };
            if let Some(event) = cal.events.into_iter().next() {
                return Some(event);
            }
        }
        None
    }

    /// Old params-aware extractor — one FULL parse per property lookup.
    pub fn extract_ical_property_with_params(
        ical_data: &str,
        property_name: &str,
    ) -> Option<(String, HashMap<String, Vec<String>>)> {
        let event = parse_first_vevent(ical_data)?;
        let prop = event
            .properties
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(property_name))?;
        let value = prop.value?;
        if value.trim().is_empty() {
            return None;
        }
        let mut params: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(param_list) = prop.params {
            for (name, values) in param_list {
                params.insert(name.to_ascii_uppercase(), values);
            }
        }
        Some((value.trim().to_string(), params))
    }

    pub fn extract_ical_property(ical_data: &str, property_name: &str) -> Option<String> {
        extract_ical_property_with_params(ical_data, property_name).map(|(v, _p)| v)
    }

    /// Comparable subset of the entity fields `from_ical` derives.
    #[derive(Debug, PartialEq)]
    pub struct BeforeEvent {
        pub summary: String,
        pub description: Option<String>,
        pub location: Option<String>,
        pub start_time: chrono::DateTime<chrono::Utc>,
        pub end_time: chrono::DateTime<chrono::Utc>,
        pub all_day: bool,
        pub rrule: Option<String>,
        pub ical_uid: Option<String>,
        pub recurrence_id: Option<chrono::DateTime<chrono::Utc>>,
    }

    /// Old `from_ical` body (8 extractor calls = 8 full parses), minus
    /// the entity envelope (ids/timestamps — identical on both sides).
    pub fn from_ical(ical_data: &str) -> Result<BeforeEvent, String> {
        let summary = extract_ical_property(ical_data, "SUMMARY").ok_or("Missing SUMMARY")?;
        let (dtstart_value, dtstart_params) =
            extract_ical_property_with_params(ical_data, "DTSTART").ok_or("Missing DTSTART")?;
        let (dtend_value, _dtend_params) =
            extract_ical_property_with_params(ical_data, "DTEND").ok_or("Missing DTEND")?;
        let all_day = dtstart_params
            .get("VALUE")
            .map(|vs| vs.iter().any(|v| v.eq_ignore_ascii_case("DATE")))
            .unwrap_or(false);
        let start_time = parse_ical_datetime(&dtstart_value, all_day)?;
        let end_time = parse_ical_datetime(&dtend_value, all_day)?;
        let description = extract_ical_property(ical_data, "DESCRIPTION");
        let location = extract_ical_property(ical_data, "LOCATION");
        let rrule = extract_ical_property(ical_data, "RRULE");
        let ical_uid = extract_ical_property(ical_data, "UID");
        let recurrence_id = match extract_ical_property_with_params(ical_data, "RECURRENCE-ID") {
            Some((value, params)) => {
                let is_date = params
                    .get("VALUE")
                    .map(|vs| vs.iter().any(|v| v.eq_ignore_ascii_case("DATE")))
                    .unwrap_or(false);
                parse_ical_datetime(&value, is_date).ok()
            }
            None => None,
        };
        Ok(BeforeEvent {
            summary,
            description,
            location,
            start_time,
            end_time,
            all_day,
            rrule,
            ical_uid,
            recurrence_id,
        })
    }

    /// Old datetime parser (verbatim semantics for the two supported forms).
    pub fn parse_ical_datetime(
        value: &str,
        is_date_only: bool,
    ) -> Result<chrono::DateTime<chrono::Utc>, String> {
        use chrono::TimeZone;
        if is_date_only {
            if value.len() != 8 {
                return Err("bad all-day".into());
            }
            let year: i32 = value[0..4].parse().map_err(|_| "year")?;
            let month: u32 = value[4..6].parse().map_err(|_| "month")?;
            let day: u32 = value[6..8].parse().map_err(|_| "day")?;
            return chrono::NaiveDate::from_ymd_opt(year, month, day)
                .map(|d| chrono::Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
                .ok_or_else(|| "date".into());
        }
        if value.len() < 15 || !value.ends_with('Z') {
            return Err(format!("bad datetime {value:?}"));
        }
        let year: i32 = value[0..4].parse().map_err(|_| "year")?;
        let month: u32 = value[4..6].parse().map_err(|_| "month")?;
        let day: u32 = value[6..8].parse().map_err(|_| "day")?;
        let hour: u32 = value[9..11].parse().map_err(|_| "hour")?;
        let minute: u32 = value[11..13].parse().map_err(|_| "minute")?;
        let second: u32 = value[13..15].parse().map_err(|_| "second")?;
        match chrono::NaiveDate::from_ymd_opt(year, month, day) {
            Some(date) => match date.and_hms_opt(hour, minute, second) {
                Some(datetime) => Ok(chrono::Utc.from_utc_datetime(&datetime)),
                None => Err("time".into()),
            },
            None => Err("date".into()),
        }
    }

    /// Old `split_vevents` — per-line uppercase String.
    pub fn split_vevents(ical_data: &str) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut in_event = false;
        let mut current = String::new();
        for raw_line in ical_data.split('\n') {
            let line = raw_line.trim_end_matches('\r');
            let upper = line.trim_start().to_ascii_uppercase();
            if upper.starts_with("BEGIN:VEVENT") {
                in_event = true;
                current.clear();
            }
            if in_event {
                current.push_str(line);
                current.push_str("\r\n");
            }
            if in_event && upper.starts_with("END:VEVENT") {
                blocks.push(std::mem::take(&mut current));
                in_event = false;
            }
        }
        blocks
    }

    /// Old `extract_vevent_chunk` — full uppercase copy of the body.
    pub fn extract_vevent_chunk(ical_data: &str) -> Option<&str> {
        let upper = ical_data.to_ascii_uppercase();
        let begin = upper.find("BEGIN:VEVENT")?;
        let after_begin = &upper[begin..];
        let rel_end = after_begin.find("END:VEVENT")?;
        let end_tag_end = begin + rel_end + "END:VEVENT".len();
        let mut end = end_tag_end;
        if ical_data[end..].starts_with('\r') {
            end += 1;
        }
        if ical_data[end..].starts_with('\n') {
            end += 1;
        }
        Some(&ical_data[begin..end])
    }

    /// Old `group_events_by_uid` — String-keyed map, UID cloned per event.
    pub fn group_events_by_uid<'a>(
        events: &'a [oxicloud::application::dtos::calendar_dto::CalendarEventDto],
    ) -> Vec<Vec<&'a oxicloud::application::dtos::calendar_dto::CalendarEventDto>> {
        let mut order: Vec<String> = Vec::new();
        let mut buckets: HashMap<
            String,
            Vec<&'a oxicloud::application::dtos::calendar_dto::CalendarEventDto>,
        > = HashMap::new();
        for event in events {
            let key = event.ical_uid.clone();
            if !buckets.contains_key(&key) {
                order.push(key.clone());
            }
            buckets.entry(key).or_default().push(event);
        }
        let mut out = Vec::with_capacity(order.len());
        for uid in order {
            let mut bucket = buckets.remove(&uid).unwrap_or_default();
            bucket.sort_by_key(|e| e.recurrence_id.is_some());
            out.push(bucket);
        }
        out
    }
}

// ─── Corpus ─────────────────────────────────────────────────────────────────

/// A realistic ~1.3 KiB VEVENT: params on DTSTART, folded DESCRIPTION,
/// three ATTENDEEs with CN/PARTSTAT, ORGANIZER, VALARM, CATEGORIES,
/// STATUS and X-props. `variant` 0 = timed master with RRULE, 1 = all-day,
/// 2 = exception override (RECURRENCE-ID).
fn build_vevent_body(i: usize, variant: usize) -> String {
    let uid = format!("evt-{i:05}@oxicloud.bench");
    let mut v = String::with_capacity(1400);
    v.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n");
    v.push_str("BEGIN:VEVENT\r\n");
    v.push_str(&format!("UID:{uid}\r\n"));
    v.push_str("DTSTAMP:20260701T120000Z\r\n");
    match variant {
        1 => {
            v.push_str("DTSTART;VALUE=DATE:20260810\r\n");
            v.push_str("DTEND;VALUE=DATE:20260811\r\n");
        }
        2 => {
            v.push_str("DTSTART:20260812T090000Z\r\n");
            v.push_str("DTEND:20260812T100000Z\r\n");
            v.push_str("RECURRENCE-ID:20260812T090000Z\r\n");
        }
        _ => {
            v.push_str("DTSTART:20260805T090000Z\r\n");
            v.push_str("DTEND:20260805T103000Z\r\n");
            v.push_str("RRULE:FREQ=WEEKLY;BYDAY=TU,TH;UNTIL=20261231T000000Z\r\n");
        }
    }
    v.push_str(&format!(
        "SUMMARY:Sprint review #{i} — métricas y datos\r\n"
    ));
    v.push_str(
        "DESCRIPTION:Repaso de los objetivos del sprint con el equipo completo\\, in\r\n cluyendo demo de la nueva vista de fotos y el plan de la ronda de rendimien\r\n to número cuatro.\r\n",
    );
    v.push_str("LOCATION:Sala Turing — 3ª planta\r\n");
    v.push_str("ORGANIZER;CN=Ana García:mailto:ana@example.com\r\n");
    v.push_str(
        "ATTENDEE;CN=Luis Pérez;PARTSTAT=ACCEPTED;ROLE=REQ-PARTICIPANT:mailto:luis@example.com\r\n",
    );
    v.push_str("ATTENDEE;CN=Sam Chen;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:sam@example.com\r\n");
    v.push_str("ATTENDEE;CN=Río Núñez;PARTSTAT=TENTATIVE:mailto:rio@example.com\r\n");
    v.push_str("CATEGORIES:TRABAJO,EQUIPO\r\n");
    v.push_str("STATUS:CONFIRMED\r\n");
    v.push_str("SEQUENCE:2\r\n");
    v.push_str("TRANSP:OPAQUE\r\n");
    v.push_str("X-OXICLOUD-ROUND:4\r\n");
    v.push_str("BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Reminder\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\n");
    v.push_str("END:VEVENT\r\n");
    v.push_str("END:VCALENDAR\r\n");
    v
}

/// N-event import body (master + exception pairs inside one VCALENDAR).
fn build_import_body(n_events: usize) -> String {
    let mut v = String::with_capacity(n_events * 1400);
    v.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Foreign//Client//EN\r\n");
    for i in 0..n_events {
        let single = build_vevent_body(i, i % 3);
        // Extract just the VEVENT block from the standalone body.
        let begin = single.find("BEGIN:VEVENT").unwrap();
        let end = single.find("END:VEVENT").unwrap() + "END:VEVENT\r\n".len();
        v.push_str(&single[begin..end]);
    }
    v.push_str("END:VCALENDAR\r\n");
    v
}

fn make_dto(i: usize, uid: &str, recurrence: Option<DateTime<Utc>>) -> CalendarEventDto {
    CalendarEventDto {
        id: Uuid::from_u128(i as u128).to_string(),
        calendar_id: Uuid::nil().to_string(),
        summary: format!("Evento {i}"),
        description: None,
        location: None,
        start_time: Utc.with_ymd_and_hms(2026, 8, 5, 9, 0, 0).unwrap(),
        end_time: Utc.with_ymd_and_hms(2026, 8, 5, 10, 0, 0).unwrap(),
        all_day: false,
        rrule: None,
        ical_uid: uid.to_string(),
        recurrence_id: recurrence,
        ical_data: build_vevent_body(i, if recurrence.is_some() { 2 } else { 0 }),
        created_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
        updated_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
    }
}

fn p50(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn time_passes<T>(passes: usize, mut f: impl FnMut() -> T) -> f64 {
    let mut per_pass = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t0 = Instant::now();
        black_box(f());
        per_pass.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    p50(per_pass)
}

fn main() {
    let n_events: usize = env::var("BENCH_EVENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let passes: usize = env::var("BENCH_PASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let group_n: usize = env::var("BENCH_GROUP_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000);

    let calendar_id = Uuid::nil();
    let bodies: Vec<String> = (0..n_events).map(|i| build_vevent_body(i, i % 3)).collect();
    let import_body = build_import_body(50);

    println!("bench_caldav_parse — {n_events} bodies, {passes} passes\n");

    // ── [1] from_ical: single-event PUT path ────────────────────────────────
    let t_before = time_passes(passes, || {
        for b in &bodies {
            black_box(before::from_ical(b).expect("before parse"));
        }
    }) / n_events as f64;
    let t_after = time_passes(passes, || {
        for b in &bodies {
            black_box(CalendarEvent::from_ical(calendar_id, b.clone()).expect("after parse"));
        }
    }) / n_events as f64;
    // The AFTER side clones the body (the real API takes it by value) —
    // measure that clone alone so the comparison can subtract it.
    let t_clone = time_passes(passes, || {
        for b in &bodies {
            black_box(b.clone());
        }
    }) / n_events as f64;
    println!("[1] from_ical µs/event (8-parse chain vs single parse)");
    println!("    BEFORE            {t_before:8.2}");
    println!(
        "    AFTER             {t_after:8.2}  (incl. {t_clone:.2} body clone)   {:.1}x",
        t_before / (t_after - t_clone)
    );

    // ── [2] parse_all_events: 50-event import PUT ───────────────────────────
    let t_before_imp = time_passes(passes, || {
        let blocks = before::split_vevents(&import_body);
        let mut out = Vec::with_capacity(blocks.len());
        for block in blocks {
            let wrapped = format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n{}END:VCALENDAR\r\n",
                block,
            );
            out.push(before::from_ical(&wrapped).expect("before import"));
        }
        out
    });
    let t_after_imp = time_passes(passes, || {
        CalendarEvent::parse_all_events(calendar_id, &import_body).expect("after import")
    });
    println!("[2] parse_all_events µs/50-event import body");
    println!("    BEFORE            {t_before_imp:8.1}");
    println!(
        "    AFTER             {t_after_imp:8.1}                              {:.1}x",
        t_before_imp / t_after_imp
    );

    // ── [3] extract_vevent_chunk: REPORT/GET read path ──────────────────────
    let t_chunk_before = time_passes(passes, || {
        for b in &bodies {
            black_box(before::extract_vevent_chunk(b));
        }
    }) / n_events as f64
        * 1000.0;
    let t_chunk_after = time_passes(passes, || {
        for b in &bodies {
            black_box(caldav_bench::extract_vevent_chunk(b));
        }
    }) / n_events as f64
        * 1000.0;
    println!("[3] extract_vevent_chunk ns/event (uppercase copy vs direct scan)");
    println!("    BEFORE            {t_chunk_before:8.0}");
    println!(
        "    AFTER             {t_chunk_after:8.0}                              {:.1}x",
        t_chunk_before / t_chunk_after
    );

    // ── [4] group_events_by_uid: REPORT fold ────────────────────────────────
    // 80% masters, 20% exception overrides sharing a master's UID.
    let dtos: Vec<CalendarEventDto> = (0..group_n)
        .map(|i| {
            if i % 5 == 4 {
                let master = i - 1;
                make_dto(
                    i,
                    &format!("evt-{master:05}@oxicloud.bench"),
                    Some(Utc.with_ymd_and_hms(2026, 8, 12, 9, 0, 0).unwrap()),
                )
            } else {
                make_dto(i, &format!("evt-{i:05}@oxicloud.bench"), None)
            }
        })
        .collect();
    let t_grp_before = time_passes(passes, || black_box(before::group_events_by_uid(&dtos)));
    let t_grp_after = time_passes(passes, || {
        black_box(caldav_bench::group_objects_by_uid(&dtos))
    });
    println!("[4] group_events_by_uid µs/{group_n} events (String keys vs borrowed)");
    println!("    BEFORE            {t_grp_before:8.1}");
    println!(
        "    AFTER             {t_grp_after:8.1}                              {:.1}x",
        t_grp_before / t_grp_after
    );

    // ── [5] Equivalence gates ───────────────────────────────────────────────
    let mut ok = true;

    // Gate A: from_ical field identity across the corpus + edge bodies.
    let mut gate_bodies: Vec<String> = bodies.clone();
    gate_bodies.push(build_vevent_body(9990, 1));
    gate_bodies.push(build_vevent_body(9991, 2));
    // Mixed-case tags + LF-only line endings (foreign client shapes).
    gate_bodies.push(
        "begin:vcalendar\nversion:2.0\nbegin:vevent\nuid:mixed-case@x\nsummary:Mixed Case\ndtstart:20260801T080000Z\ndtend:20260801T090000Z\nend:vevent\nend:vcalendar\n"
            .to_string(),
    );
    for b in &gate_bodies {
        let bf = before::from_ical(b);
        let af = CalendarEvent::from_ical(calendar_id, b.clone());
        match (bf, af) {
            (Ok(bf), Ok(af)) => {
                let same = bf.summary == af.summary()
                    && bf.description.as_deref() == af.description()
                    && bf.location.as_deref() == af.location()
                    && bf.start_time == *af.start_time()
                    && bf.end_time == *af.end_time()
                    && bf.all_day == af.all_day()
                    && bf.rrule.as_deref() == af.rrule()
                    && bf.ical_uid.as_deref() == Some(af.ical_uid())
                    && bf.recurrence_id.as_ref() == af.recurrence_id();
                if !same {
                    eprintln!("GATE A FAIL: field mismatch for body:\n{b}\n before={bf:?}");
                    ok = false;
                }
            }
            (Err(_), Err(_)) => {}
            (bf, af) => {
                eprintln!(
                    "GATE A FAIL: error parity broke (before_ok={} after_ok={}) for body:\n{b}",
                    bf.is_ok(),
                    af.is_ok()
                );
                ok = false;
            }
        }
    }

    // Gate B: parse_all_events equivalence on the import body — same
    // events, same wrapped per-row ical_data.
    let after_events =
        CalendarEvent::parse_all_events(calendar_id, &import_body).expect("import parses");
    let before_blocks = before::split_vevents(&import_body);
    if after_events.len() != before_blocks.len() {
        eprintln!(
            "GATE B FAIL: event count {} != block count {}",
            after_events.len(),
            before_blocks.len()
        );
        ok = false;
    }
    for (evt, block) in after_events.iter().zip(&before_blocks) {
        let wrapped = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//OxiCloud//NONSGML Calendar//EN\r\n{}END:VCALENDAR\r\n",
            block,
        );
        if evt.ical_data() != wrapped {
            eprintln!("GATE B FAIL: wrapped ical_data mismatch");
            ok = false;
            break;
        }
        let bf = before::from_ical(&wrapped).expect("before parses wrapped");
        if bf.summary != evt.summary() || bf.recurrence_id.as_ref() != evt.recurrence_id() {
            eprintln!("GATE B FAIL: field mismatch on wrapped block");
            ok = false;
            break;
        }
    }

    // Gate C: chunk slices byte-identical (incl. mixed-case + no-terminator).
    let mut chunk_bodies = bodies.clone();
    chunk_bodies.push("BEGIN:VCALENDAR\r\nbegin:vevent\r\nUID:x@y\r\nend:vevent".to_string());
    chunk_bodies.push("no vevent here at all".to_string());
    for b in &chunk_bodies {
        if before::extract_vevent_chunk(b) != caldav_bench::extract_vevent_chunk(b) {
            eprintln!("GATE C FAIL: chunk mismatch for body:\n{b}");
            ok = false;
        }
    }

    // Gate D: grouping identity — same UID order, same per-bucket rows.
    let g_before = before::group_events_by_uid(&dtos);
    let g_after = caldav_bench::group_objects_by_uid(&dtos);
    let shape = |g: &Vec<Vec<&CalendarEventDto>>| -> Vec<Vec<(String, bool)>> {
        g.iter()
            .map(|bucket| {
                bucket
                    .iter()
                    .map(|e| (e.id.clone(), e.recurrence_id.is_some()))
                    .collect()
            })
            .collect()
    };
    if shape(&g_before) != shape(&g_after) {
        eprintln!("GATE D FAIL: grouping mismatch");
        ok = false;
    }

    println!(
        "[5] Equivalence gates: {}",
        if ok { "OK (byte-identical)" } else { "FAILED" }
    );
    if !ok {
        std::process::exit(1);
    }
}
