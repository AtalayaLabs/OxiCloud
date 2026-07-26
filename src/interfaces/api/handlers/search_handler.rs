use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use tracing::{error, info};

use crate::application::dtos::search_dto::{
    SearchResourcesDto, SearchResourcesQuery, SearchSuggestionsDto,
};
use crate::application::ports::inbound::SearchUseCase;
use crate::common::di::AppState;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use std::sync::Arc;

/**
 * Handler for search operations through the API.
 *
 * All search processing (filtering, scoring, sorting, categorization,
 * formatting) is performed server-side. These handlers are thin HTTP
 * adapters that delegate to the SearchUseCase.
 */
/// Hard cap on the search page size. The default is 100; without a ceiling a
/// client could pass `?limit=<huge>`, which flows straight into the SQL `LIMIT`
/// and would pull that many rows into memory (and into the result cache). 500
/// is a generous page for a search UI — `total_count` still reflects the full
/// match set, so deeper results stay reachable via `offset`. Mirrors the
/// suggestions endpoint, which already clamps with `.min(20)`.
const MAX_SEARCH_LIMIT: usize = 500;

pub struct SearchHandler;

impl SearchHandler {
    // ── Why no #[utoipa::path] here? ─────────────────────────────────────────────
    // utoipa 5.4.0's proc macro generates helper structs / impls inside its expansion.
    // Rust allows struct definitions at module scope but forbids them inside impl blocks,
    // so `#[utoipa::path]` fails on every method in this impl block regardless of HTTP
    // verb or annotation content. All route handlers are free functions below.
    // TODO: collapse after utoipa upgrade.
    /// `GET /api/search` — wire-normalised search endpoint.
    ///
    /// Returns the same `items[] { resource_type, resource, meta }`
    /// envelope shape as every other `/*/resources` listing endpoint
    /// (folders, favorites, recent, trash, shared) so the SPA's
    /// `ResourceList` component consumes it as-is. Search-specific
    /// enrichment (`meta.score` + optional `snippet` + `via`) sits
    /// inline on each item.
    ///
    /// Phase 1-plus wire adapter: the internal `SearchService` still
    /// speaks `SearchCriteriaDto`/`SearchResultsDto`. The query is
    /// translated at this boundary; the result envelope is composed
    /// via `SearchResourcesDto::from_service_result`. The `is_favorite`
    /// / `is_shared` fields on each `FileDto`/`FolderDto` come from
    /// per-row EXISTS subqueries in the search SQL (see
    /// `search_files_paginated` and `search_folders`).
    ///
    /// The old `POST /api/search/advanced` variant was deleted in
    /// the same PR — every field it accepted fits cleanly as a query
    /// param.
    pub(super) async fn search_resources_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
        Query(query): Query<SearchResourcesQuery>,
    ) -> impl IntoResponse {
        info!("API: File search (normalized envelope)");

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

        // Cap page size — `SearchResourcesQuery::limit_clamped` already
        // hits `[1, 200]`, but re-clamp against MAX_SEARCH_LIMIT for
        // defence-in-depth if the constant is ever raised above 200.
        let mut criteria = query.to_criteria();
        criteria.limit = criteria.limit.min(MAX_SEARCH_LIMIT);

        match search_service.search(criteria, auth_user.id).await {
            Ok(results) => {
                info!(
                    "Search completed in {}ms — {} files, {} folders",
                    results.query_time_ms,
                    results.files.len(),
                    results.folders.len()
                );
                // Unwrap the Arc — the service caches `Arc<SearchResultsDto>`
                // so consumers share the allocation. `from_service_result`
                // consumes the DTO to move enriched rows into the envelope's
                // `resource` slot without cloning; the Arc's shared clone
                // pays one deep copy here but avoids allocating during the
                // hot cache-hit path elsewhere.
                let dto = SearchResourcesDto::from_service_result((*results).clone(), &query);
                let rows = dto.items.len();
                crate::interfaces::api::sized_json::sized_json(
                    256 + rows * crate::interfaces::api::sized_json::EST_WRAPPED_ROW_BYTES,
                    &dto,
                )
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
        auth_user: AuthUser,
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
            .suggest_with_perms(
                &params.query,
                params.folder_id.as_deref(),
                limit,
                auth_user.id,
            )
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

    /// `DELETE /admin/search/cache` — flush the shared moka search
    /// results cache. Admin-only.
    ///
    /// AuthZ audit #14 (2026-07-12): pre-fix this endpoint lived at
    /// `/api/search/cache` and required only a valid JWT — any
    /// authenticated user (external / magic-link included) could
    /// DELETE it in a loop and keep the results cache cold indefinitely
    /// (sustained DoS on every subsequent `/api/search` query). Now
    /// mounted at `/api/admin/search/cache`, gated by the
    /// `require_admin` middleware layer on the `/api/admin` nest point.
    /// The handler no longer needs an inline authz call — reaching
    /// this code implies `AuthUser` is admin by construction. Audit
    /// line on success so operator-driven flushes are traceable in
    /// security reviews.
    pub(super) async fn clear_search_cache_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
    ) -> Result<Response, AppError> {
        let caller_id = auth_user.id;
        info!("API: Clearing search cache");

        let Some(search_service) = &state.applications.search_service else {
            error!("Search service not available");
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Search service is not available" })),
            )
                .into_response());
        };

        match search_service.clear_search_cache().await {
            Ok(_) => {
                tracing::info!(
                    target: "audit",
                    event = "search.cache_cleared",
                    caller_id = %caller_id,
                    "🧹 search results cache flushed by admin",
                );
                Ok((
                    StatusCode::OK,
                    Json(json!({ "message": "Search cache cleared successfully" })),
                )
                    .into_response())
            }
            Err(err) => {
                error!("Error clearing search cache: {}", err);
                Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Error clearing search cache" })),
                )
                    .into_response())
            }
        }
    }
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
        ("query" = Option<String>, Query, description = "Text to search in names / content"),
        ("limit" = Option<u32>, Query, description = "Max items per page (1–200, default 50)"),
        ("cursor" = Option<String>, Query, description = "Opaque cursor from a previous response"),
        ("order_by" = Option<String>, Query, description = "Sort dimension: relevance (default) | name | size | updated_at | created_at"),
        ("resource_types" = Option<String>, Query, description = "Comma-separated: file, folder (both by default)"),
        ("reverse" = Option<bool>, Query, description = "Reverse the sort order"),
        ("type" = Option<String>, Query, description = "Filter by file extensions (comma-separated)"),
        ("folder_id" = Option<String>, Query, description = "Restrict search to this folder"),
        ("recursive" = Option<bool>, Query, description = "Recurse into subfolders (default true)"),
        ("created_after" = Option<u64>, Query, description = "Minimum creation timestamp (unix seconds)"),
        ("created_before" = Option<u64>, Query, description = "Maximum creation timestamp"),
        ("modified_after" = Option<u64>, Query, description = "Minimum modification timestamp"),
        ("modified_before" = Option<u64>, Query, description = "Maximum modification timestamp"),
        ("min_size" = Option<u64>, Query, description = "Minimum file size (bytes)"),
        ("max_size" = Option<u64>, Query, description = "Maximum file size (bytes)"),
    ),
    responses(
        (status = 200, description = "Search results (cursor-paginated envelope shared with /*/resources)", body = SearchResourcesDto),
        (status = 503, description = "Search service unavailable"),
    ),
    security(("bearerAuth" = [])),
    tag = "search"
)]
pub async fn search_resources(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    query: Query<SearchResourcesQuery>,
) -> impl IntoResponse {
    SearchHandler::search_resources_impl(state, auth_user, query).await
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
    security(("bearerAuth" = [])),
    tag = "search"
)]
pub async fn suggest_files(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    query: Query<SuggestParams>,
) -> impl IntoResponse {
    SearchHandler::suggest_files_impl(state, auth_user, query).await
}

#[utoipa::path(
    delete,
    path = "/api/admin/search/cache",
    responses(
        (status = 200, description = "Cache cleared"),
        (status = 401, description = "Missing or invalid token"),
        (status = 403, description = "Caller is not an admin"),
        (status = 503, description = "Search service unavailable"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn clear_search_cache(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
) -> Result<Response, AppError> {
    SearchHandler::clear_search_cache_impl(state, auth_user).await
}
