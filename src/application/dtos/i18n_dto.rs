use crate::domain::services::i18n_service::Locale;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// DTO for locale information
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct LocaleDto {
    /// Locale code (e.g., "en", "es")
    pub code: String,

    /// Locale name in its own language (e.g., "English", "Español")
    pub name: String,
}

impl From<Locale> for LocaleDto {
    fn from(locale: Locale) -> Self {
        Self::from(&locale)
    }
}

impl From<&Locale> for LocaleDto {
    fn from(locale: &Locale) -> Self {
        let code = locale.as_str().to_string();
        let name = display_name_for(&code)
            .map(str::to_string)
            .unwrap_or_else(|| code.clone());
        Self { code, name }
    }
}

/// Endonym lookup for the locales shipped under `static/locales/`. New
/// locales added in PR-A's `LocaleRegistry::discover` should be added
/// here too; an unknown code falls back to itself, which is safe but
/// looks rough in a language switcher.
fn display_name_for(code: &str) -> Option<&'static str> {
    match code {
        "en" => Some("English"),
        "es" => Some("Español"),
        "fr" => Some("Français"),
        "de" => Some("Deutsch"),
        "pt" => Some("Português"),
        "it" => Some("Italiano"),
        "nl" => Some("Nederlands"),
        "pl" => Some("Polski"),
        "ru" => Some("Русский"),
        "ja" => Some("日本語"),
        "ko" => Some("한국어"),
        "zh" => Some("中文"),
        "zh-TW" => Some("繁體中文"),
        "ar" => Some("العربية"),
        "fa" => Some("فارسی"),
        "hi" => Some("हिन्दी"),
        _ => None,
    }
}

/// DTO for translation request
#[derive(Debug, Deserialize, ToSchema)]
pub struct TranslationRequestDto {
    /// The translation key
    pub key: String,

    /// The locale code (optional, defaults to "en")
    pub locale: Option<String>,
}

/// DTO for translation response
#[derive(Debug, Serialize, ToSchema)]
pub struct TranslationResponseDto {
    /// The translation key
    pub key: String,

    /// The locale code used for translation
    pub locale: String,

    /// The translated text
    pub text: String,
}

/// DTO for translation error
#[derive(Debug, Serialize, ToSchema)]
pub struct TranslationErrorDto {
    /// The translation key that was not found
    pub key: String,

    /// The locale code used for translation
    pub locale: String,

    /// The error message
    pub error: String,
}
