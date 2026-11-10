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
    use std::io::Write;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(4));
    gzip.write_all(css.as_bytes()).expect("in-memory encoding");
    let gzip = bytes::Bytes::from(gzip.finish().expect("in-memory encoding"));
    let mut brotli = brotli::CompressorWriter::new(Vec::new(), 4096, 4, 22);
    brotli
        .write_all(css.as_bytes())
        .expect("in-memory encoding");
    let brotli = bytes::Bytes::from(brotli.into_inner());
    let etag = format!("W/\"{}\"", blake3::hash(css.as_bytes()).to_hex());
    PreparedTheme {
        id,
        css: bytes::Bytes::from(css),
        gzip,
        brotli,
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
