//! Search-results cache memory benchmark — entry-count bound vs byte bound.
//!
//! The search cache keys pages by user × query × offset × limit, and each
//! page holds up to 500 enriched rows (`MAX_SEARCH_LIMIT`) of owned Strings.
//! Bounded by ENTRY COUNT (the old scheme: `max_capacity(1000)` + TTL), a
//! burst of keystrokes/pages/users could pin ~300 MB of invisible RSS for
//! the 5-minute TTL. Bounded by BYTES (a `weigher` + 32 MiB budget — the
//! same pattern as the file-content and dedup-manifest caches), retention
//! can never exceed the budget.
//!
//! Two sub-phases over the same synthetic corpus (1,000 pages × 500 rows,
//! ~150-char paths, realistic field contents):
//!   * BEFORE — a moka cache configured exactly as the old production wiring
//!     (entry-count 1000 + 300 s TTL).
//!   * AFTER  — `build_search_results_cache(...)`, the *identical* function
//!     production now uses (weigher + 32 MiB + 300 s TTL).
//!
//! Reported per phase: entries retained, retained bytes (recomputed with the
//! production weigher after `run_pending_tasks`), best-effort process memory
//! (`VmHWM`/`VmRSS` from /proc/self/status), and hot-key `get()` p50 over
//! 100k reads (proves the weigher — which only runs on insert — does not
//! slow reads).
//!
//! NOTE on RSS: `VmHWM` is a monotonic high-water mark and the allocator may
//! keep freed pages, so the AFTER phase (which runs second, after a full
//! drop of the BEFORE cache) cannot show a peak below the BEFORE peak.
//! Treat the RSS columns as best-effort corroboration; the authoritative
//! metric is the weigher-recomputed retained bytes.
//!
//! Gates (exit code 1 on failure):
//!   * AFTER retained bytes ≤ 32 MiB budget
//!   * BEFORE retained bytes ≥ 8× the budget (measured ≈9–10×)
//!   * AFTER get() p50 within 20% of BEFORE
//!
//! No Postgres needed.
//! Run: `cargo run --release --features bench --example bench_search_cache_mem`

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxicloud::application::dtos::search_dto::{SearchFileResultDto, SearchResultsDto};
use oxicloud::application::services::search_service::{
    build_search_results_cache, search_results_entry_weight,
};

/// Distinct cached pages inserted per phase (≈ users × queries × pages).
const ENTRIES: u64 = 1_000;
/// Rows per page — the handler's `MAX_SEARCH_LIMIT` clamp.
const ROWS_PER_ENTRY: usize = 500;
/// Production TTL (unchanged by the fix).
const TTL_SECS: u64 = 300;
/// The old production bound: 1000 ENTRIES, blind to entry size.
const BEFORE_MAX_ENTRIES: u64 = 1_000;
/// The new production bound: 32 MiB of weighed bytes.
const AFTER_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Hot-key reads per phase for the p50 latency comparison.
const GETS: usize = 100_000;

const MIB: f64 = 1024.0 * 1024.0;

// ---------------------------------------------------------------------------
// Deterministic synthetic corpus (no rand dependency)
// ---------------------------------------------------------------------------

/// Tiny xorshift64 PRNG — fast, deterministic, no dependency.
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Lowercase-hex string of `chars` nibbles.
fn pseudo_hex(state: &mut u64, chars: usize) -> String {
    let mut s = String::with_capacity(chars);
    while s.len() < chars {
        let block = format!("{:016x}", xorshift(state));
        let take = (chars - s.len()).min(16);
        s.push_str(&block[..take]);
    }
    s
}

/// 36-char UUID-shaped string (8-4-4-4-12), like the real `Uuid::to_string()`
/// ids that populate `SearchFileResultDto::id` / `folder_id`.
fn pseudo_uuid(state: &mut u64) -> String {
    let h = pseudo_hex(state, 32);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// One synthetic 500-row search page with realistic field contents:
/// UUID ids, ~30-char names, ~150-char nested drive paths, real MIME types,
/// 64-hex BLAKE3 blob hashes, icon/category metadata, and a content-index
/// snippet on every 8th row.
fn synth_entry(idx: u64) -> Arc<SearchResultsDto> {
    const MIMES: [&str; 4] = [
        "application/pdf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "image/jpeg",
        "text/markdown",
    ];
    const SNIPPET: &str = "…the quarterly numbers show a steady increase in storage usage \
         across all departments, with the engineering share growing fastest and…";

    let mut rng = idx.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    let mut files = Vec::with_capacity(ROWS_PER_ENTRY);
    for row in 0..ROWS_PER_ENTRY {
        let name = format!(
            "quarterly_report_{:04}_rev{:03}.pdf",
            xorshift(&mut rng) % 10_000,
            row % 1_000
        );
        let path = format!(
            "/drives/{}/Departments/Engineering/Projects/oxicloud-benchmarks/2026/Q{}/weekly-sync-notes/attachments/{}",
            pseudo_uuid(&mut rng),
            row % 4 + 1,
            name
        );
        let content_hit = row % 8 == 0;
        let match_source = if content_hit { "content" } else { "name" };
        files.push(SearchFileResultDto {
            id: pseudo_uuid(&mut rng),
            name,
            path,
            size: 831_942,
            mime_type: MIMES[row % MIMES.len()].into(),
            folder_id: Some(pseudo_uuid(&mut rng)),
            created_at: 1_752_700_000,
            modified_at: 1_752_800_000,
            relevance_score: 50,
            size_formatted: "812.4 KB".to_string(),
            icon_class: "fas fa-file-pdf".into(),
            icon_special_class: "pdf-icon".into(),
            category: "document".into(),
            blob_hash: pseudo_hex(&mut rng, 64),
            snippet: content_hit.then(|| SNIPPET.to_string()),
            match_source: Some(match_source.to_string()),
            etag: pseudo_hex(&mut rng, 16),
            created_by: None,
            updated_by: None,
            is_favorite: false,
            is_shared: false,
        });
    }

    Arc::new(SearchResultsDto::new(
        files,
        Vec::new(),
        ROWS_PER_ENTRY,
        0,
        Some(12_345),
        3,
        "relevance".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Best-effort process memory (Linux /proc; "n/a" elsewhere)
// ---------------------------------------------------------------------------

/// Read a kB-valued field (`VmHWM`, `VmRSS`) from /proc/self/status.
fn status_kb(field: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    text.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|kb| kb.parse().ok())
}

fn fmt_kb(v: Option<u64>) -> String {
    match v {
        Some(kb) => format!("{:.1} MiB", kb as f64 / 1024.0),
        None => "n/a".to_string(),
    }
}

fn fmt_kb_delta(start: Option<u64>, end: Option<u64>) -> String {
    match (start, end) {
        (Some(s), Some(e)) => format!("{:+.1} MiB", (e as f64 - s as f64) / 1024.0),
        _ => "n/a".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Phase runner
// ---------------------------------------------------------------------------

struct PhaseReport {
    retained_entries: u64,
    retained_bytes: u64,
    hwm_start_kb: Option<u64>,
    hwm_end_kb: Option<u64>,
    rss_start_kb: Option<u64>,
    rss_end_kb: Option<u64>,
    p50_get_ns: u64,
}

/// Insert the full corpus, settle the cache, then measure retention and
/// hot-key read latency. Identical for both variants — only the cache
/// configuration differs.
async fn run_phase(cache: &moka::future::Cache<u64, Arc<SearchResultsDto>>) -> PhaseReport {
    let hwm_start_kb = status_kb("VmHWM");
    let rss_start_kb = status_kb("VmRSS");

    for i in 0..ENTRIES {
        cache.insert(i, synth_entry(i)).await;
        // Let eviction run as it would under live traffic, so evicted pages
        // are actually freed instead of piling up in moka's pending queue.
        if i % 64 == 0 {
            cache.run_pending_tasks().await;
        }
    }
    cache.run_pending_tasks().await;

    let retained_entries = cache.entry_count();
    // Recompute retained bytes with the production weigher — for the BEFORE
    // variant this is exactly the memory its entry-count bound was blind to.
    let retained_bytes: u64 = cache
        .iter()
        .map(|(k, v)| u64::from(search_results_entry_weight(&k, &v)))
        .sum();

    // Hot-key read latency: p50 over GETS reads of one resident key.
    let hot: u64 = *cache.iter().next().expect("cache is empty after fill").0;
    for _ in 0..1_000 {
        black_box(cache.get(&hot).await); // warmup
    }
    let mut lat_ns = Vec::with_capacity(GETS);
    for _ in 0..GETS {
        let t = Instant::now();
        let v = cache.get(&hot).await;
        lat_ns.push(t.elapsed().as_nanos() as u64);
        black_box(v);
    }
    lat_ns.sort_unstable();
    let p50_get_ns = lat_ns[lat_ns.len() / 2];

    PhaseReport {
        retained_entries,
        retained_bytes,
        hwm_start_kb,
        hwm_end_kb: status_kb("VmHWM"),
        rss_start_kb,
        rss_end_kb: status_kb("VmRSS"),
        p50_get_ns,
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let entry_weight = u64::from(search_results_entry_weight(&0, &synth_entry(0)));
    println!("\n###########################################################");
    println!("# Search-results cache: entry-count bound vs byte bound");
    println!(
        "# corpus: {ENTRIES} pages x {ROWS_PER_ENTRY} rows, ~{:.0} KiB/page (weigher)",
        entry_weight as f64 / 1024.0
    );
    println!(
        "# BEFORE: max_capacity({BEFORE_MAX_ENTRIES}) entries + {TTL_SECS}s TTL (old di.rs wiring)"
    );
    println!(
        "# AFTER : build_search_results_cache({TTL_SECS}, {} MiB) — production fn",
        AFTER_MAX_BYTES as f64 / MIB
    );
    println!("###########################################################\n");

    // --- Phase 1: BEFORE (entry-count bound, exactly the old wiring) ---
    let before_cache: moka::future::Cache<u64, Arc<SearchResultsDto>> =
        moka::future::Cache::builder()
            .max_capacity(BEFORE_MAX_ENTRIES)
            .time_to_live(Duration::from_secs(TTL_SECS))
            .build();
    let before = run_phase(&before_cache).await;
    // Full drop between phases so the AFTER numbers never sit on top of the
    // BEFORE cache's live memory.
    drop(before_cache);

    // --- Phase 2: AFTER (weigher + byte budget, the production builder) ---
    let after_cache = build_search_results_cache(TTL_SECS, AFTER_MAX_BYTES);
    let after = run_phase(&after_cache).await;

    // --- Report ---
    println!("| metric | BEFORE (1000 entries + TTL) | AFTER (weigher + 32 MiB) |");
    println!("|---|---|---|");
    println!(
        "| entries retained | {} | {} |",
        before.retained_entries, after.retained_entries
    );
    println!(
        "| retained bytes (weigher) | {:.1} MiB | {:.1} MiB |",
        before.retained_bytes as f64 / MIB,
        after.retained_bytes as f64 / MIB
    );
    println!(
        "| byte budget | n/a (entry-count bound) | {:.0} MiB |",
        AFTER_MAX_BYTES as f64 / MIB
    );
    println!(
        "| VmHWM phase delta (best-effort) | {} | {} |",
        fmt_kb_delta(before.hwm_start_kb, before.hwm_end_kb),
        fmt_kb_delta(after.hwm_start_kb, after.hwm_end_kb)
    );
    println!(
        "| VmRSS start -> end | {} -> {} | {} -> {} |",
        fmt_kb(before.rss_start_kb),
        fmt_kb(before.rss_end_kb),
        fmt_kb(after.rss_start_kb),
        fmt_kb(after.rss_end_kb)
    );
    println!(
        "| get() p50, hot key ({GETS} reads) | {} ns | {} ns |",
        before.p50_get_ns, after.p50_get_ns
    );
    println!(
        "\nRSS note: VmHWM is monotonic and the allocator may retain freed pages, \
         so the AFTER phase (running second) cannot peak below the BEFORE peak; \
         the weigher-recomputed retained bytes are the authoritative comparison."
    );

    // --- Gates ---
    let before_ratio = before.retained_bytes as f64 / AFTER_MAX_BYTES as f64;
    let lat_ratio = after.p50_get_ns as f64 / before.p50_get_ns.max(1) as f64;
    let gate_after_bounded = after.retained_bytes <= AFTER_MAX_BYTES;
    let gate_before_unbounded = before_ratio >= 8.0;
    let gate_latency = lat_ratio <= 1.2;

    println!("\n| gate | condition | measured | result |");
    println!("|---|---|---|---|");
    println!(
        "| AFTER bounded | retained <= 32 MiB budget | {:.1} MiB | {} |",
        after.retained_bytes as f64 / MIB,
        if gate_after_bounded { "PASS" } else { "FAIL" }
    );
    println!(
        "| BEFORE unbounded | retained >= 8x budget (~10x expected) | {before_ratio:.1}x | {} |",
        if gate_before_unbounded {
            "PASS"
        } else {
            "FAIL"
        }
    );
    println!(
        "| read parity | AFTER p50 <= 1.2x BEFORE p50 | {lat_ratio:.2}x | {} |",
        if gate_latency { "PASS" } else { "FAIL" }
    );

    if !(gate_after_bounded && gate_before_unbounded && gate_latency) {
        eprintln!("\nbench_search_cache_mem: GATE FAILURE");
        std::process::exit(1);
    }
    println!("\nAll gates passed.");
}
