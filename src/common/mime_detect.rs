//! MIME type detection using magic bytes (infer) + extension fallback (mime_guess).
//!
//! Priority order — **content first**:
//! 1. Magic bytes (`infer`) on the first [`MAGIC_BYTES_LEN`] bytes. A fact
//!    about the content, so it outranks anything the uploader asserts.
//! 2. The claimed Content-Type, if specific (not `application/octet-stream`).
//! 3. Extension-based detection (`mime_guess`).
//! 4. The original claimed type, verbatim.
//!
//! Steps 2 and 3 are both guesses by whoever uploaded the file; the claim
//! is ranked higher because the extension is the worse oracle precisely
//! where they disagree (`.ts` → `video/mp2t`).
//!
//! The claim used to come *first*, which meant magic bytes only ran for
//! clients that sent nothing — so a client could name any type it liked
//! and have it stored and later served back. Content-type is a security
//! decision on the download path, not just a display hint, so the
//! untrusted assertion must not outrank the bytes.
//!
//! Performance: < 1µs for the `infer` check (reads only header bytes, no allocation).

use std::path::Path;
use tokio::io::AsyncReadExt;

/// Maximum bytes needed for magic-byte detection. Upload ingestion peeks
/// this many bytes off the stream before forwarding them unchanged.
pub const MAGIC_BYTES_LEN: usize = 8192;

/// Extract the filename component from a `/`-separated path.
pub fn filename_from_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether a claimed Content-Type is too generic to trust — these trigger
/// magic-byte detection on the upload path.
pub fn is_generic_mime(claimed: &str) -> bool {
    claimed.is_empty() || claimed == "application/octet-stream" || claimed == "binary/octet-stream"
}

/// Refine a claimed MIME type using magic bytes and filename extension.
///
/// This is a synchronous function — the caller should already have the first
/// bytes of the content available (upload ingestion peeks them in-flight).
///
/// # Arguments
/// * `buf` — first bytes of the file (at least 8192 for best results)
/// * `filename` — original filename (used for extension fallback)
/// * `claimed` — the Content-Type sent by the client
pub fn refine_content_type(buf: &[u8], filename: &str, claimed: &str) -> String {
    // 1. Magic bytes win. This is a fact about the content; the other two
    //    candidates are assertions by whoever uploaded it. `infer` only
    //    answers on a real signature match, so a `Some` here is
    //    high-confidence and outranks both the claim and the extension.
    if let Some(kind) = infer::get(buf) {
        return kind.mime_type().to_string();
    }

    // 2. A specific claim beats the extension. `infer` carries no
    //    signatures for text-shaped formats — plain text, CSV, SVG, m3u —
    //    so silence is the normal case for a large slice of real uploads,
    //    not a red flag. Between the remaining two guesses the claim is
    //    the better one: the extension is the worse oracle in exactly the
    //    ambiguous cases (`.ts` is TypeScript far more often than it is an
    //    MPEG transport stream, but `mime_guess` says `video/mp2t`).
    if !is_generic_mime(claimed) {
        return claimed.to_string();
    }

    // 3. Extension, when the client told us nothing usable.
    let guess = mime_guess::from_path(filename);
    if let Some(mime) = guess.first() {
        return mime.to_string();
    }

    // 4. Give the claim back verbatim (generic or empty).
    claimed.to_string()
}

/// Detect the `Content-Type` to serve for an already-encoded thumbnail.
///
/// The slow encode path re-encodes to JPEG, but the fast path stores the
/// source image as-is (PNG / GIF / WebP), so the handler must not blindly
/// claim `image/jpeg`. Detects the real format from magic bytes, defaulting
/// to `image/jpeg` (the slow-path output) when detection is inconclusive.
pub fn thumbnail_content_type(data: &[u8]) -> &'static str {
    infer::get(data)
        .map(|kind| kind.mime_type())
        .filter(|mime| mime.starts_with("image/"))
        .unwrap_or("image/jpeg")
}

/// Async helper: reads the first bytes of a file on disk and refines the MIME type.
///
/// Designed for the upload path where the file has been spooled to a temp path.
pub async fn refine_content_type_from_file(
    temp_path: &Path,
    filename: &str,
    claimed: &str,
) -> String {
    // No trust-the-claim fast path: the magic bytes are the point. This
    // used to return early on any specific `claimed`, which meant the
    // sniffer only ever ran for clients that declined to guess — the
    // opposite of what it is for. (It also carried its own inlined copy
    // of the `is_generic_mime` test, so the rule lived in two places and
    // could drift.)
    //
    // Reads only the first bytes needed for magic detection, not the file.
    match tokio::fs::File::open(temp_path).await {
        Ok(mut file) => {
            let mut buf = vec![0u8; MAGIC_BYTES_LEN];
            let n = file.read(&mut buf).await.unwrap_or(0);
            refine_content_type(&buf[..n], filename, claimed)
        }
        Err(e) => {
            tracing::warn!(
                "MIME detection: failed to read {} for magic bytes: {}",
                temp_path.display(),
                e
            );
            // Unreadable temp file — fall through with no magic. Routed
            // back through `refine_content_type` rather than reimplemented
            // so the claim-vs-extension precedence stays defined once.
            refine_content_type(&[], filename, claimed)
        }
    }
}

/// Whether a MIME type identifies content that is already compressed, so
/// running Deflate over it burns CPU for ~0 % size gain.
///
/// Used by the ZIP export paths (`ZipService`, `BatchOperations`) to pick
/// `Compression::Stored` per entry instead of deflating JPEG/MP4/… bytes.
/// The set mirrors the HTTP `CompressionLayer` exclusion list in `main.rs`
/// (keep the two in sync), minus entries that are containers of possibly
/// incompressible data rather than compressed formats themselves
/// (`application/x-tar`, `application/octet-stream`) — those stay on Deflate
/// so unknown-but-compressible content is never stored uncompressed.
pub fn is_precompressed_mime(mime: &str) -> bool {
    // Strip any parameters ("; charset=…") and normalize case.
    let essence = mime.split(';').next().unwrap_or(mime).trim();

    // Compressed families: every common video/audio codec container.
    if essence.starts_with("video/") || essence.starts_with("audio/") {
        return true;
    }
    // Zip-based document bundles (docx/xlsx/pptx, odt/ods/odp, …).
    if essence.starts_with("application/vnd.openxmlformats-officedocument")
        || essence.starts_with("application/vnd.oasis.opendocument")
    {
        return true;
    }

    matches!(
        essence,
        // Raster images with built-in compression (SVG intentionally absent).
        "image/jpeg"
            | "image/png"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "image/heic"
            | "image/heif"
            | "image/jp2"
            // Already-compressed web fonts; ttf/otf left compressible.
            | "font/woff"
            | "font/woff2"
            | "application/font-woff"
            // Archives & compressed containers.
            | "application/zip"
            | "application/gzip"
            | "application/x-gzip"
            | "application/x-7z-compressed"
            | "application/x-rar-compressed"
            | "application/x-bzip2"
            | "application/zstd"
            | "application/x-xz"
            | "application/epub+zip"
            | "application/java-archive"
            | "application/vnd.android.package-archive"
            // PDF: internal streams are usually already deflated.
            | "application/pdf"
    )
}

/// ZIP entry compression for a file of the given MIME type: `Stored` for
/// already-compressed content, `Deflate` otherwise. Shared by every ZIP
/// export path (`ZipService`, `BatchOperations`).
pub fn zip_entry_compression(mime: &str) -> async_zip::Compression {
    if is_precompressed_mime(mime) {
        async_zip::Compression::Stored
    } else {
        async_zip::Compression::Deflate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── refine_content_type precedence ──────────────────────────
    //
    // The order is load-bearing, so each rank gets a test that fails if
    // the ranks are swapped back.

    /// 8-byte PNG signature — enough for `infer` to match.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

    #[test]
    fn magic_bytes_beat_a_specific_but_wrong_claim() {
        // THE case this ordering exists for. A client uploading PNG bytes
        // under `text/html` previously had that stored verbatim and served
        // back on download; now the bytes decide.
        assert_eq!(
            refine_content_type(PNG, "avatar.png", "text/html"),
            "image/png"
        );
        // Lying about the extension as well changes nothing.
        assert_eq!(
            refine_content_type(PNG, "page.html", "text/html"),
            "image/png"
        );
    }

    #[test]
    fn magic_bytes_beat_the_extension() {
        assert_eq!(
            refine_content_type(PNG, "notes.txt", "application/octet-stream"),
            "image/png"
        );
    }

    #[test]
    fn audio_is_identified_from_content_not_from_the_name() {
        // Relevant to any future "is this really audio?" gate: an ID3v2
        // header is recognised whatever the file is called or claimed.
        let mp3 = b"ID3\x04\x00\x00\x00\x00\x00\x00rest-of-file";
        assert_eq!(
            refine_content_type(mp3, "track.bin", "application/octet-stream"),
            "audio/mpeg"
        );
    }

    #[test]
    fn without_magic_a_specific_claim_beats_the_extension() {
        // `infer` has no signature for text-shaped content, so this is the
        // common case, not an edge one. The claim is preferred because the
        // extension is the worse oracle exactly here: `mime_guess` maps
        // `.ts` to `video/mp2t`, which is wrong for most real `.ts` files.
        assert_eq!(
            refine_content_type(b"export const x = 1;\n", "app.ts", "text/typescript"),
            "text/typescript"
        );
    }

    #[test]
    fn without_magic_or_a_claim_the_extension_decides() {
        let got = refine_content_type(
            b"col_a,col_b\n1,2\n",
            "data.csv",
            "application/octet-stream",
        );
        assert!(
            got.starts_with("text/csv"),
            "expected a text/csv guess from the extension, got {got}"
        );
    }

    #[test]
    fn with_nothing_to_go_on_the_claim_is_returned_verbatim() {
        assert_eq!(refine_content_type(&[], "no-extension", ""), "");
        assert_eq!(
            refine_content_type(&[], "no-extension", "application/octet-stream"),
            "application/octet-stream"
        );
    }

    #[test]
    fn empty_buffer_does_not_panic_or_match() {
        // The ingest hands an empty slice whenever the body is empty; the
        // matchers are all length-guarded, but assert it rather than trust it.
        assert_eq!(
            refine_content_type(&[], "x.png", "application/octet-stream"),
            "image/png",
            "no magic available, so the extension should decide"
        );
    }

    // ── is_precompressed_mime ───────────────────────────────────

    #[test]
    fn media_and_archives_are_precompressed() {
        for mime in [
            "image/jpeg",
            "image/webp",
            "video/mp4",
            "video/quicktime",
            "audio/mpeg",
            "application/zip",
            "application/pdf",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "font/woff2",
        ] {
            assert!(is_precompressed_mime(mime), "{mime} should be Stored");
        }
    }

    #[test]
    fn compressible_types_keep_deflate() {
        for mime in [
            "text/plain",
            "text/html",
            "application/json",
            "image/svg+xml",
            "application/x-tar",
            "application/octet-stream",
            "",
        ] {
            assert!(!is_precompressed_mime(mime), "{mime} should stay Deflate");
        }
    }

    #[test]
    fn mime_parameters_are_ignored() {
        assert!(is_precompressed_mime("image/jpeg; charset=binary"));
    }

    // ── refine_content_type (sync) ──────────────────────────────

    #[test]
    fn specific_claimed_type_is_trusted() {
        let result = refine_content_type(b"garbage", "file.txt", "image/png");
        assert_eq!(result, "image/png");
    }

    #[test]
    fn octet_stream_triggers_magic_detection_png() {
        // PNG magic bytes
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let result = refine_content_type(png, "noext", "application/octet-stream");
        assert_eq!(result, "image/png");
    }

    #[test]
    fn octet_stream_triggers_magic_detection_jpeg() {
        let jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF";
        let result = refine_content_type(jpeg, "noext", "application/octet-stream");
        assert_eq!(result, "image/jpeg");
    }

    #[test]
    fn thumbnail_content_type_detects_real_format() {
        // Fast-path thumbnails keep the source format — serve it accurately.
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        assert_eq!(thumbnail_content_type(png), "image/png");

        let jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF";
        assert_eq!(thumbnail_content_type(jpeg), "image/jpeg");

        // Inconclusive bytes default to JPEG (the slow-path encoder output).
        assert_eq!(thumbnail_content_type(b"garbage"), "image/jpeg");
        assert_eq!(thumbnail_content_type(b""), "image/jpeg");
    }

    #[test]
    fn binary_octet_stream_also_triggers_detection() {
        let jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF";
        let result = refine_content_type(jpeg, "noext", "binary/octet-stream");
        assert_eq!(result, "image/jpeg");
    }

    #[test]
    fn extension_fallback_when_no_magic_match() {
        let result = refine_content_type(b"plain text", "style.css", "application/octet-stream");
        assert_eq!(result, "text/css");
    }

    #[test]
    fn falls_back_to_claimed_when_nothing_matches() {
        let result = refine_content_type(b"unknown stuff", "noext", "application/octet-stream");
        assert_eq!(result, "application/octet-stream");
    }

    #[test]
    fn empty_claimed_triggers_detection() {
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let result = refine_content_type(png, "photo.png", "");
        assert_eq!(result, "image/png");
    }

    // ── is_generic_mime ─────────────────────────────────────────

    #[test]
    fn generic_mime_detection() {
        assert!(is_generic_mime(""));
        assert!(is_generic_mime("application/octet-stream"));
        assert!(is_generic_mime("binary/octet-stream"));
        assert!(!is_generic_mime("image/png"));
        assert!(!is_generic_mime("text/plain"));
    }

    // ── filename_from_path ──────────────────────────────────────

    #[test]
    fn extracts_filename_from_deep_path() {
        assert_eq!(filename_from_path("a/b/c/photo.jpg"), "photo.jpg");
    }

    #[test]
    fn returns_input_when_no_slash() {
        assert_eq!(filename_from_path("photo.jpg"), "photo.jpg");
    }

    #[test]
    fn handles_trailing_slash() {
        assert_eq!(filename_from_path("a/b/"), "");
    }
}
