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
    AdminCreateUserDto, AdminResetPasswordDto, DashboardStatsDto, DriveKindUsageDto,
    ListSessionsQueryDto, ListUsersQueryDto, MigrationStateDto, SaveOidcSettingsDto,
    SaveStorageSettingsDto, SendSmtpTestDto, SmtpInfoDto, SmtpTestResultDto, StartMigrationDto,
    TestOidcConnectionDto, TestStorageConnectionDto, UpdateUserActiveDto, UpdateUserQuotaDto,
    UpdateUserRoleDto,
};
use crate::application::dtos::user_dto::{FullUserDto, UserDto};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::plugin_ports::{LogQuery, PluginManagementPort, PluginMgmtError};
// JobStoreProvider is used only by the storage-migration shims below,
// but the compiler needs the trait in scope for method resolution on
// the concrete `PgJobStoreProvider` that lives on `AppState`.
use crate::common::di::AppState;
use crate::domain::repositories::drive_repository::DriveRepository;
use crate::domain::services::authorization::{Resource, Subject};
use crate::infrastructure::scheduler::{JobStoreProvider, PausedRunBrief};
use crate::interfaces::api::handlers::dedup_handler::{get_stats, recalculate_stats};
use crate::interfaces::api::handlers::search_handler::clear_search_cache;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use std::sync::Arc;
use uuid::Uuid;

#[derive(serde::Serialize)]
#[serde(untagged)]
enum AdminUsersPayload {
    /// Fat-`UserDto` per row. Emitted when `?summary=false` — legacy
    /// path retained until the FE drops the `summary=false` query
    /// (rare; the SPA uses `summary=true` for the paginated table).
    Full(Vec<UserDto>),
    /// `FullUserDto` per row — same shape one row of the /me
    /// response's embedded `full` carries. Emitted when
    /// `?summary=true`. The FE seeds `resolveUser` cache from
    /// `row.user` here (kills the per-row `/api/users/{id}` fetch).
    /// The old `AdminUserSummaryDto` returned here has been replaced
    /// by `FullUserDto`; see `docs/plan/userdto-refactor.md`.
    Summary(Vec<FullUserDto>),
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
        // Storage migration — thin shims over the recoverable-run
        // engine (job_name = "backend_migration"). Retained under
        // /storage/migration/* until the admin UI is rewired to
        // /api/admin/jobs/backend_migration/*; both paths route to
        // the same underlying JobRegistry dispatch. The old /complete
        // endpoint is retired — a finished run is a Completed row,
        // there's nothing to acknowledge.
        .route("/storage/migration", get(get_migration_status))
        .route("/storage/migration/start", post(start_migration))
        .route("/storage/migration/pause", post(pause_migration))
        .route("/storage/migration/resume", post(resume_migration))
        // K3 (storage-key-rotation): per-entry rotate trigger.
        // Normalises every blob on the named entry to its head-pair
        // format (legacy → v1, plaintext ↔ encrypted, old-key →
        // new-key). No readonly mode; safe under normal traffic.
        .route(
            "/storage/entries/{name}/rotate",
            post(trigger_backend_rotate),
        )
        // NOTE: /storage/migration/verify retired in slice 7 (see the
        // comment near where `verify_migration` used to live). Use
        // `POST /api/admin/jobs/blobs_consistency/trigger?storage=<name>`.
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
        // Session management (DPoP admin panel — see docs/plan/dpop.md
        // Gate 10). List is global cross-user with `?user_id=` narrow;
        // revoke sets `revoked=true` (row stays for audit).
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", delete(revoke_session))
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
        // JobRegistry admin surface — production, always-on,
        // audit-logged. See `docs/plan/job-registry.md` §Cross-cutting.
        // Retired the `/internal/trigger-sweep|gc|grant-cleanup` shims
        // that used to sit here (Stage 2 of the job-registry rollout).
        //
        // `/jobs` + `/jobs/{name}/trigger` cover every registered
        // JobHandler (periodic + recoverable — recoverable ones slot
        // in through `RecoverableAdapter`). The `/cancel` + `/runs`
        // + `/runs/{id}` triplet is recoverable-only — hitting them
        // on a stateless job silently gets an empty list / no-op
        // cancel, since no rows in `jobs.recoverable_runs` match.
        // See `docs/plan/job-registry.md` Part 2.
        .route("/jobs", get(list_jobs))
        .route("/jobs/{name}/trigger", post(trigger_job))
        .route("/jobs/{name}/cancel", post(cancel_job))
        .route("/jobs/{name}/pause", post(pause_job))
        .route("/jobs/{name}/runs", get(list_job_runs))
        .route("/jobs/{name}/runs/{id}", get(get_job_run))
        .route(
            "/jobs/{name}/runs/{id}/findings",
            get(list_job_run_findings),
        )
        // Retention cleanup — operator-triggered, not periodic.
        // See `purge_job_runs` docstring for the semantics.
        .route("/jobs/runs/purge", post(purge_job_runs))
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

/// GET /api/admin/storage/migration — current migration progress.
///
/// Shim over the recoverable-run engine: reads the latest
/// `backend_migration` run from `jobs.recoverable_runs` (via the
/// `JobStoreProvider`) and projects it into the legacy
/// `MigrationStateDto` shape the admin storage tab expects. When no
/// run has ever been triggered the response is an empty "idle" DTO —
/// same behaviour the old in-memory `MigrationState::default()`
/// produced.
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
    use crate::infrastructure::services::backend_migration_service::BACKEND_MIGRATION_JOB_NAME;

    let provider = state.core.job_store_provider.clone();
    let latest = provider
        .list_runs(BACKEND_MIGRATION_JOB_NAME, 1)
        .await
        .map_err(AppError::from)?
        .into_iter()
        .next();

    let Some(run) = latest else {
        return Ok(Json(idle_migration_dto()));
    };

    // Failed blobs are stored as findings, kind = "migration_failed".
    // Pull up to a reasonable ceiling — the DTO ships the full list,
    // and the admin UI truncates its own display.
    let findings = provider
        .list_findings(run.id, 500, 0)
        .await
        .map_err(AppError::from)?;
    let failed_blobs: Vec<String> = findings
        .into_iter()
        .filter(|f| f.kind == "migration_failed")
        .filter_map(|f| {
            f.detail
                .get("hash")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();

    Ok(Json(run_to_migration_dto(&run, failed_blobs)))
}

/// POST /api/admin/storage/migration/start — begin background migration.
///
/// Shim that forwards to `JobRegistry::trigger("backend_migration",
/// ...)`. `run_or_resume` (the RecoverableAdapter's inner dispatch)
/// resumes a Paused run or starts a fresh one — one endpoint covers
/// both. Exclusivity is enforced at the DB layer (the partial unique
/// index on `jobs.recoverable_runs`), so a second concurrent trigger
/// is a no-op that returns the existing run.
///
/// `StartMigrationDto.concurrency` is currently ignored — the
/// recoverable copy loop runs sequentially. Kept in the DTO for
/// wire-compat with the admin UI; will be honoured if a concurrency
/// knob is added later.
#[utoipa::path(
    post,
    path = "/api/admin/storage/migration/start",
    responses(
        (status = 200, description = "Migration started"),
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
    // Validate at the HTTP layer (before spawning) so unknown / no-op
    // targets get a synchronous 400 response instead of burning a
    // failed run row. The handler's own checks are second-line
    // defence for the resume path where args aren't repeated.
    let entries = &state.core.config.storage_entries;
    // Snapshot the active-name for the guard. Read lock held only
    // for the clone.
    let active = state
        .core
        .active_backend_name
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if entries.iter().all(|e| e.name != dto.target_name) {
        let available = if entries.is_empty() {
            "(none)".to_string()
        } else {
            entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(AppError::bad_request(format!(
            "unknown target entry `{}`. Available: [{available}]",
            dto.target_name
        )));
    }
    if dto.target_name == active {
        return Err(AppError::bad_request(format!(
            "target `{}` is the currently-active entry — pick a different entry to migrate to",
            dto.target_name
        )));
    }
    trigger_backend_migration(state, Some(dto.target_name)).await
}

/// POST /api/admin/storage/migration/pause — pause a running migration.
///
/// Shim over cooperative cancel: flips the run row's status to
/// `CancelRequested`; the recoverable handler polls between batches
/// and returns `Paused` at the next boundary. If nothing is running,
/// returns 200 with `paused: false` — matches the "no-op is fine"
/// contract of `/api/admin/jobs/{name}/cancel`.
#[utoipa::path(
    post,
    path = "/api/admin/storage/migration/pause",
    responses(
        (status = 200, description = "Pause signalled (or no-op)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn pause_migration(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::services::backend_migration_service::BACKEND_MIGRATION_JOB_NAME;

    tracing::info!(
        target: "audit",
        event = "backend_migration.pause_requested",
        "👮🏻‍♂️ Admin requested backend_migration pause"
    );

    let flipped = state
        .core
        .job_store_provider
        .request_cancel(BACKEND_MIGRATION_JOB_NAME)
        .await
        .map_err(AppError::from)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "paused":  flipped.is_some(),
            "run_id":  flipped,
            "message": if flipped.is_some() {
                "Pause requested — handler will yield at the next batch boundary"
            } else {
                "No running migration to pause"
            },
        })),
    ))
}

/// POST /api/admin/storage/migration/resume — resume a paused migration.
///
/// Same underlying trigger as `/start`: `run_or_resume` inspects the
/// latest row and picks Fresh / Resume / AlreadyActive at dispatch
/// time. Kept as a distinct endpoint for wire-compat.
#[utoipa::path(
    post,
    path = "/api/admin/storage/migration/resume",
    responses(
        (status = 200, description = "Migration resumed (or already running)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn resume_migration(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    // Resume path — no target_name in the body. The handler reads
    // it from `params.target_name` stamped on the original Fresh
    // open. Refuses gracefully via RunOutcome::Failed if there is
    // no Paused row to resume.
    trigger_backend_migration(state, None).await
}

// verify_migration endpoint retired (slice 7 of
// docs/plan/storage-multi-entry.md). It was a sample-based sanity
// check against the currently-effective target backend; superseded
// by `POST /api/admin/jobs/blobs_consistency/trigger?storage=<name>`
// which does a full walk against ANY named entry (not just the
// migration target), records structured findings per mismatch, and
// integrates with the standard runs / cancel / findings admin
// surface. Frontend "Verify integrity" button removed in the same
// slice.

/// Shared body for `start` / `resume` — both funnel through
/// `run_or_resume` via `JobRegistry::trigger`. Detaches into a
/// `tokio::spawn` so the HTTP response returns immediately — same
/// rationale as `trigger_job` above (browser timeout mid-await would
/// desync `current_run_start` from the actually-running task). The
/// admin UI polls `GET /storage/migration` for progress; the trigger
/// itself is fire-and-forget.
async fn trigger_backend_migration(
    state: Arc<AppState>,
    target_name: Option<String>,
) -> Result<axum::response::Response, AppError> {
    use crate::infrastructure::scheduler::JobRunArgs;
    use crate::infrastructure::services::backend_migration_service::BACKEND_MIGRATION_JOB_NAME;

    tracing::info!(
        target: "audit",
        event = "backend_migration.trigger_requested",
        target_name = target_name.as_deref().unwrap_or("<resume>"),
        "👮🏻‍♂️ Admin triggered backend_migration"
    );

    let registry = state.core.job_registry.clone();
    let args = JobRunArgs {
        storage: target_name,
        ..JobRunArgs::default()
    };
    tokio::spawn(async move {
        registry.trigger(BACKEND_MIGRATION_JOB_NAME, &args).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "message": "Migration dispatched — poll GET /api/admin/storage/migration for status",
            "detached": true,
        })),
    )
        .into_response())
}

/// POST /api/admin/storage/entries/{name}/rotate — trigger the
/// `backend_rotate` recoverable job on a specific entry.
///
/// Normalises every blob on `<name>` to the entry's head-pair
/// format: legacy → v1, plaintext ↔ encrypted, old-key → new-key.
/// See `docs/plan/storage-key-rotation.md` §"The rotation job".
///
/// Unlike migration, rotation does NOT engage read-only mode —
/// rewrites happen in place under normal traffic. Concurrent user
/// writes coexist safely.
///
/// The handler validates the entry name synchronously (400 on
/// unknown entry); the actual walk detaches into a
/// `tokio::spawn` so the HTTP call returns immediately.
#[utoipa::path(
    post,
    path = "/api/admin/storage/entries/{name}/rotate",
    responses(
        (status = 202, description = "Rotation dispatched"),
        (status = 400, description = "Unknown entry"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    params(
        ("name" = String, Path, description = "Storage entry name to rotate")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn trigger_backend_rotate(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    use crate::infrastructure::scheduler::JobRunArgs;
    use crate::infrastructure::services::backend_migration_service::BACKEND_MIGRATION_JOB_NAME;
    use crate::infrastructure::services::backend_rotate_service::BACKEND_ROTATE_JOB_NAME;

    // Synchronous entry-existence check — a bad name would fail the
    // run anyway, but returning 400 here spares the operator an
    // audit-log round-trip.
    let entries = &state.core.config.storage_entries;
    if entries.iter().all(|e| e.name != name) {
        let available = if entries.is_empty() {
            "(none)".to_string()
        } else {
            entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(AppError::bad_request(format!(
            "unknown storage entry `{name}`. Available: [{available}]"
        )));
    }

    // Refuse on non-active entry. `storage.blobs` describes what's on
    // the ACTIVE backend; walking it against a stale target produces a
    // `rotation_failed` finding per missing blob (pure noise) and can't
    // actually normalise anything the app reads. The right recipe for
    // "normalise a different backend" is: migrate to it (blobs land in
    // the head-pair's format on arrival — no rotation needed).
    let active = state
        .core
        .active_backend_name
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if name != active {
        return Err(AppError::bad_request(format!(
            "backend_rotate refuses non-active entry `{name}` — the DB blob registry \
             describes the active entry (`{active}`), so walking it against a stale \
             target produces spurious `rotation_failed` findings. Activate `{name}` \
             first via `Migrate & activate`, then rotate."
        )));
    }

    // Concurrency guard per plan: at most one encryption-touching
    // recoverable run at a time across the whole app. Rotation
    // rewrites blobs in place; migration copies + swaps; running
    // both simultaneously could interleave writes on the same
    // hash. Cheap check — `list_runs` limit 1 with the status
    // filter is an index scan.
    let provider = state.core.job_store_provider.clone();
    for job_name in [BACKEND_ROTATE_JOB_NAME, BACKEND_MIGRATION_JOB_NAME] {
        let in_flight = provider
            .list_runs(job_name, 5)
            .await
            .map_err(AppError::from)?
            .into_iter()
            .any(|r| {
                matches!(
                    r.status,
                    crate::infrastructure::scheduler::RunStatus::Running
                        | crate::infrastructure::scheduler::RunStatus::Paused
                        | crate::infrastructure::scheduler::RunStatus::CancelRequested
                )
            });
        if in_flight {
            return Err(AppError::bad_request(format!(
                "cannot start backend_rotate on `{name}` — `{job_name}` is already Running / \
                 Paused / CancelRequested. Wait for it to finish (or cancel via \
                 `POST /api/admin/jobs/{job_name}/cancel`)."
            )));
        }
    }

    tracing::info!(
        target: "audit",
        event = "backend_rotate.trigger_requested",
        target_name = %name,
        "👮🏻‍♂️ Admin triggered backend_rotate on `{name}`"
    );

    let registry = state.core.job_registry.clone();
    let args = JobRunArgs {
        storage: Some(name.clone()),
        ..JobRunArgs::default()
    };
    tokio::spawn(async move {
        registry.trigger(BACKEND_ROTATE_JOB_NAME, &args).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "message": format!("Rotation dispatched on `{name}` — poll GET /api/admin/jobs/{BACKEND_ROTATE_JOB_NAME} for progress"),
            "detached": true,
        })),
    )
        .into_response())
}

/// Idle-state DTO — no run has been triggered yet.
fn idle_migration_dto() -> MigrationStateDto {
    MigrationStateDto {
        status: "idle".to_string(),
        total_blobs: 0,
        migrated_blobs: 0,
        // `migrated_bytes` and `throughput_bytes_per_sec` are no
        // longer tracked — the recoverable engine bumps
        // `stats.scanned_count` (a blob-count aggregator), not a
        // bytes counter. The admin UI keeps these fields for
        // wire-compat; they read 0 / null.
        migrated_bytes: 0,
        failed_blobs: Vec::new(),
        started_at: None,
        completed_at: None,
        throughput_bytes_per_sec: None,
    }
}

/// Project a recoverable `RunSummary` into the admin UI's
/// `MigrationStateDto`. Byte-counter fields are always 0 / None —
/// see `idle_migration_dto`'s comment.
fn run_to_migration_dto(
    run: &crate::infrastructure::scheduler::RunSummary,
    failed_blobs: Vec<String>,
) -> MigrationStateDto {
    use crate::infrastructure::scheduler::RunStatus;

    // Fold CancelRequested into "paused" — from the admin UI's
    // point of view a cancel-in-flight is the "waiting for the
    // handler to yield" state. Same visual affordance as Paused.
    let status = match run.status {
        RunStatus::Running => "running",
        RunStatus::Paused => "paused",
        RunStatus::CancelRequested => "paused",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        // Cancelled is user-abandoned but terminal — same visual as
        // failed for the migration status endpoint (both mean "not
        // going to finish, look at findings/logs to know why").
        RunStatus::Cancelled => "cancelled",
    }
    .to_string();

    let (total_blobs, migrated_blobs) = match run.progress.as_ref() {
        Some(p) => (p.total, p.scanned),
        None => (
            0,
            run.stats
                .get("scanned_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        ),
    };

    MigrationStateDto {
        status,
        total_blobs,
        migrated_blobs,
        migrated_bytes: 0,
        failed_blobs,
        started_at: Some(run.started_at.to_rfc3339()),
        completed_at: run.completed_at.map(|d| d.to_rfc3339()),
        throughput_bytes_per_sec: None,
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
    // Fingerprint uses the same colon-hex rendering as the boot log,
    // the pair chain in admin/storage, and `oxicloud --fingerprint`.
    // Ed can cross-reference it against `.env` after pasting the key
    // in — if the fingerprints match, the key made it into the
    // config intact.
    let fingerprint =
        crate::common::config::fingerprint_from_base64_key(&key_b64).unwrap_or_else(|_| {
            // Should never happen — we JUST generated a 32-byte key
            // and base64-encoded it — but if the fingerprint helper
            // rejects, degrade gracefully rather than 500.
            "—".to_string()
        });

    Ok(Json(serde_json::json!({
        "key": key_b64,
        "fingerprint": fingerprint,
        "warning": "Store this key securely. If lost, encrypted data is IRRECOVERABLY LOST."
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

    // Per-drive-kind quota panel:
    //
    //   - **Personal** rolls up via the user envelope
    //     (`auth.users.storage_quota_bytes`; `= 0` means unlimited),
    //     because personal drives inherit their cap from the user per
    //     `docs/plan/drive.md`. The "N unlimited" here counts USERS
    //     with unlimited envelope, not drives.
    //   - **Shared** uses `storage.drives.quota_bytes` directly
    //     (`IS NULL` means unlimited).
    //
    // Both rows sum `used_bytes` — for personal that's
    // `auth.users.storage_used_bytes`, which is itself
    // `SUM(drives.used_bytes) WHERE kind='personal'` per the sweep
    // in `storage_usage_service.rs`. For shared it's the drive's own
    // `used_bytes`. Trashed files are excluded from both — see
    // `bug_trash_excluded_from_quota` for the known gap.
    let personal_row = sqlx::query(
        r#"
        SELECT
            COALESCE(SUM(storage_used_bytes)::INT8, 0) AS used_bytes,
            COALESCE(SUM(storage_quota_bytes) FILTER (WHERE storage_quota_bytes > 0)::INT8, 0) AS capped_quota_bytes,
            COUNT(*) FILTER (WHERE storage_quota_bytes = 0)::INT8 AS unlimited_count,
            COUNT(*) FILTER (WHERE storage_quota_bytes > 0)::INT8 AS capped_count
        FROM auth.users
        WHERE is_external = false
        "#,
    )
    .fetch_one(db_pool.as_ref())
    .await
    .map_err(|e| AppError::internal_error(format!("Personal-drive stats failed: {}", e)))?;

    let shared_row = sqlx::query(
        r#"
        SELECT
            COALESCE(SUM(used_bytes)::INT8, 0) AS used_bytes,
            COALESCE(SUM(quota_bytes) FILTER (WHERE quota_bytes IS NOT NULL)::INT8, 0) AS capped_quota_bytes,
            COUNT(*) FILTER (WHERE quota_bytes IS NULL)::INT8 AS unlimited_count,
            COUNT(*) FILTER (WHERE quota_bytes IS NOT NULL)::INT8 AS capped_count
        FROM storage.drives
        WHERE kind::text = 'shared'
        "#,
    )
    .fetch_one(db_pool.as_ref())
    .await
    .map_err(|e| AppError::internal_error(format!("Shared-drive stats failed: {}", e)))?;

    let build_row = |kind: &str, row: sqlx::postgres::PgRow| DriveKindUsageDto {
        kind: kind.to_string(),
        used_bytes: row.get("used_bytes"),
        // Only surface the cap when there's at least one capped drive
        // — else the FE would render "0 / 0 (NaN%)" for a kind that's
        // entirely unlimited.
        capped_quota_bytes: {
            let capped_count: i64 = row.get("capped_count");
            if capped_count > 0 {
                Some(row.get("capped_quota_bytes"))
            } else {
                None
            }
        },
        unlimited_count: row.get("unlimited_count"),
        capped_count: row.get("capped_count"),
    };
    let drive_usage = vec![
        build_row("personal", personal_row),
        build_row("shared", shared_row),
    ];

    // Backend physical stats (post-dedup, post-encryption) —
    // rendered in the dashboard's "Backend Storage" card next to
    // the user-quota panel. Same source `StorageSettingsDto` uses;
    // cheap aggregate over `storage.blobs`.
    let dedup_stats = state.core.dedup_service.get_stats().await;

    let stats = DashboardStatsDto {
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        auth_enabled: true,
        oidc_configured: auth_app.oidc_enabled(),
        quotas_enabled: true, // Feature flag could be checked here
        total_users: stats_row.get("total_users"),
        active_users: stats_row.get("active_users"),
        admin_users: stats_row.get("admin_users"),
        drive_usage,
        users_over_80_percent: stats_row.get("users_over_80"),
        users_over_quota: stats_row.get("users_over_quota"),
        total_bytes_stored: Some(dedup_stats.total_bytes_stored as i64),
        dedup_ratio: Some(dedup_stats.dedup_ratio),
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

/// GET /api/admin/sessions?user_id=&include_revoked=&limit=&offset= — list sessions
///
/// Global cross-user listing by default. `user_id` narrows to one
/// user; omit for cross-user. `include_revoked=true` opts into
/// showing revoked / expired rows for forensics (default hides).
/// Response is `{sessions, limit, offset}` — no total count (would
/// require a second scan; the panel paginates on presence of
/// exactly `limit` rows returned).
#[utoipa::path(
    get,
    path = "/api/admin/sessions",
    params(
        ("user_id" = Option<String>, Query, description = "Narrow to one user (UUID); omit for cross-user"),
        ("include_revoked" = Option<bool>, Query, description = "Include revoked + expired rows (default false — active only)"),
        ("limit" = Option<i64>, Query, description = "Max rows to return (default 100, max 500)"),
        ("offset" = Option<i64>, Query, description = "Pagination offset")
    ),
    responses(
        (status = 200, description = "List of sessions"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Query(query): Query<ListSessionsQueryDto>,
) -> Result<impl IntoResponse, AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let limit = query.limit.unwrap_or(100).min(500);
    let offset = query.offset.unwrap_or(0);
    let include_revoked = query.include_revoked.unwrap_or(false);
    let user_id_filter = match query.user_id.as_deref() {
        Some(s) => Some(Uuid::parse_str(s).map_err(|_| AppError::bad_request("Invalid user_id"))?),
        None => None,
    };

    // Pass the caller's DPoP thumbprint through the SessionCaller
    // wrapper so the DTO can flag which row is the admin's own
    // current session (`is_current = true`). Rendered as a "this
    // is you" badge — prevents accidentally revoking the session
    // the click came from. `None` when the admin is unbound (rare
    // — legacy / migration-window sessions), in which case no row
    // highlights.
    let caller = crate::application::dtos::session_dto::SessionCaller {
        id: auth_user.id,
        dpop_jkt: auth_user.dpop_jkt.as_deref(),
    };

    let sessions = auth
        .auth_application_service
        .admin_list_sessions_with_perms(
            state.authorization.as_ref(),
            caller,
            user_id_filter,
            include_revoked,
            limit,
            offset,
        )
        .await
        .map_err(AppError::from)?;

    // Also publish the current access-token TTL so the admin panel can
    // render an honest "revoke takes effect within N seconds" warning
    // above the table. Revoking a session flips the DB row (breaks the
    // refresh path), but any in-flight JWT stays valid until its `exp`
    // — which is `access_token_expiry_secs` from now. Showing this
    // number keeps the UX honest instead of implying instant kill.
    let access_token_expiry_secs = state.core.config.auth.access_token_expiry_secs;
    Ok(Json(serde_json::json!({
        "sessions": sessions,
        "limit": limit,
        "offset": offset,
        "access_token_expiry_secs": access_token_expiry_secs,
    })))
}

/// DELETE /api/admin/sessions/:id — revoke a session
#[utoipa::path(
    delete,
    path = "/api/admin/sessions/{id}",
    params(("id" = String, Path, description = "Session UUID")),
    responses(
        (status = 200, description = "Session revoked"),
        (status = 400, description = "Invalid UUID"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Session not found")
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn revoke_session(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let session_id = Uuid::parse_str(&id).map_err(|_| AppError::bad_request("Invalid UUID"))?;
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let caller = crate::application::dtos::session_dto::SessionCaller {
        id: auth_user.id,
        dpop_jkt: auth_user.dpop_jkt.as_deref(),
    };
    auth.auth_application_service
        .admin_revoke_session_with_perms(state.authorization.as_ref(), caller, session_id)
        .await
        .map_err(AppError::from)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "message": "Session revoked" })),
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
pub async fn get_smtp_info(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
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
pub async fn send_smtp_test(
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

// ─────────────────────────────────────────────────────
// JobRegistry admin surface (`/api/admin/jobs/*`)
// ─────────────────────────────────────────────────────

/// `GET /api/admin/jobs` — enumerate every registered job with its
/// interval, next-run/last-run timestamps, and last outcome.
///
/// Production endpoint, always on. Read-only, so no audit line —
/// the standard admin-middleware auth check is enough.
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
    let mut summary = state.core.job_registry.snapshot().await;

    // Enrich with paused-run info for recoverable jobs so the admin
    // panel can render "Resume (scanned/total)" on the row instead of
    // just "Run". One indexed SELECT hits `jobs.recoverable_runs`
    // (`one_active_run_per_job` partial UNIQUE keys the lookup);
    // failures fall back to the pre-enrichment shape so the endpoint
    // stays useful when the jobs DB is temporarily unreachable.
    if let Some(pool) = state.db_pool.as_ref() {
        let paused_rows: Vec<(String, uuid::Uuid, Option<i64>, Option<i64>)> = sqlx::query_as(
            r#"
            SELECT
                job_name,
                id,
                (stats  ->> 'scanned_count')::BIGINT AS scanned,
                (params ->> 'total_rows')::BIGINT   AS total
            FROM jobs.recoverable_runs
            WHERE status = 'Paused'
            "#,
        )
        .fetch_all(pool.as_ref())
        .await
        .unwrap_or_default();

        let by_name: std::collections::HashMap<String, PausedRunBrief> = paused_rows
            .into_iter()
            .map(|(name, id, scanned, total)| {
                (
                    name,
                    PausedRunBrief {
                        id,
                        scanned: scanned.unwrap_or(0).max(0) as u64,
                        total: total.filter(|t| *t > 0).map(|t| t as u64),
                    },
                )
            })
            .collect();

        for job in summary.iter_mut() {
            if job.recoverable
                && !job.running
                && let Some(paused) = by_name.get(&job.name)
            {
                job.paused_run = Some(paused.clone());
            }
        }
    }

    (StatusCode::OK, Json(summary)).into_response()
}

/// Query parameters for `POST /api/admin/jobs/{name}/trigger`.
///
/// `force=true` requests acceleration semantics from handlers that
/// support it (dedup_gc → grace = 0, grant_cleanup → grace = 0).
/// Silently ignored by handlers that don't (trash_cleanup,
/// usage_reconcile).
///
/// `deep=true` opts into slow variants — `consistency_batch` fans it
/// out to sub-jobs; `storage_consistency` (when implemented) will
/// re-BLAKE3 each blob for bitrot detection. See `JobRunArgs.deep`.
#[derive(serde::Deserialize)]
pub struct TriggerJobQuery {
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub deep: bool,
    /// Optional named storage entry to scope the run against — used by
    /// tenants that respect `JobRunArgs.storage` (currently
    /// `backend_migration` for its target; `blobs_consistency` /
    /// `backend_consistency` will pick this up in slice 7 to probe a
    /// non-active entry). Ignored by tenants that don't declare a
    /// semantic for it. Unknown-name validation is per-tenant — the
    /// generic trigger endpoint doesn't cross-check against
    /// `AppConfig.storage_entries`.
    #[serde(default)]
    pub storage: Option<String>,
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
        deep = query.deep,
        "👮🏻‍♂️ Admin triggered job {} (force={}, deep={})",
        name,
        query.force,
        query.deep,
    );
    let args = JobRunArgs {
        force: query.force,
        deep: query.deep,
        storage: query.storage.clone(),
    };

    // Jobs that can run for hours (backend_migration, future
    // reextract_*) are detached: `tokio::spawn` the trigger so the
    // HTTP request returns immediately. Without this, browser HTTP
    // timeouts drop the request future mid-await → the SemaphorePermit
    // gets released while the spawned handler task keeps running →
    // `current_run_start` goes stale → a second click enters the
    // "already_running" short-circuit and CLEARS the in-memory state
    // even though the original task is still copying blobs → the
    // Cancel button hides because `job.running = false`. Detaching
    // keeps the permit + `current_run_start` scoped to the actual
    // handler-task lifetime.
    //
    // Fast-completing jobs (consistency checks, batch coordinator)
    // stay inline so the operator sees the outcome envelope.
    if is_detached_job(&name) {
        let name_clone = name.clone();
        let registry = state.core.job_registry.clone();
        tokio::spawn(async move {
            registry.trigger(&name_clone, &args).await;
        });
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "ok": true,
                "dispatched": true,
                "detached": true,
                "name": name,
                "message": "Dispatched — poll /runs for status",
            })),
        )
            .into_response();
    }

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

/// Jobs that MUST be dispatched with `tokio::spawn` because they run
/// long enough to outlast an HTTP request timeout. Kept as a small
/// hardcoded allowlist (rather than a flag on `JobEntry`) until
/// there's a second long-running tenant that justifies the plumbing.
/// See the comment in `trigger_job` for why detach matters.
fn is_detached_job(name: &str) -> bool {
    matches!(name, "backend_migration")
}

/// `POST /api/admin/jobs/{name}/cancel` — cooperative cancel of the
/// currently-running recoverable run for `{name}`.
///
/// Flips the row's `status` from `Running` → `CancelRequested`. The
/// handler is responsible for polling `store.status()` between batches
/// and returning `RunOutcome::Paused` at the next safe boundary; if it
/// doesn't, the cancel is a no-op until the run completes naturally.
///
/// Returns 200 with the run_id when a Running row was flipped, 200 with
/// `cancelled: false` when nothing was running (either no runs exist,
/// or the latest is Paused / Completed / Failed / already CancelRequested).
/// Never 404 on "no active run" — the job name is registered and the
/// endpoint just reports the truth.
#[utoipa::path(
    post,
    path = "/api/admin/jobs/{name}/cancel",
    params(("name" = String, Path, description = "Registered job name")),
    responses(
        (status = 200, description = "Cancel signalled (or no-op if nothing was running)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 500, description = "DB error"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn cancel_job(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    use crate::infrastructure::scheduler::JobStoreProvider as _;
    tracing::info!(
        target: "audit",
        event = "job.cancel_requested",
        job = %name,
        "👮🏻‍♂️ Admin requested TERMINAL cancel for job {}",
        name,
    );
    // Terminal semantics: stamps `params.cancel_intent = "terminate"`
    // when a Running / CancelRequested row is present so the engine
    // upgrades the handler's yield to `Cancelled` instead of `Paused`.
    // When the current row is `Paused` (no handler running), does a
    // direct DB flip Paused → Cancelled. See
    // `PgJobStoreProvider::request_terminal_cancel`.
    match state
        .core
        .job_store_provider
        .request_terminal_cancel(&name)
        .await
    {
        Ok(Some(run_id)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "cancelled": true,
                "run_id": run_id.to_string(),
                "note": "Running row → will land in Cancelled at next batch boundary; \
                         Paused row → flipped to Cancelled immediately.",
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "cancelled": false,
                "reason": "no non-terminal run for this job",
            })),
        )
            .into_response(),
        Err(e) => AppError::internal_error(format!("cancel failed: {e}")).into_response(),
    }
}

/// `POST /api/admin/jobs/{name}/pause` — cooperative PAUSE of the
/// currently-running recoverable run for `{name}`.
///
/// Same DB mechanism as the old cancel (Running → CancelRequested,
/// handler yields to Paused), but no `cancel_intent` stamp so the
/// engine writes `Paused`. Use this to interrupt a long-running
/// job and resume it later; use `/cancel` to abandon it terminally.
///
/// Idempotent: if the row is already Paused, returns 200 with
/// `paused: false, reason: "already_paused"`.
#[utoipa::path(
    post,
    path = "/api/admin/jobs/{name}/pause",
    params(("name" = String, Path, description = "Registered job name")),
    responses(
        (status = 200, description = "Pause signalled (or no-op if nothing was running)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 500, description = "DB error"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn pause_job(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    use crate::infrastructure::scheduler::JobStoreProvider as _;
    tracing::info!(
        target: "audit",
        event = "job.pause_requested",
        job = %name,
        "👮🏻‍♂️ Admin requested pause for job {}",
        name,
    );
    match state.core.job_store_provider.request_cancel(&name).await {
        Ok(Some(run_id)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "paused": true,
                "run_id": run_id.to_string(),
                "note": "Handler will yield at the next batch boundary; row will land in Paused.",
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "paused": false,
                "reason": "no running run for this job",
            })),
        )
            .into_response(),
        Err(e) => AppError::internal_error(format!("pause failed: {e}")).into_response(),
    }
}

/// Query parameters for `GET /api/admin/jobs/{name}/runs`.
#[derive(serde::Deserialize)]
pub struct ListRunsQuery {
    /// Cap on returned rows. Server-side clamps to 100 defensively.
    #[serde(default = "default_runs_limit")]
    pub limit: u32,
}

fn default_runs_limit() -> u32 {
    20
}

/// `GET /api/admin/jobs/{name}/runs?limit=N` — history of recoverable
/// runs for a registered job, newest first. Includes terminal +
/// non-terminal rows.
///
/// Read-only, no audit line — standard admin-middleware auth is enough.
#[utoipa::path(
    get,
    path = "/api/admin/jobs/{name}/runs",
    params(
        ("name" = String, Path, description = "Registered job name"),
        ("limit" = Option<u32>, Query, description = "Max rows to return (default 20, capped at 100)"),
    ),
    responses(
        (status = 200, description = "Runs listed (may be empty)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 500, description = "DB error"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn list_job_runs(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<ListRunsQuery>,
) -> impl IntoResponse {
    use crate::infrastructure::scheduler::JobStoreProvider as _;
    let limit = query.limit.clamp(1, 100);
    match state.core.job_store_provider.list_runs(&name, limit).await {
        Ok(runs) => (StatusCode::OK, Json(runs)).into_response(),
        Err(e) => AppError::internal_error(format!("list_runs failed: {e}")).into_response(),
    }
}

/// `GET /api/admin/jobs/{name}/runs/{id}` — single-run detail.
///
/// Returns 404 when the id doesn't exist. `{name}` is not validated
/// against the run's `job_name` — the id is globally unique — but
/// keeping the name in the URL path lets operators build stable
/// per-job history links without knowing individual run ids upfront.
#[utoipa::path(
    get,
    path = "/api/admin/jobs/{name}/runs/{id}",
    params(
        ("name" = String, Path, description = "Registered job name"),
        ("id" = String, Path, description = "Run UUID"),
    ),
    responses(
        (status = 200, description = "Run detail"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Run not found"),
        (status = 500, description = "DB error"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn get_job_run(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((_name, id)): axum::extract::Path<(String, uuid::Uuid)>,
) -> impl IntoResponse {
    use crate::infrastructure::scheduler::JobStoreProvider as _;
    match state.core.job_store_provider.get_run_by_id(id).await {
        Ok(Some(run)) => (StatusCode::OK, Json(run)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "run not found", "id": id.to_string() })),
        )
            .into_response(),
        Err(e) => AppError::internal_error(format!("get_run failed: {e}")).into_response(),
    }
}

/// Query parameters for `GET /api/admin/jobs/{name}/runs/{id}/findings`.
#[derive(serde::Deserialize)]
pub struct ListFindingsQuery {
    /// Page size — server clamps to 500 defensively.
    #[serde(default = "default_findings_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

fn default_findings_limit() -> u32 {
    100
}

/// `GET /api/admin/jobs/{name}/runs/{id}/findings?limit=N&offset=M` —
/// paginated findings emitted by a specific run of a recoverable job.
///
/// 404 when the run id doesn't exist (anti-enum: caller knew the id
/// somehow; we don't leak whether it was pruned vs never-existed).
/// Read-only, no audit — standard admin-middleware auth is enough.
///
/// `{name}` is not validated against the run's `job_name` (the id is
/// globally unique) but keeps the URL path consistent with the other
/// per-run endpoints for stable per-job history links.
#[utoipa::path(
    get,
    path = "/api/admin/jobs/{name}/runs/{id}/findings",
    params(
        ("name" = String, Path, description = "Registered job name"),
        ("id" = String, Path, description = "Run UUID"),
        ("limit" = Option<u32>, Query, description = "Max rows (default 100, capped at 500)"),
        ("offset" = Option<u32>, Query, description = "Rows to skip (default 0)"),
    ),
    responses(
        (status = 200, description = "Findings listed (may be empty)"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Run not found"),
        (status = 500, description = "DB error"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn list_job_run_findings(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((_name, id)): axum::extract::Path<(String, uuid::Uuid)>,
    axum::extract::Query(query): axum::extract::Query<ListFindingsQuery>,
) -> impl IntoResponse {
    use crate::infrastructure::scheduler::JobStoreProvider as _;
    // Existence check first — otherwise a paged listing of a
    // nonexistent run returns 200 [] which is indistinguishable from
    // "run exists, no findings" and breaks operator drill-down.
    match state.core.job_store_provider.get_run_by_id(id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "run not found", "id": id.to_string() })),
            )
                .into_response();
        }
        Err(e) => return AppError::internal_error(format!("get_run failed: {e}")).into_response(),
    }
    let limit = query.limit.clamp(1, 500);
    match state
        .core
        .job_store_provider
        .list_findings(id, limit, query.offset)
        .await
    {
        Ok(findings) => (StatusCode::OK, Json(findings)).into_response(),
        Err(e) => AppError::internal_error(format!("list_findings failed: {e}")).into_response(),
    }
}

/// Query parameters for `POST /api/admin/jobs/runs/purge`.
///
/// `days` — retention window. Terminal runs (`Completed`, `Failed`)
/// with `completed_at` older than this many days ago are deleted
/// (with their findings via CASCADE). Default 30. Minimum enforced
/// at 1 by the provider — zero would eat runs completed seconds
/// ago. Non-terminal runs are ALWAYS preserved regardless of age.
#[derive(serde::Deserialize)]
pub struct PurgeJobRunsQuery {
    #[serde(default = "default_purge_days")]
    pub days: i32,
}

fn default_purge_days() -> i32 {
    30
}

/// `POST /api/admin/jobs/runs/purge?days=N` — operator-triggered
/// cleanup of old terminal runs + their findings. Not periodic;
/// admins fire this when they want to reclaim `jobs.*` history
/// space. Delegates entirely to
/// `JobStoreProvider::purge_terminal_runs` — no SQL in the handler
/// (see `AGENTS.md` § handler thinness).
#[utoipa::path(
    post,
    path = "/api/admin/jobs/runs/purge",
    params(
        ("days" = Option<i32>, Query, description = "Retention window in days (default 30, minimum 1). Terminal runs older than this are deleted with their findings; non-terminal runs are always preserved."),
    ),
    responses(
        (status = 200, description = "Purge complete"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Admin required"),
        (status = 500, description = "DB error"),
    ),
    security(("bearerAuth" = [])),
    tag = "admin"
)]
pub async fn purge_job_runs(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<PurgeJobRunsQuery>,
) -> impl IntoResponse {
    use crate::infrastructure::scheduler::JobStoreProvider as _;
    let retention_days = query.days.max(1);
    match state
        .core
        .job_store_provider
        .purge_terminal_runs(retention_days)
        .await
    {
        Ok(purged) => {
            tracing::info!(
                target: "audit",
                event = "jobs.runs_purged",
                purged = purged,
                retention_days = retention_days,
                "👮🏻‍♂️ admin purged {purged} terminal recoverable-run row(s) past {retention_days} day retention (findings cascaded)",
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "purged":         purged,
                    "retention_days": retention_days,
                })),
            )
                .into_response()
        }
        Err(e) => AppError::internal_error(format!("purge failed: {e}")).into_response(),
    }
}
