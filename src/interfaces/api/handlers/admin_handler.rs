use axum::{
    Router,
    extract::{DefaultBodyLimit, Json, Multipart, Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post, put},
};

use crate::application::dtos::drive_dto::DriveDto;
use crate::application::dtos::grant_dto::{GrantDto, RoleDto, SubjectDto, SubjectTypeDto};
use crate::application::dtos::plugin_dto::{
    PluginInfoDto, PluginLogEntryDto, PluginLogPageDto, PluginLogQueryDto, PluginRetentionDto,
    SetEnabledDto,
};
use crate::application::dtos::settings_dto::{
    AdminCreateUserDto, AdminResetPasswordDto, DashboardStatsDto, ListUsersQueryDto,
    MigrationStateDto, SaveOidcSettingsDto, SaveStorageSettingsDto, SendSmtpTestDto, SmtpInfoDto,
    SmtpTestResultDto, StartMigrationDto, TestOidcConnectionDto, TestStorageConnectionDto,
    UpdateUserActiveDto, UpdateUserQuotaDto, UpdateUserRoleDto, VerifyMigrationDto,
};
use crate::application::dtos::user_dto::{AdminUserSummaryDto, UserDto};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::plugin_ports::{LogQuery, PluginManagementPort, PluginMgmtError};
use crate::application::ports::storage_ports::StorageUsagePort;
use crate::common::di::AppState;
use crate::domain::repositories::drive_repository::DriveRepository;
use crate::domain::services::authorization::{Resource, Subject};
use crate::interfaces::api::handlers::dedup_handler::{get_stats, recalculate_stats};
use crate::interfaces::api::handlers::search_handler::clear_search_cache;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use std::sync::Arc;
use uuid::Uuid;

#[derive(serde::Serialize)]
#[serde(untagged)]
enum AdminUsersPayload {
    Full(Vec<UserDto>),
    Summary(Vec<AdminUserSummaryDto>),
}

#[derive(serde::Serialize)]
struct AdminUsersPageResponse {
    users: AdminUsersPayload,
    total: i64,
    limit: i64,
    offset: i64,
}

/// Admin API routes — all require admin role.
pub fn admin_routes() -> Router<Arc<AppState>> {
    use super::admin_external_mounts as ext_mounts;
    Router::new()
        // External file mounts
        .route(
            "/external-mounts",
            get(ext_mounts::list_external_mounts).post(ext_mounts::create_external_mount),
        )
        .route(
            "/external-mounts/{id}",
            delete(ext_mounts::delete_external_mount),
        )
        // OIDC settings
        .route("/settings/oidc", get(get_oidc_settings))
        .route("/settings/oidc", put(save_oidc_settings))
        .route("/settings/oidc/test", post(test_oidc_connection))
        // Storage settings
        .route("/settings/storage", get(get_storage_settings))
        .route("/settings/storage", put(save_storage_settings))
        .route("/settings/storage/test", post(test_storage_connection))
        // Storage migration
        .route("/storage/migration", get(get_migration_status))
        .route("/storage/migration/start", post(start_migration))
        .route("/storage/migration/pause", post(pause_migration))
        .route("/storage/migration/resume", post(resume_migration))
        .route("/storage/migration/complete", post(complete_migration))
        .route("/storage/migration/verify", post(verify_migration))
        // Encryption key generation
        .route(
            "/settings/storage/generate-key",
            post(generate_encryption_key),
        )
        // Dashboard / stats
        .route("/dashboard", get(get_dashboard_stats))
        // User management
        .route("/users", get(list_users))
        .route("/users", post(create_user))
        .route("/users/{id}", get(get_user))
        .route("/users/{id}", delete(delete_user))
        .route("/users/{id}/role", put(update_user_role))
        .route("/users/{id}/active", put(update_user_active))
        .route("/users/{id}/quota", put(update_user_quota))
        .route("/users/{id}/password", put(reset_user_password))
        .route(
            "/users/{id}/promote-to-internal",
            post(admin_promote_external_to_internal),
        )
        // Registration control
        .route("/settings/registration", put(set_registration_setting))
        // Audio metadata
        .route("/audio/metadata/reextract", post(reextract_audio_metadata))
        // Image/video capture metadata (Photos timeline backfill)
        .route("/photos/metadata/reextract", post(reextract_image_metadata))
        // Plugin management
        .route("/plugins", get(list_plugins))
        // Install caps the request body at 32 MiB (overriding the global
        // multi-GB upload limit) — a plugin bundle is small; the unpack also
        // enforces a 64 MiB decompressed ceiling.
        .route(
            "/plugins",
            post(install_plugin).layer(DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        .route("/plugins/{id}/enabled", put(set_plugin_enabled))
        .route("/plugins/{id}", delete(delete_plugin))
        // Plugin logs + per-plugin retention
        .route("/plugins/{id}/logs", get(get_plugin_logs))
        .route("/plugins/{id}/logs", delete(clear_plugin_logs))
        .route("/plugins/{id}/logs/stream", get(stream_plugin_logs))
        .route("/plugins/{id}/retention", get(get_plugin_retention))
        .route("/plugins/{id}/retention", put(set_plugin_retention))
        // Search — operator flush of the shared moka results cache
        // (AuthZ audit #14, 2026-07-16). `invalidate_all()` semantics
        // touch every tenant, so this is admin-only. Lived at
        // `/api/search/cache` pre-2026-07-17; the URL now declares
        // its admin intent up front.
        .route("/search/cache", delete(clear_search_cache))
        // Dedup — global storage stats + integrity recalculation
        // (AuthZ audit #24 + #25, 2026-07-17). Both are operator-only
        // observability / maintenance surfaces (blob-count-level data
        // + verify_integrity sweep). Moved here from `/api/dedup/*`
        // so the URL declares admin intent and the middleware layer
        // enforces it — same pattern as `search/cache` above. The
        // any-authenticated sibling routes (`/check`, `/check-batch`,
        // `/blob/{hash}`) stay at `/api/dedup/*`.
        .route("/dedup/stats", get(get_stats))
        .route("/dedup/recalculate", post(recalculate_stats))
        // SMTP diagnostics
        .route("/smtp/info", get(get_smtp_info))
        .route("/smtp/test", post(send_smtp_test))
        // Test-only capture endpoint. The handler short-circuits to 404
        // when `OXICLOUD_SMTP_MOCK` is off, so production deployments
        // can route the path freely without leaking inboxes.
        .route("/smtp/test/captured", get(get_captured_email))
        // Test-only sweep triggers. Routes are always registered; the
        // handlers themselves short-circuit to 404 when
        // `features.enable_admin_internal_endpoints` is off — matches
        // the `/smtp/test/captured` convention so production
        // deployments don't need a different route table.
        .route("/internal/trigger-sweep", post(internal_trigger_sweep))
        .route("/internal/trigger-gc", post(internal_trigger_gc))
        .route(
            "/internal/trigger-grant-cleanup",
            post(internal_trigger_grant_cleanup),
        )
        // JobRegistry admin surface — production, always-on,
        // audit-logged. See `docs/plan/job-registry.md` §Cross-cutting.
        // The `/internal/trigger-*` shims above will be retired in a
        // follow-up PR (deprecated forwards to these endpoints).
        .route("/jobs", get(list_jobs))
        .route("/jobs/{name}/trigger", post(trigger_job))
        // Drives — admin-wide view (distinct from `/api/drives` which
        // is filtered to the caller's role grants).
        .route("/drives", get(list_all_drives))
        .route("/drives/{id}", delete(delete_drive_admin))
        .route(
            "/drives/{id}/members",
            get(list_drive_members_admin).post(add_drive_member_admin),
        )
        .route(
            "/drives/{id}/members/{kind}/{sid}",
            axum::routing::patch(update_drive_member_admin).delete(remove_drive_member_admin),
        )
}

// Every route under `/api/admin/*` is gated by the
// `require_admin` middleware layer wired at the router nest point
// (`routes.rs::admin_router`). Handlers no longer need an inline
// guard call — the caller is guaranteed to be admin by construction.
// Callers that need the caller's id read it from the `AuthUser`
// extractor (`middleware::auth::AuthUser`), populated by the outer
// `auth_middleware`.

/// GET /api/admin/settings/oidc — get OIDC settings for the admin panel
#[utoipa::path(
    get,
    path = "/api/admin/settings/oidc",
    responses(
        (status = 200, description = "OIDC settings"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn get_oidc_settings(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let svc = state
        .admin_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Admin settings service not available"))?;

    let settings = svc
        .get_oidc_settings()
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to load settings: {}", e)))?;

    Ok(Json(settings))
}

/// PUT /api/admin/settings/oidc — save OIDC settings + hot-reload
#[utoipa::path(
    put,
    path = "/api/admin/settings/oidc",
    responses(
        (status = 200, description = "OIDC settings saved"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn save_oidc_settings(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(dto): Json<SaveOidcSettingsDto>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = auth_user.id;

    let svc = state
        .admin_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Admin settings service not available"))?;

    svc.save_oidc_settings(dto, user_id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to save settings: {}", e)))?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "message": "OIDC settings saved and applied successfully"
        })),
    ))
}

/// POST /api/admin/settings/oidc/test — test OIDC discovery
async fn test_oidc_connection(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<TestOidcConnectionDto>,
) -> Result<impl IntoResponse, AppError> {
    let svc = state
        .admin_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Admin settings service not available"))?;

    let result = svc
        .test_oidc_connection(dto)
        .await
        .map_err(|e| AppError::internal_error(format!("Connection test failed: {}", e)))?;

    Ok(Json(result))
}

// ─────────────────────────────────────────────────────
// Storage settings handlers
// ─────────────────────────────────────────────────────

/// GET /api/admin/settings/storage — get storage backend settings
#[utoipa::path(
    get,
    path = "/api/admin/settings/storage",
    responses(
        (status = 200, description = "Storage settings"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn get_storage_settings(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let svc = state
        .storage_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Storage settings service not available"))?;

    let settings = svc
        .get_storage_settings()
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to load storage settings: {}", e)))?;

    Ok(Json(settings))
}

/// PUT /api/admin/settings/storage — save storage backend settings
#[utoipa::path(
    put,
    path = "/api/admin/settings/storage",
    responses(
        (status = 200, description = "Storage settings saved"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn save_storage_settings(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(dto): Json<SaveStorageSettingsDto>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = auth_user.id;

    let svc = state
        .storage_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Storage settings service not available"))?;

    svc.save_storage_settings(dto, user_id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to save storage settings: {}", e)))?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "message": "Storage settings saved successfully"
        })),
    ))
}

/// POST /api/admin/settings/storage/test — test storage backend connection
async fn test_storage_connection(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<TestStorageConnectionDto>,
) -> Result<impl IntoResponse, AppError> {
    let svc = state
        .storage_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Storage settings service not available"))?;

    let result = svc
        .test_storage_connection(dto)
        .await
        .map_err(|e| AppError::internal_error(format!("Storage connection test failed: {}", e)))?;

    Ok(Json(result))
}

// ─────────────────────────────────────────────────────
// Storage migration handlers
// ─────────────────────────────────────────────────────

/// GET /api/admin/storage/migration — current migration progress
#[utoipa::path(
    get,
    path = "/api/admin/storage/migration",
    responses(
        (status = 200, description = "Current migration status"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn get_migration_status(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let s = state.migration_state.read().await;
    Ok(Json(migration_state_to_dto(&s)))
}

/// POST /api/admin/storage/migration/start — begin background migration
#[utoipa::path(
    post,
    path = "/api/admin/storage/migration/start",
    responses(
        (status = 200, description = "Migration started"),
        (status = 400, description = "Migration already running"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn start_migration(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<StartMigrationDto>,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::services::migration_blob_backend::MigrationStatus;

    // Check not already running.
    {
        let s = state.migration_state.read().await;
        if s.status == MigrationStatus::Running {
            return Err(AppError::bad_request("A migration is already running"));
        }
    }

    let pool = state
        .db_pool
        .clone()
        .ok_or_else(|| AppError::internal_error("Database not available"))?;

    let source = state.core.dedup_service.backend().clone();
    let svc = state
        .storage_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Storage settings service not available"))?;

    // Build target backend from saved settings.
    let effective = svc
        .load_effective_storage_config()
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to load storage config: {}", e)))?;

    let target = build_backend_from_config(&effective)
        .map_err(|e| AppError::internal_error(format!("Failed to build target backend: {}", e)))?;
    target
        .initialize()
        .await
        .map_err(|e| AppError::internal_error(format!("Target backend init failed: {}", e)))?;

    let concurrency = dto.concurrency.unwrap_or(4).clamp(1, 16);
    let migration_state = state.migration_state.clone();

    // Spawn the background migration job.
    tokio::spawn(async move {
        if let Err(e) = crate::infrastructure::services::migration_job::run_migration(
            source,
            target,
            pool,
            migration_state,
            concurrency,
        )
        .await
        {
            tracing::error!("Migration job error: {}", e);
        }
    });

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "message": "Migration started" })),
    ))
}

/// POST /api/admin/storage/migration/pause — pause running migration
#[utoipa::path(
    post,
    path = "/api/admin/storage/migration/pause",
    responses(
        (status = 200, description = "Migration paused"),
        (status = 400, description = "No running migration"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn pause_migration(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::services::migration_blob_backend::MigrationStatus;

    let mut s = state.migration_state.write().await;
    if s.status != MigrationStatus::Running {
        return Err(AppError::bad_request("No running migration to pause"));
    }
    s.status = MigrationStatus::Paused;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "message": "Migration paused" })),
    ))
}

/// POST /api/admin/storage/migration/resume — resume paused migration
#[utoipa::path(
    post,
    path = "/api/admin/storage/migration/resume",
    responses(
        (status = 200, description = "Migration resumed"),
        (status = 400, description = "No paused migration"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn resume_migration(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::services::migration_blob_backend::MigrationStatus;

    // Set status back to Running — the background task checks on each blob.
    let mut s = state.migration_state.write().await;
    if s.status != MigrationStatus::Paused {
        return Err(AppError::bad_request("No paused migration to resume"));
    }
    s.status = MigrationStatus::Running;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "message": "Migration resumed" })),
    ))
}

/// POST /api/admin/storage/migration/complete — finalize migration
#[utoipa::path(
    post,
    path = "/api/admin/storage/migration/complete",
    responses(
        (status = 200, description = "Migration finalized"),
        (status = 400, description = "Migration not completed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn complete_migration(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::services::migration_blob_backend::MigrationStatus;

    let s = state.migration_state.read().await;
    if s.status != MigrationStatus::Completed {
        return Err(AppError::bad_request(
            "Migration must be completed (100%) before finalizing",
        ));
    }
    drop(s);

    // Mark as idle — the admin has acknowledged completion.
    let mut s = state.migration_state.write().await;
    s.status = MigrationStatus::Idle;

    Ok((
        StatusCode::OK,
        Json(
            serde_json::json!({ "message": "Migration finalized. Restart the server to use the new backend." }),
        ),
    ))
}

/// POST /api/admin/storage/migration/verify — run integrity check
#[utoipa::path(
    post,
    path = "/api/admin/storage/migration/verify",
    responses(
        (status = 200, description = "Verification result"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 500, description = "Verification failed")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn verify_migration(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<VerifyMigrationDto>,
) -> Result<impl IntoResponse, AppError> {
    let pool = state
        .db_pool
        .clone()
        .ok_or_else(|| AppError::internal_error("Database not available"))?;

    let svc = state
        .storage_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Storage settings service not available"))?;

    let effective = svc
        .load_effective_storage_config()
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to load storage config: {}", e)))?;

    let target = build_backend_from_config(&effective)
        .map_err(|e| AppError::internal_error(format!("Failed to build target backend: {}", e)))?;
    target
        .initialize()
        .await
        .map_err(|e| AppError::internal_error(format!("Target backend init failed: {}", e)))?;

    let sample_size = dto.sample_size.unwrap_or(100).clamp(1, 1000);

    let result =
        crate::infrastructure::services::migration_job::verify_migration(target, pool, sample_size)
            .await
            .map_err(|e| AppError::internal_error(format!("Verification failed: {}", e)))?;

    Ok(Json(result))
}

/// Helper: convert MigrationState to DTO for JSON serialization.
fn migration_state_to_dto(
    s: &crate::infrastructure::services::migration_blob_backend::MigrationState,
) -> MigrationStateDto {
    let throughput = match (s.started_at, s.migrated_bytes) {
        (Some(start), bytes) if bytes > 0 => {
            let elapsed = chrono::Utc::now()
                .signed_duration_since(start)
                .num_seconds()
                .max(1) as f64;
            Some(bytes as f64 / elapsed)
        }
        _ => None,
    };

    MigrationStateDto {
        status: format!("{:?}", s.status).to_lowercase(),
        total_blobs: s.total_blobs,
        migrated_blobs: s.migrated_blobs,
        migrated_bytes: s.migrated_bytes,
        failed_blobs: s.failed_blobs.clone(),
        started_at: s.started_at.map(|d| d.to_rfc3339()),
        completed_at: s.completed_at.map(|d| d.to_rfc3339()),
        throughput_bytes_per_sec: throughput,
    }
}

/// POST /api/admin/settings/storage/generate-key — generate a random AES-256 key.
#[utoipa::path(
    post,
    path = "/api/admin/settings/storage/generate-key",
    responses(
        (status = 200, description = "Generated AES-256 key"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn generate_encryption_key() -> Result<impl IntoResponse, AppError> {
    let key =
        crate::infrastructure::services::encrypted_blob_backend::EncryptedBlobBackend::generate_key(
        );
    let key_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key);

    Ok(Json(serde_json::json!({
        "key": key_b64,
        "warning": "Store this key securely. If lost, encrypted data is IRRECOVERABLY LOST."
    })))
}

/// Helper: build a BlobStorageBackend from StorageConfig.
fn build_backend_from_config(
    config: &crate::common::config::StorageConfig,
) -> Result<
    std::sync::Arc<dyn crate::application::ports::blob_storage_ports::BlobStorageBackend>,
    String,
> {
    match config.backend {
        crate::common::config::StorageBackendType::Local => Ok(std::sync::Arc::new(
            crate::infrastructure::services::local_blob_backend::LocalBlobBackend::new(
                std::path::Path::new(&config.root_dir),
            ),
        )),
        crate::common::config::StorageBackendType::S3 => {
            let s3 = config.s3.as_ref().ok_or("S3 config missing")?;
            Ok(std::sync::Arc::new(
                crate::infrastructure::services::s3_blob_backend::S3BlobBackend::new(s3),
            ))
        }
        crate::common::config::StorageBackendType::Azure => {
            let az = config.azure.as_ref().ok_or("Azure config missing")?;
            Ok(std::sync::Arc::new(
                crate::infrastructure::services::azure_blob_backend::AzureBlobBackend::new(az),
            ))
        }
    }
}

// ============================================================================
// Dashboard / Stats
// ============================================================================

/// GET /api/admin/dashboard — full dashboard statistics
#[utoipa::path(
    get,
    path = "/api/admin/dashboard",
    responses(
        (status = 200, description = "Dashboard statistics"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn get_dashboard_stats(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let auth_app = &auth.auth_application_service;

    // Get storage stats from repository (single efficient query)
    let db_pool = state
        .db_pool
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Database not available"))?;

    // Use direct SQL for aggregated stats — more efficient than loading all users.
    //
    // Scope: internal users only (`is_external = false`). External
    // accounts (grant-only magic-link / OCM recipients) have no
    // storage envelope by construction (DB CHECK
    // `users_external_no_storage`) and cannot be admin
    // (`users_external_not_admin`), so they'd inflate `total_users`
    // and `active_users` with rows that don't represent operational
    // seats. The audit list (`/api/admin/users`) still shows every
    // account; only the dashboard totals filter externals out.
    let stats_row = sqlx::query(
        r#"
        SELECT
            COUNT(*)::INT8 as total_users,
            COUNT(*) FILTER (WHERE active = true)::INT8 as active_users,
            COUNT(*) FILTER (WHERE role::text = 'admin')::INT8 as admin_users,
            COALESCE(SUM(storage_quota_bytes)::INT8, 0) as total_quota_bytes,
            COALESCE(SUM(storage_used_bytes)::INT8, 0) as total_used_bytes,
            COUNT(*) FILTER (WHERE storage_quota_bytes > 0 AND storage_used_bytes > storage_quota_bytes * 0.8)::INT8 as users_over_80,
            COUNT(*) FILTER (WHERE storage_quota_bytes > 0 AND storage_used_bytes > storage_quota_bytes)::INT8 as users_over_quota
        FROM auth.users
        WHERE is_external = false
        "#
    )
    .fetch_one(db_pool.as_ref())
    .await
    .map_err(|e| AppError::internal_error(format!("Database query failed: {}", e)))?;

    use sqlx::Row;
    let total_quota: i64 = stats_row.get("total_quota_bytes");
    let total_used: i64 = stats_row.get("total_used_bytes");
    let usage_percent = if total_quota > 0 {
        (total_used as f64 / total_quota as f64) * 100.0
    } else {
        0.0
    };

    let stats = DashboardStatsDto {
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        auth_enabled: true,
        oidc_configured: auth_app.oidc_enabled(),
        quotas_enabled: true, // Feature flag could be checked here
        total_users: stats_row.get("total_users"),
        active_users: stats_row.get("active_users"),
        admin_users: stats_row.get("admin_users"),
        total_quota_bytes: total_quota,
        total_used_bytes: total_used,
        storage_usage_percent: (usage_percent * 100.0).round() / 100.0,
        users_over_80_percent: stats_row.get("users_over_80"),
        users_over_quota: stats_row.get("users_over_quota"),
        registration_enabled: {
            if let Some(svc) = state.admin_settings_service.as_ref() {
                svc.get_registration_enabled().await
            } else {
                true // default: enabled
            }
        },
    };

    Ok(Json(stats))
}

// ============================================================================
// User Management
// ============================================================================

/// GET /api/admin/users?limit=50&offset=0 — list all users
#[utoipa::path(
    get,
    path = "/api/admin/users",
    params(
        ("limit" = Option<i64>, Query, description = "Max users to return (default 100, max 500)"),
        ("offset" = Option<i64>, Query, description = "Pagination offset"),
        ("summary" = Option<bool>, Query, description = "Return the compact management-table projection")
    ),
    responses(
        (status = 200, description = "List of users"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Query(query): Query<ListUsersQueryDto>,
) -> Result<impl IntoResponse, AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let limit = query.limit.unwrap_or(100).min(500);
    let offset = query.offset.unwrap_or(0);

    // Admin surface must show *every* account for audit — grant-only
    // magic-link / OCM recipients (is_external = true) included. The
    // internal-only variant is used by system address book / sharee
    // search, where surfacing externals would leak identities. See
    // `auth_application_service::list_users` doc for the split.
    let users = if query.summary.unwrap_or(false) {
        AdminUsersPayload::Summary(
            auth.auth_application_service
                .list_user_summaries_including_external_with_perms(
                    state.authorization.as_ref(),
                    auth_user.id,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::from)?,
        )
    } else {
        AdminUsersPayload::Full(
            auth.auth_application_service
                .list_users_including_external_with_perms(
                    state.authorization.as_ref(),
                    auth_user.id,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::from)?,
        )
    };

    let total = auth
        .auth_application_service
        .count_users_efficient()
        .await
        .unwrap_or(0);

    Ok(Json(AdminUsersPageResponse {
        users,
        total,
        limit,
        offset,
    }))
}

/// GET /api/admin/users/:id — get single user
#[utoipa::path(
    get,
    path = "/api/admin/users/{id}",
    params(("id" = String, Path, description = "User UUID")),
    responses(
        (status = 200, description = "User details"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "User not found")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let id = Uuid::parse_str(&id).map_err(|_| AppError::bad_request("Invalid UUID"))?;

    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let user = auth
        .auth_application_service
        .get_user_admin(id)
        .await
        .map_err(|e| AppError::not_found(format!("User not found: {}", e)))?;

    Ok(Json(user))
}

/// DELETE /api/admin/users/:id — delete a user
#[utoipa::path(
    delete,
    path = "/api/admin/users/{id}",
    params(("id" = String, Path, description = "User UUID")),
    responses(
        (status = 200, description = "User deleted"),
        (status = 400, description = "Cannot delete own account"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;

    let id = Uuid::parse_str(&id).map_err(|_| AppError::bad_request("Invalid UUID"))?;

    // Prevent self-deletion
    if admin_id == id {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Cannot delete your own account",
            "SelfDeletion",
        ));
    }

    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    auth.auth_application_service
        .delete_user_admin(id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to delete user: {}", e)))?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "message": "User deleted successfully"
        })),
    ))
}

/// PUT /api/admin/users/:id/role — change user role
#[utoipa::path(
    put,
    path = "/api/admin/users/{id}/role",
    params(("id" = String, Path, description = "User UUID")),
    responses(
        (status = 200, description = "Role updated"),
        (status = 400, description = "Cannot change own role"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn update_user_role(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
    Json(dto): Json<UpdateUserRoleDto>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;

    let id = Uuid::parse_str(&id).map_err(|_| AppError::bad_request("Invalid UUID"))?;

    // Prevent changing own role
    if admin_id == id {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Cannot change your own role",
            "SelfRoleChange",
        ));
    }

    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    auth.auth_application_service
        .change_user_role(id, &dto.role)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to change role: {}", e)))?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "message": format!("User role updated to '{}'", dto.role)
        })),
    ))
}

/// PUT /api/admin/users/:id/active — activate/deactivate user
#[utoipa::path(
    put,
    path = "/api/admin/users/{id}/active",
    params(("id" = String, Path, description = "User UUID")),
    responses(
        (status = 200, description = "User active status updated"),
        (status = 400, description = "Cannot deactivate own account"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn update_user_active(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
    Json(dto): Json<UpdateUserActiveDto>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;

    let id = Uuid::parse_str(&id).map_err(|_| AppError::bad_request("Invalid UUID"))?;

    // Prevent deactivating yourself
    if admin_id == id && !dto.active {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Cannot deactivate your own account",
            "SelfDeactivation",
        ));
    }

    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    auth.auth_application_service
        .set_user_active(id, dto.active)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to update user status: {}", e)))?;

    let status = if dto.active {
        "activated"
    } else {
        "deactivated"
    };
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "message": format!("User {}", status)
        })),
    ))
}

/// PUT /api/admin/users/:id/quota — update user storage quota
#[utoipa::path(
    put,
    path = "/api/admin/users/{id}/quota",
    params(("id" = String, Path, description = "User UUID")),
    responses(
        (status = 200, description = "Quota updated"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn update_user_quota(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(dto): Json<UpdateUserQuotaDto>,
) -> Result<impl IntoResponse, AppError> {
    let id = Uuid::parse_str(&id).map_err(|_| AppError::bad_request("Invalid UUID"))?;

    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    auth.auth_application_service
        .update_user_quota(id, dto.quota_bytes)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to update quota: {}", e)))?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "message": "User quota updated",
            "quota_bytes": dto.quota_bytes,
        })),
    ))
}

// ============================================================================
// Admin User Creation & Password Reset
// ============================================================================

/// POST /api/admin/users — create a new user (admin only)
#[utoipa::path(
    post,
    path = "/api/admin/users",
    responses(
        (status = 201, description = "User created"),
        (status = 400, description = "Invalid user data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<AdminCreateUserDto>,
) -> Result<impl IntoResponse, AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let user = auth
        .auth_application_service
        .admin_create_user(dto)
        .await
        .map_err(|e| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                format!("Failed to create user: {}", e),
                "CreateUserFailed",
            )
        })?;

    Ok((StatusCode::CREATED, Json(user)))
}

/// PUT /api/admin/users/:id/password — reset a user's password (admin only)
#[utoipa::path(
    put,
    path = "/api/admin/users/{id}/password",
    params(("id" = String, Path, description = "User UUID")),
    responses(
        (status = 200, description = "Password reset"),
        (status = 400, description = "Invalid password"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn reset_user_password(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(dto): Json<AdminResetPasswordDto>,
) -> Result<impl IntoResponse, AppError> {
    let id = Uuid::parse_str(&id).map_err(|_| AppError::bad_request("Invalid UUID"))?;

    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    auth.auth_application_service
        .admin_reset_password(id, &dto.new_password)
        .await
        .map_err(|e| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                format!("Failed to reset password: {}", e),
                "ResetPasswordFailed",
            )
        })?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "message": "Password reset successfully"
        })),
    ))
}

/// POST /api/admin/users/{id}/promote-to-internal — flip an external
/// (grant-only) account into a normal internal account, provisioning
/// its personal drive on the way. The deployment MUST have magic-link
/// login enabled (the admin doesn't set the user's password on their
/// behalf, so the promoted user needs some way to log in). Refuses
/// OIDC-linked users and users who are already internal.
#[utoipa::path(
    post,
    path = "/api/admin/users/{id}/promote-to-internal",
    params(("id" = String, Path, description = "Target user id")),
    responses(
        (status = 200, description = "User promoted", body = UserDto),
        (status = 400, description = "Magic-link login is disabled on this deployment"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required (or target is OIDC-linked)"),
        (status = 404, description = "User not found"),
        (status = 409, description = "User is already internal"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn admin_promote_external_to_internal(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let target_id = Uuid::parse_str(&id).map_err(|_| AppError::bad_request("Invalid UUID"))?;

    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let dto = auth
        .auth_application_service
        .admin_promote_external_to_internal(auth_user.id, target_id)
        .await
        .map_err(AppError::from)?;

    Ok((StatusCode::OK, Json(dto)))
}

// ============================================================================
// Registration Control
// ============================================================================

/// PUT /api/admin/settings/registration — enable/disable public registration
#[utoipa::path(
    put,
    path = "/api/admin/settings/registration",
    responses(
        (status = 200, description = "Registration setting updated"),
        (status = 400, description = "Missing field"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn set_registration_setting(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;

    let enabled = body
        .get("registration_enabled")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                "Missing boolean field 'registration_enabled'",
                "InvalidInput",
            )
        })?;

    let svc = state
        .admin_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Admin settings service not available"))?;

    svc.set_registration_enabled(enabled, admin_id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to save setting: {}", e)))?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "message": format!("Public registration {}", if enabled { "enabled" } else { "disabled" }),
            "registration_enabled": enabled,
        })),
    ))
}

async fn reextract_audio_metadata(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let audio_service = state
        .applications
        .audio_metadata_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Audio metadata service not available"))?;

    let result = audio_service
        .reextract_all_audio_metadata()
        .await
        .map_err(|e| {
            AppError::internal_error(format!("Failed to re-extract audio metadata: {}", e))
        })?;

    Ok(Json(serde_json::json!({
        "message": "Audio metadata extraction complete",
        "total": result.total,
        "processed": result.processed,
        "failed": result.failed,
    })))
}

/// Backfill image/video capture dates (EXIF / container creation time) into
/// `storage.file_metadata` for every existing media file, re-bucketing the
/// Photos timeline by real capture date. Safe to re-run (idempotent upsert).
async fn reextract_image_metadata(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let result = state
        .applications
        .media_metadata_service
        .reextract_all_image_metadata()
        .await
        .map_err(|e| {
            AppError::internal_error(format!("Failed to re-extract capture metadata: {}", e))
        })?;

    Ok(Json(serde_json::json!({
        "message": "Image/video capture-metadata extraction complete",
        "total": result.total,
        "processed": result.processed,
        "failed": result.failed,
    })))
}

// ─────────────────────────────────────────────────────
// SMTP diagnostics
// ─────────────────────────────────────────────────────
//
// The SMTP backend is configured exclusively via OXICLOUD_SMTP_* env
// vars (see docs/config/env.md). The admin UI uses these two endpoints
// purely for diagnostics:
//   - `get_smtp_info` shows the current runtime config (read-only — no
//     write endpoint exists; operators edit `.env` and restart).
//   - `send_smtp_test` sends a hardcoded confirmation mail to a
//     recipient supplied by the admin, returning the SMTP server's
//     response so the operator can correlate it with their relay logs.

/// GET /api/admin/smtp/info — read-only view of the running SMTP config.
#[utoipa::path(
    get,
    path = "/api/admin/smtp/info",
    responses(
        (status = 200, description = "Current SMTP settings", body = SmtpInfoDto),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
async fn get_smtp_info(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let smtp = &state.core.config.smtp;
    let info = SmtpInfoDto {
        enabled: smtp.is_enabled() && state.email_sender.is_some(),
        host: smtp.host.clone(),
        port: smtp.port,
        tls: match smtp.tls {
            crate::common::config::SmtpTlsMode::Starttls => "starttls".to_string(),
            crate::common::config::SmtpTlsMode::Tls => "tls".to_string(),
            crate::common::config::SmtpTlsMode::None => "none".to_string(),
        },
        from: smtp.from.clone(),
        user_state: if smtp.user.is_empty() {
            "<anon>"
        } else {
            "<set>"
        },
    };

    Ok(Json(info))
}

/// GET /api/admin/smtp/test/captured?to=<email> — test-only inbox lookup.
///
/// Returns the most recently captured outbound message for `to` when
/// `OXICLOUD_SMTP_MOCK=true`. In production / non-mock mode this
/// returns 404 to keep the endpoint inert.
async fn get_captured_email(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CapturedEmailQuery>,
) -> Result<impl IntoResponse, AppError> {
    if !std::env::var("OXICLOUD_SMTP_MOCK")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
    {
        return Err(AppError::not_found(
            "Capture endpoint is only available when OXICLOUD_SMTP_MOCK=true",
        ));
    }

    let recipient = params.to.trim();
    if recipient.is_empty() {
        return Err(AppError::bad_request("`to` query parameter is required"));
    }

    let Some(mock) = state.mock_email_sender.as_ref() else {
        return Err(AppError::not_found(
            "Mock sender is not active (set OXICLOUD_SMTP_MOCK=true)",
        ));
    };

    match mock.last_for(recipient).await {
        Some(captured) => Ok(Json((*captured).clone())),
        None => Err(AppError::not_found(format!(
            "No captured message for '{}'",
            recipient
        ))),
    }
}

#[derive(Debug, serde::Deserialize)]
struct CapturedEmailQuery {
    to: String,
}

/// POST /api/admin/smtp/test — send a diagnostic email to `dto.to`.
///
/// Returns 200 regardless of SMTP outcome; the body's `success` flag
/// + `code`/`message` (or `error`) tell the frontend what to render.
/// This keeps SMTP-level failures (4xx/5xx replies, connection
/// timeouts) as ordinary diagnostic data rather than HTTP errors.
#[utoipa::path(
    post,
    path = "/api/admin/smtp/test",
    request_body = SendSmtpTestDto,
    responses(
        (status = 200, description = "Send attempt completed", body = SmtpTestResultDto),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 503, description = "SMTP not configured"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
async fn send_smtp_test(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(dto): Json<SendSmtpTestDto>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;

    let recipient = dto.to.trim().to_string();
    if recipient.is_empty() {
        return Err(AppError::bad_request("Recipient address is required"));
    }

    let sender = state.email_sender.as_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SMTP is not configured (set OXICLOUD_SMTP_HOST in .env to enable)",
            "ServiceUnavailable",
        )
    })?;

    let message = crate::application::ports::email_sender::EmailMessage {
        to: recipient.clone(),
        subject: "OxiCloud SMTP test".to_string(),
        text_body: format!(
            "This is a diagnostic message sent from your OxiCloud instance.\n\
             \n\
             If you are reading this, your SMTP relay accepted the message — \
             outbound email is wired up correctly.\n\
             \n\
             Triggered by admin user id {} on {}.\n",
            admin_id,
            chrono::Utc::now().to_rfc3339(),
        ),
        html_body: None,
    };

    tracing::info!(
        target: "audit",
        event = "smtp.test_send",
        admin_id = %admin_id,
        recipient = %recipient,
    );

    let result = match sender.send(message).await {
        Ok(outcome) => {
            tracing::info!(
                target: "audit",
                event = "smtp.test_send_ok",
                admin_id = %admin_id,
                recipient = %recipient,
                code = outcome.code,
                message = %outcome.message,
            );
            SmtpTestResultDto {
                success: true,
                code: Some(outcome.code),
                message: Some(outcome.message),
                error: None,
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "audit",
                event = "smtp.test_send_failed",
                admin_id = %admin_id,
                recipient = %recipient,
                error = %e.message,
            );
            SmtpTestResultDto {
                success: false,
                code: None,
                message: None,
                error: Some(e.message),
            }
        }
    };

    Ok(Json(result))
}

// ---- Plugin management -----------------------------------------------------

/// Resolve the plugin-management port, or 503 when plugins are compiled out or
/// disabled via `OXICLOUD_ENABLE_PLUGINS`. The admin UI treats this 503 as the
/// "plugins disabled" state rather than an error.
fn plugin_mgmt(state: &AppState) -> Result<&Arc<dyn PluginManagementPort>, AppError> {
    state.plugin_management.as_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Plugins are disabled",
            "PluginsDisabled",
        )
    })
}

/// Map a management-layer error to an HTTP error. NotFound → 404, IdExists →
/// 409, Rejected → 400 (with the stable reason key in the message), Io → 500.
fn map_mgmt_err(err: &PluginMgmtError) -> AppError {
    match err {
        PluginMgmtError::NotFound => AppError::not_found("Plugin not found"),
        PluginMgmtError::IdExists => {
            AppError::conflict("A plugin with this id is already installed")
        }
        PluginMgmtError::Rejected(reason) => AppError::new(
            StatusCode::BAD_REQUEST,
            format!("Plugin rejected: {reason}"),
            "PluginRejected",
        ),
        PluginMgmtError::Io(msg) => {
            AppError::internal_error(format!("Plugin operation failed: {msg}"))
        }
    }
}

/// GET /api/admin/plugins — list installed plugins.
pub async fn list_plugins(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let mgmt = plugin_mgmt(&state)?;
    let plugins: Vec<PluginInfoDto> = mgmt.list().into_iter().map(PluginInfoDto::from).collect();
    // `enabled` reports that the plugin *subsystem* is active (reaching here
    // means it is — `plugin_mgmt` returns 503 otherwise, which the UI reads as
    // the disabled state). Per-plugin enablement is each entry's own `enabled`.
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "enabled": true, "plugins": plugins })),
    ))
}

/// PUT /api/admin/plugins/{id}/enabled — enable or disable a plugin.
pub async fn set_plugin_enabled(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
    Json(dto): Json<SetEnabledDto>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;
    let mgmt = plugin_mgmt(&state)?;
    mgmt.set_enabled(&id, dto.enabled)
        .map_err(|e| map_mgmt_err(&e))?;

    if dto.enabled {
        tracing::info!(
            target: "audit",
            event = "plugin.enabled",
            plugin_id = %id,
            admin_id = %admin_id,
            "👮🏻‍♂️ plugin enabled by admin"
        );
    } else {
        tracing::info!(
            target: "audit",
            event = "plugin.disabled",
            plugin_id = %id,
            admin_id = %admin_id,
            "👮🏻‍♂️ plugin disabled by admin"
        );
    }

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "message": if dto.enabled { "Plugin enabled" } else { "Plugin disabled" },
            "id": id,
            "enabled": dto.enabled,
        })),
    ))
}

/// POST /api/admin/plugins — install a plugin from a multipart body with a
/// single `bundle` part: a `.zip` containing `plugin.toml` and its `.wasm`.
pub async fn install_plugin(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;
    let mgmt = plugin_mgmt(&state)?;

    let mut bundle: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(format!("Invalid multipart body: {e}")))?
    {
        if field.name() == Some("bundle") {
            bundle = Some(
                field
                    .bytes()
                    .await
                    .map_err(|e| AppError::bad_request(format!("Invalid bundle part: {e}")))?
                    .to_vec(),
            );
        }
    }

    let bundle = match bundle {
        Some(b) => b,
        None => {
            tracing::warn!(
                target: "audit",
                event = "plugin.install_rejected",
                reason = "missing_part",
                admin_id = %admin_id,
                "👮🏻‍♂️ plugin install rejected: missing 'bundle' part"
            );
            return Err(AppError::bad_request("A 'bundle' (.zip) part is required"));
        }
    };

    match mgmt.install_bundle(bundle) {
        Ok(info) => {
            tracing::info!(
                target: "audit",
                event = "plugin.installed",
                plugin_id = %info.id,
                admin_id = %admin_id,
                "👮🏻‍♂️ plugin installed by admin"
            );
            Ok((StatusCode::CREATED, Json(PluginInfoDto::from(info))))
        }
        Err(e) => {
            tracing::warn!(
                target: "audit",
                event = "plugin.install_rejected",
                reason = e.reason(),
                admin_id = %admin_id,
                "👮🏻‍♂️ plugin install rejected"
            );
            Err(map_mgmt_err(&e))
        }
    }
}

/// DELETE /api/admin/plugins/{id} — uninstall a plugin and delete its files.
pub async fn delete_plugin(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;
    let mgmt = plugin_mgmt(&state)?;
    mgmt.remove(&id).map_err(|e| map_mgmt_err(&e))?;

    tracing::info!(
        target: "audit",
        event = "plugin.removed",
        plugin_id = %id,
        admin_id = %admin_id,
        "👮🏻‍♂️ plugin removed by admin"
    );

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "message": "Plugin removed", "id": id })),
    ))
}

/// GET /api/admin/plugins/{id}/logs — a filtered, paginated page of a plugin's
/// structured log entries (newest first).
pub async fn get_plugin_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<PluginLogQueryDto>,
) -> Result<impl IntoResponse, AppError> {
    let mgmt = plugin_mgmt(&state)?;

    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    let page = mgmt
        .read_logs(
            &id,
            LogQuery {
                level: q.level,
                search: q.search,
                offset,
                limit,
            },
        )
        .await
        .map_err(|e| map_mgmt_err(&e))?;

    Ok(Json(PluginLogPageDto::from_page(page, limit, offset)))
}

/// DELETE /api/admin/plugins/{id}/logs — wipe a plugin's persisted logs.
pub async fn clear_plugin_logs(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;
    let mgmt = plugin_mgmt(&state)?;
    mgmt.clear_logs(&id).await.map_err(|e| map_mgmt_err(&e))?;

    tracing::info!(
        target: "audit",
        event = "plugin.logs_cleared",
        plugin_id = %id,
        admin_id = %admin_id,
        "👮🏻‍♂️ plugin logs cleared by admin"
    );

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "message": "Plugin logs cleared", "id": id })),
    ))
}

/// GET /api/admin/plugins/{id}/logs/stream — Server-Sent Events live tail. Each
/// `message` event carries one new log entry (JSON); a `lagged` event signals
/// the client should resync after falling behind. Auth rides the access cookie,
/// so `EventSource` works without setting headers.
pub async fn stream_plugin_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    use tokio_stream::StreamExt;
    use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

    let mgmt = plugin_mgmt(&state)?;
    if !mgmt.list().iter().any(|p| p.id == id) {
        return Err(AppError::not_found("Plugin not found"));
    }

    let rx = mgmt.subscribe_logs();
    let want = id;
    let stream = BroadcastStream::new(rx).filter_map(move |res| match res {
        Ok(ev) if ev.plugin_id == want => {
            let dto = PluginLogEntryDto::from(ev.entry);
            let event = Event::default()
                .json_data(&dto)
                .unwrap_or_else(|_| Event::default().comment("serialize error"));
            Some(Ok::<Event, std::convert::Infallible>(event))
        }
        Ok(_) => None,
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            Some(Ok(Event::default().event("lagged").data(n.to_string())))
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// GET /api/admin/plugins/{id}/retention — the plugin's effective retention.
pub async fn get_plugin_retention(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let mgmt = plugin_mgmt(&state)?;
    let settings = mgmt
        .get_retention(&id)
        .await
        .map_err(|e| map_mgmt_err(&e))?;
    Ok(Json(PluginRetentionDto::from(settings)))
}

/// PUT /api/admin/plugins/{id}/retention — set the plugin's retention policy.
pub async fn set_plugin_retention(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
    Json(dto): Json<PluginRetentionDto>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;
    let mgmt = plugin_mgmt(&state)?;
    mgmt.set_retention(&id, dto.into())
        .await
        .map_err(|e| map_mgmt_err(&e))?;

    tracing::info!(
        target: "audit",
        event = "plugin.retention_updated",
        plugin_id = %id,
        admin_id = %admin_id,
        retention_days = dto.retention_days,
        max_bytes = dto.max_bytes,
        "👮🏻‍♂️ plugin log retention updated by admin"
    );

    Ok((StatusCode::OK, Json(dto)))
}

/// GET /api/admin/drives — list every drive on the system, admin-only.
///
/// Distinct from `GET /api/drives`, which is the caller's own listing
/// filtered through `role_grants`. An admin who creates a shared drive
/// for someone else has no grant on it — but the admin panel still
/// needs to see the drive (to audit, to manage, to delete). The admin
/// guard at the handler edge is the access control; no role filtering
/// happens in the repo (see `drive_repository::list_all`).
///
/// Returns rows ordered by display name. `caller_role` is omitted —
/// the admin is not necessarily a drive member, so the field would be
/// misleading here.
#[utoipa::path(
    get,
    path = "/api/admin/drives",
    responses(
        (status = 200, description = "Every drive on the system", body = Vec<DriveDto>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn list_all_drives(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let drives = state
        .drive_repo
        .list_all()
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to list drives: {e}")))?;
    let dtos: Vec<DriveDto> = drives.into_iter().map(DriveDto::from).collect();
    Ok((StatusCode::OK, Json(dtos)))
}

/// GET /api/admin/drives/{id}/members — list every role grant on a drive,
/// admin-only.
///
/// Distinct from `GET /api/drives/{id}/members` which goes through
/// `DriveManagementService::list_members` and requires `Permission::Read`
/// on the drive. The admin who created the drive for someone else has
/// no role on it, so the user-facing endpoint would 404 for them.
///
/// This endpoint reuses the engine's `list_grants_on_resource` directly
/// — same query, same shape, just gated by the admin middleware instead
/// of by `authz.require`. Returns the same `Vec<GrantDto>` so the
/// frontend renders it through the existing grant types.
#[utoipa::path(
    get,
    path = "/api/admin/drives/{id}/members",
    params(("id" = Uuid, Path, description = "Drive UUID")),
    responses(
        (status = 200, description = "Role grants on the drive", body = Vec<GrantDto>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn list_drive_members_admin(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(drive_id): axum::extract::Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let grants = state
        .authorization
        .list_grants_on_resource(Resource::Drive(drive_id))
        .await
        .map_err(AppError::from)?;
    let dtos: Vec<GrantDto> = grants.into_iter().map(GrantDto::from).collect();
    Ok((StatusCode::OK, Json(dtos)))
}

/// Body for `POST /api/admin/drives/{id}/members` and
/// `PATCH /api/admin/drives/{id}/members/{kind}/{sid}` — same wire shape
/// as the user-facing endpoints, kept here so this handler doesn't pull
/// in the regular drive-handler module's DTOs (which would create a
/// circular feel between admin and user-facing surfaces).
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct AdminAddDriveMemberDto {
    pub subject: SubjectDto,
    pub role: RoleDto,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct AdminUpdateDriveMemberDto {
    pub role: RoleDto,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn admin_parse_subject(kind: SubjectTypeDto, id: Uuid) -> Subject {
    match kind {
        SubjectTypeDto::User => Subject::User(id),
        SubjectTypeDto::Group => Subject::Group(id),
        SubjectTypeDto::Token => Subject::Token(id),
    }
}

/// POST /api/admin/drives/{id}/members — add or refresh a member's role
/// without holding `Manage` on the drive. Admin-only; bypasses the
/// per-drive authz check via the `caller_is_admin = true` argument on
/// `DriveManagementService::set_member_role`. Personal-drive guard and
/// last-owner protection still apply.
#[utoipa::path(
    post,
    path = "/api/admin/drives/{id}/members",
    params(("id" = Uuid, Path, description = "Drive UUID")),
    request_body = AdminAddDriveMemberDto,
    responses(
        (status = 201, description = "Member added", body = GrantDto),
        (status = 400, description = "Validation error (e.g. last-owner constraint)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 405, description = "Personal drive — membership is immutable"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn add_drive_member_admin(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    axum::extract::Path(drive_id): axum::extract::Path<Uuid>,
    Json(dto): Json<AdminAddDriveMemberDto>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;
    let subject = admin_parse_subject(dto.subject.kind, dto.subject.id);
    let grant = state
        .drive_management_service
        .set_member_role(
            admin_id,
            true,
            drive_id,
            subject,
            dto.role.into(),
            dto.expires_at,
        )
        .await
        .map_err(AppError::from)?;
    Ok((StatusCode::CREATED, Json(GrantDto::from(grant))))
}

/// PATCH /api/admin/drives/{id}/members/{kind}/{sid} — change a member's
/// role / expiry as an admin. Same admin-bypass shape as
/// `add_drive_member_admin`.
#[utoipa::path(
    patch,
    path = "/api/admin/drives/{id}/members/{kind}/{sid}",
    params(
        ("id" = Uuid, Path, description = "Drive UUID"),
        ("kind" = String, Path, description = "Subject kind: user|group|token"),
        ("sid" = Uuid, Path, description = "Subject UUID"),
    ),
    request_body = AdminUpdateDriveMemberDto,
    responses(
        (status = 200, description = "Member role updated", body = GrantDto),
        (status = 400, description = "Validation error (e.g. last-owner demotion)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 405, description = "Personal drive — membership is immutable"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn update_drive_member_admin(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    axum::extract::Path((drive_id, kind, subject_id)): axum::extract::Path<(
        Uuid,
        SubjectTypeDto,
        Uuid,
    )>,
    Json(dto): Json<AdminUpdateDriveMemberDto>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;
    let subject = admin_parse_subject(kind, subject_id);
    let grant = state
        .drive_management_service
        .set_member_role(
            admin_id,
            true,
            drive_id,
            subject,
            dto.role.into(),
            dto.expires_at,
        )
        .await
        .map_err(AppError::from)?;
    Ok((StatusCode::OK, Json(GrantDto::from(grant))))
}

/// DELETE /api/admin/drives/{id}/members/{kind}/{sid} — remove a
/// member as an admin. Bypasses `Manage`; keeps last-owner protection.
#[utoipa::path(
    delete,
    path = "/api/admin/drives/{id}/members/{kind}/{sid}",
    params(
        ("id" = Uuid, Path, description = "Drive UUID"),
        ("kind" = String, Path, description = "Subject kind: user|group|token"),
        ("sid" = Uuid, Path, description = "Subject UUID"),
    ),
    responses(
        (status = 204, description = "Member removed (or wasn't a member — idempotent)"),
        (status = 400, description = "Last-owner protection — promote another member first"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 405, description = "Personal drive — membership is immutable"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn remove_drive_member_admin(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    axum::extract::Path((drive_id, kind, subject_id)): axum::extract::Path<(
        Uuid,
        SubjectTypeDto,
        Uuid,
    )>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;
    let subject = admin_parse_subject(kind, subject_id);
    state
        .drive_management_service
        .remove_member(admin_id, true, drive_id, subject)
        .await
        .map_err(AppError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/admin/drives/{id}` — admin-only drive delete (D3b).
///
/// Same shape as the user-facing `DELETE /api/drives/{id}`, but
/// bypasses the per-drive `Manage` check (the admin guard at the
/// route edge is the access control). The remaining invariants —
/// default Personal drive is undeletable, drive must be empty — still
/// apply: an admin can't accidentally wipe a populated drive or the
/// default home folder of any user. Audit emits
/// `drive.deleted_via_admin` on success.
#[utoipa::path(
    delete,
    path = "/api/admin/drives/{id}",
    params(("id" = Uuid, Path, description = "Drive UUID")),
    responses(
        (status = 204, description = "Drive deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 405, description = "Default Personal drive — undeletable"),
        (status = 409, description = "Drive is not empty"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn delete_drive_admin(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    axum::extract::Path(drive_id): axum::extract::Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let admin_id = auth_user.id;
    state
        .drive_management_service
        .delete_drive(admin_id, true, drive_id)
        .await
        .map_err(AppError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

// ════════════════════════════════════════════════════════════════════════════
// Test-only sweep triggers (`/api/admin/internal/*`)
//
// Wraps the periodic background jobs (storage-usage reconciliation,
// blob garbage collection) behind admin-gated synchronous endpoints
// so Hurl / integration tests can wait for them deterministically
// rather than polling the cached value. Disabled at the handler edge
// when `features.enable_admin_internal_endpoints == false` — match
// the `/smtp/test/captured` convention so production deployments
// don't need a different route table.
// ════════════════════════════════════════════════════════════════════════════

/// Refusal when the test-only endpoints are disabled. Returns 404
/// rather than 403 to avoid leaking the route's existence (and the
/// corresponding config flag) to an unauthenticated probe — the
/// legitimate test runner sets the env explicitly.
fn internal_endpoints_disabled() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "endpoint not available" })),
    )
        .into_response()
}

/// `POST /api/admin/internal/trigger-sweep` — run the storage-usage
/// reconciliation sweep synchronously.
///
/// Test-only. Recomputes `users.storage_used_bytes` and
/// `drives.used_bytes` from `SUM(size) WHERE NOT is_trashed`, in the
/// same set-based UPDATEs the periodic ticker runs. Used by Hurl
/// suites that need to assert post-delete quota convergence without
/// waiting out the sweep interval (default 600 s).
#[utoipa::path(
    post,
    path = "/api/admin/internal/trigger-sweep",
    responses(
        (status = 200, description = "Sweep ran"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Endpoint disabled (set OXICLOUD_ENABLE_ADMIN_INTERNAL_ENDPOINTS=true)"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn internal_trigger_sweep(
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !state.core.config.features.enable_admin_internal_endpoints {
        return internal_endpoints_disabled();
    }
    let svc = match state.storage_usage_service.as_ref() {
        Some(s) => s,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "storage_usage_service not available",
                })),
            )
                .into_response();
        }
    };
    // Order matches the periodic ticker (`start_reconciliation_job`):
    // drive sweep first because the user sweep reads `drives.used_bytes`
    // (sum-of-personal-drives — `docs/plan/drive.md` §7). Running them
    // in the other order makes the user counter freeze on the previous
    // tick's drive numbers — invisible in steady state but breaks any
    // Hurl that trashes + sweeps within one call.
    if let Err(e) = svc.update_all_drives_storage_usage().await {
        return AppError::internal_error(format!("drive sweep failed: {e}")).into_response();
    }
    if let Err(e) = svc.update_all_users_storage_usage().await {
        return AppError::internal_error(format!("user sweep failed: {e}")).into_response();
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "ran": ["drives", "users"] })),
    )
        .into_response()
}

/// Query parameters for `POST /api/admin/internal/trigger-gc`.
///
/// `force=true` bypasses the orphan-grace window so the sweep reaps
/// just-orphaned blobs in the same call. Without this, a blob orphaned
/// less than `GC_ORPHAN_GRACE_SECS` (1 h) ago survives the sweep — the
/// grace exists so a concurrent uploader pinning a just-orphaned chunk
/// can't race the row-delete → file-unlink gap. Integration tests
/// don't have concurrent uploaders, so the test runner sets
/// `force=true` to make the sweep deterministic within a test's
/// runtime.
#[derive(Debug, serde::Deserialize, Default)]
pub struct InternalTriggerGcQuery {
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/admin/internal/trigger-gc` — run the blob garbage
/// collector synchronously.
///
/// Test-only. Drops `file_blobs` rows with `ref_count = 0` (subject
/// to the orphan-grace window) and their on-disk content. Same call
/// as the inline post-purge GC and the periodic blob-GC sweep — just
/// exposed under an admin route so Hurl can wait for it
/// deterministically. Add `?force=true` to bypass the grace window —
/// see [`InternalTriggerGcQuery`].
#[utoipa::path(
    post,
    path = "/api/admin/internal/trigger-gc",
    params(("force" = Option<bool>, Query, description = "Bypass the orphan-grace window (test-only)")),
    responses(
        (status = 200, description = "GC ran"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Endpoint disabled (set OXICLOUD_ENABLE_ADMIN_INTERNAL_ENDPOINTS=true)"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn internal_trigger_gc(
    State(state): State<Arc<AppState>>,
    Query(query): Query<InternalTriggerGcQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !state.core.config.features.enable_admin_internal_endpoints {
        return internal_endpoints_disabled();
    }
    let result = if query.force {
        state.core.dedup_service.garbage_collect_force().await
    } else {
        state.core.dedup_service.garbage_collect().await
    };
    match result {
        Ok((blobs_deleted, bytes_freed)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "blobs_deleted": blobs_deleted,
                "bytes_freed": bytes_freed,
                "forced": query.force,
            })),
        )
            .into_response(),
        Err(e) => AppError::internal_error(format!("gc failed: {e}")).into_response(),
    }
}

/// Query parameters for `POST /api/admin/internal/trigger-grant-cleanup`.
///
/// `force=true` sets the grace window to `0` for this call — deletes
/// every row whose `expires_at` is in the past, right now. Enables
/// Hurl regressions to plant a past-dated grant and immediately
/// observe it purged, without waiting the configured
/// `OXICLOUD_GRANT_CLEANUP_GRACE_DAYS` out.
///
/// Without `force`, the daemon's configured grace applies — the same
/// SQL the daily loop runs.
#[derive(Debug, serde::Deserialize, Default)]
pub struct InternalTriggerGrantCleanupQuery {
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/admin/internal/trigger-grant-cleanup` — run the expired-
/// grant purge synchronously.
///
/// Test-only. Deletes rows from `storage.role_grants` whose
/// `expires_at` is more than `grace_days` in the past (or immediately,
/// with `?force=true`). Same SQL as the periodic `GrantCleanupService`
/// daemon — exposed under an admin route so Hurl can wait for it
/// deterministically.
///
/// Response fields:
///   `grants_deleted` — count of rows removed by this invocation
///   `grace_days`    — the grace window that was applied (0 when
///                     `?force=true`, otherwise the config value)
///   `forced`        — echoes the query param
#[utoipa::path(
    post,
    path = "/api/admin/internal/trigger-grant-cleanup",
    params(("force" = Option<bool>, Query, description = "Force grace = 0 for this run (test-only)")),
    responses(
        (status = 200, description = "Purge ran"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Endpoint disabled (set OXICLOUD_ENABLE_ADMIN_INTERNAL_ENDPOINTS=true)"),
        (status = 503, description = "Grant-cleanup daemon disabled (OXICLOUD_GRANT_CLEANUP_ENABLED=false)"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn internal_trigger_grant_cleanup(
    State(state): State<Arc<AppState>>,
    Query(query): Query<InternalTriggerGrantCleanupQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !state.core.config.features.enable_admin_internal_endpoints {
        return internal_endpoints_disabled();
    }
    // Daemon may be disabled by config even when the internal-endpoint
    // gate is on. Return 503 (rather than 404 or 500) so integration
    // tests can distinguish "surface not exposed" from "surface
    // exposed but backing service off".
    let svc = match state.grant_cleanup_service.as_ref() {
        Some(s) => s,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "grant_cleanup_service not available (disabled by OXICLOUD_GRANT_CLEANUP_ENABLED=false)",
                })),
            )
                .into_response();
        }
    };
    // `force=true` collapses the grace window to zero for this run
    // only — the daemon's configured grace is untouched. Mirrors the
    // `trigger-gc?force=true` shape.
    let grace_override = if query.force { Some(0) } else { None };
    let grants_deleted = match svc.purge(grace_override).await {
        Ok(n) => n,
        Err(e) => {
            return AppError::internal_error(format!("grant cleanup failed: {e}")).into_response();
        }
    };
    let grace_days = grace_override.unwrap_or_else(|| svc.grace_days());
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "grants_deleted": grants_deleted,
            "grace_days": grace_days,
            "forced": query.force,
        })),
    )
        .into_response()
}

// ─────────────────────────────────────────────────────
// JobRegistry admin surface (`/api/admin/jobs/*`)
// ─────────────────────────────────────────────────────

/// `GET /api/admin/jobs` — enumerate every registered job with its
/// interval, next-run/last-run timestamps, and last outcome.
///
/// Production endpoint (no `OXICLOUD_ENABLE_ADMIN_INTERNAL_ENDPOINTS`
/// gate). Read-only, so no audit line — the standard admin-middleware
/// auth check is enough.
#[utoipa::path(
    get,
    path = "/api/admin/jobs",
    responses(
        (status = 200, description = "Jobs listed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn list_jobs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let summary = state.core.job_registry.snapshot().await;
    (StatusCode::OK, Json(summary)).into_response()
}

/// Query parameters for `POST /api/admin/jobs/{name}/trigger`.
///
/// `force=true` requests acceleration semantics from handlers that
/// support it (dedup_gc → grace = 0, grant_cleanup → grace = 0).
/// Silently ignored by handlers that don't (trash_cleanup,
/// storage_reconcile).
#[derive(serde::Deserialize)]
pub struct TriggerJobQuery {
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/admin/jobs/{name}/trigger` — dispatch one run off-schedule.
///
/// Returns the job's `JobOutcome` inline. Idempotent under exclusivity:
/// if the previous run is still in flight, the handler returns
/// `Ok { count: 0, extra: { "skipped": "already_running" } }` rather
/// than spawning a parallel dispatch.
///
/// Emits an audit line before dispatch — bulk-mutation side effects on
/// operator command belong on the audit stream.
#[utoipa::path(
    post,
    path = "/api/admin/jobs/{name}/trigger",
    params(("name" = String, Path, description = "Registered job name")),
    responses(
        (status = 200, description = "Dispatched; outcome inline"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Job not registered"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn trigger_job(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<TriggerJobQuery>,
) -> impl IntoResponse {
    use crate::infrastructure::scheduler::JobRunArgs;
    // Audit line BEFORE dispatch so an operator triggering something
    // that then hangs still leaves a trail.
    tracing::info!(
        target: "audit",
        event = "job.trigger",
        job = %name,
        force = query.force,
        "👮🏻‍♂️ Admin triggered job {} (force={})",
        name,
        query.force,
    );
    let args = JobRunArgs { force: query.force };
    match state.core.job_registry.trigger(&name, &args).await {
        Some(outcome) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "outcome": outcome })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "job not registered",
                "name": name,
            })),
        )
            .into_response(),
    }
}
