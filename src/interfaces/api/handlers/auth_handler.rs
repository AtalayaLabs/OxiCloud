use axum::{
    Router,
    extract::{ConnectInfo, Json, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post, put},
};
use std::net::SocketAddr;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::dtos::user_dto::{
    AuthResponseDto, ChangePasswordDto, LoginDto, OidcCallbackQueryDto, OidcExchangeDto,
    OidcProviderInfoDto, RefreshTokenDto, RegisterDto, SetupAdminDto, UserDto,
};
use crate::application::services::auth_application_service::{OidcCallbackResult, RegisterResult};
use crate::common::di::AppState;
use crate::interfaces::api::cookie_auth;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::CurrentUserId;
use crate::interfaces::middleware::trusted_proxy::client_ip_from_parts;
use serde::Deserialize;

/// Public auth routes, no authentication required.
pub fn auth_public_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/status", get(get_system_status))
        // OIDC endpoints (all public)
        .route("/oidc/providers", get(oidc_providers))
        .route("/oidc/authorize", get(oidc_authorize))
        .route("/oidc/callback", get(oidc_callback))
        .route("/oidc/exchange", post(oidc_exchange))
        // Login-via-email — sends a magic-link to the user's email so
        // accounts with no other login credential can sign in.
        .route("/magic-link/send", post(send_magic_link))
}

/// Protected auth routes, require authentication (auth + CSRF middleware
/// must be applied by the caller in main.rs).
pub fn auth_protected_routes() -> Router<Arc<AppState>> {
    use axum::routing::patch;
    Router::new()
        .route("/me", get(get_current_user))
        .route("/me/image", put(update_user_image))
        .route("/me/profile", patch(update_profile))
        .route("/change-password", put(change_password))
        .route("/logout", post(logout))
}

/// Rate-limited auth routes, split out so main.rs can apply per-endpoint
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

/// Public setup route, only active before the first admin is created.
pub fn setup_route() -> Router<Arc<AppState>> {
    Router::new().route("/setup", post(setup_admin))
}

/// Register a new user account.
///
/// **Response shape depends on SMTP availability**:
///
/// - **SMTP configured** (`magic_link_invite_service` is wired): the
///   endpoint returns a **uniform 200** for both success and collision
///   (anti-enumeration). The "Registration request received" message
///   covers both branches honestly because successful email-only
///   signups receive a welcome magic-link. Real outcome recorded in
///   the `audit` channel as `auth.register` with `reason` one of
///   `created`, `email_taken`, `username_taken`.
/// - **SMTP not configured**: there is no welcome-mail cover story, so
///   the classic `201 + UserDto` on success and `409` on collision
///   apply. Anti-enumeration would just be misleading UX (telling the
///   user to check an email that will never arrive). Email-only
///   signup is **503** in this mode because the user would otherwise
///   be stranded with an account they can't log into.
///
/// **Instance-wide policy stays visible** in both modes: when
/// registration is disabled by the admin or password registration is
/// disabled in OIDC-only mode, the endpoint returns **403** with a
/// clear message. These are instance-wide settings, not per-user
/// oracles — legitimate users deserve an actionable error.
#[utoipa::path(
    post,
    path = "/api/auth/register",
    request_body = RegisterDto,
    responses(
        (status = 200, description = "Uniform registration response (SMTP configured, anti-enumeration mode)"),
        (status = 201, description = "User registered successfully (SMTP not configured)", body = UserDto),
        (status = 400, description = "Validation error (malformed request body)"),
        (status = 403, description = "Registration disabled (admin setting or OIDC-only mode)"),
        (status = 409, description = "Username or email already taken (SMTP not configured)"),
        (status = 503, description = "Email-only signup requires SMTP to be configured"),
    ),
    tag = "auth"
)]
pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<RegisterDto>,
) -> Result<axum::response::Response, AppError> {
    // Uniform 200 response used in anti-enumeration mode (SMTP wired).
    let uniform_ok = || {
        let payload = serde_json::json!({
            "message": "Registration request received.",
        });
        (StatusCode::OK, Json(payload)).into_response()
    };

    // Verify auth service exists
    let auth_service = match state.auth_service.as_ref() {
        Some(service) => service,
        None => {
            tracing::error!("Auth service not configured");
            return Err(AppError::internal_error(
                "Authentication service not configured",
            ));
        }
    };

    // Block password registration when OIDC-only mode is active.
    // Email-only signup still works in OIDC-only mode (no password
    // stored; the user authenticates via magic-link).
    if dto.password.is_some()
        && auth_service
            .auth_application_service
            .password_login_disabled()
    {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Password registration is disabled. Please use SSO/OIDC to sign in.",
            "PasswordRegistrationDisabled",
        ));
    }

    // Admin disabled public registration globally — surface 403.
    if let Some(admin_svc) = state.admin_settings_service.as_ref()
        && !admin_svc.get_registration_enabled().await
    {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Public registration has been disabled by the administrator.",
            "RegistrationDisabled",
        ));
    }

    // Email-only signup requires SMTP. Without it the welcome mail
    // can't be dispatched and the user is stranded with no way to log
    // in. 503 is the right response: instance-wide policy, no per-user
    // oracle leaked.
    let smtp_enabled = state.magic_link_invite_service.is_some();
    if dto.password.is_none() && !smtp_enabled {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Email-only registration requires SMTP to be configured on this server.",
            "SmtpRequired",
        ));
    }

    let was_passwordless = dto.password.is_none();
    let email = dto.email.clone();

    let result = match auth_service.auth_application_service.register(dto).await {
        Ok(r) => r,
        Err(err) => {
            tracing::error!("Registration failed: {}", err);
            return Err(err.into());
        }
    };

    match result {
        RegisterResult::Created(user) => {
            // Email-only signup: dispatch the welcome magic-link with
            // a fresh browser-binding challenge (PR 22). Best-effort —
            // SMTP failures don't roll back the user.
            let challenge = cookie_auth::generate_magic_request_challenge();
            let login_ttl_secs = (state.core.config.magic_link.login_ttl_minutes * 60) as i64;
            if was_passwordless
                && let Some(invite) = state.magic_link_invite_service.as_ref()
                && let Err(e) = invite.send_login_link(&email, &challenge).await
            {
                tracing::warn!(
                    target: "audit",
                    event = "auth.register_welcome_mail_failed",
                    user_id = %user.id,
                    email = %email,
                    error = %e,
                    "register: welcome magic-link send failed (user created)",
                );
            }
            if smtp_enabled {
                // Anti-enumeration mode: hide success-vs-collision behind
                // the uniform "check your email" cover story. Attach the
                // browser-binding challenge cookie on every email-only
                // path — preserves the "did a mail go out" anti-enum
                // property at the cookie level too.
                let mut resp = uniform_ok();
                if was_passwordless {
                    cookie_auth::append_magic_request_cookie(
                        resp.headers_mut(),
                        &challenge,
                        login_ttl_secs,
                    );
                }
                Ok(resp)
            } else {
                // Classic mode: clear 201 + UserDto so the frontend can
                // log the user in directly with the password they just
                // submitted. Unbox the DTO for the JSON serialisation.
                Ok((StatusCode::CREATED, Json(*user)).into_response())
            }
        }
        RegisterResult::UsernameTaken => {
            if smtp_enabled {
                Ok(uniform_ok())
            } else {
                Err(AppError::new(
                    StatusCode::CONFLICT,
                    "Username is already taken",
                    "UsernameTaken",
                ))
            }
        }
        RegisterResult::EmailTaken => {
            if smtp_enabled {
                Ok(uniform_ok())
            } else {
                Err(AppError::new(
                    StatusCode::CONFLICT,
                    "Email is already registered",
                    "EmailTaken",
                ))
            }
        }
    }
}

/// Authenticate with username and password.
///
/// On success, sets `oxicloud_access`, `oxicloud_refresh`, and `oxicloud_csrf`
/// HttpOnly cookies in addition to returning the tokens in the JSON body.
#[utoipa::path(
    post,
    path = "/api/auth/login",
    request_body = LoginDto,
    responses(
        (status = 200, description = "Login successful — tokens in body and cookies", body = AuthResponseDto),
        (status = 401, description = "Invalid credentials or password login disabled"),
        (status = 403, description = "Account disabled"),
        (status = 429, description = "Account temporarily locked (too many failed attempts)"),
    ),
    tag = "auth"
)]
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
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
    let client_ip = client_ip_from_parts(&headers, Some(peer), false);
    if let Err(lockout_secs) = auth_service.login_lockout.check(&dto.username, &client_ip) {
        tracing::warn!(
            target: "audit",
            event = "auth.login",
            reason = "account_ip_locked",
            username = %dto.username,
            ip = %client_ip,
            lockout_secs = lockout_secs,
            "Login rejected: account temporarily locked for this IP"
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
            // ── Successful login, reset lockout counter ──
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
            // arrived over plain HTTP, the browser will reject them (#241).
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

/// Refresh an access token.
///
/// Accepts the refresh token from **either**:
/// 1. JSON body `{ "refresh_token": "..." }` (API clients / backward compat)
/// 2. HttpOnly `oxicloud_refresh` cookie (browsers)
///
/// Issues new access + refresh tokens and rotates all three auth cookies.
#[utoipa::path(
    post,
    path = "/api/auth/refresh",
    request_body(content = inline(RefreshTokenDto),
        description = "Optional — omit when using the HttpOnly cookie"),
    responses(
        (status = 200, description = "New tokens issued", body = AuthResponseDto),
        (status = 401, description = "Refresh token missing, expired, or revoked"),
    ),
    tag = "auth"
)]
pub async fn refresh_token(
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

/// Return the authenticated user's profile, including cached storage usage.
#[utoipa::path(
    get,
    path = "/api/auth/me",
    responses(
        (status = 200, description = "Current user profile", body = UserDto),
        (status = 401, description = "Not authenticated"),
    ),
    security(("bearerAuth" = [])),
    tag = "auth"
)]
pub async fn get_current_user(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    // Storage usage is served from the cached `storage_used_bytes` column —
    // it is NOT recomputed here. Recomputing on this hot endpoint meant an
    // O(N) `SUM(size)` over all the user's files plus an `UPDATE` of
    // `auth.users` on every single call (one of the most frequent endpoints).
    // The cached value is kept current by the per-upload update and a periodic
    // background reconciliation sweep
    // (see `StorageUsageService::start_reconciliation_job`).
    let user = auth_service
        .auth_application_service
        .get_user_by_id(user_id)
        .await?;

    Ok((StatusCode::OK, Json(user)))
}

/// DTO for updating the user's profile image.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateUserImageDto {
    /// Image URL (https/http) or data URI (data:image/png|webp|jpeg;base64,…). Null to clear.
    pub image: Option<String>,
}

/// Change the current user's password.
#[utoipa::path(
    put,
    path = "/api/auth/change-password",
    request_body = ChangePasswordDto,
    responses(
        (status = 200, description = "Password changed successfully"),
        (status = 400, description = "New password does not meet requirements"),
        (status = 401, description = "Not authenticated or current password incorrect"),
    ),
    security(("bearerAuth" = [])),
    tag = "auth"
)]
pub async fn change_password(
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

/// Update the caller's profile (PR 24).
///
/// Fields are individually optional — absent = no change. Username is
/// **claim-once, immutable**: passing `username` when the caller
/// already has one is rejected with 409 (the DAV / NextCloud path
/// surface bakes username in as a stable identifier; renaming would
/// break clients). Given / family name are freely settable.
///
/// OIDC-linked users are rejected wholesale with 403 — their profile
/// is owned by the IdP.
#[utoipa::path(
    patch,
    path = "/api/auth/me/profile",
    request_body = crate::application::dtos::user_dto::UpdateProfileDto,
    responses(
        (status = 200, description = "Updated profile (UserDto)", body = UserDto),
        (status = 400, description = "Validation error (e.g. invalid handle format, empty given_name)"),
        (status = 401, description = "Not authenticated"),
        (status = 403, description = "OIDC-managed profile — edit at the IdP"),
        (status = 409, description = "Username already claimed (immutable) or taken by another user"),
    ),
    security(("bearerAuth" = [])),
    tag = "auth"
)]
pub async fn update_profile(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
    Json(dto): Json<crate::application::dtos::user_dto::UpdateProfileDto>,
) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    let updated = auth_service
        .auth_application_service
        .update_profile_with_perms(user_id, dto, &state.locale_registry)
        .await?;

    Ok((StatusCode::OK, Json(updated)))
}

// TODO: add utoipa
pub async fn update_user_image(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
    Json(dto): Json<UpdateUserImageDto>,
) -> impl IntoResponse {
    let auth_service = match state.auth_service.as_ref() {
        Some(svc) => svc,
        None => {
            return AppError::internal_error("Authentication service not configured")
                .into_response();
        }
    };

    match auth_service
        .auth_application_service
        .update_user_image(user_id, dto.image)
        .await
    {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

/// Revoke the current session and clear auth cookies.
///
/// Accepts the refresh token from **either** a JSON body
/// `{ "refresh_token": "..." }` (API clients) or the `oxicloud_refresh`
/// HttpOnly cookie (browsers).
#[utoipa::path(
    post,
    path = "/api/auth/logout",
    request_body(content = inline(RefreshTokenDto),
        description = "Optional — omit when using the HttpOnly cookie"),
    responses(
        (status = 200, description = "Logged out, auth cookies cleared"),
        (status = 401, description = "Not authenticated or refresh token missing"),
    ),
    security(("bearerAuth" = [])),
    tag = "auth"
)]
pub async fn logout(
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

/// One-time endpoint to create the first admin user.
///
/// Available only when the system is not yet initialized (no admin exists).
/// Once the admin is created the endpoint permanently returns 403.
/// Uses an atomic "claim" operation so concurrent requests cannot both succeed.
#[utoipa::path(
    post,
    path = "/api/setup",
    request_body = SetupAdminDto,
    responses(
        (status = 201, description = "First admin created and system initialized", body = UserDto),
        (status = 403, description = "System already initialized"),
        (status = 503, description = "Auth service not configured"),
    ),
    tag = "auth"
)]
pub async fn setup_admin(
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

    // 4. ATOMIC: claim initialization, only one concurrent request can win.
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
        // Not fatal, the claim already prevents concurrent re-initialization,
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

/// System initialisation state, returned by `GET /api/auth/status`.
#[derive(serde::Serialize, ToSchema)]
pub struct SystemStatus {
    /// Whether the system has been set up with an admin.
    initialized: bool,
    /// Number of admin users in the system.
    admin_count: i64,
    /// Whether self-registration is allowed.
    registration_allowed: bool,
}

/// Return the system initialisation state (used by the UI before setup).
#[utoipa::path(
    get,
    path = "/api/auth/status",
    responses(
        (status = 200, description = "System status", body = SystemStatus),
        (status = 503, description = "Auth service not configured"),
    ),
    tag = "auth"
)]
pub async fn get_system_status(
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

/// Return OIDC provider information for the login UI.
///
/// Returns `enabled: false` when OIDC is not configured.
#[utoipa::path(
    get,
    path = "/api/auth/oidc/providers",
    responses(
        (status = 200, description = "OIDC provider info (enabled=false when OIDC not configured)", body = OidcProviderInfoDto),
        (status = 503, description = "Auth service not configured"),
    ),
    tag = "auth"
)]
pub async fn oidc_providers(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
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

/// Initiate OIDC authorization — redirects to the configured identity provider.
///
/// Generates PKCE, CSRF state, and nonce then issues a 302 redirect to the
/// provider's authorization endpoint.
#[utoipa::path(
    get,
    path = "/api/auth/oidc/authorize",
    responses(
        (status = 302, description = "Redirect to OIDC provider authorization URL"),
        (status = 404, description = "OIDC not enabled"),
        (status = 503, description = "Auth service not configured"),
    ),
    tag = "auth"
)]
pub async fn oidc_authorize(
    State(state): State<Arc<AppState>>,
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

    // Prepare OIDC authorization flow (generates CSRF state, PKCE pair, nonce)
    let authorize_url = auth_app.prepare_oidc_authorize().await?;

    tracing::info!("OIDC authorize redirect generated");

    Ok(Redirect::temporary(&authorize_url))
}

/// Handle the OIDC provider callback.
///
/// Validates the `state` / PKCE / nonce, exchanges the code for tokens, then
/// redirects the browser to the frontend with a short-lived exchange code
/// (`/?oidc_code=…`).
#[utoipa::path(
    get,
    path = "/api/auth/oidc/callback",
    params(
        ("code" = String, Query, description = "Authorization code from the OIDC provider"),
        ("state" = String, Query, description = "CSRF state value echoed by the provider"),
    ),
    responses(
        (status = 302, description = "Redirect to frontend with one-time exchange code"),
        (status = 401, description = "OIDC validation failed (bad state, nonce, or code)"),
        (status = 404, description = "OIDC not enabled"),
    ),
    tag = "auth"
)]
pub async fn oidc_callback(
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
        .oidc_callback(&query.code, &query.state, &state.locale_registry)
        .await
        .map_err(|e| {
            tracing::error!("OIDC callback failed: {}", e);
            AppError::from(e)
        })?;

    match result {
        OidcCallbackResult::WebLogin { exchange_code } => {
            // Regular web login, redirect to frontend with exchange code
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
            // Nextcloud Login Flow v2, create app password and complete flow
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

/// Exchange a one-time OIDC code for access + refresh tokens.
///
/// The frontend calls this after being redirected back with `?oidc_code=…`.
/// The exchange code is valid for a single use and expires in 60 s.
#[utoipa::path(
    post,
    path = "/api/auth/oidc/exchange",
    request_body = OidcExchangeDto,
    responses(
        (status = 200, description = "Tokens issued, auth cookies set", body = AuthResponseDto),
        (status = 401, description = "Exchange code invalid or expired"),
    ),
    tag = "auth"
)]
pub async fn oidc_exchange(
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
        auth_response
            .user
            .username
            .as_deref()
            .unwrap_or(&auth_response.user.email)
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

/// Request body for `POST /api/auth/magic-link/send`.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct SendMagicLinkDto {
    pub email: String,
}

/// POST /api/auth/magic-link/send — request a sign-in link by email.
///
/// Always returns 200 with a uniform message regardless of outcome, so
/// the response shape doesn't leak account existence. The real outcome
/// (sent / no-account / has-credential / account-deactivated /
/// malformed-email) is recorded in the `audit` channel via
/// `MagicLinkInviteService::send_login_link`.
///
/// 503 only when the magic-link feature isn't configured at all
/// (SMTP env missing) — operators need to know about misconfiguration;
/// it's not a state an anonymous caller can probe via timing because
/// the absence of the entire feature is visible from any other
/// endpoint touching `/api/auth/magic-link/*`.
///
/// PR 12 rate limits:
/// - **Per-source-IP**, 200/hour — bounds the cost of one attacker
///   spreading low per-email volumes over many target addresses.
/// - **Per-target-email**, 5/hour, keyed on the normalised email —
///   stops the endpoint from being an email-bombing primitive against
///   a single known recipient.
/// Both caps return the uniform 200 (never 429 to anonymous callers,
/// otherwise the status itself becomes an enumeration oracle); the
/// real reason is recorded in the audit channel.
/// Authenticated callers (Authorization header or access cookie
/// present) bypass both limits.
#[utoipa::path(
    post,
    path = "/api/auth/magic-link/send",
    request_body = SendMagicLinkDto,
    responses(
        (status = 200, description = "Uniform 'if an account exists, a link will be sent' response"),
        (status = 503, description = "Magic-link / SMTP is not configured on this server"),
    ),
    tag = "auth",
)]
pub async fn send_magic_link(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
) -> Result<Response, AppError> {
    let Some(invite_svc) = state.magic_link_invite_service.as_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Magic-link sign-in is not configured on this server",
            "ServiceUnavailable",
        ));
    };

    // Authentication signal — presence (not validity) of Bearer header
    // OR access cookie. We deliberately don't decode the JWT here: a
    // stale-cookie holder gets a 401 from any other endpoint they
    // touch, and the worst-case bypass of these anti-flood caps is a
    // narrow window where an attacker keeps a single expired cookie
    // alive. False-negatives (a logged-in user being rate-limited
    // resending to themselves) are the real cost we're avoiding.
    let headers = req.headers().clone();
    let is_authenticated = headers.contains_key(axum::http::header::AUTHORIZATION)
        || crate::interfaces::api::cookie_auth::extract_cookie_value(
            &headers,
            crate::interfaces::api::cookie_auth::ACCESS_COOKIE,
        )
        .is_some();

    let client_ip = crate::interfaces::middleware::rate_limit::extract_client_ip(&req);

    // Body parsing — manual because Request<Body> already consumed
    // any chance of a Json extractor. 4 KiB is generous for
    // `{ "email": "..." }`.
    let body_bytes = axum::body::to_bytes(req.into_body(), 4 * 1024)
        .await
        .map_err(|_| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                "Request body too large or unreadable",
                "InvalidInput",
            )
        })?;
    let body: SendMagicLinkDto = serde_json::from_slice(&body_bytes).map_err(|e| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            format!("Invalid JSON body: {e}"),
            "InvalidInput",
        )
    })?;

    // Per-request browser-binding challenge (PR 22). Generated for
    // every request and set as a cookie on every 200 response —
    // including the silent-rate-limit paths — so the cookie's
    // presence is uniform and can't be used as an enumeration oracle.
    // The corresponding token row only carries the challenge when a
    // token is actually minted; cookie-without-token simply fails to
    // match on the eventual redemption.
    let challenge = cookie_auth::generate_magic_request_challenge();
    let login_ttl_secs = (state.core.config.magic_link.login_ttl_minutes * 60) as i64;
    let challenge_for_closure = challenge.clone();

    let uniform_ok = || {
        let payload = serde_json::json!({
            "message": "If an account exists for that email, a sign-in link will be sent.",
        });
        let mut resp = (StatusCode::OK, Json(payload)).into_response();
        cookie_auth::append_magic_request_cookie(
            resp.headers_mut(),
            &challenge_for_closure,
            login_ttl_secs,
        );
        resp
    };

    if !is_authenticated {
        // Per-IP backstop fires first — covers the case where an
        // attacker iterates many distinct emails to spread the
        // per-email budget thin.
        if state
            .magic_link_send_per_ip_rate_limiter
            .check_and_increment(&client_ip)
            .is_err()
        {
            tracing::warn!(
                target: "audit",
                event = "auth.magic_link_send",
                reason = "rate_limited_ip",
                ip = %client_ip,
                "Per-IP rate limit exceeded on /api/auth/magic-link/send"
            );
            return Ok(uniform_ok());
        }

        // Per-target-email cap, keyed on the normalised form so
        // casing/IDN-host tricks don't multiply the budget. Malformed
        // addresses skip this check and fall through to the service,
        // which records its own audit entry under reason="malformed_email".
        if let Ok(normalised) =
            crate::domain::services::email_normalize::normalize_email(&body.email)
            && state
                .magic_link_send_per_email_rate_limiter
                .check_and_increment(&normalised)
                .is_err()
        {
            tracing::warn!(
                target: "audit",
                event = "auth.magic_link_send",
                reason = "rate_limited_email",
                ip = %client_ip,
                "Per-target-email rate limit exceeded on /api/auth/magic-link/send"
            );
            return Ok(uniform_ok());
        }
    }

    // The service swallows every operational outcome and logs the truth
    // via the audit channel; we surface only an internal error (DB down,
    // etc.). Anti-enumeration means we always return the same body.
    invite_svc
        .send_login_link(&body.email, &challenge)
        .await
        .map_err(AppError::from)?;

    Ok(uniform_ok())
}
