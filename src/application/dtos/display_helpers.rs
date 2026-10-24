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

/// The resource categories the server classifies into — the fine set.
///
/// `SearchTypeFilter` is the COARSE view of this, and the two are
/// deliberately separate: the filter's `Document` spans Pdf, Document,
/// Spreadsheet, Presentation, Markdown and Text, so collapsing them would
/// either coarsen the Type column (users lose "Spreadsheet" vs "PDF") or
/// inflate the picker to fourteen options. What IS unified is the
/// relationship — `category_orders_for_filter` maps coarse to fine
/// *categories*, so the sort integers appear in exactly one place
/// ([`category_order_for`]).
///
/// On the wire this is a machine key, never display text: the SPA's
/// `typeLabel` maps it to a localised string. Rendering it raw is the i18n
/// bug that step 1 of `retire-precomputed-display-fields.md` fixed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
pub enum ResourceCategory {
    Image,
    Video,
    Audio,
    #[serde(rename = "PDF")]
    Pdf,
    /// Word-processor documents — `.doc`, `.docx`, `.odt`, `.rtf`.
    ///
    /// NOT named `Document`, deliberately. `SearchTypeFilter::Document` is
    /// the COARSE bucket that spans this plus Pdf, Spreadsheet,
    /// Presentation, Markdown and Text — one word for two different sets
    /// is how a reader ends up believing the filter returns only these.
    /// `TextDocument` was considered and rejected: `Text` already exists
    /// for plain text, and most desktops call a `.txt` a "text document".
    WordProcessor,
    Spreadsheet,
    Presentation,
    Archive,
    Code,
    Markdown,
    Text,
    /// Distributable packages — `.deb`, `.rpm`, `.dmg`, `.msi`, `.pkg`.
    ///
    /// Renamed from `Installer` once `.exe` moved to `Executable`: what is
    /// left really is the package formats, and all of them are separable
    /// by content (`.msi` is OLE2, `.deb` is an `ar` archive, `.rpm` and
    /// `.dmg` carry their own signatures). They are deliberately NOT
    /// `Archive`, even though several are archives underneath — a user
    /// extracts an archive and installs a package.
    Package,
    /// Configuration and structured-data files — JSON, YAML, TOML, INI.
    ///
    /// Split out of `Code` because a deployment's worth of `.yaml` is not
    /// what someone means by "code", and these are the files an operator
    /// actually goes looking for. The arms must sit BEFORE the `text/x-`
    /// prefix: `mime_guess` maps `.yaml` to `text/x-yaml` and `.toml` to
    /// `text/x-toml`, both of which that prefix would otherwise claim.
    Config,
    /// Disk and VM images.
    ///
    /// ⚠ Mostly NOT content-detectable, and the reason is structural: an
    /// ISO's `CD001` marker lives at offset 32769, far past the 8 KiB head
    /// `content_rescan` reads, so `infer` returns None and `mime_guess`
    /// offers only `application/octet-stream`. `.iso` and `.img` are
    /// therefore extension-only, permanently. qcow2, VMDK and VDI do have
    /// real signatures and are matched by MIME.
    DiskImage,
    /// Fonts.
    ///
    /// Both naming families are matched, and that is not belt-and-braces:
    /// `infer` returns the LEGACY `application/font-woff` /
    /// `application/font-sfnt` for real font bytes, while `mime_guess`
    /// returns the modern `font/ttf` and `font/woff2` for the same files
    /// by extension. Matching only `font/` would miss everything the
    /// sniffer produces — verified against real signatures.
    Font,
    /// Ebooks — EPUB, MOBI, AZW3, FB2.
    ///
    /// EPUB is a ZIP whose first stored entry is `mimetype`, the same
    /// trick ODF uses, so `infer` identifies it as `application/epub+zip`
    /// rather than a bare zip. AZW3 and FB2 have no signature and arrive
    /// by extension only.
    Ebook,
    /// Compiled binaries and bytecode — PE, Mach-O, WebAssembly, Java
    /// class files, shared libraries.
    ///
    /// Distinct from `Installer`: a `.deb` or `.msi` is a package you
    /// install, a `.so` or `.dll` is neither. They were conflated, with
    /// `.exe` filed as an installer.
    ///
    /// NOT named `Application`, deliberately — `application/` is the MIME
    /// top-level type for PDF, zip and most formats in this file, so the
    /// name would mean two things at once, which is the problem
    /// `WordProcessor` was renamed to avoid. The user-facing label is a
    /// locale string and may well read "Applications".
    ///
    /// ⚠ ELF is NOT detectable here: `infer` carries no ELF signature
    /// (verified), `.so` guesses `application/octet-stream` and `.elf` /
    /// `.dylib` guess nothing at all. Linux binaries therefore classify as
    /// `Unknown` and no rescan can improve them — the gap is in the
    /// sniffer, not in this table.
    Executable,
    /// Not a file category — the constant every folder DTO carries. It was
    /// the bare string `"Folder"` in thirteen places, where a typo in any
    /// one of them was silent.
    Folder,
    /// Genuinely unidentifiable content.
    ///
    /// This variant is the point of typing the set. Both catch-alls used to
    /// return `Document`, so the Type column called an unidentifiable blob
    /// a "Document" — a confident lie — and `category_order_for`'s
    /// `_ => 9999` arm was UNREACHABLE, which is why `type=unknown` in
    /// search found nothing but rows whose column still held the DEFAULT.
    /// `Unknown` over `Other`: less friendly as a label, but true.
    Unknown,
}

impl ResourceCategory {
    /// The stable machine key, identical to the serialised form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "Image",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Pdf => "PDF",
            Self::WordProcessor => "WordProcessor",
            Self::Spreadsheet => "Spreadsheet",
            Self::Presentation => "Presentation",
            Self::Archive => "Archive",
            Self::Code => "Code",
            Self::Markdown => "Markdown",
            Self::Text => "Text",
            Self::Package => "Package",
            Self::Config => "Config",
            Self::DiskImage => "DiskImage",
            Self::Font => "Font",
            Self::Ebook => "Ebook",
            Self::Executable => "Executable",
            Self::Folder => "Folder",
            Self::Unknown => "Unknown",
        }
    }
}

impl std::fmt::Display for ResourceCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Arc<str> interning for closed-set display values ────────────────
//
// `FileDto` / `FolderDto` store `category` and `mime_type` as `Arc<str>` so
// DTO clones are O(1). But `Arc::<str>::from(&str)` always allocates +
// copies, so building the DTO paid a heap allocation per field per row even
// though the value space is a small closed set. Interning turns each
// conversion into a HashMap lookup + refcount bump. (It paid 3-4 before the
// icon and size fields left the wire.)

// `DISPLAY_INTERN` / `intern_display` lived here. They existed so the
// `category` DTO field — a small closed set repeated on every row — cost a
// HashMap lookup and a refcount bump instead of an allocation and a copy.
//
// Typing the set as `ResourceCategory` removed the problem rather than
// optimising it: the enum is `Copy` and one byte, so there is no allocation
// left to avoid and no table to keep in sync. The comment above that asked
// readers to "keep this table in sync when adding a value" is gone with
// it — a missed entry used to be a silently lost optimization, and now the
// compiler decides.
//
// `MIME_INTERN` below stays: `mime_type` is an open set of arbitrary
// strings, so it still benefits.

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
pub fn category_for(name: &str, mime: &str) -> ResourceCategory {
    let mut buf = [0u8; MAX_CLASSIFIED_EXT];
    let ext = ext_of(name).and_then(|e| lower_ext_into(e, &mut buf));
    category_with_ext(mime, ext)
}

fn category_with_ext(mime: &str, ext: Option<&str>) -> ResourceCategory {
    // 1. Specific MIME matches
    match mime {
        "application/pdf" => return ResourceCategory::Pdf,
        "application/msword"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.oasis.opendocument.text" => return ResourceCategory::WordProcessor,
        "application/vnd.ms-excel"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.oasis.opendocument.spreadsheet"
        | "text/csv" => return ResourceCategory::Spreadsheet,
        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.oasis.opendocument.presentation" => {
            return ResourceCategory::Presentation;
        }
        "application/zip"
        | "application/x-rar-compressed"
        | "application/vnd.rar"
        | "application/x-7z-compressed"
        | "application/gzip"
        | "application/x-tar" => return ResourceCategory::Archive,
        // Config FIRST: `text/x-yaml` and `text/x-toml` would otherwise be
        // swallowed by the `text/x-` prefix arm below, and
        // `application/json` by the Code arm.
        "application/json" | "text/x-yaml" | "text/x-toml" | "application/toml" => {
            return ResourceCategory::Config;
        }
        "application/x-qemu-disk"
        | "application/x-virtualbox-vmdk"
        | "application/x-virtualbox-vdi"
        | "application/x-iso9660-image" => return ResourceCategory::DiskImage,
        "application/javascript"
        | "application/typescript"
        | "application/xml"
        | "application/sql"
        | "application/x-sh"
        | "application/x-shellscript" => return ResourceCategory::Code,
        "application/epub+zip" | "application/x-mobipocket-ebook" => {
            return ResourceCategory::Ebook;
        }
        // `infer` emits the legacy `application/font-*` names; `mime_guess`
        // emits the modern `font/*` ones (handled by the prefix arm
        // below). Both reach real files, so both are matched.
        "application/font-woff" | "application/font-sfnt" | "application/vnd.ms-fontobject" => {
            return ResourceCategory::Font;
        }
        "application/x-apple-diskimage"
        | "application/x-msi"
        | "application/vnd.debian.binary-package"
        | "application/x-rpm" => return ResourceCategory::Package,
        // These are the strings `infer` actually produces, verified
        // against real magic bytes. The old list named
        // `application/x-ms-dos-executable` and `application/x-msdownload`
        // for `.exe` — neither is what `infer` returns, so that arm was
        // dead and a sniffed executable only reached a category via its
        // extension.
        "application/vnd.microsoft.portable-executable"
        | "application/x-mach-binary"
        | "application/wasm"
        | "application/java"
        | "application/x-executable"
        | "application/x-sharedlib"
        | "application/x-msdownload" => return ResourceCategory::Executable,
        _ => {}
    }

    // 2. MIME prefix
    if mime.starts_with("font/") {
        return ResourceCategory::Font;
    } else if mime.starts_with("image/") {
        return ResourceCategory::Image;
    } else if mime.starts_with("video/") {
        return ResourceCategory::Video;
    } else if mime.starts_with("audio/") {
        return ResourceCategory::Audio;
    } else if mime.starts_with("text/x-") || mime.contains("script") || mime.contains("javascript")
    {
        return ResourceCategory::Code;
    } else if mime.starts_with("text/markdown") {
        return ResourceCategory::Markdown;
    } else if mime.starts_with("text/html") || mime.starts_with("text/css") {
        return ResourceCategory::Code;
    }

    // 3. The extension, for the cases MIME provably cannot answer.
    //
    // ⚠ `text/plain` used to return ResourceCategory::Text in step 2, ABOVE this point,
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
            "pdf" => ResourceCategory::Pdf,
            "doc" | "docx" | "odt" | "rtf" => ResourceCategory::WordProcessor,
            // `.txt` moved out of the word-processor bucket: a
            // `text/plain` .txt already classified as Text, so the same
            // file got two different categories depending on whether its
            // stored mime happened to be known.
            "txt" | "text" | "log" => ResourceCategory::Text,
            "xls" | "xlsx" | "ods" | "csv" => ResourceCategory::Spreadsheet,
            "ppt" | "pptx" | "odp" | "key" => ResourceCategory::Presentation,
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "svg" | "webp" | "ico" | "tiff" | "heic"
            | "avif" => ResourceCategory::Image,
            "mp4" | "avi" | "mkv" | "mov" | "wmv" | "flv" | "webm" | "m4v" => {
                ResourceCategory::Video
            }
            "mp3" | "wav" | "ogg" | "flac" | "aac" | "wma" | "m4a" | "opus" => {
                ResourceCategory::Audio
            }
            "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" => ResourceCategory::Archive,
            // The Installer/Executable split is a PACKAGING convention,
            // not a content distinction, and `.appimage` is the case that
            // proves it: an AppImage is an ELF binary with a squashfs
            // appended — byte-for-byte the same magic as any other ELF. So
            // it sits with the executables, since running it is what you
            // do, while the formats that genuinely are packages keep their
            // own bucket. Those are separable by content (`.msi` is OLE2,
            // `.deb` is an `ar` archive, `.rpm` and `.dmg` have their own
            // signatures); AppImage is not, which is why it moved here.
            "msi" | "dmg" | "deb" | "rpm" | "pkg" => ResourceCategory::Package,
            "ttf" | "otf" | "woff" | "woff2" | "eot" | "ttc" => ResourceCategory::Font,
            // `.iso` / `.img` can only ever be classified here — see the
            // variant's note on the 32769-byte offset.
            "iso" | "img" | "qcow2" | "vmdk" | "vdi" | "vhd" | "vhdx" => {
                ResourceCategory::DiskImage
            }
            "json" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "env" | "properties" => {
                ResourceCategory::Config
            }
            "epub" | "mobi" | "azw" | "azw3" | "fb2" | "cbz" | "cbr" => ResourceCategory::Ebook,
            "exe" | "dll" | "so" | "dylib" | "elf" | "appimage" | "wasm" | "class" | "jar"
            | "o" => ResourceCategory::Executable,
            "js" | "jsx" | "ts" | "tsx" | "py" | "rs" | "go" | "java" | "c" | "h" | "cpp"
            | "hpp" | "cc" | "cs" | "rb" | "php" | "swift" | "kt" | "scala" | "r" | "lua"
            | "pl" | "html" | "htm" | "css" | "scss" | "xml" | "sql" | "sh" | "bash" | "bat"
            | "ps1" | "vue" | "svelte" => ResourceCategory::Code,
            "md" | "markdown" | "rst" => ResourceCategory::Markdown,
            // `text/plain` with an extension we do not classify is still
            // text, and must not become "Document".
            _ if mime.starts_with("text/") => ResourceCategory::Text,
            // UNKNOWN, not Document. An unidentifiable blob used to be
            // labelled "Document" in the Type column, which is a
            // confident lie, and it made the 9999 sort arm unreachable.
            _ => ResourceCategory::Unknown,
        };
    }

    if mime.starts_with("text/") {
        return ResourceCategory::Text;
    }

    ResourceCategory::Unknown
}

/// Returns the sort order for a file category, stored as `category_order` in `storage.files`.
///
/// Values are **sparse multiples of 100** so a future category can be slotted between two
/// existing ones (e.g. "RichText" = 550, between Document=500 and Spreadsheet=600) without
/// renumbering any rows. Folders are not handled here — the SQL query hard-codes 0 for them.
///
/// This function delegates to [`category_for`] so the two are always in sync.
pub fn category_order_for(name: &str, mime: &str) -> i16 {
    order_of(category_for(name, mime))
}

/// The sort integers, in the ONE place they are written.
///
/// Exhaustive on purpose — no `_` arm. A new `ResourceCategory` variant
/// must fail to compile here rather than silently landing in a catch-all,
/// which is how `Unknown` stayed unreachable for so long.
pub fn order_of(category: ResourceCategory) -> i16 {
    use ResourceCategory as C;
    match category {
        C::Folder => 0,
        C::Image => 100,
        C::Video => 200,
        C::Audio => 300,
        C::Pdf => 400,
        C::WordProcessor => 500,
        C::Spreadsheet => 600,
        C::Presentation => 700,
        C::Archive => 800,
        C::Code => 900,
        C::Markdown => 1000,
        C::Text => 1100,
        C::Package => 1200,
        C::Config => 950,
        C::Font => 1250,
        C::DiskImage => 1350,
        C::Ebook => 450,
        C::Executable => 1300,
        C::Unknown => 9999,
    }
}

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
pub fn categories_for_filter(filter: SearchTypeFilter) -> &'static [ResourceCategory] {
    use ResourceCategory as C;
    use SearchTypeFilter as F;
    match filter {
        F::Image => &[C::Image],
        F::Video => &[C::Video],
        F::Audio => &[C::Audio],
        // The UI's single "Documents" affordance is coarser than the DB's
        // grouping on purpose: a user picking Documents wants the PDF, the
        // spreadsheet and the README, not three separate filters.
        F::Document => &[
            C::Pdf,
            C::WordProcessor,
            C::Spreadsheet,
            C::Presentation,
            C::Markdown,
            C::Text,
            // An ebook is something you read, so it belongs with the
            // documents rather than needing its own picker option.
            C::Ebook,
        ],
        // Installer rides with Archive, and the grouping is a compromise
        // rather than a truth: a `.deb` is a package, a `.exe` is not an
        // archive at all. It is here because the alternative is what
        // shipped before — `Installer` in NO bucket, so a `.msi` could not
        // be found by any type filter. Found by
        // `every_category_is_reachable_from_some_filter` on its first run;
        // the old hand-written integer list `[400,500,600,700,1000,1100]`
        // omitted 1200 the same way. A dedicated picker option would be
        // better, and needs a `SearchTypeFilter` variant plus SPA work.
        F::Archive => &[C::Archive, C::Package, C::Executable, C::DiskImage],
        // Server-side only, exactly like `Unknown`: it exists so a
        // font library IS filterable, without adding a niche option
        // to the SPA picker until someone asks.
        F::Font => &[C::Font],
        // Config rides with Code: someone filtering for code wants
        // the repo yaml and toml alongside its sources.
        F::Code => &[C::Code, C::Config],
        // Unidentifiable content. Surfaced because an operator chasing
        // broken type detection needs a way to list these; left out of the
        // SPA's own picker until someone asks for it.
        F::Unknown => &[C::Unknown],
        // Neither maps to a `storage.files` value — they select which UNION
        // branches run instead. Named rather than left to a catch-all so the
        // reader sees that the omission is deliberate, and so a new variant
        // cannot hide in this arm.
        F::All | F::Folder => &[],
    }
}

/// The same mapping, as the integers the SQL filter binds.
///
/// Derived from [`categories_for_filter`] rather than written out, so the
/// coarse-to-fine relationship lives in ONE place and the integers in
/// another ([`order_of`]). Before this, the filter listed
/// `[400,500,600,700,1000,1100]` — a second encoding of a mapping that
/// already existed, free to drift from it.
pub fn category_orders_for_filter(filter: SearchTypeFilter) -> Vec<i16> {
    categories_for_filter(filter)
        .iter()
        .copied()
        .map(order_of)
        .collect()
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
    /// Every variant, so the drift guards below cannot silently skip one.
    ///
    /// Hand-written rather than derived: there is no `strum`-style iterator
    /// here, so the list is the one thing a new variant does NOT update
    /// automatically. The length assertion is what forces the issue — add a
    /// variant and this fails until you list it.
    const ALL: &[ResourceCategory] = &[
        ResourceCategory::Image,
        ResourceCategory::Video,
        ResourceCategory::Audio,
        ResourceCategory::Pdf,
        ResourceCategory::WordProcessor,
        ResourceCategory::Spreadsheet,
        ResourceCategory::Presentation,
        ResourceCategory::Archive,
        ResourceCategory::Code,
        ResourceCategory::Markdown,
        ResourceCategory::Text,
        ResourceCategory::Package,
        ResourceCategory::Config,
        ResourceCategory::DiskImage,
        ResourceCategory::Font,
        ResourceCategory::Ebook,
        ResourceCategory::Executable,
        ResourceCategory::Folder,
        ResourceCategory::Unknown,
    ];

    #[test]
    fn the_variant_list_is_complete() {
        // `order_of` is an exhaustive match, so a new variant breaks the
        // build there — but it would NOT break `ALL`, and every guard
        // below iterates `ALL`. Pin the count so it does.
        assert_eq!(ALL.len(), 19, "a variant was added without updating ALL");
    }

    /// `as_str` and the serde representation are two hand-written copies of
    /// the same wire key, and the serialised form is what the SPA switches
    /// on. If they drift, the server sorts by one name and the client reads
    /// another — with `Pdf`/`"PDF"` the likeliest casualty, since it is the
    /// one variant whose key is not its Rust name.
    #[test]
    fn as_str_matches_the_serialised_key() {
        for &c in ALL {
            let json = serde_json::to_string(&c).expect("serialise");
            let expected = format!("\"{}\"", c.as_str());
            assert_eq!(json, expected, "{c:?}: as_str and serde disagree");
        }
    }

    /// The round trip, so `Deserialize` cannot drift from `Serialize`
    /// either — DTOs on the request side parse this.
    #[test]
    fn the_wire_key_round_trips() {
        for &c in ALL {
            let json = serde_json::to_string(&c).expect("serialise");
            let back: ResourceCategory = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back, c);
        }
    }

    /// Distinct orders, and the documented sparse-hundreds shape.
    ///
    /// Two categories sharing an integer would silently merge in the "Type"
    /// sort AND make the search filter return the other one's rows — the
    /// column is what the filter matches on, not the category.
    #[test]
    fn every_category_has_its_own_order() {
        let mut seen: Vec<(i16, ResourceCategory)> = Vec::new();
        for &c in ALL {
            let o = order_of(c);
            if let Some((_, other)) = seen.iter().find(|(x, _)| *x == o) {
                panic!("{c:?} and {other:?} share order {o}");
            }
            seen.push((o, c));
        }
        // Folders sort first and files never collide with them; the
        // migration's backfill and `copy_folder_tree` both rely on 0 being
        // the folder sentinel.
        assert_eq!(order_of(ResourceCategory::Folder), 0);
        // 1..=99 are reserved for future non-file resource kinds — see
        // docs/architecture/resource-listing.md.
        for &c in ALL {
            let o = order_of(c);
            assert!(
                o == 0 || o >= 100,
                "{c:?} has order {o}, inside the reserved 1..=99 band",
            );
        }
    }

    /// The coarse filter must reach every fine category, or a file becomes
    /// unfindable by type.
    ///
    /// `Folder` is excluded because `SearchTypeFilter::Folder` selects a
    /// different UNION branch rather than a `category_order` value — see
    /// `categories_for_filter`'s last arm.
    #[test]
    fn every_category_is_reachable_from_some_filter() {
        use SearchTypeFilter as F;
        const FILTERS: &[F] = &[
            F::All,
            F::Folder,
            F::Image,
            F::Video,
            F::Audio,
            F::Document,
            F::Archive,
            F::Code,
            F::Font,
            F::Unknown,
        ];
        for &c in ALL {
            if c == ResourceCategory::Folder {
                continue;
            }
            let reachable = FILTERS
                .iter()
                .any(|&f| categories_for_filter(f).contains(&c));
            assert!(
                reachable,
                "{c:?} is in no SearchTypeFilter bucket — files of this \
                 category cannot be found by type",
            );
        }
    }

    /// The integers the SQL binds are derived from the category mapping,
    /// not written twice. This pins that they agree, which is the drift the
    /// old hand-written `[400,500,600,700,1000,1100]` invited.
    #[test]
    fn filter_orders_are_derived_from_the_categories() {
        use SearchTypeFilter as F;
        for &f in &[
            F::Image,
            F::Video,
            F::Audio,
            F::Document,
            F::Archive,
            F::Code,
            F::Unknown,
        ] {
            let from_categories: Vec<i16> = categories_for_filter(f)
                .iter()
                .copied()
                .map(order_of)
                .collect();
            assert_eq!(category_orders_for_filter(f), from_categories, "{f:?}");
        }
        // The two that map to no column value at all.
        assert!(category_orders_for_filter(F::All).is_empty());
        assert!(category_orders_for_filter(F::Folder).is_empty());
    }

    #[test]
    fn text_plain_source_files_are_code() {
        for name in ["a.c", "x.cpp", "t.py", "h.h", "m.cc"] {
            assert_eq!(
                category_for(name, "text/plain"),
                ResourceCategory::Code,
                "{name} stored as text/plain must classify as Code",
            );
        }

        // Still Text when the extension says nothing useful, and when
        // there is no extension at all. `.txt` is Text rather than a
        // word-processor document — it used to be the latter, so the same
        // file was categorised two different ways depending on whether its
        // stored mime happened to be recognised.
        assert_eq!(
            category_for("notes.txt", "text/plain"),
            ResourceCategory::Text
        );
        assert_eq!(category_for("README", "text/plain"), ResourceCategory::Text);
        assert_eq!(
            category_for("data.weird", "text/plain"),
            ResourceCategory::Text
        );

        // Markdown keeps its own bucket rather than collapsing to Code.
        assert_eq!(
            category_for("r.rst", "text/plain"),
            ResourceCategory::Markdown
        );

        // A mime that DOES know better still wins — the extension is a
        // fallback, not an override.
        assert_eq!(category_for("evil.c", "image/png"), ResourceCategory::Image);
    }

    #[test]
    fn test_category_for() {
        // MIME-based
        assert_eq!(category_for("", "image/jpeg"), ResourceCategory::Image);
        assert_eq!(category_for("", "video/webm"), ResourceCategory::Video);
        assert_eq!(category_for("", "audio/ogg"), ResourceCategory::Audio);
        assert_eq!(category_for("", "application/pdf"), ResourceCategory::Pdf);
        assert_eq!(
            category_for("", "application/zip"),
            ResourceCategory::Archive
        );

        // Extension-based fallback
        assert_eq!(
            category_for("main.rs", "application/octet-stream"),
            ResourceCategory::Code
        );
        assert_eq!(
            category_for("photo.jpg", "application/octet-stream"),
            ResourceCategory::Image
        );
        assert_eq!(
            category_for("notes.md", "application/octet-stream"),
            ResourceCategory::Markdown
        );
    }

    /// Every value the closed-set display functions can return must hit
    /// the intern table (same bytes, shared allocation) — a miss is only
    /// a lost optimization, but this test keeps the table in sync.

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
