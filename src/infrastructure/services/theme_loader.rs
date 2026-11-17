//! Read each theme directory’s index.css verbatim, without parsing CSS.
//! Only the operator can install files. Request handlers receive prepared bytes,
//! never paths; every read is bounded and anchored to directory descriptors.
use crate::{
    application::{
        dtos::theme_dto::ThemeDiagnosticDto,
        ports::theme_ports::{PreparedTheme, ThemeCatalogue, ThemeCatalogueLoader},
    },
    common::{config::ThemesConfig, errors::DomainError},
    domain::entities::theme::ThemeId,
};
use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Component, Path},
    sync::Arc,
};
const MAX_FILE: u64 = 256 * 1024;
const MAX_THEMES: usize = 64;
fn invalid(message: impl Into<String>) -> DomainError {
    DomainError::validation_error(message)
}

fn read_theme(directory: &fsx::Dir, id: &str) -> Result<PreparedTheme, DomainError> {
    let file = fsx::open_child_file(directory, "index.css")
        .map_err(|e| invalid(format!("{id}/index.css: {e}")))?;
    if !file
        .metadata()
        .map_err(|e| invalid(e.to_string()))?
        .is_file()
    {
        return Err(invalid("Theme CSS must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| invalid(format!("{id}/index.css: {e}")))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(invalid("Theme CSS exceeds 256 KiB"));
    }
    let css = String::from_utf8(bytes).map_err(|_| invalid("Theme CSS must be UTF-8"))?;
    Ok(super::theme_assets::prepare_theme(id.into(), css))
}

pub struct ThemeLoader {
    pub config: ThemesConfig,
}
impl ThemeCatalogueLoader for ThemeLoader {
    fn load(&self) -> Result<ThemeCatalogue, DomainError> {
        let mut themes = super::theme_assets::built_in_themes();
        if self.config.force_default {
            return Ok(ThemeCatalogue {
                themes,
                diagnostics: Vec::new(),
            });
        }
        let mut diagnostics = Vec::new();
        let root = match fsx::open_root(&self.config.path) {
            Ok(root) => root,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.config.path_explicit => {
                return Ok(ThemeCatalogue {
                    themes,
                    diagnostics,
                });
            }
            Err(e) => {
                return Err(DomainError::internal_error(
                    "Theme",
                    format!("Cannot open theme root: {e}"),
                ));
            }
        };
        let mut names = BTreeSet::new();
        let mut count = 0;
        fsx::for_each_entry(&root, |raw, is_regular_file| {
            if raw == b"." || raw == b".." || is_regular_file {
                return Ok(());
            }
            count += 1;
            if count > MAX_THEMES {
                return Err(invalid("Theme catalogue exceeds 64 entries"));
            }
            let name = std::str::from_utf8(raw)
                .map(str::to_owned)
                .unwrap_or_default();
            names.insert(name);
            Ok(())
        })?;
        for name in names {
            let result = (|| {
                ThemeId::parse(&name)?;
                if name == "default" {
                    return Err(invalid("Reserved theme identifier"));
                }
                let dir = fsx::open_child_dir(&root, &name)
                    .map_err(|e| invalid(format!("Theme directory unavailable: {e}")))?;
                read_theme(&dir, &name)
            })();
            match result {
                Ok(theme) => {
                    themes.insert(name, Arc::new(theme));
                }
                Err(error) => {
                    tracing::warn!(theme_id = %name, %error, "Excluding invalid theme");
                    diagnostics.push(ThemeDiagnosticDto {
                        theme_id: ThemeId::parse(&name).ok().map(|id| id.as_str().to_owned()),
                        reason: "invalid_theme".into(),
                    });
                }
            }
        }
        Ok(ThemeCatalogue {
            themes,
            diagnostics,
        })
    }
}
/// `..` stays forbidden. An operator-configured root should NAME a place,
/// not navigate to one, and a value containing `..` is harder to audit
/// than the path it resolves to.
fn refuse_parent_traversal(path: &Path) -> std::io::Result<()> {
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(std::io::Error::other(
            "Parent traversal in theme root is forbidden",
        ));
    }
    Ok(())
}

/// Platform file access for the loader. Both halves give the same three
/// guarantees: entries are enumerated from the opened root, a theme
/// directory or `index.css` that is a symlink is refused, and nothing but
/// a directory / regular file is ever read.
#[cfg(unix)]
mod fsx {
    use super::{DomainError, File, Path, refuse_parent_traversal};
    use rustix::fs::{FileType, Mode, OFlags, open, openat};

    /// An open directory descriptor: every child is opened relative to it.
    pub struct Dir(File);

    fn open_child(parent: &Dir, name: &str, directory: bool) -> std::io::Result<File> {
        let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        if directory {
            flags |= OFlags::DIRECTORY;
        }
        Ok(File::from(openat(&parent.0, name, flags, Mode::empty())?))
    }
    pub fn open_child_file(parent: &Dir, name: &str) -> std::io::Result<File> {
        open_child(parent, name, false)
    }
    pub fn open_child_dir(parent: &Dir, name: &str) -> std::io::Result<Dir> {
        open_child(parent, name, true).map(Dir)
    }

    /// Enumerate the anchored descriptor, not a path that could be replaced.
    pub fn for_each_entry(
        dir: &Dir,
        mut visit: impl FnMut(&[u8], bool) -> Result<(), DomainError>,
    ) -> Result<(), DomainError> {
        let mut entries = rustix::fs::Dir::read_from(&dir.0)
            .map_err(|e| DomainError::internal_error("Theme", e.to_string()))?;
        for entry in &mut entries {
            let entry = entry.map_err(|e| DomainError::internal_error("Theme", e.to_string()))?;
            visit(
                entry.file_name().to_bytes(),
                entry.file_type() == FileType::RegularFile,
            )?;
        }
        Ok(())
    }

    pub fn open_root(path: &Path) -> std::io::Result<Dir> {
        refuse_parent_traversal(path)?;
        // Open the configured root directly instead of walking its
        // components with `NOFOLLOW`.
        //
        // The walk refused a symlink in ANY ancestor, which is not portable:
        // on macOS `/var` is itself a symlink (`/var -> private/var`) and
        // `TMPDIR` lives under it, so every path below `/var` — including
        // every `tempfile::tempdir()` — failed with ENOTDIR before the root
        // was ever reached. Linux `/tmp` is a real directory, so CI never saw
        // it. Nor is there a portable primitive for the strict policy:
        // `openat2(RESOLVE_NO_SYMLINKS)` is Linux 5.6+, `O_RESOLVE_BENEATH`
        // is FreeBSD, macOS has neither. `O_DIRECTORY` is POSIX-2008 and
        // works everywhere we run, still yielding ENOTDIR when the path is
        // genuinely not a directory.
        //
        // The guard that matters is untouched. The root is operator
        // configuration — deliberately chosen, trusted input — whereas the
        // theme directories and `index.css` files UNDER it are whatever
        // happened to land in a shared folder, and those still go through
        // `open_child` with `NOFOLLOW`. A symlinked theme or a symlinked CSS
        // file is refused exactly as before; a symlinked ancestor of the
        // operator's own root no longer takes the whole feature down.
        Ok(Dir(File::from(open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
            Mode::empty(),
        )?)))
    }
}

/// Windows has no `openat`, so the anchor is a handle instead of a
/// descriptor: every directory is held open WITHOUT `FILE_SHARE_DELETE`,
/// which makes NTFS refuse to rename, replace or delete it (or any
/// ancestor) while the loader reads below it. Children are opened by path
/// under that pinned directory.
///
/// Symlinks AND junctions are reparse points. `FILE_FLAG_OPEN_REPARSE_POINT`
/// opens the link itself instead of its target, and the attribute is then
/// checked on the opened handle — no check-then-open gap. FIFOs have no
/// filesystem namespace on Windows (named pipes live under `\\.\pipe\`),
/// so there is no `NONBLOCK` equivalent to carry over.
#[cfg(windows)]
mod fsx {
    use super::{DomainError, File, Path, refuse_parent_traversal};
    use std::{
        fs::OpenOptions,
        io,
        os::windows::fs::{MetadataExt, OpenOptionsExt},
        path::PathBuf,
    };

    // winnt.h / winbase.h values; std exposes the hooks but not the names.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000; // required to open a directory
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    /// A pinned directory: the handle keeps the path from being swapped.
    pub struct Dir {
        _handle: File,
        path: PathBuf,
    }

    fn open(path: &Path, directory: bool, follow: bool) -> io::Result<File> {
        let mut flags = if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
        if !follow {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(flags)
            .open(path)?;
        let meta = file.metadata()?;
        if !follow && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other("symbolic link or junction refused"));
        }
        if directory && !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "not a directory",
            ));
        }
        Ok(file)
    }
    pub fn open_child_file(parent: &Dir, name: &str) -> io::Result<File> {
        open(&parent.path.join(name), false, false)
    }
    pub fn open_child_dir(parent: &Dir, name: &str) -> io::Result<Dir> {
        let path = parent.path.join(name);
        Ok(Dir {
            _handle: open(&path, true, false)?,
            path,
        })
    }

    pub fn for_each_entry(
        dir: &Dir,
        mut visit: impl FnMut(&[u8], bool) -> Result<(), DomainError>,
    ) -> Result<(), DomainError> {
        let entries = std::fs::read_dir(&dir.path)
            .map_err(|e| DomainError::internal_error("Theme", e.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|e| DomainError::internal_error("Theme", e.to_string()))?;
            let kind = entry
                .file_type()
                .map_err(|e| DomainError::internal_error("Theme", e.to_string()))?;
            // A non-UTF-8 name becomes "" and fails `ThemeId::parse`,
            // exactly like the POSIX path's `unwrap_or_default`.
            let name = entry.file_name();
            visit(
                name.to_str().map(str::as_bytes).unwrap_or_default(),
                kind.is_file() && !kind.is_symlink(),
            )?;
        }
        Ok(())
    }

    /// Like the POSIX root: operator configuration, so a symlinked root
    /// (or ancestor) is followed; only the entries below it are untrusted.
    pub fn open_root(path: &Path) -> io::Result<Dir> {
        refuse_parent_traversal(path)?;
        Ok(Dir {
            _handle: open(path, true, true)?,
            path: path.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn loader(path: &Path) -> ThemeLoader {
        ThemeLoader {
            config: ThemesConfig {
                path: path.to_owned(),
                path_explicit: true,
                force_default: false,
            },
        }
    }
    fn fixture(css: &str) -> (tempfile::TempDir, ThemeCatalogue) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sober")).unwrap();
        std::fs::write(dir.path().join("sober/index.css"), css).unwrap();
        let catalogue = loader(dir.path()).load().unwrap();
        (dir, catalogue)
    }
    #[test]
    fn operator_css_is_served_verbatim_including_component_rules() {
        // Regression pin: the old token validator rejected component CSS and
        // rewrote values. Operator files now remain byte-for-byte unchanged.
        let css = ":root { --custom: color-mix(in srgb, red, blue); }\n.button { border-radius: 2rem !important; }\n";
        let (_dir, catalogue) = fixture(css);
        assert_eq!(catalogue.themes["sober"].css.as_ref(), css.as_bytes());
        assert_eq!(catalogue.themes.len(), 2);
    }
    // POSIX-only: `std::os::unix::fs::symlink` and `rustix::fs::mknodat`
    // do not exist on Windows. The Windows counterpart is
    // `a_junctioned_theme_directory_is_excluded` below.
    #[cfg(unix)]
    #[test]
    fn symlinks_special_files_and_oversized_files_are_excluded() {
        use std::os::unix::fs::symlink;
        let (dir, _) = fixture(":root {}");
        let css = dir.path().join("sober/index.css");
        std::fs::remove_file(&css).unwrap();
        symlink("/etc/passwd", &css).unwrap();
        assert_eq!(loader(dir.path()).load().unwrap().themes.len(), 1);
        std::fs::remove_file(&css).unwrap();
        // The FIFO leg is Linux-only. `rustix::fs::mknodat` is declared
        // `#[cfg(not(any(apple, …)))]`, so referencing it on macOS is a
        // COMPILE error, not a skipped assertion — it took the whole
        // `cargo test --lib` binary down for every contributor on a Mac
        // while CI (ubuntu) stayed green. The symlink and oversized-file
        // legs around it are portable and still run everywhere.
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        {
            rustix::fs::mknodat(
                rustix::fs::CWD,
                &css,
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::RUSR,
                0,
            )
            .unwrap();
            assert_eq!(loader(dir.path()).load().unwrap().themes.len(), 1);
            std::fs::remove_file(&css).unwrap();
        }
        std::fs::write(css, vec![b' '; MAX_FILE as usize + 1]).unwrap();
        assert_eq!(loader(dir.path()).load().unwrap().themes.len(), 1);
    }
    #[cfg(unix)]
    #[test]
    fn catalogue_limits_utf8_and_root_symlinks_fail_loudly() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        for i in 0..65 {
            std::fs::create_dir(dir.path().join(format!("theme-{i}"))).unwrap();
        }
        assert!(loader(dir.path()).load().is_err());
        let (other, _) = fixture(":root {}");
        std::fs::write(other.path().join("sober/index.css"), [0xff]).unwrap();
        assert_eq!(loader(other.path()).load().unwrap().diagnostics.len(), 1);
        let root_link = other.path().join("link");
        symlink(dir.path(), &root_link).unwrap();
        assert!(loader(&root_link).load().is_err());
    }
    #[test]
    fn oversized_and_non_utf8_css_are_excluded_and_the_catalogue_is_capped() {
        // Portable half of the two POSIX-only tests above, so the size,
        // encoding and count limits are also pinned on Windows.
        let (dir, _) = fixture(":root {}");
        let css = dir.path().join("sober/index.css");
        std::fs::write(&css, vec![b' '; MAX_FILE as usize + 1]).unwrap();
        assert_eq!(loader(dir.path()).load().unwrap().themes.len(), 1);
        std::fs::write(&css, [0xff]).unwrap();
        assert_eq!(loader(dir.path()).load().unwrap().diagnostics.len(), 1);
        let many = tempfile::tempdir().unwrap();
        for i in 0..65 {
            std::fs::create_dir(many.path().join(format!("theme-{i}"))).unwrap();
        }
        assert!(loader(many.path()).load().is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_junctioned_theme_directory_is_excluded() {
        // Regression pin for the Windows loader: a theme directory that is
        // a reparse point must be refused like a symlink on POSIX
        // (`NOFOLLOW`), or a junction dropped into a shared themes folder
        // would make the loader serve CSS from anywhere on the host.
        // Junctions need no privilege (unlike `symlink_dir`), so this runs
        // on every Windows machine.
        let (dir, _) = fixture(":root {}");
        let target = dir.path().join("sober");
        let link = dir.path().join("linked");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "mklink /J failed");
        let catalogue = loader(dir.path()).load().unwrap();
        assert!(catalogue.themes.contains_key("sober"));
        assert!(!catalogue.themes.contains_key("linked"));
        assert_eq!(catalogue.diagnostics[0].theme_id.as_deref(), Some("linked"));
    }

    #[test]
    fn only_index_css_is_loaded_without_parsing_or_reading_siblings() {
        // Regression pin: the loader must serve index.css unchanged, even when
        // it mentions a missing import; sibling files are never read or merged.
        let css = "@import './missing.css';\n/* arbitrary CSS */\n.x { --custom: value; }\n";
        let (root, catalogue) = fixture(css);
        assert_eq!(catalogue.themes["sober"].css.as_ref(), css.as_bytes());
        std::fs::write(root.path().join("sober/extra.css"), [0xff]).unwrap();
        assert_eq!(
            loader(root.path()).load().unwrap().themes["sober"]
                .css
                .as_ref(),
            css.as_bytes()
        );
        std::fs::remove_file(root.path().join("sober/index.css")).unwrap();
        std::fs::write(root.path().join("sober/theme.css"), ":root {}").unwrap();
        let missing = loader(root.path()).load().unwrap();
        assert!(!missing.themes.contains_key("sober"));
        assert_eq!(missing.diagnostics[0].theme_id.as_deref(), Some("sober"));
    }

    #[test]
    fn root_files_are_ignored_and_slate_can_be_uninstalled() {
        // Regression pin: root metadata is not a theme, and deleting Slate's
        // folder must remove it instead of restoring an embedded alternative.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("tokens.css"), ":root {}").unwrap();
        std::fs::write(root.path().join("NOTICE.md"), "Not a theme").unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/themes/slate/index.css");
        std::fs::create_dir(root.path().join("slate")).unwrap();
        std::fs::copy(&source, root.path().join("slate/index.css")).unwrap();
        let catalogue = loader(root.path()).load().unwrap();
        assert!(catalogue.diagnostics.is_empty());
        assert_eq!(
            catalogue.themes["slate"].css.as_ref(),
            std::fs::read(source).unwrap()
        );
        std::fs::remove_dir_all(root.path().join("slate")).unwrap();
        assert_eq!(loader(root.path()).load().unwrap().themes.len(), 1);
    }

    #[test]
    fn recovery_ignores_an_unavailable_operator_directory() {
        let root = tempfile::tempdir().unwrap();
        let mut loader = loader(&root.path().join("absent"));
        loader.config.force_default = true;
        assert!(loader.load().unwrap().themes.contains_key("default"));
    }
    #[test]
    fn only_default_is_available_without_the_source_tree() {
        let root = tempfile::tempdir().unwrap();
        let mut loader = loader(&root.path().join("absent"));
        loader.config.path_explicit = false;
        let catalogue = loader.load().unwrap();
        assert_eq!(catalogue.themes.len(), 1);
        assert!(catalogue.themes.contains_key("default"));
    }
    #[test]
    fn distinguishes_missing_explicit_root_and_reserved_default() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");
        assert!(loader(&missing).load().is_err());
        let mut implicit = loader(&missing);
        implicit.config.path_explicit = false;
        assert_eq!(implicit.load().unwrap().themes.len(), 1);
        std::fs::create_dir(dir.path().join("default")).unwrap();
        assert_eq!(loader(dir.path()).load().unwrap().diagnostics.len(), 1);
    }
}
