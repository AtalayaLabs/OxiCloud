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

fn read_theme(directory: &File, id: &str) -> Result<PreparedTheme, DomainError> {
    let file = open_child(directory, "index.css", false)
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
        let root = match open_directory(&self.config.path) {
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
        // Enumerate the anchored descriptor, not a path that could be replaced.
        let mut entries = rustix::fs::Dir::read_from(&root)
            .map_err(|e| DomainError::internal_error("Theme", e.to_string()))?;
        let mut names = BTreeSet::new();
        let mut count = 0;
        for entry in &mut entries {
            let entry = entry.map_err(|e| DomainError::internal_error("Theme", e.to_string()))?;
            let raw = entry.file_name().to_bytes();
            if raw == b"." || raw == b".." {
                continue;
            }
            if entry.file_type() == rustix::fs::FileType::RegularFile {
                continue;
            }
            count += 1;
            if count > MAX_THEMES {
                return Err(invalid("Theme catalogue exceeds 64 entries"));
            }
            let name = std::str::from_utf8(raw)
                .map(str::to_owned)
                .unwrap_or_default();
            names.insert(name);
        }
        for name in names {
            let result = (|| {
                ThemeId::parse(&name)?;
                if name == "default" {
                    return Err(invalid("Reserved theme identifier"));
                }
                let dir = open_child(&root, &name, true)
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
fn open_child(parent: &File, name: &str, directory: bool) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags, openat};
    let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    Ok(File::from(openat(parent, name, flags, Mode::empty())?))
}
fn open_directory(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags, open};

    // `..` stays forbidden. An operator-configured root should NAME a
    // place, not navigate to one, and a value containing `..` is harder
    // to audit than the path it resolves to.
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(std::io::Error::other(
            "Parent traversal in theme root is forbidden",
        ));
    }

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
    Ok(File::from(open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
        Mode::empty(),
    )?))
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
