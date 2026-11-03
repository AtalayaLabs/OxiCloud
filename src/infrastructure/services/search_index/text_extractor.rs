//! Pure-Rust text extraction for the content index.
//!
//! Supported: plain text/code/markup, PDF (text layer), Office OOXML
//! (docx/xlsx/pptx) and OpenDocument (odt/ods/odp). Images, media and
//! archives are reported as [`ExtractedText::Unsupported`] WITHOUT reading
//! the blob (the worker checks [`supports`] first).
//!
//! Everything here is CPU-bound and synchronous — the worker runs it inside
//! `spawn_blocking`, one extraction at a time, so user-facing latency is
//! never affected. Output is whitespace-normalized and hard-capped at the
//! caller-provided byte budget.

use std::io::{BufReader, Cursor};
use std::panic::{AssertUnwindSafe, catch_unwind};

use quick_xml::events::Event;

/// Outcome of one extraction attempt.
#[derive(Debug)]
pub enum ExtractedText {
    /// Usable text (normalized, capped).
    Text(String),
    /// Extractor ran fine but produced no text (e.g. empty document,
    /// scanned PDF without a text layer, binary masquerading as text).
    Empty,
    /// No extractor handles this name/MIME combination.
    Unsupported,
    /// Extractor failed or panicked — terminal for this blob (recorded so it
    /// is never retried until the extractor version bumps).
    Failed(String),
}

/// The format registry lives in `document_format` — hoisted out of here so
/// the creation-date extractor asks the same question and gets the same
/// answer. A second copy of this table would be the duplication AGENTS.md
/// forbids, and it would drift silently: a format added for text and missed
/// for dates looks like "that document has no creation date".
use crate::infrastructure::services::document_format::{self, DocumentFormat as Kind};

/// Whether [`extract`] has an extractor for this file — the worker calls this
/// BEFORE reading the blob, so unsupported content (photos, video, archives)
/// costs zero I/O.
pub fn supports(name: &str, mime: &str) -> bool {
    document_format::supports_text(name, mime)
}

/// Extract plain text from `bytes`, capped at `max_text_bytes` of UTF-8.
pub fn extract(name: &str, mime: &str, bytes: &[u8], max_text_bytes: usize) -> ExtractedText {
    let Some(kind) = document_format::classify(name, mime) else {
        return ExtractedText::Unsupported;
    };

    let result = match kind {
        Kind::Plain => extract_plain(bytes, max_text_bytes),
        Kind::Pdf => extract_pdf(bytes, max_text_bytes),
        Kind::Docx => {
            extract_zipped_xml(bytes, ZipSource::Fixed("word/document.xml"), max_text_bytes)
        }
        Kind::Xlsx => extract_zipped_xml(
            bytes,
            ZipSource::Fixed("xl/sharedStrings.xml"),
            max_text_bytes,
        ),
        Kind::Pptx => extract_zipped_xml(bytes, ZipSource::Slides, max_text_bytes),
        Kind::Odf => extract_zipped_xml(bytes, ZipSource::Fixed("content.xml"), max_text_bytes),
    };

    match result {
        Ok(text) if text.is_empty() => ExtractedText::Empty,
        Ok(text) => ExtractedText::Text(text),
        Err(reason) => ExtractedText::Failed(reason),
    }
}

/// Collapse whitespace runs and cap at `max_bytes` (on a char boundary).
/// Normalization keeps the index lean and makes stored previews readable.
fn normalize_and_cap(text: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_bytes));
    for word in text.split_whitespace() {
        if out.len() + word.len() + 1 > max_bytes {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn extract_plain(bytes: &[u8], max_text_bytes: usize) -> Result<String, String> {
    // NUL byte in the head = binary masquerading under a textual name/MIME.
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Ok(String::new());
    }
    // Decode at most ~2x the budget — normalization only shrinks text, so
    // anything beyond that can never reach the output.
    let slice_end = bytes.len().min(max_text_bytes.saturating_mul(2));
    let text = String::from_utf8_lossy(&bytes[..slice_end]);
    Ok(normalize_and_cap(&text, max_text_bytes))
}

fn extract_pdf(bytes: &[u8], max_text_bytes: usize) -> Result<String, String> {
    // pdf-extract is known to panic on malformed documents; a poisoned blob
    // must mark itself 'failed' instead of taking the worker down.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(bytes)
    }));
    match outcome {
        Ok(Ok(text)) => Ok(normalize_and_cap(&text, max_text_bytes)),
        Ok(Err(e)) => Err(format!("pdf: {e}")),
        Err(_) => Err("pdf: extractor panicked".to_owned()),
    }
}

enum ZipSource {
    /// One well-known entry (docx body, xlsx shared strings, ODF content).
    Fixed(&'static str),
    /// Every `ppt/slides/slideN.xml` entry.
    Slides,
}

/// Panic-guarded wrapper, mirroring [`extract_pdf`].
///
/// `extract_pdf` has had a `catch_unwind` since a malformed PDF was found to
/// panic `pdf_extract`; the zipped-XML path had none, even though it parses
/// exactly as untrusted a byte stream through two more third-party parsers
/// (`zip` for the container, the XML reader for each entry). A broken `.xlsx`
/// or `.docx` must mark itself failed, not take the index worker down with it.
///
/// Unbounded *memory* is already handled separately: `max_text_bytes` caps the
/// output and `collect_xml_text` stops reading once that cap is reached, so a
/// zip bomb costs bounded work rather than the whole expansion. This guard is
/// specifically about panics.
fn extract_zipped_xml(
    bytes: &[u8],
    source: ZipSource,
    max_text_bytes: usize,
) -> Result<String, String> {
    match catch_unwind(AssertUnwindSafe(|| {
        extract_zipped_xml_inner(bytes, source, max_text_bytes)
    })) {
        Ok(result) => result,
        Err(_) => Err("zip: extractor panicked".to_owned()),
    }
}

fn extract_zipped_xml_inner(
    bytes: &[u8],
    source: ZipSource,
    max_text_bytes: usize,
) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("zip: {e}"))?;

    let entries: Vec<String> = match source {
        ZipSource::Fixed(name) => vec![name.to_owned()],
        ZipSource::Slides => {
            // zip 9's `file_names()` returns `Result<Cow<'_, str>,
            // ZipError>` items (was `&str` in zip 8). Drop entries whose
            // name couldn't be decoded — same effect as the pre-upgrade
            // path, which simply never saw those entries. See
            // RUSTSEC-2026-0367/0368/0369 for the upgrade rationale.
            let mut slides: Vec<String> = archive
                .file_names()
                .filter_map(|r| r.ok())
                .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
                .map(|n| n.into_owned())
                .collect();
            slides.sort();
            slides
        }
    };

    let mut text = String::new();
    for entry in entries {
        let Ok(file) = archive.by_name(&entry) else {
            // Tolerated: e.g. an xlsx with no shared strings table.
            continue;
        };
        collect_xml_text(BufReader::new(file), &mut text, max_text_bytes)
            .map_err(|e| format!("{entry}: {e}"))?;
        if text.len() >= max_text_bytes {
            break;
        }
    }
    Ok(normalize_and_cap(&text, max_text_bytes))
}

/// Append every XML text node to `out` (capped). Text RUNS are concatenated
/// without separators — OOXML splits words across `<w:t>` runs arbitrarily —
/// while paragraph/cell boundaries insert whitespace so distinct words never
/// fuse together.
fn collect_xml_text<R: std::io::BufRead>(
    reader: R,
    out: &mut String,
    max_bytes: usize,
) -> Result<(), String> {
    let mut xml = quick_xml::Reader::from_reader(reader);
    let mut buf = Vec::new();
    loop {
        if out.len() >= max_bytes {
            return Ok(());
        }
        match xml.read_event_into(&mut buf) {
            Ok(Event::Text(t)) => {
                // quick-xml 0.41+ makes XmlVersion explicit on xml_content()
                // so callers pick 1.0 vs 1.1 entity-normalization rules. Text
                // extraction is version-agnostic — 1.0 is the sane default.
                if let Ok(decoded) = t.xml_content(quick_xml::XmlVersion::Implicit1_0) {
                    out.push_str(&decoded);
                }
            }
            Ok(Event::GeneralRef(r)) => {
                // quick-xml emits entity references as separate events.
                // Character refs (&#65;) and the predefined five resolve to
                // their literal character; unknown custom entities are
                // dropped (no DTD resolution).
                if let Ok(Some(ch)) = r.resolve_char_ref() {
                    out.push(ch);
                } else if let Ok(name) = r.decode() {
                    match name.as_ref() {
                        "amp" => out.push('&'),
                        "lt" => out.push('<'),
                        "gt" => out.push('>'),
                        "apos" => out.push('\''),
                        "quot" => out.push('"'),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                // Paragraphs (docx w:p, pptx a:p, ODF text:p/text:h), table
                // rows and xlsx shared-string items all separate words.
                let local = e.local_name();
                if matches!(local.as_ref(), b"p" | b"h" | b"si" | b"row" | b"br") {
                    out.push('\n');
                }
            }
            Ok(Event::Empty(e)) => {
                // Self-closing breaks/tabs inside a paragraph (<w:br/>, <w:tab/>).
                let local = e.local_name();
                if matches!(local.as_ref(), b"br" | b"tab") {
                    out.push(' ');
                }
            }
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(format!("xml: {e}")),
        }
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn build_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn text_of(outcome: ExtractedText) -> String {
        match outcome {
            ExtractedText::Text(t) => t,
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn plain_text_is_normalized_and_capped() {
        let out = extract(
            "notas.txt",
            "text/plain",
            b"receta  de\n\npatatas   bravas",
            1024,
        );
        assert_eq!(text_of(out), "receta de patatas bravas");

        let big = "palabra ".repeat(1000);
        let out = text_of(extract("big.txt", "text/plain", big.as_bytes(), 64));
        assert!(out.len() <= 64, "cap exceeded: {}", out.len());
        assert!(out.ends_with("palabra"), "must cut on word boundary");
    }

    #[test]
    fn binary_masquerading_as_text_yields_empty() {
        let mut bytes = b"PK\x03\x04".to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        assert!(matches!(
            extract("raro.txt", "text/plain", &bytes, 1024),
            ExtractedText::Empty
        ));
    }

    #[test]
    fn unsupported_types_are_reported_without_reading() {
        assert!(!supports("foto.jpg", "image/jpeg"));
        assert!(matches!(
            extract("foto.jpg", "image/jpeg", &[0xFF, 0xD8], 1024),
            ExtractedText::Unsupported
        ));
        assert!(supports("recetas.pdf", "application/pdf"));
        assert!(
            supports("notas", "text/plain"),
            "MIME wins without extension"
        );
    }

    #[test]
    fn docx_runs_concatenate_and_paragraphs_separate() {
        let body = r#"<?xml version="1.0"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:t>pata</w:t></w:r><w:r><w:t>tas</w:t></w:r></w:p>
                <w:p><w:r><w:t>bravas &amp; ali oli</w:t></w:r></w:p>
              </w:body>
            </w:document>"#;
        let bytes = build_zip(&[("word/document.xml", body)]);
        let out = text_of(extract(
            "receta.docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            &bytes,
            4096,
        ));
        assert_eq!(out, "patatas bravas & ali oli");
    }

    #[test]
    fn xlsx_shared_strings_extract() {
        let shared = r#"<?xml version="1.0"?>
            <sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="2">
              <si><t>patatas</t></si>
              <si><t>900 kg</t></si>
            </sst>"#;
        let bytes = build_zip(&[("xl/sharedStrings.xml", shared)]);
        let out = text_of(extract(
            "stock.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            &bytes,
            4096,
        ));
        assert_eq!(out, "patatas 900 kg");
    }

    #[test]
    fn odt_content_extracts_by_extension_fallback() {
        let content = r#"<?xml version="1.0"?>
            <office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                                     xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
              <office:body><office:text>
                <text:p>tortilla de patatas</text:p>
              </office:text></office:body>
            </office:document-content>"#;
        let bytes = build_zip(&[("content.xml", content)]);
        // Generic MIME — classification must fall back to the .odt extension.
        let out = text_of(extract(
            "receta.odt",
            "application/octet-stream",
            &bytes,
            4096,
        ));
        assert_eq!(out, "tortilla de patatas");
    }

    #[test]
    fn corrupt_pdf_fails_terminally_instead_of_panicking() {
        assert!(matches!(
            extract("roto.pdf", "application/pdf", b"definitely not a pdf", 4096),
            ExtractedText::Failed(_)
        ));
    }

    /// The zipped-XML formats need the same guarantee the PDF path has had.
    /// Each goes through two more third-party parsers on untrusted bytes —
    /// `zip` for the container, the XML reader per entry — and until
    /// `extract_zipped_xml` gained its `catch_unwind` a panic in either would
    /// have taken the content-index worker down rather than marking one blob
    /// failed.
    ///
    /// Garbage bytes under each of the four mime types: the container itself
    /// is invalid, so this exercises the zip-open failure path. A panic
    /// anywhere in it fails the test by aborting the test binary, which is
    /// exactly the signal wanted.
    #[test]
    fn corrupt_office_documents_fail_terminally_instead_of_panicking() {
        for (name, mime) in [
            (
                "broken.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "broken.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "broken.pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("broken.odt", "application/vnd.oasis.opendocument.text"),
        ] {
            assert!(
                matches!(
                    extract(name, mime, b"definitely not a zip archive", 4096),
                    ExtractedText::Failed(_)
                ),
                "{name} ({mime}) should fail terminally, not panic or succeed"
            );
        }
    }

    /// A ZIP that opens but has none of the expected entries must also be a
    /// clean failure — this is the shape a truncated or re-zipped office file
    /// takes, and it reaches further into the extractor than pure garbage.
    #[test]
    fn valid_zip_without_expected_entries_fails_cleanly() {
        // Smallest possible valid zip: an empty central directory.
        let empty_zip: &[u8] = &[
            0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(matches!(
            extract(
                "empty.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                empty_zip,
                4096
            ),
            ExtractedText::Failed(_) | ExtractedText::Empty
        ));
    }
}
