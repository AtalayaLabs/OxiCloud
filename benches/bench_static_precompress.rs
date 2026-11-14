//! Static-asset compression benchmark — on-the-fly Brotli per request vs
//! serving a precompressed sibling.
//!
//! The SPA router compressed every compressible static response on the fly
//! (tower-http `CompressionLayer`, backed by `async-compression`'s Brotli at
//! `Level::Default`) — the same immutable `/_app/immutable` bundle re-encoded
//! on EVERY request. The change teaches `ServeDir` to serve build-time
//! `.br`/`.gz` siblings (`precompressed_br()/precompressed_gzip()` +
//! `frontend/scripts/precompress.mjs`), so a request costs a file read.
//!
//! This isolates exactly that per-request delta on a JS-bundle-like payload:
//!   BEFORE — Brotli-encode the asset with async-compression Level::Default
//!            (what the layer does per request)
//!   AFTER  — read the precompressed sibling from disk (what ServeDir does)
//!
//! Run (no Postgres needed):
//!   cargo bench --features=bench --bench bench_static_precompress
//! Tunables: BENCH_ASSET_KB (700), BENCH_REPS (30)

use std::env;
use std::io::Write as _;
use std::time::Instant;

use tokio::io::AsyncReadExt;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// JS-like corpus: repetitive identifiers + literals, compresses like a real
/// minified bundle (roughly 3-5×).
fn synth_js(len: usize, seed: &mut u64) -> Vec<u8> {
    const FRAGS: &[&str] = &[
        "function(e,t,n){var r=this;",
        "return Object.assign({},",
        "const a=document.querySelector(",
        "export default{data(){return{",
        "await fetch(url,{method:'POST',headers:",
        ".map(function(x){return x.id});",
        "if(void 0!==e&&null!==t){",
        "console.error('unhandled',err);",
    ];
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        out.extend_from_slice(FRAGS[(*seed as usize) % FRAGS.len()].as_bytes());
        // sprinkle some varying identifiers so it's not pathological
        let _ = write!(out, "v{}", *seed % 1000);
    }
    out.truncate(len);
    out
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let asset_kb: usize = env_or("BENCH_ASSET_KB", 700);
    let reps: usize = env_or("BENCH_REPS", 30);
    let mut seed = 0xC0FFEEu64;
    let asset = synth_js(asset_kb * 1024, &mut seed);

    // Precompress once (build-time cost, paid once per deploy).
    let dir = tempfile::tempdir().expect("tempdir");
    let br_path = dir.path().join("bundle.js.br");
    let t = Instant::now();
    let precompressed = {
        use async_compression::tokio::bufread::BrotliEncoder;
        let mut enc = BrotliEncoder::new(std::io::Cursor::new(asset.clone()));
        let mut out = Vec::new();
        enc.read_to_end(&mut out).await.expect("precompress");
        out
    };
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    std::fs::write(&br_path, &precompressed).expect("write .br");

    println!(
        "asset: {} KiB JS-like → {} KiB brotli ({}% smaller); one-time build cost {:.1} ms\n",
        asset.len() / 1024,
        precompressed.len() / 1024,
        100 - precompressed.len() * 100 / asset.len(),
        build_ms
    );

    // BEFORE: per-request Brotli at the layer's default level.
    let mut enc_times = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        use async_compression::tokio::bufread::BrotliEncoder;
        let mut enc = BrotliEncoder::new(std::io::Cursor::new(asset.clone()));
        let mut out = Vec::new();
        enc.read_to_end(&mut out).await.expect("encode");
        std::hint::black_box(&out);
        enc_times.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    // AFTER: per-request read of the precompressed sibling.
    let mut read_times = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        let mut f = tokio::fs::File::open(&br_path).await.expect("open");
        let mut out = Vec::new();
        f.read_to_end(&mut out).await.expect("read");
        std::hint::black_box(&out);
        read_times.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    // ── Dynamic-response level sweep ─────────────────────────────────────
    // The global API CompressionLayer (main.rs) compresses JSON responses
    // per request. async-compression's Level::Default for Brotli is
    // QUALITY 11 (brotli-8.0.2 encode.rs:323 via compression-codecs) — a
    // deploy-grade setting on a per-request path. Sweep levels on a
    // JSON-like 64 KiB body to pick the runtime quality.
    let json_body = synth_js(64 * 1024, &mut seed); // JSON compresses like JS
    println!("\n# per-request Brotli level on a 64 KiB JSON-like API response");
    println!("{:<22} {:>10} {:>12}", "level", "ms/resp", "out KiB");
    for (label, level) in [
        ("Default (= q11!)", async_compression::Level::Default),
        ("Precise(4)", async_compression::Level::Precise(4)),
        ("Fastest", async_compression::Level::Fastest),
    ] {
        let mut times = Vec::with_capacity(reps);
        let mut out_len = 0;
        for _ in 0..reps {
            let t = Instant::now();
            use async_compression::tokio::bufread::BrotliEncoder;
            let mut enc =
                BrotliEncoder::with_quality(std::io::Cursor::new(json_body.clone()), level);
            let mut out = Vec::new();
            enc.read_to_end(&mut out).await.expect("encode");
            out_len = out.len();
            std::hint::black_box(&out);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "{:<22} {:>10.2} {:>12.1}",
            label,
            median(times),
            out_len as f64 / 1024.0
        );
    }

    let enc = median(enc_times);
    let read = median(read_times);
    println!(
        "{:<34} {:>10} {:>9}",
        "mode (per request)", "ms", "vs BEFORE"
    );
    println!(
        "{:<34} {:>10.2} {:>9}",
        "BEFORE on-the-fly Brotli", enc, "1.0x"
    );
    println!(
        "{:<34} {:>10.3} {:>8.0}x",
        "AFTER  precompressed read",
        read,
        enc / read
    );
    println!("\n(BEFORE also holds ~1 tokio task busy for the duration on every request;");
    println!(" AFTER additionally ships the deploy-time q11 encoding, usually smaller than");
    println!(" the runtime default level.)");
}
