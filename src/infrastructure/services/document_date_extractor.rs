//! Read the creation date a document claims about itself.
//!
//! The second consumer of `document_format`'s registry (the first being the
//! search index's text extractor), and the writer that makes
//! `storage.content_metadata.created_at` mean something for files that are
//! not photos or videos. See `docs/plan/normalize-file-dates.md` §4 and
//! `docs/architecture/dates.md`.
//!
//! **This parses untrusted input to read one timestamp**, so every entry
//! point is bounded and panic-guarded. The codebase has already been bitten
//! here: `text_extractor::extract_pdf` carries a `catch_unwind` because a
//! malformed PDF *panics* `pdf_extract`, pinned by
//! `corrupt_pdf_fails_terminally_instead_of_panicking` and
//! `tests/api/broken_pdf_resilience.hurl`. A date is worth far less than the
//! worker, so the rule here is: any failure yields `None` and the content is
//! simply recorded as claiming no date.
//!
//! (Related: `panic = "abort"` must stay off release builds, or every
//! `catch_unwind` in this file silently stops working.)

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use quick_xml::events::Event;

use super::document_format::{self, CreationDateSource};

/// Most a metadata entry may expand to before we stop reading it.
///
/// `docProps/core.xml` is a few hundred bytes in practice; ODF `meta.xml` is
/// usually under 2 KiB. 256 KiB is far beyond any honest document and keeps a
/// zip bomb in `docProps` from becoming a worker stall — the entry is
/// attacker-controlled and the declared size cannot be trusted, so the cap is
/// applied to what we actually read, not to what the header claims.
const MAX_METADATA_ENTRY_BYTES: u64 = 256 * 1024;

/// How much of a PDF to scan for the `/Info` dictionary, at each end.
///
/// Not a whole-file read: a PDF's trailer lives at the end, and a linearized
/// one repeats its `/Info` near the front, so both ends together cover the
/// realistic cases at a fixed cost instead of pulling a 500 MB scan into RAM
/// to find twenty bytes.
const PDF_SCAN_WINDOW: usize = 64 * 1024;

/// The creation date this document claims, or `None`.
///
/// `None` covers every failure — unsupported format, unreadable file, broken
/// container, absent property, unparseable timestamp, panicking parser —
/// because they are all the same answer downstream: this content claims no
/// creation date, so `nc:creation_time` reports `0` and `media_sort_date`
/// falls through to the content clock. Never guess a date; see
/// `docs/architecture/dates.md` § When creation date is unknown.
pub fn extract(path: &Path, mime_type: &str) -> Option<DateTime<Utc>> {
    // The name is empty on purpose: by the time extraction runs, `mime_type`
    // has been through `mime_detect::refine_content_type`, so a `.docx` that
    // arrived as `application/octet-stream` already carries its real type.
    // The stored path is content-addressed and has no meaningful extension to
    // offer anyway.
    let source = document_format::classify("", mime_type)?.creation_date_source()?;

    match catch_unwind(AssertUnwindSafe(|| match source {
        CreationDateSource::OoxmlCoreProps => {
            zipped_property(path, "docProps/core.xml", b"created")
        }
        CreationDateSource::OdfMeta => zipped_property(path, "meta.xml", b"creation-date"),
        CreationDateSource::PdfTrailer => pdf_creation_date(path),
    })) {
        Ok(found) => found,
        Err(_) => {
            tracing::warn!(
                target: "oxicloud::metadata",
                ?source,
                "document date extractor panicked; recording no claimed date",
            );
            None
        }
    }
}

/// Pull one dated element out of a single zip entry.
///
/// Opens the archive over the FILE rather than a byte buffer: `ZipArchive`
/// needs `Seek`, which a `File` already is, so reading one small entry out of
/// a 300 MB `.xlsx` costs the central directory plus that entry — not the
/// whole archive in RAM.
///
/// `local_name` is matched without its namespace prefix, because the prefix
/// is a document-local choice: the same property is `dcterms:created` in OOXML
/// written by Word and `created` with a default namespace elsewhere, and ODF
/// writes `meta:creation-date`. Matching the suffix is what makes one parser
/// serve both.
fn zipped_property(path: &Path, entry: &str, local_name: &[u8]) -> Option<DateTime<Utc>> {
    let file = File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file)).ok()?;
    let entry = archive.by_name(entry).ok()?;

    let reader = BufReader::new(entry.take(MAX_METADATA_ENTRY_BYTES));
    let mut xml = quick_xml::Reader::from_reader(reader);
    let mut buf = Vec::new();
    let mut inside = false;

    loop {
        match xml.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                inside = local_suffix(e.name().as_ref()) == local_name;
            }
            Ok(Event::Text(t)) if inside => {
                let decoded = t.xml_content(quick_xml::XmlVersion::Implicit1_0).ok()?;
                return parse_iso_8601(decoded.trim());
            }
            Ok(Event::End(_)) => inside = false,
            Ok(Event::Eof) => return None,
            Ok(_) => {}
            // A truncated or malformed entry is "no date", not an error worth
            // propagating: the cap above can itself cut a document mid-tag.
            Err(_) => return None,
        }
        buf.clear();
    }
}

/// The part of a tag name after its namespace prefix.
fn local_suffix(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|b| *b == b':') {
        Some(at) => &name[at + 1..],
        None => name,
    }
}

/// Find `/CreationDate` in a PDF's `/Info` dictionary.
///
/// ⚠ **A deliberate shortcut, with a known blind spot.** This scans for the
/// key rather than resolving the trailer's `/Info` reference through the
/// cross-reference table, because the proper parse needs a full PDF object
/// model and `lopdf` is **not** in the tree — only `pdf-extract`, which does
/// not expose metadata. (`docs/plan/normalize-file-dates.md` §4 claimed both
/// were available; that was wrong.) Adding a PDF object parser to read one
/// date is a dependency decision of its own, not something to slip into this
/// phase.
///
/// What it misses: PDF 1.5+ may place `/Info` inside a *compressed object
/// stream*, where the key is deflated and this scan cannot see it. Those
/// documents report no claimed date — the honest answer, and the same one
/// they got before this existed. What it gets right is the common case: every
/// PDF with an uncompressed `/Info`, which is what Word, LibreOffice,
/// Ghostscript and most scanners emit.
///
/// If a revised document has several `/CreationDate` entries, the LAST one in
/// the file wins: PDF appends revisions, so the latest trailer is the live
/// one.
fn pdf_creation_date(path: &Path) -> Option<DateTime<Utc>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();

    let mut window = |from: u64, size: usize| -> Option<Vec<u8>> {
        file.seek(SeekFrom::Start(from)).ok()?;
        let mut buf = vec![0u8; size];
        let read = file.read(&mut buf).ok()?;
        buf.truncate(read);
        Some(buf)
    };

    // Tail first — the trailer is there, and so is the live revision.
    let tail_at = len.saturating_sub(PDF_SCAN_WINDOW as u64);
    if let Some(bytes) = window(tail_at, PDF_SCAN_WINDOW)
        && let Some(found) = scan_pdf_creation_date(&bytes)
    {
        return Some(found);
    }
    // Then the head, for a linearized document that repeats `/Info` up front.
    // Skipped when the file is small enough that the tail window already
    // covered all of it.
    if tail_at > 0
        && let Some(bytes) = window(0, PDF_SCAN_WINDOW)
    {
        return scan_pdf_creation_date(&bytes);
    }
    None
}

/// The last `/CreationDate (...)` in a byte window.
fn scan_pdf_creation_date(bytes: &[u8]) -> Option<DateTime<Utc>> {
    const KEY: &[u8] = b"/CreationDate";
    let mut found = None;
    let mut from = 0usize;

    while let Some(at) = find(&bytes[from..], KEY) {
        let after = from + at + KEY.len();
        if let Some(value) = pdf_string_at(&bytes[after..])
            && let Some(dt) = parse_pdf_date(&value)
        {
            found = Some(dt);
        }
        from = after;
    }
    found
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The contents of the `( … )` literal string that follows, if any.
///
/// Stops at the closing paren, honouring the backslash escape so a date
/// containing `\)` cannot truncate the value. Bounded to a length no honest
/// date reaches, so a window full of `(((((` cannot walk far.
fn pdf_string_at(bytes: &[u8]) -> Option<Vec<u8>> {
    const MAX_DATE_LEN: usize = 64;
    let open = bytes.iter().position(|b| !b.is_ascii_whitespace())?;
    if bytes.get(open)? != &b'(' {
        return None;
    }
    let mut out = Vec::new();
    let mut escaped = false;
    for b in bytes.iter().skip(open + 1).take(MAX_DATE_LEN) {
        match (escaped, *b) {
            (true, c) => {
                out.push(c);
                escaped = false;
            }
            (false, b'\\') => escaped = true,
            (false, b')') => return Some(out),
            (false, c) => out.push(c),
        }
    }
    None
}

/// Parse a PDF date string: `D:YYYYMMDDHHmmSSOHH'mm'`, where everything
/// after the year is optional and `O` is `+`, `-` or `Z`.
///
/// Permissive about the parts a producer omits (the spec allows truncation
/// at any field) and strict about what it does supply — a half-parsed date
/// would be a confident wrong answer, which is worse than none.
fn parse_pdf_date(raw: &[u8]) -> Option<DateTime<Utc>> {
    let s = std::str::from_utf8(raw).ok()?.trim();
    let s = s.strip_prefix("D:").unwrap_or(s);
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 {
        return None;
    }
    let num = |at: usize, len: usize, default: u32| -> Option<u32> {
        if digits.len() < at + len {
            return Some(default);
        }
        digits.get(at..at + len)?.parse().ok()
    };

    let year: i32 = digits.get(0..4)?.parse().ok()?;
    let naive = chrono::NaiveDate::from_ymd_opt(year, num(4, 2, 1)?, num(6, 2, 1)?)?.and_hms_opt(
        num(8, 2, 0)?,
        num(10, 2, 0)?,
        num(12, 2, 0)?,
    )?;

    // The offset tells us what instant that local time was. Absent means the
    // producer did not say; treat it as UTC rather than inventing a zone.
    let rest = &s[digits.len()..];
    let offset_seconds = parse_pdf_offset(rest).unwrap_or(0);
    Utc.from_utc_datetime(&naive)
        .checked_sub_signed(chrono::Duration::seconds(offset_seconds as i64))
}

/// `+HH'mm'`, `-HH'mm'` or `Z` → seconds east of UTC.
fn parse_pdf_offset(rest: &str) -> Option<i32> {
    let mut chars = rest.chars();
    let sign = match chars.next()? {
        '+' => 1,
        '-' => -1,
        'Z' | 'z' => return Some(0),
        _ => return None,
    };
    let rest: String = chars.collect();
    let digits: Vec<u32> = rest
        .chars()
        .filter(|c| c.is_ascii_digit())
        .filter_map(|c| c.to_digit(10))
        .collect();
    if digits.len() < 2 {
        return None;
    }
    let hours = (digits[0] * 10 + digits[1]) as i32;
    let minutes = if digits.len() >= 4 {
        (digits[2] * 10 + digits[3]) as i32
    } else {
        0
    };
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Parse the ISO 8601 that OOXML and ODF write.
///
/// OOXML stamps `dcterms:created` as W3CDTF, normally with a `Z`. ODF
/// commonly omits the zone entirely (`2019-03-04T10:22:00`), and some writers
/// emit sub-second precision. Absent zone is read as UTC — not as the
/// server's local time, which would make the same bytes yield different
/// dates on two instances, and these rows are shared by content hash.
fn parse_iso_8601(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, format) {
            return Some(Utc.from_utc_datetime(&naive));
        }
        if format == "%Y-%m-%d"
            && let Ok(date) = chrono::NaiveDate::parse_from_str(s, format)
        {
            return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ooxml_and_odf_timestamps_both_parse() {
        // OOXML: W3CDTF with a zone.
        assert_eq!(
            parse_iso_8601("2019-03-04T10:22:00Z").map(|d| d.to_rfc3339()),
            Some("2019-03-04T10:22:00+00:00".to_owned())
        );
        // An offset is honoured, not stripped.
        assert_eq!(
            parse_iso_8601("2019-03-04T11:22:00+01:00").map(|d| d.to_rfc3339()),
            Some("2019-03-04T10:22:00+00:00".to_owned())
        );
        // ODF: no zone at all — read as UTC, never as server-local, because
        // these rows are keyed by CONTENT and shared between instances.
        assert_eq!(
            parse_iso_8601("2019-03-04T10:22:00").map(|d| d.to_rfc3339()),
            Some("2019-03-04T10:22:00+00:00".to_owned())
        );
        // Sub-second precision, which some writers emit.
        assert_eq!(
            parse_iso_8601("2019-03-04T10:22:00.123").map(|d| d.to_rfc3339()),
            Some("2019-03-04T10:22:00.123+00:00".to_owned())
        );
        assert_eq!(parse_iso_8601(""), None);
        assert_eq!(parse_iso_8601("last Tuesday"), None);
    }

    #[test]
    fn pdf_dates_parse_with_and_without_their_optional_parts() {
        let at = |s: &str| parse_pdf_date(s.as_bytes()).map(|d| d.to_rfc3339());

        assert_eq!(
            at("D:20190304102200Z"),
            Some("2019-03-04T10:22:00+00:00".to_owned())
        );
        // ⚠ The offset is what the local time was OFFSET BY, so it is
        // SUBTRACTED to reach UTC. Adding it instead is the classic sign
        // error and would put every European document an hour early.
        assert_eq!(
            at("D:20190304112200+01'00'"),
            Some("2019-03-04T10:22:00+00:00".to_owned())
        );
        assert_eq!(
            at("D:20190304092200-01'00'"),
            Some("2019-03-04T10:22:00+00:00".to_owned())
        );
        // Truncated at any field, which the spec allows.
        assert_eq!(at("D:2019"), Some("2019-01-01T00:00:00+00:00".to_owned()));
        assert_eq!(at("D:201903"), Some("2019-03-01T00:00:00+00:00".to_owned()));
        // No `D:` prefix — seen in the wild.
        assert_eq!(
            at("20190304102200"),
            Some("2019-03-04T10:22:00+00:00".to_owned())
        );
        // Junk is None, never a partial guess.
        assert_eq!(at(""), None);
        assert_eq!(at("D:"), None);
        assert_eq!(at("D:19"), None);
        assert_eq!(at("D:20191399999999"), None, "an impossible date parsed");
    }

    /// The scanner takes the LAST match, because a PDF appends revisions and
    /// the latest trailer is the live one.
    #[test]
    fn the_last_creation_date_in_the_file_wins() {
        let bytes = b"/CreationDate (D:20100101000000Z) ... /CreationDate (D:20200202000000Z)";
        assert_eq!(
            scan_pdf_creation_date(bytes).map(|d| d.to_rfc3339()),
            Some("2020-02-02T00:00:00+00:00".to_owned())
        );
    }

    #[test]
    fn a_scan_with_nothing_to_find_is_none_not_a_panic() {
        assert_eq!(scan_pdf_creation_date(b""), None);
        assert_eq!(scan_pdf_creation_date(b"%PDF-1.7 no info dict here"), None);
        // Key present, value malformed or absent.
        assert_eq!(scan_pdf_creation_date(b"/CreationDate"), None);
        assert_eq!(scan_pdf_creation_date(b"/CreationDate ()"), None);
        assert_eq!(scan_pdf_creation_date(b"/CreationDate (nonsense)"), None);
        // An unterminated string must not walk off the end.
        assert_eq!(scan_pdf_creation_date(b"/CreationDate (D:2019"), None);
        // A reference instead of a literal — the blind spot, answered
        // honestly rather than guessed.
        assert_eq!(scan_pdf_creation_date(b"/CreationDate 12 0 R"), None);
    }

    #[test]
    fn a_pdf_string_stops_at_an_unescaped_paren_only() {
        assert_eq!(pdf_string_at(b"(abc)").as_deref(), Some(&b"abc"[..]));
        assert_eq!(pdf_string_at(b"  (abc)").as_deref(), Some(&b"abc"[..]));
        // The escape is honoured, so a `)` inside the value does not cut it.
        assert_eq!(pdf_string_at(br"(a\)b)").as_deref(), Some(&b"a)b"[..]));
        assert_eq!(pdf_string_at(b"12 0 R"), None);
        assert_eq!(pdf_string_at(b""), None);
        // Unterminated: bounded, and no value.
        assert_eq!(pdf_string_at(b"(aaaa"), None);
    }

    #[test]
    fn a_namespace_prefix_is_not_part_of_the_name() {
        assert_eq!(local_suffix(b"dcterms:created"), b"created");
        assert_eq!(local_suffix(b"meta:creation-date"), b"creation-date");
        assert_eq!(local_suffix(b"created"), b"created");
    }

    /// Build a real zip at `path` with one entry.
    fn write_zip(path: &Path, entry: &str, body: &str) {
        use std::io::Write;
        let file = File::create(path).expect("create fixture");
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(entry, zip::write::SimpleFileOptions::default())
            .expect("start entry");
        zip.write_all(body.as_bytes()).expect("write entry");
        zip.finish().expect("finish zip");
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oxicloud-dde-{}-{name}", std::process::id()))
    }

    /// The real OOXML path, through a real archive — the unit tests above
    /// only cover the timestamp grammar, which would still pass if the
    /// container reading were broken.
    #[test]
    fn an_ooxml_core_props_date_is_read_through_the_container() {
        let path = temp_path("core.docx");
        write_zip(
            &path,
            "docProps/core.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<cp:coreProperties xmlns:cp="x" xmlns:dcterms="http://purl.org/dc/terms/">
  <dc:title xmlns:dc="y">Quarterly report</dc:title>
  <dcterms:created xsi:type="dcterms:W3CDTF" xmlns:xsi="z">2019-03-04T10:22:00Z</dcterms:created>
  <dcterms:modified xsi:type="dcterms:W3CDTF" xmlns:xsi="z">2024-11-02T08:00:00Z</dcterms:modified>
</cp:coreProperties>"#,
        );

        let mime = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        assert_eq!(
            extract(&path, mime).map(|d| d.to_rfc3339()),
            Some("2019-03-04T10:22:00+00:00".to_owned()),
            "the claimed creation date was not read from docProps/core.xml",
        );

        // ⚠ `created`, not `modified`. They sit side by side in the same
        // entry with near-identical names, and taking the wrong one would
        // produce a plausible date that tracks edits — the exact drift
        // `nc:creation_time` refuses to report.
        assert_ne!(
            extract(&path, mime).map(|d| d.to_rfc3339()),
            Some("2024-11-02T08:00:00+00:00".to_owned()),
            "the extractor read dcterms:modified as the creation date",
        );

        let _ = std::fs::remove_file(&path);
    }

    /// ODF puts it elsewhere, under a different prefix, usually with no
    /// timezone — so the same parser has to serve a different shape.
    #[test]
    fn an_odf_meta_date_is_read_through_the_container() {
        let path = temp_path("meta.odt");
        write_zip(
            &path,
            "meta.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-meta xmlns:office="x" xmlns:meta="y">
  <office:meta><meta:creation-date>2019-03-04T10:22:00</meta:creation-date></office:meta>
</office:document-meta>"#,
        );

        assert_eq!(
            extract(&path, "application/vnd.oasis.opendocument.text").map(|d| d.to_rfc3339()),
            Some("2019-03-04T10:22:00+00:00".to_owned()),
        );

        let _ = std::fs::remove_file(&path);
    }

    /// A container that is not a container, and one missing the entry.
    /// Neither may panic or stall — a `.docx` is attacker-supplied.
    #[test]
    fn a_broken_container_claims_no_date() {
        let mime = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

        let not_a_zip = temp_path("broken.docx");
        std::fs::write(&not_a_zip, b"PK\x03\x04 truncated and then garbage").expect("write");
        assert_eq!(extract(&not_a_zip, mime), None);
        let _ = std::fs::remove_file(&not_a_zip);

        let wrong_entry = temp_path("noprops.docx");
        write_zip(&wrong_entry, "word/document.xml", "<w:document/>");
        assert_eq!(extract(&wrong_entry, mime), None);
        let _ = std::fs::remove_file(&wrong_entry);

        let no_date = temp_path("empty-props.docx");
        write_zip(
            &no_date,
            "docProps/core.xml",
            r#"<cp:coreProperties xmlns:cp="x"><dc:title xmlns:dc="y">No date</dc:title></cp:coreProperties>"#,
        );
        assert_eq!(extract(&no_date, mime), None);
        let _ = std::fs::remove_file(&no_date);
    }

    /// The whole PDF path, including the tail-window seek that the
    /// byte-level scanner tests bypass.
    #[test]
    fn a_pdf_creation_date_is_found_in_the_trailer_window() {
        let path = temp_path("doc.pdf");
        let mut body = b"%PDF-1.4\n".to_vec();
        // Pad past the head window so only the TAIL scan can find it, which
        // is where a real trailer lives.
        body.extend(std::iter::repeat_n(b' ', PDF_SCAN_WINDOW + 1024));
        body.extend_from_slice(b"trailer\n<< /Info 1 0 R >>\n");
        body.extend_from_slice(b"1 0 obj << /CreationDate (D:20190304102200+01'00') >> endobj\n");
        std::fs::write(&path, &body).expect("write pdf fixture");

        assert_eq!(
            extract(&path, "application/pdf").map(|d| d.to_rfc3339()),
            Some("2019-03-04T09:22:00+00:00".to_owned()),
            "the trailer-window scan did not find /CreationDate",
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Unsupported content must not even reach a parser — the gate is the
    /// format registry, and `text/plain` is the case that proves the two
    /// gates differ (extractable text, no extractable date).
    #[test]
    fn unsupported_content_never_reaches_a_parser() {
        let missing = Path::new("/nonexistent/oxicloud-test-file");
        assert_eq!(extract(missing, "text/plain"), None);
        assert_eq!(extract(missing, "image/jpeg"), None);
        assert_eq!(extract(missing, ""), None);
        // Supported, but the file is absent: still None, never a panic.
        assert_eq!(extract(missing, "application/pdf"), None);
    }
}
