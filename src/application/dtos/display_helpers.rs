//! Shared display helpers for DTOs.
//!
//! These functions centralise the mime→icon / mime→category / size→human-string
//! logic so that every API response carries pre-computed display fields and the
//! frontend does **not** need to duplicate these mappings.
//!
//! The approach is: try MIME first (specific matches beat prefix matches),
//! then fall back to the file extension when the MIME is generic
//! (`application/octet-stream` or empty).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock};

use crate::application::dtos::search_dto::SearchTypeFilter;

// ─── Arc<str> interning for closed-set display values ────────────────
//
// `FileDto` / `FolderDto` store their display fields as `Arc<str>` so DTO
// clones are O(1). But `Arc::<str>::from(&str)` always allocates + copies,
// so building the DTO paid 3-4 heap allocations per row even though the
// value space is a small closed set. Interning turns each conversion into
// a HashMap lookup + refcount bump.

/// Every `&'static str` that [`icon_class_for`], [`icon_special_class_for`]
/// and [`category_for`] can return, plus the folder-DTO constants.
///
/// Keep this table in sync when adding a value to those functions — a
/// missing entry is not a bug (callers fall back to `Arc::from`, same
/// bytes, one extra allocation), just a lost optimization.
static DISPLAY_INTERN: LazyLock<HashMap<&'static str, Arc<str>>> = LazyLock::new(|| {
    const CLOSED_SET: &[&str] = &[
        // icon_class_for
        "fas fa-file-pdf",
        "fas fa-file-word",
        "fas fa-file-excel",
        "fas fa-file-powerpoint",
        "fas fa-file-archive",
        "fas fa-file-code",
        "fas fa-hdd",
        "fas fa-file-image",
        "fas fa-file-video",
        "fas fa-file-audio",
        "fas fa-file-alt",
        "fas fa-terminal",
        "fas fa-file",
        // icon_special_class_for
        "pdf-icon",
        "doc-icon",
        "spreadsheet-icon",
        "presentation-icon",
        "archive-icon",
        "code-icon json-icon",
        "code-icon js-icon",
        "code-icon ts-icon",
        "code-icon html-icon",
        "code-icon sql-icon",
        "code-icon config-icon",
        "code-icon php-icon",
        "script-icon",
        "installer-icon",
        "image-icon",
        "video-icon",
        "audio-icon",
        "code-icon py-icon",
        "code-icon rust-icon",
        "code-icon",
        "code-icon go-icon",
        "code-icon ruby-icon",
        "code-icon md-icon",
        "code-icon css-icon",
        "code-icon java-icon",
        "code-icon c-icon",
        "code-icon cs-icon",
        "code-icon swift-icon",
        "",
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
        // FolderDto constants
        "fas fa-folder",
        "folder-icon",
        "Folder",
    ];
    CLOSED_SET.iter().map(|s| (*s, Arc::from(*s))).collect()
});

/// Returns a shared `Arc<str>` for a display value from the closed sets
/// above (icon class, icon special class, category). Lookup + refcount
/// bump instead of alloc + copy; unknown values (future additions not
/// yet in the table) fall back to `Arc::from` with identical bytes.
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

/// The three display classifications for one `(name, mime)` pair.
pub struct DisplayClass {
    pub icon_class: &'static str,
    pub icon_special_class: &'static str,
    pub category: &'static str,
}

/// Run all three classifiers over one `(name, mime)` pair, lowering the
/// extension **once into a stack buffer** instead of each classifier
/// allocating its own `to_ascii_lowercase()` String on the fallback path
/// (generic/empty MIME rows — common for code and unknown types). Each
/// decision tree is byte-for-byte the classifier it replaces
/// (benches/ROUND11.md §21 gates the equivalence over a corpus).
pub fn classify_display(name: &str, mime: &str) -> DisplayClass {
    let mut buf = [0u8; MAX_CLASSIFIED_EXT];
    let ext = ext_of(name).and_then(|e| lower_ext_into(e, &mut buf));
    DisplayClass {
        icon_class: icon_class_with_ext(mime, ext),
        icon_special_class: icon_special_class_with_ext(mime, ext),
        category: category_with_ext(mime, ext),
    }
}

// ─── Icon class (FontAwesome) ────────────────────────────────────────

/// Returns the FontAwesome icon class for a file, considering both MIME
/// and filename extension as fallback.
///
/// Use this instead of the old `mime_to_icon_class` whenever the filename
/// is available.
pub fn icon_class_for(name: &str, mime: &str) -> &'static str {
    let mut buf = [0u8; MAX_CLASSIFIED_EXT];
    let ext = ext_of(name).and_then(|e| lower_ext_into(e, &mut buf));
    icon_class_with_ext(mime, ext)
}

fn icon_class_with_ext(mime: &str, ext: Option<&str>) -> &'static str {
    // 1. Try specific MIME matches first
    match mime {
        "application/pdf" => return "fas fa-file-pdf",
        // MS Office & OpenDocument – Word
        "application/msword"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.oasis.opendocument.text" => return "fas fa-file-word",
        // Excel
        "application/vnd.ms-excel"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.oasis.opendocument.spreadsheet"
        | "text/csv" => return "fas fa-file-excel",
        // PowerPoint
        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.oasis.opendocument.presentation" => return "fas fa-file-powerpoint",
        // Archives
        "application/zip"
        | "application/x-rar-compressed"
        | "application/vnd.rar"
        | "application/x-7z-compressed"
        | "application/gzip"
        | "application/x-gzip"
        | "application/x-tar"
        | "application/x-bzip2"
        | "application/x-xz"
        | "application/x-compress" => return "fas fa-file-archive",
        // JSON / JavaScript / code transported as application/*
        "application/json"
        | "application/ld+json"
        | "application/javascript"
        | "application/typescript"
        | "application/x-httpd-php"
        | "application/xml"
        | "application/xhtml+xml"
        | "application/sql"
        | "application/x-yaml"
        | "application/toml"
        | "application/x-sh"
        | "application/x-shellscript"
        | "application/x-csh" => return "fas fa-file-code",
        // Installers / disk images
        "application/x-apple-diskimage"
        | "application/x-ms-dos-executable"
        | "application/x-msdownload"
        | "application/x-msi"
        | "application/vnd.debian.binary-package"
        | "application/x-rpm"
        | "application/vnd.appimage" => return "fas fa-hdd",
        _ => {}
    }

    // 2. MIME prefix matches
    if mime.starts_with("image/") {
        return "fas fa-file-image";
    } else if mime.starts_with("video/") {
        return "fas fa-file-video";
    } else if mime.starts_with("audio/") {
        return "fas fa-file-audio";
    } else if mime.starts_with("text/x-script")
        || mime.starts_with("text/x-python")
        || mime.starts_with("text/x-java")
        || mime.starts_with("text/x-c")
        || mime.starts_with("text/x-rust")
        || mime.starts_with("text/x-go")
        || mime.starts_with("text/x-ruby")
        || mime.starts_with("text/x-shellscript")
        || mime.starts_with("text/x-php")
        || mime.contains("javascript")
        || mime.contains("typescript")
    {
        return "fas fa-file-code";
    } else if mime.starts_with("text/") {
        return "fas fa-file-alt";
    }

    // 3. Extension-based fallback (for application/octet-stream, empty, etc.)
    if let Some(ext) = ext {
        return match ext {
            "pdf" => "fas fa-file-pdf",
            "doc" | "docx" | "odt" | "rtf" => "fas fa-file-word",
            "xls" | "xlsx" | "ods" | "csv" => "fas fa-file-excel",
            "ppt" | "pptx" | "odp" | "key" => "fas fa-file-powerpoint",
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "svg" | "webp" | "ico" | "tiff" | "tif"
            | "heic" | "heif" | "avif" => "fas fa-file-image",
            "mp4" | "avi" | "mkv" | "mov" | "wmv" | "flv" | "webm" | "m4v" => "fas fa-file-video",
            "mp3" | "wav" | "ogg" | "flac" | "aac" | "wma" | "m4a" | "opus" => "fas fa-file-audio",
            "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "lz4" => {
                "fas fa-file-archive"
            }
            "exe" | "msi" | "dmg" | "deb" | "rpm" | "appimage" | "pkg" | "snap" | "flatpak" => {
                "fas fa-hdd"
            }
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "py" | "pyw" | "rs" | "go" | "java"
            | "kt" | "kts" | "scala" | "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" | "cs" | "rb"
            | "php" | "swift" | "r" | "lua" | "pl" | "pm" | "html" | "htm" | "css" | "scss"
            | "sass" | "less" | "json" | "xml" | "yaml" | "yml" | "toml" | "ini" | "cfg"
            | "conf" | "sql" | "graphql" | "proto" | "vue" | "svelte" => "fas fa-file-code",
            "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" => "fas fa-terminal",
            "md" | "markdown" | "rst" | "txt" => "fas fa-file-alt",
            _ => "fas fa-file",
        };
    }

    "fas fa-file"
}

// ─── Icon special class (CSS styling) ────────────────────────────────

/// Returns the CSS class for styling the icon container, considering both
/// MIME and filename extension.
///
/// The returned class maps to CSS rules in `style.css` that set colours,
/// backgrounds and decorative pseudo-elements per file type.
pub fn icon_special_class_for(name: &str, mime: &str) -> &'static str {
    let mut buf = [0u8; MAX_CLASSIFIED_EXT];
    let ext = ext_of(name).and_then(|e| lower_ext_into(e, &mut buf));
    icon_special_class_with_ext(mime, ext)
}

fn icon_special_class_with_ext(mime: &str, ext: Option<&str>) -> &'static str {
    // 1. Specific MIME matches
    match mime {
        "application/pdf" => return "pdf-icon",
        "application/msword"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.oasis.opendocument.text" => return "doc-icon",
        "application/vnd.ms-excel"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.oasis.opendocument.spreadsheet"
        | "text/csv" => return "spreadsheet-icon",
        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.oasis.opendocument.presentation" => return "presentation-icon",
        "application/zip"
        | "application/x-rar-compressed"
        | "application/vnd.rar"
        | "application/x-7z-compressed"
        | "application/gzip"
        | "application/x-gzip"
        | "application/x-tar"
        | "application/x-bzip2"
        | "application/x-xz"
        | "application/x-compress" => return "archive-icon",
        "application/json" | "application/ld+json" => return "code-icon json-icon",
        "application/javascript" => return "code-icon js-icon",
        "application/typescript" => return "code-icon ts-icon",
        "application/xml" | "application/xhtml+xml" => return "code-icon html-icon",
        "application/sql" => return "code-icon sql-icon",
        "application/x-yaml" | "application/toml" => return "code-icon config-icon",
        "application/x-httpd-php" => return "code-icon php-icon",
        "application/x-sh" | "application/x-shellscript" | "application/x-csh" => {
            return "script-icon";
        }
        "application/x-apple-diskimage"
        | "application/x-ms-dos-executable"
        | "application/x-msdownload"
        | "application/x-msi"
        | "application/vnd.debian.binary-package"
        | "application/x-rpm"
        | "application/vnd.appimage" => return "installer-icon",
        _ => {}
    }

    // 2. MIME prefix matches
    if mime.starts_with("image/") {
        return "image-icon";
    } else if mime.starts_with("video/") {
        return "video-icon";
    } else if mime.starts_with("audio/") {
        return "audio-icon";
    } else if mime.starts_with("text/x-python") {
        return "code-icon py-icon";
    } else if mime.starts_with("text/x-rust") {
        return "code-icon rust-icon";
    } else if mime.starts_with("text/x-java") || mime.starts_with("text/x-c") {
        return "code-icon";
    } else if mime.starts_with("text/x-go") {
        return "code-icon go-icon";
    } else if mime.starts_with("text/x-ruby") {
        return "code-icon ruby-icon";
    } else if mime.starts_with("text/x-shellscript") {
        return "script-icon";
    } else if mime.starts_with("text/x-script") || mime.starts_with("text/x-php") {
        return "code-icon";
    } else if mime.starts_with("text/markdown") {
        return "code-icon md-icon";
    } else if mime.starts_with("text/html") {
        return "code-icon html-icon";
    } else if mime.starts_with("text/css") {
        return "code-icon css-icon";
    } else if mime.contains("javascript") {
        return "code-icon js-icon";
    } else if mime.contains("typescript") {
        return "code-icon ts-icon";
    } else if mime.starts_with("text/") {
        return "doc-icon";
    }

    // 3. Extension-based fallback
    if let Some(ext) = ext {
        return match ext {
            "pdf" => "pdf-icon",
            "doc" | "docx" | "odt" | "rtf" => "doc-icon",
            "xls" | "xlsx" | "ods" | "csv" => "spreadsheet-icon",
            "ppt" | "pptx" | "odp" | "key" => "presentation-icon",
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "svg" | "webp" | "ico" | "tiff" | "tif"
            | "heic" | "heif" | "avif" => "image-icon",
            "mp4" | "avi" | "mkv" | "mov" | "wmv" | "flv" | "webm" | "m4v" => "video-icon",
            "mp3" | "wav" | "ogg" | "flac" | "aac" | "wma" | "m4a" | "opus" => "audio-icon",
            "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "lz4" => "archive-icon",
            "exe" | "msi" | "dmg" | "deb" | "rpm" | "appimage" | "pkg" | "snap" | "flatpak" => {
                "installer-icon"
            }
            "py" | "pyw" => "code-icon py-icon",
            "rs" => "code-icon rust-icon",
            "go" => "code-icon go-icon",
            "java" | "kt" | "kts" | "scala" => "code-icon java-icon",
            "js" | "jsx" | "mjs" | "cjs" => "code-icon js-icon",
            "ts" | "tsx" => "code-icon ts-icon",
            "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" => "code-icon c-icon",
            "cs" => "code-icon cs-icon",
            "rb" => "code-icon ruby-icon",
            "php" => "code-icon php-icon",
            "swift" => "code-icon swift-icon",
            "r" | "lua" | "pl" | "pm" => "code-icon",
            "html" | "htm" => "code-icon html-icon",
            "css" | "scss" | "sass" | "less" => "code-icon css-icon",
            "json" => "code-icon json-icon",
            "xml" => "code-icon html-icon",
            "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" => "code-icon config-icon",
            "sql" | "graphql" | "proto" => "code-icon sql-icon",
            "vue" | "svelte" => "code-icon js-icon",
            "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" => "script-icon",
            "md" | "markdown" | "rst" => "code-icon md-icon",
            "txt" => "doc-icon",
            _ => "",
        };
    }

    ""
}

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
    } else if mime.starts_with("text/") {
        return "Text";
    }

    // 3. Extension fallback
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
            "js" | "jsx" | "ts" | "tsx" | "py" | "rs" | "go" | "java" | "c" | "cpp" | "cs"
            | "rb" | "php" | "swift" | "kt" | "scala" | "r" | "lua" | "pl" | "html" | "htm"
            | "css" | "scss" | "json" | "xml" | "yaml" | "yml" | "toml" | "sql" | "sh" | "bash"
            | "bat" | "ps1" | "vue" | "svelte" => "Code",
            "md" | "markdown" | "rst" => "Markdown",
            _ => "Document",
        };
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

/// Formats a byte count into a human-readable string (1024-based).
///
/// Matches the JavaScript `formatFileSize()` output exactly so the frontend
/// does not need its own per-file formatting.
///
/// Examples: `"0 Bytes"`, `"1.5 KB"`, `"3.27 MB"`.
pub fn format_file_size(bytes: u64) -> String {
    if bytes == 0 {
        return "0 Bytes".to_string();
    }

    const K: f64 = 1024.0;
    const SIZES: [&str; 5] = ["Bytes", "KB", "MB", "GB", "TB"];

    let i = ((bytes as f64).ln() / K.ln()).floor() as usize;
    let i = i.min(SIZES.len() - 1);

    let value = bytes as f64 / K.powi(i as i32);

    // Single buffer: write the 2-decimal value, strip trailing zeros in
    // place (matches JS parseFloat behaviour), then append the unit.
    // 16 chars covers the worst case ("16777216 TB" for u64::MAX,
    // "1023.99 Bytes" for the longest unit), so no realloc occurs.
    let mut out = String::with_capacity(16);
    let _ = write!(out, "{:.2}", value);
    while out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.pop();
    }
    out.push(' ');
    out.push_str(SIZES[i]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_file_size() {
        assert_eq!(format_file_size(0), "0 Bytes");
        assert_eq!(format_file_size(500), "500 Bytes");
        assert_eq!(format_file_size(1024), "1 KB");
        assert_eq!(format_file_size(1536), "1.5 KB");
        assert_eq!(format_file_size(1_048_576), "1 MB");
        assert_eq!(format_file_size(3_423_744), "3.27 MB");
        assert_eq!(format_file_size(1_073_741_824), "1 GB");
    }

    #[test]
    fn test_icon_class_for_with_extension_fallback() {
        // Specific MIME types
        assert_eq!(
            icon_class_for("doc.pdf", "application/pdf"),
            "fas fa-file-pdf"
        );
        assert_eq!(
            icon_class_for(
                "file.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            ),
            "fas fa-file-word"
        );

        // Extension fallback when MIME is generic
        assert_eq!(
            icon_class_for("script.py", "application/octet-stream"),
            "fas fa-file-code"
        );
        assert_eq!(
            icon_class_for("app.dmg", "application/octet-stream"),
            "fas fa-hdd"
        );
        assert_eq!(
            icon_class_for("archive.zip", "application/octet-stream"),
            "fas fa-file-archive"
        );
        assert_eq!(
            icon_class_for("data.xlsx", "application/octet-stream"),
            "fas fa-file-excel"
        );
        assert_eq!(
            icon_class_for("run.sh", "application/octet-stream"),
            "fas fa-terminal"
        );
    }

    #[test]
    fn test_icon_special_class_for() {
        // MIME-based
        assert_eq!(icon_special_class_for("", "image/png"), "image-icon");
        assert_eq!(icon_special_class_for("", "application/pdf"), "pdf-icon");
        assert_eq!(
            icon_special_class_for("", "application/json"),
            "code-icon json-icon"
        );

        // Extension-based fallback
        assert_eq!(
            icon_special_class_for("main.py", "application/octet-stream"),
            "code-icon py-icon"
        );
        assert_eq!(
            icon_special_class_for("lib.rs", "application/octet-stream"),
            "code-icon rust-icon"
        );
        assert_eq!(
            icon_special_class_for("style.css", "application/octet-stream"),
            "code-icon css-icon"
        );
        assert_eq!(
            icon_special_class_for("data.xlsx", "application/octet-stream"),
            "spreadsheet-icon"
        );
        assert_eq!(
            icon_special_class_for("backup.tar", "application/octet-stream"),
            "archive-icon"
        );
        assert_eq!(
            icon_special_class_for("setup.dmg", "application/octet-stream"),
            "installer-icon"
        );
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
            "fas fa-file-pdf",
            "fas fa-file",
            "fas fa-terminal",
            "fas fa-folder",
            "code-icon rust-icon",
            "folder-icon",
            "",
            "PDF",
            "Folder",
            "Document",
            "Markdown",
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
