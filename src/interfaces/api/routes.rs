use crate::application::ports::scope_ports::{Condition, MinRole};
use crate::application::services::batch_operations::BatchOperationService;
use crate::application::services::scope_table::PolicyRegistry;
use crate::common::di::AppState;
use crate::interfaces::api::policy_router::{
    GatedRouter, Policied, get as policy_get, post as policy_post,
};

/// The seven share-visitor-reachable GETs all carry this one condition.
/// Declared per VERB: three of their paths also serve a write, which no
/// visitor may reach.
const VISITOR: [Condition; 1] = [Condition::visitor_allowed()];

/// No credential at all. The spec turns this into `security: []`, which is
/// the only honest way to document an endpoint that needs none.
const PUBLIC: [Condition; 1] = [Condition::public()];
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
        "version": env!("OXICLOUD_VERSION")
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
    list_files_query, move_file_simple, rename_file, replace_file_content,
    upload_file_with_thumbnails, upload_thumbnail,
};
use crate::interfaces::api::handlers::folder_handler::{
    create_folder, delete_folder_with_trash, download_folder_zip, get_folder, get_folder_ancestors,
    list_folder_resources, list_root_folders, move_folder, rename_folder, upload_preflight,
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
/// Returns the router AND its declarations.
///
/// Every operation here is PUBLIC — reachable with no credential — and
/// says so with `.policy([Condition::public()])`. That is not decoration:
/// the default policy is an authenticated user, so without it the spec
/// claims a bearer these routes do not need, and `just openapi` reports
/// each one as a disagreement.
///
/// Declared per route rather than on the subtree, because this router is
/// nested at `/api` alongside the protected one — a prefix declaration
/// would cover both.
pub fn create_public_api_routes(
    app_state: &Arc<AppState>,
) -> (Router<Arc<AppState>>, PolicyRegistry) {
    let share_service = app_state.share_service.clone();
    let i18n_service = Some(app_state.applications.i18n_service.clone());

    let mut router = GatedRouter::<Arc<AppState>>::new();

    // Public share access routes — no auth required
    if let Some(share_service) = share_service {
        use crate::interfaces::api::handlers::share_handler;

        use crate::interfaces::middleware::rate_limit::{RateLimiter, rate_limit_share_verify};

        // Share-password verification is budgeted from the login limits —
        // see `rate_limit_share_verify`. Built here rather than in `main.rs`
        // alongside the other three because this router is assembled here and
        // threading a limiter through `create_public_api_routes` would make
        // every caller of this function know about rate limiting.
        let rl = &app_state.core.config.auth.rate_limit;
        let verify_limiter = Arc::new(RateLimiter::new(
            rl.login_max_requests,
            rl.login_window_secs,
            100_000,
        ));

        let public_share_router = GatedRouter::new()
            .route(
                "/{token}",
                policy_get(share_handler::access_shared_item).policy(PUBLIC),
            )
            .route(
                "/{token}/verify",
                // A rate limit is not a policy: the two coexist.
                policy_post(share_handler::verify_shared_item_password)
                    .policy(PUBLIC)
                    .layer(axum::middleware::from_fn_with_state(
                        verify_limiter,
                        rate_limit_share_verify,
                    )),
            )
            .with_state(share_service);

        // `/s/{token}` and `/s/{token}/verify` are all that remain of the
        // public-share API. They are a LOGIN METHOD: they resolve the token,
        // enforce the password gate, and issue the `oxi_shares` ring cookie.
        // Everything a visitor does afterwards — browsing, thumbnails,
        // downloads, zips — goes through the ordinary `/api/folders/*` and
        // `/api/files/*` routes, authorized by the ReBAC grant the share
        // already owns.
        //
        // The six endpoints that used to live here (download, contents,
        // contents/{folder_id}, file/{file_id}, zip, zip/{folder_id}) were a
        // parallel implementation of that surface and are gone; see the
        // Phase 4 commit for what they were and why they could not stay.
        router = router.nest_gated("/s", public_share_router);
    }

    // i18n routes — no auth required (localization should be available before login)
    if let Some(i18n_service) = i18n_service {
        let i18n_router = GatedRouter::new()
            .route("/locales", policy_get(get_locales).policy(PUBLIC))
            .route("/translate", policy_get(translate).policy(PUBLIC))
            .route(
                "/locales/{locale_code}",
                policy_get(get_translations_by_locale).policy(PUBLIC),
            )
            .with_state(i18n_service);

        router = router.nest_gated("/i18n", i18n_router);
    }

    // Version endpoint — public, no auth required
    router = router.route("/version", policy_get(get_version).policy(PUBLIC));
    router = router.route("/openapi.json", policy_get(get_openapi_spec).policy(PUBLIC));

    // Server-configuration discovery endpoint — public, unauthenticated.
    // Returns feature flags, version, and a snapshot of the server-status
    // header for one-shot boot hydration by the SPA. See
    // `handlers/config_handler.rs` for the DTO shape and rationale.
    router = router.route(
        "/config",
        policy_get(crate::interfaces::api::handlers::config_handler::get_config)
            .policy(PUBLIC)
            .with_state(app_state.clone()),
    );

    router.finish()
}

/// Creates protected API routes for the application.
///
/// These routes require authentication when auth is enabled.
/// Receives the fully-assembled `AppState` and extracts all needed services
/// from it, avoiding a long parameter list.
/// The router AND the fully-resolved policy registry, ready to install or
/// to mark a spec with.
///
/// One function because there are TWO callers that must not drift: the
/// server, which installs the registry for the gate, and
/// `generate-openapi`, which marks the published spec from it. When the
/// rebasing lived in `main.rs` alone, the generator produced a registry
/// keyed `/admin` instead of `/api/admin` and silently marked nothing.
pub fn api_router_and_policies(
    app_state: &Arc<AppState>,
) -> (Router<Arc<AppState>>, Router<Arc<AppState>>, PolicyRegistry) {
    let (protected, protected_decls) = create_api_routes(app_state);
    let (public, public_decls) = create_public_api_routes(app_state);

    // Both are nested at `/api` and both write their declarations RELATIVE
    // to themselves, so both rebase the same way.
    let mut registry = protected_decls.rebased_under("/api");
    registry.absorb(public_decls.rebased_under("/api"));
    // Routers that `main.rs` builds and mounts itself, declared here so
    // ONE function owns the whole registry. Their routers are constructed
    // and dropped: only the declarations are wanted, and construction is
    // pure.
    //
    // This is deliberate redundancy with `main.rs`'s nests — the prefix
    // appears in both — and it is policed rather than trusted: a prefix
    // that matches no operation is an ORPHAN (a boot error), and a public
    // route whose declaration never lands is reported by `just openapi`.
    // Both caught the earlier versions of this, where the generator saw a
    // registry the server did not have.
    {
        use crate::interfaces::api::handlers::{
            auth_handler, device_auth_handler, opaque_auth_handler,
        };
        for (prefix, decls) in [
            (
                "/api/auth",
                auth_handler::auth_public_routes().into_policies(),
            ),
            ("/api/auth", auth_handler::login_route().into_policies()),
            ("/api/auth", auth_handler::register_route().into_policies()),
            ("/api/auth", auth_handler::refresh_route().into_policies()),
            ("/api", auth_handler::setup_route().into_policies()),
            (
                "/api/auth/opaque/login",
                opaque_auth_handler::opaque_login_routes().into_policies(),
            ),
            (
                "/api/auth/opaque",
                opaque_auth_handler::opaque_params_routes().into_policies(),
            ),
            (
                "/api/auth/device",
                device_auth_handler::device_auth_public_routes().into_policies(),
            ),
        ] {
            registry.absorb(decls.rebased_under(prefix));
        }
    }

    // Plus the operations that have no route to declare against — the DAV
    // surfaces.
    registry.absorb(crate::interfaces::api::policy_declarations::declarations());

    (protected, public, registry)
}

/// Returns the router AND the policy declarations made against it.
///
/// Route, layer and policy are declared in ONE place — that is the point
/// of `.policy([...])` sitting on the nest or the handler rather than in a
/// central table. The declarations come back as a registry because a
/// `Router` cannot be asked what it holds, and the paths here are relative
/// to this router, so `main.rs` rebases them under `/api`.
pub fn create_api_routes(app_state: &Arc<AppState>) -> (Router<Arc<AppState>>, PolicyRegistry) {
    // Extract services from the pre-built AppState
    let folder_service = app_state.applications.folder_service_concrete.clone();
    let file_retrieval_service = app_state.applications.file_retrieval_service.clone();
    let file_management_service = app_state.applications.file_management_service.clone();
    let trash_service = app_state.trash_service.clone();
    let search_service = app_state.applications.search_service.clone();
    let share_service = app_state.share_service.clone();
    let favorites_service = app_state.favorites_service.clone();
    let recent_service = app_state.recent_service.clone();
    let notification_service = app_state.notification_service.clone();
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
    let folders_basic_router = GatedRouter::new()
        .route("/", get(list_root_folders))
        // Reachable by a public-share VISITOR, per verb: the GET only.
        .route(
            "/{id}/resources",
            policy_get(list_folder_resources).policy(VISITOR),
        )
        .with_state(folder_service.clone());

    // Single-item CRUD emits a FolderDto to the SPA and MUST carry
    // authoritative `is_favorite` / `is_shared` flags. The impls call
    // the `caller_flags` enrichment helper via
    // `state.favorites_service`, so the full `AppState` is required.
    let folders_crud_router = GatedRouter::new()
        .route("/", post(create_folder))
        .route("/{id}", policy_get(get_folder).policy(VISITOR))
        // Ancestor chain for the shared breadcrumb component — one
        // round-trip vs the pre-2026-07-26 per-segment `getFolder` walk
        // on the /files client. Returns caller-visible parents (walk
        // stops at share/drive-membership boundary) + access_source.
        .route(
            "/{id}/ancestors",
            policy_get(get_folder_ancestors).policy(VISITOR),
        )
        .route("/{id}/upload-preflight", post(upload_preflight))
        .route("/{id}/rename", put(rename_folder))
        .route("/{id}/move", put(move_folder))
        .with_state(app_state.clone());

    // Special route for ZIP download that requires AppState instead of just FolderService
    let folder_zip_router = GatedRouter::new()
        .route(
            "/{id}/download",
            policy_get(download_folder_zip).policy(VISITOR),
        )
        .with_state(app_state.clone());

    // Create folder operations that use trash (requires full AppState)
    let folders_ops_router = GatedRouter::new().route("/{id}", delete(delete_folder_with_trash));

    // Merge the routers
    let folders_router = folders_basic_router
        .merge(folders_crud_router)
        .merge(folders_ops_router)
        .merge(folder_zip_router);

    // Create file routes for basic operations and trash-enabled delete
    let basic_file_router = GatedRouter::new()
        .route("/", get(list_files_query))
        .route("/upload", post(upload_file_with_thumbnails))
        .route("/by-hash", post(create_file_by_hash))
        .route("/delta/negotiate", post(delta_negotiate))
        .route("/delta/chunks", put(delta_upload_chunks))
        .route("/delta/commit", post(delta_commit))
        .route("/delta/download", post(delta_download_chunks))
        .route("/{id}/manifest", get(delta_file_manifest))
        .route("/{id}/content", put(replace_file_content))
        // Visitor-reachable, GET only — the DELETE on this same path
        // must NOT be, which is why the declaration names the verb.
        .route("/{id}", policy_get(download_file).policy(VISITOR))
        // Split rather than chained (`get(..).put(..)`), so the GET can
        // be visitor-reachable while the PUT is not. Two `route()` calls
        // on one path with DIFFERENT verbs merge; axum panics only on a
        // duplicate SAME method.
        .route(
            "/{id}/thumbnail/{size}",
            policy_get(get_thumbnail).policy(VISITOR),
        )
        .route("/{id}/thumbnail/{size}", put(upload_thumbnail))
        .route(
            "/{id}/metadata",
            policy_get(get_file_metadata).policy(VISITOR),
        )
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
    let file_operations_router = GatedRouter::new()
        .route("/{id}", delete(delete_file))
        .route("/{id}/move", put(move_file_simple))
        .route("/{id}/rename", put(rename_file));

    // Merge the routers
    let mut files_router = basic_file_router.merge(file_operations_router);

    // `GET /api/files/recent-changes` — who changed what lately
    // (`docs/plan/recently-modified.md`).
    //
    // Mounted only when `enable_recent_changes` is on, so an operator who
    // declined the feature gets a 404 on the path rather than a surface the
    // SPA merely hides.
    //
    // `require_internal_user_layer` is a SCOPE question, not a grant: "may
    // this KIND of caller reach this surface at all", with no resource to
    // name, so it belongs here and not in a service. An external is a guest
    // invited to specific resources; a feed of colleagues' activity
    // aggregates more than that invitation implied, and they are already 403
    // on the system contacts listing and on every DAV surface.
    if app_state.core.config.features.enable_recent_changes {
        use crate::interfaces::api::handlers::recent_changes_handler;
        files_router = files_router.merge(
            GatedRouter::new()
                .route(
                    "/recent-changes",
                    // Internal users only: a feed of colleagues' activity
                    // aggregates more than a guest's invitation implied.
                    policy_get(recent_changes_handler::list_recent_changes)
                        .policy([Condition::role(MinRole::Internal)]),
                )
                .with_state(app_state.clone()),
        );
    }

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

    // Notifications bell (Slice E). Mounted only when the service is
    // wired (i.e. auth is enabled — bell requires a caller). Non-
    // registration path: with the flag off, the routes 404 instead of
    // 5xx-ing on a NULL service — matches the OXICLOUD_MESSAGEBUS_ENABLE
    // approach for `/api/rt/*` and `OXICLOUD_ENABLE_EXTERNAL_MOUNTS`
    // for admin mounts.
    let notifications_router = if let Some(ref svc) = notification_service {
        use crate::interfaces::api::handlers::notifications_handler;
        Router::new()
            .route("/", get(notifications_handler::list_notifications))
            .route("/unread", get(notifications_handler::unread_count))
            .route("/read-all", post(notifications_handler::mark_all_read))
            .route("/{id}/read", post(notifications_handler::mark_read))
            .route("/{id}", delete(notifications_handler::delete_notification))
            .with_state(svc.clone())
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
        // gate covers them by construction. The `/check/{hash}` URL is
        // split — the user-scoped shape stays here (no `ref_count`,
        // 404 on no visible reference); the admin shape with the
        // global `ref_count` lives at `/api/admin/dedup/check/{hash}`.
        // See §9b of `docs/plan/photos-resources-migration.md`.
        .with_state(app_state.clone());

    let mut router = GatedRouter::<Arc<AppState>>::new()
        .nest_gated("/folders", folders_router)
        .nest_gated("/files", files_router)
        .nest("/uploads", chunked_upload_router)
        .nest("/dedup", dedup_router)
        .nest("/batch", batch_router)
        .nest("/search", search_router)
        .nest("/shares", share_router)
        .nest("/grants", grants_router)
        .nest("/favorites", favorites_router)
        .nest("/recent", recent_router)
        .nest("/notifications", notifications_router);

    // Photos timeline endpoint — lists all image/video files sorted by capture date.
    // Top-level gate on `features.enable_photos`: when the operator sets
    // `OXICLOUD_ENABLE_PHOTOS=false` the whole `/api/photos` prefix stays
    // unmounted (same shape as the search gate), so non-SPA clients also
    // see a 404 rather than a surface the SPA merely hides. The inner
    // `places_service.is_some()` gate on `/geo` keeps the finer-grained
    // sub-feature control exactly as before.
    if app_state.core.config.features.enable_photos {
        use crate::interfaces::api::handlers::photos_handler;

        let mut photos_router =
            Router::new().route("/resources", get(photos_handler::list_photos_resources));
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

        // Creating a playlist is internal-only, and ONLY the create
        // route is gated. Everything else in this nest stays reachable
        // for externals: a playlist can be shared with a guest
        // (`POST /{id}/share`), and the per-resource handlers already
        // demand a grant via `require_playlist_perm`. Blocking the whole
        // nest would take shared-playlist access away from exactly the
        // principal kind grants exist for.
        //
        // Why a layer here instead of a check in `create_playlist`:
        // `MusicService::create_playlist` has no pre-write gate (it
        // seeds the caller an Owner grant and returns), and the service
        // holds no handle that can resolve a user, so the router is
        // where this costs nothing. A service-layer gate is the better
        // home the day `MusicService` grows a user port.
        let music_router = GatedRouter::new()
            // Creating a playlist is internal-only: the service seeds the
            // caller an Owner grant and has no user port to gate on. The
            // LIST on this same path stays open, which is why the
            // declaration names the verb — and why `POST /` no longer
            // needs a router of its own just to carry a layer.
            .route(
                "/",
                policy_post(music_handler::create_playlist)
                    .policy([Condition::role(MinRole::Internal)]),
            )
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

        router = router.nest_gated("/playlists", music_router);
        tracing::info!("Music routes initialized");
    }

    // REST browse API for CardDAV contacts, groups, and OxiCloud users.
    // Write operations and protocol sync remain on the /carddav endpoint.
    //
    // Gated on `features.enable_contacts` the same way `/api/photos` is
    // gated on `enable_photos`: with the flag off the whole `/api/contacts`
    // prefix stays unmounted, so a non-SPA client gets a 404 rather than a
    // surface the SPA merely hides. The flag is NOT `enable_carddav` —
    // that governs the sync protocol, this governs the browse UI, and
    // either can be wanted without the other.
    if app_state.core.config.features.enable_contacts
        && let Some(contact_service) = app_state.contact_use_case.clone()
    {
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

        // Creating an address book is internal-only — same reasoning as
        // the playlist create above, and the same narrow scope. The
        // CardDAV door to this service method (`MKCOL`) has been gated
        // all along by `require_internal_user_layer` on `/carddav`; REST
        // reached the identical `ContactService::create_address_book`
        // with no gate at all, which is the inconsistency this closes.
        //
        // Reads and in-book writes stay open: an address book can be
        // shared, `create_contact` / `create_group` already demand
        // `Permission::Create` on the book, and `list_address_books`
        // deliberately handles an external caller (it hides the system
        // "OxiCloud Users" book from them). Gating the nest would make
        // that branch dead code and cut guests off from books shared
        // with them.
        let contacts_router = GatedRouter::new()
            // Creating an address book is internal-only. LISTING stays
            // open: `list_address_books` deliberately handles an external
            // caller, hiding the system "OxiCloud Users" book from them,
            // and gating the path would make that branch dead code and
            // cut guests off from books shared with them.
            .route(
                "/",
                policy_post(contacts_handler::create_address_book)
                    .policy([Condition::role(MinRole::Internal)]),
            )
            .route("/", get(contacts_handler::list_address_books))
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

        router = router.nest_gated("/address-books", contacts_router);
        tracing::info!("Contacts REST API routes initialized");
    }

    // JSON calendar API for the SPA. CalDAV stays the sync surface at
    // /caldav; this is the same CalendarService behind JWT auth so the
    // frontend does not have to parse XML and iCalendar in the browser.
    //
    // Gated on `features.enable_calendars` the same way `/api/contacts`
    // is gated on `enable_contacts`: with the flag off the whole
    // `/api/calendars` prefix stays unmounted, so a non-SPA client gets
    // a 404 rather than a surface the SPA merely hides. The flag is NOT
    // `enable_caldav` — that governs the sync protocol, this governs
    // the browse UI, and either can be wanted without the other.
    if app_state.core.config.features.enable_calendars
        && let Some(calendar_service) = app_state.calendar_use_case.clone()
    {
        use crate::interfaces::api::handlers::calendar_handler::{self, CalendarApiState};

        let calendar_router = Router::new()
            .route(
                "/",
                get(calendar_handler::list_calendars).post(calendar_handler::create_calendar),
            )
            .route(
                "/{calendar_id}",
                put(calendar_handler::update_calendar).delete(calendar_handler::delete_calendar),
            )
            .route(
                "/{calendar_id}/events",
                get(calendar_handler::list_events).post(calendar_handler::create_event),
            )
            .route(
                "/{calendar_id}/events/{event_id}",
                put(calendar_handler::update_event).delete(calendar_handler::delete_event),
            )
            .with_state(CalendarApiState { calendar_service });

        router = router.nest("/calendars", calendar_router);
        tracing::info!("Calendar REST API routes initialized");
    }

    // NOTE: WebDAV routes are mounted at top-level (/webdav) in main.rs
    // for client compatibility, NOT under /api.

    // NOTE: CalDAV and CardDAV routes are mounted at top-level (/caldav, /carddav)
    // in main.rs for protocol compliance, NOT under /api.

    // Admin settings routes — the whole subtree is admin-only by
    // construction, declared once on the nest. Any route added to
    // `admin_handler::admin_routes()` inherits it, so an implementor
    // cannot forget a gate and silently expose a non-admin surface.
    //
    // The `require_admin` layer that used to sit here is GONE (PR 1 step
    // 4-xi): the declaration below is the gate, and the policy gate
    // evaluates it — applied once in `main.rs` to the whole protected
    // subtree, which `route_layer` carries into this nest.
    //
    // `/api/admin/transfer-ownership` tightens to Owner, which
    // `require_admin` could not express at all; that refusal lives in the
    // service until `admin_routes()` can declare per verb.
    let admin_router = admin_handler::admin_routes(app_state).with_state(app_state.clone());
    router = router.nest(
        "/admin",
        admin_router.policy([Condition::role(MinRole::Admin)]),
    );

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

    // Message bus — ticket issuance (`POST /api/rt/ticket`). Stays in
    // the protected router (auth + DPoP), so the caller proves session
    // + DPoP-key possession before a ticket is minted. See
    // `handlers/rt_ticket_handler.rs` and `docs/architecture/message-bus-and-notifications.md § F`.
    //
    // Gated by `enable_message_bus`: when disabled, the route is NOT
    // registered — Axum returns 404 (no 5xx alerts, no ambiguous 403).
    // The paired WS route in `main.rs` uses the same guard.
    if app_state.core.config.features.enable_message_bus {
        router = router.route(
            "/rt/ticket",
            post(crate::interfaces::api::handlers::rt_ticket_handler::issue_rt_ticket)
                .with_state(app_state.clone()),
        );
    }

    // The WS upgrade (`GET /api/rt/ws`) is registered OUTSIDE the
    // protected-api middleware stack — a browser cannot attach a
    // `DPoP:` header to `new WebSocket()`, so the standard stack
    // 401s on every DPoP-bound session. See the `rt_ws` module doc
    // for the self-auth logic (ticket subprotocol or bearer token).
    // Registration happens in `main.rs` where the outer router owns
    // the middleware layering.

    // Authenticated runtime server-status snapshot. Shape = full
    // HeaderPayload (same as the X-Server-Status response header on
    // every authenticated response). Carries migration / rotation /
    // holder — the fields `/api/config.server_status` deliberately
    // omits because they'd leak infrastructure topology
    // (`migration.target` is a backend name, `holder.display` is an
    // operator-authored label) to anonymous callers. See
    // `handlers/server_status_handler.rs`.
    router = router.route(
        "/status",
        get(crate::interfaces::api::handlers::server_status_handler::get_server_status)
            .with_state(app_state.clone()),
    );

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
    // No per-router layers beyond that: the global `TraceLayer` + request-id
    // stack in `main.rs` wraps the whole app (this `/api` router is nested
    // into it), so a second `TraceLayer` here just double-wrapped every
    // `/api` request in a redundant span + response-future poll
    // (benches/ROUND13.md §H1). Compression is likewise the global layer's
    // job — re-applying it here (no predicate) would compress media
    // downloads, burning CPU for ~0 gain and stripping `Content-Length`.
    let (router, policy) = router
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            crate::interfaces::middleware::server_status::server_status_middleware,
        ))
        .finish();
    (router, policy)
}

/// Catch-all 404 for unknown `/api/*` paths. Pure log-anchoring
/// shim — see the comment in `create_api_routes`.
async fn api_not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap()
}
