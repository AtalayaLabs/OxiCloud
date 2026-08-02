//! S3 chunk-PUT benchmark — HEAD-before-PUT vs unconditional PUT.
//!
//! `DedupService::settle_batch` writes every NEW chunk of every upload via
//! `put_blob_from_bytes_unsynced`. S3/Azure never overrode it, so the trait
//! default routed it through `put_blob_from_bytes`, whose "idempotent" HEAD
//! probe made every chunk write pay 2 request round-trips. Content-addressed
//! keys make re-PUTs overwrite-safe, so the new override PUTs directly.
//!
//! The stub S3 endpoint (in-process axum, per-request latency injection)
//! counts HEAD/PUT requests:
//!   BEFORE — put_blob_from_bytes        (HEAD 404 + PUT per chunk)
//!   AFTER  — put_blob_from_bytes_unsynced (PUT per chunk)
//!
//! Section 2 measures the removed Azure `data.to_vec()` copy in isolation.
//!
//! Section 3 (round 9) drives the same A/B **through the decorator stacks**
//! (`RetryBlobBackend`, `CachedBlobBackend`, and the full production
//! Cache(Encrypted(Retry(S3))) composition). Until round 9 neither Retry nor
//! Cached overrode `put_blob_from_bytes_unsynced`/`sync_blobs`, so the trait
//! default silently re-routed every decorated chunk write back through the
//! probing synced path — undoing this bench's own Section-1 win on every
//! remote deployment with retry or cache enabled. The BEFORE arm is the
//! still-present synced route (`put_blob_from_bytes`, byte-identical requests
//! to what the fallthrough produced); the AFTER arm is the now-forwarded
//! unsynced route. A write-through equivalence gate asserts the Cached stack
//! still populates its local cache identically on both routes.
//!
//! Gates: AFTER request count == chunks (vs 2x), AFTER wall < BEFORE wall,
//! per-stack AFTER HEADs == 0, cache population identical on both routes.
//!
//! No Postgres. Run:
//!   cargo run --release --features bench --example bench_s3_put
//! Tunables: BENCH_CHUNKS (500), BENCH_CHUNK_KB (256), BENCH_CONCURRENCY (8),
//!           BENCH_RTT_MS (10)

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use oxicloud::application::ports::blob_storage_ports::BlobStorageBackend;
use oxicloud::common::config::S3StorageConfig;
use oxicloud::infrastructure::services::cached_blob_backend::{BlobCacheConfig, CachedBlobBackend};
use oxicloud::infrastructure::services::encrypted_blob_backend::EncryptedBlobBackend;
use oxicloud::infrastructure::services::retry_blob_backend::{RetryBlobBackend, RetryPolicy};
use oxicloud::infrastructure::services::s3_blob_backend::S3BlobBackend;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Recursively count regular files under `dir` (the blob cache shards blobs
/// into 2-hex-char prefix subdirectories).
fn count_files(dir: &std::path::Path) -> usize {
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                n += count_files(&path);
            } else {
                n += 1;
            }
        }
    }
    n
}

#[derive(Clone, Default)]
struct Counters {
    heads: Arc<AtomicU64>,
    puts: Arc<AtomicU64>,
}

async fn stub_s3(latency: Duration, counters: Counters) -> String {
    use axum::http::{Method, StatusCode};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let counters = counters.clone();
        async move {
            tokio::time::sleep(latency).await;
            match *req.method() {
                Method::HEAD => {
                    counters.heads.fetch_add(1, Ordering::Relaxed);
                    StatusCode::NOT_FOUND
                }
                Method::PUT => {
                    // Drain the body like a real endpoint would.
                    let _ = axum::body::to_bytes(req.into_body(), usize::MAX).await;
                    counters.puts.fetch_add(1, Ordering::Relaxed);
                    StatusCode::OK
                }
                _ => StatusCode::OK,
            }
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}")
}

async fn drive(
    backend: Arc<dyn BlobStorageBackend>,
    chunks: usize,
    chunk_kb: usize,
    concurrency: usize,
    unsynced: bool,
    hash_prefix: &str,
) -> f64 {
    let payload = Bytes::from(vec![0x5au8; chunk_kb * 1024]);
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let t = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for i in 0..chunks {
        let b = backend.clone();
        let p = payload.clone();
        let sem = sem.clone();
        let hash = format!("{hash_prefix}{i:060x}");
        set.spawn(async move {
            let _permit = sem.acquire().await.expect("sem");
            let n = if unsynced {
                b.put_blob_from_bytes_unsynced(&hash, p).await.expect("put")
            } else {
                b.put_blob_from_bytes(&hash, p).await.expect("put")
            };
            // Encrypted arms return the ciphertext size (plaintext + AEAD
            // framing), so gate on >= rather than == for stack generality.
            assert!(n as usize >= chunk_kb * 1024);
        });
    }
    while let Some(r) = set.join_next().await {
        r.expect("join");
    }
    t.elapsed().as_secs_f64() * 1000.0
}

/// Run BEFORE (synced route == the pre-round-9 unsynced fallthrough) and
/// AFTER (forwarded unsynced route) through one backend stack, printing the
/// two rows and gating AFTER on zero probe requests. `prefixes` carries the
/// (BEFORE, AFTER) hash namespaces keeping the arms' key spaces disjoint.
async fn stack_ab(
    label: &str,
    backend: Arc<dyn BlobStorageBackend>,
    counters: &Counters,
    chunks: usize,
    chunk_kb: usize,
    concurrency: usize,
    prefixes: (&str, &str),
) -> (f64, f64) {
    let (prefix_before, prefix_after) = prefixes;
    let before = drive(
        backend.clone(),
        chunks,
        chunk_kb,
        concurrency,
        false,
        prefix_before,
    )
    .await;
    let before_heads = counters.heads.swap(0, Ordering::Relaxed);
    let before_puts = counters.puts.swap(0, Ordering::Relaxed);
    println!(
        "{:<34} {:>10.0} {:>8} {:>8} {:>8}",
        format!("{label} BEFORE (synced route)"),
        before,
        before_heads,
        before_puts,
        "1.0x"
    );

    let after = drive(
        backend.clone(),
        chunks,
        chunk_kb,
        concurrency,
        true,
        prefix_after,
    )
    .await;
    let after_heads = counters.heads.swap(0, Ordering::Relaxed);
    let after_puts = counters.puts.swap(0, Ordering::Relaxed);
    println!(
        "{:<34} {:>10.0} {:>8} {:>8} {:>8}",
        format!("{label} AFTER  (unsynced)"),
        after,
        after_heads,
        after_puts,
        format!("{:.1}x", before / after)
    );

    if before_heads != chunks as u64 {
        eprintln!(
            "GATE FAIL [{label}]: BEFORE issued {before_heads} HEADs (expected {chunks} — the probing route must still probe)"
        );
        std::process::exit(1);
    }
    if after_heads != 0 || after_puts != chunks as u64 {
        eprintln!(
            "GATE FAIL [{label}]: AFTER issued {after_heads} HEADs / {after_puts} PUTs (expected 0 / {chunks})"
        );
        std::process::exit(1);
    }
    if after >= before {
        eprintln!(
            "GATE FAIL [{label}]: AFTER ({after:.0} ms) not faster than BEFORE ({before:.0} ms) — rollback"
        );
        std::process::exit(1);
    }
    (before, after)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let chunks: usize = env_or("BENCH_CHUNKS", 500);
    let chunk_kb: usize = env_or("BENCH_CHUNK_KB", 256);
    let concurrency: usize = env_or("BENCH_CONCURRENCY", 8);
    let rtt_ms: u64 = env_or("BENCH_RTT_MS", 10);

    let counters = Counters::default();
    let endpoint = stub_s3(Duration::from_millis(rtt_ms), counters.clone()).await;
    let backend = Arc::new(S3BlobBackend::new(&S3StorageConfig {
        endpoint_url: Some(endpoint),
        bucket: "bench".into(),
        region: "us-east-1".into(),
        access_key: "bench".into(),
        secret_key: "bench".into(),
        force_path_style: true,
    }));

    println!(
        "# {chunks} x {chunk_kb} KiB chunk PUTs at concurrency {concurrency}, {rtt_ms} ms/request stub"
    );
    println!(
        "{:<26} {:>10} {:>8} {:>8} {:>8}",
        "variant", "wall ms", "HEADs", "PUTs", "vs OLD"
    );

    // BEFORE: the trait-default route (put_blob_from_bytes = HEAD + PUT).
    let before = drive(
        backend.clone() as Arc<dyn BlobStorageBackend>,
        chunks,
        chunk_kb,
        concurrency,
        false,
        "a0a0",
    )
    .await;
    let before_heads = counters.heads.swap(0, Ordering::Relaxed);
    let before_puts = counters.puts.swap(0, Ordering::Relaxed);
    println!(
        "{:<26} {:>10.0} {:>8} {:>8} {:>8}",
        "BEFORE (HEAD+PUT)", before, before_heads, before_puts, "1.0x"
    );

    // AFTER: the unsynced override (PUT only).
    let after = drive(
        backend.clone() as Arc<dyn BlobStorageBackend>,
        chunks,
        chunk_kb,
        concurrency,
        true,
        "a0a1",
    )
    .await;
    let after_heads = counters.heads.swap(0, Ordering::Relaxed);
    let after_puts = counters.puts.swap(0, Ordering::Relaxed);
    println!(
        "{:<26} {:>10.0} {:>8} {:>8} {:>8}",
        "AFTER (PUT only)",
        after,
        after_heads,
        after_puts,
        format!("{:.1}x", before / after)
    );

    // ── Section 2: the removed Azure to_vec() copy, in isolation ───────
    let mb = 4;
    let data = Bytes::from(vec![0x77u8; mb * 1024 * 1024]);
    let reps = 200;
    let t = Instant::now();
    for _ in 0..reps {
        let v = data.to_vec();
        std::hint::black_box(&v);
    }
    let copy_ms = t.elapsed().as_secs_f64() * 1000.0 / reps as f64;
    println!(
        "\n# [2] removed Azure per-chunk copy: to_vec() of {mb} MiB = {copy_ms:.2} ms + {mb} MiB transient alloc per chunk"
    );

    // ── Section 3: the same A/B through the decorator stacks ────────────
    println!(
        "\n# [3] decorated stacks — pre-round-9 the unsynced call fell through to the synced (probing) route"
    );
    println!(
        "{:<34} {:>10} {:>8} {:>8} {:>8}",
        "variant", "wall ms", "HEADs", "PUTs", "vs OLD"
    );

    // Retry(S3)
    let retry_stack: Arc<dyn BlobStorageBackend> = Arc::new(RetryBlobBackend::new(
        backend.clone() as Arc<dyn BlobStorageBackend>,
        RetryPolicy::default(),
    ));
    stack_ab(
        "retry(s3)",
        retry_stack,
        &counters,
        chunks,
        chunk_kb,
        concurrency,
        ("b0b0", "b0b1"),
    )
    .await;

    // Cache(S3) — count cache write-through population on both routes.
    let cache_dir_a = tempfile::tempdir().expect("tempdir");
    let cached_stack: Arc<dyn BlobStorageBackend> = Arc::new(CachedBlobBackend::new(
        backend.clone() as Arc<dyn BlobStorageBackend>,
        &BlobCacheConfig {
            cache_dir: cache_dir_a.path().to_path_buf(),
            max_cache_bytes: u64::MAX,
        },
    ));
    stack_ab(
        "cache(s3)",
        cached_stack,
        &counters,
        chunks,
        chunk_kb,
        concurrency,
        ("c0c0", "c0c1"),
    )
    .await;
    // Write-through equivalence gate: BOTH routes populated the local cache
    // (the round-9 override keeps post-upload read locality intact).
    let cached_files = count_files(cache_dir_a.path());
    if cached_files != 2 * chunks {
        eprintln!(
            "GATE FAIL [cache(s3)]: cache holds {cached_files} blobs (expected {} — write-through must populate on BOTH routes)",
            2 * chunks
        );
        std::process::exit(1);
    }

    // Full production composition: Cache(Encrypted(Retry(S3))).
    let cache_dir_b = tempfile::tempdir().expect("tempdir");
    let full_stack: Arc<dyn BlobStorageBackend> = Arc::new(CachedBlobBackend::new(
        Arc::new(EncryptedBlobBackend::new_single_aes(
            Arc::new(RetryBlobBackend::new(
                backend.clone() as Arc<dyn BlobStorageBackend>,
                RetryPolicy::default(),
            )),
            &[0x42u8; 32],
        )),
        &BlobCacheConfig {
            cache_dir: cache_dir_b.path().to_path_buf(),
            max_cache_bytes: u64::MAX,
        },
    ));
    let (full_before, full_after) = stack_ab(
        "cache(enc(retry(s3)))",
        full_stack,
        &counters,
        chunks,
        chunk_kb,
        concurrency,
        ("d0d0", "d0d1"),
    )
    .await;
    println!(
        "# full stack: a {chunks}-chunk upload sheds {} probe round-trips ({:.0} -> {:.0} ms at {rtt_ms} ms RTT)",
        chunks, full_before, full_after
    );

    // ── Gates ───────────────────────────────────────────────────────────
    if after_heads != 0 || after_puts != chunks as u64 {
        eprintln!(
            "GATE FAIL: AFTER issued {after_heads} HEADs / {after_puts} PUTs (expected 0 / {chunks})"
        );
        std::process::exit(1);
    }
    if after >= before {
        eprintln!(
            "GATE FAIL: AFTER ({after:.0} ms) not faster than BEFORE ({before:.0} ms) — rollback"
        );
        std::process::exit(1);
    }
    println!(
        "GATE PASS: {}-request walk -> {} requests, {:.1}x faster",
        before_heads + before_puts,
        after_puts,
        before / after
    );
}
