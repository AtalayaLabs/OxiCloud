//! Video thumbnail benchmark — Option B (server-side ffmpeg frame → WebP).
//!
//! Measures the "after" of moving video thumbnail generation off the browser
//! and onto the server:
//!   * extraction time per codec/resolution (the new server cost),
//!   * the WebP thumbnail bytes actually served per tile (the new transfer),
//!   * codec coverage incl. HEVC/MOV — the iPhone case a browser `<video>`
//!     cannot decode, so the old client-side path produced *no* thumbnail.
//!
//! vs the OLD client-side path, whose first view of each video tile re-downloaded
//! the video from the server (metadata + byte-ranges, up to the whole file) and
//! PUT 3 JPEGs back.
//!
//! Requires `ffmpeg` on PATH (with libx264/libx265/libvpx-vp9 to generate the
//! corpus). Run: `cargo bench --features=bench --bench bench_video_thumbnails`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oxicloud::application::ports::thumbnail_ports::ThumbnailFormat;
use oxicloud::application::ports::video_frame_ports::VideoFramePort;
use oxicloud::infrastructure::services::ffmpeg_video_frame_service::FfmpegVideoFrameService;
use oxicloud::infrastructure::services::thumbnail_service::{ThumbnailService, ThumbnailSize};

/// One synthetic test video: a label, output filename, and the ffmpeg encode
/// args (a `testsrc` pattern keeps it license-free and deterministic enough).
struct VideoSpec {
    name: &'static str,
    filename: &'static str,
    encode_args: &'static [&'static str],
}

const SPECS: &[VideoSpec] = &[
    VideoSpec {
        name: "h264 720p",
        filename: "video_h264_720p.mp4",
        encode_args: &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=3:size=1280x720:rate=30",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ],
    },
    VideoSpec {
        name: "h264 1080p",
        filename: "video_h264_1080p.mp4",
        encode_args: &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=3:size=1920x1080:rate=30",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ],
    },
    VideoSpec {
        // The iPhone case: HEVC/H.265 in a QuickTime .mov — undecodable by a
        // browser <video>, so the old client path produced nothing for these.
        name: "HEVC 1080p .mov",
        filename: "video_hevc_1080p.mov",
        encode_args: &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=3:size=1920x1080:rate=30",
            "-c:v",
            "libx265",
            "-tag:v",
            "hvc1",
            "-pix_fmt",
            "yuv420p",
        ],
    },
    VideoSpec {
        name: "VP9 720p .webm",
        filename: "video_vp9_720p.webm",
        encode_args: &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=3:size=1280x720:rate=30",
            "-c:v",
            "libvpx-vp9",
            "-b:v",
            "1M",
        ],
    },
];

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("corpus")
}

/// Generate a test video with ffmpeg if it isn't already on disk.
fn ensure_video(ffmpeg: &str, spec: &VideoSpec, path: &Path) {
    if path.exists() {
        return;
    }
    let mut cmd = std::process::Command::new(ffmpeg);
    cmd.arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error");
    cmd.args(spec.encode_args);
    cmd.arg(path);
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("ffmpeg gen {} exited {s}", spec.name),
        Err(e) => eprintln!("ffmpeg gen {} failed: {e}", spec.name),
    }
}

#[tokio::main]
async fn main() {
    let ffmpeg = std::env::var("OXICLOUD_FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string());
    if !FfmpegVideoFrameService::is_available(&ffmpeg) {
        eprintln!("ffmpeg not found (set OXICLOUD_FFMPEG_PATH) — cannot run video bench");
        std::process::exit(1);
    }

    let dir = corpus_dir();
    let _ = std::fs::create_dir_all(&dir);
    for spec in SPECS {
        ensure_video(&ffmpeg, spec, &dir.join(spec.filename));
    }

    let svc = FfmpegVideoFrameService::new(ffmpeg.clone(), 4, Duration::from_secs(60));

    println!("== Video thumbnails: server-side ffmpeg frame → WebP pipeline (Option B) ==");
    println!(
        "| {:<16} | {:>9} | {:>10} | {:>9} | {:>6} | {:>8} | {:>7} | {:<4} |",
        "case", "video KB", "extract ms", "frame KB", "icon B", "prev B", "large B", "ok"
    );
    println!(
        "|{:-<18}|{:-<11}|{:-<12}|{:-<11}|{:-<8}|{:-<10}|{:-<9}|{:-<6}|",
        "", "", "", "", "", "", "", ""
    );

    let mut old_transfer_kb = 0f64; // re-download the video on first view (worst case)
    let mut new_transfer_b = 0u64; // fetch the preview WebP thumbnail
    let mut covered = 0usize;

    for spec in SPECS {
        let path = dir.join(spec.filename);
        let video_kb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1024.0;

        // Best-of-3 extraction time.
        let mut best = Duration::MAX;
        let mut frame = bytes::Bytes::new();
        let mut ok = true;
        for _ in 0..3 {
            let t = Instant::now();
            match svc.extract_frame(&path).await {
                Ok(f) => {
                    best = best.min(t.elapsed());
                    frame = f;
                }
                Err(e) => {
                    println!("| {:<16} | extract failed: {e}", spec.name);
                    ok = false;
                    break;
                }
            }
        }
        if !ok || frame.is_empty() {
            continue;
        }

        let thumbs = match ThumbnailService::bench_render_all_fmt(&frame, ThumbnailFormat::Webp) {
            Ok(t) => t,
            Err(e) => {
                println!("| {:<16} | webp render failed: {e}", spec.name);
                continue;
            }
        };
        let sz = |want: ThumbnailSize| {
            thumbs
                .iter()
                .find(|(s, _)| *s == want)
                .map(|(_, b)| *b)
                .unwrap_or(0)
        };
        let (icon, preview, large) = (
            sz(ThumbnailSize::Icon),
            sz(ThumbnailSize::Preview),
            sz(ThumbnailSize::Large),
        );

        covered += 1;
        old_transfer_kb += video_kb;
        new_transfer_b += preview as u64;

        println!(
            "| {:<16} | {:>9.1} | {:>10.1} | {:>9.1} | {:>6} | {:>8} | {:>7} | {:<4} |",
            spec.name,
            video_kb,
            best.as_secs_f64() * 1000.0,
            frame.len() as f64 / 1024.0,
            icon,
            preview,
            large,
            "yes"
        );
    }

    println!(
        "\n  Coverage: {}/{} codecs produced a thumbnail server-side (incl. HEVC/.mov — the \
         browser <video> path produced 0 for HEVC).",
        covered,
        SPECS.len()
    );
    let new_kb = new_transfer_b as f64 / 1024.0;
    println!(
        "  Per-first-view transfer to show {} video tiles:\n    OLD (client re-downloads the video, worst case): {:.0} KB  +  3 JPEG PUTs/video\n    NEW (fetch the server WebP preview):              {:.1} KB  +  0 client decode\n    → up to {:.0}× less data, and it is eager (ready before the gallery asks).",
        covered,
        old_transfer_kb,
        new_kb,
        if new_kb > 0.0 {
            old_transfer_kb / new_kb
        } else {
            0.0
        }
    );
}
