#!/usr/bin/env bash
# Regenerate `tests/fixtures/id3-sample.mp3`.
#
# A 2.2 KB MP3 — 0.4 s of a 440 Hz sine — carrying a full ID3v2.4 tag set.
# It exists because no fixture in the repo was tagged, so nothing asserted
# that a title or an artist survives the trip out of a blob. The asserting
# tests are in `src/infrastructure/services/audio_metadata_service.rs` and
# `tests/api/audio_id3_extraction.hurl`.
#
# Committed as a generator, not just a binary, so the expected values are
# reviewable and the fixture is reproducible. Not wired into the build: the
# fixture is checked in and stable.
#
# ffmpeg writes ID3v2.4, which is the point as much as a convenience — the
# year arrives as TDRC, and `id3::Tag::year()` reads only the v2.3 TYER.
# That gap was live in shipped code until this fixture surfaced it; keep the
# tag version as-is or the regression pin stops pinning anything.
#
# Requires ffmpeg with libmp3lame. See `scripts/helpers/make-exif-fixture.py`
# for the image counterpart.
#
# Usage, from the repo root:
#     scripts/helpers/make-id3-fixture.sh [out.mp3]

set -euo pipefail

OUT="${1:-tests/fixtures/id3-sample.mp3}"

ffmpeg -hide_banner -loglevel error \
    -f lavfi -i "sine=frequency=440:duration=0.4" \
    -codec:a libmp3lame -b:a 32k -ar 22050 -ac 1 \
    -metadata title="Sniff Test" \
    -metadata artist="OxiCloud QA" \
    -metadata album="Fixture Sessions" \
    -metadata album_artist="Various QA" \
    -metadata genre="Electronic" \
    -metadata track="7" \
    -metadata disc="2" \
    -metadata date="2019" \
    -y "$OUT"

echo "wrote $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes)"
