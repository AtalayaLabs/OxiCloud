use axum::{
    Router,
    extract::{Json, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::{delete, get, post, put},
};

use crate::application::dtos::settings_dto::{
    AdminCreateUserDto, AdminResetPasswordDto, DashboardStatsDto, ListUsersQueryDto,
    MigrationStateDto, SaveOidcSettingsDto, SaveStorageSettingsDto, StartMigrationDto,
    TestOidcConnectionDto, TestStorageConnectionDto, UpdateUserActiveDto, UpdateUserQuotaDto,
    UpdateUserRoleDto, VerifyMigrationDto,
};
use crate::application::ports::auth_ports::TokenServicePort;
use crate::common::di::AppState;
use crate::interfaces::errors::AppError;
use std::sync::Arc;
use uuid::Uuid;

/// Admin API routes — all require admin role.
pub fn admin_routes() -> Router<Arc<AppState>> {
    Router::new()
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
        .route("/settings/general", get(get_general_settings))
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
        // Registration control
        .route("/settings/registration", get(get_registration_setting))
        .route("/settings/registration", put(set_registration_setting))
        // Audio metadata
        .route("/audio/metadata/reextract", post(reextract_audio_metadata))
}

/// Validate JWT and require admin role. Returns (user_id, role).
async fn admin_guard(state: &AppState, headers: &HeaderMap) -> Result<(Uuid, String), AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").map(|s| s.to_string()))
        .or_else(|| {
            crate::interfaces::api::cookie_auth::extract_cookie_value(
                headers,
                crate::interfaces::api::cookie_auth::ACCESS_COOKIE,
            )
        })
        .ok_or_else(|| AppError::unauthorized("Authorization token required"))?;

    let claims = auth
        .token_service
        .validate_token(&token)
        .map_err(|e| AppError::unauthorized(format!("Invalid token: {}", e)))?;

    if claims.role != "admin" {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Admin access required",
            "Forbidden",
        ));
    }

    Ok((
        Uuid::parse_str(&claims.sub)
            .map_err(|_| AppError::internal_error("Invalid user ID in token"))?,
        claims.role,
    ))
}

/// GET /api/admin/settings/oidc — get OIDC settings for the admin panel
#[utoipa::path(
    get,
    path = "/api/admin/settings/oidc",
    responses(
        (status = 200, description = "OIDC settings"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    tag = "admin"
)]
pub async fn get_oidc_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn save_oidc_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(dto): Json<SaveOidcSettingsDto>,
) -> Result<impl IntoResponse, AppError> {
    let (user_id, _) = admin_guard(&state, &headers).await?;

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
    headers: HeaderMap,
    Json(dto): Json<TestOidcConnectionDto>,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn get_storage_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn save_storage_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(dto): Json<SaveStorageSettingsDto>,
) -> Result<impl IntoResponse, AppError> {
    let (user_id, _) = admin_guard(&state, &headers).await?;

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
    headers: HeaderMap,
    Json(dto): Json<TestStorageConnectionDto>,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn get_migration_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;
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
    tag = "admin"
)]
pub async fn start_migration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(dto): Json<StartMigrationDto>,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::services::migration_blob_backend::MigrationStatus;

    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn pause_migration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::services::migration_blob_backend::MigrationStatus;
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn resume_migration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::services::migration_blob_backend::MigrationStatus;
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn complete_migration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::services::migration_blob_backend::MigrationStatus;
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn verify_migration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(dto): Json<VerifyMigrationDto>,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn generate_encryption_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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

/// GET /api/admin/settings/general — system overview (backward compat)
#[utoipa::path(
    get,
    path = "/api/admin/settings/general",
    responses(
        (status = 200, description = "General system settings"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    tag = "admin"
)]
pub async fn get_general_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let user_count = auth
        .auth_application_service
        .count_users_efficient()
        .await
        .unwrap_or(0);
    let oidc_configured = auth.auth_application_service.oidc_enabled();

    Ok(Json(serde_json::json!({
        "server_version": env!("CARGO_PKG_VERSION"),
        "auth_enabled": true,
        "total_users": user_count,
        "oidc_configured": oidc_configured,
    })))
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
    tag = "admin"
)]
pub async fn get_dashboard_stats(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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

    // Use direct SQL for aggregated stats — more efficient than loading all users
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
        ("offset" = Option<i64>, Query, description = "Pagination offset")
    ),
    responses(
        (status = 200, description = "List of users"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    tag = "admin"
)]
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ListUsersQueryDto>,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let limit = query.limit.unwrap_or(100).min(500);
    let offset = query.offset.unwrap_or(0);

    let users = auth
        .auth_application_service
        .list_users(limit, offset)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to list users: {}", e)))?;

    let total = auth
        .auth_application_service
        .count_users_efficient()
        .await
        .unwrap_or(0);

    Ok(Json(serde_json::json!({
        "users": users,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
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
    tag = "admin"
)]
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let (admin_id, _) = admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn update_user_role(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(dto): Json<UpdateUserRoleDto>,
) -> Result<impl IntoResponse, AppError> {
    let (admin_id, _) = admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn update_user_active(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(dto): Json<UpdateUserActiveDto>,
) -> Result<impl IntoResponse, AppError> {
    let (admin_id, _) = admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn update_user_quota(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(dto): Json<UpdateUserQuotaDto>,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(dto): Json<AdminCreateUserDto>,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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
    tag = "admin"
)]
pub async fn reset_user_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(dto): Json<AdminResetPasswordDto>,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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

// ============================================================================
// Registration Control
// ============================================================================

/// GET /api/admin/settings/registration — check if public registration is enabled
#[utoipa::path(
    get,
    path = "/api/admin/settings/registration",
    responses(
        (status = 200, description = "Registration setting"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    tag = "admin"
)]
pub async fn get_registration_setting(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

    let svc = state
        .admin_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Admin settings service not available"))?;

    let val = svc.get_registration_enabled().await;

    Ok(Json(serde_json::json!({
        "registration_enabled": val,
    })))
}

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
    tag = "admin"
)]
pub async fn set_registration_setting(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, AppError> {
    let (admin_id, _) = admin_guard(&state, &headers).await?;

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
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    admin_guard(&state, &headers).await?;

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
