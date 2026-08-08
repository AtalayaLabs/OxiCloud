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
    OidcProviderInfoDto, RefreshTokenDto, RegisterDto, SetupAdminDto, UpgradeToInternalDto,
    UserDto,
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
        // OIDC Back-Channel Logout 1.0 — public (server-to-server call
        // from the IdP with a signed logout_token; no cookies, no CSRF).
        .route("/oidc/backchannel-logout", post(oidc_backchannel_logout))
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
        .route("/upgrade-to-internal", post(upgrade_to_internal))
        .route("/logout", post(logout))
        // Self-service OIDC identity linking — see
        // docs/plan/oidc-account-linking.md.
        .route("/oidc/link/start", post(oidc_link_start))
        .route("/oidc/unlink", post(oidc_unlink))
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

    // Block password registration when the policy forbids password
    // logins (OIDC-only mode OR `OXICLOUD_AUTH_METHODS` allowlist
    // without `password`). Email-only signup still works — the user
    // authenticates via magic-link or SSO on their first visit.
    if dto.password.is_some()
        && !auth_service
            .auth_application_service
            .is_password_login_allowed()
    {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Password registration is disabled by policy.",
            "PasswordRegistrationDisabled",
        ));
    }

    // Symmetric guard: when magic-link is off, an email-only signup has
    // no path to a session (there's no token to click). Refuse rather
    // than silently succeed and leave the user with an unusable account.
    if dto.password.is_none()
        && !auth_service
            .auth_application_service
            .is_magic_link_login_allowed()
    {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Email-only registration requires magic-link login, which is disabled.",
            "MagicLinkLoginDisabled",
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

    // Operator-configured allowlist of email domains that can
    // self-register. Empty list = no restriction (any domain accepted).
    // Distinct from `OXICLOUD_EXTERNAL_EMAIL_DOMAINS`, which gates
    // magic-link / grant invitations — an operator can leave that
    // permissive while locking self-registration down, or vice versa.
    //
    // Matching mirrors the magic-link list:
    //   * post-`@` part of the address is extracted and lowercased
    //   * case-insensitive exact match against the allowlist
    //   * no wildcard / subdomain expansion (list every domain
    //     explicitly, per the config docstring)
    //
    // Audit-log denials at the `audit` target so operators can spot
    // enumeration / probe attempts — mirrors the shape used by the
    // magic-link domain rejection at
    // `magic_link_invite_service.rs`.
    let allow_list = &state.core.config.auth.registration_allowed_email_domains;
    if !allow_list.is_empty() {
        let domain = dto
            .email
            .split('@')
            .nth(1)
            .map(|d| d.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if domain.is_empty() || !allow_list.iter().any(|d| d == &domain) {
            tracing::info!(
                target: "audit",
                event = "auth.register_rejected",
                reason = "domain_not_allowed",
                domain = %domain,
                "👮🏻‍♂️ Public registration refused: email domain not in \
                 OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS"
            );
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "Registration is not open to this email domain.",
                "RegistrationDomainNotAllowed",
            ));
        }
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

    // Check if password login is allowed (composes the legacy OIDC-only
    // flag with the newer `OXICLOUD_AUTH_METHODS` allowlist). When
    // disabled, return `PasswordLoginDisabled` so the SPA can hide the
    // password field and surface the available fallback (magic-link or
    // SSO) instead of showing a generic "invalid credentials".
    if !auth_service
        .auth_application_service
        .is_password_login_allowed()
    {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Password login is disabled by policy.",
            "PasswordLoginDisabled",
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
            // Remap the `require_verified_email` refusal (message
            // string comes from AuthApplicationService::login) into a
            // distinguished error_type and, critically, PIGGYBACK a
            // verification link on the successful-password proof: the
            // caller just showed they know the password, so we can
            // safely mint a verification magic-link for their address
            // without going through the anti-enum-fronted
            // `magic-link/send` (which would refuse `has_password`).
            //
            // This branch is reached ONLY when the password validated
            // successfully — the service checks `require_verified_email`
            // AFTER the password check specifically so an attacker
            // without the password can't discover an account's
            // verification state from the response shape.
            // Phase 4: the service refuses legacy login for
            // OPAQUE-migrated users with this exact message. Remap to a
            // stable `error_type` the SPA can branch on — a legit
            // caller reaching this branch would already have taken the
            // OPAQUE path (the SPA's login form calls
            // `/api/auth/opaque/login/lookup` first), so this response
            // primarily serves legacy clients and downgrade-attack
            // detection. 403 keeps the shape consistent with the other
            // policy refusals (PasswordLoginDisabled, EmailNotVerified).
            if err.message == "Password login refused: this account has migrated to OPAQUE" {
                return Err(AppError::new(
                    StatusCode::FORBIDDEN,
                    "Password login is no longer available for this account. Please sign in via the OPAQUE flow.",
                    "OpaqueLoginRequired",
                ));
            }
            if err.message == "Email not verified" {
                // Best-effort auto-send. We swallow any error and still
                // return the same EmailNotVerified response — the
                // frontend hint ("check your inbox") doubles as the
                // resend affordance if delivery didn't land.
                if let Some(invite_svc) = state.magic_link_invite_service.as_ref() {
                    // Re-look up the user by identifier (mirrors the
                    // service's login dispatch) to get the User entity
                    // that the verification helper needs. On any
                    // lookup failure we skip the send — attacker never
                    // sees the difference.
                    let lookup = if dto.username.contains('@') {
                        auth_service
                            .auth_application_service
                            .find_user_by_email(&dto.username)
                            .await
                    } else {
                        auth_service
                            .auth_application_service
                            .find_user_by_username(&dto.username)
                            .await
                    };
                    if let Ok(user) = lookup {
                        let challenge = cookie_auth::generate_magic_request_challenge();
                        let _ = invite_svc
                            .send_verification_link_authenticated(&user, &challenge)
                            .await;
                    }
                }
                return Err(AppError::new(
                    StatusCode::FORBIDDEN,
                    "Your email is not verified. We sent a verification link to your inbox.",
                    "EmailNotVerified",
                ));
            }
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
    // O(N) `SUM(size)` plus an `UPDATE` of `auth.users` on every single call
    // (one of the most frequent endpoints). The cached value is kept current
    // by the per-upload update and a periodic background reconciliation sweep
    // (see `StorageUsageService::start_reconciliation_job`).
    //
    // Semantics (`docs/plan/drive.md` §7): `storage_used_bytes` is the SUM
    // of `used_bytes` across the user's personal drives only. Shared drives
    // never count against this envelope — collaborating in a team drive
    // costs no personal bytes. The matching cap is
    // `storage_quota_bytes` (admin-only mutation).
    let mut user = auth_service
        .auth_application_service
        .get_user_by_id(user_id)
        .await?;

    // Overlay the cached `force_password_change` flag (see UserFlags).
    // `From<User>` defaults to false; the SPA reads this field on
    // startup to decide whether to enter mandatory change-password
    // mode. Using the cached path (`get_user_flags` → `user_flags_cache`)
    // avoids a second DB round-trip on this hot endpoint.
    if let Ok(flags) = auth_service
        .auth_application_service
        .get_user_flags(user_id)
        .await
    {
        user.force_password_change = flags.force_password_change;
    }

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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    CurrentUserId(user_id): CurrentUserId,
    Json(dto): Json<ChangePasswordDto>,
) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    // Resolve the CURRENT session id from the refresh-token cookie so
    // the service layer can revoke every OTHER session (classic
    // password-change security posture) while keeping THIS session
    // alive — the SPA needs to hit `/api/auth/opaque/register/*`
    // immediately after this response to re-mint the OPAQUE envelope
    // under the new password. If we revoked the current session too
    // (the old behaviour), the follow-up register requests would 401
    // silently and the envelope would stay bound to the OLD password.
    //
    // Best-effort: an unauthenticated or cookie-less caller (a CLI
    // hitting this endpoint with just a bearer, no refresh cookie)
    // falls back to `None` → the service revokes ALL sessions, same
    // as the pre-refactor behaviour. That's the safer default when we
    // can't identify "this" session.
    let keep_session_id: Option<Uuid> = {
        let refresh_tok = cookie_auth::extract_cookie_value(&headers, cookie_auth::REFRESH_COOKIE);
        match refresh_tok {
            Some(tok) => auth_service
                .auth_application_service
                .get_session_id_by_refresh_token(&tok)
                .await
                .ok()
                .flatten(),
            None => None,
        }
    };

    match auth_service
        .auth_application_service
        .change_password(user_id, dto, keep_session_id)
        .await
    {
        Ok(()) => {
            // Fire-and-forget security notification: password changed
            // at [now] from [client_ip]. Reaches the user out-of-band
            // so a compromised-account victim can notice and alert
            // their admin. SMTP delivery failures don't affect the
            // 200 response (the change already succeeded); the
            // service's own audit log tracks send outcomes.
            //
            // Runs on a background task so a slow SMTP handshake
            // (30-60 s under a marginal mail server) can't stall the
            // response to the SPA. Cloning the `Arc<MagicLinkInviteService>`
            // is a refcount bump; the User entity is re-fetched inside
            // the task from the same user_id we just verified.
            if let Some(invite_svc) = state.magic_link_invite_service.as_ref() {
                let client_ip = crate::interfaces::middleware::trusted_proxy::client_ip_from_parts(
                    &headers,
                    Some(peer),
                    false,
                )
                .to_string();
                let invite = invite_svc.clone();
                let auth = auth_service.auth_application_service.clone();
                tokio::spawn(async move {
                    // Refetch the user entity fresh — the change we
                    // just made rewrote the row (password_hash), and
                    // the notification method reads `is_active` +
                    // `is_oidc_user` + `email` off the entity to
                    // decide whether to send and where.
                    match auth.get_user_entity(user_id).await {
                        Ok(u) => {
                            let _ = invite
                                .send_password_changed_notification(&u, &client_ip)
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "audit",
                                event = "auth.password_changed_notification_lookup_failed",
                                user_id = %user_id,
                                error = %e.message,
                                "🔔 skipped notification: could not re-fetch user after change_password"
                            );
                        }
                    }
                });
            }
            Ok(StatusCode::OK)
        }
        Err(err) => {
            // Remap the same-as-current guard into a stable error_type
            // the SPA can surface as "pick a different one" without
            // needing to fall back to the generic 400 message. The
            // service returns InvalidInput; keep the 400 status but
            // swap the shape.
            if err.message == "New password must differ from the current password" {
                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    "New password must differ from the current password",
                    "PasswordUnchanged",
                ));
            }
            Err(err.into())
        }
    }
}

/// Convert the authenticated external user into a full internal
/// account. The caller must currently be `is_external = true`; on
/// success, `is_external` is flipped to `false`, a personal drive is
/// provisioned (atomic CTE via `PersonalDriveLifecycleHook`), and the
/// user's flags cache is invalidated so subsequent per-request guards
/// see the new state within cache-round-trip time.
///
/// Password policy:
///   * If the deployment offers magic-link login
///     (`OXICLOUD_AUTH_METHODS` includes `magic_link` AND OIDC is not
///     enabled AND SMTP is wired), the body's `password` field is
///     optional — an upgraded user without a password stays magic-
///     link-only for login.
///   * Otherwise, `password` is required — refused with 400
///     `error_type = "PasswordRequired"`.
///
/// Domain gate: the caller's email domain MUST be in
/// `OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS` (when non-empty).
/// Otherwise invitations would become a bypass of the operator's
/// self-registration policy. Refused with 403
/// `error_type = "RegistrationDomainNotAllowed"`.
///
/// Response: the updated `UserDto` (post-upgrade view — `is_external`
/// is false, `storage_quota_bytes` is set).
#[utoipa::path(
    post,
    path = "/api/auth/upgrade-to-internal",
    request_body = UpgradeToInternalDto,
    responses(
        (status = 200, description = "Upgrade succeeded", body = UserDto),
        (status = 400, description = "Password missing / too short"),
        (status = 401, description = "Not authenticated"),
        (status = 403, description = "OIDC user, or domain not in allowlist"),
        (status = 409, description = "Already internal"),
    ),
    security(("bearerAuth" = [])),
    tag = "auth"
)]
pub async fn upgrade_to_internal(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
    Json(dto): Json<UpgradeToInternalDto>,
) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?;

    // Domain gate. Mirrors the register handler
    // (`OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS`). Rationale: an
    // internal-user invitation must NOT become a way around the
    // operator's self-registration policy. If a domain isn't
    // allowlisted for register, it shouldn't be allowed for upgrade
    // either. External users on non-allowlisted domains remain
    // external — they can still act on shared resources but never own
    // a drive of their own on this deployment.
    let allow_list = &state.core.config.auth.registration_allowed_email_domains;
    if !allow_list.is_empty() {
        // The service re-fetches the user inside `upgrade_to_internal`;
        // one extra id-lookup here just to extract the email is cheap
        // and keeps the domain check at the same layer as the register
        // handler for consistency.
        let email = auth_service
            .auth_application_service
            .get_user_by_id(user_id)
            .await
            .map(|dto| dto.email)?;
        let domain = email
            .split('@')
            .nth(1)
            .map(|d| d.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if domain.is_empty() || !allow_list.iter().any(|d| d == &domain) {
            tracing::info!(
                target: "audit",
                event = "user.upgrade_rejected",
                reason = "domain_not_allowed",
                user_id = %user_id,
                domain = %domain,
                "👮🏻‍♂️ upgrade refused: email domain not in \
                 OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS"
            );
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "This deployment does not accept new accounts from your email domain.",
                "RegistrationDomainNotAllowed",
            ));
        }
    }

    let updated = auth_service
        .auth_application_service
        .upgrade_to_internal(user_id, dto)
        .await
        .map_err(|err| match err.message.as_str() {
            "Account is already internal" => {
                AppError::new(StatusCode::CONFLICT, err.message.clone(), "AlreadyInternal")
            }
            "SSO/OIDC accounts are managed by your identity provider" => {
                AppError::new(StatusCode::FORBIDDEN, err.message.clone(), "ManagedByIdP")
            }
            m if m.starts_with("Password is required") => AppError::new(
                StatusCode::BAD_REQUEST,
                err.message.clone(),
                "PasswordRequired",
            ),
            _ => AppError::from(err),
        })?;

    Ok((StatusCode::OK, Json(updated)))
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

    // Post-logout redirect URI = OxiCloud's `/login`. Must be registered on
    // the OIDC client (Keycloak: "Valid post logout redirect URIs"), else
    // the IdP will refuse the redirect and strand the user on its error page.
    let post_logout_redirect_uri = format!("{}/login", state.core.config.base_url());

    let post_logout_url = auth_service
        .auth_application_service
        .logout(user_id, &refresh_token, &post_logout_redirect_uri)
        .await?;

    // Clear HttpOnly + CSRF cookies so the browser forgets the session
    // regardless of whether we also redirect to the IdP.
    let body = post_logout_url
        .map(|url| serde_json::json!({ "post_logout_url": url }))
        .unwrap_or_else(|| serde_json::json!({}));
    let mut response = (StatusCode::OK, axum::Json(body)).into_response();
    cookie_auth::append_clear_cookies(response.headers_mut());
    cookie_auth::append_clear_csrf_cookie(response.headers_mut());
    Ok(response)
}

/// OIDC Back-Channel Logout 1.0 receiver.
///
/// The IdP POSTs a signed `logout_token` JWT here when a user's SSO
/// session ends (they logged out elsewhere, admin revoked the session,
/// account was disabled). Body is `application/x-www-form-urlencoded`
/// per spec §2.5 with a single `logout_token` field.
///
/// Response codes are constrained by the spec (§2.8):
/// - 200 on successful processing (including a validated token that
///   matched no OxiCloud sessions — the notification is still "handled").
/// - 400 on any validation failure (bad signature, expired, missing
///   required claims, replay). We do NOT return 200 with an error body.
///
/// This endpoint is public: no auth middleware, no CSRF, no cookie.
/// The `logout_token` signature IS the authentication — an unsigned or
/// wrongly-signed token gets rejected by the OIDC service validator.
#[utoipa::path(
    post,
    path = "/api/auth/oidc/backchannel-logout",
    request_body(
        content_type = "application/x-www-form-urlencoded",
        description = "Form-encoded body with a single `logout_token` field (the signed JWT from the IdP)",
    ),
    responses(
        (status = 200, description = "Logout notification accepted (0 or more sessions revoked)"),
        (status = 400, description = "logout_token missing, malformed, or failed validation"),
        (status = 503, description = "OIDC not configured on this deployment"),
    ),
    tag = "auth"
)]
pub async fn oidc_backchannel_logout(
    State(state): State<Arc<AppState>>,
    axum::Form(form): axum::Form<BackchannelLogoutForm>,
) -> Response {
    let auth_service = match state.auth_service.as_ref() {
        Some(s) => s,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "authentication_not_configured",
                    "error_description": "OIDC is not enabled on this deployment"
                })),
            )
                .into_response();
        }
    };

    match auth_service
        .auth_application_service
        .backchannel_logout(&form.logout_token)
        .await
    {
        Ok(revoked) => {
            tracing::info!(
                target: "audit",
                event = "oidc.backchannel_logout_accepted",
                revoked_count = revoked,
                "👮🏻‍♂️ OIDC backchannel-logout accepted"
            );
            // Spec §2.8: response body has no defined content. Empty JSON
            // object keeps content-type coherent and is cheap for the IdP
            // to skip.
            (StatusCode::OK, axum::Json(serde_json::json!({}))).into_response()
        }
        Err(e) => {
            // Log the real reason for operators; return a spec-compliant
            // 400 with a minimal error payload (spec §2.8 recommends
            // `application/json` with `error` + `error_description` per
            // OAuth 2.0 error style).
            tracing::warn!(
                target: "audit",
                event = "oidc.backchannel_logout_rejected",
                reason = %e,
                "👮🏻‍♂️ OIDC backchannel-logout rejected"
            );
            (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": "invalid_request",
                    "error_description": "logout_token validation failed"
                })),
            )
                .into_response()
        }
    }
}

/// Form body carried by OIDC Back-Channel Logout notifications.
#[derive(Debug, serde::Deserialize)]
pub struct BackchannelLogoutForm {
    pub logout_token: String,
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

    // Policy questions the SPA needs to decide which forms to render.
    // `is_magic_link_login_allowed()` composes SMTP wiring + allowlist +
    // the "OIDC master → no magic-link login" hard rule; the login page
    // shows the magic-link tab iff this is true.
    let password_login_enabled = auth_app.is_password_login_allowed();
    let magic_link_login_enabled = auth_app.is_magic_link_login_allowed();
    let require_verified_email = auth_app.require_verified_email();
    let auto_redirect_to_oidc = auth_app.auto_redirect_to_oidc();

    if !auth_app.oidc_enabled() {
        return Ok(Json(OidcProviderInfoDto {
            enabled: false,
            issuer: String::new(),
            provider_name: String::new(),
            authorize_endpoint: String::new(),
            password_login_enabled,
            magic_link_login_enabled,
            require_verified_email,
            auto_redirect_to_oidc: false,
        }));
    }

    let config = auth_app.oidc_config().unwrap();

    // Prefer the DISCOVERY document's issuer — that's what
    // OidcService uses to validate id_tokens AND what lands on
    // `auth.users.federation_issuer` at JIT provisioning / lazy
    // rebind. The config's `issuer_url` is only what the operator
    // typed to point at discovery; the discovery document publishes
    // the authoritative value (may differ by trailing slash, host
    // casing, etc). **Cache-only lookup on purpose** — this
    // endpoint is PUBLIC + UNAUTHENTICATED, so triggering an IdP
    // HTTP fetch per request is a DoS amplifier (attacker at N req/s
    // → we hit the IdP at N req/s, and the cache only stores on
    // success so a degraded IdP means every call retries). On cold
    // cache (before the first real OIDC flow warms it), fall back to
    // the operator-typed `config.issuer_url`. In practice the cache
    // is warm within seconds of the first login; the fallback only
    // shows during that window and is only wrong if the IdP
    // publishes an issuer that differs from the URL used to fetch
    // discovery (rare in normal deployments).
    let issuer = auth_app
        .oidc_service()
        .and_then(|svc| svc.cached_issuer())
        .unwrap_or_else(|| config.issuer_url.clone());

    Ok(Json(OidcProviderInfoDto {
        enabled: true,
        issuer,
        provider_name: config.provider_name.clone(),
        authorize_endpoint: "/api/auth/oidc/authorize".to_string(),
        password_login_enabled,
        magic_link_login_enabled,
        require_verified_email,
        auto_redirect_to_oidc,
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

/// Start a self-service OIDC linking flow for the currently-authenticated
/// user. Returns the authorize URL for the SPA to `window.location`
/// navigate to. Callback lands on the standard `/api/auth/oidc/callback`
/// which dispatches to the link branch based on the state cache's
/// `intent` field. See docs/plan/oidc-account-linking.md.
#[utoipa::path(
    post,
    path = "/api/auth/oidc/link/start",
    responses(
        (status = 200, description = "Authorize URL to navigate the user to", body = serde_json::Value),
        (status = 401, description = "Not authenticated"),
        (status = 404, description = "OIDC not enabled"),
        (status = 409, description = "User is already linked — unlink first"),
    ),
    security(("bearerAuth" = [])),
    tag = "auth"
)]
pub async fn oidc_link_start(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
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

    let authorize_url = auth_app.prepare_oidc_link(user_id).await?;

    Ok(Json(serde_json::json!({
        "authorize_url": authorize_url,
    })))
}

/// Unlink the current user's OIDC identity. Refuses when the user has
/// no other credential (password / OPAQUE) — see plan doc for the
/// no-alternative-auth guard rationale.
#[utoipa::path(
    post,
    path = "/api/auth/oidc/unlink",
    responses(
        (status = 200, description = "OIDC identity unlinked (or was already unlinked)"),
        (status = 401, description = "Not authenticated"),
        (status = 403, description = "Refused — user has no other credential and would be locked out"),
    ),
    security(("bearerAuth" = [])),
    tag = "auth"
)]
pub async fn oidc_unlink(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
) -> Result<impl IntoResponse, AppError> {
    let auth_service = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    // Translate the app-service's generic AccessDenied refusal into a
    // stable machine-readable `error_type` the SPA can switch on to
    // render the "set a password first" affordance. The app service
    // already emits the audit line with reason=no_alternative_auth;
    // this hop maps the domain error to a wire contract.
    match auth_service
        .auth_application_service
        .unlink_oidc(user_id)
        .await
    {
        Ok(()) => Ok(StatusCode::OK),
        Err(e) if e.kind == crate::domain::errors::ErrorKind::AccessDenied => Err(AppError::new(
            StatusCode::FORBIDDEN,
            e.message.clone(),
            "NoAlternativeAuth",
        )),
        Err(e) => Err(e.into()),
    }
}

/// Handle the OIDC provider callback.
///
/// Validates the `state` / PKCE / nonce, exchanges the code for tokens, then
/// redirects the browser to the frontend login route with a short-lived
/// exchange code (`/login?oidc_code=…`), which the SPA swaps for a session.
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

    // Exchange code, validate state/nonce/PKCE, authenticate user.
    // Any Err path (expired state on refresh, consumed code on replay,
    // anti-takeover email refusal, etc.) is caught below and turned
    // into a redirect to /login?login_error=<key> — a JSON 4xx here
    // would render as raw JSON in the browser since the caller is
    // mid-navigation from the IdP, not the SPA. The SPA login page
    // renders localized copy per key.
    let result = match auth_app
        .oidc_callback(&query.code, &query.state, &state.locale_registry)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("OIDC callback failed: {}", e);
            let config = auth_app.oidc_config().unwrap();
            let frontend_url = config.frontend_url.trim_end_matches('/');
            // AccessDenied covers the CSRF/state/code/nonce validation
            // failures (the common "refresh replayed a consumed state"
            // case). Everything else is bucketed as a generic callback
            // failure — operators dig into the log line above for the
            // specifics; end-users only need "try again" guidance.
            let reason = match e.kind {
                crate::domain::errors::ErrorKind::AccessDenied => "callback_denied",
                _ => "callback_failed",
            };
            let redirect_url = format!("{}/login?login_error={}", frontend_url, reason);
            return Ok(Redirect::temporary(&redirect_url).into_response());
        }
    };

    match result {
        OidcCallbackResult::WebLogin { exchange_code } => {
            // Regular web login, redirect to frontend with exchange code
            let config = auth_app.oidc_config().unwrap();
            let frontend_url = config.frontend_url.trim_end_matches('/');
            let redirect_url = format!("{}/login?oidc_code={}", frontend_url, exchange_code);
            tracing::info!("OIDC login successful, redirecting with exchange code");
            Ok(Redirect::temporary(&redirect_url).into_response())
        }
        OidcCallbackResult::NextcloudLogin {
            nc_flow_token,
            user_id,
            username,
        } => {
            // Hand the browser off to the shared LFv2 completion path.
            // That path lists the user's drives, renders the picker
            // when there are ≥ 2, and only completes the flow (via the
            // poll backchannel) when the user has picked. Prior to
            // this refactor the OIDC arm minted the app password
            // inline and completed with the bare username — customers
            // with multiple drives had no way to pick a non-home
            // drive under SSO, and the deprecated `nc://` redirect
            // caused the "Impossible de valider la requête" dialog on
            // NC clients that had already picked up credentials via
            // the poll endpoint. Routing through the shared helper
            // fixes both.
            tracing::info!(
                user = %username,
                "OIDC callback → NC Login Flow v2: handing off to picker/completion path"
            );
            Ok(
                crate::interfaces::nextcloud::login_v2_handler::handle_oidc_login_completion(
                    &state,
                    &nc_flow_token,
                    user_id,
                    &username,
                )
                .await,
            )
        }
        // Self-service link flow completion — redirect the user back
        // to their profile with a query-param signal the SPA reads on
        // mount to render a toast + strip the param via history.
        // See docs/plan/oidc-account-linking.md § UX flow — link.
        OidcCallbackResult::LinkCompleted { user_id } => {
            let config = auth_app.oidc_config().unwrap();
            let frontend_url = config.frontend_url.trim_end_matches('/');
            let redirect_url = format!("{}/profile?linked=1", frontend_url);
            tracing::info!(
                user_id = %user_id,
                "OIDC link completed, redirecting to /profile?linked=1"
            );
            Ok(Redirect::temporary(&redirect_url).into_response())
        }
        OidcCallbackResult::LinkRefused { reason } => {
            let config = auth_app.oidc_config().unwrap();
            let frontend_url = config.frontend_url.trim_end_matches('/');
            let redirect_url = format!("{}/profile?link_error={}", frontend_url, reason);
            tracing::info!(
                reason = reason,
                "OIDC link refused, redirecting to /profile?link_error"
            );
            Ok(Redirect::temporary(&redirect_url).into_response())
        }
        // Redirect the browser back to the login page with a
        // machine-readable reason on the query string, mirroring the
        // LinkRefused → `/profile?link_error=<reason>` pattern above.
        // The browser is mid-redirect from the IdP; returning a 409
        // JSON body would leave the user staring at raw JSON. The SPA
        // login page reads `?login_error=<reason>` on mount, renders a
        // localized notice, and strips the param via history.replaceState.
        //
        // Reasons currently emitted (see auth_application_service.rs
        // auto-link decision tree): auto_link_disabled,
        // auto_link_email_not_verified, already_linked_elsewhere,
        // email_ambiguous.
        OidcCallbackResult::AutoLinkRefused { reason } => {
            let config = auth_app.oidc_config().unwrap();
            let frontend_url = config.frontend_url.trim_end_matches('/');
            let redirect_url = format!("{}/login?login_error={}", frontend_url, reason);
            tracing::info!(
                reason = reason,
                "OIDC auto-link refused, redirecting to /login?login_error"
            );
            Ok(Redirect::temporary(&redirect_url).into_response())
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

    // Policy: `OXICLOUD_AUTH_METHODS` may forbid magic-link login even
    // when SMTP is wired (an operator might want the invite path — used
    // by admins to seed accounts — without offering it as a login
    // fallback). Refuse with the same anti-enum shape as any other
    // policy-gated endpoint.
    if let Some(auth) = state.auth_service.as_ref()
        && !auth.auth_application_service.is_magic_link_login_allowed()
    {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Magic-link login is disabled by policy.",
            "MagicLinkLoginDisabled",
        ));
    }

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

    // Login-identifier resolution. The DTO field is named `email` for
    // backwards-compat, but the value may be either an email address or
    // a username — dispatch matches the `POST /api/auth/login`
    // convention (`@` present → email, else → username). Username
    // lookups happen BEFORE rate-limiting so `alice` and
    // `alice@example.com` bucket on the same key; without this,
    // alternating shapes would double the effective per-email budget.
    //
    // Anti-enum: username misses fall through to `body.email` unchanged
    // and land in the malformed_email / no_account branches downstream,
    // both of which return the uniform 200 with an audit line.
    let resolved_email = if let Some(auth) = state.auth_service.as_ref() {
        auth.auth_application_service
            .resolve_login_identifier_to_email(&body.email)
            .await
            .unwrap_or_else(|| body.email.clone())
    } else {
        body.email.clone()
    };

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
        // Buckets on the RESOLVED email (post-username lookup) so
        // username and email inputs for the same account share one
        // budget — see resolve_login_identifier_to_email() above.
        if let Ok(normalised) =
            crate::domain::services::email_normalize::normalize_email(&resolved_email)
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
    // We pass the resolved email — if the caller sent a username, the
    // service sees the corresponding address; if the caller sent a
    // bare unknown identifier, the service still audits it as
    // malformed_email / no_account.
    invite_svc
        .send_login_link(&resolved_email, &challenge)
        .await
        .map_err(AppError::from)?;

    Ok(uniform_ok())
}
