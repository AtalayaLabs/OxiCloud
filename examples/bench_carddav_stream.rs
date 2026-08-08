//! CardDAV whole-book response benchmark — buffered vs cursor streaming
//! (ROUND6).
//!
//! The REPORT path (addressbook-query, sync-collection) and the depth-1
//! collection PROPFIND materialised EVERY contact DTO of the book in
//! one Vec, then rendered the complete multistatus into a second in-RAM
//! buffer — the book resident twice, TTFB = full generation. AFTER
//! streams ONE ordered scan (`full_name, first_name, last_name`, the
//! buffered listing's order) through a PG cursor and emits fixed-size
//! pages (contacts carry no bundling constraint).
//!
//! Drives the REAL repository + adapter writers both ways at the repo
//! layer (authz identical both sides, excluded). Gates: streamed
//! concatenation byte-identical to the buffered output for the REPORT
//! (getetag poll shape) AND the collection PROPFIND (allprop), seeded
//! with strictly distinct names so ordering is deterministic.
//!
//! Run (needs Postgres up; reads DATABASE_URL from .env):
//!   cargo run --release --features bench --example bench_carddav_stream
//! Tunables (env): BENCH_CONTACTS (8000), BENCH_PAGE (500), BENCH_PASSES (9).

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use oxicloud::application::adapters::carddav_adapter::{CardDavAdapter, CardDavReportType};
use oxicloud::application::adapters::webdav_adapter::{
    PropFindRequest, PropFindType, QualifiedName,
};
use oxicloud::application::dtos::address_book_dto::AddressBookDto;
use oxicloud::application::dtos::contact_dto::ContactDto;
use oxicloud::domain::repositories::contact_repository::ContactRepository;
use oxicloud::infrastructure::repositories::pg::ContactPgRepository;
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

struct Seeded {
    book_id: Uuid,
    owner_id: Uuid,
}

async fn seed(pool: &PgPool, n: usize) -> Seeded {
    let owner_id: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_cardstream', 'bench_cardstream@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("seed user");
    let book_id: Uuid = sqlx::query_scalar(
        "INSERT INTO carddav.address_books (id, name, owner_id)
         VALUES (gen_random_uuid(), 'Libreta grande', $1) RETURNING id",
    )
    .bind(owner_id)
    .fetch_one(pool)
    .await
    .expect("seed book");

    let mut tx = pool.begin().await.expect("begin");
    for i in 0..n {
        // Strictly distinct full_names keep the listing order (and thus
        // the byte gate) deterministic. Every production row carries its
        // full serialized vCard — the payload whose double-residency the
        // streaming path removes — so the seed does too (~250 B each).
        let uid = format!("contact-{i:06}");
        let vcard = format!(
            "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:Persona {i:06}\r\nN:Apellido{i};Nombre{i};;;\r\nEMAIL;TYPE=INTERNET:persona{i}@bench.invalid\r\nTEL;TYPE=CELL:+34 600 {i:06}\r\nORG:OxiCloud Bench\r\nNOTE:Fila sintetica del banco de pruebas CardDAV.\r\nEND:VCARD\r\n"
        );
        sqlx::query(
            "INSERT INTO carddav.contacts
                 (id, address_book_id, uid, full_name, first_name, last_name, vcard, etag)
             VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(book_id)
        .bind(&uid)
        .bind(format!("Persona {i:06}"))
        .bind(format!("Nombre{i}"))
        .bind(format!("Apellido{i}"))
        .bind(&vcard)
        .bind(format!("{:016x}", (i as u64).wrapping_mul(2_654_435_761)))
        .execute(&mut *tx)
        .await
        .expect("seed contact");
    }
    tx.commit().await.expect("commit");
    Seeded { book_id, owner_id }
}

async fn cleanup(pool: &PgPool, s: &Seeded) {
    let _ = sqlx::query("DELETE FROM carddav.contacts WHERE address_book_id = $1")
        .bind(s.book_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM carddav.address_books WHERE id = $1")
        .bind(s.book_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(s.owner_id)
        .execute(pool)
        .await;
}

fn report_shape() -> CardDavReportType {
    CardDavReportType::AddressbookQuery {
        props: vec![
            QualifiedName::new("DAV:", "getetag"),
            QualifiedName::new("DAV:", "getcontenttype"),
        ],
    }
}

async fn fetch_all_dtos(repo: &ContactPgRepository, book_id: &Uuid) -> Vec<ContactDto> {
    repo.get_contacts_by_address_book(book_id)
        .await
        .expect("list contacts")
        .into_iter()
        .map(ContactDto::from)
        .collect()
}

/// BEFORE: full fetch + whole-response buffer. First byte exists only
/// when everything does.
async fn buffered_report(
    repo: &ContactPgRepository,
    book_id: &Uuid,
    base_href: &str,
) -> (f64, Vec<u8>) {
    let t0 = Instant::now();
    let contacts = fetch_all_dtos(repo, book_id).await;
    let mut out = Vec::with_capacity(contacts.len() * 256);
    CardDavAdapter::generate_contacts_response(
        &mut out,
        &contacts,
        &report_shape(),
        base_href,
        &[],
        None,
    )
    .expect("generate");
    (t0.elapsed().as_secs_f64() * 1e3, out)
}

/// AFTER: cursor + page writers (the handler loop over public pieces).
/// Returns (ttfb_ms — first data page rendered, wall_ms, bytes).
async fn streamed_report(
    repo: &ContactPgRepository,
    book_id: &Uuid,
    base_href: &str,
    page_rows: usize,
    accumulate: bool,
) -> (f64, f64, Vec<u8>) {
    use futures::TryStreamExt;
    let t0 = Instant::now();
    let mut ttfb = None;
    let mut all = Vec::new();
    let report = report_shape();

    let mut chunk = Vec::with_capacity(160);
    {
        let mut w = quick_xml::Writer::new(&mut chunk);
        CardDavAdapter::write_report_multistatus_start(&mut w).expect("start");
    }
    if accumulate {
        all.extend_from_slice(&chunk);
    }

    let mut rows = repo.stream_contacts_by_book(*book_id);
    let mut page: Vec<ContactDto> = Vec::with_capacity(page_rows);
    loop {
        let next = rows
            .try_next()
            .await
            .expect("stream row")
            .map(ContactDto::from);
        let flush = match &next {
            Some(_) => page.len() >= page_rows,
            None => !page.is_empty(),
        };
        if flush {
            let mut chunk = Vec::with_capacity(page.len() * 256 + 64);
            {
                let mut w = quick_xml::Writer::new(&mut chunk);
                CardDavAdapter::write_contacts_report_page(&mut w, &page, &report, base_href)
                    .expect("page");
            }
            ttfb.get_or_insert_with(|| t0.elapsed().as_secs_f64() * 1e3);
            page.clear();
            if accumulate {
                all.extend_from_slice(&chunk);
            }
            std::hint::black_box(&chunk);
        }
        match next {
            Some(c) => page.push(c),
            None => break,
        }
    }

    let mut chunk = Vec::with_capacity(32);
    {
        let mut w = quick_xml::Writer::new(&mut chunk);
        CardDavAdapter::write_carddav_multistatus_end(&mut w).expect("end");
    }
    if accumulate {
        all.extend_from_slice(&chunk);
    }
    (
        ttfb.unwrap_or(f64::NAN),
        t0.elapsed().as_secs_f64() * 1e3,
        all,
    )
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

fn book_dto(seeded: &Seeded) -> AddressBookDto {
    AddressBookDto {
        id: seeded.book_id.to_string(),
        name: "Libreta grande".to_string(),
        owner_id: seeded.owner_id.to_string(),
        ..AddressBookDto::default()
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let n: usize = env::var("BENCH_CONTACTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8000);
    let page_rows: usize = env::var("BENCH_PAGE")
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
    let repo = ContactPgRepository::new(pool.clone());
    let base_href = format!("/carddav/{}/", seeded.book_id);

    println!("bench_carddav_stream — {n} contacts, page={page_rows}, {passes} passes\n");

    // ── Equivalence gates ───────────────────────────────────────────────────
    let (_, before_bytes) = buffered_report(&repo, &seeded.book_id, &base_href).await;
    let (_, _, after_bytes) =
        streamed_report(&repo, &seeded.book_id, &base_href, page_rows, true).await;
    let gate_report = before_bytes == after_bytes;

    // Collection PROPFIND (allprop): buffered generator vs head+pages.
    let request = PropFindRequest {
        prop_find_type: PropFindType::AllProp,
    };
    let book = book_dto(&seeded);
    let contacts_all = fetch_all_dtos(&repo, &seeded.book_id).await;
    let mut coll_before = Vec::new();
    CardDavAdapter::generate_addressbook_collection_propfind(
        &mut coll_before,
        &book,
        &contacts_all,
        &request,
        &base_href,
        "1",
    )
    .expect("collection");
    drop(contacts_all);
    let coll_after = {
        use futures::TryStreamExt;
        let mut out = Vec::new();
        {
            let mut w = quick_xml::Writer::new(&mut out);
            CardDavAdapter::write_collection_head(&mut w, &book, &request, &base_href)
                .expect("head");
        }
        let mut rows = repo.stream_contacts_by_book(seeded.book_id);
        let mut page: Vec<ContactDto> = Vec::with_capacity(page_rows);
        loop {
            let next = rows
                .try_next()
                .await
                .expect("stream row")
                .map(ContactDto::from);
            let flush = match &next {
                Some(_) => page.len() >= page_rows,
                None => !page.is_empty(),
            };
            if flush {
                let mut w = quick_xml::Writer::new(&mut out);
                CardDavAdapter::write_collection_contact_page(&mut w, &page, &base_href)
                    .expect("page");
                page.clear();
            }
            match next {
                Some(c) => page.push(c),
                None => break,
            }
        }
        let mut w = quick_xml::Writer::new(&mut out);
        CardDavAdapter::write_carddav_multistatus_end(&mut w).expect("end");
        out
    };
    let gate_coll = coll_before == coll_after;
    drop(coll_before);
    drop(coll_after);

    // ── [1] REPORT timing + peak ────────────────────────────────────────────
    let mut b_wall = Vec::new();
    let mut a_wall = Vec::new();
    let mut a_ttfb = Vec::new();
    for _ in 0..passes {
        let (w, out) = buffered_report(&repo, &seeded.book_id, &base_href).await;
        std::hint::black_box(out);
        b_wall.push(w);
        let (t, w, _) = streamed_report(&repo, &seeded.book_id, &base_href, page_rows, false).await;
        a_ttfb.push(t);
        a_wall.push(w);
    }
    reset_peak();
    let (_, out) = buffered_report(&repo, &seeded.book_id, &base_href).await;
    drop(out);
    let peak_before = peak_mib();
    reset_peak();
    let _ = streamed_report(&repo, &seeded.book_id, &base_href, page_rows, false).await;
    let peak_after = peak_mib();

    let bw = p50(b_wall);
    let aw = p50(a_wall);
    let at = p50(a_ttfb);
    println!("[1] REPORT addressbook-query (getetag)   TTFB ms   wall ms   peak heap MiB");
    println!("    BEFORE (buffered)                   {bw:8.1}  {bw:8.1}   {peak_before:10.1}");
    println!(
        "    AFTER  (cursor stream)              {at:8.1}  {aw:8.1}   {peak_after:10.1}   TTFB {:.1}x, heap {:.1}x lower",
        bw / at,
        peak_before / peak_after
    );

    cleanup(&pool, &seeded).await;

    println!(
        "\n[gate] REPORT byte-identical: {} · collection PROPFIND byte-identical: {}",
        if gate_report { "OK" } else { "FAILED" },
        if gate_coll { "OK" } else { "FAILED" }
    );
    if !gate_report || !gate_coll {
        std::process::exit(1);
    }
}
