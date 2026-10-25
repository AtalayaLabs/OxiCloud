//! Which document format a file is, for the two things that need to know.
//!
//! Hoisted out of `search_index::text_extractor`, where `Kind` and `classify`
//! were private and the module sat under `search_index/` — placement that
//! correctly said "this is a search concern". A creation date is not, so a
//! second consumer could not reach the classifier and would have had to
//! reimplement it. One format registry, two consumers, neither pretending to
//! be the other:
//!
//! | | text | creation date |
//! |---|---|---|
//! | reads | the whole blob, the document body | a PDF trailer dict, or one small zip entry |
//! | consumer | the Tantivy index | `media_sort_date`, `nc:creation_time` |
//! | timing | background queue (`content_index_worker`) | at or near upload |
//!
//! The module was deliberately NOT renamed `metadata_extractor` and kept
//! where it was: it extracts *text*, and growing date extraction into it would
//! conflate two concerns with different cost profiles and different callers.
//! See `docs/plan/normalize-file-dates.md` §4.

/// A format we can read something out of.
///
/// The OOXML trio stay separate variants although they share a container:
/// their *text* lives in different entries (`word/document.xml` versus a
/// workbook's shared strings versus slide parts), even though their *date*
/// lives in the same `docProps/core.xml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentFormat {
    Plain,
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    Odf,
}

impl DocumentFormat {
    /// Whether this format can carry a creation date in its own metadata.
    ///
    /// `Plain` cannot, and that is the whole reason this predicate exists
    /// rather than reusing `supports`: a `.txt` is extractable *text* and
    /// un-extractable *date*, so the two gates are not the same set. Reading
    /// a 200 MB log to discover it has no `dcterms:created` is pure waste.
    pub fn carries_creation_date(self) -> bool {
        !matches!(self, Self::Plain)
    }

    /// Where the claimed creation date lives, for the formats that have one.
    ///
    /// Returned rather than branched at the callsite so the extractor reads
    /// as one dispatch, and so this table is the single place a new format
    /// gets added.
    pub fn creation_date_source(self) -> Option<CreationDateSource> {
        match self {
            Self::Plain => None,
            Self::Pdf => Some(CreationDateSource::PdfTrailer),
            // All three OOXML types put it in the same entry — the trio is
            // split by where their TEXT lives, not their date.
            Self::Docx | Self::Xlsx | Self::Pptx => Some(CreationDateSource::OoxmlCoreProps),
            Self::Odf => Some(CreationDateSource::OdfMeta),
        }
    }
}

/// Where in a document the claimed creation date is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreationDateSource {
    /// `/CreationDate` in the trailer's `/Info` dictionary, as a `D:` string.
    PdfTrailer,
    /// `docProps/core.xml` → `dcterms:created`, ISO 8601.
    OoxmlCoreProps,
    /// `meta.xml` → `meta:creation-date`, ISO 8601 without a zone.
    OdfMeta,
}

/// MIME types (beyond `text/*`) parsed as plain text.
const TEXTUAL_MIMES: &[&str] = &[
    "application/json",
    "application/ld+json",
    "application/xml",
    "application/javascript",
    "application/x-javascript",
    "application/x-yaml",
    "application/yaml",
    "application/toml",
    "application/x-sh",
    "application/x-shellscript",
    "application/sql",
    "image/svg+xml",
];

/// Extensions parsed as plain text when the MIME type is generic
/// (`application/octet-stream` uploads are common on WebDAV clients).
const TEXTUAL_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "json", "xml", "yaml", "yml", "toml", "ini", "cfg",
    "conf", "log", "rs", "js", "mjs", "ts", "jsx", "tsx", "css", "scss", "html", "htm", "py", "rb",
    "go", "java", "c", "h", "cpp", "hpp", "cs", "php", "sh", "sql", "tex", "svg",
];

fn extension_of(name: &str) -> Option<String> {
    name.rsplit_once('.').map(|(_, ext)| ext.to_lowercase())
}

/// Resolve a format from the MIME type, falling back to the extension.
///
/// The MIME type first and the extension only as a fallback — the opposite of
/// nothing else in the codebase, and deliberate: by the time a file reaches
/// here its `mime_type` has already been through
/// `mime_detect::refine_content_type`, which sniffed the magic bytes. The
/// extension arm exists for the generic-claim case that step 4 leaves behind.
pub fn classify(name: &str, mime: &str) -> Option<DocumentFormat> {
    let mime = mime
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();

    if mime.starts_with("text/") || TEXTUAL_MIMES.contains(&mime.as_str()) {
        return Some(DocumentFormat::Plain);
    }
    match mime.as_str() {
        "application/pdf" => return Some(DocumentFormat::Pdf),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            return Some(DocumentFormat::Docx);
        }
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
            return Some(DocumentFormat::Xlsx);
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            return Some(DocumentFormat::Pptx);
        }
        "application/vnd.oasis.opendocument.text"
        | "application/vnd.oasis.opendocument.spreadsheet"
        | "application/vnd.oasis.opendocument.presentation" => return Some(DocumentFormat::Odf),
        _ => {}
    }

    // Generic MIME — fall back to the extension.
    match extension_of(name)?.as_str() {
        ext if TEXTUAL_EXTENSIONS.contains(&ext) => Some(DocumentFormat::Plain),
        "pdf" => Some(DocumentFormat::Pdf),
        "docx" => Some(DocumentFormat::Docx),
        "xlsx" => Some(DocumentFormat::Xlsx),
        "pptx" => Some(DocumentFormat::Pptx),
        "odt" | "ods" | "odp" => Some(DocumentFormat::Odf),
        _ => None,
    }
}

/// Whether any text extractor handles this file — the search worker calls
/// this BEFORE reading the blob, so unsupported content (photos, video,
/// archives) costs zero I/O.
pub fn supports_text(name: &str, mime: &str) -> bool {
    classify(name, mime).is_some()
}

/// Whether this file could carry a claimed creation date — the same
/// zero-I/O gate, for the other consumer.
pub fn supports_creation_date(name: &str, mime: &str) -> bool {
    classify(name, mime).is_some_and(DocumentFormat::carries_creation_date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_beats_extension_and_the_extension_is_the_fallback() {
        // A real claim resolves without the name mattering.
        assert_eq!(classify("", "application/pdf"), Some(DocumentFormat::Pdf));
        // A generic claim falls through to the name — the shape WebDAV
        // clients produce when they send octet-stream for everything.
        assert_eq!(
            classify("report.docx", "application/octet-stream"),
            Some(DocumentFormat::Docx)
        );
        assert_eq!(classify("notes.txt", ""), Some(DocumentFormat::Plain));
        assert_eq!(classify("photo.jpg", "image/jpeg"), None);
    }

    /// ⚠ The two gates are DIFFERENT sets, and conflating them is the
    /// mistake this pins: a `.txt` is extractable text with no extractable
    /// date. Gating the date extractor on `supports_text` would fetch every
    /// log file in the corpus to find nothing.
    #[test]
    fn text_and_date_are_not_the_same_gate() {
        assert!(supports_text("notes.txt", "text/plain"));
        assert!(!supports_creation_date("notes.txt", "text/plain"));

        for (name, mime) in [
            ("a.pdf", "application/pdf"),
            (
                "a.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            ("a.odt", "application/vnd.oasis.opendocument.text"),
        ] {
            assert!(supports_text(name, mime), "{name} lost text support");
            assert!(supports_creation_date(name, mime), "{name} lost its date");
        }

        // Neither gate opens for content with no document shape at all.
        assert!(!supports_text("photo.jpg", "image/jpeg"));
        assert!(!supports_creation_date("photo.jpg", "image/jpeg"));
    }

    /// The OOXML trio share one date location while keeping separate
    /// variants, because their TEXT lives in different entries. A future
    /// edit that merges them would silently break text extraction.
    #[test]
    fn every_dated_format_names_where_its_date_lives() {
        assert_eq!(
            DocumentFormat::Docx.creation_date_source(),
            Some(CreationDateSource::OoxmlCoreProps)
        );
        assert_eq!(
            DocumentFormat::Xlsx.creation_date_source(),
            Some(CreationDateSource::OoxmlCoreProps)
        );
        assert_eq!(
            DocumentFormat::Pptx.creation_date_source(),
            Some(CreationDateSource::OoxmlCoreProps)
        );
        assert_eq!(
            DocumentFormat::Pdf.creation_date_source(),
            Some(CreationDateSource::PdfTrailer)
        );
        assert_eq!(
            DocumentFormat::Odf.creation_date_source(),
            Some(CreationDateSource::OdfMeta)
        );
        // And the one that has none says so, rather than defaulting into a
        // parser that would read the whole file for nothing.
        assert_eq!(DocumentFormat::Plain.creation_date_source(), None);
    }
}
