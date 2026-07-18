//! Round-5 micro-allocation pack — per-request/per-row churn removed
//! from five hot paths. Each section is BEFORE (verbatim old shape) vs
//! AFTER (the shipped code or its exact pattern), with byte/structure
//! equality gates. No Postgres.
//!
//!   [1] search suggest enrichment: entity clone + 3 field re-clones per
//!       row → consume + move.
//!   [2] `list_readable_by` warm hit: deep `Vec<DriveWithRootName>`
//!       clone per request → `Arc` refcount bump.
//!   [3] SPA listing rows (folder/recent/favorites handlers): raw
//!       `Arc::from` per closed-set display field → `intern_display` /
//!       `intern_mime` lookups.
//!   [4] NC PROPFIND child hrefs: per-row re-encode of username + parent
//!       path (`nc_href`) → prefix precomputed once + name-only encode.
//!   [5] CardDAV REPORT (getetag poll): per-REPORT props clone +
//!       per-contact href String + etag `format!` → borrowed props,
//!       reused href buffer, exact-size quoting.
//!
//! Run (no Postgres needed):
//!   cargo run --release --features bench --example bench_micro_allocs
//! Tunables (env): BENCH_ROWS (5000), BENCH_PASSES (60).

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::{TimeZone, Utc};
use oxicloud::application::adapters::carddav_adapter::{CardDavAdapter, CardDavReportType};
use oxicloud::application::adapters::webdav_adapter::QualifiedName;
use oxicloud::application::dtos::contact_dto::ContactDto;
use oxicloud::application::dtos::display_helpers::{
    category_for, icon_class_for, icon_special_class_for, intern_display, intern_mime,
};
use oxicloud::application::dtos::file_dto::FileDto;
use oxicloud::application::dtos::search_dto::SearchSuggestionItem;
use oxicloud::domain::entities::drive::{Drive, DriveKind};
use oxicloud::domain::entities::file::File;
use oxicloud::domain::repositories::drive_repository::DriveWithRootName;
use oxicloud::interfaces::nextcloud::webdav_handler::nc_href;
use uuid::Uuid;

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

fn p50(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn time_passes<T>(passes: usize, mut f: impl FnMut() -> T) -> f64 {
    let mut per = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t0 = Instant::now();
        black_box(f());
        per.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    p50(per)
}

fn allocs_of<T>(mut f: impl FnMut() -> T) -> u64 {
    let s0 = ALLOC_CALLS.load(Ordering::Relaxed);
    black_box(f());
    ALLOC_CALLS.load(Ordering::Relaxed) - s0
}

// ─── Corpus builders ────────────────────────────────────────────────────────

fn make_files(n: usize) -> Vec<File> {
    (0..n)
        .map(|i| {
            File::from_materialized_row(
                Uuid::from_u128(i as u128).to_string(),
                format!("documento-{i}.pdf"),
                Some("/Personal/Proyectos/2026"),
                1024 + i as u64,
                "application/pdf".to_string(),
                None,
                1_700_000_000,
                1_750_000_000,
                format!("{:032x}", i),
                None,
                None,
            )
            .expect("file")
        })
        .collect()
}

fn compute_relevance(name: &str, q: &str) -> u32 {
    if name.to_lowercase().contains(q) {
        100
    } else {
        50
    }
}

/// The suggest enrichment loop — BEFORE: per-row entity clone + field
/// re-clones (verbatim old shape, icon helper substituted identically
/// on both arms).
fn suggest_before(files: &[File], q: &str) -> Vec<SearchSuggestionItem> {
    let mut out = Vec::new();
    let query_lower = q.to_lowercase();
    for file in files {
        let file_dto = FileDto::from(file.clone());
        let score = compute_relevance(&file_dto.name, &query_lower);
        out.push(SearchSuggestionItem {
            name: file_dto.name.clone(),
            item_type: "file".to_string(),
            id: file_dto.id.clone(),
            path: file_dto.path.clone(),
            // `.into()` bridges the round-9 `Arc<str>` field type; the
            // conversion is identical on both arms so the round-5 delta
            // this bench gates (clone vs move) is unaffected.
            icon_class: icon_class_for(&file_dto.name, &file_dto.mime_type)
                .to_string()
                .into(),
            icon_special_class: icon_special_class_for(&file_dto.name, &file_dto.mime_type)
                .to_string()
                .into(),
            relevance_score: score,
        });
    }
    out
}

/// AFTER: consume + move (the shipped shape).
fn suggest_after(files: Vec<File>, q: &str) -> Vec<SearchSuggestionItem> {
    let mut out = Vec::new();
    let query_lower = q.to_lowercase();
    for file in files {
        let file_dto = FileDto::from(file);
        let score = compute_relevance(&file_dto.name, &query_lower);
        let icon_class = icon_class_for(&file_dto.name, &file_dto.mime_type).to_string();
        let icon_special_class =
            icon_special_class_for(&file_dto.name, &file_dto.mime_type).to_string();
        out.push(SearchSuggestionItem {
            name: file_dto.name,
            item_type: "file".to_string(),
            id: file_dto.id,
            path: file_dto.path,
            // Same `.into()` bridge as the BEFORE arm — see note there.
            icon_class: icon_class.into(),
            icon_special_class: icon_special_class.into(),
            relevance_score: score,
        });
    }
    out
}

fn make_drives(n: usize) -> Vec<DriveWithRootName> {
    (0..n)
        .map(|i| DriveWithRootName {
            drive: Drive {
                id: Uuid::from_u128(i as u128),
                kind: if i == 0 {
                    DriveKind::Personal
                } else {
                    DriveKind::Shared
                },
                default_for_user: (i == 0).then(|| Uuid::from_u128(999)),
                root_folder_id: Uuid::from_u128(1000 + i as u128),
                quota_bytes: Some(10_737_418_240),
                used_bytes: 123_456_789,
                policies: serde_json::json!({}),
                created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                updated_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
            },
            root_folder_name: format!("Drive número {i}"),
            caller_role: None,
        })
        .collect()
}

fn make_contacts(n: usize) -> Vec<ContactDto> {
    (0..n)
        .map(|i| ContactDto {
            id: Uuid::from_u128(i as u128).to_string(),
            uid: format!("contact-{i:05}"),
            etag: format!("{:016x}", i * 2_654_435_761u64 as usize),
            full_name: Some(format!("Persona {i}")),
            ..ContactDto::default()
        })
        .collect()
}

// BEFORE replica of the CardDAV REPORT emitter (props.clone + per-row
// href String + etag format!) for the getetag poll shape — the
// address-data branch is never hit with this prop set, so the replica
// stays self-contained.
mod before_carddav {
    use super::*;
    use quick_xml::Writer;
    use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};

    pub fn generate_contacts_response(
        out: &mut Vec<u8>,
        contacts: &[ContactDto],
        report: &CardDavReportType,
        base_href: &str,
    ) {
        let mut xml_writer = Writer::new(out);
        xml_writer
            .write_event(Event::Start(
                BytesStart::new("D:multistatus").with_attributes([
                    ("xmlns:D", "DAV:"),
                    ("xmlns:CR", "urn:ietf:params:xml:ns:carddav"),
                ]),
            ))
            .unwrap();

        let props = match report {
            CardDavReportType::AddressbookQuery { props } => props.clone(),
            CardDavReportType::AddressbookMultiget { props, .. } => props.clone(),
            CardDavReportType::SyncCollection { props, .. } => props.clone(),
        };

        for contact in contacts {
            let href = format!("{}{}.vcf", base_href, contact.uid);
            xml_writer
                .write_event(Event::Start(BytesStart::new("D:response")))
                .unwrap();
            xml_writer
                .write_event(Event::Start(BytesStart::new("D:href")))
                .unwrap();
            xml_writer
                .write_event(Event::Text(BytesText::new(&href)))
                .unwrap();
            xml_writer
                .write_event(Event::End(BytesEnd::new("D:href")))
                .unwrap();
            xml_writer
                .write_event(Event::Start(BytesStart::new("D:propstat")))
                .unwrap();
            xml_writer
                .write_event(Event::Start(BytesStart::new("D:prop")))
                .unwrap();
            for prop in &props {
                match (prop.namespace.as_str(), prop.name.as_str()) {
                    ("DAV:", "resourcetype") => {
                        xml_writer
                            .write_event(Event::Empty(BytesStart::new("D:resourcetype")))
                            .unwrap();
                    }
                    ("DAV:", "getetag") => {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getetag")))
                            .unwrap();
                        xml_writer
                            .write_event(Event::Text(BytesText::new(&format!(
                                "\"{}\"",
                                contact.etag
                            ))))
                            .unwrap();
                        xml_writer
                            .write_event(Event::End(BytesEnd::new("D:getetag")))
                            .unwrap();
                    }
                    ("DAV:", "getcontenttype") => {
                        xml_writer
                            .write_event(Event::Start(BytesStart::new("D:getcontenttype")))
                            .unwrap();
                        xml_writer
                            .write_event(Event::Text(BytesText::new("text/vcard; charset=utf-8")))
                            .unwrap();
                        xml_writer
                            .write_event(Event::End(BytesEnd::new("D:getcontenttype")))
                            .unwrap();
                    }
                    _ => {}
                }
            }
            xml_writer
                .write_event(Event::End(BytesEnd::new("D:prop")))
                .unwrap();
            xml_writer
                .write_event(Event::Start(BytesStart::new("D:status")))
                .unwrap();
            xml_writer
                .write_event(Event::Text(BytesText::new("HTTP/1.1 200 OK")))
                .unwrap();
            xml_writer
                .write_event(Event::End(BytesEnd::new("D:status")))
                .unwrap();
            xml_writer
                .write_event(Event::End(BytesEnd::new("D:propstat")))
                .unwrap();
            xml_writer
                .write_event(Event::End(BytesEnd::new("D:response")))
                .unwrap();
        }
        xml_writer
            .write_event(Event::End(BytesEnd::new("D:multistatus")))
            .unwrap();
    }
}

fn main() {
    let rows: usize = env::var("BENCH_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000);
    let passes: usize = env::var("BENCH_PASSES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let mut ok = true;

    println!("bench_micro_allocs — {rows} rows, {passes} passes\n");

    // ── [1] suggest enrichment ──────────────────────────────────────────────
    {
        let files = make_files(200); // suggest is limit-bounded (~10-200)
        let t_b = time_passes(passes, || suggest_before(&files, "doc"));
        // Production AFTER consumes the caller's Vec — no clone exists.
        // The replay clone happens OUTSIDE the timed window.
        let t_a = {
            let mut per = Vec::with_capacity(passes);
            for _ in 0..passes {
                let corpus = files.clone();
                let t0 = Instant::now();
                black_box(suggest_after(corpus, "doc"));
                per.push(t0.elapsed().as_secs_f64() * 1e6);
            }
            p50(per)
        };
        // Alloc parity: charge the corpus clone to neither arm by
        // measuring BEFORE with its borrow (clones inside) and AFTER
        // seeded from a pre-cloned Vec outside the counter window.
        let a_b = allocs_of(|| suggest_before(&files, "doc")) as f64 / files.len() as f64;
        let mut pre = Some(files.clone());
        let a_a =
            allocs_of(|| suggest_after(pre.take().unwrap(), "doc")) as f64 / files.len() as f64;
        let g_b = suggest_before(&files, "doc");
        let g_a = suggest_after(files.clone(), "doc");
        let same = g_b.len() == g_a.len()
            && g_b.iter().zip(&g_a).all(|(x, y)| {
                x.name == y.name && x.id == y.id && x.path == y.path && x.icon_class == y.icon_class
            });
        if !same {
            eprintln!("GATE FAIL suggest");
            ok = false;
        }
        println!("[1] suggest enrichment (200 rows)   µs/pass   allocs/row");
        println!("    BEFORE (clone per row)         {t_b:8.1}   {a_b:7.2}");
        println!(
            "    AFTER  (consume + move)        {t_a:8.1}   {a_a:7.2}   {:.2}x",
            t_b / t_a
        );
    }

    // ── [2] readable-drives warm hit ────────────────────────────────────────
    {
        let value = Arc::new(make_drives(3));
        let cache: moka::sync::Cache<Uuid, Arc<Vec<DriveWithRootName>>> =
            moka::sync::Cache::new(100);
        let user = Uuid::from_u128(42);
        cache.insert(user, value);
        let hit_before = || {
            let arc = cache.get(&user).expect("warm");
            let v: Vec<DriveWithRootName> = (*arc).clone(); // old: deep clone out
            v
        };
        let hit_after = || cache.get(&user).expect("warm"); // new: Arc bump
        let n_iters = 10_000u32;
        let t_b = time_passes(passes, || {
            for _ in 0..n_iters {
                black_box(hit_before());
            }
        }) / n_iters as f64
            * 1000.0;
        let t_a = time_passes(passes, || {
            for _ in 0..n_iters {
                black_box(hit_after());
            }
        }) / n_iters as f64
            * 1000.0;
        let a_b = allocs_of(hit_before);
        let a_a = allocs_of(hit_after);
        let g = hit_before();
        let ga = hit_after();
        if g.len() != ga.len() || g[0].root_folder_name != ga[0].root_folder_name {
            eprintln!("GATE FAIL readable hit");
            ok = false;
        }
        println!("[2] list_readable_by warm hit (3 drives)   ns/hit   allocs/hit");
        println!("    BEFORE (deep Vec clone)              {t_b:8.1}   {a_b:7}");
        println!(
            "    AFTER  (Arc refcount bump)           {t_a:8.1}   {a_a:7}   {:.1}x",
            t_b / t_a
        );
    }

    // ── [3] SPA listing closed-set fields ───────────────────────────────────
    {
        let names: Vec<String> = (0..rows).map(|i| format!("informe-{i}.pdf")).collect();
        let mime = "application/pdf";
        let row_before = |name: &str| {
            (
                Arc::<str>::from(mime),
                Arc::<str>::from(icon_class_for(name, mime)),
                Arc::<str>::from(icon_special_class_for(name, mime)),
                Arc::<str>::from(category_for(name, mime)),
            )
        };
        let row_after = |name: &str| {
            (
                intern_mime(mime),
                intern_display(icon_class_for(name, mime)),
                intern_display(icon_special_class_for(name, mime)),
                intern_display(category_for(name, mime)),
            )
        };
        let t_b = time_passes(passes, || {
            for n in &names {
                black_box(row_before(n));
            }
        }) / rows as f64
            * 1000.0;
        let t_a = time_passes(passes, || {
            for n in &names {
                black_box(row_after(n));
            }
        }) / rows as f64
            * 1000.0;
        let a_b = allocs_of(|| row_before(&names[0]));
        let a_a = allocs_of(|| row_after(&names[0]));
        let (bm, bi, bs, bc) = row_before(&names[0]);
        let (am, ai, as_, ac) = row_after(&names[0]);
        if *bm != *am || *bi != *ai || *bs != *as_ || *bc != *ac {
            eprintln!("GATE FAIL interning content");
            ok = false;
        }
        println!("[3] listing closed-set fields             ns/row   allocs/row");
        println!("    BEFORE (Arc::from ×4)                {t_b:8.1}   {a_b:7}");
        println!(
            "    AFTER  (intern lookups ×4)           {t_a:8.1}   {a_a:7}   {:.1}x",
            t_b / t_a
        );
    }

    // ── [4] NC PROPFIND child hrefs ─────────────────────────────────────────
    {
        let username = "ana.garcia";
        let subpath = "Personal/Proyectos 2026/Diseño";
        let names: Vec<String> = (0..rows)
            .map(|i| format!("archivo con espacios {i}.png"))
            .collect();
        // Verbatim replica of the production shape — `subpath` is a
        // const here, so the emptiness test is statically known.
        #[allow(clippy::const_is_empty)]
        let href_before = |name: &str| {
            let child_sub = if subpath.is_empty() {
                name.to_string()
            } else {
                format!("{}/{}", subpath.trim_end_matches('/'), name)
            };
            nc_href(username, &child_sub)
        };
        let prefix = {
            let base = nc_href(username, subpath);
            if base.ends_with('/') {
                base
            } else {
                format!("{base}/")
            }
        };
        let href_after = |name: &str| format!("{}{}", prefix, urlencoding::encode(name));
        let t_b = time_passes(passes, || {
            for n in &names {
                black_box(href_before(n));
            }
        }) / rows as f64
            * 1000.0;
        let t_a = time_passes(passes, || {
            for n in &names {
                black_box(href_after(n));
            }
        }) / rows as f64
            * 1000.0;
        let a_b = allocs_of(|| href_before(&names[0]));
        let a_a = allocs_of(|| href_after(&names[0]));
        for n in names.iter().take(50) {
            if href_before(n) != href_after(n) {
                eprintln!("GATE FAIL href: {} != {}", href_before(n), href_after(n));
                ok = false;
                break;
            }
        }
        println!("[4] NC child hrefs (depth-3 parent)       ns/row   allocs/row");
        println!("    BEFORE (nc_href per row)             {t_b:8.1}   {a_b:7}");
        println!(
            "    AFTER  (prefix + name encode)        {t_a:8.1}   {a_a:7}   {:.1}x",
            t_b / t_a
        );
    }

    // ── [5] CardDAV REPORT getetag poll ─────────────────────────────────────
    {
        let contacts = make_contacts(rows);
        let report = CardDavReportType::AddressbookQuery {
            props: vec![
                QualifiedName::new("DAV:", "getetag"),
                QualifiedName::new("DAV:", "getcontenttype"),
            ],
        };
        let base = "/carddav/libreta/";
        let run_before = || {
            let mut out = Vec::with_capacity(contacts.len() * 256);
            before_carddav::generate_contacts_response(&mut out, &contacts, &report, base);
            out
        };
        let run_after = || {
            let mut out = Vec::with_capacity(contacts.len() * 256);
            CardDavAdapter::generate_contacts_response(
                &mut out,
                &contacts,
                &report,
                base,
                &[],
                None,
            )
            .expect("generate");
            out
        };
        let t_b = time_passes(passes.min(30), run_before);
        let t_a = time_passes(passes.min(30), run_after);
        let xb = run_before();
        let xa = run_after();
        if xb != xa {
            let at = xb.iter().zip(&xa).position(|(a, b)| a != b).unwrap_or(0);
            eprintln!(
                "GATE FAIL carddav at byte {at}: …{}… vs …{}…",
                String::from_utf8_lossy(&xb[at.saturating_sub(60)..(at + 60).min(xb.len())]),
                String::from_utf8_lossy(&xa[at.saturating_sub(60)..(at + 60).min(xa.len())]),
            );
            ok = false;
        }
        println!("[5] CardDAV REPORT getetag ({rows} contacts)   µs/report");
        println!("    BEFORE (clone + format! churn)       {t_b:8.1}");
        println!(
            "    AFTER  (borrow + reuse + exact-size) {t_a:8.1}            {:.2}x",
            t_b / t_a
        );
    }

    println!(
        "\n[gate] {}",
        if ok {
            "OK (identical outputs)"
        } else {
            "FAILED"
        }
    );
    if !ok {
        std::process::exit(1);
    }
}
