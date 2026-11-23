//! CachedBlobBackend miss-stampede benchmark — duplicate remote fetches.
//!
//! K concurrent cold readers of ONE blob (a video player's parallel Range
//! probes on an uncached file, N sync clients pulling the same new file)
//! used to each download the FULL blob from the remote backend and race
//! their writes on one shared deterministic `.tmp` path. The per-hash
//! single-flight gate coalesces them onto one download; waiters serve the
//! leader's cached file.
//!
//! The mock inner backend counts `get_blob_stream` calls and serves a
//! 32 MiB blob with an injected 15 ms first-byte latency + paced chunks
//! (models a remote object store).
//!
//!   BEFORE (emulated) — K concurrent direct inner fetches, each draining
//!                       the full stream (what the old miss path did)
//!   AFTER             — K concurrent `CachedBlobBackend::get_blob_stream`
//!                       on a cold cache
//!
//! Gates: AFTER's inner-fetch count == 1; the cached file must BLAKE3-match
//! the source; K x full-drain wall reported for both.
//!
//! No Postgres. Run:
//!   cargo bench --features=bench --bench bench_blob_cache
//! Tunables: BENCH_CONCURRENCY (16), BENCH_BLOB_MB (32)

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt;
use oxicloud::application::ports::blob_storage_ports::{
    BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use oxicloud::domain::errors::DomainError;
use oxicloud::infrastructure::services::cached_blob_backend::{BlobCacheConfig, CachedBlobBackend};

type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Mock remote backend: one in-RAM blob, counted reads, and — crucially —
/// SHARED aggregate bandwidth: concurrent streams split one simulated
/// 1 GiB/s link (a real NIC/egress link doesn't hand every duplicate
/// download its own private lane, so duplicate fetches cost real wall
/// time, not just bytes).
struct MockRemote {
    data: Bytes,
    fetches: AtomicU64,
    bytes_served: AtomicU64,
    /// Virtual time (µs since bench start) when the shared link frees up.
    link_busy_until_us: Arc<tokio::sync::Mutex<u64>>,
    epoch: Instant,
}

const LINK_BYTES_PER_SEC: u64 = 1024 * 1024 * 1024; // 1 GiB/s aggregate

impl MockRemote {
    fn new(data: Bytes) -> Self {
        Self {
            data,
            fetches: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
            link_busy_until_us: Arc::new(tokio::sync::Mutex::new(0)),
            epoch: Instant::now(),
        }
    }

    fn stream(&self) -> BlobStream {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        self.bytes_served
            .fetch_add(self.data.len() as u64, Ordering::Relaxed);
        let data = self.data.clone();
        let link = self.link_busy_until_us.clone();
        let epoch = self.epoch;
        let s = async_stream::stream! {
            // First-byte latency of a remote GET.
            tokio::time::sleep(Duration::from_millis(15)).await;
            let chunk = 4 * 1024 * 1024;
            let mut off = 0usize;
            while off < data.len() {
                let end = (off + chunk).min(data.len());
                // Reserve this chunk's slot on the shared link, then sleep
                // until the slot has elapsed — bandwidth divides across
                // every in-flight stream.
                let slot_us = (end - off) as u64 * 1_000_000 / LINK_BYTES_PER_SEC;
                let wake_us = {
                    let mut busy = link.lock().await;
                    let now_us = epoch.elapsed().as_micros() as u64;
                    let start = (*busy).max(now_us);
                    *busy = start + slot_us;
                    *busy
                };
                let now_us = epoch.elapsed().as_micros() as u64;
                if wake_us > now_us {
                    tokio::time::sleep(Duration::from_micros(wake_us - now_us)).await;
                }
                yield Ok::<Bytes, std::io::Error>(data.slice(off..end));
                off = end;
            }
        };
        Box::pin(s)
    }
}

impl BlobStorageBackend for MockRemote {
    fn initialize(&self) -> BoxFut<'_, Result<(), DomainError>> {
        Box::pin(async { Ok(()) })
    }
    fn put_blob(&self, _hash: &str, _source_path: &Path) -> BoxFut<'_, Result<u64, DomainError>> {
        Box::pin(async { Ok(0) })
    }
    fn put_blob_from_bytes(
        &self,
        _hash: &str,
        data: Bytes,
    ) -> BoxFut<'_, Result<u64, DomainError>> {
        Box::pin(async move { Ok(data.len() as u64) })
    }
    fn get_blob_stream(&self, _hash: &str) -> BoxFut<'_, Result<BlobStream, DomainError>> {
        let s = self.stream();
        Box::pin(async move { Ok(s) })
    }
    fn get_blob_range_stream(
        &self,
        _hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> BoxFut<'_, Result<BlobStream, DomainError>> {
        let data = self.data.clone();
        self.fetches.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let end = end.unwrap_or(data.len() as u64).min(data.len() as u64);
            let s = futures::stream::once(async move {
                Ok::<Bytes, std::io::Error>(data.slice(start as usize..end as usize))
            });
            Ok(Box::pin(s) as BlobStream)
        })
    }
    fn delete_blob(&self, _hash: &str) -> BoxFut<'_, Result<(), DomainError>> {
        Box::pin(async { Ok(()) })
    }
    fn blob_exists(&self, _hash: &str) -> BoxFut<'_, Result<bool, DomainError>> {
        Box::pin(async { Ok(true) })
    }
    fn blob_size(&self, _hash: &str) -> BoxFut<'_, Result<u64, DomainError>> {
        let n = self.data.len() as u64;
        Box::pin(async move { Ok(n) })
    }
    fn health_check(&self) -> BoxFut<'_, Result<StorageHealthStatus, DomainError>> {
        Box::pin(async {
            Ok(StorageHealthStatus {
                connected: true,
                backend_type: "mock".into(),
                message: "ok".into(),
                available_bytes: None,
            })
        })
    }
    fn backend_type(&self) -> &'static str {
        "mock"
    }
    fn local_blob_path(&self, _hash: &str) -> Option<PathBuf> {
        None
    }
}

async fn drain(mut s: BlobStream) -> (u64, [u8; 32]) {
    let mut hasher = blake3::Hasher::new();
    let mut n = 0u64;
    while let Some(chunk) = s.next().await {
        let b = chunk.expect("chunk");
        n += b.len() as u64;
        hasher.update(&b);
    }
    (n, hasher.finalize().into())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let k: usize = env_or("BENCH_CONCURRENCY", 16);
    let blob_mb: usize = env_or("BENCH_BLOB_MB", 32);

    let data: Bytes = (0..blob_mb * 1024 * 1024)
        .map(|i| (i * 37 % 249) as u8)
        .collect::<Vec<u8>>()
        .into();
    let ref_hash: [u8; 32] = blake3::hash(&data).into();
    let blob_len = data.len() as u64;
    let hash = "benchblobcache00000000000000000000000000000000000000000000000000";

    // ── BEFORE (emulated): K concurrent direct inner fetches ───────────
    let remote = Arc::new(MockRemote::new(data.clone()));
    let t = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..k {
        let r = remote.clone();
        set.spawn(async move {
            let s = r.get_blob_stream(hash).await.expect("stream");
            drain(s).await
        });
    }
    while let Some(res) = set.join_next().await {
        let (n, h) = res.expect("join");
        assert_eq!(n, blob_len);
        assert_eq!(h, ref_hash);
    }
    let before_wall = t.elapsed().as_secs_f64() * 1000.0;
    let before_fetches = remote.fetches.load(Ordering::Relaxed);
    let before_mb = remote.bytes_served.load(Ordering::Relaxed) / (1024 * 1024);

    // ── AFTER: K concurrent CachedBlobBackend reads, cold cache ────────
    let remote = Arc::new(MockRemote::new(data.clone()));
    let dir = tempfile::tempdir().expect("tempdir");
    let cached = Arc::new(CachedBlobBackend::new(
        remote.clone(),
        &BlobCacheConfig {
            cache_dir: dir.path().to_path_buf(),
            max_cache_bytes: 1 << 30,
        },
    ));
    cached.initialize().await.expect("init");

    let t = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..k {
        let c = cached.clone();
        set.spawn(async move {
            let s = c.get_blob_stream(hash).await.expect("stream");
            drain(s).await
        });
    }
    while let Some(res) = set.join_next().await {
        let (n, h) = res.expect("join");
        assert_eq!(n, blob_len);
        assert_eq!(h, ref_hash, "cached read corrupted");
    }
    let after_wall = t.elapsed().as_secs_f64() * 1000.0;
    let after_fetches = remote.fetches.load(Ordering::Relaxed);
    let after_mb = remote.bytes_served.load(Ordering::Relaxed) / (1024 * 1024);

    // Integrity of the durable cache file itself.
    let (n, h) = drain(cached.get_blob_stream(hash).await.expect("warm")).await;
    assert_eq!(n, blob_len);
    assert_eq!(h, ref_hash, "durable cache file corrupted");
    let warm_fetches = remote.fetches.load(Ordering::Relaxed) - after_fetches;

    println!("# {k} concurrent cold readers of one {blob_mb} MiB blob (remote: 15 ms TTFB, paced)");
    println!(
        "{:<24} {:>10} {:>14} {:>12}",
        "variant", "wall ms", "inner fetches", "remote MiB"
    );
    println!(
        "{:<24} {:>10.0} {:>14} {:>12}",
        "BEFORE (per-caller)", before_wall, before_fetches, before_mb
    );
    println!(
        "{:<24} {:>10.0} {:>14} {:>12}",
        "AFTER (single-flight)", after_wall, after_fetches, after_mb
    );

    // ── Gates ───────────────────────────────────────────────────────────
    if after_fetches != 1 {
        eprintln!("GATE FAIL: expected exactly 1 coalesced remote fetch, got {after_fetches}");
        std::process::exit(1);
    }
    if warm_fetches != 0 {
        eprintln!("GATE FAIL: warm read hit the remote backend");
        std::process::exit(1);
    }
    println!("\nGATE PASS: {before_fetches} remote fetches -> 1, cache file verified");
}
