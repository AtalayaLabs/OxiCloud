#!/usr/bin/env python3
"""Build minimal documents that each claim a KNOWN creation date.

For `tests/api/document_date_extraction.hurl`, which asserts the server
reads that date back out over `nc:creation_time`. Python stdlib only —
`zipfile` builds the OOXML/ODF containers and the PDF is written by hand,
so there is nothing to install (the house rule: no exiftool, no PIL).

Each format carries a DIFFERENT instant, so a cross-wiring — reading the
docx property for an odt, say — shows up as the wrong number rather than
passing by luck. The PDF is the exception: it deliberately claims the same
instant as the docx but writes it as `11:22+01'00'`, so the offset
arithmetic is observable end to end. Get the sign backwards and it reports
12:22 instead of 10:22.

Usage:
    python3 scripts/helpers/make-document-date-fixtures.py [outdir]

Default outdir is tests/fixtures/. Prints the epoch seconds each file
should produce, which is what the Hurl assertions pin.
"""

import sys
import zipfile
from datetime import datetime, timezone
from pathlib import Path

OUT = Path(sys.argv[1] if len(sys.argv) > 1 else "tests/fixtures")

# (file, iso the document claims, the instant it means)
DOCX_CREATED = "2019-03-04T10:22:00Z"
XLSX_CREATED = "2020-06-15T08:30:00Z"
PPTX_CREATED = "2022-01-20T16:05:00Z"
# ODF commonly omits the zone entirely. Read as UTC — never as server-local,
# which would make the same bytes yield different dates on two instances.
ODT_CREATED = "2021-09-09T14:45:00"
# Same instant as the docx, written with an offset.
PDF_CREATED = "D:20190304112200+01'00'"


def core_props(created: str) -> str:
    """OOXML `docProps/core.xml`.

    `dcterms:modified` sits right beside `dcterms:created` under a nearly
    identical name, and is set to a LATER date on purpose: reading the wrong
    one yields a plausible value that tracks edits, which is exactly the
    drift `nc:creation_time` refuses to report.
    """
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<cp:coreProperties'
        ' xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"'
        ' xmlns:dc="http://purl.org/dc/elements/1.1/"'
        ' xmlns:dcterms="http://purl.org/dc/terms/"'
        ' xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">'
        "<dc:title>OxiCloud date fixture</dc:title>"
        f'<dcterms:created xsi:type="dcterms:W3CDTF">{created}</dcterms:created>'
        '<dcterms:modified xsi:type="dcterms:W3CDTF">2024-11-02T08:00:00Z</dcterms:modified>'
        "</cp:coreProperties>"
    )


CONTENT_TYPES = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
    '<Default Extension="xml" ContentType="application/xml"/>'
    '<Default Extension="rels" '
    'ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
    "</Types>"
)

RELS = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
    '<Relationships '
    'xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>'
)


def write_ooxml(path: Path, body_entry: str, body: str, created: str) -> None:
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", CONTENT_TYPES)
        z.writestr("_rels/.rels", RELS)
        z.writestr("docProps/core.xml", core_props(created))
        z.writestr(body_entry, body)


def write_odf(path: Path, mimetype: str, created: str) -> None:
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        # `mimetype` first and STORED, per the ODF spec — it exists so
        # sniffers can read it without inflating anything.
        z.writestr(
            zipfile.ZipInfo("mimetype"), mimetype, compress_type=zipfile.ZIP_STORED
        )
        z.writestr(
            "meta.xml",
            '<?xml version="1.0" encoding="UTF-8"?>\n'
            '<office:document-meta'
            ' xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"'
            ' xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0">'
            "<office:meta>"
            f"<meta:creation-date>{created}</meta:creation-date>"
            "<dc:date xmlns:dc='http://purl.org/dc/elements/1.1/'>"
            "2024-11-02T08:00:00</dc:date>"
            "</office:meta></office:document-meta>",
        )
        z.writestr("content.xml", "<office:document-content/>")


def write_pdf(path: Path, created: str) -> None:
    """A small but structurally real PDF with an uncompressed /Info.

    Uncompressed on purpose: that is what the server's scanner can read, and
    what Word, LibreOffice, Ghostscript and most scanners emit. A PDF 1.5+
    document hiding /Info in a compressed object stream reports no claimed
    date — a documented blind spot, not something this fixture should paper
    over.
    """
    objects = [
        "1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n",
        "2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n",
        "3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 72 72] >> endobj\n",
        f"4 0 obj << /Producer (OxiCloud fixture) /CreationDate ({created}) >> endobj\n",
    ]
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for obj in objects:
        offsets.append(len(out))
        out += obj.encode()
    start_xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n".encode()
    out += b"0000000000 65535 f \n"
    for off in offsets:
        out += f"{off:010d} 00000 n \n".encode()
    out += (
        f"trailer << /Size {len(objects) + 1} /Root 1 0 R /Info 4 0 R >>\n"
        f"startxref\n{start_xref}\n%%EOF\n"
    ).encode()
    path.write_bytes(bytes(out))


def epoch(iso: str) -> int:
    """Epoch seconds for the instant a document claims."""
    if iso.startswith("D:"):
        body = iso[2:]
        naive = datetime.strptime(body[:14], "%Y%m%d%H%M%S")
        sign = 1 if body[14] == "+" else -1
        hours = int(body[15:17])
        minutes = int(body[18:20])
        offset = sign * (hours * 3600 + minutes * 60)
        return int(naive.replace(tzinfo=timezone.utc).timestamp()) - offset
    if iso.endswith("Z"):
        return int(
            datetime.strptime(iso, "%Y-%m-%dT%H:%M:%SZ")
            .replace(tzinfo=timezone.utc)
            .timestamp()
        )
    # No zone: read as UTC, matching the server.
    return int(
        datetime.strptime(iso, "%Y-%m-%dT%H:%M:%S")
        .replace(tzinfo=timezone.utc)
        .timestamp()
    )


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)

    write_ooxml(
        OUT / "doc-date.docx",
        "word/document.xml",
        "<w:document/>",
        DOCX_CREATED,
    )
    write_ooxml(
        OUT / "doc-date.xlsx",
        "xl/workbook.xml",
        "<workbook/>",
        XLSX_CREATED,
    )
    write_ooxml(
        OUT / "doc-date.pptx",
        "ppt/presentation.xml",
        "<p:presentation/>",
        PPTX_CREATED,
    )
    write_odf(
        OUT / "doc-date.odt",
        "application/vnd.oasis.opendocument.text",
        ODT_CREATED,
    )
    write_pdf(OUT / "doc-date.pdf", PDF_CREATED)

    print("fixture                 claims                       nc:creation_time")
    for name, claimed in [
        ("doc-date.docx", DOCX_CREATED),
        ("doc-date.xlsx", XLSX_CREATED),
        ("doc-date.pptx", PPTX_CREATED),
        ("doc-date.odt", ODT_CREATED),
        ("doc-date.pdf", PDF_CREATED),
    ]:
        print(f"{name:<23} {claimed:<28} {epoch(claimed)}")


if __name__ == "__main__":
    main()
