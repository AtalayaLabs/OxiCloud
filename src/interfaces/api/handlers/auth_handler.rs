use axum::{
    Router,
    extract::{Json, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post, put},
};
use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::user_dto::{
    ChangePasswordDto, LoginDto, OidcCallbackQueryDto, OidcExchangeDto, OidcProviderInfoDto,
    RefreshTokenDto, RegisterDto, SetupAdminDto,
};
use crate::application::services::auth_application_service::OidcCallbackResult;
use crate::common::di::AppState;
use crate::interfaces::api::cookie_auth;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::CurrentUserId;
use crate::interfaces::middleware::rate_limit::extract_client_ip_from_parts;

/// Public auth routes — no authentication required.
pub fn auth_public_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/status", get(get_system_status))
        // OIDC endpoints (all public)
        .route("/oidc/providers", get(oidc_providers))
        .route("/oidc/authorize", get(oidc_authorize))
        .route("/oidc/callback", get(oidc_callback))
        .route("/oidc/exchange", post(oidc_exchange))
}

/// Protected auth routes — require authentication (auth + CSRF middleware
/// must be applied by the caller in main.rs).
pub fn auth_protected_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/me", get(get_current_user))
        .route("/change-password", put(change_password))
        .route("/logout", post(logout))
}

/// Rate-limited auth routes — split out so main.rs can apply per-endpoint
/// rate limiting middleware independently.
pub fn login_route() -> Router<Arc<AppState>> {
    Router::new().route("/login", post(login))
}

pub fn register_route() -> Router<Arc<AppState>> {
    Router::new().route("/register", post(register))
}

pub fn refresh_route() -> Router<Arc<AppState>> {
    Router::new().route("/refresh", post(refresh_token))
}

/// Public setup route — only active before the first admin is created.
pub fn setup_route() -> Router<Arc<AppState>> {
    Router::new().route("/setup", post(setup_admin))
}

async fn register(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<RegisterDto>,
) -> Result<impl IntoResponse, AppError> {
    // Add detailed logging for debugging
    tracing::info!("Registration attempt for user: {}", dto.username);

    // Verify auth service exists
    let auth_service = match state.auth_service.as_ref() {
        Some(service) => {
            tracing::info!("Auth service found, proceeding with registration");
            service
        }
        None => {
            tracing::error!("Auth service not configured");
            return Err(AppError::internal_error(
                "Authentication service not configured",
            ));
        }
    };

    // Fix #5: Block password registration when OIDC-only mode is active
    if auth_service
        .auth_application_service
        .password_login_disabled()
    {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Password registration is disabled. Please use SSO/OIDC to sign in.",
            "PasswordRegistrationDisabled",
        ));
    }

    // Check if public registration has been disabled by the admin
    if let Some(admin_svc) = state.admin_settings_service.as_ref()
        && !admin_svc.get_registration_enabled().await
    {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Public registration has been disabled by the administrator.",
            "RegistrationDisabled",
        ));
    }

    // Registration logic (admin detection, fresh-install handling, duplicate
    // checks) is all inside the service layer. Call it directly.
    match auth_service
        .auth_application_service
        .register(dto.clone())
        .await
    {
        Ok(user) => {
            tracing::info!("Registration successful for user: {}", dto.username);
            Ok((StatusCode::CREATED, Json(user)))
        }
        Err(err) => {
            tracing::error!("Registration failed for user {}: {}", dto.username, err);
            Err(err.into())
        }
    }
}

async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(dto): Json<LoginDto>,
) -> Result<Response, AppError> {
    // Add detailed logging for debugging
    tracing::info!("Login attempt for user: {}", dto.username);

    // Verify auth service exists
    let auth_service = match state.auth_service.as_ref() {
        Some(service) => {
            tracing::info!("Auth service found, proceeding with login");
            service
        }
        None => {
            tracing::error!("Auth service not configured");
            return Err(AppError::internal_error(
                "Authentication service not configured",
            ));
        }
    };

    // ── Account lockout check ──────────────────────────────────────────
    // Reject immediately if (this account, this IP) has too many consecutive
    // failures. The IP is part of the key so an attacker flooding bad
    // passwords from one address cannot lock a legitimate user out of the
    // same account from a different address (issue #323). The check runs
    // BEFORE Argon2 to save CPU under brute-force attacks.
    let client_ip = extract_client_ip_from_parts(&headers, None);
    if let Err(lockout_secs) = auth_service
        .login_lockout
        .check(&dto.username, &client_ip)
    {
        tracing::warn!(
            username = %dto.username,
            client_ip = %client_ip,
            lockout_secs = lockout_secs,
            "Login rejected — account temporarily locked for this IP"
        );
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "Account temporarily locked due to too many failed attempts. Try again in {} seconds.",
                lockout_secs
            ),
            "AccountLocked",
        ));
    }

    // Check if password login is disabled (OIDC-only mode)
    if auth_service
        .auth_application_service
        .password_login_disabled()
    {
        return Err(AppError::unauthorized(
            "Password login is disabled. Please use SSO/OIDC to sign in.",
        ));
    }

    // Try the normal login process
    match auth_service
        .auth_application_service
        .login(dto.clone())
        .await
    {
        Ok(auth_response) => {
            // ── Successful login — reset lockout counter ──
            auth_service
                .login_lockout
                .record_success(&dto.username, &client_ip);

            tracing::info!("Login successful for user: {}", dto.username);
            // Log the response structure for debugging
            tracing::debug!("Auth response: {:?}", &auth_response);

            // Ensure the response has the expected fields
            if auth_response.access_token.is_empty() || auth_response.refresh_token.is_empty() {
                tracing::error!(
                    "Login response contains empty tokens for user: {}",
                    dto.username
                );
                return Err(AppError::internal_error(
                    "Error generating authentication tokens",
                ));
            }

            // ── Set HttpOnly cookies so the browser never stores tokens in JS ──
            let mut response = (StatusCode::OK, Json(&auth_response)).into_response();
            cookie_auth::append_auth_cookies(
                response.headers_mut(),
                &auth_response.access_token,
                &auth_response.refresh_token,
                auth_response.expires_in,
                state.core.config.auth.refresh_token_expiry_secs,
            );
            cookie_auth::append_csrf_cookie(response.headers_mut(), auth_response.expires_in);

            // Diagnostic: warn when Secure cookies are set but the request
            // arrived over plain HTTP — the browser will reject them (#241).
            if cookie_auth::is_cookie_secure() {
                let is_tls = headers
                    .get("x-forwarded-proto")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|p| p.eq_ignore_ascii_case("https"));
                if !is_tls {
                    tracing::warn!(
                        "Login for '{}': Secure cookies are enabled but the request \
                         does not appear to be over HTTPS (no X-Forwarded-Proto: https). \
                         The browser may reject the cookies. Set OXICLOUD_COOKIE_SECURE=false \
                         in .env if you access OxiCloud via plain HTTP.",
                        dto.username,
                    );
                }
            }

            Ok(response)
        }
        Err(err) => {
            // ── Record failed attempt for lockout tracking ──
            auth_service
                .login_lockout
                .record_failure(&dto.username, &client_ip);
            tracing::error!("Login failed for user {}: {}", dto.username, err);
            Err(err.into())
        }
    }
}

/// Token refresh — accepts the refresh token from **either**:
/// 1. JSON body `{ "refresh_token": "..." }` (API clients, backward compat)
/// 2. HttpOnly cookie `oxicloud_refresh` (browsers)
async fn refresh_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, AppError> {
    tracing::info!("Token refresh requested");

    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    // Try JSON body first (backward compat), then fall back to HttpOnly cookie
    let refresh_tok = serde_json::from_slice::<RefreshTokenDto>(&body)
        .ok()
        .map(|dto| dto.refresh_token)
        .or_else(|| cookie_auth::extract_cookie_value(&headers, cookie_auth::REFRESH_COOKIE))
        .ok_or_else(|| AppError::unauthorized("Refresh token required (JSON body or cookie)"))?;

    let dto = RefreshTokenDto {
        refresh_token: refresh_tok,
    };

    let auth_response = auth_service
        .auth_application_service
        .refresh_token(dto)
        .await?;

    tracing::info!("Token refresh successful, new token issued");

    let mut response = (StatusCode::OK, Json(&auth_response)).into_response();
    cookie_auth::append_auth_cookies(
        response.headers_mut(),
        &auth_response.access_token,
        &auth_response.refresh_token,
        auth_response.expires_in,
        state.core.config.auth.refresh_token_expiry_secs,
    );
    cookie_auth::append_csrf_cookie(response.headers_mut(), auth_response.expires_in);
    Ok(response)
}

async fn get_current_user(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    // First, update the storage usage statistics
    // IMPORTANT: We await the calculation to return updated data
    if let Some(storage_usage_service) = state.storage_usage_service.as_ref() {
        // Calculate storage synchronously (we await the result)
        match storage_usage_service
            .update_user_storage_usage(user_id)
            .await
        {
            Ok(usage) => {
                tracing::info!(
                    "Updated storage usage for user {}: {} bytes",
                    user_id,
                    usage
                );
            }
            Err(e) => {
                // Only log a warning, don't fail the entire request
                tracing::warn!("Failed to update storage usage for user {}: {}", user_id, e);
            }
        }
    }

    // Now get the user data WITH the updated storage
    let user = auth_service
        .auth_application_service
        .get_user_by_id(user_id)
        .await?;

    Ok((StatusCode::OK, Json(user)))
}

async fn change_password(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
    Json(dto): Json<ChangePasswordDto>,
) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    auth_service
        .auth_application_service
        .change_password(user_id, dto)
        .await?;

    Ok(StatusCode::OK)
}

async fn logout(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    // Extract the REFRESH token (not the access token) so the service can
    // look up and revoke the correct session.
    // Strategy: try JSON body first (API clients), then HttpOnly cookie (browsers).
    let refresh_token = serde_json::from_slice::<RefreshTokenDto>(&body)
        .ok()
        .map(|dto| dto.refresh_token)
        .or_else(|| cookie_auth::extract_cookie_value(&headers, cookie_auth::REFRESH_COOKIE))
        .ok_or_else(|| {
            AppError::unauthorized("Refresh token required for logout (JSON body or cookie)")
        })?;

    auth_service
        .auth_application_service
        .logout(user_id, &refresh_token)
        .await?;

    // Clear HttpOnly + CSRF cookies so the browser forgets the session
    let mut response = StatusCode::OK.into_response();
    cookie_auth::append_clear_cookies(response.headers_mut());
    cookie_auth::append_clear_csrf_cookie(response.headers_mut());
    Ok(response)
}

/// POST /api/setup — One-time endpoint to create the first admin user.
///
/// Available only when the system is not yet initialized (no admin exists).
/// Once the admin is created, the system is marked as initialized and this
/// endpoint returns 403 for all subsequent requests.
///
/// Uses an atomic "claim" operation to prevent race conditions: even if two
/// requests arrive simultaneously, only one will succeed in marking the
/// system as initialized and creating the admin.
async fn setup_admin(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<SetupAdminDto>,
) -> Result<impl IntoResponse, AppError> {
    tracing::info!("Setup admin request received for user: {}", dto.username);

    // 1. Verify auth service exists
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    // 2. Verify admin settings service exists
    let admin_svc = state
        .admin_settings_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Admin settings service not configured"))?;

    // 3. Quick pre-check: if the system is already initialized, reject early
    //    (avoids Argon2 work on obviously-late requests)
    if admin_svc.is_system_initialized().await {
        tracing::warn!(
            "Setup admin rejected: system already initialized (user: {})",
            dto.username
        );
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "System is already initialized. Use the admin panel to manage users.",
            "SystemAlreadyInitialized",
        ));
    }

    // 4. ATOMIC: claim initialization — only one concurrent request can win.
    //    We use Uuid::nil() as a placeholder because the admin user
    //    doesn't exist yet. It will be updated to the real id below.
    let claimed = admin_svc
        .try_claim_initialization(Uuid::nil())
        .await
        .map_err(|e| {
            tracing::error!("Failed to claim system initialization: {}", e);
            AppError::internal_error("Failed to claim system initialization")
        })?;

    if !claimed {
        tracing::warn!(
            "Setup admin rejected: another request already claimed initialization (user: {})",
            dto.username
        );
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "System is already initialized. Use the admin panel to manage users.",
            "SystemAlreadyInitialized",
        ));
    }

    // 5. Create the first admin user (we hold the exclusive claim)
    let user = auth_service
        .auth_application_service
        .setup_create_admin(dto.username.clone(), dto.email, dto.password)
        .await
        .map_err(|e| {
            tracing::error!("Setup admin creation failed: {}", e);
            AppError::from(e)
        })?;

    // 5. Update the initialization record with the real admin user_id
    let real_user_id = Uuid::parse_str(&user.id).unwrap_or_default();
    if let Err(e) = admin_svc.mark_system_initialized(real_user_id).await {
        // Not fatal — the claim already prevents concurrent re-initialization,
        // and the "pending" marker is still "true" so the system stays locked.
        tracing::error!(
            "Created admin but failed to update initialized_by with real user id: {}",
            e
        );
    }

    tracing::info!(
        "System initialized: first admin '{}' created successfully",
        dto.username
    );

    Ok((StatusCode::CREATED, Json(user)))
}

/// Get system status - returns whether admin is configured
/// This is a public endpoint used to determine if setup is needed
#[derive(serde::Serialize)]
struct SystemStatus {
    /// Whether the system has been set up with an admin
    initialized: bool,
    /// Number of admin users in the system
    admin_count: i64,
    /// Whether registration is allowed (only if admin exists)
    registration_allowed: bool,
}

async fn get_system_status(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    // Use the DB flag as the authoritative source for initialization status
    let db_initialized = if let Some(admin_svc) = state.admin_settings_service.as_ref() {
        admin_svc.is_system_initialized().await
    } else {
        false
    };

    // Count admin users for additional info
    let admin_count = auth_service
        .auth_application_service
        .count_admin_users()
        .await
        .unwrap_or(0);

    let status = SystemStatus {
        initialized: db_initialized || admin_count > 0,
        admin_count,
        registration_allowed: db_initialized || admin_count > 0,
    };

    tracing::info!(
        "System status check: initialized={}, admin_count={}",
        status.initialized,
        status.admin_count
    );

    Ok((StatusCode::OK, Json(status)))
}

// ============================================================================
// ============================================================================
// OIDC Handlers
// ============================================================================

/// GET /api/auth/oidc/providers — Returns OIDC provider info for the UI
async fn oidc_providers(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let auth_app = &auth_service.auth_application_service;

    if !auth_app.oidc_enabled() {
        return Ok(Json(OidcProviderInfoDto {
            enabled: false,
            provider_name: String::new(),
            authorize_endpoint: String::new(),
            password_login_enabled: true,
        }));
    }

    let config = auth_app.oidc_config().unwrap();

    Ok(Json(OidcProviderInfoDto {
        enabled: true,
        provider_name: config.provider_name.clone(),
        authorize_endpoint: "/api/auth/oidc/authorize".to_string(),
        password_login_enabled: !config.disable_password_login,
    }))
}

/// GET /api/auth/oidc/authorize — Redirects user to the OIDC provider
async fn oidc_authorize(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let auth_app = &auth_service.auth_application_service;

    if !auth_app.oidc_enabled() {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "OIDC is not enabled",
            "OidcDisabled",
        ));
    }

    // Prepare OIDC authorization flow (generates CSRF state, PKCE pair, nonce)
    let authorize_url = auth_app.prepare_oidc_authorize().await?;

    tracing::info!("OIDC authorize redirect generated");

    Ok(Redirect::temporary(&authorize_url))
}

/// GET /api/auth/oidc/callback?code=...&state=... — Handles OIDC callback
async fn oidc_callback(
    State(state): State<Arc<AppState>>,
    Query(query): Query<OidcCallbackQueryDto>,
) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let auth_app = &auth_service.auth_application_service;

    if !auth_app.oidc_enabled() {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "OIDC is not enabled",
            "OidcDisabled",
        ));
    }

    tracing::info!("OIDC callback received with code");

    // Exchange code, validate state/nonce/PKCE, authenticate user
    let result = auth_app
        .oidc_callback(&query.code, &query.state)
        .await
        .map_err(|e| {
            tracing::error!("OIDC callback failed: {}", e);
            AppError::from(e)
        })?;

    match result {
        OidcCallbackResult::WebLogin { exchange_code } => {
            // Regular web login — redirect to frontend with exchange code
            let config = auth_app.oidc_config().unwrap();
            let frontend_url = config.frontend_url.trim_end_matches('/');
            let redirect_url = format!("{}/?oidc_code={}", frontend_url, exchange_code);
            tracing::info!("OIDC login successful, redirecting with exchange code");
            Ok(Redirect::temporary(&redirect_url))
        }
        OidcCallbackResult::NextcloudLogin {
            nc_flow_token,
            user_id,
            username,
        } => {
            // Nextcloud Login Flow v2 — create app password and complete flow
            let nextcloud = state
                .nextcloud
                .as_ref()
                .ok_or_else(|| AppError::internal_error("Nextcloud services not configured"))?;

            let (_id, app_password) = nextcloud
                .app_passwords
                .create_nc(user_id, "Nextcloud (OIDC)")
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, user = %username, "OIDC+NC: failed to create app password");
                    AppError::from(e)
                })?;

            let base_url = state.core.config.base_url();
            let completed =
                nextcloud
                    .login_flow
                    .complete(&nc_flow_token, &username, &base_url, &app_password);

            if completed {
                tracing::info!(
                    user = %username,
                    "OIDC login completed Nextcloud Login Flow v2 successfully"
                );
                let nc_url = format!(
                    "nc://login/server:{}&user:{}&password:{}",
                    base_url, username, app_password
                );
                Ok(Redirect::temporary(&nc_url))
            } else {
                tracing::error!(
                    user = %username,
                    "OIDC+NC: login flow token expired or not found"
                );
                Ok(Redirect::temporary(
                    "/nextcloud-error.html?type=session-expired",
                ))
            }
        }
    }
}

/// POST /api/auth/oidc/exchange — Exchange one-time code for auth tokens
/// Request body: { "code": "<one_time_code>" }
async fn oidc_exchange(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OidcExchangeDto>,
) -> Result<Response, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let auth_response = auth_service
        .auth_application_service
        .exchange_oidc_token(&body.code)
        .map_err(|e| {
            tracing::warn!("OIDC token exchange failed: {}", e);
            AppError::from(e)
        })?;

    tracing::info!(
        "OIDC token exchange successful for user: {}",
        auth_response.user.username
    );

    // Set HttpOnly cookies for the browser
    let mut response = (StatusCode::OK, Json(&auth_response)).into_response();
    cookie_auth::append_auth_cookies(
        response.headers_mut(),
        &auth_response.access_token,
        &auth_response.refresh_token,
        auth_response.expires_in,
        state.core.config.auth.refresh_token_expiry_secs,
    );
    cookie_auth::append_csrf_cookie(response.headers_mut(), auth_response.expires_in);
    Ok(response)
}
