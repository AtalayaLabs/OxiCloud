//! Blob download read-ahead benchmark — `read_prefetch()` / `buffered(N)`.
//!
//! Isolates the ONE variable the local-backend change touches: the chunk
//! read-ahead depth fed to `buffered(N)` when reassembling a CDC file on the
//! download path (`BlobHandler::stream_chunks`). It rebuilds the *exact*
//! production combinator —
//!
//!   `stream::iter(hashes).map(get_blob_stream).buffered(N).try_flatten()`
//!
//! — over a REAL `LocalBlobBackend` whose chunk files are scattered across the
//! 256 hash-prefix directories exactly like production, then drains it and
//! reports throughput. The `N = 1` row is the current production behaviour
//! ("antes"); the higher-N rows are the candidate change ("después").
//!
//! The outcome is workload-dependent (the trait doc for `read_prefetch` argues
//! local should stay at 1), so the bench sweeps the two axes that decide it:
//!   • Consumer speed — `unthrottled` (disk-bound: a localhost / LAN client that
//!     drains as fast as the disk delivers) vs `throttled@<MB/s>` (network-bound:
//!     a real remote client where the socket, not the disk, is the bottleneck —
//!     this is where overlapping the next chunk's open+read with the current
//!     chunk's socket drain is supposed to pay off).
//!   • Page-cache state — `warm` (re-read, no disk I/O) vs `cold`
//!     (`posix_fadvise(DONTNEED)` evicts each chunk file first, Linux only —
//!     where concurrent opens on scattered files can instead cause seek
//!     contention). `cold` is best-effort: on tmpfs/overlayfs the eviction is a
//!     no-op and `cold` ≈ `warm` (noted in the output).
//!
//! Run (no Postgres needed):
//!   cargo run --release --features bench --example bench_blob_prefetch
//! Tunables (env):
//!   BENCH_FILE_MB (256)         total blob size
//!   BENCH_CHUNK_KB (256)        per-chunk size (matches CDC_AVG_CHUNK)
//!   BENCH_PREFETCH ("1,2,4,8,16")
//!   BENCH_THROTTLE_MBPS ("0,300,100")  0 = unthrottled; each value = a throttled run
//!   BENCH_REPS (5)              repetitions per cell; median reported
//!   BENCH_COLD (1)              also run cold-cache rows (Linux x86-64 only)

use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};

use oxicloud::application::ports::blob_storage_ports::BlobStorageBackend;
use oxicloud::infrastructure::services::local_blob_backend::LocalBlobBackend;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_list_usize(key: &str, default: &[usize]) -> Vec<usize> {
    env::var(key)
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|x| x.trim().parse().ok())
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| default.to_vec())
}

// ── cold-cache eviction (Linux x86-64 only, best-effort) ─────────────────────
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
unsafe extern "C" {
    fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
}
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const POSIX_FADV_DONTNEED: i32 = 4;

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn evict(paths: &[PathBuf]) {
    use std::os::unix::io::AsRawFd;
    for p in paths {
        if let Ok(f) = std::fs::File::open(p) {
            // len = 0 → "from offset to end of file" (the whole blob).
            unsafe {
                posix_fadvise(f.as_raw_fd(), 0, 0, POSIX_FADV_DONTNEED);
            }
        }
    }
}
#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn evict(_paths: &[PathBuf]) {}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const COLD_SUPPORTED: bool = true;
#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
const COLD_SUPPORTED: bool = false;

/// Fill `buf` with distinct, well-distributed bytes (xorshift64 seeded per
/// chunk) so every chunk hashes to a different BLAKE3 → scattered across the
/// 256 prefix dirs, matching production's content-addressed layout.
fn fill_chunk(buf: &mut [u8], seed: u64) {
    let mut s = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut i = 0;
    while i + 8 <= buf.len() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        buf[i..i + 8].copy_from_slice(&s.to_le_bytes());
        i += 8;
    }
    while i < buf.len() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        buf[i] = s as u8;
        i += 1;
    }
}

/// Drain the production reassembly pipeline once; return bytes read.
/// `throttle_bps == 0` means unthrottled (drain as fast as possible).
async fn run_once(
    backend: Arc<dyn BlobStorageBackend>,
    hashes: Vec<String>,
    prefetch: usize,
    throttle_bps: f64,
) -> u64 {
    let backend_for_map = backend.clone();
    let mut byte_stream = stream::iter(hashes)
        .map(move |hash| {
            let b = backend_for_map.clone();
            async move { b.get_blob_stream(&hash).await }
        })
        .buffered(prefetch.max(1))
        .map(|r| r.map_err(std::io::Error::other))
        .try_flatten();

    let mut total: u64 = 0;
    // Coarse token-bucket: only sleep once the accumulated owed time clears a
    // 2 ms floor, so the throttle models a rate-limited socket without drowning
    // the measurement in sub-ms timer noise.
    let per_byte_secs = if throttle_bps > 0.0 {
        1.0 / throttle_bps
    } else {
        0.0
    };
    let mut owed = Duration::ZERO;

    while let Some(item) = byte_stream.next().await {
        let chunk = item.expect("blob stream item");
        total += chunk.len() as u64;
        if per_byte_secs > 0.0 {
            owed += Duration::from_secs_f64(chunk.len() as f64 * per_byte_secs);
            if owed >= Duration::from_millis(2) {
                tokio::time::sleep(owed).await;
                owed = Duration::ZERO;
            }
        }
    }
    total
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let file_mb: usize = env_or("BENCH_FILE_MB", 256);
    let chunk_kb: usize = env_or("BENCH_CHUNK_KB", 256);
    let prefetches = env_list_usize("BENCH_PREFETCH", &[1, 2, 4, 8, 16]);
    let throttles_mbps = env_list_usize("BENCH_THROTTLE_MBPS", &[0, 300, 100]);
    let reps: usize = env_or("BENCH_REPS", 5);
    let want_cold: bool = env_or::<u8>("BENCH_COLD", 1) != 0;

    let chunk_bytes = chunk_kb * 1024;
    let total_bytes = file_mb * 1024 * 1024;
    let n_chunks = total_bytes.div_ceil(chunk_bytes);

    let tmp = tempfile::tempdir().expect("tempdir");
    let backend_local = LocalBlobBackend::new(tmp.path());
    backend_local.initialize().await.expect("init backend");

    // ── Build the blob: write n_chunks distinct content-addressed chunk files.
    let mut hashes: Vec<String> = Vec::with_capacity(n_chunks);
    let mut paths: Vec<PathBuf> = Vec::with_capacity(n_chunks);
    let mut buf = vec![0u8; chunk_bytes];
    let build_start = Instant::now();
    for i in 0..n_chunks {
        fill_chunk(&mut buf, i as u64);
        let data = Bytes::copy_from_slice(&buf);
        let hash = blake3::hash(&data).to_hex().to_string();
        backend_local
            .put_blob_from_bytes(&hash, data)
            .await
            .expect("put blob");
        paths.push(backend_local.blob_path(&hash));
        hashes.push(hash);
    }
    let backend: Arc<dyn BlobStorageBackend> = Arc::new(backend_local);
    let actual_bytes: u64 = (n_chunks * chunk_bytes) as u64;

    println!("\n############################################################");
    println!("# Blob download read-ahead (read_prefetch / buffered(N))");
    println!(
        "# blob: {} MiB in {} chunks of {} KiB (built in {:.1}s)",
        file_mb,
        n_chunks,
        chunk_kb,
        build_start.elapsed().as_secs_f64()
    );
    println!(
        "# production LocalBlobBackend.read_prefetch() = {}",
        backend.read_prefetch()
    );
    println!(
        "# reps/cell: {reps} (median MB/s reported)  cold-cache: {}",
        {
            if !COLD_SUPPORTED {
                "unsupported (non-Linux) → warm only"
            } else if want_cold {
                "yes (posix_fadvise DONTNEED, best-effort)"
            } else {
                "disabled (BENCH_COLD=0)"
            }
        }
    );
    println!("# N=1 is current production ('antes'); higher N is the candidate ('después')");
    println!("############################################################\n");
    println!(
        "| {:<22} | {:>8} | {:>9} | {:>8} | {:>9} |",
        "scenario", "prefetch", "med MB/s", "min ms", "vs N=1"
    );
    println!(
        "|{:-<24}|{:-<10}|{:-<11}|{:-<10}|{:-<11}|",
        "", "", "", "", ""
    );

    let mb = actual_bytes as f64 / (1024.0 * 1024.0);

    // cache states to test
    let mut cache_states: Vec<&str> = vec!["warm"];
    if want_cold && COLD_SUPPORTED {
        cache_states.push("cold");
    }

    for &thr_mbps in &throttles_mbps {
        let throttle_bps = thr_mbps as f64 * 1024.0 * 1024.0;
        let thr_label = if thr_mbps == 0 {
            "unthrottled".to_string()
        } else {
            format!("throttled@{}MB/s", thr_mbps)
        };

        for cache in &cache_states {
            let scenario = format!("{}/{}", cache, thr_label);
            let mut baseline_mbps: Option<f64> = None;

            for &pf in &prefetches {
                let mut samples_mbps: Vec<f64> = Vec::with_capacity(reps);
                let mut min_ms = f64::MAX;

                // one warmup (also primes warm-cache state)
                let _ = run_once(backend.clone(), hashes.clone(), pf, throttle_bps).await;

                for _ in 0..reps {
                    if *cache == "cold" {
                        evict(&paths);
                    }
                    let t = Instant::now();
                    let got = run_once(backend.clone(), hashes.clone(), pf, throttle_bps).await;
                    let secs = t.elapsed().as_secs_f64();
                    assert_eq!(got, actual_bytes, "short read");
                    samples_mbps.push(mb / secs);
                    min_ms = min_ms.min(secs * 1000.0);
                }

                samples_mbps.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let med = samples_mbps[samples_mbps.len() / 2];
                let delta = match baseline_mbps {
                    None => {
                        baseline_mbps = Some(med);
                        "—".to_string()
                    }
                    Some(base) => format!("{:+.1}%", (med / base - 1.0) * 100.0),
                };

                println!(
                    "| {:<22} | {:>8} | {:>9.1} | {:>8.1} | {:>9} |",
                    scenario, pf, med, min_ms, delta
                );
            }
            println!(
                "|{:-<24}|{:-<10}|{:-<11}|{:-<10}|{:-<11}|",
                "", "", "", "", ""
            );
        }
    }

    println!(
        "\nInterpretation: a '+x%' under 'vs N=1' is the read-ahead gain over current\n\
         production for that scenario; a negative value is a regression. Network-bound\n\
         rows (throttled) are the realistic remote-download case; unthrottled rows are\n\
         disk-bound (localhost/LAN). Pick the smallest N that wins the throttled rows\n\
         without regressing the disk-bound/cold rows.\n"
    );
}
