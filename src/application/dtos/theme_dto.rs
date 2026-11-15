//! Public administrative contracts contain diagnostics, never CSS or local paths.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ThemeDiagnosticDto {
    pub theme_id: Option<String>,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AppearanceDto {
    pub available_themes: Vec<String>,
    pub default_theme_id: String,
    pub selected_theme_id: Option<String>,
    pub active_theme_id: String,
    pub warning: Option<String>,
    pub theme_diagnostics: Vec<ThemeDiagnosticDto>,
}
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectThemeDto {
    pub theme_id: String,
}
