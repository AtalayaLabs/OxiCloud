//! Round-15 tantivy zero-hit snippet skip (no Postgres).
//!
//! `TantivyContentIndex::search_blocking` builds a `SnippetGenerator` from the
//! query right after the `TopDocs` search — but a `SnippetGenerator::create`
//! compiles the query against the index (term lookups + weight build), and when
//! the query matched NO documents that generator is never used (the per-hit
//! loop is empty). The shipped fix returns `Ok(Vec::new())` as soon as
//! `top_docs.is_empty()`, before the create.
//!
//! This bench reproduces the exact skipped operation on a RAM index built with
//! the public tantivy API (same crate + version the service uses):
//!   BEFORE = search (→ 0 hits) + `SnippetGenerator::create` (+ `set_max_num_chars`)
//!   AFTER  = search (→ 0 hits) + `top_docs.is_empty()` early return
//! The delta is the wasted create the fix removes from every no-hit content
//! search. A sanity arm confirms a term that DOES hit still yields a snippet, so
//! the skip only ever triggers on a genuine zero-hit query.
//!
//! Run:
//!   cargo bench --features=bench --bench bench_round15_tantivy
//! Tunables (env): BENCH_ITERS (50000), BENCH_DOCS (400)

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{STORED, STRING, Schema, TEXT, Value as _};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, TantivyDocument, doc};

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

const SNIPPET_MAX_CHARS: usize = 200;

fn main() {
    let iters: usize = env_or("BENCH_ITERS", 50_000);
    let docs: usize = env_or("BENCH_DOCS", 400);

    println!("#################################################################");
    println!("# Round-15 tantivy zero-hit snippet skip");
    println!("#################################################################");

    // ── Build a RAM index: a stored content field + a name field, the shape
    //    the service indexes. Fill it with realistic prose so create() has real
    //    terms to weigh. ────────────────────────────────────────────────────
    let mut schema_builder = Schema::builder();
    let name = schema_builder.add_text_field("name", STRING | STORED);
    let content = schema_builder.add_text_field("content", TEXT | STORED);
    let schema = schema_builder.build();
    let index = Index::create_in_ram(schema);

    const WORDS: &[&str] = &[
        "informe",
        "trimestral",
        "ventas",
        "region",
        "norte",
        "presupuesto",
        "reunion",
        "proyecto",
        "cliente",
        "factura",
        "contrato",
        "entrega",
        "calendario",
        "documento",
        "resumen",
        "analisis",
        "resultados",
        "equipo",
    ];
    {
        let mut writer = index.writer(15_000_000).expect("writer");
        for i in 0..docs {
            let body: String = (0..40)
                .map(|j| WORDS[(i * 7 + j * 13) % WORDS.len()])
                .collect::<Vec<_>>()
                .join(" ");
            writer
                .add_document(doc!(
                    name => format!("doc-{i}.txt"),
                    content => body,
                ))
                .expect("add");
        }
        writer.commit().expect("commit");
    }
    let reader = index.reader().expect("reader");
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(&index, vec![content]);

    // A multi-term query of words that appear in NO document → zero hits, but
    // valid tokens (so the real code reaches the search, not the empty-token
    // guard). These are plausible-but-absent search terms.
    let miss_query = parser
        .parse_query("zzznonexistent quuxfoobar wibblewobble")
        .expect("parse");
    // A query that DOES hit — the sanity arm.
    let hit_query = parser.parse_query("informe ventas").expect("parse");

    // ── Correctness gates ──────────────────────────────────────────────────
    let miss_hits = searcher
        .search(&miss_query, &TopDocs::with_limit(32).order_by_score())
        .expect("search");
    assert!(
        miss_hits.is_empty(),
        "miss query must return zero hits (got {})",
        miss_hits.len()
    );

    let hit_hits = searcher
        .search(&hit_query, &TopDocs::with_limit(32).order_by_score())
        .expect("search");
    assert!(!hit_hits.is_empty(), "hit query must return hits");
    // The generator the fix keeps for real hits still produces a fragment.
    let generator = SnippetGenerator::create(&searcher, &*hit_query, content).expect("gen");
    let (_, addr) = hit_hits[0];
    let d: TantivyDocument = searcher.doc(addr).expect("doc");
    let preview = d
        .get_first(content)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        !generator.snippet(&preview).fragment().is_empty(),
        "a real hit must still yield a snippet fragment"
    );

    // ── BEFORE: search + build the snippet generator even on zero hits. ──────
    let a0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..iters {
        let top = searcher
            .search(&miss_query, &TopDocs::with_limit(32).order_by_score())
            .expect("search");
        let sg = SnippetGenerator::create(&searcher, &*miss_query, content).map(|mut g| {
            g.set_max_num_chars(SNIPPET_MAX_CHARS);
            g
        });
        black_box((top.len(), sg.is_ok()));
    }
    let before_ns = t.elapsed().as_nanos() as f64 / iters as f64;
    let before_allocs = (ALLOC_CALLS.load(Ordering::Relaxed) - a0) as f64 / iters as f64;

    // ── AFTER: search + the shipped early return on an empty result. ─────────
    let a1 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    for _ in 0..iters {
        let top = searcher
            .search(&miss_query, &TopDocs::with_limit(32).order_by_score())
            .expect("search");
        if top.is_empty() {
            black_box(top.len());
            continue;
        }
        // Unreached for the miss query; present so the arm is structurally the
        // shipped code, not a stripped one.
        let sg = SnippetGenerator::create(&searcher, &*miss_query, content).map(|mut g| {
            g.set_max_num_chars(SNIPPET_MAX_CHARS);
            g
        });
        black_box(sg.is_ok());
    }
    let after_ns = t.elapsed().as_nanos() as f64 / iters as f64;
    let after_allocs = (ALLOC_CALLS.load(Ordering::Relaxed) - a1) as f64 / iters as f64;

    println!("\n## zero-hit content search ({docs} docs indexed)");
    println!("| arm | ns/op | allocs/op |");
    println!(
        "| {:<40} | {:>12.1} | {:>10.2} |",
        "BEFORE search + snippet create", before_ns, before_allocs
    );
    println!(
        "| {:<40} | {:>12.1} | {:>10.2} |",
        "AFTER  search + is_empty skip", after_ns, after_allocs
    );
    println!(
        "# {:.2}x wall, {:.2} fewer allocs/op",
        before_ns / after_ns,
        before_allocs - after_allocs
    );

    if after_ns >= before_ns || after_allocs >= before_allocs {
        eprintln!(
            "GATE FAIL [B3]: zero-hit skip did not beat building the snippet generator — rollback"
        );
        std::process::exit(1);
    }

    println!("\nGATE PASS");
}
