//! Round-2 benchmark battery — five before/after gates in one binary.
//!
//! Each section isolates exactly what its change touches; a section whose
//! AFTER does not beat its BEFORE is grounds for rolling that change back.
//!
//!   [1] range-cache    — per-seek: PG resolve + open/seek/read  vs  moka hit + Bytes::slice
//!   [2] nc-chunk-gate  — per-PUT session-bytes gate: dir scan+stat  vs  counter
//!   [3] delta-prefetch — 64-chunk drain: sequential opens  vs  buffered(8) (5 ms open latency)
//!   [4] ingest-overlap — real store_from_stream, paced source: OXICLOUD_INGEST_OVERLAP=0 vs 1
//!   [5] zip-stream     — time-to-first-byte: temp-file build  vs  duplex streaming
//!
//! Run (needs Postgres for [1] and [4]; reads DATABASE_URL from .env):
//!   cargo run --release --features bench --example bench_round2
//! Select sections: BENCH_SECTIONS="1,2,3,4,5"

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

fn fill_random(buf: &mut [u8], seed: &mut u64) {
    for chunk in buf.chunks_mut(8) {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        let b = seed.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(&b[..n]);
    }
}

// ── [1] range-cache ─────────────────────────────────────────────────────────
async fn section_range_cache(url: &str) {
    println!("\n== [1] range-cache: per-seek cost, 256 KiB ranges over a 6 MiB media file ==");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(url)
        .await
        .expect("pg");

    // Seed: drive→folder→file row (the BEFORE path resolves blob_hash by id)
    // plus the blob bytes on disk for the open/seek/read.
    let mut tx = pool.begin().await.expect("tx");
    let drive_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, quota_bytes) VALUES ('shared', NULL) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let folder_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('bench_range', '/bench_range', 'bench_range', $1) RETURNING id",
    )
    .bind(drive_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
        .bind(folder_id)
        .bind(drive_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let blob_hash = "benchrange000000000000000000000000000000000000000000000000000000";
    let file_id: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
         VALUES ('video.mp4', $1, $2, 6291456, 'video/mp4', $3) RETURNING id",
    )
    .bind(folder_id)
    .bind(blob_hash)
    .bind(drive_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut data = vec![0u8; 6 * 1024 * 1024];
    let mut seed = 7u64;
    fill_random(&mut data, &mut seed);
    let blob_path = dir.path().join("blob");
    std::fs::write(&blob_path, &data).unwrap();

    // AFTER: warm content cache keyed by hash.
    let cache: moka::sync::Cache<String, Bytes> = moka::sync::Cache::new(1000);
    cache.insert(blob_hash.to_string(), Bytes::from(data.clone()));

    let secs = 3u64;
    let range_len = 256 * 1024usize;
    for mode in ["BEFORE", "AFTER"] {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut lats = Vec::new();
        let mut off = 0usize;
        while Instant::now() < deadline {
            let t = Instant::now();
            if mode == "BEFORE" {
                // 1. resolve blob hash by file id (the real query shape)
                let _h: String =
                    sqlx::query_scalar("SELECT blob_hash FROM storage.files WHERE id = $1")
                        .bind(file_id)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                // 2. open + seek + read the range (manifest lookup is already
                //    a moka hit post-round-1, so it's omitted on both sides)
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let mut f = tokio::fs::File::open(&blob_path).await.unwrap();
                f.seek(std::io::SeekFrom::Start(off as u64)).await.unwrap();
                let mut buf = vec![0u8; range_len];
                f.read_exact(&mut buf).await.unwrap();
                std::hint::black_box(&buf);
            } else {
                let bytes = cache.get(blob_hash).unwrap();
                let slice = bytes.slice(off..off + range_len);
                std::hint::black_box(&slice);
            }
            lats.push(t.elapsed().as_secs_f64() * 1e6);
            off = (off + range_len) % (data.len() - range_len);
        }
        lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "  {:<7} {:>9.0} seeks/s   p50 {:>8.2} µs   p99 {:>8.2} µs",
            mode,
            lats.len() as f64 / secs as f64,
            pct(&lats, 0.5),
            pct(&lats, 0.99),
        );
    }

    let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(drive_id)
        .execute(&pool)
        .await;
}

// ── [2] nc-chunk-gate ───────────────────────────────────────────────────────
async fn section_nc_chunk_gate() {
    println!("\n== [2] nc-chunk-gate: cumulative gate cost across a 1000-chunk upload ==");
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("alice").join("upload-1");
    tokio::fs::create_dir_all(&session).await.unwrap();

    let chunks: usize = env_or("BENCH_CHUNKS", 1000);
    // BEFORE: every PUT lists the dir and stats every existing chunk.
    let t0 = Instant::now();
    for k in 0..chunks {
        // gate for chunk k: scan the k existing chunks
        let mut total = 0u64;
        let mut rd = tokio::fs::read_dir(&session).await.unwrap();
        while let Some(e) = rd.next_entry().await.unwrap() {
            total += e.metadata().await.unwrap().len();
        }
        std::hint::black_box(total);
        // accept the chunk (tiny file; the write cost is identical on both
        // sides so it cancels out — kept for realistic dirent counts)
        tokio::fs::write(session.join(format!("{k:05}")), b"x")
            .await
            .unwrap();
    }
    let before = t0.elapsed().as_secs_f64() * 1000.0;

    // Reset dir.
    tokio::fs::remove_dir_all(&session).await.unwrap();
    tokio::fs::create_dir_all(&session).await.unwrap();

    // AFTER: O(1) counter (moka read + insert per PUT).
    let counter: moka::sync::Cache<String, u64> = moka::sync::Cache::new(10);
    counter.insert("s".into(), 0);
    let t0 = Instant::now();
    for k in 0..chunks {
        let total = counter.get("s").unwrap();
        std::hint::black_box(total);
        tokio::fs::write(session.join(format!("{k:05}")), b"x")
            .await
            .unwrap();
        counter.insert("s".into(), total + 1);
    }
    let after = t0.elapsed().as_secs_f64() * 1000.0;

    println!(
        "  BEFORE dir-scan gate: {before:>9.1} ms total   AFTER counter gate: {after:>9.1} ms total   ({:.1}x)",
        before / after
    );
    println!("  (gate work alone; chunk-write cost included identically on both sides)");
}

// ── [3] delta-prefetch ──────────────────────────────────────────────────────
async fn section_delta_prefetch() {
    println!(
        "\n== [3] delta-prefetch: 64-chunk drain, 5 ms per-open latency (object-store model) =="
    );
    let n_chunks = 64usize;
    let chunk_kb = 256usize;
    let mut seed = 11u64;
    let mut payload = vec![0u8; chunk_kb * 1024];
    fill_random(&mut payload, &mut seed);
    let payload = Bytes::from(payload);

    // One "chunk open" = latency + a 4-frame byte stream (the shape the
    // handler drains). Sequential = old; buffered(8) = new combinator.
    let open = |p: Bytes| async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        Ok::<_, std::io::Error>(stream::iter(
            p.chunks(64 * 1024)
                .map(|c| Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        ))
    };

    for (label, prefetch) in [("BEFORE sequential", 1usize), ("AFTER  buffered(8)", 8)] {
        let t0 = Instant::now();
        let mut drained = 0u64;
        let mut s = stream::iter(vec![payload.clone(); n_chunks])
            .map(&open)
            .buffered(prefetch)
            .try_flatten();
        while let Some(part) = s.next().await {
            drained += part.unwrap().len() as u64;
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!("  {label}: {ms:>8.1} ms for {} MiB", drained / 1024 / 1024);
    }
    println!("  (local-disk gain for the same combinator: +7-12% — benches/BLOB-PREFETCH.md)");
}

// ── [4] ingest-overlap ──────────────────────────────────────────────────────
async fn section_ingest_overlap(url: &str) {
    println!("\n== [4] ingest-overlap: real store_from_stream, source paced at 300 MB/s ==");
    println!(
        "  (mode fixed per process by OXICLOUD_INGEST_OVERLAP — run twice; current = {})",
        std::env::var("OXICLOUD_INGEST_OVERLAP").unwrap_or_else(|_| "1/default".into())
    );
    use oxicloud::infrastructure::services::blob_handler::BlobHandler;
    use oxicloud::infrastructure::services::local_blob_backend::LocalBlobBackend;

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(10)
            .connect(url)
            .await
            .expect("pg"),
    );
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(LocalBlobBackend::new(dir.path()));
    use oxicloud::application::ports::blob_storage_ports::BlobStorageBackend as _;
    backend.initialize().await.expect("init backend");
    let svc = BlobHandler::new(backend, pool.clone(), pool.clone());

    let total_mb: usize = env_or("BENCH_INGEST_MB", 512);
    let pace_mbps: f64 = env_or("BENCH_PACE_MBPS", 300.0);
    let frame = 256 * 1024usize;
    let mut seed = std::process::id() as u64 | 0xABCD << 32; // unique content per run — no dedup hits
    let frames: Vec<Bytes> = (0..total_mb * 1024 * 1024 / frame)
        .map(|_| {
            let mut b = vec![0u8; frame];
            fill_random(&mut b, &mut seed);
            Bytes::from(b)
        })
        .collect();
    let frame_interval = Duration::from_secs_f64(frame as f64 / (pace_mbps * 1e6));

    let t0 = Instant::now();
    let source = stream::iter(frames.into_iter().map(Ok::<Bytes, std::io::Error>)).then(
        move |f| async move {
            tokio::time::sleep(frame_interval).await;
            f
        },
    );
    let result = svc.store_from_stream(source, None).await.expect("ingest");
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "  ingested {} MiB in {:.2} s  →  {:.0} MB/s (blob {})",
        total_mb,
        secs,
        total_mb as f64 / secs,
        &result.hash()[..12],
    );
    // Cleanup: release the reference so GC can reap the bench blobs.
    let _ = svc.remove_reference(result.hash()).await;
}

// ── [5] zip-stream ──────────────────────────────────────────────────────────
async fn section_zip_stream() {
    println!("\n== [5] zip-stream: time-to-first-byte, 48 x 4 MiB media corpus ==");
    use async_zip::base::write::ZipFileWriter;
    use async_zip::{Compression, ZipEntryBuilder};
    use futures::io::AsyncWriteExt as _;

    let files: usize = env_or("BENCH_ZIP_FILES", 48);
    let mb: usize = env_or("BENCH_ZIP_MB", 4);
    let mut seed = 13u64;
    let corpus: Vec<Bytes> = (0..files)
        .map(|_| {
            let mut b = vec![0u8; mb * 1024 * 1024];
            fill_random(&mut b, &mut seed);
            Bytes::from(b)
        })
        .collect();

    async fn write_all_entries<W: tokio::io::AsyncWrite + Unpin>(sink: W, corpus: &[Bytes]) {
        let buf = tokio::io::BufWriter::with_capacity(256 * 1024, sink);
        let mut zip = ZipFileWriter::with_tokio(buf);
        for (i, data) in corpus.iter().enumerate() {
            let entry = ZipEntryBuilder::new(format!("IMG_{i:04}.jpg").into(), Compression::Stored);
            let mut w = zip.write_entry_stream(entry).await.unwrap();
            for c in data.chunks(64 * 1024) {
                w.write_all(c).await.unwrap();
            }
            w.close().await.unwrap();
        }
        let mut compat = zip.close().await.unwrap();
        compat.close().await.unwrap();
    }

    // BEFORE: build the whole archive into a temp file, then "respond".
    let t0 = Instant::now();
    let temp = tempfile::NamedTempFile::new().unwrap();
    let f = tokio::fs::File::create(temp.path()).await.unwrap();
    write_all_entries(f, &corpus).await;
    // first byte = read back the first chunk
    use tokio::io::AsyncReadExt;
    let mut rf = tokio::fs::File::open(temp.path()).await.unwrap();
    let mut first = vec![0u8; 64 * 1024];
    rf.read_exact(&mut first).await.unwrap();
    let ttfb_before = t0.elapsed().as_secs_f64() * 1000.0;
    let mut rest = Vec::new();
    rf.read_to_end(&mut rest).await.unwrap();
    let total_before = t0.elapsed().as_secs_f64() * 1000.0;

    // AFTER: duplex — first byte as soon as the first entry flushes.
    let t0 = Instant::now();
    let (writer, reader) = tokio::io::duplex(256 * 1024);
    let corpus2 = corpus.clone();
    let jh = tokio::spawn(async move { write_all_entries(writer, &corpus2).await });
    let mut rs = tokio_util::io::ReaderStream::new(reader);
    let firstb = rs.next().await.unwrap().unwrap();
    std::hint::black_box(&firstb);
    let ttfb_after = t0.elapsed().as_secs_f64() * 1000.0;
    let mut drained = firstb.len();
    while let Some(c) = rs.next().await {
        drained += c.unwrap().len();
    }
    jh.await.unwrap();
    let total_after = t0.elapsed().as_secs_f64() * 1000.0;

    println!("  BEFORE temp-file : TTFB {ttfb_before:>8.1} ms   total {total_before:>8.1} ms");
    println!(
        "  AFTER  streaming : TTFB {ttfb_after:>8.1} ms   total {total_after:>8.1} ms   (TTFB {:.0}x, {} MiB drained)",
        ttfb_before / ttfb_after.max(0.001),
        drained / 1024 / 1024
    );
    println!("  (TTFB scales with archive size in BEFORE; constant in AFTER)");
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").unwrap_or_default();
    let sections: Vec<u32> = env::var("BENCH_SECTIONS")
        .unwrap_or_else(|_| "1,2,3,4,5".into())
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();

    let _ = median(vec![0.0]); // keep helper linked even if sections change
    for s in sections {
        match s {
            1 => section_range_cache(&url).await,
            2 => section_nc_chunk_gate().await,
            3 => section_delta_prefetch().await,
            4 => section_ingest_overlap(&url).await,
            5 => section_zip_stream().await,
            _ => {}
        }
    }
}
