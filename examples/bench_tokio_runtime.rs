//! Tokio runtime tuning benchmark — worker over-subscription + blocking-pool RSS.
//!
//! Measures the two things `build_runtime` changes versus the `#[tokio::main]`
//! defaults, on a realistic mix of async work + CPU-bound `spawn_blocking`.
//!
//! Part A — worker over-subscription (throughput + tail latency)
//!   Drives `concurrency` async "requests", each doing an async hop
//!   (`yield_now`) plus inline CPU work (BLAKE3 over a buffer — stands in for
//!   JSON ser/de, parsing, auth math that real handlers run on the worker).
//!   Compares a runtime with many workers (what tokio's default spawns on a
//!   CFS-quota-limited box, because `available_parallelism()` ignores the quota)
//!   against one sized to the real core budget. **Run pinned to the quota's
//!   cores** to reproduce the pathology, e.g. `taskset -c 0,1` on a 2-core quota:
//!     taskset -c 0,1 cargo run --release --features bench --example bench_tokio_runtime
//!   Under taskset the "before" worker count is forced high (BENCH_WORKERS_BEFORE)
//!   to model "tokio counted all host cores"; "after" uses the visible core count.
//!
//! Part B — blocking-pool RSS blast radius
//!   Floods the blocking pool with memory-heavy tasks (each ~`BENCH_ALLOC_MB`,
//!   the order of an Argon2 hash or an image decode buffer) and samples peak RSS.
//!   Compares tokio's default `max_blocking_threads = 512` against a bounded
//!   pool. Shows the worst-case resident-memory difference the cap prevents.
//!
//! No Postgres needed. Tunables (env):
//!   BENCH_CONCURRENCY (96)  BENCH_SECONDS (4)  BENCH_BURN_KB (256)
//!   BENCH_WORKERS_BEFORE (32)  BENCH_WORKERS_AFTER (visible cores)
//!   BENCH_BLOCKING_TASKS (96)  BENCH_ALLOC_MB (16)  BENCH_HOLD_MS (120)
//!   BENCH_MAX_BLOCKING_AFTER (16)

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use oxicloud::common::config::RuntimeConfig;
use oxicloud::common::runtime::{cgroup_cpu_quota, effective_parallelism, pool_sizes};

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(target_os = "linux")]
fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}
#[cfg(not(target_os = "linux"))]
fn rss_mb() -> u64 {
    0
}

fn build_rt(workers: usize, max_blocking: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(max_blocking)
        .enable_all()
        .build()
        .expect("build runtime")
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Part A: returns (requests, req/s, p50_us, p99_us, max_us).
fn bench_workers(
    workers: usize,
    max_blocking: usize,
    concurrency: usize,
    secs: u64,
    burn_bytes: usize,
) -> (u64, f64, u64, u64, u64) {
    let rt = build_rt(workers, max_blocking);
    let (total, mut lats) = rt.block_on(async move {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut handles = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            handles.push(tokio::spawn(async move {
                let buf = vec![0xa5u8; burn_bytes];
                let mut count = 0u64;
                let mut lats: Vec<u64> = Vec::with_capacity(4096);
                while Instant::now() < deadline {
                    let t = Instant::now();
                    // async hop — models awaiting I/O between CPU steps.
                    tokio::task::yield_now().await;
                    // inline CPU work on the worker — models handler compute.
                    let h = blake3::hash(&buf);
                    std::hint::black_box(h.as_bytes()[0]);
                    lats.push(t.elapsed().as_micros() as u64);
                    count += 1;
                }
                (count, lats)
            }));
        }
        let mut total = 0u64;
        let mut all: Vec<u64> = Vec::new();
        for h in handles {
            let (c, l) = h.await.expect("join");
            total += c;
            all.extend_from_slice(&l);
        }
        (total, all)
    });
    lats.sort_unstable();
    let reqs_per_s = total as f64 / secs as f64;
    (
        total,
        reqs_per_s,
        percentile(&lats, 50.0),
        percentile(&lats, 99.0),
        lats.last().copied().unwrap_or(0),
    )
}

/// Part B: returns (peak_rss_mb, baseline_rss_mb).
fn bench_blocking_rss(
    max_blocking: usize,
    n_tasks: usize,
    alloc_mb: usize,
    hold_ms: u64,
) -> (u64, u64) {
    // Two workers is plenty for the async sampler; the variable under test is
    // the blocking pool, not the worker pool.
    let rt = build_rt(2, max_blocking);
    rt.block_on(async move {
        let baseline = rss_mb();
        let peak = Arc::new(AtomicU64::new(baseline));
        let stop = Arc::new(AtomicBool::new(false));

        let p = peak.clone();
        let s = stop.clone();
        let sampler = tokio::spawn(async move {
            while !s.load(Ordering::Relaxed) {
                p.fetch_max(rss_mb(), Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
        });

        let mut handles = Vec::with_capacity(n_tasks);
        for _ in 0..n_tasks {
            handles.push(tokio::task::spawn_blocking(move || {
                let mut v = vec![0u8; alloc_mb * 1024 * 1024];
                // Touch every page so the allocation is actually resident.
                let mut i = 0;
                while i < v.len() {
                    v[i] = 1;
                    i += 4096;
                }
                std::thread::sleep(Duration::from_millis(hold_ms));
                std::hint::black_box(v.len());
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        peak.fetch_max(rss_mb(), Ordering::Relaxed);
        stop.store(true, Ordering::Relaxed);
        let _ = sampler.await;
        (peak.load(Ordering::Relaxed), baseline)
    })
}

fn main() {
    let concurrency: usize = env_or("BENCH_CONCURRENCY", 96);
    let secs: u64 = env_or("BENCH_SECONDS", 4);
    let burn_kb: usize = env_or("BENCH_BURN_KB", 256);
    let visible = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let workers_before: usize = env_or("BENCH_WORKERS_BEFORE", 32);
    let workers_after: usize = env_or("BENCH_WORKERS_AFTER", visible);

    let blocking_tasks: usize = env_or("BENCH_BLOCKING_TASKS", 96);
    let alloc_mb: usize = env_or("BENCH_ALLOC_MB", 16);
    let hold_ms: u64 = env_or("BENCH_HOLD_MS", 120);
    let max_blocking_after: usize = env_or("BENCH_MAX_BLOCKING_AFTER", 16);

    let (def_workers, def_max_blocking) = pool_sizes(&RuntimeConfig::from_env());

    println!("\n############################################################");
    println!("# Tokio runtime tuning benchmark");
    println!(
        "# available_parallelism = {}   cgroup_cpu_quota = {:?}   effective = {}",
        visible,
        cgroup_cpu_quota(),
        effective_parallelism()
    );
    println!(
        "# build_runtime would pick: worker_threads={} max_blocking_threads={}",
        def_workers, def_max_blocking
    );
    println!("############################################################");

    // ── Part A ──────────────────────────────────────────────────────────────
    println!("\n[A] Worker over-subscription under CPU contention");
    println!("    workload: {concurrency} concurrent requests, {burn_kb} KiB BLAKE3 each, {secs}s");
    println!("    (run under `taskset -c 0,1` to model a 2-core quota)\n");
    println!(
        "| {:<26} | {:>8} | {:>10} | {:>8} | {:>8} |",
        "runtime", "req/s", "requests", "p50 us", "p99 us"
    );
    println!(
        "|{:-<28}|{:-<10}|{:-<12}|{:-<10}|{:-<10}|",
        "", "", "", "", ""
    );

    let burn_bytes = burn_kb * 1024;
    let before = bench_workers(workers_before, 512, concurrency, secs, burn_bytes);
    println!(
        "| {:<26} | {:>8.0} | {:>10} | {:>8} | {:>8} |",
        format!("before: {workers_before} workers"),
        before.1,
        before.0,
        before.2,
        before.3
    );
    let after = bench_workers(
        workers_after,
        def_max_blocking,
        concurrency,
        secs,
        burn_bytes,
    );
    println!(
        "| {:<26} | {:>8.0} | {:>10} | {:>8} | {:>8} |",
        format!("after: {workers_after} workers"),
        after.1,
        after.0,
        after.2,
        after.3
    );
    let thr_delta = (after.1 / before.1 - 1.0) * 100.0;
    let p99_delta = (after.3 as f64 / before.3.max(1) as f64 - 1.0) * 100.0;
    println!(
        "\n    → throughput {:+.1}% , p99 latency {:+.1}%  (after vs before)",
        thr_delta, p99_delta
    );

    // ── Part B ──────────────────────────────────────────────────────────────
    println!("\n[B] Blocking-pool RSS blast radius");
    println!(
        "    workload: {blocking_tasks} concurrent spawn_blocking tasks, {alloc_mb} MiB each, hold {hold_ms}ms\n"
    );
    println!(
        "| {:<26} | {:>12} | {:>12} |",
        "max_blocking_threads", "peak RSS MiB", "vs default"
    );
    println!("|{:-<28}|{:-<14}|{:-<14}|", "", "", "");

    let (peak_def, base_def) = bench_blocking_rss(512, blocking_tasks, alloc_mb, hold_ms);
    println!(
        "| {:<26} | {:>12} | {:>12} |",
        "before: 512 (tokio default)", peak_def, "—"
    );
    let (peak_cap, _base_cap) =
        bench_blocking_rss(max_blocking_after, blocking_tasks, alloc_mb, hold_ms);
    let saved = peak_def as i64 - peak_cap as i64;
    println!(
        "| {:<26} | {:>12} | {:>12} |",
        format!("after: {max_blocking_after} (bounded)"),
        peak_cap,
        format!("-{} MiB", saved.max(0))
    );
    println!("    (baseline RSS before the flood: ~{base_def} MiB)\n");
}
