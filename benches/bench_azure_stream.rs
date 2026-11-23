//! Azure download-path benchmark — whole-blob buffering vs streaming (ROUND4).
//!
//! The old `AzureBlobBackend::get_blob_stream` / `get_blob_range_stream`
//! drained the ENTIRE blob (or range) into one `Vec<u8>` before yielding
//! a single mega-chunk: whole-blob RAM residency per reader, TTFB = full
//! download time, and with `read_prefetch() = 8` the CDC reassembly path
//! could hold 8 entire chunk-blobs at once. AFTER forwards the SDK's
//! page/body streams directly (first page still awaited eagerly so a
//! missing blob is an up-front NotFound).
//!
//! Technique: a local axum stub speaks just enough of the Azure Blob GET
//! REST surface (ranged 16 MiB pages, `x-ms-*` headers) for the REAL
//! `azure_storage_blobs` client — the backend points at it via the new
//! `endpoint_url` override (also the Azurite hook). The stub synthesizes
//! blob bytes deterministically per offset, so it holds no buffer and
//! the peak-live-heap metric isolates the CLIENT path. BEFORE is the old
//! collect-everything logic copied verbatim; AFTER is the real
//! `AzureBlobBackend`. BLAKE3 gates assert byte-identical payloads.
//!
//! Run (no Postgres needed):
//!   cargo bench --features=bench --bench bench_azure_stream
//! Tunables (env): BENCH_MB (256) blob size, BENCH_TAIL_MB (128) range tail.

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use bytes::Bytes;
use futures::StreamExt;
use oxicloud::application::ports::blob_storage_ports::BlobStorageBackend;
use oxicloud::common::config::AzureStorageConfig;
use oxicloud::infrastructure::services::azure_blob_backend::AzureBlobBackend;
use tokio::net::TcpListener;

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

// ─── Deterministic blob content (no stored buffer) ──────────────────────────

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Fill `out` with the blob bytes at absolute offset `offset`.
fn fill_at(out: &mut [u8], offset: u64) {
    let mut i = 0usize;
    while i < out.len() {
        let abs = offset + i as u64;
        let block = abs / 8;
        let word = splitmix64(block).to_le_bytes();
        let start_in_word = (abs % 8) as usize;
        let take = (8 - start_in_word).min(out.len() - i);
        out[i..i + take].copy_from_slice(&word[start_in_word..start_in_word + take]);
        i += take;
    }
}

/// BLAKE3 of an arbitrary blob range, streamed in 1 MiB pieces.
fn expected_hash(offset: u64, len: u64) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < len {
        let take = ((len - pos) as usize).min(buf.len());
        fill_at(&mut buf[..take], offset + pos);
        hasher.update(&buf[..take]);
        pos += take as u64;
    }
    hasher.finalize()
}

// ─── Azure Blob GET stub ────────────────────────────────────────────────────

fn parse_range(headers: &HeaderMap) -> Option<(u64, Option<u64>)> {
    let raw = headers
        .get("x-ms-range")
        .or_else(|| headers.get("range"))?
        .to_str()
        .ok()?;
    let spec = raw.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: u64 = a.parse().ok()?;
    let end: Option<u64> = if b.is_empty() { None } else { b.parse().ok() };
    Some((start, end))
}

/// Serve GET {container}/{blob} with ranged responses in streamed 256 KiB
/// frames, synthesizing content per offset — the stub never holds the blob.
async fn stub_azure(blob_len: u64) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");

    let app = axum::Router::new().fallback(move |req: Request<Body>| async move {
        if req.method() != axum::http::Method::GET {
            return Response::builder()
                .status(StatusCode::CREATED)
                .header("etag", "\"0x1\"")
                .header("last-modified", "Thu, 01 Jan 2026 00:00:00 GMT")
                .header("x-ms-request-id", "11111111-1111-1111-1111-111111111111")
                .header("date", "Thu, 01 Jan 2026 00:00:00 GMT")
                .body(Body::empty())
                .unwrap();
        }
        let (start, end_incl) = parse_range(req.headers()).unwrap_or((0, None));
        let end_incl = end_incl.unwrap_or(blob_len - 1).min(blob_len - 1);
        let this_len = end_incl - start + 1;

        // Stream the payload in 256 KiB frames, generated on the fly.
        let body_stream = futures::stream::unfold(0u64, move |sent| async move {
            if sent >= this_len {
                return None;
            }
            let take = ((this_len - sent) as usize).min(256 * 1024);
            let mut frame = vec![0u8; take];
            fill_at(&mut frame, start + sent);
            Some((
                Ok::<Bytes, std::io::Error>(Bytes::from(frame)),
                sent + take as u64,
            ))
        });

        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("content-type", "application/octet-stream")
            .header("content-length", this_len.to_string())
            .header(
                "content-range",
                format!("bytes {start}-{end_incl}/{blob_len}"),
            )
            .header("etag", "\"0x1\"")
            .header("last-modified", "Thu, 01 Jan 2026 00:00:00 GMT")
            .header("x-ms-blob-type", "BlockBlob")
            .header("x-ms-lease-status", "unlocked")
            .header("x-ms-lease-state", "available")
            .header("x-ms-request-id", "11111111-1111-1111-1111-111111111111")
            .header("x-ms-version", "2020-04-08")
            .header("x-ms-creation-time", "Thu, 01 Jan 2026 00:00:00 GMT")
            .header("x-ms-server-encrypted", "true")
            .header("date", "Thu, 01 Jan 2026 00:00:00 GMT")
            .body(Body::from_stream(body_stream))
            .unwrap()
    });

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("stub serve");
    });
    format!("http://{addr}/devaccount")
}

// ─── BEFORE: verbatim old collect-everything implementations ────────────────

mod before {
    use super::*;
    use azure_storage_blobs::prelude::BlobClient;
    use oxicloud::application::ports::blob_storage_ports::BlobStream;

    /// Old `get_blob_stream` body (drain everything, yield one chunk).
    pub async fn get_blob_stream(client: &BlobClient) -> Result<BlobStream, String> {
        let mut result_data: Vec<u8> = Vec::new();
        let mut stream = client.get().into_stream();

        while let Some(response) = stream.next().await {
            let response = response.map_err(|e| format!("Failed to get blob: {e}"))?;
            let mut body = response.data;
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|e| format!("Stream read error: {e}"))?;
                result_data.extend_from_slice(&chunk);
            }
        }

        let stream: BlobStream = Box::pin(futures::stream::once(async move {
            Ok(Bytes::from(result_data))
        }));
        Ok(stream)
    }

    /// Old `get_blob_range_stream` body.
    pub async fn get_blob_range_stream(
        client: &BlobClient,
        start: u64,
        end: Option<u64>,
    ) -> Result<BlobStream, String> {
        let range = match end {
            Some(e) => azure_core::request_options::Range::new(start, e),
            None => azure_core::request_options::Range::new(start, u64::MAX),
        };

        let mut result_data: Vec<u8> = Vec::new();
        let mut stream = client.get().range(range).into_stream();

        while let Some(response) = stream.next().await {
            let response = response.map_err(|e| format!("Failed to get blob range: {e}"))?;
            let mut body = response.data;
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|e| format!("Stream range read error: {e}"))?;
                result_data.extend_from_slice(&chunk);
            }
        }

        let stream: BlobStream = Box::pin(futures::stream::once(async move {
            Ok(Bytes::from(result_data))
        }));
        Ok(stream)
    }
}

// ─── Drain helper: TTFB + wall + hash ───────────────────────────────────────

async fn drain(
    stream: oxicloud::application::ports::blob_storage_ports::BlobStream,
    t0: Instant,
) -> (f64, f64, blake3::Hash, u64) {
    let mut stream = stream;
    let mut hasher = blake3::Hasher::new();
    let mut ttfb = None;
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("stream chunk");
        if ttfb.is_none() {
            ttfb = Some(t0.elapsed().as_secs_f64() * 1e3);
        }
        total += chunk.len() as u64;
        hasher.update(&chunk);
    }
    (
        ttfb.unwrap_or(f64::NAN),
        t0.elapsed().as_secs_f64() * 1e3,
        hasher.finalize(),
        total,
    )
}

fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn peak_mib() -> f64 {
    PEAK.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mb: u64 = env::var("BENCH_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let tail_mb: u64 = env::var("BENCH_TAIL_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128);
    let blob_len = mb * 1024 * 1024;
    let hash = "aabbccdd00112233445566778899eeff00112233445566778899aabbccddeeff";

    let endpoint = stub_azure(blob_len).await;
    println!("bench_azure_stream — {mb} MiB blob via local stub at {endpoint}\n");

    // AFTER: the real backend pointed at the stub via endpoint_url.
    let backend = AzureBlobBackend::new(&AzureStorageConfig {
        account_name: "devaccount".to_string(),
        account_key: base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"benchkeybenchkeybenchkey",
        ),
        container: "blobs".to_string(),
        sas_token: None,
        endpoint_url: Some(endpoint.clone()),
    });

    // BEFORE: a raw SDK client at the same endpoint for the verbatim old code.
    let creds = azure_storage::StorageCredentials::access_key(
        "devaccount",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"benchkeybenchkeybenchkey",
        ),
    );
    let old_client = azure_storage_blobs::prelude::ClientBuilder::with_location(
        azure_storage::CloudLocation::Custom {
            account: "devaccount".to_string(),
            uri: endpoint.clone(),
        },
        creds,
    )
    .container_client("blobs")
    .blob_client(format!("{}/{}.blob", &hash[0..2], hash));

    let expect_full = expected_hash(0, blob_len);
    let tail_start = blob_len - tail_mb * 1024 * 1024;
    let expect_tail = expected_hash(tail_start, blob_len - tail_start);

    // ── [1] Full-blob download ──────────────────────────────────────────────
    reset_peak();
    let t0 = Instant::now();
    let s = before::get_blob_stream(&old_client)
        .await
        .expect("before stream");
    let (ttfb_b, wall_b, hash_b, len_b) = drain(s, t0).await;
    let peak_b = peak_mib();

    reset_peak();
    let t0 = Instant::now();
    let s = backend.get_blob_stream(hash).await.expect("after stream");
    let (ttfb_a, wall_a, hash_a, len_a) = drain(s, t0).await;
    let peak_a = peak_mib();

    println!("[1] full {mb} MiB download        TTFB ms    wall ms   peak live heap MiB");
    println!("    BEFORE (collect-then-yield) {ttfb_b:9.1}  {wall_b:9.1}   {peak_b:10.1}");
    println!(
        "    AFTER  (streamed)           {ttfb_a:9.1}  {wall_a:9.1}   {peak_a:10.1}   TTFB {:.0}x, heap {:.0}x lower",
        ttfb_b / ttfb_a,
        peak_b / peak_a
    );

    // ── [2] Open-ended range (seek to last {tail_mb} MiB) ───────────────────
    reset_peak();
    let t0 = Instant::now();
    let s = before::get_blob_range_stream(&old_client, tail_start, None)
        .await
        .expect("before range");
    let (rttfb_b, rwall_b, rhash_b, rlen_b) = drain(s, t0).await;
    let rpeak_b = peak_mib();

    reset_peak();
    let t0 = Instant::now();
    let s = backend
        .get_blob_range_stream(hash, tail_start, None)
        .await
        .expect("after range");
    let (rttfb_a, rwall_a, rhash_a, rlen_a) = drain(s, t0).await;
    let rpeak_a = peak_mib();

    println!("[2] range bytes={tail_start}- ({tail_mb} MiB tail)");
    println!("    BEFORE (collect-then-yield) {rttfb_b:9.1}  {rwall_b:9.1}   {rpeak_b:10.1}");
    println!(
        "    AFTER  (streamed)           {rttfb_a:9.1}  {rwall_a:9.1}   {rpeak_a:10.1}   TTFB {:.0}x, heap {:.0}x lower",
        rttfb_b / rttfb_a,
        rpeak_b / rpeak_a
    );

    // ── Equivalence gates ───────────────────────────────────────────────────
    let mut ok = true;
    if hash_b != expect_full || hash_a != expect_full || len_b != blob_len || len_a != blob_len {
        eprintln!("GATE FAIL full blob: hashes/length differ");
        ok = false;
    }
    if rhash_b != expect_tail || rhash_a != expect_tail || rlen_b != rlen_a {
        eprintln!("GATE FAIL range: hashes/length differ");
        ok = false;
    }
    println!(
        "\n[gate] BLAKE3(BEFORE) == BLAKE3(AFTER) == source: {}",
        if ok { "OK" } else { "FAILED" }
    );
    if !ok {
        std::process::exit(1);
    }
}
