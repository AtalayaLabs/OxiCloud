//! The wire shape of `GET /api/files/{id}/metadata`.
//!
//! Facts extracted FROM a file's content, as opposed to facts about the file
//! row. The backing table is `storage.content_metadata`, keyed by content
//! hash: N copies of a photo share one row, and so do N copies of a
//! spreadsheet.
//!
//! This existed only as `body = Object` in the OpenAPI document and as a
//! hand-written interface in the SPA, so the generated client knew nothing
//! about it and the two could drift silently in either direction.

use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;

/// The body's `message` when there is nothing to report.
///
/// Deliberately not "No EXIF metadata available", which is what it said
/// while EXIF was the only writer. The endpoint now answers for documents
/// too, where the facts come from `docProps/core.xml`, ODF `meta.xml` or a
/// PDF trailer and are not EXIF in any sense.
pub const NO_CONTENT_METADATA: &str = "No extracted metadata available";

/// Content-derived metadata for one file.
///
/// Every field is optional because extraction is best-effort and
/// format-dependent: a photo fills the camera and geo columns, a `.docx`
/// fills `created_at` alone, and content nothing can be read from fills
/// none of them.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FileContentMetadataDto {
    /// The file the caller asked about. Echoed back because the row itself
    /// is keyed by CONTENT, so it carries no file identity of its own.
    pub file_id: String,

    /// **The creation date the content claims about itself.**
    ///
    /// EXIF `DateTimeOriginal` for a photo, the container creation time for
    /// a video, `dcterms:created` for OOXML, `meta:creation-date` for ODF,
    /// `/CreationDate` for a PDF.
    ///
    /// Named `created_at` to match its column, and **not** `captured_at`,
    /// which is what this field was called while the only writer was EXIF.
    /// A `.docx` is not *captured*; the word was photo vocabulary for a
    /// value that now covers every format the extractor reads. Note this is
    /// a different thing from `FileDto.created_at`, which is when this
    /// instance received the bytes — the vocabulary is "Added" for arrival
    /// and "Created" for the claim, per `docs/architecture/dates.md`.
    ///
    /// `null` means the content claims nothing, which is a real answer and
    /// never a guess: see
    /// `docs/architecture/dates.md § When creation date is unknown`.
    pub created_at: Option<DateTime<Utc>>,

    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    /// EXIF orientation (1-8), for viewers that rotate on display.
    pub orientation: Option<i16>,
    pub width: Option<i32>,
    pub height: Option<i32>,

    /// Present only when there is nothing to report, so a client can tell
    /// "we looked and found none" from a field it forgot to read.
    ///
    /// Kept in the SAME shape as a populated response rather than returned
    /// as a different body: the endpoint answers 200 either way, and two
    /// shapes behind one status is what made `body = Object` the only
    /// honest annotation before this type existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl FileContentMetadataDto {
    /// The "nothing extracted" answer.
    ///
    /// Reached three ways that a caller cannot and need not distinguish:
    /// the content carries no readable metadata, no extractor has run yet,
    /// or the file lives on an external mount and is never indexed.
    pub fn empty(file_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            file_id: file_id.into(),
            created_at: None,
            latitude: None,
            longitude: None,
            camera_make: None,
            camera_model: None,
            orientation: None,
            width: None,
            height: None,
            message: Some(message.into()),
        }
    }
}
