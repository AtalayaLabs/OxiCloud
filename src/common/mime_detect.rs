//! MIME type detection using magic bytes (infer) + extension fallback (mime_guess).
//!
//! Priority order — **content first**:
//! 1. Magic bytes (`infer`) on the first [`MAGIC_BYTES_LEN`] bytes. A fact
//!    about the content, so it outranks anything the uploader asserts —
//!    except when the signature describes only the *envelope*, in which
//!    case the claim or the extension may narrow it. See
//!    [`generic_envelope`] for which detections those are and why.
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

/// Detections that describe the **envelope** rather than the content.
///
/// `infer` matches on leading bytes, and several formats share their first
/// bytes with a more generic one that is simultaneously correct and useless:
///
/// * a `.docx` / `.xlsx` / `.odt` / `.epub` **is** a ZIP. `infer` tries to
///   look inside (`matchers/doc.rs::check_msooml` walks the local file
///   headers), but that only works while the discriminating entry falls
///   inside the bytes we peeked — an oversized `[Content_Types].xml` or a
///   padded archive pushes it out of reach and the answer degrades to
///   `application/zip`.
/// * an `.svg` **is** XML, and `infer` has no SVG matcher at all, so every
///   SVG degrades to `text/xml`.
///
/// Storing the envelope loses real information: `mime_type` is served back
/// as `Content-Type`, gates thumbnailing and metadata extraction, and is what
/// a WebDAV client reads from `getcontenttype`. An SVG stored as `text/xml`
/// stops being an image everywhere at once.
///
/// Deliberately NOT here:
///
/// * `text/x-shellscript` — `matchers/text.rs::is_shellscript` is just
///   `buf[..2] == b"#!"`, so every `#!/usr/bin/env python3` is "a shell
///   script". Wrong, but same top-level category and no effect on the wire
///   or on server-side derivation: picking the highlighter from the
///   extension is the frontend's job.
/// * `text/html` — knowing something is HTML is security-relevant on the
///   download path. That detection should stand, not be talked out of.
/// * plain source code (`.c`, `.rs`, `.cpp`, …) — `infer` has no signature,
///   returns `None`, and the claim/extension path below handles it exactly
///   as it always did.
fn generic_envelope(detected: &str) -> bool {
    matches!(
        detected,
        "application/zip"
            | "application/x-tar"
            | "application/gzip"
            | "application/x-ole-storage"
            | "text/xml"
    )
}

/// Whether `candidate` is a known specialisation of `generic` — a format
/// that genuinely IS that envelope while saying something more useful.
///
/// This is the security-load-bearing half of the carve-out. It is a
/// whitelist of *narrowings*, never an escape hatch: the bytes have already
/// proved the envelope, so the only thing a claim or an extension can do
/// here is name which flavour of that envelope it is. `text/html` does not
/// specialise `application/zip`, so no claim can turn zip bytes into markup.
/// The same whitelist, for the `content_rescan` job.
///
/// Re-exported rather than copied because the two layers MUST agree: an
/// upload and a later rescan that disagreed about the same bytes would
/// flip a file's type back and forth on every run. A second copy of this
/// table is how that happens.
pub fn specialises_for_rescan(candidate: &str, generic: &str) -> bool {
    specialises(candidate, generic)
}

fn specialises(candidate: &str, generic: &str) -> bool {
    // Strip any `; charset=…` parameter before comparing.
    let c = candidate.split(';').next().unwrap_or(candidate).trim();
    match generic {
        // Both of these are *container* envelopes: a huge number of formats
        // are "a zip with a particular layout" or "an OLE2 compound file
        // with particular streams", and neither signature can tell you
        // which.
        //
        // The shape of the accepted set, rather than a list of formats.
        // An earlier version enumerated the two office families and was
        // measured against Apache Tika's 826-file corpus (see
        // `tika_corpus_audit`); it missed roughly 25 real cases —
        // `.msg` → `vnd.ms-outlook`, `.vsd` → `vnd.visio`,
        // `.mpp` → `vnd.ms-project`, `.kmz` → `vnd.google-earth.kmz`,
        // `.xps` → `vnd.ms-xpsdocument`, `.ggb` → `vnd.geogebra.file`,
        // `.thmx` → `vnd.ms-officetheme`, and every password-protected
        // OOXML document, which is an OLE2 wrapper around the real zip.
        // Enumerating that tail is a losing game; `application/vnd.*` is
        // the registry's own name for "a specific vendor format", which is
        // exactly the predicate wanted here.
        //
        // Still a whitelist: `text/html`, `image/svg+xml`,
        // `application/javascript` and `text/plain` are all outside the
        // vendor tree, so none of them can be smuggled past the sniffer.
        "application/zip" | "application/x-ole-storage" => {
            c.starts_with("application/vnd.")
                || c.ends_with("+zip")
                || matches!(
                    c,
                    "application/msword" | "application/java-archive" | "application/x-mspublisher"
                )
        }
        // `image/svg+xml`, and any other `+xml` structured suffix.
        "text/xml" => c.ends_with("+xml"),
        // tar and gzip have no narrowing worth preferring — `.tar.gz` maps
        // straight back to `application/gzip`, and `.svgz` is honestly
        // gzip (the SVG inside is a second decode away). Listed as
        // envelopes anyway so the set reads as "what infer can emit
        // generically", not "what we happened to handle".
        _ => false,
    }
}

/// Extensions where `mime_guess`'s answer is wrong often enough to cause
/// real damage, with the value to use instead.
///
/// This exists because `mime_guess` resolves a shared extension to
/// whichever registration it happens to hold, and for a few extensions
/// that is a format nobody has produced in twenty years. The stored MIME
/// is not cosmetic — it GATES derived features — so a wrong answer does
/// not merely mislabel, it routes the file into the wrong pipeline.
///
/// Applied at the extension step only. A specific client claim still
/// wins (step 2), and magic bytes still win over everything (step 1), so
/// this never overrides better information — it replaces a guess with a
/// better guess.
///
/// ⚠ Entries must be formats `infer` CANNOT detect, or the override is
/// dead code: step 1 would have answered first. Both of the cases
/// considered here are invisible to `infer` (verified), which is exactly
/// why the extension is load-bearing for them.
fn extension_override(ext: &str) -> Option<&'static str> {
    match ext {
        // `.rpm` resolves to `audio/x-pn-realaudio-plugin` — a RealAudio
        // plugin, a format from the 1990s. RPM packages are ubiquitous and
        // RealAudio plugins are extinct, so the mapping is wrong in
        // essentially every real case.
        //
        // And it is not a cosmetic wrong: `audio/*` passes
        // `AudioMetadataService::is_audio_file`, so uploading an RPM ran
        // the ID3 tag parser over a package, and the Type column called it
        // Audio. `infer` has no RPM signature, so nothing downstream could
        // correct it either.
        "rpm" => Some("application/x-rpm"),
        _ => None,
    }
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
    //
    //    …with one carve-out: some signatures describe the ENVELOPE, not
    //    the content. See `generic_envelope`.
    if let Some(kind) = infer::get(buf) {
        let detected = kind.mime_type();
        if generic_envelope(detected) {
            // The claim first, then the extension — but only a candidate
            // that genuinely IS this envelope. A client cannot use this to
            // smuggle an unrelated type past the sniffer: the bytes have
            // already proved the envelope, and the claim may only narrow
            // *within* it.
            if specialises(claimed, detected) {
                return claimed.to_string();
            }
            if let Some(guess) = mime_guess::from_path(filename).first() {
                let guess = guess.to_string();
                if specialises(&guess, detected) {
                    return guess;
                }
            }
        }
        return detected.to_string();
    }

    // 2. A specific claim beats the extension. `infer` carries no
    //    signatures for most text-shaped formats — plain text, CSV, m3u,
    //    and every source language — so silence is the normal case for a
    //    large slice of real uploads,
    //    not a red flag. Between the remaining two guesses the claim is
    //    the better one: the extension is the worse oracle in exactly the
    //    ambiguous cases (`.ts` is TypeScript far more often than it is an
    //    MPEG transport stream, but `mime_guess` says `video/mp2t`).
    if !is_generic_mime(claimed) {
        return claimed.to_string();
    }

    // 3. Extension, when the client told us nothing usable — with the
    //    known-bad mappings corrected first. See `extension_override`.
    if let Some(ext) = filename.rsplit_once('.').map(|(_, e)| e)
        && let Some(fixed) = extension_override(&ext.to_ascii_lowercase())
    {
        return fixed.to_string();
    }
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

    /// ⚠ `.rpm` is a package, not a RealAudio plugin.
    ///
    /// `mime_guess` resolves `rpm` to `audio/x-pn-realaudio-plugin`, a
    /// 1990s format, and `infer` has no RPM signature — so with no client
    /// claim an uploaded RPM was stored as `audio/*` on every path. That
    /// passes `AudioMetadataService::is_audio_file`, so the ID3 tag parser
    /// ran over a package, and the Type column called it Audio.
    ///
    /// Found 2026-10-24 while checking what the classifier actually sees
    /// for package extensions.
    #[test]
    fn a_package_extension_is_not_audio() {
        // No claim, no signature: the override is the only thing standing
        // between an RPM and the audio pipeline.
        assert_eq!(
            refine_content_type(b"\xed\xab\xee\xdb", "nginx-1.0.rpm", ""),
            "application/x-rpm"
        );
        // Also with a useless generic claim, which is what most clients
        // send for an unknown extension.
        assert_eq!(
            refine_content_type(b"zzz", "pkg.rpm", "application/octet-stream"),
            "application/x-rpm"
        );
        // Case-insensitive, like every other extension path here.
        assert_eq!(
            refine_content_type(b"zzz", "PKG.RPM", ""),
            "application/x-rpm"
        );

        // ⚠ The override must NOT outrank better information. A specific
        // claim is the caller telling us something the table cannot know,
        // and magic bytes are a fact about the content.
        assert_eq!(
            refine_content_type(b"zzz", "weird.rpm", "audio/x-pn-realaudio-plugin"),
            "audio/x-pn-realaudio-plugin",
            "a deliberate claim must still win over the override"
        );
        let png: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(
            refine_content_type(png, "mislabelled.rpm", ""),
            "image/png",
            "magic bytes must still win over the override"
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

    // ── envelope detections defer to a narrowing ────────────────
    //
    // Regression locks for the carve-out in `generic_envelope`. Each case
    // below broke, or would have broken, when magic bytes were promoted
    // above the claim.

    /// Minimal ZIP local-file-header signature. Enough for `infer::is_zip`,
    /// and deliberately WITHOUT a `word/` entry — that is the case where
    /// OOXML detection degrades to `application/zip`, which is exactly the
    /// state this carve-out exists for.
    const ZIP: &[u8] = b"PK\x03\x04\x14\x00\x00\x00\x08\x00";

    const SVG: &[u8] = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"></svg>"#;

    #[test]
    fn zip_envelope_defers_to_an_ooxml_claim() {
        // The regression: a browser DOES send the real OOXML type for a
        // .docx. Before magic-first that claim was kept; after, `is_zip`
        // outranked it and the file was stored as a bare archive.
        assert_eq!(
            refine_content_type(
                ZIP,
                "report.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            ),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
    }

    #[test]
    fn zip_envelope_defers_to_the_extension_when_the_client_says_nothing() {
        for (name, expected) in [
            (
                "report.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "sheet.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "deck.pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("notes.odt", "application/vnd.oasis.opendocument.text"),
            ("book.epub", "application/epub+zip"),
        ] {
            assert_eq!(
                refine_content_type(ZIP, name, "application/octet-stream"),
                expected,
                "{name} should narrow the zip envelope"
            );
        }
    }

    #[test]
    fn svg_is_an_image_not_xml() {
        // `infer` has no SVG matcher, so every SVG hits `is_xml` and comes
        // back `text/xml`. Stored that way it stops being an image: no
        // thumbnail, absent from the photos grid, wrong Content-Type.
        assert_eq!(
            refine_content_type(SVG, "logo.svg", "image/svg+xml"),
            "image/svg+xml"
        );
        assert_eq!(
            refine_content_type(SVG, "logo.svg", "application/octet-stream"),
            "image/svg+xml"
        );
    }

    #[test]
    fn a_plain_archive_stays_an_archive() {
        // The inverse, and the whole reason the rule is a whitelist of
        // narrowings rather than "the extension wins for containers".
        assert_eq!(
            refine_content_type(ZIP, "backup.zip", "application/zip"),
            "application/zip"
        );
        assert_eq!(
            refine_content_type(ZIP, "backup.zip", "application/octet-stream"),
            "application/zip"
        );
        assert_eq!(
            refine_content_type(ZIP, "archive", "application/octet-stream"),
            "application/zip",
            "nothing to narrow with"
        );
    }

    /// OLE2 compound-file signature with a zeroed CLSID, which is what
    /// `infer` cannot resolve to a specific type — it answers
    /// `application/x-ole-storage`. Synthesised rather than fixtured:
    /// every case below is reachable from these eight bytes.
    const OLE2: &[u8] = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1\x00\x00\x00\x00\x00\x00\x00\x00";

    #[test]
    fn ole_and_zip_envelopes_narrow_across_the_vendor_tree() {
        // Each of these was a miss in the first version of `specialises`,
        // found by running Apache Tika's corpus through it. Kept as a
        // table so the next envelope format is one line, not a redesign.
        for (bytes, name, expected) in [
            (OLE2, "mail.msg", "application/vnd.ms-outlook"),
            (OLE2, "plan.mpp", "application/vnd.ms-project"),
            (OLE2, "diagram.vsd", "application/vnd.visio"),
            // A password-protected OOXML document is an OLE2 wrapper
            // around the real zip — so the bytes say "compound file" and
            // only the name says "spreadsheet".
            (
                OLE2,
                "locked.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (ZIP, "earth.kmz", "application/vnd.google-earth.kmz"),
            (ZIP, "print.xps", "application/vnd.ms-xpsdocument"),
            (ZIP, "theme.thmx", "application/vnd.ms-officetheme"),
        ] {
            assert_eq!(
                refine_content_type(bytes, name, "application/octet-stream"),
                expected,
                "{name} should narrow its envelope"
            );
        }
    }

    #[test]
    fn an_unresolvable_ole_file_keeps_the_envelope() {
        // The inverse for the OLE arm: nothing to narrow with, so the
        // honest generic stands.
        assert_eq!(
            refine_content_type(OLE2, "mystery.bin", "application/octet-stream"),
            "application/x-ole-storage"
        );
    }

    #[test]
    fn a_real_xml_file_stays_xml() {
        let xml = br#"<?xml version="1.0"?><note><to>you</to></note>"#;
        assert_eq!(
            refine_content_type(xml, "data.xml", "application/octet-stream"),
            "text/xml"
        );
    }

    #[test]
    fn the_narrowing_cannot_smuggle_an_unrelated_type() {
        // ⚠ The security property. `specialises` is a whitelist: a claim may
        // only name a flavour of the envelope the BYTES already proved. If
        // this ever starts returning text/html, the magic-first fix has been
        // undone and a crafted upload can choose how it is served.
        assert_eq!(
            refine_content_type(ZIP, "payload.zip", "text/html"),
            "application/zip"
        );
        assert_eq!(
            refine_content_type(SVG, "page.xml", "text/html"),
            "text/xml"
        );
    }

    #[test]
    fn non_envelope_detections_never_defer_to_the_extension() {
        // ⚠ The other half of that property. PNG is not an envelope, so the
        // filename gets no say — otherwise the carve-out would hand the
        // extension back the authority the magic-first change took from the
        // claim, and `evil.docx` full of PNG bytes would be served as a
        // document.
        let png = b"\x89PNG\r\n\x1a\n";
        assert_eq!(
            refine_content_type(png, "evil.docx", "text/html"),
            "image/png"
        );
        assert_eq!(
            refine_content_type(png, "evil.svg", "image/svg+xml"),
            "image/png"
        );
    }

    #[test]
    fn source_code_is_untouched_by_all_of_this() {
        // `infer` has no signature for C/C++/Rust/Go, so the claim and then
        // the extension decide exactly as before magic-first. Named as a
        // test because "did we break source files?" is the first question
        // anyone asks about a content sniffer.
        assert_eq!(
            refine_content_type(
                b"fn main() { println!(\"hi\"); }\n",
                "main.rs",
                "text/x-rust"
            ),
            "text/x-rust"
        );
        assert_eq!(
            refine_content_type(b"#include <stdio.h>\nint main(){}\n", "a.c", "text/x-c"),
            "text/x-c"
        );
    }

    #[test]
    fn a_shebang_is_left_to_infer_deliberately() {
        // `is_shellscript` is `buf[..2] == b"#!"`, so a Python script reads
        // as a shell script. Not corrected here: same top-level category, no
        // effect on the wire or on server-side derivation, and picking the
        // highlighter from the extension is the frontend's job. Asserted so
        // the choice is visible rather than accidental.
        assert_eq!(
            refine_content_type(b"#!/usr/bin/env python3\nprint(1)\n", "tool.py", ""),
            "text/x-shellscript"
        );
    }

    /// The everyday types, against real committed fixtures.
    ///
    /// Everything else in this module tests synthesised byte sequences,
    /// which is right for edge cases but proves nothing about the files
    /// people actually store. These are real images, a real H.264 video,
    /// a real PDF and a real WAV already in the repo for other suites.
    ///
    /// Video and audio earn their place here beyond tidiness: the stored
    /// type is a GATE on derived content.
    /// `ThumbnailRefreshHook::on_file_created` consults
    /// `is_supported_video(content_type)`, `AudioMetadataService` consults
    /// `is_audio_file(content_type)`, and `MediaMetadataService::handles()`
    /// gates on the image/video prefixes. Get the type wrong and those
    /// features do not fail — they silently never run, which is the
    /// failure mode nobody notices until a user asks why their music has
    /// no titles.
    ///
    /// Reads only `MAGIC_BYTES_LEN`, so the 2.7 MB video costs nothing.
    #[test]
    fn everyday_types_from_real_fixtures() {
        // `claimed` is empty on purpose: this asserts what the BYTES say,
        // with no help from a client.
        for (path, expected) in [
            ("tests/fixtures/green-image.png", "image/png"),
            ("tests/fixtures/dedup-test.jpg", "image/jpeg"),
            ("tests/fixtures/oxicloud-logo.jpg", "image/jpeg"),
            ("tests/fixtures/free_video_over_1MB.mp4", "video/mp4"),
            ("tests/fixtures/broken.pdf", "application/pdf"),
            ("tests/e2e/fixtures/tone.wav", "audio/x-wav"),
            // An ID3v2-prefixed MP3 — the shape every tagged file in a
            // real library has, and the one where the magic bytes are the
            // TAG rather than an MPEG frame sync. Detection here is a
            // GATE: `AudioMetadataService` parses tags only for
            // `is_audio_file(stored_type)`, so getting this wrong costs a
            // whole music library its titles, silently.
            ("tests/fixtures/id3-sample.mp3", "audio/mpeg"),
        ] {
            let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let head = &bytes[..bytes.len().min(MAGIC_BYTES_LEN)];
            let name = Path::new(path).file_name().unwrap().to_str().unwrap();
            assert_eq!(refine_content_type(head, name, ""), expected, "{path}");
        }

        // Plain text has no signature, so the extension carries it — the
        // single most common upload on any deployment.
        let txt = std::fs::read("tests/fixtures/hello.txt").expect("hello.txt");
        assert_eq!(refine_content_type(&txt, "hello.txt", ""), "text/plain");
    }

    /// Phone-camera formats, synthesised from their ISO-BMFF `ftyp` box.
    ///
    /// HEIC is the iPhone default and MOV its video companion, so between
    /// them they are a large share of what a real deployment ingests.
    /// Synthesised rather than fixtured: `is_isobmff` needs only a length,
    /// the `ftyp` tag and a brand, so the whole case fits in a constant and
    /// costs the repo nothing. (Real `.heic` / `.mov` / `.mp3` samples do
    /// exist under `tests/fixtures/mime-types/`, but that directory is
    /// gitignored — a test depending on it would pass here and fail on a
    /// fresh clone.)
    #[test]
    fn phone_camera_formats_are_detected() {
        // 0x18 = 24 bytes total: length, "ftyp", major brand, minor
        // version, then two compatible brands.
        let heic = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic";
        assert_eq!(
            refine_content_type(heic, "IMG_1034.heic", "application/octet-stream"),
            "image/heif",
            "an iPhone photo must be recognised from its brand"
        );
        // ⚠ Note what this does NOT do: the `.heic` extension says
        // `image/heic`, but `image/heif` is not an envelope, so no
        // narrowing fires and the detected spelling stands. That is one of
        // the parent-type residuals measured against the Tika corpus — it
        // needs a type hierarchy, not a whitelist entry. Pinned so the
        // behaviour is deliberate rather than surprising.
        assert_eq!(
            refine_content_type(heic, "IMG_1034.heic", "image/heic"),
            "image/heif"
        );

        let mov = b"\x00\x00\x00\x14ftypqt  \x00\x00\x00\x00";
        assert_eq!(
            refine_content_type(mov, "IMG_1035.mov", "application/octet-stream"),
            "video/quicktime"
        );
    }

    #[test]
    fn mp3_is_detected_with_or_without_id3() {
        // Tagged: an ID3v2 header. This is what a tagged music file looks
        // like, and the one the audio-metadata extractor then parses.
        assert_eq!(
            refine_content_type(b"ID3\x04\x00\x00\x00\x00\x00\x00body", "track.mp3", ""),
            "audio/mpeg"
        );
        // Untagged: a bare MPEG frame sync. Plenty of real files have no
        // ID3 at all, and they must still reach `is_audio_file`.
        assert_eq!(
            refine_content_type(b"\xFF\xFB\x90\x64\x00\x00\x00\x00", "track.mp3", ""),
            "audio/mpeg"
        );
    }

    /// The gates those types feed, asserted against the same strings.
    ///
    /// A prefix test looks too trivial to test until `infer` returns
    /// `audio/x-wav` rather than `audio/wav` and someone has written
    /// `== "audio/mpeg"` somewhere. This pins that the x- forms still
    /// satisfy the predicates the lifecycle hooks use.
    #[test]
    fn everyday_types_satisfy_the_derivation_gates() {
        for audio in ["audio/x-wav", "audio/mpeg", "audio/x-flac", "audio/mp4"] {
            assert!(
                audio.starts_with("audio/"),
                "{audio} must pass AudioMetadataService::is_audio_file"
            );
        }
        for visual in ["image/png", "image/jpeg", "video/mp4", "image/heif"] {
            assert!(
                visual.starts_with("image/") || visual.starts_with("video/"),
                "{visual} must pass MediaMetadataService::handles"
            );
        }
    }

    /// Audit `refine_content_type` against a real-world corpus.
    ///
    /// `#[ignore]`d and opt-in: it needs Apache Tika's `test-documents`
    /// copied into `tests/fixtures/mime-types/`, which is gitignored
    /// (~119 MB, 826 files, includes `*KEY.pem` that secret scanners
    /// reject). Copy it in and run:
    ///
    /// ```text
    /// for F in ./tika-parsers-standard-modules/*/src/test/resources/test-documents/*; \
    ///   do cp -i "$F" tests/fixtures/mime-types; done
    /// cargo test --lib mime_detect::tests::tika_corpus_audit -- --ignored --nocapture
    /// ```
    ///
    /// Not an assertion — a report. The question it answers is "which
    /// detections describe the envelope rather than the content", which
    /// is how `generic_envelope` should be extended: from evidence, not
    /// from guessing which format is a zip in disguise.
    #[test]
    #[ignore = "needs the Tika corpus copied into tests/fixtures/mime-types (gitignored)"]
    fn tika_corpus_audit() {
        use std::collections::BTreeMap;

        let dir = Path::new("tests/fixtures/mime-types");
        let Ok(entries) = std::fs::read_dir(dir) else {
            eprintln!("corpus absent at {} — see the doc comment", dir.display());
            return;
        };

        let mut silent = 0usize;
        let mut agreed = 0usize;
        // detected -> extension-implied -> example filenames
        let mut disagreements: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let head = &bytes[..bytes.len().min(MAGIC_BYTES_LEN)];
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();

            let Some(kind) = infer::get(head) else {
                silent += 1;
                continue;
            };
            let detected = kind.mime_type().to_string();

            // What the filename claims, as the reference "truth" — it is
            // what a user and every other tool would call this file.
            let by_ext = mime_guess::from_path(&name)
                .first()
                .map(|m| m.to_string())
                .unwrap_or_default();

            // What we would actually store today.
            let stored = refine_content_type(head, &name, "");

            if by_ext.is_empty() || stored == by_ext {
                agreed += 1;
            } else {
                disagreements
                    .entry((detected, by_ext))
                    .or_default()
                    .push(name);
            }
        }

        eprintln!("\n── Tika corpus audit ───────────────────────────────");
        eprintln!("infer silent (claim/extension decides): {silent}");
        eprintln!("stored type matches the extension:      {agreed}");
        eprintln!("disagreements, by (detected → extension):");
        for ((detected, by_ext), names) in &disagreements {
            eprintln!(
                "  {:>4}×  {detected}  →  {by_ext}\n          e.g. {}",
                names.len(),
                names.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            );
        }
        eprintln!("────────────────────────────────────────────────────\n");
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
