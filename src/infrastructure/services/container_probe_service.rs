//! Does this container actually carry video? — an `ffprobe` wrapper.
//!
//! A container signature says what the WRAPPER is, not what is inside it,
//! and `infer` matches signatures. For four shapes the stored type is a
//! coin flip, in both directions:
//!
//! | container | stored today | truth when there is no video stream |
//! |---|---|---|
//! | MP4, brand not `M4A` | `video/mp4` | `audio/mp4` |
//! | WebM | `video/webm` | `audio/webm` |
//! | Matroska | `video/x-matroska` | `audio/x-matroska` |
//! | Ogg | **`audio/ogg`** | `video/ogg` *when there IS video* |
//!
//! Ogg runs the other way: `infer` has no `video/ogg` entry at all, so
//! Theora in Ogg is stored as audio. The correction is therefore
//! bidirectional, not "demote video to audio".
//!
//! ### Why this cannot be fixed at upload
//!
//! `refine_content_type` sees the first 8 KiB, and an MP4's `moov` atom is
//! routinely past that — for a non-faststart file it is at the END. So the
//! container default is often the best answer available at ingest, and the
//! backlog has to be drained by something that can open the whole file.
//!
//! ### ⚠ The ordering trap
//!
//! Every derived feature gates on the mime BEFORE running its extractor
//! (`on_file_created` → `is_audio_file(mime)?` → audio extractor), so an
//! audio-only MP4 stored as `video/mp4` never reaches the code that could
//! discover it is audio. The information is locked behind the decision it
//! would correct. Any caller must therefore probe OUTSIDE the gate, never
//! from inside an extractor the gate may never call.
//!
//! ### ⚠ Cover art is a video stream
//!
//! An MP3 or M4A with embedded album art carries a real video stream —
//! an mjpeg or png still — and a naive "has a video stream?" probe calls
//! every such file a video. The disposition flag `attached_pic` is what
//! separates them, and ignoring it would mis-promote a large share of any
//! music library. That is why this parses `ffprobe`'s JSON rather than
//! counting lines of `-select_streams v` output.
//!
//! ### Optional, like every other ffmpeg user here
//!
//! Video thumbnails already degrade gracefully when ffmpeg is absent, and
//! so does this: no binary means no correction, the container default
//! stands, and a later run picks it up. Nothing fails.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::common::errors::DomainError;

/// The four container types whose audio-vs-video reading is ambiguous
/// from magic bytes alone, paired with the type to use when the probe
/// disagrees with what is stored.
///
/// `(stored, has_video, corrected)` — a flat table rather than nested
/// matching, so adding a fifth container is one line and the inverse
/// Ogg case cannot be lost in an `if`.
const AMBIGUOUS: &[(&str, bool, &str)] = &[
    // No video stream → it is audio in a video wrapper.
    ("video/mp4", false, "audio/mp4"),
    ("video/webm", false, "audio/webm"),
    ("video/x-matroska", false, "audio/x-matroska"),
    // Ogg is the inverse: infer has no video/ogg entry, so Theora
    // arrives labelled as audio and needs promoting.
    ("audio/ogg", true, "video/ogg"),
];

/// Is this a type whose audio/video reading needs a stream probe?
pub fn is_ambiguous_container(mime: &str) -> bool {
    AMBIGUOUS.iter().any(|(stored, _, _)| *stored == mime)
}

/// The corrected type for `mime` given what the probe found, or `None`
/// when the stored type is already right.
pub fn corrected_mime(mime: &str, has_video: bool) -> Option<&'static str> {
    AMBIGUOUS
        .iter()
        .find(|(stored, when, _)| *stored == mime && *when == has_video)
        .map(|(_, _, corrected)| *corrected)
}

pub struct ContainerProbe {
    ffprobe_path: String,
    /// Bounds concurrent child processes, like the video-frame service:
    /// a corpus sweep would otherwise fork one per content.
    semaphore: Arc<Semaphore>,
    timeout: Duration,
}

impl ContainerProbe {
    pub fn new(ffprobe_path: String, concurrency: usize, timeout: Duration) -> Self {
        Self {
            ffprobe_path,
            semaphore: Arc::new(Semaphore::new(concurrency.max(1))),
            timeout,
        }
    }

    /// Derive the `ffprobe` path from the configured `ffmpeg` one.
    ///
    /// They ship together and live side by side, so `/opt/bin/ffmpeg`
    /// implies `/opt/bin/ffprobe` and a bare `ffmpeg` implies a bare
    /// `ffprobe` resolved on `PATH`. Deriving avoids a second env var
    /// that would be wrong only in the configuration nobody has.
    pub fn path_from_ffmpeg(ffmpeg_path: &str) -> String {
        match ffmpeg_path.rsplit_once('/') {
            Some((dir, bin)) => format!("{dir}/{}", bin.replace("ffmpeg", "ffprobe")),
            None => ffmpeg_path.replace("ffmpeg", "ffprobe"),
        }
    }

    /// Startup probe, mirroring `FfmpegVideoFrameService::is_available`:
    /// synchronous so the composition root can decide once at boot
    /// rather than failing per content.
    pub fn is_available(ffprobe_path: &str) -> bool {
        std::process::Command::new(ffprobe_path)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Whether `path` carries at least one NON-cover-art video stream.
    ///
    /// Asks for JSON and reads `disposition.attached_pic`, because album
    /// art is a video stream and counting raw stream lines would promote
    /// every tagged MP3 to video.
    pub async fn has_video_stream(&self, path: &Path) -> Result<bool, DomainError> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| DomainError::internal_error("ContainerProbe", e.to_string()))?;

        let out = tokio::time::timeout(
            self.timeout,
            tokio::process::Command::new(&self.ffprobe_path)
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v",
                    "-show_entries",
                    "stream=codec_type:stream_disposition=attached_pic",
                    "-of",
                    "json",
                ])
                .arg(path)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| {
            // Timeout is environmental, not a property of the content.
            DomainError::timeout("ContainerProbe", "ffprobe timed out")
        })?
        .map_err(|e| DomainError::internal_error("ContainerProbe", format!("ffprobe: {e}")))?;

        if !out.status.success() {
            return Err(DomainError::internal_error(
                "ContainerProbe",
                format!("ffprobe exited {}", out.status),
            ));
        }

        let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
            DomainError::internal_error("ContainerProbe", format!("ffprobe json: {e}"))
        })?;

        Ok(Self::has_real_video(&parsed))
    }

    /// Split out so the cover-art rule is testable without a binary.
    fn has_real_video(parsed: &serde_json::Value) -> bool {
        parsed
            .get("streams")
            .and_then(|s| s.as_array())
            .is_some_and(|streams| {
                streams.iter().any(|s| {
                    let is_video = s.get("codec_type").and_then(|c| c.as_str()) == Some("video");
                    // Absent disposition means not attached art.
                    let attached = s
                        .get("disposition")
                        .and_then(|d| d.get("attached_pic"))
                        .and_then(|a| a.as_i64())
                        .unwrap_or(0);
                    is_video && attached == 0
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffprobe_path_follows_ffmpeg() {
        assert_eq!(ContainerProbe::path_from_ffmpeg("ffmpeg"), "ffprobe");
        assert_eq!(
            ContainerProbe::path_from_ffmpeg("/opt/homebrew/bin/ffmpeg"),
            "/opt/homebrew/bin/ffprobe"
        );
        // A directory containing "ffmpeg" must not be rewritten — only
        // the binary name is replaced.
        assert_eq!(
            ContainerProbe::path_from_ffmpeg("/opt/ffmpeg/bin/ffmpeg"),
            "/opt/ffmpeg/bin/ffprobe"
        );
    }

    /// ⚠ The cover-art case, and the reason this parses JSON.
    ///
    /// The JSON below is not invented: it was captured from the real
    /// `ffprobe` invocation this service issues, against an audio-only
    /// MP4, an MP4 with a genuine video track, and an M4A with an
    /// attached cover image. So the parser is tested against the shape
    /// the binary actually emits, without CI needing the binary.
    ///
    /// An MP3 or M4A with embedded album art reports a real video
    /// stream — an mjpeg still with `attached_pic: 1`. A probe that
    /// counted video streams would call every tagged track a video and
    /// promote a whole music library out of the audio pipeline.
    #[test]
    fn album_art_is_not_a_video_stream() {
        let art = serde_json::json!({
            "streams": [{ "codec_type": "video", "disposition": { "attached_pic": 1 } }]
        });
        assert!(!ContainerProbe::has_real_video(&art));

        let real = serde_json::json!({
            "streams": [{ "codec_type": "video", "disposition": { "attached_pic": 0 } }]
        });
        assert!(ContainerProbe::has_real_video(&real));

        // Audio-only: ffprobe returns an empty array, not an error.
        let none = serde_json::json!({ "streams": [] });
        assert!(!ContainerProbe::has_real_video(&none));

        // A stream with no disposition object at all is real video.
        let bare = serde_json::json!({ "streams": [{ "codec_type": "video" }] });
        assert!(ContainerProbe::has_real_video(&bare));
    }

    /// The correction table, including the inverse Ogg direction — the
    /// one an "if it has no video, demote it" implementation loses.
    #[test]
    fn the_correction_runs_in_both_directions() {
        assert_eq!(corrected_mime("video/mp4", false), Some("audio/mp4"));
        assert_eq!(corrected_mime("video/webm", false), Some("audio/webm"));
        assert_eq!(
            corrected_mime("video/x-matroska", false),
            Some("audio/x-matroska")
        );
        // Ogg the other way: infer has no video/ogg entry, so Theora
        // arrives as audio and must be PROMOTED.
        assert_eq!(corrected_mime("audio/ogg", true), Some("video/ogg"));

        // Already correct → no change, in every direction.
        assert_eq!(corrected_mime("video/mp4", true), None);
        assert_eq!(corrected_mime("audio/ogg", false), None);
        assert_eq!(corrected_mime("image/png", false), None);
    }

    #[test]
    fn only_the_four_containers_are_probed_unit() {
        assert!(is_ambiguous_container("video/mp4"));
        assert!(is_ambiguous_container("audio/ogg"));
        assert!(!is_ambiguous_container("audio/mpeg"));
        assert!(!is_ambiguous_container("image/jpeg"));
        // Cheap probes only: every extra entry here is a full blob
        // download per content per run.
        assert_eq!(AMBIGUOUS.len(), 4);
    }
}

// Requires `ffmpeg` (which ships `ffprobe`) on PATH:
//   just test-integration container_probe
//
// These cover the one thing the unit tests above cannot: the ffprobe
// INVOCATION. The argument list is eight fixed strings, and changing
// one — `-show_entries`, the `-of json` selector, the
// `stream_disposition` key — leaves every JSON-parsing test passing
// while the real probe returns something useless. The unit tests pin
// how we read the answer; these pin that we asked the question.
//
// A broken invocation degrades safely in production (Err → a
// `container_probe_failed` finding → the container default stands), so
// this catches it at CI time rather than preventing damage.
#[cfg(integration_tests)]
mod integration_tests {
    use super::*;
    use std::time::Duration;

    fn probe() -> ContainerProbe {
        ContainerProbe::new(
            ContainerProbe::path_from_ffmpeg("ffmpeg"),
            1,
            Duration::from_secs(30),
        )
    }

    /// Both answers, against real files, through the real binary.
    ///
    /// One test rather than two so the controls cannot drift apart: a
    /// probe that always answered `false` would pass a cover-art-only
    /// test, and one that always answered `true` would pass a
    /// video-only test. Only the pair means anything.
    ///
    /// ⚠ The cover-art half is the one that matters. That file carries
    /// a genuine mjpeg video stream, separated from real video ONLY by
    /// `disposition.attached_pic`. Get it wrong and every tagged track
    /// in a music library is promoted to `video/*` and silently drops
    /// out of audio extraction.
    #[tokio::test]
    async fn the_real_binary_answers_both_ways() {
        assert!(
            ContainerProbe::is_available(&ContainerProbe::path_from_ffmpeg("ffmpeg")),
            "ffprobe is required for the integration suite — install ffmpeg",
        );
        let p = probe();

        let video = std::path::Path::new("tests/fixtures/container-video.mp4");
        assert!(
            p.has_video_stream(video).await.expect("probe the video"),
            "a real video track was not detected — the ffprobe invocation \
             is wrong, or -show_entries no longer returns codec_type",
        );

        let art = std::path::Path::new("tests/fixtures/container-cover-art.m4a");
        assert!(
            !p.has_video_stream(art).await.expect("probe the cover art"),
            "embedded album art was counted as a video stream — every \
             tagged track in a music library would be promoted out of \
             audio extraction",
        );
    }

    /// End to end over the correction table: what the probe reports has
    /// to turn into the right stored type.
    #[tokio::test]
    async fn a_probed_answer_maps_to_the_corrected_type() {
        let p = probe();

        // Audio hiding in a video wrapper. The cover-art fixture is an
        // M4A, so read it as if it had arrived labelled `video/mp4` —
        // which is exactly what a brand-less audio MP4 does.
        let art = std::path::Path::new("tests/fixtures/container-cover-art.m4a");
        let has_video = p.has_video_stream(art).await.expect("probe");
        assert_eq!(corrected_mime("video/mp4", has_video), Some("audio/mp4"));

        // Real video stays put…
        let video = std::path::Path::new("tests/fixtures/container-video.mp4");
        let has_video = p.has_video_stream(video).await.expect("probe");
        assert_eq!(corrected_mime("video/mp4", has_video), None);
        // …and the same reading promotes a Theora-in-Ogg.
        assert_eq!(corrected_mime("audio/ogg", has_video), Some("video/ogg"));
    }
}
