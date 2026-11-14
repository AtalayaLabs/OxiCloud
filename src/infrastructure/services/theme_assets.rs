//! The recovery default works without a source checkout or installation path.
use crate::application::ports::theme_ports::PreparedTheme;
use std::{collections::BTreeMap, sync::Arc};

pub fn built_in_themes() -> BTreeMap<String, Arc<PreparedTheme>> {
    [(
        "default",
        include_str!("../../../resources/themes/tokens.css"),
    )]
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
    fn only_default_is_embedded() {
        // Regression pin: an embedded alternative theme cannot be uninstalled
        // by removing its folder. Only the recovery default belongs here.
        let catalogue = built_in_themes();
        assert_eq!(catalogue.len(), 1);
        assert!(!catalogue["default"].css.is_empty());
    }
}
