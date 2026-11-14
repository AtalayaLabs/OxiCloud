//! ZIP entry-compression benchmark — `Deflate`-always vs MIME-aware `Stored`.
//!
//! Isolates the ONE variable the ZIP-export change touches: the per-entry
//! `Compression` mode chosen by `ZipService::write_prefetched_file` /
//! `BatchOperations::add_file_entry_streamed`. It rebuilds the *exact*
//! production writer stack —
//!
//!   `ZipFileWriter::with_tokio(BufWriter(File))` + `write_entry_stream`
//!   fed in ~64 KiB chunks (the blob-stream chunk size)
//!
//! — and writes the same corpus once per mode, measuring wall time, process
//! CPU time (utime+stime via `getrusage(RUSAGE_SELF)`), and final archive size.
//!
//! Corpora:
//!   • `media`  — incompressible bytes (models JPEG/HEIC/MP4/WebP, the
//!     dominant "download folder" payload). Deflate here is pure CPU burn.
//!   • `text`   — compressible text (models docs/source). Deflate genuinely
//!     shrinks these; the MIME-aware change keeps deflating them.
//!   • `mixed`  — 80 % media / 20 % text by bytes: `all-Deflate` row is the
//!     production behaviour BEFORE the change; `mime-aware` row (Stored for
//!     media, Deflate for text) is AFTER.
//!
//! Run (no Postgres needed):
//!   cargo bench --features=bench --bench bench_zip_media
//! Tunables (env):
//!   BENCH_MEDIA_FILES (48)   BENCH_MEDIA_MB (4)   per-file size
//!   BENCH_TEXT_FILES (24)    BENCH_TEXT_MB (2)
//!   BENCH_REPS (3)           median reported

use std::env;
use std::time::{Duration, Instant};

use async_zip::base::write::ZipFileWriter;
use async_zip::{Compression, ZipEntryBuilder};
use futures::io::AsyncWriteExt as FuturesWriteExt;
use tokio::io::BufWriter;

const CHUNK: usize = 64 * 1024; // blob-stream chunk size on the real path

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

use oxicloud::bench_support::process_metrics::cpu_seconds;

/// Deterministic xorshift64* stream — incompressible "media" bytes.
fn fill_random(buf: &mut [u8], seed: &mut u64) {
    for chunk in buf.chunks_mut(8) {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        let bytes = seed.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(&bytes[..n]);
    }
}

/// Compressible pseudo-text (~3-4× deflate ratio, like real docs/source).
fn fill_text(buf: &mut [u8], seed: &mut u64) {
    const WORDS: &[&str] = &[
        "the",
        "quick",
        "brown",
        "fox",
        "jumps",
        "over",
        "lazy",
        "dog",
        "folder",
        "file",
        "storage",
        "performance",
        "benchmark",
        "archive",
        "download",
        "stream",
    ];
    let mut pos = 0;
    while pos < buf.len() {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        let w = WORDS[(*seed as usize) % WORDS.len()].as_bytes();
        let n = w.len().min(buf.len() - pos);
        buf[pos..pos + n].copy_from_slice(&w[..n]);
        pos += n;
        if pos < buf.len() {
            buf[pos] = b' ';
            pos += 1;
        }
    }
}

struct CorpusFile {
    name: String,
    data: Vec<u8>,
    is_media: bool,
}

struct RunResult {
    wall: Duration,
    cpu: f64,
    bytes_out: u64,
}

/// Write the corpus through the exact production writer stack, choosing the
/// compression mode per entry with `pick`.
async fn write_zip(files: &[CorpusFile], pick: impl Fn(&CorpusFile) -> Compression) -> RunResult {
    let temp = tempfile::NamedTempFile::new().expect("temp file");
    let tokio_file = tokio::fs::File::create(temp.path()).await.expect("create");
    let buf_writer = BufWriter::with_capacity(256 * 1024, tokio_file);
    let mut zip = ZipFileWriter::with_tokio(buf_writer);

    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    for f in files {
        let entry = ZipEntryBuilder::new(f.name.clone().into(), pick(f));
        let mut w = zip.write_entry_stream(entry).await.expect("entry start");
        for chunk in f.data.chunks(CHUNK) {
            w.write_all(chunk).await.expect("chunk write");
        }
        w.close().await.expect("entry close");
    }
    let mut compat = zip.close().await.expect("zip close");
    compat.close().await.expect("flush");
    let wall = t0.elapsed();
    let cpu = cpu_seconds() - cpu0;

    let bytes_out = std::fs::metadata(temp.path()).map(|m| m.len()).unwrap_or(0);
    RunResult {
        wall,
        cpu,
        bytes_out,
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let media_files: usize = env_or("BENCH_MEDIA_FILES", 48);
    let media_mb: usize = env_or("BENCH_MEDIA_MB", 4);
    let text_files: usize = env_or("BENCH_TEXT_FILES", 24);
    let text_mb: usize = env_or("BENCH_TEXT_MB", 2);
    let reps: usize = env_or("BENCH_REPS", 3);

    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut corpus: Vec<CorpusFile> = Vec::new();
    for i in 0..media_files {
        let mut data = vec![0u8; media_mb * 1024 * 1024];
        fill_random(&mut data, &mut seed);
        corpus.push(CorpusFile {
            name: format!("photos/IMG_{i:04}.jpg"),
            data,
            is_media: true,
        });
    }
    for i in 0..text_files {
        let mut data = vec![0u8; text_mb * 1024 * 1024];
        fill_text(&mut data, &mut seed);
        corpus.push(CorpusFile {
            name: format!("docs/notes_{i:04}.txt"),
            data,
            is_media: false,
        });
    }
    let media_bytes: usize = corpus
        .iter()
        .filter(|f| f.is_media)
        .map(|f| f.data.len())
        .sum();
    let text_bytes: usize = corpus
        .iter()
        .filter(|f| !f.is_media)
        .map(|f| f.data.len())
        .sum();
    let total_mb = (media_bytes + text_bytes) as f64 / 1048576.0;
    println!(
        "corpus: {} media files ({} MiB, incompressible) + {} text files ({} MiB, compressible), {} reps\n",
        media_files,
        media_bytes / 1048576,
        text_files,
        text_bytes / 1048576,
        reps
    );

    // (label, per-entry compression picker)
    type Picker = Box<dyn Fn(&CorpusFile) -> Compression>;
    let modes: Vec<(&str, Picker)> = vec![
        (
            "all-Deflate (BEFORE)",
            Box::new(|_: &CorpusFile| Compression::Deflate),
        ),
        (
            "mime-aware (AFTER) ",
            Box::new(|f: &CorpusFile| {
                if f.is_media {
                    Compression::Stored
                } else {
                    Compression::Deflate
                }
            }),
        ),
        (
            "all-Stored (bound) ",
            Box::new(|_: &CorpusFile| Compression::Stored),
        ),
    ];

    println!(
        "{:<22} {:>9} {:>9} {:>10} {:>11} {:>9}",
        "mode", "wall s", "cpu s", "MB/s", "out MiB", "ratio"
    );
    let mut baseline_wall = None;
    for (label, pick) in &modes {
        let mut walls = Vec::new();
        let mut cpus = Vec::new();
        let mut out = 0u64;
        for _ in 0..reps {
            let r = write_zip(&corpus, pick).await;
            walls.push(r.wall.as_secs_f64());
            cpus.push(r.cpu);
            out = r.bytes_out;
        }
        let wall = median(walls);
        let cpu = median(cpus);
        let speedup = baseline_wall
            .map(|b: f64| format!("{:.2}x", b / wall))
            .unwrap_or_else(|| "1.00x".into());
        if baseline_wall.is_none() {
            baseline_wall = Some(wall);
        }
        println!(
            "{:<22} {:>9.3} {:>9.2} {:>10.1} {:>11.1} {:>9}",
            label,
            wall,
            cpu,
            total_mb / wall,
            out as f64 / 1048576.0,
            speedup
        );
    }
    println!("\n(archive `out MiB` for mime-aware stays ~= all-Deflate: media doesn't deflate,");
    println!(" text keeps Deflate — the win is CPU/wall, not size loss)");
}
