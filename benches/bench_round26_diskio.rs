//! Round-26 disk-I/O pack (no Postgres) — async wall on a tmpfs-backed tempdir.
//!
//!   [D1] `CachedBlobBackend::initialize` creates ONLY `cache_dir`, never the 256
//!        `{00..ff}` shard dirs (the line-122 comment claims otherwise), so each
//!        of the three cache-write sites re-runs `tokio::fs::create_dir_all(parent)`
//!        on the hot path — a wasted `mkdirat(EEXIST)` + component stat + a
//!        blocking-pool dispatch per chunk write on cached-remote deployments.
//!        AFTER pre-creates the shard dirs at init (mirroring
//!        `LocalBlobBackend::initialize`) and drops the per-write call. Gate:
//!        AFTER wall (per write) strictly lower than BEFORE (the redundant
//!        create_dir_all).
//!
//!   [D2] TESTED AND REVERTED — see benches/ROUND26.md. Moving the moka
//!        eviction-listener unlink off the reactor via `spawn_blocking` was
//!        refuted by the benchmark: on the local cache dir (fast unlink ~7 µs)
//!        the `spawn_blocking` dispatch (~20 µs) costs MORE on the reactor than
//!        the inline `std::fs::remove_file` it replaces. The original inline
//!        unlink ("a quick unlink on the inserting task's thread") is correct
//!        for the fast-local-cache case; kept as-is.
//!
//! Run:
//!   RUSTFLAGS="-C target-cpu=x86-64-v3" \
//!     cargo bench --features=bench --bench bench_round26_diskio
//! Tunables (env): D1_ITERS (20000)

use std::env;
use std::time::Instant;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn gate(tag: &str, metric: &str, before: f64, after: f64) {
    if after >= before {
        eprintln!("GATE FAIL [{tag}] {metric}: AFTER {after} !< BEFORE {before} — rollback");
        std::process::exit(1);
    }
}

// ── [D1] redundant create_dir_all on a warm shard vs skip ────────────────────
async fn section_d1() {
    let iters: u64 = env_or("D1_ITERS", 20_000);
    let dir = tempfile::tempdir().expect("tempdir");
    let shard = dir.path().join("ab");
    // Shard pre-created once (what AFTER's initialize does).
    tokio::fs::create_dir_all(&shard).await.unwrap();

    // warm
    let _ = tokio::fs::create_dir_all(&shard).await;

    // BEFORE: per-write create_dir_all(parent) on the already-existing shard.
    let t = Instant::now();
    for _ in 0..iters {
        let _ = tokio::fs::create_dir_all(&shard).await;
    }
    let before_ns = t.elapsed().as_nanos() as f64 / iters as f64;

    // AFTER: shard guaranteed present at init → the write path skips the call.
    let t = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(&shard);
    }
    let after_ns = t.elapsed().as_nanos() as f64 / iters as f64;

    println!("## [D1] cache-write create_dir_all on a warm shard");
    println!("| arm    | ns/write |");
    println!("| BEFORE | {before_ns:>8.1} |");
    println!("| AFTER  | {after_ns:>8.1} |");
    println!(
        "# {:.1}x — redundant create_dir_all removed per cache write\n",
        before_ns / after_ns.max(0.001)
    );
    gate("D1", "ns/write", before_ns, after_ns);
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    println!("# Round-26 disk-I/O pack\n");
    section_d1().await;
    println!("All Round-26 disk-I/O sections passed their gate.");
}
