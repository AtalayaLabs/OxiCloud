use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::json;
use tracing::{error, info};

use crate::application::dtos::search_dto::{
    SearchCriteriaDto, SearchResultsDto, SearchSuggestionsDto,
};
use crate::application::ports::inbound::SearchUseCase;
use crate::common::di::AppState;
use crate::interfaces::middleware::auth::AuthUser;
use std::sync::Arc;

/**
 * Handler for search operations through the API.
 *
 * All search processing (filtering, scoring, sorting, categorization,
 * formatting) is performed server-side. These handlers are thin HTTP
 * adapters that delegate to the SearchUseCase.
 */
pub struct SearchHandler;

impl SearchHandler {
    // ── Why no #[utoipa::path] here? ─────────────────────────────────────────────
    // utoipa 5.4.0's proc macro generates helper structs / impls inside its expansion.
    // Rust allows struct definitions at module scope but forbids them inside impl blocks,
    // so `#[utoipa::path]` fails on every method in this impl block regardless of HTTP
    // verb or annotation content. All route handlers are free functions below.
    // TODO: collapse after utoipa upgrade.
    pub(super) async fn search_files_get_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
        Query(params): Query<SearchParams>,
    ) -> impl IntoResponse {
        info!("API: File search with parameters: {:?}", params);

        let search_service = match &state.applications.search_service {
            Some(service) => service,
            None => {
                error!("Search service not available");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "Search service is not available" })),
                )
                    .into_response();
            }
        };

        let search_criteria = SearchCriteriaDto {
            name_contains: params.query,
            file_types: params
                .type_filter
                .map(|t| t.split(',').map(|s| s.trim().to_string()).collect()),
            created_after: params.created_after,
            created_before: params.created_before,
            modified_after: params.modified_after,
            modified_before: params.modified_before,
            min_size: params.min_size,
            max_size: params.max_size,
            folder_id: params.folder_id,
            recursive: params.recursive.unwrap_or(true),
            limit: params.limit.unwrap_or(100),
            offset: params.offset.unwrap_or(0),
            sort_by: params.sort_by.unwrap_or_else(|| "relevance".to_string()),
        };

        match search_service.search(search_criteria, auth_user.id).await {
            Ok(results) => {
                info!(
                    "Search completed in {}ms — {} files, {} folders",
                    results.query_time_ms,
                    results.files.len(),
                    results.folders.len()
                );
                (StatusCode::OK, Json(&*results)).into_response()
            }
            Err(err) => {
                error!("Search error: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Search error" })),
                )
                    .into_response()
            }
        }
    }

    /// Advanced search with full criteria in the request body.
    pub(super) async fn search_files_post_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
        Json(criteria): Json<SearchCriteriaDto>,
    ) -> impl IntoResponse {
        info!("API: Advanced file search");

        let search_service = match &state.applications.search_service {
            Some(service) => service,
            None => {
                error!("Search service not available");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "Search service is not available" })),
                )
                    .into_response();
            }
        };

        match search_service.search(criteria, auth_user.id).await {
            Ok(results) => {
                info!(
                    "Advanced search completed in {}ms — {} files, {} folders",
                    results.query_time_ms,
                    results.files.len(),
                    results.folders.len()
                );
                (StatusCode::OK, Json(&*results)).into_response()
            }
            Err(err) => {
                error!("Search error: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Search error" })),
                )
                    .into_response()
            }
        }
    }

    /// Autocomplete suggestions for search.
    pub(super) async fn suggest_files_impl(
        State(state): State<Arc<AppState>>,
        Query(params): Query<SuggestParams>,
    ) -> impl IntoResponse {
        info!("API: Search suggestions for {:?}", params.query);

        let search_service = match &state.applications.search_service {
            Some(service) => service,
            None => {
                error!("Search service not available");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "Search service is not available" })),
                )
                    .into_response();
            }
        };

        let limit = params.limit.unwrap_or(10).min(20);

        match search_service
            .suggest(&params.query, params.folder_id.as_deref(), limit)
            .await
        {
            Ok(suggestions) => {
                info!(
                    "Suggestions completed in {}ms — {} results",
                    suggestions.query_time_ms,
                    suggestions.suggestions.len()
                );
                (StatusCode::OK, Json(suggestions)).into_response()
            }
            Err(err) => {
                error!("Suggestions error: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Suggestions error" })),
                )
                    .into_response()
            }
        }
    }

    /// DELETE /search/cache — clears the search results cache.
    pub(super) async fn clear_search_cache_impl(
        State(state): State<Arc<AppState>>,
    ) -> impl IntoResponse {
        info!("API: Clearing search cache");

        let search_service = match &state.applications.search_service {
            Some(service) => service,
            None => {
                error!("Search service not available");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "Search service is not available" })),
                )
                    .into_response();
            }
        };

        match search_service.clear_search_cache().await {
            Ok(_) => {
                info!("Search cache cleared successfully");
                (
                    StatusCode::OK,
                    Json(json!({ "message": "Search cache cleared successfully" })),
                )
                    .into_response()
            }
            Err(err) => {
                error!("Error clearing search cache: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Error clearing search cache" })),
                )
                    .into_response()
            }
        }
    }
}

/// Search parameters for the GET /search endpoint
#[derive(Debug, serde::Deserialize)]
pub struct SearchParams {
    /// Text to search in file and folder names
    pub query: Option<String>,

    /// Filter by file types (comma-separated extensions)
    #[serde(rename = "type")]
    pub type_filter: Option<String>,

    /// Created after this timestamp
    pub created_after: Option<u64>,

    /// Created before this timestamp
    pub created_before: Option<u64>,

    /// Modified after this timestamp
    pub modified_after: Option<u64>,

    /// Modified before this timestamp
    pub modified_before: Option<u64>,

    /// Minimum file size in bytes
    pub min_size: Option<u64>,

    /// Maximum file size in bytes
    pub max_size: Option<u64>,

    /// Folder ID to limit the search scope
    pub folder_id: Option<String>,

    /// Recursive search in subfolders (default: true)
    pub recursive: Option<bool>,

    /// Result limit for pagination
    pub limit: Option<usize>,

    /// Offset for pagination
    pub offset: Option<usize>,

    /// Sort order: relevance | name | name_desc | date | date_desc | size | size_desc
    pub sort_by: Option<String>,
}

/// Parameters for the GET /search/suggest endpoint
#[derive(Debug, serde::Deserialize)]
pub struct SuggestParams {
    /// Text to search for suggestions
    pub query: String,

    /// Folder ID to limit the suggestion scope
    pub folder_id: Option<String>,

    /// Maximum number of suggestions (default 10, max 20)
    pub limit: Option<usize>,
}

// ── Route handlers (free functions) ──────────────────────────────────────────
//
// All four route functions live here rather than as methods on SearchHandler
// because utoipa 5.4.0's #[utoipa::path] macro generates helper structs inside
// its expansion. Rust allows struct definitions at module scope but forbids them
// inside impl blocks — so every #[utoipa::path] annotation on a SearchHandler
// method fails to compile regardless of HTTP verb or annotation content.
//
// All logic lives in the SearchHandler::*_impl methods above; these thin wrappers
// exist solely to carry the OpenAPI annotation at a scope where utoipa can
// generate its helper types.
//
// routes.rs calls these free functions directly.
// TODO: collapse back into the impl block after a utoipa upgrade resolves the issue.

#[utoipa::path(
    get,
    path = "/api/search",
    params(
        ("query" = Option<String>, Query, description = "Text to search in names"),
        ("type" = Option<String>, Query, description = "Comma-separated MIME type filter"),
        ("folder_id" = Option<String>, Query, description = "Restrict search to this folder"),
        ("recursive" = Option<bool>, Query, description = "Include sub-folders"),
        ("limit" = Option<u32>, Query, description = "Max results"),
        ("offset" = Option<u32>, Query, description = "Pagination offset"),
    ),
    responses(
        (status = 200, description = "Search results", body = SearchResultsDto),
        (status = 503, description = "Search service unavailable"),
    ),
    tag = "search"
)]
pub async fn search_files_get(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    query: Query<SearchParams>,
) -> impl IntoResponse {
    SearchHandler::search_files_get_impl(state, auth_user, query).await
}

#[utoipa::path(
    post,
    path = "/api/search/advanced",
    request_body(content = SearchCriteriaDto, content_type = "application/json", description = "Search criteria"),
    responses(
        (status = 200, description = "Search results", body = SearchResultsDto),
        (status = 503, description = "Search service unavailable"),
    ),
    tag = "search"
)]
pub async fn search_files_post(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    json: Json<SearchCriteriaDto>,
) -> impl IntoResponse {
    SearchHandler::search_files_post_impl(state, auth_user, json).await
}

#[utoipa::path(
    get,
    path = "/api/search/suggest",
    params(
        ("query" = String, Query, description = "Partial name to complete"),
        ("folder_id" = Option<String>, Query, description = "Restrict to this folder"),
        ("limit" = Option<u32>, Query, description = "Max suggestions (default 10, max 20)"),
    ),
    responses(
        (status = 200, description = "Suggestions", body = SearchSuggestionsDto),
        (status = 503, description = "Search service unavailable"),
    ),
    tag = "search"
)]
pub async fn suggest_files(
    state: State<Arc<AppState>>,
    query: Query<SuggestParams>,
) -> impl IntoResponse {
    SearchHandler::suggest_files_impl(state, query).await
}

#[utoipa::path(
    delete,
    path = "/api/search/cache",
    responses(
        (status = 200, description = "Cache cleared"),
        (status = 503, description = "Search service unavailable"),
    ),
    tag = "search"
)]
pub async fn clear_search_cache(state: State<Arc<AppState>>) -> impl IntoResponse {
    SearchHandler::clear_search_cache_impl(state).await
}
