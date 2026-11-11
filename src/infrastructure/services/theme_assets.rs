//! Shipped themes work without a source checkout or writable installation path.
use crate::application::ports::theme_ports::PreparedTheme;
use std::{collections::BTreeMap, sync::Arc};

pub fn built_in_themes() -> BTreeMap<String, Arc<PreparedTheme>> {
    [
        (
            "default",
            include_str!("../../../resources/themes/tokens.css"),
        ),
        (
            "modern-pro",
            include_str!("../../../resources/themes/installed/modern-pro/theme.css"),
        ),
    ]
    .into_iter()
    .map(|(id, css)| (id.into(), Arc::new(prepare_theme(id.into(), css.into()))))
    .collect()
}

pub fn prepare_theme(id: String, css: String) -> PreparedTheme {
    let etag = format!("W/\"{}\"", blake3::hash(css.as_bytes()).to_hex());
    PreparedTheme {
        id,
        css: bytes::Bytes::from(css),
        etag,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shipped_catalogue_contains_default_and_modern_pro_without_disk_access() {
        // Distribution regression pin: bundled-assets alone did not include
        // the operator theme directory; Modern Pro disappeared outside git.
        let catalogue = built_in_themes();
        assert!(catalogue.contains_key("default"));
        assert!(!catalogue["modern-pro"].css.is_empty());
    }
}
