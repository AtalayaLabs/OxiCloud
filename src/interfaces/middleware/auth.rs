use axum::{
    extract::{FromRequestParts, Request, State},
    http::{StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use uuid::Uuid;

use crate::common::di::AppState;

// Re-export CurrentCaller from application layer for use in handlers
pub use crate::application::dtos::user_dto::CurrentCaller;
use crate::application::ports::auth_ports::TokenServicePort;
use crate::domain::entities::presented_token::PresentedToken;
use crate::domain::entities::user::CallerKind;
use crate::domain::entities::user::UserRole;
use crate::domain::services::authorization::Subject;
use crate::infrastructure::services::share_ring;
use crate::interfaces::middleware::user::{LiveRole, resolve_live_role};

/// Marker inserted into request extensions when the user was authenticated
/// via the `oxicloud_access` HttpOnly cookie rather than a Bearer/Basic header.
/// The CSRF middleware uses this to decide whether CSRF validation is required.
#[derive(Clone, Copy, Debug)]
pub struct CookieAuthenticated;

// Newtype over Arc<CurrentCaller> for zero-allocation extraction.
// `Deref<Target = CurrentCaller>` lets handlers access `.id`, `.username`,
// `.email`, `.role` transparently — no signature changes needed.
#[derive(Clone, Debug)]
pub struct AuthUser(pub Arc<CurrentCaller>);

impl std::ops::Deref for AuthUser {
    type Target = CurrentCaller;
    #[inline]
    fn deref(&self) -> &CurrentCaller {
        &self.0
    }
}

/// Reusable extractor that gets the user_id of the authenticated user.
/// Automatically extracted from the `CurrentCaller` inserted by the auth middleware.
///
/// Usage in handlers:
/// ```ignore
/// async fn my_handler(CurrentUserId(user_id): CurrentUserId) -> impl IntoResponse { ... }
/// ```
#[derive(Clone, Debug)]
pub struct CurrentUserId(pub Uuid);

/// Record the principal on the request span — `visitor_id` for a public-share
/// visitor, `user_id` otherwise. **Never both.**
///
/// A share visitor's id is the ring's per-visitor uuid: it matches no
/// `auth.users` row and never will. Recording it as `user_id` would hand
/// operators an id that looks lookupable and is not, and would make share
/// traffic indistinguishable from user traffic in every log query.
fn record_principal_on_span(cc: &CurrentCaller) {
    let span = tracing::Span::current();
    if cc.is_anonymous() {
        span.record("visitor_id", tracing::field::display(cc.id));
    } else {
        span.record("user_id", tracing::field::display(cc.id));
    };
}

/// Assert that `cc` meets a minimum role, or deny.
///
/// The single place a role requirement is expressed. Returns a `Result`
/// rather than a bool on purpose: with `?` the outcome cannot be ignored,
/// which an `is_anonymous()` bool invites.
///
/// **This is not the only enforcement point.** Four paths authenticate
/// without ever reaching an extractor — `middleware/admin.rs`'s
/// `require_authenticated`, the three DAV handlers' hand-rolled
/// `extract_user`, `POST /api/auth/refresh` (mounted outside
/// `auth_middleware`), and `GET /api/rt/ws` (self-auths from a raw Bearer).
/// Each must call this too; see `src/AGENTS.md` § AuthZ enforcement points.
pub fn require_role(cc: &CurrentCaller, min: UserRole) -> Result<(), AuthError> {
    if cc.role_enum().at_least(min) {
        return Ok(());
    }
    tracing::info!(
        target: "audit",
        event = "authz.denied",
        reason = "insufficient_role",
        caller_id = %cc.id,
        role = cc.kind.role().as_str(),
        required = min.as_str(),
        "👮🏻‍♂️ principal role is below the minimum this endpoint requires",
    );
    Err(AuthError::AccessDenied(format!(
        "This endpoint requires role `{}`",
        min.as_str()
    )))
}

/// The unlocked share ring, inserted into request extensions by
/// `auth_middleware` when the browser presents a valid `oxi_shares` cookie.
///
/// Separate from `CurrentCaller` on purpose: the ring says what you have
/// *unlocked*, not who you *are*. A logged-in user carries both.
#[derive(Clone, Debug)]
pub struct ShareRing(pub share_ring::Ring);

/// Every credential the caller holds, as authorization subjects — the only
/// extractor that accepts a public-share visitor.
///
/// `AuthUser` means "a real user" and refuses `anonymous`. A route reachable
/// through a share link opts in by taking this instead: one line, visible in
/// the signature, reviewable per route. Widening a route stays a decision
/// rather than becoming a default.
///
/// Returns a **set**, because a browser can hold more than one credential and
/// the request cannot say which it means — an `<img>` tag sends every cookie
/// it has and nothing else. The user's own subject comes first so ordinary
/// browsing is decided on the first check; see
/// [`AuthorizationEngine::require_any`].
///
/// This is the single place a *session* concept (`role`, cookies) becomes an
/// *authorization* concept (`Subject`). Handlers never see the role and never
/// branch on it; services take the subjects and the engine matches grants
/// against them. Keeping the two vocabularies apart here is what lets the
/// engine stay ignorant of HTTP sessions entirely.
pub struct CallerSubjects(pub Vec<Subject>);

impl<S> FromRequestParts<S> for CallerSubjects
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let cc = parts
            .extensions
            .get::<Arc<CurrentCaller>>()
            .ok_or(AuthError::UserNotFound)?;

        let mut subjects = Vec::new();
        // A real user authorises as themselves. An anonymous visitor's `id`
        // is a ring visitor id matching no `auth.users` row, so it must NOT
        // become a `Subject::User` — that would match a visitor id against
        // user grants: a valid UUID belonging to nobody, so the query runs
        // and silently returns nothing.
        if !cc.is_anonymous() {
            subjects.push(Subject::User(cc.id));
        }
        if let Some(ShareRing(ring)) = parts.extensions.get::<ShareRing>() {
            subjects.extend(ring.shares.iter().copied().map(Subject::Token));
        }

        if subjects.is_empty() {
            // An anonymous principal with no ring. Unreachable — the ring is
            // what makes a principal anonymous in the first place — but an
            // empty credential set must never read as "nothing objected".
            tracing::error!(
                target: "audit",
                event = "authz.denied",
                reason = "no_credentials",
                caller_id = %cc.id,
                role = %cc.kind.role().as_str(),
                "👮🏻‍♂️ principal carries no usable credential",
            );
            return Err(AuthError::AccessDenied("No usable credential".to_string()));
        }
        Ok(CallerSubjects(subjects))
    }
}

/// Build an [`AuthUser`] from request extensions, for handlers that receive a
/// raw `Request` and therefore cannot use `FromRequestParts`.
///
/// The three DAV surfaces (`/webdav`, `/caldav`, `/carddav`) each hand-rolled
/// this, which meant the entire DAV surface was invisible to any rule added to
/// the `AuthUser` extractor — three copies, three chances to forget. One
/// implementation instead, so the guard cannot drift between them.
pub fn auth_user_from_extensions(
    ext: &axum::http::Extensions,
) -> Result<AuthUser, crate::interfaces::errors::AppError> {
    use crate::interfaces::errors::AppError;
    let cc = ext
        .get::<Arc<CurrentCaller>>()
        .cloned()
        .ok_or_else(|| AppError::unauthorized("Authentication required"))?;
    require_role(&cc, UserRole::User)
        .map_err(|_| AppError::forbidden("This surface requires a user account"))?;
    Ok(AuthUser(cc))
}

// Implement FromRequestParts for AuthUser — allows using `auth_user: AuthUser` in handlers.
// Cost: 1 atomic increment (~1 ns) instead of 3 String clones (~100 ns + 3 mallocs).
//
// `AuthUser` means "a real user principal". It REJECTS `role = anonymous`,
// which is what keeps the ~200 handlers taking it fail-closed against
// public-share sessions with no edit to any of them. A route that should be
// reachable by a share visitor opts in explicitly with a different extractor
// rather than this one relaxing.
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let cc = parts
            .extensions
            .get::<Arc<CurrentCaller>>()
            .cloned()
            .ok_or(AuthError::UserNotFound)?;
        require_role(&cc, UserRole::User)?;
        Ok(AuthUser(cc))
    }
}

// Implement FromRequestParts for CurrentUserId — lightweight extractor for user_id only
impl<S> FromRequestParts<S> for CurrentUserId
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let cc = parts
            .extensions
            .get::<Arc<CurrentCaller>>()
            .ok_or(AuthError::UserNotFound)?;
        // Same contract as `AuthUser` — this yields a `user_id`, and an
        // anonymous principal has no `auth.users` row for that id to mean.
        // It also closes the message-bus door for free: `POST /api/rt/ticket`
        // takes this extractor, so no ticket is minted and the WS upgrade has
        // no credential to present.
        require_role(cc, UserRole::User)?;
        Ok(CurrentUserId(cc.id))
    }
}

// `OptionalUserId` used to live here — an infallible extractor yielding
// `Option<Uuid>`. It was deleted rather than taught about anonymous roles:
// it had ZERO call sites in `src/` and `tests/`, so "it returns None for
// anonymous" would have been a protection that guarded nothing. An
// unused permissive extractor is a trap for the next person who reaches
// for it; if an optional principal is ever genuinely needed, reintroduce
// it deliberately with a role decision baked in.

// Error for authentication operations
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Token not provided")]
    TokenNotProvided,

    #[error("Invalid token: {0}")]
    InvalidToken(String),

    #[error("Token expired")]
    TokenExpired,

    #[error("User not found")]
    UserNotFound,

    #[error("Account is no longer active")]
    AccountInactive,

    #[error("Access denied: {0}")]
    AccessDenied(String),

    #[error("Authentication service unavailable")]
    AuthServiceUnavailable,

    /// The live-role re-check could not read the user record, so the
    /// caller's role is unknown. Distinct from [`Self::AccountInactive`]
    /// on purpose: that one is a 401 and means "your account is gone",
    /// which during a database blip would log out every session instead
    /// of asking them to retry. See [`LiveRole::Indeterminate`].
    ///
    /// Also distinct from [`Self::AuthServiceUnavailable`], which is a
    /// misconfiguration (no auth service wired at all) and stays a 500 —
    /// retrying it will never help.
    #[error("Unable to verify account state")]
    LiveRoleIndeterminate { retryable: bool },
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        // Route through AppError so every rejection out of the auth
        // middleware emits the uniform ApiErrorBody wire shape
        // (`{status, error, message, error_type}`). Pre-normalization
        // this path emitted a bare `{"error": "..."}` body, drifting
        // from every handler-emitted error and leaving the SPA's
        // `body.error ?? body.message` fallback with a non-populated
        // `.message` field. See docs/plan/normalize-errors.md.
        let app = match self {
            AuthError::TokenNotProvided => {
                crate::interfaces::errors::AppError::unauthorized("Token not provided")
            }
            AuthError::InvalidToken(msg) => crate::interfaces::errors::AppError::unauthorized(msg),
            AuthError::TokenExpired => {
                crate::interfaces::errors::AppError::unauthorized("Token expired")
            }
            AuthError::UserNotFound => {
                crate::interfaces::errors::AppError::unauthorized("User not found")
            }
            AuthError::AccountInactive => {
                crate::interfaces::errors::AppError::unauthorized("Account is no longer active")
            }
            AuthError::AccessDenied(msg) => crate::interfaces::errors::AppError::forbidden(msg),
            AuthError::AuthServiceUnavailable => {
                crate::interfaces::errors::AppError::internal_error(
                    "Authentication service unavailable",
                )
            }
            AuthError::LiveRoleIndeterminate { retryable } => {
                crate::interfaces::middleware::user::indeterminate_app_error(retryable)
            }
        };
        app.into_response()
    }
}

/// Secure authentication middleware.
///
/// Supports three authentication methods (tried in order):
/// 1. **Bearer JWT** — standard token in `Authorization: Bearer <token>`
/// 2. **Basic Auth with App Passwords** — for DAV clients (DAVx⁵, Thunderbird, rclone)
///    that send `Authorization: Basic base64(username:app_password)`
/// 3. **HttpOnly Cookie** — `oxicloud_access` cookie set by the login endpoint;
///    used by browser-based sessions so tokens are never exposed to JS.
///
/// Bearer is tried first; if no Bearer header is found, Basic is attempted,
/// then the cookie fallback.
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    // Where this request is coming FROM, for `auth.sessions.last_seen_ip`.
    //
    // One small allocation per request, which is a deliberate reversal of
    // the borrow-only treatment `client_ip_display` exists for
    // (benches/ROUND13.md §H2): the tracker has to OWN the value across
    // its flush window, so a borrow cannot work. It buys the
    // created-from / used-from mismatch that is the credential-theft
    // signal — and it is one ~16-byte `String` next to a JWT validation
    // and a cache read, not next to nothing.
    //
    // Computed before the credential arms because three of them stamp,
    // and recomputing per arm would be the actual waste. `None` when no
    // address can be resolved, which leaves the column untouched rather
    // than clearing it.
    let observed_ip: Option<String> =
        Some(super::trusted_proxy::client_ip_display(&request, false).to_string())
            .filter(|s| !s.is_empty() && s != "-");

    // Borrow the Authorization header straight from the request instead of
    // taking axum's `HeaderMap` extractor, which clones the whole map (~2
    // allocs) on every authenticated request purely to read it
    // (benches/ROUND14.md §A4). The borrow is dead by the time each arm
    // reaches `request.extensions_mut()` / `next.run(request)` (NLL), so no
    // owned copy is needed.
    // Parse the share ring once, before any credential arm, because a
    // LOGGED-IN user can carry one too: Alice opening a colleague's public
    // link keeps her own identity and gains the share. Arm 4 below also uses
    // it as the anonymous principal when no user credential is present.
    //
    // A malformed, expired or foreign-signed ring is simply absent — the
    // caller cannot act on the distinction, and an unreadable cookie must
    // never break an otherwise valid user request.
    if let Some(ring) = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(share_ring::extract_from_cookie_header)
        .and_then(|jwt| share_ring::verify(&state.core.config.auth.jwt_secret, jwt))
    {
        request.extensions_mut().insert(ShareRing(ring));
    }

    // Borrow the Authorization header straight from the request instead of
    // taking axum's `HeaderMap` extractor, which clones the whole map (~2
    // allocs) on every authenticated request purely to read it
    // (benches/ROUND14.md §A4). The borrow is dead by the time each arm
    // reaches `request.extensions_mut()` / `next.run(request)` (NLL), so no
    // owned copy is needed.
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    // ── 1. Try Bearer JWT ────────────────────────────────────────
    if let Some(header_value) = auth_header {
        if let Some(token_str) = header_value.strip_prefix("Bearer ") {
            let token_str = token_str.trim();
            if !token_str.is_empty() {
                tracing::debug!("Processing Bearer authentication token");

                if let Some(auth_service) = state.auth_service.as_ref() {
                    let token_service = &auth_service.token_service;
                    match token_service.validate_token(token_str) {
                        Ok(claims) => {
                            tracing::debug!(
                                "Token validated successfully for user: {}",
                                claims.username
                            );
                            // Pre-parsed at decode time (benches/ROUND14.md §A3);
                            // nil only for a malformed sub, which we reject as before.
                            let user_id = claims.sub_id;
                            if user_id.is_nil() {
                                return Err(AuthError::InvalidToken(
                                    "Invalid user ID in token".to_string(),
                                ));
                            }
                            // A cryptographically valid token must not outlive the
                            // account: re-check the live record so deactivation,
                            // deletion and demotion take effect within the flags-cache
                            // TTL instead of waiting for token expiry. The returned
                            // role is authoritative — never the frozen JWT claim.
                            // Used here AND carried on the `CurrentCaller` built
                            // below, which is the single channel a handler or a
                            // `Gate` reads it from.
                            let presented = PresentedToken {
                                issued_at: Some(claims.iat),
                                session_id: claims.sid,
                            };
                            let kind = match resolve_live_role(
                                auth_service.auth_application_service.as_ref(),
                                user_id,
                                &claims.role,
                                presented,
                            )
                            .await
                            {
                                LiveRole::Active(kind) => kind,
                                LiveRole::Revoked => return Err(AuthError::AccountInactive),
                                // Unknown, not inactive. `AccountInactive`
                                // is a 401, which would tell a Bearer
                                // client its token is dead and send it to
                                // re-authenticate over a blip.
                                LiveRole::Indeterminate { retryable } => {
                                    return Err(AuthError::LiveRoleIndeterminate { retryable });
                                }
                            };
                            // `username`/`email` are `Arc<str>` refcount
                            // bumps out of the cached claims; `caller` is a
                            // one-byte `Copy` enum — the whole build is 1
                            // alloc (the `Arc::new`) instead of 4.
                            let current_user = Arc::new(CurrentCaller {
                                id: user_id,
                                kind,
                                token: presented,
                                username: Arc::clone(&claims.username),
                                email: Arc::clone(&claims.email),
                                dpop_jkt: claims.dpop_jkt.clone(),
                            });
                            record_principal_on_span(&current_user);
                            request.extensions_mut().insert(current_user);
                            // Bump per-session liveness for the
                            // Prometheus gauges. O(1) DashMap upsert
                            // — no I/O on this hot path. The `sid`
                            // claim is `None` on tokens minted by
                            // pre-`sid` builds, in which case the
                            // stamp is skipped entirely — no
                            // fallback lookup, no round-trip.
                            if let (Some(sid), Some(tracker)) =
                                (claims.sid, state.last_seen_tracker.as_ref())
                            {
                                tracker.stamp(sid, observed_ip.clone());
                            }
                            return Ok(next.run(request).await);
                        }
                        Err(e) => {
                            tracing::warn!("Bearer token validation failed: {}", e);
                            return Err(AuthError::InvalidToken(format!("Invalid token: {}", e)));
                        }
                    }
                }
            }
        }

        // ── 2. Try Basic Auth with App Passwords ─────────────────
        if let Some(basic_encoded) = header_value.strip_prefix("Basic ") {
            let basic_encoded = basic_encoded.trim();
            if !basic_encoded.is_empty() {
                tracing::debug!("Processing Basic authentication (app password)");

                // Decode base64(username:password)
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(basic_encoded)
                    .map_err(|_| {
                        AuthError::InvalidToken("Invalid Basic auth encoding".to_string())
                    })?;
                let credentials = String::from_utf8(decoded).map_err(|_| {
                    AuthError::InvalidToken("Invalid Basic auth encoding".to_string())
                })?;

                let (username, password) = credentials.split_once(':').ok_or_else(|| {
                    AuthError::InvalidToken("Invalid Basic auth format".to_string())
                })?;

                if let Some(app_pw_service) = state.app_password_service.as_ref() {
                    match app_pw_service.verify_basic_auth(username, password).await {
                        Ok((user_id, uname, email)) => {
                            tracing::debug!(
                                "App password authentication successful for user: {}",
                                uname
                            );

                            // Re-check the LIVE user record, exactly as the
                            // Bearer and cookie branches do.
                            //
                            // This branch had no live check at all, and the
                            // verification it trusts is cached for
                            // `BASIC_AUTH_CACHE_TTL_SECS` (300 s) keyed on
                            // the username+password hash — so a deactivated
                            // account kept full WebDAV / CalDAV / CardDAV
                            // access for up to five minutes after an admin
                            // disabled it, and a demoted admin kept the
                            // cached `role` just as long. Caught by
                            // `tests/api/auth_doors_deactivated_account.hurl`,
                            // where a PROPFIND answered 207 after
                            // deactivation.
                            //
                            // The account check inside `verify_basic_auth`
                            // cannot close this: it runs on a cache MISS,
                            // which is precisely what a client that just
                            // authenticated does not produce. This read is
                            // the `UserFlags` moka entry every Bearer
                            // request already pays for, and deactivation
                            // invalidates it eagerly, so the refusal is
                            // immediate rather than TTL-bounded.
                            //
                            // `None` for the issued-at: Basic auth carries
                            // no JWT, so the revocation cutoff cannot apply
                            // here — an app password is revoked by deleting
                            // it, not by `sessions_valid_from`.
                            //
                            // ⚠️  HOT PATH — read before adding anything here.
                            //
                            // This runs on EVERY WebDAV / NextCloud / CalDAV
                            // / CardDAV request, and those clients are far
                            // chattier than the SPA: a sync pass is one
                            // PROPFIND per folder, and desktop clients poll
                            // continuously. Whatever sits in this branch is
                            // multiplied by that traffic.
                            //
                            // What makes the check affordable:
                            //   * `get_user_flags` is a moka hit in the
                            //     common case — no DB round trip at all.
                            //   * On a miss it is ONE single-row lookup by
                            //     primary key of five small columns (no
                            //     `image`), and `try_get_with` coalesces
                            //     concurrent misses for the same user into a
                            //     single query — so twenty parallel
                            //     PROPFINDs on a cold entry cost one.
                            //   * The miss rate is therefore ~one tiny query
                            //     per active user per `USER_FLAGS_CACHE_TTL`
                            //     (30 s), which is the same cost every
                            //     Bearer request has always paid.
                            //
                            // So: do NOT add an uncached read, a second
                            // query, or anything per-request that touches
                            // the database here. If a new account property
                            // must be enforced, put it on `UserFlags` so it
                            // rides the existing cached read — that is
                            // precisely why `sessions_valid_from` was added
                            // as a column there rather than as its own
                            // lookup.
                            // Deny-by-default if there is no auth service to
                            // ask: without it the account state cannot be
                            // verified, and an unverifiable account must not
                            // be admitted. `AuthServiceUnavailable` is a 500
                            // because it is a misconfiguration, not a blip.
                            let Some(auth_svc) = state.auth_service.as_ref() else {
                                return Err(AuthError::AuthServiceUnavailable);
                            };
                            let kind = match crate::interfaces::middleware::user::resolve_live_role(
                                auth_svc.auth_application_service.as_ref(),
                                user_id,
                                // No claim to pass: an app password carries
                                // no role of its own. `claim_role` exists
                                // only for the anonymous short-circuit, and
                                // `from_session("")` rejects this, so the
                                // live read is always taken — which is what
                                // should happen. The previous value came
                                // from the 300 s Basic-auth cache and said
                                // nothing this branch could use.
                                "",
                                // An app password is not a JWT: no `iat`,
                                // no `sid`. So neither revocation form
                                // reaches it — it is revoked by DELETING
                                // it, which the 300 s verification cache
                                // bounds. `active` and the live role,
                                // which is what this call is for, apply
                                // regardless.
                                crate::domain::entities::presented_token::PresentedToken::default(),
                            )
                            .await
                            {
                                crate::interfaces::middleware::user::LiveRole::Active(kind) => kind,
                                crate::interfaces::middleware::user::LiveRole::Revoked => {
                                    tracing::info!(
                                        target: "audit",
                                        event = "auth.login_rejected",
                                        reason = "account_not_live",
                                        origin = "app_password",
                                        caller_id = %user_id,
                                        "👮🏻‍♂️ app-password request refused — account is no longer active",
                                    );
                                    if is_dav_path(request.uri().path()) {
                                        return Ok(dav_basic_auth_challenge(
                                            "Invalid username or app password",
                                        ));
                                    }
                                    return Err(AuthError::AccountInactive);
                                }
                                crate::interfaces::middleware::user::LiveRole::Indeterminate {
                                    retryable,
                                } => {
                                    return Err(AuthError::LiveRoleIndeterminate { retryable });
                                }
                            };

                            // App-password sessions are always unbound —
                            // they belong to NC clients / CLI / mobile
                            // tools without WebCrypto. DPoP middleware
                            // exempts them.
                            let current_user = Arc::new(CurrentCaller {
                                id: user_id,
                                kind,
                                // An app password is not a JWT: no `sid`, no
                                // `iat`. Revoked by deleting it.
                                token: PresentedToken::default(),
                                username: uname,
                                email,
                                dpop_jkt: None,
                                // Basic auth is app-password only.
                            });
                            // Basic auth is app-password only — never a share.
                            record_principal_on_span(&current_user);
                            request.extensions_mut().insert(current_user);
                            return Ok(next.run(request).await);
                        }
                        Err(e) => {
                            tracing::warn!("App password verification failed: {}", e);
                            // For DAV clients: include WWW-Authenticate so the client
                            // re-prompts for credentials rather than failing silently.
                            if is_dav_path(request.uri().path()) {
                                return Ok(dav_basic_auth_challenge(
                                    "Invalid username or app password",
                                ));
                            }
                            return Err(AuthError::InvalidToken(
                                "Invalid username or app password".to_string(),
                            ));
                        }
                    }
                } else {
                    tracing::warn!("Basic auth attempted but app password service not configured");
                    return Err(AuthError::InvalidToken(
                        "App passwords are not enabled".to_string(),
                    ));
                }
            }
        }
    }

    // ── 3. Try HttpOnly cookie (browser sessions) ────────────────
    {
        use crate::interfaces::api::cookie_auth;

        if let Some(token_str) =
            cookie_auth::extract_cookie_str(request.headers(), cookie_auth::ACCESS_COOKIE)
            && !token_str.is_empty()
        {
            tracing::debug!("Processing cookie-based authentication");

            if let Some(auth_service) = state.auth_service.as_ref() {
                let token_service = &auth_service.token_service;
                match token_service.validate_token(token_str) {
                    Ok(claims) => {
                        tracing::debug!("Cookie token validated for user: {}", claims.username);
                        // Pre-parsed at decode time (benches/ROUND14.md §A3).
                        let user_id = claims.sub_id;
                        if user_id.is_nil() {
                            return Err(AuthError::InvalidToken(
                                "Invalid user ID in token".to_string(),
                            ));
                        }
                        // Same live-account re-check as the Bearer path. On
                        // revocation we fall through (rather than erroring) so the
                        // browser receives the standard 401 and redirects to
                        // /login, exactly like an invalid or expired cookie.
                        // Same as the Bearer branch: used here and carried on the
                        // `CurrentCaller` built below.
                        let presented = PresentedToken {
                            issued_at: Some(claims.iat),
                            session_id: claims.sid,
                        };
                        match resolve_live_role(
                            auth_service.auth_application_service.as_ref(),
                            user_id,
                            &claims.role,
                            presented,
                        )
                        .await
                        {
                            LiveRole::Active(kind) => {
                                let current_user = Arc::new(CurrentCaller {
                                    id: user_id,
                                    kind,
                                    token: presented,
                                    username: Arc::clone(&claims.username),
                                    email: Arc::clone(&claims.email),
                                    dpop_jkt: claims.dpop_jkt.clone(),
                                });
                                record_principal_on_span(&current_user);
                                request.extensions_mut().insert(current_user);
                                request.extensions_mut().insert(CookieAuthenticated);
                                // Cookie-auth branch stamps the same
                                // way as the Bearer branch above —
                                // see that site for the O(1) /
                                // no-DB rationale.
                                if let (Some(sid), Some(tracker)) =
                                    (claims.sid, state.last_seen_tracker.as_ref())
                                {
                                    tracker.stamp(sid, observed_ip.clone());
                                }
                                return Ok(next.run(request).await);
                            }
                            LiveRole::Revoked => {
                                // Fall through to the unauthenticated 401 / login redirect.
                            }
                            // Must NOT fall through. Falling through means
                            // 401 and a redirect to /login — so a database
                            // blip would log out every browser session at
                            // once, which is the mass-lockout the old
                            // fail-open arm was there to avoid. Answer
                            // 503 and let the SPA retry with the cookie it
                            // already has.
                            LiveRole::Indeterminate { retryable } => {
                                return Err(AuthError::LiveRoleIndeterminate { retryable });
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Cookie token validation failed: {}", e);
                        // Don't return error — fall through to "no token" so
                        // the browser gets a 401 and can redirect to /login.
                    }
                }
            }
        }
    }

    // No valid credentials found via any method.
    if state.auth_service.is_none() {
        tracing::error!("Auth middleware invoked but auth service is not configured");
        return Err(AuthError::AuthServiceUnavailable);
    }

    // For DAV requests with no credentials at all: return 401 with
    // WWW-Authenticate so that spec-compliant clients (Thunderbird, DAVx5,
    // Apple Calendar/Contacts, Nautilus, Cyberduck, Windows Explorer, macOS
    // Finder) know to prompt for credentials and retry. Unlike `curl -u`, these
    // clients do NOT send Basic credentials preemptively — without the
    // challenge they never authenticate and fail with "discovery failed" / 401.
    // Non-DAV routes return the standard AuthError which renders without this
    // header — keeping browser sessions redirecting to /login as before.
    if is_dav_path(request.uri().path()) {
        return Ok(dav_basic_auth_challenge("Authentication required"));
    }

    // ── 4. Share ring only: a public-share visitor ───────────────
    //
    // Reached when no user credential was presented but the browser carries
    // a valid `oxi_shares` cookie. The ring IS the anonymous session —
    // there is no `auth.sessions` row and no second token — so "holds a
    // ring, is not a user" is exactly what makes a principal anonymous.
    //
    // Note this runs AFTER the DAV challenge above: a ring must never
    // authenticate a WebDAV/CalDAV/CardDAV request. Those surfaces are
    // user-only, and `require_internal_user` would refuse anyway, but the
    // ordering means the question never arises.
    //
    // The principal is built ONLY from a verified ring, which is what makes
    // "anonymous implies a share credential" structural rather than checked.
    if let Some(ring) = request.extensions().get::<ShareRing>().cloned() {
        let current_user = Arc::new(CurrentCaller {
            // Named rather than inferred from a bool pair: a visitor has no
            // `auth.users` row, so "internal" and "external" were both
            // false of them and the old pair had to assert one.
            kind: CallerKind::Visitor,
            // A share ring is not a session: no `sid`, no `iat`.
            token: PresentedToken::default(),
            // The ring's visitor id: stable across this visit, matching no
            // `auth.users` row. Logged as `visitor_id`, never `user_id`.
            id: ring.0.visitor_id,
            // Empty rather than fabricated — a visitor has no account, and
            // inventing a name would be indistinguishable from a real one.
            username: Arc::from(""),
            email: Arc::from(""),
            // Unbound: no DPoP keypair, so the middleware exempts it exactly
            // as it does app passwords.
            dpop_jkt: None,
        });
        record_principal_on_span(&current_user);
        request.extensions_mut().insert(current_user);
        return Ok(next.run(request).await);
    }

    Err(AuthError::TokenNotProvided)
}

/// DAV protocol surfaces (WebDAV, CalDAV, CardDAV) authenticate over HTTP Basic.
/// Spec-compliant clients (Thunderbird, DAVx5, Apple Calendar/Contacts, file
/// managers) only send credentials after receiving a `401` carrying a
/// `WWW-Authenticate: Basic` challenge, so these paths must emit it. Browser and
/// JSON-API routes deliberately do not, so they keep redirecting to `/login`.
fn is_dav_path(path: &str) -> bool {
    path.starts_with("/webdav") || path.starts_with("/caldav") || path.starts_with("/carddav")
}

/// Build the `401 Unauthorized` Basic-auth challenge shared by every DAV
/// surface, so clients re-prompt for credentials instead of failing silently.
fn dav_basic_auth_challenge(message: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, r#"Basic realm="OxiCloud""#)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(axum::body::Body::from(message))
        .unwrap()
}

/// Middleware to verify that the authenticated user has an admin role.
///
/// Must be applied AFTER auth_middleware, as it depends on `CurrentCaller`
/// being present in the request extensions. The role carried by
/// `CurrentCaller` is the *live* role resolved by `auth_middleware` (see
/// [`resolve_live_role`]), not the JWT claim, so a demotion is honoured
/// here within the flags-cache TTL.
///
/// Denial shapes distinguish authn from authz:
///   - `CurrentCaller` present, role below admin → 403 Forbidden.
///   - `CurrentCaller` absent → 401 Unauthorized. Should not happen in
///     practice (auth_middleware guards against it), but the
///     defensive fallback returns the honest shape: "we don't know
///     who you are" is 401, not "we know you and refuse" (403).
pub async fn require_admin(request: Request, next: Next) -> Response {
    // Get the CurrentCaller inserted by auth_middleware
    if let Some(current_user) = request.extensions().get::<Arc<CurrentCaller>>() {
        // `at_least`, not `== "admin"`: the owner outranks admin and must
        // reach every admin route. Comparing the spelling denied them.
        if current_user.role_enum().at_least(UserRole::Admin) {
            tracing::debug!("Admin access granted for user: {}", current_user.username);
            return next.run(request).await;
        }
        tracing::info!(
            target: "audit",
            event = "authz.admin_denied",
            reason = "not_admin",
            caller_id = %current_user.id,
            role = %current_user.kind.role().as_str(),
            "👮🏻‍♂️ admin-only route denied for non-admin caller"
        );
        return AuthError::AccessDenied("Admin role required".to_string()).into_response();
    }

    tracing::info!(
        target: "audit",
        event = "authz.admin_denied",
        reason = "unauthenticated",
        "👮🏻‍♂️ admin-only route reached with no authenticated user"
    );
    AuthError::TokenNotProvided.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Takes a `CallerKind` rather than a role string: these tests exercise
    /// the role gate, and a string would reintroduce the parse this change
    /// removed.
    fn principal(kind: CallerKind) -> CurrentCaller {
        CurrentCaller {
            id: Uuid::new_v4(),
            username: "visitor".into(),
            email: "".into(),
            kind,
            dpop_jkt: None,
            // These tests assert on role resolution, not on revocation.
            token: PresentedToken::default(),
        }
    }

    /// The property the whole public-share design rests on: `AuthUser` —
    /// the extractor ~200 handlers take — refuses an anonymous principal.
    /// If this ever passes for `anonymous`, every one of those handlers is
    /// reachable by a share-link visitor.
    #[test]
    fn anonymous_is_refused_a_user_role() {
        let err = require_role(&principal(CallerKind::Visitor), UserRole::User)
            .expect_err("anonymous must not satisfy a user requirement");
        assert!(matches!(err, AuthError::AccessDenied(_)));

        require_role(
            &principal(CallerKind::Internal(UserRole::User)),
            UserRole::User,
        )
        .expect("a user satisfies user");
        require_role(
            &principal(CallerKind::Internal(UserRole::Admin)),
            UserRole::User,
        )
        .expect("an admin satisfies user");
    }

    #[test]
    fn only_admin_satisfies_admin() {
        require_role(
            &principal(CallerKind::Internal(UserRole::Admin)),
            UserRole::Admin,
        )
        .expect("admin satisfies admin");
        assert!(
            require_role(
                &principal(CallerKind::Internal(UserRole::User)),
                UserRole::Admin
            )
            .is_err()
        );
        assert!(require_role(&principal(CallerKind::Visitor), UserRole::Admin).is_err());
    }

    /// The fail-closed property this used to test is now UNREPRESENTABLE,
    /// which is the better outcome and worth recording rather than losing.
    ///
    /// It iterated garbage role strings (`""`, `"superuser"`, `"Admin"`,
    /// `"root"`, `"user "`) and asserted each degraded to anonymous rather
    /// than to a real user — guarding against the `_ => UserRole::User`
    /// habit found at every other parse site, which is fail-OPEN. With
    /// `CallerKind` there is no string to be garbage: the only constructors are
    /// `from_flags` (whose role comes from `UserRole::from_stored`, which
    /// returns `None` for anything unrecognised) and the typed variants.
    ///
    /// What still CAN regress is the default, so that is what this pins:
    /// `CallerKind::default()` must stay the least privileged value, because it
    /// exists only to satisfy `#[serde(skip)]` and a careless
    /// `CallerKind::default()` must fail closed.
    #[test]
    fn caller_default_is_least_privileged() {
        let cc = principal(CallerKind::default());
        assert_eq!(cc.kind, CallerKind::Visitor);
        assert!(cc.is_anonymous());
        assert!(require_role(&cc, UserRole::User).is_err());
    }

    #[test]
    fn dav_paths_receive_basic_auth_challenge() {
        // Regression for #480: CalDAV/CardDAV clients (Thunderbird, DAVx5) only
        // send credentials after a 401 carrying WWW-Authenticate. All three DAV
        // surfaces must qualify so the challenge is emitted.
        for path in [
            "/webdav/",
            "/webdav/admin/file.txt",
            "/caldav/",
            "/caldav/admin/cal/",
            "/carddav/",
            "/carddav/principals/admin/",
        ] {
            assert!(is_dav_path(path), "{path} should be treated as a DAV path");
        }
    }

    #[test]
    fn non_dav_paths_do_not_receive_basic_auth_challenge() {
        for path in [
            "/",
            "/api/files",
            "/login",
            "/index.html",
            "/.well-known/caldav",
        ] {
            assert!(
                !is_dav_path(path),
                "{path} must not get a Basic-auth challenge (browser/API surface)"
            );
        }
    }

    #[test]
    fn challenge_sets_www_authenticate_header() {
        let resp = dav_basic_auth_challenge("Authentication required");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok()),
            Some(r#"Basic realm="OxiCloud""#),
        );
    }

    /// Builds the subject set the way the extractor does, without a request.
    ///
    /// The extractor reads two extensions and composes them; the composition
    /// is the part worth pinning, so it is exercised directly rather than
    /// through a router that would test axum more than it tests this.
    fn subjects_for(cc: &CurrentCaller, ring: Option<&share_ring::Ring>) -> Vec<Subject> {
        let mut subjects = Vec::new();
        if !cc.is_anonymous() {
            subjects.push(Subject::User(cc.id));
        }
        if let Some(ring) = ring {
            subjects.extend(ring.shares.iter().copied().map(Subject::Token));
        }
        subjects
    }

    /// A visitor's `id` is a ring visitor id — a well-formed UUID that
    /// belongs to no `auth.users` row. Turning it into `Subject::User` would
    /// not error; it would run the grant query and quietly match nothing,
    /// which reads as "denied" for the wrong reason and would hide a real
    /// bug behind a plausible 404.
    #[test]
    fn an_anonymous_visitor_contributes_no_user_subject() {
        let visitor = principal(CallerKind::Visitor);
        let share = Uuid::new_v4();
        let ring = share_ring::Ring {
            visitor_id: visitor.id,
            shares: vec![share],
        };

        assert_eq!(
            subjects_for(&visitor, Some(&ring)),
            vec![Subject::Token(share)],
        );
    }

    /// Alice clicking a colleague's share link keeps her identity AND gains
    /// the share. Neither credential may displace the other: dropping hers
    /// silently downgrades her session, dropping the ring 404s the shared
    /// file she was invited to.
    ///
    /// Her own subject must come FIRST — ordinary browsing is then decided on
    /// the first check and never pays for the ring.
    #[test]
    fn a_logged_in_user_carries_their_identity_and_the_ring() {
        let alice = principal(CallerKind::Internal(UserRole::User));
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let ring = share_ring::Ring {
            visitor_id: Uuid::new_v4(),
            shares: vec![first, second],
        };

        assert_eq!(
            subjects_for(&alice, Some(&ring)),
            vec![
                Subject::User(alice.id),
                Subject::Token(first),
                Subject::Token(second),
            ],
        );
    }

    /// No ring is the overwhelmingly common case and must cost nothing.
    #[test]
    fn a_user_without_a_ring_is_a_single_subject() {
        let alice = principal(CallerKind::Internal(UserRole::User));
        assert_eq!(subjects_for(&alice, None), vec![Subject::User(alice.id)]);
    }

    /// The empty set is what the extractor refuses outright. It is
    /// unreachable — holding a ring is what makes a principal anonymous —
    /// but if it ever arises it must not reach the engine, where an empty
    /// subject list could read as "nothing objected".
    #[test]
    fn an_anonymous_principal_without_a_ring_yields_nothing() {
        assert!(subjects_for(&principal(CallerKind::Visitor), None).is_empty());
    }

    #[test]
    fn account_inactive_maps_to_401() {
        // A token that is still cryptographically valid but whose account was
        // deactivated/deleted must be rejected with 401 (credentials no longer
        // valid), so browsers redirect to /login rather than seeing a 403.
        let resp = AuthError::AccountInactive.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
