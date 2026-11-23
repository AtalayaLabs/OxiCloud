//! Upload spool/assembly I/O benchmark — buffer sizing on the chunk paths.
//!
//! Section 1 — assembly read (`stream_from_files`): every completed chunked
//! upload is read back once, part file by part file, through
//! `ReaderStream::with_capacity(file, N)`. Each poll is one blocking-pool
//! dispatch + one read(2) of N bytes; the shipped capacity was 64 KiB while
//! every other blob read path uses 256 KiB+. Sweeps N over
//! 64K/256K/512K/1M and reports wall time + read syscalls.
//!
//! Section 2 — chunk spool write (`stream_body_to_path`): the PUT handlers
//! wrote each HTTP frame (~16-64 KiB) straight to a bare tokio File — one
//! blocking-pool dispatch + write(2) per frame. Compares that against the
//! adopted `BufWriter::with_capacity(512 KiB)`.
//!
//! No Postgres. Run:
//!   cargo bench --features=bench --bench bench_upload_spool
//! Tunables: BENCH_PARTS (16), BENCH_PART_MB (10), BENCH_FRAME_KB (16),
//!           BENCH_SPOOL_MB (10), BENCH_REPS (5)
//!
//! Linux-only. The whole story this bench tells — "did the capacity
//! sweep cut read(2)/write(2) counts" — is built on `/proc/self/io`'s
//! `syscr`/`syscw` counters. Darwin's `getrusage.ru_inblock`/
//! `ru_oublock` measure block-device I/O (page-cache misses), not
//! syscall counts, so a cross-platform port would silently change
//! what the bench claims to measure. Skip on non-Linux instead of
//! lying.

// Everything below (helpers + the Linux `main`) is gated — the stub
// `main` for non-Linux lives further down. Keep the module header's
// doc-comment imports etc. unconditional so `cargo doc --no-deps`
// doesn't grow an "ignored on this target" wart.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::env;
use std::path::PathBuf;
use std::time::Instant;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// (read syscalls, write syscalls) from /proc/self/io.
fn io_counters() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/io").expect("io");
    let get = |k: &str| {
        s.lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    (get("syscr:"), get("syscw:"))
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

/// The `stream_from_files` shape with a parameterized capacity.
async fn drain_parts(paths: Vec<PathBuf>, cap: usize) -> (u64, [u8; 32]) {
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    let s = stream::iter(paths.into_iter().map(Ok::<_, std::io::Error>))
        .and_then(|path| async move {
            tokio::fs::File::open(path)
                .await
                .map(|file| ReaderStream::with_capacity(file, cap))
        })
        .try_flatten();
    let mut s = Box::pin(s);
    while let Some(chunk) = s.next().await {
        let chunk = chunk.expect("read");
        total += chunk.len() as u64;
        hasher.update(&chunk);
    }
    (total, hasher.finalize().into())
}

/// The `stream_body_to_path` inner loop: frames -> file, optionally buffered.
async fn spool_frames(frames: &[Bytes], path: &std::path::Path, buffered: bool) {
    let file = tokio::fs::File::create(path).await.expect("create");
    if buffered {
        let mut w = tokio::io::BufWriter::with_capacity(512 * 1024, file);
        for f in frames {
            w.write_all(f).await.expect("write");
        }
        w.flush().await.expect("flush");
    } else {
        let mut w = file;
        for f in frames {
            w.write_all(f).await.expect("write");
        }
        w.flush().await.expect("flush");
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "bench_upload_spool: skipped on this OS — /proc/self/io syscr/syscw \
         has no portable equivalent; see the module doc comment."
    );
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let parts: usize = env_or("BENCH_PARTS", 16);
    let part_mb: usize = env_or("BENCH_PART_MB", 10);
    let frame_kb: usize = env_or("BENCH_FRAME_KB", 16);
    let spool_mb: usize = env_or("BENCH_SPOOL_MB", 10);
    let reps: usize = env_or("BENCH_REPS", 5);

    let dir = tempfile::tempdir().expect("tempdir");

    // ── Section 1: assembly read capacity sweep ─────────────────────────
    println!("# [1] assembly read: {parts} x {part_mb} MiB part files, warm page cache");
    let mut paths = Vec::with_capacity(parts);
    let payload: Vec<u8> = (0..part_mb * 1024 * 1024)
        .map(|i| (i * 31 % 251) as u8)
        .collect();
    for i in 0..parts {
        let p = dir.path().join(format!("part_{i:05}"));
        tokio::fs::write(&p, &payload).await.expect("seed part");
        paths.push(p);
    }
    let expect_total = (parts * part_mb * 1024 * 1024) as u64;
    let (_, ref_hash) = drain_parts(paths.clone(), 256 * 1024).await;

    println!(
        "{:<10} {:>10} {:>12} {:>8}",
        "capacity", "wall ms", "read sysc", "vs 64K"
    );
    let mut base: Option<f64> = None;
    for cap in [64 * 1024, 256 * 1024, 512 * 1024, 1024 * 1024] {
        let mut walls = Vec::with_capacity(reps);
        let mut syscr = 0u64;
        for _ in 0..reps {
            let (r0, _) = io_counters();
            let t = Instant::now();
            let (total, h) = drain_parts(paths.clone(), cap).await;
            walls.push(t.elapsed().as_secs_f64() * 1000.0);
            let (r1, _) = io_counters();
            syscr = r1 - r0;
            assert_eq!(total, expect_total);
            assert_eq!(h, ref_hash, "content mismatch at capacity {cap}");
        }
        let ms = median(walls);
        let speedup = base
            .map(|b| format!("{:.2}x", b / ms))
            .unwrap_or_else(|| "1.00x".into());
        if base.is_none() {
            base = Some(ms);
        }
        println!(
            "{:<10} {:>10.1} {:>12} {:>8}",
            format!("{}K", cap / 1024),
            ms,
            syscr,
            speedup
        );
    }

    // ── Section 2: chunk spool write, per-frame vs buffered ─────────────
    let frames_n = spool_mb * 1024 / frame_kb;
    println!(
        "\n# [2] chunk spool: {frames_n} x {frame_kb} KiB frames ({spool_mb} MiB), 20 files/rep"
    );
    let frame: Bytes = Bytes::from(vec![0xabu8; frame_kb * 1024]);
    let frames: Vec<Bytes> = (0..frames_n).map(|_| frame.clone()).collect();

    println!(
        "{:<22} {:>10} {:>12} {:>8}",
        "variant", "wall ms", "write sysc", "vs bare"
    );
    let mut base: Option<f64> = None;
    for (label, buffered) in [
        ("bare File (BEFORE)", false),
        ("BufWriter 512K (AFTER)", true),
    ] {
        let mut walls = Vec::with_capacity(reps);
        let mut syscw = 0u64;
        for r in 0..reps {
            let (_, w0) = io_counters();
            let t = Instant::now();
            for i in 0..20 {
                let p = dir.path().join(format!("spool_{r}_{i}"));
                spool_frames(&frames, &p, buffered).await;
                tokio::fs::remove_file(&p).await.ok();
            }
            walls.push(t.elapsed().as_secs_f64() * 1000.0);
            let (_, w1) = io_counters();
            syscw = w1 - w0;
        }
        let ms = median(walls);
        let speedup = base
            .map(|b| format!("{:.2}x", b / ms))
            .unwrap_or_else(|| "1.00x".into());
        if base.is_none() {
            base = Some(ms);
        }
        println!("{label:<22} {:>10.1} {:>12} {:>8}", ms, syscw, speedup);
    }
}
