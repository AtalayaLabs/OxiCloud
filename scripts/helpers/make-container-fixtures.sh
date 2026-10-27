#!/usr/bin/env bash
# Regenerate the two container-probe fixtures.
#
#   tests/fixtures/container-video.mp4   a real video track   → probe TRUE
#   tests/fixtures/container-cover-art.m4a  audio + album art → probe FALSE
#
# Two, not three. The probe has exactly two answers, so one fixture per
# answer is the whole matrix — and the useful "false" is the cover-art
# one rather than a plain audio-only file. An M4A with embedded art
# carries a REAL mjpeg video stream, so it is what separates a correct
# probe from one that counts video streams and promotes an entire music
# library out of the audio pipeline. A plain audio-only MP4 would test
# the same answer down an easier path (`streams: []`), which the unit
# tests already cover from captured JSON.
#
# These exist to pin the ffprobe INVOCATION, which unit tests cannot:
# the arguments are eight fixed strings, and editing one leaves the
# JSON-parsing tests passing while the real probe returns nothing
# useful. See `container_probe_service.rs` integration tests.
#
# Requires ffmpeg (ships ffprobe). Usage, from the repo root:
#     scripts/helpers/make-container-fixtures.sh

set -euo pipefail

OUT_DIR="${1:-tests/fixtures}"

# A genuine video track — the positive control. Without it, a probe that
# always answered "no video" would pass every other assertion.
ffmpeg -hide_banner -loglevel error \
    -f lavfi -i "testsrc=duration=0.3:size=64x48:rate=10" \
    -f lavfi -i "sine=frequency=440:duration=0.3" \
    -c:v libx264 -c:a aac \
    -y "$OUT_DIR/container-video.mp4"

# Audio plus an attached cover image. `-disposition:v attached_pic` is
# the whole point: ffprobe reports a video stream here, and only the
# disposition flag says it is artwork.
ffmpeg -hide_banner -loglevel error \
    -f lavfi -i "sine=frequency=440:duration=0.3" \
    -f lavfi -i "color=c=red:s=32x32:d=0.3" \
    -map 0:a -map 1:v -c:a aac -c:v mjpeg -frames:v 1 \
    -disposition:v attached_pic \
    -y "$OUT_DIR/container-cover-art.m4a"

for f in container-video.mp4 container-cover-art.m4a; do
    echo "wrote $OUT_DIR/$f ($(wc -c < "$OUT_DIR/$f" | tr -d ' ') bytes)"
done
