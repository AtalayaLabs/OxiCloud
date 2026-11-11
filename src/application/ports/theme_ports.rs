//! Prepared theme data crosses the loading boundary without CSS-parser types.
use crate::{application::dtos::theme_dto::ThemeDiagnosticDto, common::errors::DomainError};
use bytes::Bytes;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug)]
pub struct PreparedTheme {
    pub id: String,
    pub css: Bytes,
    pub etag: String,
}

#[derive(Debug)]
pub struct ThemeCatalogue {
    pub themes: BTreeMap<String, Arc<PreparedTheme>>,
    pub diagnostics: Vec<ThemeDiagnosticDto>,
}
pub trait ThemeCatalogueLoader {
    fn load(&self) -> Result<ThemeCatalogue, DomainError>;
}
