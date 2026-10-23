#!/usr/bin/env python3
"""Regenerate `tests/fixtures/exif-sample.jpg`.

The fixture is a 96x64 JPEG carrying a real EXIF APP1 segment: capture
date, camera make/model, a non-default orientation and GPS coordinates.
It exists because every other image fixture in the repo is EXIF-free, so
nothing exercised the extractor end to end — see the asserting tests in
`src/infrastructure/services/exif_service.rs` and
`tests/api/photos_exif_extraction.hurl`.

Committed as a generator, not just a binary, so the expected values are
reviewable and the fixture is reproducible. Not wired into the build: the
fixture is checked in and stable. To regenerate, from the repo root:

    magick -size 96x64 gradient:navy-orange -quality 70 /tmp/plain.jpg
    python3 scripts/helpers/make-exif-fixture.py /tmp/plain.jpg \\
        tests/fixtures/exif-sample.jpg

Requires ImageMagick for the carrier image. See
`scripts/helpers/make-id3-fixture.sh` for the audio counterpart.

ImageMagick cannot do this alone — `-set exif:...` is silently dropped and
produces no profile — and exiftool is not a dependency worth adding for one
fixture, so the segment is assembled here from the TIFF/EXIF spec directly.
Stdlib only.
"""

import struct
import sys

# TIFF field types
ASCII = 2
SHORT = 3
LONG = 4
RATIONAL = 5

TYPE_SIZE = {ASCII: 1, SHORT: 2, LONG: 4, RATIONAL: 8}

# --- The values the fixture asserts on -------------------------------------
MAKE = "OxiCloud"
MODEL = "TestCam A1"
ORIENTATION = 6  # rotate 90° CW — a value no default would produce
DATE_TIME_ORIGINAL = "2019:07:14 10:30:00"
# Pointe du Raz, Brittany. Deliberately not 0,0 and not a round number.
LAT = (48, 2, 18.0)
LON = (4, 44, 3.0)
LAT_REF = "N"
LON_REF = "W"


def rational(num, den):
    return struct.pack("<II", num, den)


def dms(d, m, s):
    return rational(d, 1) + rational(m, 1) + rational(int(s * 100), 100)


def build_ifd(entries, ifd_offset):
    """Serialise one IFD.

    `entries` is a list of (tag, type, count, payload_bytes). Payloads of 4
    bytes or fewer live inline; longer ones are appended after the IFD and
    referenced by offset. All offsets are from the start of the TIFF header,
    so the caller passes where this IFD begins.
    """
    count = len(entries)
    # 2-byte count + 12 bytes per entry + 4-byte next-IFD pointer
    data_start = ifd_offset + 2 + 12 * count + 4

    out = struct.pack("<H", count)
    overflow = b""
    for tag, typ, cnt, payload in sorted(entries, key=lambda e: e[0]):
        assert len(payload) == TYPE_SIZE[typ] * cnt, (tag, len(payload))
        if len(payload) <= 4:
            value = payload + b"\x00" * (4 - len(payload))
        else:
            value = struct.pack("<I", data_start + len(overflow))
            overflow += payload
        out += struct.pack("<HHI", tag, typ, cnt) + value
    return out, overflow, data_start


def asciiz(s):
    return s.encode("ascii") + b"\x00"


def build_exif():
    # Lay the three IFDs out back to back. IFD0 is sized first (its entry
    # count is fixed), then the SubIFD and GPS IFD follow, so IFD0's pointer
    # tags can name their offsets.
    ifd0_entries = 5  # Make, Model, Orientation, ExifIFD, GPSIFD
    ifd0_start = 8
    ifd0_overflow = len(asciiz(MAKE)) + len(asciiz(MODEL))
    ifd0_end = ifd0_start + 2 + 12 * ifd0_entries + 4 + ifd0_overflow

    sub_entries = 1  # DateTimeOriginal
    sub_start = ifd0_end
    sub_overflow = len(asciiz(DATE_TIME_ORIGINAL))
    sub_end = sub_start + 2 + 12 * sub_entries + 4 + sub_overflow

    gps_start = sub_end

    ifd0, ifd0_data, _ = build_ifd(
        [
            (0x010F, ASCII, len(asciiz(MAKE)), asciiz(MAKE)),
            (0x0110, ASCII, len(asciiz(MODEL)), asciiz(MODEL)),
            (0x0112, SHORT, 1, struct.pack("<H", ORIENTATION)),
            (0x8769, LONG, 1, struct.pack("<I", sub_start)),
            (0x8825, LONG, 1, struct.pack("<I", gps_start)),
        ],
        ifd0_start,
    )

    sub, sub_data, _ = build_ifd(
        [
            (
                0x9003,
                ASCII,
                len(asciiz(DATE_TIME_ORIGINAL)),
                asciiz(DATE_TIME_ORIGINAL),
            ),
        ],
        sub_start,
    )

    gps, gps_data, _ = build_ifd(
        [
            (0x0001, ASCII, 2, asciiz(LAT_REF)),
            (0x0002, RATIONAL, 3, dms(*LAT)),
            (0x0003, ASCII, 2, asciiz(LON_REF)),
            (0x0004, RATIONAL, 3, dms(*LON)),
        ],
        gps_start,
    )

    tiff = (
        b"II\x2a\x00"
        + struct.pack("<I", ifd0_start)
        + ifd0
        + struct.pack("<I", 0)
        + ifd0_data
        + sub
        + struct.pack("<I", 0)
        + sub_data
        + gps
        + struct.pack("<I", 0)
        + gps_data
    )

    # Sanity: the computed offsets must match where the IFDs actually landed.
    assert tiff[sub_start : sub_start + 2] == sub[:2], "SubIFD offset drifted"
    assert tiff[gps_start : gps_start + 2] == gps[:2], "GPS IFD offset drifted"

    payload = b"Exif\x00\x00" + tiff
    return b"\xff\xe1" + struct.pack(">H", len(payload) + 2) + payload


def main():
    src, dst = sys.argv[1], sys.argv[2]
    jpeg = open(src, "rb").read()
    assert jpeg[:2] == b"\xff\xd8", "not a JPEG"
    # APP1 goes immediately after SOI, before any other marker.
    open(dst, "wb").write(jpeg[:2] + build_exif() + jpeg[2:])
    print(f"wrote {dst} ({len(jpeg) + len(build_exif())} bytes)")


if __name__ == "__main__":
    main()
