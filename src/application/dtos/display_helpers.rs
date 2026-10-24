//! Shared display helpers for DTOs — the mime→category mapping and the
//! `Arc<str>` interning that keeps it cheap per row.
//!
//! The approach is: try MIME first (specific matches beat prefix matches),
//! then fall back to the file extension when the MIME is generic
//! (`application/octet-stream` or empty). The extension half is transitional:
//! once content sniffing has normalised stored MIME
//! (`docs/plan/blob-resniff.md`) the classifier becomes mime-only. See the
//! `ResourceCategory` section of
//! `docs/plan/retire-precomputed-display-fields.md`.
//!
//! **This module used to also own mime→icon and size→human-string**, on the
//! stated principle that "every API response carries pre-computed display
//! fields and the frontend does **not** need to duplicate these mappings".
//! That principle was reversed deliberately, not drifted away from:
//! `icon_special_class` named a stylesheet that no longer existed,
//! `icon_class` was a CSS class the SPA only ever parsed a glyph name back
//! out of, and `size_formatted` had two disagreeing implementations here
//! while the SPA rendered its own string anyway. All three are gone
//! (`retire-precomputed-display-fields.md`, steps 2/4/5) and the icon
//! mapping now lives in `frontend/src/lib/utils/display.ts`.
//!
//! `category` stays server-side on purpose: `category_order_for` turns it
//! into `storage.files.category_order`, the indexed `type` sort key, so the
//! FE's buckets and the server's `ORDER BY` must agree. Deriving it in the
//! client would let them disagree.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use crate::application::dtos::search_dto::SearchTypeFilter;

// ─── Arc<str> interning for closed-set display values ────────────────
//
// `FileDto` / `FolderDto` store `category` and `mime_type` as `Arc<str>` so
// DTO clones are O(1). But `Arc::<str>::from(&str)` always allocates +
// copies, so building the DTO paid a heap allocation per field per row even
// though the value space is a small closed set. Interning turns each
// conversion into a HashMap lookup + refcount bump. (It paid 3-4 before the
// icon and size fields left the wire.)

/// Every `&'static str` that [`category_for`] can return, plus the
/// folder-DTO constant.
///
/// Keep this table in sync when adding a value to those functions — a
/// missing entry is not a bug (callers fall back to `Arc::from`, same
/// bytes, one extra allocation), just a lost optimization.
static DISPLAY_INTERN: LazyLock<HashMap<&'static str, Arc<str>>> = LazyLock::new(|| {
    const CLOSED_SET: &[&str] = &[
        // category_for
        "PDF",
        "Document",
        "Spreadsheet",
        "Presentation",
        "Archive",
        "Code",
        "Installer",
        "Image",
        "Video",
        "Audio",
        "Markdown",
        "Text",
        // FolderDto constant
        "Folder",
    ];
    CLOSED_SET.iter().map(|s| (*s, Arc::from(*s))).collect()
});

/// Returns a shared `Arc<str>` for a closed-set display value (category, or
/// the folder constant). Lookup + refcount bump instead of alloc + copy;
/// unknown values (future additions not yet in the table) fall back to
/// `Arc::from` with identical bytes.
pub fn intern_display(s: &'static str) -> Arc<str> {
    DISPLAY_INTERN
        .get(s)
        .cloned()
        .unwrap_or_else(|| Arc::from(s))
}

/// The MIME types that dominate real storage rows. Exotic types fall back
/// to a per-row `Arc::from` — correctness is unaffected, only the alloc is.
static MIME_INTERN: LazyLock<HashMap<&'static str, Arc<str>>> = LazyLock::new(|| {
    const COMMON_MIMES: &[&str] = &[
        "",
        "directory",
        "application/octet-stream",
        // Images
        "image/jpeg",
        "image/png",
        "image/gif",
        "image/webp",
        "image/svg+xml",
        "image/heic",
        "image/heif",
        "image/avif",
        "image/bmp",
        "image/tiff",
        "image/x-icon",
        // Video
        "video/mp4",
        "video/quicktime",
        "video/webm",
        "video/x-matroska",
        "video/x-msvideo",
        // Audio
        "audio/mpeg",
        "audio/mp4",
        "audio/ogg",
        "audio/flac",
        "audio/wav",
        "audio/x-wav",
        "audio/aac",
        // Documents
        "application/pdf",
        "application/msword",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "application/vnd.ms-excel",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "application/vnd.ms-powerpoint",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "application/vnd.oasis.opendocument.text",
        "application/vnd.oasis.opendocument.spreadsheet",
        // Text / code
        "text/plain",
        "text/csv",
        "text/html",
        "text/css",
        "text/markdown",
        "text/xml",
        "application/json",
        "application/javascript",
        "application/xml",
        "application/x-yaml",
        // Archives
        "application/zip",
        "application/gzip",
        "application/x-tar",
        "application/x-7z-compressed",
        "application/x-rar-compressed",
    ];
    COMMON_MIMES.iter().map(|s| (*s, Arc::from(*s))).collect()
});

/// Returns a shared `Arc<str>` for the given MIME type. Common types hit
/// the intern table (refcount bump); exotic ones allocate as before.
pub fn intern_mime(mime: &str) -> Arc<str> {
    MIME_INTERN
        .get(mime)
        .cloned()
        .unwrap_or_else(|| Arc::from(mime))
}

// ─── Private: extract lowercase extension from a filename ────────────
fn ext_of(name: &str) -> Option<&str> {
    let name = name.rsplit('/').next().unwrap_or(name); // strip path
    let after_dot = name.rsplit('.').next()?;
    // Reject the whole name (no dot) or empty after dot
    if after_dot.len() == name.len() || after_dot.is_empty() {
        return None;
    }
    Some(after_dot)
}

/// Longest extension any classifier table matches ("appimage", "markdown"
/// — 8 bytes). Longer extensions can only ever hit the `_` arms, so they
/// skip the buffer entirely.
const MAX_CLASSIFIED_EXT: usize = 16;

/// Lowercase `ext` into `buf` without heap allocation. Returns `None` for
/// extensions longer than any table entry — the caller must then take the
/// same default arm `ext.to_ascii_lowercase()` would have fallen into.
fn lower_ext_into<'b>(ext: &str, buf: &'b mut [u8; MAX_CLASSIFIED_EXT]) -> Option<&'b str> {
    let bytes = ext.as_bytes();
    if bytes.len() > MAX_CLASSIFIED_EXT {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        buf[i] = b.to_ascii_lowercase();
    }
    // ASCII-lowercasing bytes keeps UTF-8 validity (non-ASCII bytes pass
    // through untouched).
    std::str::from_utf8(&buf[..bytes.len()]).ok()
}

// `DisplayClass` / `classify_display` lived here: a fused pass that lowered
// the extension once into a stack buffer and ran all three display
// classifiers off it, instead of each allocating its own
// `to_ascii_lowercase()` on the generic-MIME fallback path (ROUND11 §21,
// 2.12x). Steps 2 and 4 of `retire-precomputed-display-fields.md` retired
// `icon_special_class` and `icon_class` from the wire, leaving `category_for`
// as the only classifier — there is nothing left to fuse, so the struct and
// the wrapper are gone and callers use `category_for` directly.

// ─── Category label ──────────────────────────────────────────────────

/// Returns a human-readable category label, considering MIME + extension.
pub fn category_for(name: &str, mime: &str) -> &'static str {
    let mut buf = [0u8; MAX_CLASSIFIED_EXT];
    let ext = ext_of(name).and_then(|e| lower_ext_into(e, &mut buf));
    category_with_ext(mime, ext)
}

fn category_with_ext(mime: &str, ext: Option<&str>) -> &'static str {
    // 1. Specific MIME matches
    match mime {
        "application/pdf" => return "PDF",
        "application/msword"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.oasis.opendocument.text" => return "Document",
        "application/vnd.ms-excel"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.oasis.opendocument.spreadsheet"
        | "text/csv" => return "Spreadsheet",
        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.oasis.opendocument.presentation" => return "Presentation",
        "application/zip"
        | "application/x-rar-compressed"
        | "application/vnd.rar"
        | "application/x-7z-compressed"
        | "application/gzip"
        | "application/x-tar" => return "Archive",
        "application/json"
        | "application/javascript"
        | "application/typescript"
        | "application/xml"
        | "application/sql"
        | "application/x-sh"
        | "application/x-shellscript" => return "Code",
        "application/x-apple-diskimage"
        | "application/x-ms-dos-executable"
        | "application/x-msdownload"
        | "application/x-msi" => return "Installer",
        _ => {}
    }

    // 2. MIME prefix
    if mime.starts_with("image/") {
        return "Image";
    } else if mime.starts_with("video/") {
        return "Video";
    } else if mime.starts_with("audio/") {
        return "Audio";
    } else if mime.starts_with("text/x-") || mime.contains("script") || mime.contains("javascript")
    {
        return "Code";
    } else if mime.starts_with("text/markdown") {
        return "Markdown";
    } else if mime.starts_with("text/html") || mime.starts_with("text/css") {
        return "Code";
    }

    // 3. The extension, for the cases MIME provably cannot answer.
    //
    // ⚠ `text/plain` used to return "Text" in step 2, ABOVE this point,
    // which meant the extension was never consulted for it. `mime_guess`
    // maps `.c`, `.cpp` and `.py` to `text/plain` — so every C, C++ and
    // Python file was categorised Text rather than Code, invisibly,
    // because "Text" is a plausible answer for a text file. Found
    // 2026-10-24 while scoping the mime-only work; see the test.
    //
    // This is also why the name cannot be dropped from the signature.
    // Source code has NO magic bytes — `infer` returns None for every
    // language and always will, so `content_rescan` can never improve
    // these rows — and `text/plain` cannot tell a C file from a shopping
    // list. The extension is the only signal, permanently.
    if let Some(ext) = ext {
        return match ext {
            "pdf" => "PDF",
            "doc" | "docx" | "odt" | "rtf" | "txt" => "Document",
            "xls" | "xlsx" | "ods" | "csv" => "Spreadsheet",
            "ppt" | "pptx" | "odp" | "key" => "Presentation",
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "svg" | "webp" | "ico" | "tiff" | "heic"
            | "avif" => "Image",
            "mp4" | "avi" | "mkv" | "mov" | "wmv" | "flv" | "webm" | "m4v" => "Video",
            "mp3" | "wav" | "ogg" | "flac" | "aac" | "wma" | "m4a" | "opus" => "Audio",
            "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" => "Archive",
            "exe" | "msi" | "dmg" | "deb" | "rpm" | "appimage" => "Installer",
            "js" | "jsx" | "ts" | "tsx" | "py" | "rs" | "go" | "java" | "c" | "h" | "cpp"
            | "hpp" | "cc" | "cs" | "rb" | "php" | "swift" | "kt" | "scala" | "r" | "lua"
            | "pl" | "html" | "htm" | "css" | "scss" | "json" | "xml" | "yaml" | "yml" | "toml"
            | "sql" | "sh" | "bash" | "bat" | "ps1" | "vue" | "svelte" => "Code",
            "md" | "markdown" | "rst" => "Markdown",
            // `text/plain` with an extension we do not classify is still
            // text, and must not become "Document".
            _ if mime.starts_with("text/") => "Text",
            _ => "Document",
        };
    }

    if mime.starts_with("text/") {
        return "Text";
    }

    "Document"
}

/// Returns the sort order for a file category, stored as `category_order` in `storage.files`.
///
/// Values are **sparse multiples of 100** so a future category can be slotted between two
/// existing ones (e.g. "RichText" = 550, between Document=500 and Spreadsheet=600) without
/// renumbering any rows. Folders are not handled here — the SQL query hard-codes 0 for them.
///
/// This function delegates to [`category_for`] so the two are always in sync.
pub fn category_order_for(name: &str, mime: &str) -> i16 {
    match category_for(name, mime) {
        "Image" => 100,
        "Video" => 200,
        "Audio" => 300,
        "PDF" => 400,
        "Document" => 500,
        "Spreadsheet" => 600,
        "Presentation" => 700,
        "Archive" => 800,
        "Code" => 900,
        "Markdown" => 1000,
        "Text" => 1100,
        "Installer" => 1200,
        _ => 9999, // Other / unknown
    }
}

/// The `category_order` values a coarse UI type filter maps to.
///
/// Lives here, immediately below [`category_order_for`], because the two MUST
/// agree: this function answers "which stored values count as `documents`"
/// and that one decides what gets stored. Split them across files and they
/// drift — the mapping already exists in Rust and in the backfill of
/// `20260527000001_files_category_order.sql`, and a third copy is what
/// `20261103000009` deliberately avoided when `copy_folder_tree` needed it.
///
/// **Server-side only.** The SPA sends one coarse token (`documents`) and
/// never sees these integers; putting the expansion in the client would just
/// move the guess from file extensions to magic numbers.
///
/// `Folders` and `All` are absent on purpose — neither maps to a
/// `storage.files` value. A folder has no row in that table at all, so those
/// two cases are handled by selecting which UNION branches run, not by a
/// predicate. See `docs/plan/search-type-filter-by-category.md`.
///
/// `0` and `1`–`99` are reserved (folder sentinel + headroom) and must never
/// appear here — see `docs/architecture/resource-listing.md`.
///
/// Takes the typed [`SearchTypeFilter`], not a `&str`, so this match is
/// **exhaustive**: adding a filter variant is a compile error here, which is
/// the whole point of the enum. A stringly-typed version would need a
/// catch-all, and that arm would silently return "no categories" for the new
/// variant — an empty result page with no error, the exact failure mode §7
/// spent seven commits removing.
pub fn category_orders_for_filter(filter: SearchTypeFilter) -> &'static [i16] {
    use SearchTypeFilter as F;
    match filter {
        F::Image => &[100],
        F::Video => &[200],
        F::Audio => &[300],
        // The UI's single "Documents" affordance is coarser than the DB's
        // grouping on purpose: a user picking Documents wants the PDF, the
        // spreadsheet and the README, not three separate filters.
        F::Document => &[400, 500, 600, 700, 1000, 1100],
        F::Archive => &[800],
        F::Code => &[900],
        // Unknown/undetected mime. Surfaced because an operator chasing a
        // broken type detection needs a way to list these; left out of the
        // SPA's own picker until someone asks for it.
        F::Other => &[9999],
        // Neither maps to a `storage.files` value — they select which UNION
        // branches run instead. Named rather than left to a catch-all so the
        // reader sees that the omission is deliberate, and so a new variant
        // cannot hide in this arm.
        F::All | F::Folder => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `test_icon_class_for_with_extension_fallback` lived here. The
    // equivalent coverage now sits in `frontend/src/lib/utils/display.test.ts`
    // against `iconNameFor`, which is where the classifier moved — same
    // corpus (specific MIME, generic MIME + extension, shell scripts), same
    // resolution order, asserting registry names instead of CSS classes.

    /// ⚠ `text/plain` source files are Code, not Text.
    ///
    /// `mime_guess` maps `.c`, `.cpp` and `.py` to `text/plain`, and the
    /// classifier used to return "Text" for any `text/` prefix BEFORE
    /// consulting the extension — so every C, C++ and Python file in the
    /// corpus was categorised Text. Invisible in the UI, because "Text"
    /// is a plausible answer for a text file; it surfaced only when
    /// probing what `refine_content_type` actually stores for source
    /// files (2026-10-24).
    ///
    /// This also pins why the name cannot leave the signature: source
    /// code has no magic bytes, so `infer` is silent for every language
    /// and `content_rescan` can never improve these rows.
    #[test]
    fn text_plain_source_files_are_code() {
        for name in ["a.c", "x.cpp", "t.py", "h.h", "m.cc"] {
            assert_eq!(
                category_for(name, "text/plain"),
                "Code",
                "{name} stored as text/plain must classify as Code",
            );
        }

        // Still Text when the extension says nothing useful, and when
        // there is no extension at all.
        assert_eq!(category_for("notes.txt", "text/plain"), "Document");
        assert_eq!(category_for("README", "text/plain"), "Text");
        assert_eq!(category_for("data.weird", "text/plain"), "Text");

        // Markdown keeps its own bucket rather than collapsing to Code.
        assert_eq!(category_for("r.rst", "text/plain"), "Markdown");

        // A mime that DOES know better still wins — the extension is a
        // fallback, not an override.
        assert_eq!(category_for("evil.c", "image/png"), "Image");
    }

    #[test]
    fn test_category_for() {
        // MIME-based
        assert_eq!(category_for("", "image/jpeg"), "Image");
        assert_eq!(category_for("", "video/webm"), "Video");
        assert_eq!(category_for("", "audio/ogg"), "Audio");
        assert_eq!(category_for("", "application/pdf"), "PDF");
        assert_eq!(category_for("", "application/zip"), "Archive");

        // Extension-based fallback
        assert_eq!(category_for("main.rs", "application/octet-stream"), "Code");
        assert_eq!(
            category_for("photo.jpg", "application/octet-stream"),
            "Image"
        );
        assert_eq!(
            category_for("notes.md", "application/octet-stream"),
            "Markdown"
        );
    }

    /// Every value the closed-set display functions can return must hit
    /// the intern table (same bytes, shared allocation) — a miss is only
    /// a lost optimization, but this test keeps the table in sync.
    #[test]
    fn test_intern_display_covers_closed_sets_and_shares_storage() {
        for s in [
            // The `fas fa-*` glyph classes and the `*-icon` CSS classes were
            // both here until steps 2 and 4 of
            // `retire-precomputed-display-fields.md` took `icon_special_class`
            // and `icon_class` off the wire. `category_for` is the only
            // closed-set classifier left, so the table is its values plus the
            // folder constant.
            "PDF", "Folder", "Document", "Markdown",
        ] {
            let a = intern_display(s);
            let b = intern_display(s);
            assert_eq!(&*a, s, "interned bytes must be identical");
            assert!(
                Arc::ptr_eq(&a, &b),
                "closed-set value {s:?} must come from the intern table"
            );
        }
    }

    #[test]
    fn test_intern_mime_common_hits_table_exotic_falls_back() {
        let a = intern_mime("image/jpeg");
        let b = intern_mime("image/jpeg");
        assert_eq!(&*a, "image/jpeg");
        assert!(Arc::ptr_eq(&a, &b), "common MIME must be interned");

        let exotic = intern_mime("chemical/x-pdb");
        assert_eq!(&*exotic, "chemical/x-pdb");
        let exotic2 = intern_mime("chemical/x-pdb");
        assert!(
            !Arc::ptr_eq(&exotic, &exotic2),
            "exotic MIME falls back to a fresh Arc"
        );
    }

    #[test]
    fn test_ext_of() {
        assert_eq!(ext_of("file.txt"), Some("txt"));
        assert_eq!(ext_of("archive.tar.gz"), Some("gz"));
        assert_eq!(ext_of("no_extension"), None);
        assert_eq!(ext_of(".gitignore"), Some("gitignore")); // dot file treated as having extension
        assert_eq!(ext_of("path/to/file.rs"), Some("rs"));
    }
}
