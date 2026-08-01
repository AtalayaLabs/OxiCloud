use crate::application::services::batch_operations::BatchOperationService;
use crate::common::di::AppState;
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Json as AxumJson, Response},
    routing::{any, delete, get, patch, post, put},
};
use serde_json::json;
use std::sync::Arc;
use utoipa::OpenApi;

/// Liveness probe — returns 200 if the process is running, no DB check.
async fn health() -> impl IntoResponse {
    (StatusCode::OK, AxumJson(json!({"status": "ok"})))
}

/// Readiness probe — returns 200 if the DB pool can serve queries, 503 otherwise.
async fn ready(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match &state.db_pool {
        Some(pool) => match sqlx::query("SELECT 1").execute(pool.as_ref()).await {
            Ok(_) => (
                StatusCode::OK,
                AxumJson(json!({"status": "ok", "db": "ok"})),
            ),
            Err(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                AxumJson(json!({"status": "error", "db": "error"})),
            ),
        },
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            AxumJson(json!({"status": "error", "db": "not configured"})),
        ),
    }
}

/// Returns the application version from Cargo.toml (compile-time constant)
async fn get_version() -> AxumJson<serde_json::Value> {
    AxumJson(json!({
        "name": "OxiCloud",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// Pre-serialized OpenAPI spec. `ApiDoc::openapi()` reconstructs the whole
/// 171 KiB paths/schemas tree and re-serializes it per request (2.8 ms /
/// 12 474 allocs); the spec is process-invariant, so serialize once and
/// hand back a `Bytes` refcount bump (~18 ns — benches/ROUND11.md).
static OPENAPI_BODY: std::sync::OnceLock<bytes::Bytes> = std::sync::OnceLock::new();

async fn get_openapi_spec() -> axum::response::Response {
    let body = OPENAPI_BODY.get_or_init(|| {
        bytes::Bytes::from(
            serde_json::to_vec(&super::ApiDoc::openapi()).expect("openapi spec serializes"),
        )
    });
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body.clone()))
        .expect("static openapi response")
}

use crate::interfaces::api::handlers::admin_handler;
use crate::interfaces::api::handlers::batch_handler::{self, BatchHandlerState};
// `chunked_upload_handler::*` are marked `#[deprecated]` (prefer
// `/api/files/delta/*`); the router still needs to reference them
// until clients migrate. See the `chunked_upload_router` block
// below for the local `#[allow(deprecated)]`.
#[allow(deprecated)]
use crate::interfaces::api::handlers::chunked_upload_handler::{
    cancel_upload, complete_upload, create_upload, get_upload_status, upload_chunk,
};
use crate::interfaces::api::handlers::delta_upload_handler::{
    delta_commit, delta_download_chunks, delta_file_manifest, delta_negotiate, delta_upload_chunks,
};
use crate::interfaces::api::handlers::file_handler::{
    create_file_by_hash, delete_file, download_file, get_file_metadata, get_thumbnail,
    list_files_query, move_file_simple, rename_file, upload_file_with_thumbnails, upload_thumbnail,
};
use crate::interfaces::api::handlers::folder_handler::{
    create_folder, delete_folder_with_trash, download_folder_zip, get_folder, get_folder_ancestors,
    list_folder_resources, list_root_folders, move_folder, rename_folder,
};
use crate::interfaces::api::handlers::i18n_handler::{
    get_locales, get_translations_by_locale, translate,
};
use crate::interfaces::api::handlers::search_handler::{search_resources, suggest_files};
use crate::interfaces::api::handlers::trash_handler;

/// Creates root-level health check routes — mounted directly at `/`, not under `/api/`.
/// (follow docker/kubernetes best practices)
///
/// - `GET /health` — liveness probe, no DB check, always 200 if process is up.
/// - `GET /ready`  — readiness probe, pings DB pool, returns 503 if unreachable.
pub fn create_health_routes(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .with_state(app_state.clone())
}

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

        // AppState-backed share endpoints (download, contents, file, zip)
        router = router
            .route(
                "/s/{token}/download",
                get(share_handler::download_shared_file),
            )
            .route(
                "/s/{token}/contents",
                get(share_handler::list_share_contents_root),
            )
            .route(
                "/s/{token}/contents/{folder_id}",
                get(share_handler::list_share_contents_subfolder),
            )
            .route(
                "/s/{token}/file/{file_id}",
                get(share_handler::download_share_file_in_folder),
            )
            .route(
                "/s/{token}/zip",
                get(share_handler::download_share_zip_root),
            )
            .route(
                "/s/{token}/zip/{folder_id}",
                get(share_handler::download_share_zip_subfolder),
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
    // authorization is no longer extracted separately — the grants router now
    // uses app_state directly so handlers can access all services.

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

    // Basic folder listing (no caller-flag enrichment needed — flags
    // come from the per-row SQL EXISTS in the listing repo).
    let folders_basic_router = Router::new()
        .route("/", get(list_root_folders))
        .route("/{id}/resources", get(list_folder_resources))
        .with_state(folder_service.clone());

    // Single-item CRUD emits a FolderDto to the SPA and MUST carry
    // authoritative `is_favorite` / `is_shared` flags. The impls call
    // the `caller_flags` enrichment helper via
    // `state.favorites_service`, so the full `AppState` is required.
    let folders_crud_router = Router::new()
        .route("/", post(create_folder))
        .route("/{id}", get(get_folder))
        // Ancestor chain for the shared breadcrumb component — one
        // round-trip vs the pre-2026-07-26 per-segment `getFolder` walk
        // on the /files client. Returns caller-visible parents (walk
        // stops at share/drive-membership boundary) + access_source.
        .route("/{id}/ancestors", get(get_folder_ancestors))
        .route("/{id}/rename", put(rename_folder))
        .route("/{id}/move", put(move_folder))
        .with_state(app_state.clone());

    // Special route for ZIP download that requires AppState instead of just FolderService
    let folder_zip_router = Router::new()
        .route("/{id}/download", get(download_folder_zip))
        .with_state(app_state.clone());

    // Create folder operations that use trash (requires full AppState)
    let folders_ops_router = Router::new().route("/{id}", delete(delete_folder_with_trash));

    // Merge the routers
    let folders_router = folders_basic_router
        .merge(folders_crud_router)
        .merge(folders_ops_router)
        .merge(folder_zip_router);

    // Create file routes for basic operations and trash-enabled delete
    let basic_file_router = Router::new()
        .route("/", get(list_files_query))
        .route("/upload", post(upload_file_with_thumbnails))
        .route("/by-hash", post(create_file_by_hash))
        .route("/delta/negotiate", post(delta_negotiate))
        .route("/delta/chunks", put(delta_upload_chunks))
        .route("/delta/commit", post(delta_commit))
        .route("/delta/download", post(delta_download_chunks))
        .route("/{id}/manifest", get(delta_file_manifest))
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
        .route("/folders/copy", post(batch_handler::copy_folders_batch))
        .route("/folders/move", post(batch_handler::move_folders_batch))
        // Trash operations (soft delete)
        .route("/trash", post(batch_handler::trash_batch))
        // Download as ZIP
        .route("/download", post(batch_handler::download_batch_post))
        // work arround for drag & drop (does not support POST requests)
        .route("/download", get(batch_handler::download_batch_querystring))
        .with_state(batch_handler_state);

    // Create search routes if the service is available
    let search_router = if search_service.is_some() {
        Router::new()
            // Simple search with query parameters
            // Cursor-paginated search with the `/*/resources` envelope
            // (items + next_cursor + meta). `POST /search/advanced` was
            // deleted alongside this normalization — every field it
            // accepted fits fine as a query param, and it shared 100%
            // of the service call with GET (no fast-vs-deep semantics).
            .route("/", get(search_resources))
            // Lightweight autocomplete suggestions
            .route("/suggest", get(suggest_files))
            // `DELETE /api/search/cache` used to live here as a per-user-
            // reachable endpoint. It's an operator-only debug lever
            // (moka `invalidate_all()` — nukes every tenant), so it
            // moved to `/api/admin/search/cache` where the URL declares
            // intent. AuthZ audit #14 (2026-07-16).
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

    // Create routes for ReBAC grants (/api/grants).
    // State is Arc<AppState> so that the new list_shared_with_me handler can
    // access file/folder services. Existing handlers still extract
    // State<Arc<PgAclEngine>> via the FromRef impl in di.rs.
    let grants_router = {
        use crate::interfaces::api::handlers::grant_handler;
        Router::new()
            .route("/", post(grant_handler::create_grant))
            .route("/", get(grant_handler::list_on_resource))
            .route("/{id}", delete(grant_handler::revoke_grant))
            .route("/{id}/notify", post(grant_handler::notify_grant_recipient))
            .route("/role", put(grant_handler::set_role))
            .route("/incoming", get(grant_handler::list_incoming))
            .route(
                "/incoming/resources",
                get(grant_handler::list_shared_with_me),
            )
            .route("/outgoing", get(grant_handler::list_outgoing))
            .route("/outgoing/resources", get(grant_handler::list_my_shares))
            .with_state(app_state.clone())
    };

    // Create a router without the i18n routes
    // Create routes for favorites if the service is available
    let favorites_router = if let Some(favorites_service) = favorites_service.clone() {
        use crate::interfaces::api::handlers::favorites_handler::{self, list_favorites_resources};

        Router::new()
            .route("/resources", get(list_favorites_resources))
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
            .route("/resources", get(recent_handler::list_recent_resources))
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
    //
    // Each handler carries `#[deprecated]` so utoipa marks the OpenAPI paths
    // deprecated (Swagger UI shows the strikethrough + banner) and existing
    // callers get a compile-time nudge to migrate to `/api/files/delta/*`.
    // The route registration itself has to keep referencing them until the
    // clients migrate off, so we suppress the local `deprecated` lint here.
    #[allow(deprecated)]
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
    use super::handlers::dedup_handler::{check_hash, check_hashes_batch, get_blob};
    let dedup_router = Router::new()
        .route("/check/{hash}", get(check_hash))
        .route("/check-batch", post(check_hashes_batch))
        .route("/blob/{hash}", get(get_blob))
        // NOTE: `remove_reference` is intentionally NOT exposed as a
        // public endpoint — ref_count management is an internal concern
        // handled automatically when files are deleted via the file API.
        //
        // `/stats` and `/recalculate` moved to `/api/admin/dedup/*`
        // (AuthZ audit #24/#25, 2026-07-17) so the middleware admin
        // gate covers them by construction. See
        // `admin_handler::admin_routes()`.
        .with_state(app_state.clone());

    let mut router = Router::new()
        .nest("/folders", folders_router)
        .nest("/files", files_router)
        .nest("/uploads", chunked_upload_router)
        .nest("/dedup", dedup_router)
        .nest("/batch", batch_router)
        .nest("/search", search_router)
        .nest("/shares", share_router)
        .nest("/grants", grants_router)
        .nest("/favorites", favorites_router)
        .nest("/recent", recent_router);

    // Photos timeline endpoint — lists all image/video files sorted by capture date
    {
        use crate::interfaces::api::handlers::photos_handler;

        let mut photos_router = Router::new().route("/", get(photos_handler::list_photos));
        if app_state.places_service.is_some() {
            photos_router = photos_router.route("/geo", get(photos_handler::list_photos_geo));
        }
        let photos_router = photos_router.with_state(app_state.clone());

        router = router.nest("/photos", photos_router);
    }

    // Drives — every drive the caller can read. D0 ships the read-only
    // listing; D2 adds the membership API; D3 the create-shared-drive flow.
    {
        use crate::interfaces::api::handlers::drive_handler;

        let drives_router = Router::new()
            .route(
                "/",
                get(drive_handler::list_drives).post(drive_handler::create_drive),
            )
            .route("/{id}", axum::routing::delete(drive_handler::delete_drive))
            .route(
                "/{id}/policies",
                patch(drive_handler::update_drive_policies),
            )
            .route("/{id}/quota", patch(drive_handler::update_drive_quota))
            .route(
                "/{id}/members",
                get(drive_handler::list_drive_members).post(drive_handler::add_drive_member),
            )
            .route(
                "/{id}/members/{kind}/{sid}",
                patch(drive_handler::update_drive_member)
                    .delete(drive_handler::remove_drive_member),
            )
            .with_state(app_state.clone());

        router = router.nest("/drives", drives_router);
    }

    // People (faces) routes — mounted only when OXICLOUD_ENABLE_FACES is on.
    if app_state.people_service.is_some() {
        use crate::interfaces::api::handlers::people_handler;

        let people_router = Router::new()
            .route("/", get(people_handler::list_people))
            .route("/merge", post(people_handler::merge_people))
            .route("/recluster", post(people_handler::recluster))
            .route("/data", delete(people_handler::delete_all))
            .route("/faces/{file_id}", get(people_handler::faces_for_file))
            .route("/{id}", patch(people_handler::rename_person))
            .route("/{id}/photos", get(people_handler::person_photos))
            .with_state(app_state.clone());

        router = router.nest("/people", people_router);
    }

    // Re-enable trash routes to make the trash view work
    if let Some(_trash_service_ref) = trash_service.clone() {
        tracing::info!("Setting up trash routes for trash view");

        let trash_router = Router::new()
            // Literal paths first — order matters for axum overlap handling
            // when a wildcard like /{id} could otherwise capture them.
            .route("/resources", get(trash_handler::get_trash_resources))
            .route("/empty", delete(trash_handler::empty_trash))
            // Per-drive empty (D2b stage 4 / per-drive UX). Scoped
            // empty of one drive's trash; refused 404 when the caller
            // lacks Delete on the named drive.
            .route(
                "/drive/{drive_id}",
                delete(trash_handler::empty_trash_for_drive),
            )
            .route("/files/{id}", delete(trash_handler::move_file_to_trash))
            .route("/folders/{id}", delete(trash_handler::move_folder_to_trash))
            .route("/{id}/restore", post(trash_handler::restore_from_trash))
            .route("/{id}", delete(trash_handler::delete_permanently))
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

    // REST browse API for CardDAV contacts, groups, and OxiCloud users.
    // Write operations and protocol sync remain on the /carddav endpoint.
    if let Some(contact_service) = app_state.contact_use_case.clone() {
        use crate::interfaces::api::handlers::contacts_handler::{self, ContactsApiState};

        let auth_svc = app_state
            .auth_service
            .as_ref()
            .map(|s| s.auth_application_service.clone());

        let contacts_state = ContactsApiState {
            contact_service,
            auth_service: auth_svc,
            expose_system_users: app_state.core.config.features.expose_system_users,
        };

        let contacts_router = Router::new()
            .route(
                "/",
                get(contacts_handler::list_address_books)
                    .post(contacts_handler::create_address_book),
            )
            .route(
                "/{book_id}",
                put(contacts_handler::update_address_book)
                    .delete(contacts_handler::delete_address_book),
            )
            .route(
                "/{book_id}/contacts",
                get(contacts_handler::list_contacts).post(contacts_handler::create_contact),
            )
            .route(
                "/{book_id}/contacts/{contact_id}",
                get(contacts_handler::get_contact)
                    .put(contacts_handler::update_contact)
                    .delete(contacts_handler::delete_contact),
            )
            .route(
                "/{book_id}/groups",
                get(contacts_handler::list_groups).post(contacts_handler::create_group),
            )
            .route(
                "/{book_id}/groups/{group_id}",
                get(contacts_handler::get_group)
                    .put(contacts_handler::update_group)
                    .delete(contacts_handler::delete_group),
            )
            .route(
                "/{book_id}/groups/{group_id}/contacts",
                get(contacts_handler::list_contacts_in_group)
                    .post(contacts_handler::add_contact_to_group),
            )
            .route(
                "/{book_id}/groups/{group_id}/contacts/{contact_id}",
                delete(contacts_handler::remove_contact_from_group),
            )
            .with_state(contacts_state);

        router = router.nest("/address-books", contacts_router);
        tracing::info!("Contacts REST API routes initialized");
    }

    // NOTE: WebDAV routes are mounted at top-level (/webdav) in main.rs
    // for client compatibility, NOT under /api.

    // NOTE: CalDAV and CardDAV routes are mounted at top-level (/caldav, /carddav)
    // in main.rs for protocol compliance, NOT under /api.

    // Admin settings routes — the whole subtree is admin-only by
    // construction. The `require_admin` layer runs AFTER the outer
    // `auth_middleware` (main.rs::protected_api), so it can rely on
    // `CurrentUser` already being in the request extensions. Any new
    // route added to `admin_handler::admin_routes()` inherits the
    // gate automatically — implementors no longer have to remember
    // to call `require_admin(&state, &headers).await?` inline, and a
    // forgotten call can't silently expose a non-admin surface.
    let admin_router = admin_handler::admin_routes()
        .layer(axum::middleware::from_fn(
            crate::interfaces::middleware::auth::require_admin,
        ))
        .with_state(app_state.clone());
    router = router.nest("/admin", admin_router);

    // ReBAC subject-group management. All mutating routes are admin-gated;
    // /api/groups/search is authenticated-only so the share dialog can list
    // groups as recipients.
    let group_router =
        crate::interfaces::api::handlers::subject_group_handler::subject_group_routes()
            .with_state(app_state.clone());
    router = router.nest("/groups", group_router);

    // Per-user profile lookup `/api/users/{id}` — authenticated only,
    // throttled by a per-caller limiter inside the handler. External
    // callers are 403'd in the service layer.
    let users_router = crate::interfaces::api::handlers::users_handler::user_routes()
        .with_state(app_state.clone());
    router = router.nest("/users", users_router);

    // Collector for any unknown `/api/*` path. Without this, an
    // unmatched API URL falls through Axum's matcher to the
    // ServeDir fallback and is logged under `http::web` — wrong
    // surface for operator triage. Adding the catch-all here
    // anchors the 404 on whichever access-log layer the parent
    // mount applies (i.e. `http::api`).
    //
    // The bare `/api/auth/*`, `/api/wopi/*`, and other more-
    // specific nests are registered at higher specificity and
    // still win over this catch-all — Axum's matcher prefers
    // them on every overlapping request.
    router = router.route("/{*rest}", any(api_not_found));

    // Server-status header. Stamps `X-Server-Status` on every
    // response so the frontend's fetch wrapper can update a
    // reactive store — banner shows/hides without polling.
    // Sub-nanosecond on the hot path (single atomic load), a few
    // µs on the cold path (only during a running migration). See
    // `middleware::server_status`.
    let router = router.layer(axum::middleware::from_fn_with_state(
        app_state.clone(),
        crate::interfaces::middleware::server_status::server_status_middleware,
    ));

    // No per-router layers beyond that: the global `TraceLayer` + request-id
    // stack in `main.rs` wraps the whole app (this `/api` router is nested
    // into it), so a second `TraceLayer` here just double-wrapped every
    // `/api` request in a redundant span + response-future poll
    // (benches/ROUND13.md §H1). Compression is likewise the global layer's
    // job — re-applying it here (no predicate) would compress media
    // downloads, burning CPU for ~0 gain and stripping `Content-Length`.
    router
}

/// Catch-all 404 for unknown `/api/*` paths. Pure log-anchoring
/// shim — see the comment in `create_api_routes`.
async fn api_not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap()
}
