//! Unified name-conflict resolution policy and candidate name generation.
//!
//! Shared across:
//! - upload conflict resolution (`on_conflict = rename` for files and folders),
//! - share-link mount placement (`ShareMountPgRepository::create_mount_on`),
//! - future trash-restore disambiguation (undelete onto a colliding live name).
//!
//! Convention matches Nextcloud / macOS / PR #791: the existing item stays
//! conceptually `(1)`, and disambiguated copies start at ` (2)`, ` (3)`, … up to
//! [`MAX_CONFLICT_ATTEMPTS`].
//!
//! - Folders always receive the suffix at the end: `Docs` → `Docs (2)`.
//! - Files receive the suffix before their extension (`report.pdf` →
//!   `report (2).pdf`), while protecting dotfiles (`.env` → `.env (2)`) and
//!   preserving common compound archive extensions (`archive.tar.gz` →
//!   `archive (2).tar.gz`).

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::services::path_service::normalize_storage_name;

/// Upper bound on disambiguation attempts (`base`, `base (2)`, …, `base (100)`).
///
/// Matches the 100-attempt cap established in `ShareMountPgRepository::create_mount_on`
/// so a runaway directory cannot trigger unbounded database probes.
pub const MAX_CONFLICT_ATTEMPTS: u32 = 100;

/// How a create/upload endpoint handles a colliding sibling name in the target
/// folder.
///
/// Default is [`ConflictPolicy::Fail`] (`409 AlreadyExists`, preserving the
/// existing idempotent-upload semantics for identical content).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Refuse the operation with `409 AlreadyExists` when a sibling of a
    /// different content/kind already holds the name.
    #[default]
    Fail,
    /// Atomically pick the first free disambiguated sibling name (` (2)`,
    /// ` (3)`, …) and create the new resource there.
    Rename,
}

impl ConflictPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictPolicy::Fail => "fail",
            ConflictPolicy::Rename => "rename",
        }
    }
}

impl FromStr for ConflictPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "fail" => Ok(ConflictPolicy::Fail),
            "rename" => Ok(ConflictPolicy::Rename),
            other => Err(format!(
                "Invalid conflict policy '{other}': expected 'fail' or 'rename'"
            )),
        }
    }
}

/// Whether the candidate name belongs to a file (preserves extension) or a
/// folder (suffixes at the end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    File,
    Folder,
}

/// Common multi-part archive extensions where splitting on the final dot alone
/// would break the logical extension (e.g. `backup.tar (2).gz` instead of
/// `backup (2).tar.gz`).
const COMPOUND_EXTENSIONS: &[&str] = &[
    ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz4", ".tar.br",
];

/// Split a filename into `(stem, extension)` where `extension` includes the
/// leading dot (or `""` when none applies).
///
/// Rules:
/// - Leading-dot hidden files without a secondary extension (`.env`,
///   `.gitignore`) keep the whole name as `stem` (`(".env", "")`).
/// - Dotfiles with a secondary extension (`.env.local`) split after the first
///   segment (`(".env", ".local")`).
/// - Trailing dot (`file.`) is treated as having no extension (`("file.", "")`).
/// - Compound archive extensions (`.tar.gz`, `.tar.xz`, `.tar.bz2`, `.tar.zst`,
///   `.tar.lz4`, `.tar.br`) are matched case-insensitively while preserving the
///   original casing of the input slice, and only when preceded by a non-empty
///   stem other than `"."`.
pub fn split_stem_and_extension(name: &str) -> (&str, &str) {
    let lower = name.to_ascii_lowercase();
    for ext in COMPOUND_EXTENSIONS {
        if lower.ends_with(ext) && name.len() > ext.len() {
            let cut = name.len() - ext.len();
            let stem = &name[..cut];
            if !stem.is_empty() && stem != "." {
                return (stem, &name[cut..]);
            }
        }
    }

    match name.rfind('.') {
        Some(idx) if idx > 0 && idx < name.len() - 1 => (&name[..idx], &name[idx..]),
        _ => (name, ""),
    }
}

/// Produce the `attempt`-th candidate name (1-based) in canonical NFC form.
///
/// - `attempt <= 1` returns the NFC-normalized `base` unmodified.
/// - `attempt >= 2` appends ` ({attempt})` at the end for [`NameKind::Folder`],
///   or right before the extension for [`NameKind::File`].
pub fn conflict_candidate_name(base: &str, kind: NameKind, attempt: u32) -> String {
    let norm = normalize_storage_name(base);
    if attempt <= 1 {
        return norm;
    }
    match kind {
        NameKind::Folder => format!("{norm} ({attempt})"),
        NameKind::File => {
            let (stem, ext) = split_stem_and_extension(&norm);
            format!("{stem} ({attempt}){ext}")
        }
    }
}

/// Lazy iterator yielding candidate names for `attempt = 1..=max_attempts`.
pub fn conflict_candidates(
    base: &str,
    kind: NameKind,
    max_attempts: u32,
) -> impl Iterator<Item = String> {
    let norm = normalize_storage_name(base);
    (1..=max_attempts).map(move |attempt| {
        if attempt <= 1 {
            norm.clone()
        } else {
            match kind {
                NameKind::Folder => format!("{norm} ({attempt})"),
                NameKind::File => {
                    let (stem, ext) = split_stem_and_extension(&norm);
                    format!("{stem} ({attempt}){ext}")
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_candidates_preserve_single_extension() {
        let got: Vec<String> = conflict_candidates("report.pdf", NameKind::File, 3).collect();
        assert_eq!(got, vec!["report.pdf", "report (2).pdf", "report (3).pdf"]);
    }

    #[test]
    fn file_candidates_handle_multiple_dots() {
        assert_eq!(
            conflict_candidate_name("v1.2.report.pdf", NameKind::File, 2),
            "v1.2.report (2).pdf"
        );
    }

    #[test]
    fn file_candidates_preserve_compound_archive_extensions_case_insensitively() {
        assert_eq!(
            conflict_candidate_name("backup.tar.gz", NameKind::File, 2),
            "backup (2).tar.gz"
        );
        assert_eq!(
            conflict_candidate_name("ARCHIVE.TAR.XZ", NameKind::File, 3),
            "ARCHIVE (3).TAR.XZ"
        );
        assert_eq!(
            conflict_candidate_name("pkg.tar.zst", NameKind::File, 2),
            "pkg (2).tar.zst"
        );
        // A bare ".tar.gz" has no non-empty stem before ".tar.gz" — falls back to dotfile rule.
        assert_eq!(
            conflict_candidate_name(".tar.gz", NameKind::File, 2),
            ".tar (2).gz"
        );
    }

    #[test]
    fn file_candidates_protect_dotfiles_and_extensionless_files() {
        assert_eq!(
            conflict_candidate_name(".env", NameKind::File, 2),
            ".env (2)"
        );
        assert_eq!(
            conflict_candidate_name(".gitignore", NameKind::File, 2),
            ".gitignore (2)"
        );
        assert_eq!(
            conflict_candidate_name(".env.local", NameKind::File, 2),
            ".env (2).local"
        );
        assert_eq!(
            conflict_candidate_name("README", NameKind::File, 2),
            "README (2)"
        );
        assert_eq!(
            conflict_candidate_name("file.", NameKind::File, 2),
            "file. (2)"
        );
    }

    #[test]
    fn folder_candidates_match_pr_791_convention() {
        let got: Vec<String> = conflict_candidates("Docs", NameKind::Folder, 3).collect();
        assert_eq!(got, vec!["Docs", "Docs (2)", "Docs (3)"]);

        // Dots in folder names are NOT file extensions.
        assert_eq!(
            conflict_candidate_name("release.v1.0", NameKind::Folder, 2),
            "release.v1.0 (2)"
        );
    }

    #[test]
    fn candidates_nfc_normalize_decomposed_unicode() {
        // NFD: 'e' + combining acute accent U+0301 -> NFC U+00E9 ('é')
        let nfd = "cafe\u{0301}.txt";
        assert_eq!(conflict_candidate_name(nfd, NameKind::File, 1), "café.txt");
        assert_eq!(
            conflict_candidate_name(nfd, NameKind::File, 2),
            "café (2).txt"
        );
    }

    #[test]
    fn conflict_policy_parses_wire_strings() {
        assert_eq!(ConflictPolicy::default(), ConflictPolicy::Fail);
        assert_eq!("fail".parse::<ConflictPolicy>(), Ok(ConflictPolicy::Fail));
        assert_eq!(
            "rename".parse::<ConflictPolicy>(),
            Ok(ConflictPolicy::Rename)
        );
        assert!("overwrite".parse::<ConflictPolicy>().is_err());
    }
}
