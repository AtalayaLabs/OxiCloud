use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use std::sync::Arc;

use crate::application::dtos::i18n_dto::{
    LocaleDto, TranslationErrorDto, TranslationRequestDto, TranslationResponseDto,
};
use crate::application::services::i18n_application_service::I18nApplicationService;
use crate::domain::services::i18n_service::{I18nError, Locale};

type AppState = Arc<I18nApplicationService>;

/// Handler for i18n-related API endpoints
pub struct I18nHandler;

impl I18nHandler {
    // ── Why no #[utoipa::path] here? ─────────────────────────────────────────────
    // utoipa 5.4.0's proc macro generates helper structs / impls inside its expansion.
    // Rust allows struct definitions at module scope but forbids them inside impl blocks,
    // so `#[utoipa::path]` fails on every method in this impl block regardless of HTTP
    // verb or annotation content. All route handlers are free functions below.
    // TODO: collapse after utoipa upgrade.
    pub(super) async fn get_locales_impl(State(service): State<AppState>) -> impl IntoResponse {
        let locales = service.available_locales().await;
        let locale_dtos: Vec<LocaleDto> = locales.into_iter().map(LocaleDto::from).collect();

        (StatusCode::OK, Json(locale_dtos)).into_response()
    }

    /// Translates a single key to the requested locale.
    pub(super) async fn translate_impl(
        State(service): State<AppState>,
        Query(query): Query<TranslationRequestDto>,
    ) -> impl IntoResponse {
        let locale = match &query.locale {
            Some(locale_str) => match Locale::from_code(locale_str) {
                Some(locale) => Some(locale),
                None => {
                    let error = TranslationErrorDto {
                        key: query.key.clone(),
                        locale: locale_str.clone(),
                        error: format!("Unsupported locale: {}", locale_str),
                    };
                    return (StatusCode::BAD_REQUEST, Json(error)).into_response();
                }
            },
            None => None,
        };

        match service.translate(&query.key, locale).await {
            Ok(text) => {
                let response = TranslationResponseDto {
                    key: query.key,
                    locale: locale.unwrap_or(Locale::default()).as_str().to_string(),
                    text,
                };
                (StatusCode::OK, Json(response)).into_response()
            }
            Err(err) => {
                let (status, error_msg) = match &err {
                    I18nError::KeyNotFound(_) => (StatusCode::NOT_FOUND, err.to_string()),
                    I18nError::InvalidLocale(_) => (StatusCode::BAD_REQUEST, err.to_string()),
                    I18nError::LoadError(_) => {
                        tracing::error!("I18n load error: {}", err);
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Translation loading error".to_string(),
                        )
                    }
                };

                let error = TranslationErrorDto {
                    key: query.key,
                    locale: locale.unwrap_or(Locale::default()).as_str().to_string(),
                    error: error_msg,
                };

                (status, Json(error)).into_response()
            }
        }
    }

    /// Returns all translations for a locale as a flat key→value object.
    pub(super) async fn get_translations_by_locale_impl(
        State(service): State<AppState>,
        Path(locale_code): Path<String>,
    ) -> impl IntoResponse {
        Self::get_translations(State(service), locale_code).await
    }

    /// Gets all translations for a locale
    pub async fn get_translations(
        State(_service): State<AppState>,
        locale_code: String,
    ) -> impl IntoResponse {
        let locale = match Locale::from_code(&locale_code) {
            Some(locale) => locale,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("Unsupported locale: {}", locale_code)
                    })),
                )
                    .into_response();
            }
        };

        // This implementation is a bit weird, as we don't have a way to get all translations
        // We should improve the I18nService to support this
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "locale": locale.as_str()
            })),
        )
            .into_response()
    }
}

// ── Route handlers (free functions) ──────────────────────────────────────────
//
// All three route functions live here rather than as methods on I18nHandler
// because utoipa 5.4.0's #[utoipa::path] macro generates helper structs inside
// its expansion. Rust allows struct definitions at module scope but forbids them
// inside impl blocks — so every #[utoipa::path] annotation on an I18nHandler
// method fails to compile regardless of HTTP verb or annotation content.
//
// All logic lives in the I18nHandler::*_impl methods above; these thin wrappers
// exist solely to carry the OpenAPI annotation at a scope where utoipa can
// generate its helper types.
//
// routes.rs calls these free functions directly.
// TODO: collapse back into the impl block after a utoipa upgrade resolves the issue.

#[utoipa::path(
    get,
    path = "/api/i18n/locales",
    responses(
        (status = 200, description = "Available locales", body = Vec<LocaleDto>),
    ),
    tag = "i18n"
)]
pub async fn get_locales(state: State<AppState>) -> impl IntoResponse {
    I18nHandler::get_locales_impl(state).await
}

#[utoipa::path(
    get,
    path = "/api/i18n/translate",
    params(
        ("key" = String, Query, description = "Translation key"),
        ("locale" = Option<String>, Query, description = "Target locale code (defaults to en)"),
    ),
    responses(
        (status = 200, description = "Translation", body = TranslationResponseDto),
        (status = 400, description = "Unsupported locale", body = TranslationErrorDto),
        (status = 404, description = "Key not found"),
    ),
    tag = "i18n"
)]
pub async fn translate(
    state: State<AppState>,
    query: Query<TranslationRequestDto>,
) -> impl IntoResponse {
    I18nHandler::translate_impl(state, query).await
}

#[utoipa::path(
    get,
    path = "/api/i18n/locales/{locale_code}",
    params(("locale_code" = String, Path, description = "Locale code, e.g. en, fr, de")),
    responses(
        (status = 200, description = "All translations for this locale"),
        (status = 400, description = "Unsupported locale"),
    ),
    tag = "i18n"
)]
pub async fn get_translations_by_locale(
    state: State<AppState>,
    path: Path<String>,
) -> impl IntoResponse {
    I18nHandler::get_translations_by_locale_impl(state, path).await
}
