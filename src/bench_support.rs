//! Phase 0 perf-benchmark support — deterministic image corpus.
//!
//! Shared by `benches/thumbnails.rs` (Task 0.2, criterion latency + output
//! size) and `benches/bench_thumbnails_mem.rs` (Task 0.3, peak RAM +
//! throughput). Gated behind the `bench` feature so it never touches normal
//! builds.
//!
//! The corpus is **generated deterministically** as photo-realistic images
//! (per-channel sums of low-frequency 2D sinusoids — smooth, gradually-varying
//! color fields like an in-focus scene — plus mild grain), so it is
//! reproducible, license-free, and compresses the way real photos do. This
//! matters for the codec comparison: pure white noise is a high-frequency
//! pathology that wildly distorts JPEG-vs-WebP byte ratios. Files are written to
//! `benches/corpus/` (git-ignored) on first run and reused afterwards.
//!
//! Files already present on disk are **always preferred** over generation — so
//! you can drop your own real photos into `benches/corpus/` using the documented
//! filenames (see [`CASE_SPECS`]) to benchmark against real-world data.

use std::io::Cursor;
use std::path::PathBuf;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};

/// One corpus entry: the encoded file bytes plus its probed dimensions.
pub struct CorpusCase {
    /// Stable identifier (e.g. `"jpeg_12mp"`), used as the bench label.
    pub name: &'static str,
    /// Container format (`"jpeg"`, `"png"`, `"gif"`, `"webp"`).
    pub format: &'static str,
    /// Actual decoded width (probed from the bytes).
    pub width: u32,
    /// Actual decoded height (probed from the bytes).
    pub height: u32,
    /// Encoded file bytes (what the thumbnail pipeline receives as `&[u8]`).
    pub bytes: Vec<u8>,
}

impl CorpusCase {
    /// Megapixels of the source image (decoded resolution).
    pub fn megapixels(&self) -> f64 {
        (self.width as f64 * self.height as f64) / 1_000_000.0
    }
}

/// Declarative description of a synthetic corpus image.
struct Spec {
    name: &'static str,
    filename: &'static str,
    format: &'static str,
    width: u32,
    height: u32,
    /// JPEG quality (ignored for non-JPEG formats).
    quality: u8,
    /// When set, injects an EXIF Orientation tag into the JPEG (e.g. 6 = rotate
    /// 90° CW) to exercise the orientation-correction path.
    exif_orientation: Option<u16>,
}

/// The corpus matrix: a spread of sizes and formats so we never trust an
/// average. Drop a real photo at `benches/corpus/<filename>` to override any
/// entry with real-world data.
const CASE_SPECS: &[Spec] = &[
    // JPEG photo-like sources at the three sizes that matter for "hundreds of
    // phone/camera photos". These dominate the real upload load.
    Spec {
        name: "jpeg_12mp",
        filename: "jpeg_12mp.jpg",
        format: "jpeg",
        width: 4000,
        height: 3000,
        quality: 90,
        exif_orientation: None,
    },
    Spec {
        name: "jpeg_24mp",
        filename: "jpeg_24mp.jpg",
        format: "jpeg",
        width: 6000,
        height: 4000,
        quality: 90,
        exif_orientation: None,
    },
    // 8000×6000 = 48 MP, just under the 50 MP MAX_DECODE_PIXELS guard.
    Spec {
        name: "jpeg_48mp",
        filename: "jpeg_48mp.jpg",
        format: "jpeg",
        width: 8000,
        height: 6000,
        quality: 90,
        exif_orientation: None,
    },
    // Non-JPEG decode paths (no DCT shrink-on-load possible — useful contrast).
    Spec {
        name: "png_large",
        filename: "png_large.png",
        format: "png",
        width: 3000,
        height: 2000,
        quality: 0,
        exif_orientation: None,
    },
    Spec {
        name: "gif_large",
        filename: "gif_large.gif",
        format: "gif",
        width: 600,
        height: 600,
        quality: 0,
        exif_orientation: None,
    },
    Spec {
        name: "webp_large",
        filename: "webp_large.webp",
        format: "webp",
        width: 1280,
        height: 853,
        quality: 0,
        exif_orientation: None,
    },
    // Small source: exercises the (future) no-upscale clamp — Large=800 target
    // is bigger than the 300 px source.
    Spec {
        name: "small_300",
        filename: "small_300.jpg",
        format: "jpeg",
        width: 300,
        height: 300,
        quality: 90,
        exif_orientation: None,
    },
    // EXIF orientation ≠ 1: exercises the rotate/flip correction path.
    Spec {
        name: "jpeg_exif_orient",
        filename: "jpeg_exif_orient.jpg",
        format: "jpeg",
        width: 4000,
        height: 3000,
        quality: 90,
        exif_orientation: Some(6),
    },
];

/// Absolute path to `benches/corpus/` next to this crate's `Cargo.toml`.
pub fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("corpus")
}

/// Load the corpus, generating any missing files on disk first.
///
/// Existing files win, so user-provided real photos are used as-is. A case that
/// fails to generate (e.g. an unavailable encoder) is logged and skipped rather
/// than aborting the whole baseline.
pub fn load_or_generate() -> Vec<CorpusCase> {
    let dir = corpus_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        panic!("bench_support: cannot create {}: {e}", dir.display());
    }

    let mut out = Vec::new();
    for spec in CASE_SPECS {
        let path = dir.join(spec.filename);
        let bytes = if path.exists() {
            match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("bench_support: skipping {} (read failed: {e})", spec.name);
                    continue;
                }
            }
        } else {
            match generate(spec) {
                Ok(b) => {
                    if let Err(e) = std::fs::write(&path, &b) {
                        eprintln!("bench_support: could not cache {} ({e})", path.display());
                    }
                    b
                }
                Err(e) => {
                    eprintln!(
                        "bench_support: skipping {} (generate failed: {e})",
                        spec.name
                    );
                    continue;
                }
            }
        };

        let (width, height) = probe_dimensions(&bytes).unwrap_or((spec.width, spec.height));
        out.push(CorpusCase {
            name: spec.name,
            format: spec.format,
            width,
            height,
            bytes,
        });
    }
    out
}

/// Probe the decoded dimensions of encoded image bytes without a full decode.
fn probe_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Render and encode one spec into file bytes.
fn generate(spec: &Spec) -> Result<Vec<u8>, String> {
    let img = synthesize(spec.width, spec.height, seed_for(spec.name));

    match spec.format {
        "jpeg" => {
            let mut buf = Vec::new();
            let encoder = JpegEncoder::new_with_quality(&mut buf, spec.quality);
            img.write_with_encoder(encoder)
                .map_err(|e| format!("jpeg encode: {e}"))?;
            match spec.exif_orientation {
                Some(o) => inject_exif_orientation(&buf, o),
                None => Ok(buf),
            }
        }
        other => {
            let fmt = match other {
                "png" => ImageFormat::Png,
                "gif" => ImageFormat::Gif,
                "webp" => ImageFormat::WebP,
                _ => return Err(format!("unknown format {other}")),
            };
            let mut buf = Vec::new();
            DynamicImage::ImageRgb8(img)
                .write_to(&mut Cursor::new(&mut buf), fmt)
                .map_err(|e| format!("{other} encode: {e}"))?;
            Ok(buf)
        }
    }
}

/// One smooth low-frequency 2D sinusoid component (a "color field").
struct Wave {
    fx: f32,
    fy: f32,
    phase: f32,
    amp: f32,
}

/// Build a **photo-realistic** RGB image: per channel, a sum of low-frequency
/// 2D sinusoids (smooth, gradually-varying color fields, like an in-focus scene)
/// plus mild ±6 grain. Unlike pure white noise, this compresses the way real
/// photos do (smooth regions JPEG/WebP handle efficiently), so the codec
/// comparison is representative rather than a high-frequency pathology.
/// Deterministic for a given seed (byte-stable corpus). Drop real photos into
/// `benches/corpus/` with the documented filenames to benchmark on real data.
fn synthesize(width: u32, height: u32, seed: u64) -> RgbImage {
    let mut img = RgbImage::new(width, height);
    let mut state = seed | 1; // xorshift requires a non-zero state

    let mk_waves = |state: &mut u64| -> [Wave; 4] {
        std::array::from_fn(|_| Wave {
            fx: 0.5 + (xorshift(state) % 7) as f32 * 0.5, // 0.5..3.5 cycles across the image
            fy: 0.5 + (xorshift(state) % 7) as f32 * 0.5,
            phase: (xorshift(state) % 628) as f32 / 100.0, // 0..2π
            amp: 18.0 + (xorshift(state) % 42) as f32,     // 18..60
        })
    };
    let channels = [
        mk_waves(&mut state),
        mk_waves(&mut state),
        mk_waves(&mut state),
    ];
    let bases = [112.0f32, 124.0, 136.0]; // mid-tone per channel

    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    let eval = |waves: &[Wave; 4], base: f32, u: f32, v: f32| -> f32 {
        let mut acc = base;
        for wv in waves {
            acc += wv.amp * (std::f32::consts::TAU * (wv.fx * u + wv.fy * v) + wv.phase).sin();
        }
        acc
    };

    for y in 0..height {
        let v = y as f32 / h;
        for x in 0..width {
            let u = x as f32 / w;
            let grain = (xorshift(&mut state) & 0x0F) as f32 - 8.0; // ±8 fine texture
            let px = std::array::from_fn(|c| {
                (eval(&channels[c], bases[c], u, v) + grain).clamp(0.0, 255.0) as u8
            });
            img.put_pixel(x, y, Rgb(px));
        }
    }
    img
}

/// Tiny xorshift64 PRNG — fast, deterministic, no dependency.
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Per-case fixed seed so each image has distinct noise but stays reproducible.
fn seed_for(name: &str) -> u64 {
    // FNV-1a over the name → splitmix-ish spread.
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash.wrapping_mul(0x9E3779B97F4A7C15) | 1
}

/// Splice a minimal, standard EXIF APP1 segment carrying a single Orientation
/// tag into a baseline JPEG, right after the SOI marker. Little-endian TIFF.
fn inject_exif_orientation(jpeg: &[u8], orientation: u16) -> Result<Vec<u8>, String> {
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return Err("not a JPEG (missing SOI)".into());
    }

    // TIFF body (little-endian "II").
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&0x2Au16.to_le_bytes()); // magic 42
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
    tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 directory entry
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // tag: Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // type: SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&(orientation as u32).to_le_bytes()); // value (SHORT in low bytes)
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD = none

    let mut payload = Vec::with_capacity(6 + tiff.len());
    payload.extend_from_slice(b"Exif\0\0");
    payload.extend_from_slice(&tiff);

    let seg_len = u16::try_from(2 + payload.len()).map_err(|_| "EXIF segment too large")?;

    let mut out = Vec::with_capacity(jpeg.len() + 4 + payload.len());
    out.extend_from_slice(&jpeg[0..2]); // SOI
    out.extend_from_slice(&[0xFF, 0xE1]); // APP1 marker
    out.extend_from_slice(&seg_len.to_be_bytes()); // APP1 length (big-endian)
    out.extend_from_slice(&payload);
    out.extend_from_slice(&jpeg[2..]); // rest of the original JPEG
    Ok(out)
}

/// Cross-platform process-level metrics the benches print alongside their
/// own measurements — CPU time, peak RSS. Previously each bench shelled
/// into `/proc/self/{stat,status,io}`, which pinned the whole suite to
/// Linux. This module reads the same numbers through `getrusage` so the
/// benches run on Darwin too (CI is Linux; developer laptops are not).
///
/// Not every /proc field has a portable equivalent — `/proc/self/io`'s
/// `read_bytes`/`write_bytes` syscall counters have no `getrusage` or
/// `libproc` analogue. The one bench that depends on them
/// (`bench_upload_spool`) stays gated on `cfg(target_os = "linux")`
/// rather than carrying a half-accurate Darwin port.
pub mod process_metrics {
    /// Process CPU time (user + system) in seconds since start.
    ///
    /// Reads both halves of `rusage(RUSAGE_SELF)` and sums them — same
    /// number the Linux `/proc/self/stat` fields 14 + 15 produce, but
    /// portable to Darwin.
    pub fn cpu_seconds() -> f64 {
        // SAFETY: `getrusage` writes into a caller-owned `rusage`; we hand
        // it a zero-initialised one and read the result after a `== 0`
        // check. No aliasing, no lifetime concerns.
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            let rc = libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) failed");
            let secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0;
            secs(usage.ru_utime) + secs(usage.ru_stime)
        }
    }

    /// Peak resident set size in kilobytes.
    ///
    /// Reads `rusage.ru_maxrss`. The UNIT is OS-specific and has bitten
    /// every tool that ever shipped this metric cross-platform:
    ///   * Linux — kilobytes (as POSIX recommends).
    ///   * Darwin — bytes.
    ///
    /// This helper hides the difference; callers get KB on both.
    pub fn peak_rss_kb() -> u64 {
        // SAFETY: same as above; getrusage is the only syscall.
        let raw = unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            let rc = libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) failed");
            usage.ru_maxrss as u64
        };
        if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
            raw / 1024
        } else {
            raw
        }
    }
}
