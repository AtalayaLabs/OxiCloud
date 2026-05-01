use crate::application::services::batch_operations::BatchOperationService;
use crate::common::di::AppState;
use axum::{
    Router,
    extract::DefaultBodyLimit,
    response::Json as AxumJson,
    routing::{delete, get, post, put},
};
use serde_json::json;
use std::sync::Arc;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};
use utoipa::OpenApi;

/// Returns the application version from Cargo.toml (compile-time constant)
async fn get_version() -> AxumJson<serde_json::Value> {
    AxumJson(json!({
        "name": "OxiCloud",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

async fn get_openapi_spec() -> AxumJson<utoipa::openapi::OpenApi> {
    AxumJson(super::ApiDoc::openapi())
}

use crate::interfaces::api::handlers::admin_handler;
use crate::interfaces::api::handlers::batch_handler::{self, BatchHandlerState};
use crate::interfaces::api::handlers::chunked_upload_handler::{
    cancel_upload, complete_upload, create_upload, get_upload_status, upload_chunk,
};
use crate::interfaces::api::handlers::file_handler::{
    delete_file, download_file, get_file_metadata, get_thumbnail, list_files_query,
    move_file_simple, rename_file, upload_file_with_thumbnails, upload_thumbnail,
};
use crate::interfaces::api::handlers::folder_handler::{
    create_folder, delete_folder_with_trash, download_folder_zip, get_folder,
    list_folder_contents, list_folder_contents_paginated, list_folder_listing,
    list_root_folders, list_root_folders_paginated, move_folder, rename_folder,
};
use crate::interfaces::api::handlers::i18n_handler::{
    get_locales, get_translations_by_locale, translate,
};
use crate::interfaces::api::handlers::search_handler::{
    clear_search_cache, search_files_get, search_files_post, suggest_files,
};
use crate::interfaces::api::handlers::trash_handler;

/// Creates public API routes that should NOT require authentication.
pub fn create_public_api_routes(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    let share_service = app_state.share_service.clone();
    let i18n_service = Some(app_state.applications.i18n_service.clone());

    let mut router = Router::new();

    // Public share access routes — no auth required
    if let Some(share_service) = share_service {
        use crate::interfaces::api::handlers::share_handler;

        let public_share_router = Router::new()
            .route("/{token}", get(share_handler::access_shared_item))
            .route(
                "/{token}/verify",
                post(share_handler::verify_shared_item_password),
            )
            .with_state(share_service);

        router = router.nest("/s", public_share_router);

        // Download endpoint uses full AppState (needs FileRetrievalService)
        router = router.route(
            "/s/{token}/download",
            get(share_handler::download_shared_file),
        );
    }

    // i18n routes — no auth required (localization should be available before login)
    if let Some(i18n_service) = i18n_service {
        let i18n_router = Router::new()
            .route("/locales", get(get_locales))
            .route("/translate", get(translate))
            .route("/locales/{locale_code}", get(get_translations_by_locale))
            .with_state(i18n_service);

        router = router.nest("/i18n", i18n_router);
    }

    // Version endpoint — public, no auth required
    router = router.route("/version", get(get_version));
    router = router.route("/openapi.json", get(get_openapi_spec));

    router
}

/// Creates protected API routes for the application.
///
/// These routes require authentication when auth is enabled.
/// Receives the fully-assembled `AppState` and extracts all needed services
/// from it, avoiding a long parameter list.
pub fn create_api_routes(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    // Extract services from the pre-built AppState
    let folder_service = app_state.applications.folder_service_concrete.clone();
    let file_retrieval_service = app_state.applications.file_retrieval_service.clone();
    let file_management_service = app_state.applications.file_management_service.clone();
    let trash_service = app_state.trash_service.clone();
    let search_service = app_state.applications.search_service.clone();
    let share_service = app_state.share_service.clone();
    let favorites_service = app_state.favorites_service.clone();
    let recent_service = app_state.recent_service.clone();

    // Initialize the batch operations service
    let mut batch_service_builder = BatchOperationService::default(
        file_retrieval_service.clone(),
        file_management_service.clone(),
        folder_service.clone(),
    );
    if let Some(ref ts) = trash_service {
        batch_service_builder = batch_service_builder.with_trash_service(ts.clone());
    }
    let batch_service = Arc::new(batch_service_builder);

    // Create state for the batch operations handler
    let batch_handler_state = BatchHandlerState {
        batch_service: batch_service.clone(),
    };

    // Create the basic folders router with service operations
    let folders_basic_router = Router::new()
        .route("/", post(create_folder))
        .route("/", get(list_root_folders))
        .route("/paginated", get(list_root_folders_paginated))
        .route("/{id}", get(get_folder))
        .route("/{id}/contents", get(list_folder_contents))
        .route("/{id}/contents/paginated", get(list_folder_contents_paginated))
        .route("/{id}/rename", put(rename_folder))
        .route("/{id}/move", put(move_folder))
        .with_state(folder_service.clone());

    // Special route for ZIP download that requires AppState instead of just FolderService
    let folder_zip_router = Router::new()
        .route("/{id}/download", get(download_folder_zip))
        .with_state(app_state.clone());

    // Combined listing endpoint: returns both sub-folders AND files in one
    // response.  Needs full AppState because it calls both FolderService
    // and FileRetrievalService concurrently.
    let folder_listing_router = Router::new()
        .route("/{id}/listing", get(list_folder_listing))
        .with_state(app_state.clone());

    // Create folder operations that use trash (requires full AppState)
    let folders_ops_router =
        Router::new().route("/{id}", delete(delete_folder_with_trash));

    // Merge the routers
    let folders_router = folders_basic_router
        .merge(folders_ops_router)
        .merge(folder_zip_router)
        .merge(folder_listing_router);

    // Create file routes for basic operations and trash-enabled delete
    let basic_file_router = Router::new()
        .route("/", get(list_files_query))
        .route("/upload", post(upload_file_with_thumbnails))
        .route("/{id}", get(download_file))
        .route(
            "/{id}/thumbnail/{size}",
            get(get_thumbnail).put(upload_thumbnail),
        )
        .route("/{id}/metadata", get(get_file_metadata))
        .layer(DefaultBodyLimit::max({
            // Use architecture-appropriate body limit: 10 GB on 64-bit, 1 GB on 32-bit
            #[cfg(target_pointer_width = "64")]
            const FILE_BODY_LIMIT: usize = 10 * 1024 * 1024 * 1024;
            #[cfg(target_pointer_width = "32")]
            const FILE_BODY_LIMIT: usize = 1024 * 1024 * 1024;
            FILE_BODY_LIMIT
        })) // for file uploads
        .with_state(app_state.clone());

    // File operations with trash support
    let file_operations_router = Router::new()
        .route("/{id}", delete(delete_file))
        .route("/{id}/move", put(move_file_simple))
        .route("/{id}/rename", put(rename_file));

    // Merge the routers
    let files_router = basic_file_router.merge(file_operations_router);

    // Create routes for batch operations
    let batch_router = Router::new()
        // File operations
        .route("/files/move", post(batch_handler::move_files_batch))
        .route("/files/copy", post(batch_handler::copy_files_batch))
        .route("/files/delete", post(batch_handler::delete_files_batch))
        .route("/files/get", post(batch_handler::get_files_batch))
        // Folder operations
        .route("/folders/delete", post(batch_handler::delete_folders_batch))
        .route("/folders/create", post(batch_handler::create_folders_batch))
        .route("/folders/get", post(batch_handler::get_folders_batch))
        .route("/folders/move", post(batch_handler::move_folders_batch))
        // Trash operations (soft delete)
        .route("/trash", post(batch_handler::trash_batch))
        // Download as ZIP
        .route("/download", post(batch_handler::download_batch))
        .with_state(batch_handler_state);

    // Create search routes if the service is available
    let search_router = if search_service.is_some() {
        Router::new()
            // Simple search with query parameters
            .route("/", get(search_files_get))
            // Lightweight autocomplete suggestions
            .route("/suggest", get(suggest_files))
            // Advanced search with full criteria object
            .route("/advanced", post(search_files_post))
            // Clear search cache
            .route("/cache", delete(clear_search_cache))
            .with_state(app_state.clone())
    } else {
        Router::new()
    };

    // Direct handler implementations for sharing, without depending on ShareHandler

    // Create routes for shared resources management (requires auth)
    let share_router = if let Some(share_service) = share_service.clone() {
        use crate::interfaces::api::handlers::share_handler;

        Router::new()
            .route("/", post(share_handler::create_shared_link))
            .route("/", get(share_handler::get_user_shares))
            .route("/{id}", get(share_handler::get_shared_link))
            .route("/{id}", put(share_handler::update_shared_link))
            .route("/{id}", delete(share_handler::delete_shared_link))
            .with_state(share_service.clone())
    } else {
        Router::new()
    };

    // Create a router without the i18n routes
    // Create routes for favorites if the service is available
    let favorites_router = if let Some(favorites_service) = favorites_service.clone() {
        use crate::interfaces::api::handlers::favorites_handler;

        Router::new()
            .route("/", get(favorites_handler::get_favorites))
            .route("/batch", post(favorites_handler::batch_add_favorites))
            .route(
                "/{item_type}/{item_id}",
                post(favorites_handler::add_favorite),
            )
            .route(
                "/{item_type}/{item_id}",
                delete(favorites_handler::remove_favorite),
            )
            .with_state(favorites_service.clone())
    } else {
        Router::new()
    };

    // Create routes for recent items if the service is available
    let recent_router = if let Some(recent_service) = recent_service.clone() {
        use crate::interfaces::api::handlers::recent_handler;

        Router::new()
            .route("/", get(recent_handler::get_recent_items))
            .route(
                "/{item_type}/{item_id}",
                post(recent_handler::record_item_access),
            )
            .route(
                "/{item_type}/{item_id}",
                delete(recent_handler::remove_from_recent),
            )
            .route("/clear", delete(recent_handler::clear_recent_items))
            .with_state(recent_service.clone())
    } else {
        Router::new()
    };

    // Create routes for chunked uploads (large files >10MB).
    // All five handlers are free functions — see chunked_upload_handler.rs for why
    // #[utoipa::path] cannot be applied to ChunkedUploadHandler impl methods directly.
    let chunked_upload_router = Router::new()
        .route("/", post(create_upload))
        .route("/{upload_id}", axum::routing::patch(upload_chunk))
        .route("/{upload_id}", axum::routing::head(get_upload_status))
        .route("/{upload_id}/complete", post(complete_upload))
        .route("/{upload_id}", delete(cancel_upload))
        .with_state(app_state.clone());

    // Create routes for deduplication endpoints.
    // All handlers are free functions — see dedup_handler.rs for why
    // #[utoipa::path] cannot be applied to DedupHandler impl methods directly.
    use super::handlers::dedup_handler::{
        check_hash, get_blob, get_stats, recalculate_stats, upload_with_dedup,
    };
    let dedup_router = Router::new()
        .route("/check/{hash}", get(check_hash))
        .route("/upload", post(upload_with_dedup))
        .route("/stats", get(get_stats))
        .route("/blob/{hash}", get(get_blob))
        // NOTE: remove_reference is intentionally NOT exposed as a public
        // endpoint — ref_count management is an internal concern handled
        // automatically when files are deleted via the file API.
        .route("/recalculate", post(recalculate_stats))
        .with_state(app_state.clone());

    let mut router = Router::new()
        .nest("/folders", folders_router)
        .nest("/files", files_router)
        .nest("/uploads", chunked_upload_router)
        .nest("/dedup", dedup_router)
        .nest("/batch", batch_router)
        .nest("/search", search_router)
        .nest("/shares", share_router)
        .nest("/favorites", favorites_router)
        .nest("/recent", recent_router);

    // Photos timeline endpoint — lists all image/video files sorted by capture date
    {
        use crate::interfaces::api::handlers::photos_handler;

        let photos_router = Router::new()
            .route("/", get(photos_handler::list_photos))
            .with_state(app_state.clone());

        router = router.nest("/photos", photos_router);
    }

    // Re-enable trash routes to make the trash view work
    if let Some(_trash_service_ref) = trash_service.clone() {
        tracing::info!("Setting up trash routes for trash view");

        let trash_router = Router::new()
            .route("/", get(trash_handler::get_trash_items))
            .route("/files/{id}", delete(trash_handler::move_file_to_trash))
            .route("/folders/{id}", delete(trash_handler::move_folder_to_trash))
            .route("/{id}/restore", post(trash_handler::restore_from_trash))
            .route("/{id}", delete(trash_handler::delete_permanently))
            .route("/empty", delete(trash_handler::empty_trash))
            .with_state(app_state.clone());

        router = router.nest("/trash", trash_router);
    } else {
        tracing::warn!("Trash service not available - trash view will not work");
    }

    // Music/Playlist routes
    if let Some(ref music_svc) = app_state.music_service {
        use crate::interfaces::api::handlers::music_handler;

        let music_router = Router::new()
            .route("/", post(music_handler::create_playlist))
            .route("/", get(music_handler::list_playlists))
            .route("/{playlist_id}", get(music_handler::get_playlist))
            .route("/{playlist_id}", put(music_handler::update_playlist))
            .route(
                "/{playlist_id}",
                axum::routing::delete(music_handler::delete_playlist),
            )
            .route(
                "/{playlist_id}/tracks",
                get(music_handler::list_playlist_tracks),
            )
            .route("/{playlist_id}/tracks", post(music_handler::add_tracks))
            .route(
                "/{playlist_id}/tracks/{file_id}",
                axum::routing::delete(music_handler::remove_track),
            )
            .route("/{playlist_id}/reorder", put(music_handler::reorder_tracks))
            .route("/{playlist_id}/share", post(music_handler::share_playlist))
            .route(
                "/{playlist_id}/share/{user_id}",
                axum::routing::delete(music_handler::remove_share),
            )
            .route(
                "/{playlist_id}/shares",
                get(music_handler::get_playlist_shares),
            )
            .route(
                "/audio-metadata/{file_id}",
                get(music_handler::get_audio_metadata),
            )
            .with_state(music_svc.clone());

        router = router.nest("/playlists", music_router);
        tracing::info!("Music routes initialized");
    }

    // NOTE: WebDAV routes are mounted at top-level (/webdav) in main.rs
    // for client compatibility, NOT under /api.

    // NOTE: CalDAV and CardDAV routes are mounted at top-level (/caldav, /carddav)
    // in main.rs for protocol compliance, NOT under /api.

    // Admin settings routes (protected by admin_guard inside the handler)
    let admin_router = admin_handler::admin_routes().with_state(app_state.clone());
    router = router.nest("/admin", admin_router);

    // Transparent compression (gzip + brotli) for all API responses.
    // tower-http negotiates via Accept-Encoding and skips already-compressed
    // content types automatically. No manual compression in handlers.
    router
        .layer(CompressionLayer::new().br(true).gzip(true))
        .layer(TraceLayer::new_for_http())
}
