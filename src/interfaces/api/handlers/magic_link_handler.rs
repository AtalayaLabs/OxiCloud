//! Magic-link redemption endpoint.
//!
//! Single public route: `GET /magic/v1/{token}`. Validating the token is
//! the entire authentication — the URL is the credential.
//!
//! Successful redemption:
//!   1. Atomically marks the token used (single-use, race-free).
//!   2. Issues access + refresh JWT for the token's owning user.
//!   3. Sets the standard `oxicloud_access` / `oxicloud_refresh` /
//!      `oxicloud_csrf` cookies (same as `POST /api/auth/login`).
//!   4. 302-redirects to a SPA route based on the token's resource
//!      target:
//!        - Folder       → `/files/{id}`
//!        - File or NULL → `/shared-with-me`
//!
//! Files don't have a deep-link route today; v1 lands file invitations
//! on Shared With Me where the file shows up.
//!
//! Failure cases (all return 4xx without setting cookies):
//!   - Token not found / expired / already used → 410 Gone.
//!   - Magic-link feature disabled (no SMTP / repo) → 503.
//!   - Owning user deactivated → 410 Gone.
//!
//! Page bodies are rendered via askama templates under
//! `templates/magic_link/`; all user-visible strings come from the
//! `server.magic_link.page.*` keys in `static/locales/`.

use std::sync::Arc;

use askama::Template;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName, LOCATION, PRAGMA, REFERRER_POLICY},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::application::services::auth_application_service::{
    MagicLinkRedeemResult, MagicLinkRedemption,
};
use crate::common::di::AppState;
use crate::common::errors::ErrorKind;
use crate::common::locale::Locale;
use crate::domain::entities::magic_link_token::MagicLinkResourceKind;
use crate::interfaces::api::cookie_auth;
use crate::interfaces::middleware::locale::RequestLocale;
use crate::interfaces::middleware::rate_limit::extract_client_ip;

/// Build the `/magic/v1/{token}` router. Mounted at the top of the
/// application tree in `main.rs` — no auth middleware, no CSRF (the
/// token is the credential, the route is GET-only).
///
/// `POST /magic/v1/{token}/resend` is a sibling endpoint that lets the
/// 410-Gone page offer a one-click "send me a fresh link" button. The
/// recipient email is looked up server-side from the (expired/used)
/// token row — no PII in the URL — and the rate limits attached to
/// `POST /api/auth/magic-link/send` apply identically.
///
/// **Cache-Control: no-store** applies to every response from this
/// router. Magic-link responses are auth-state-sensitive in three
/// distinct ways and none of them should ever be persisted by a
/// browser or intermediate proxy:
///
/// 1. The 410 / cross-browser-confirm pages reveal *that the token
///    existed in some state*. Caching them across users of a shared
///    machine is an information leak.
/// 2. The successful-redemption 302 sets `oxicloud_access` /
///    `oxicloud_refresh` / `oxicloud_csrf` cookies. A cached redirect
///    response could replay those cookies in a wrong session context.
/// 3. The resend-confirmation page can be re-submitted; serving a
///    stale copy from cache could mask a fresh state on the server.
///
/// `Pragma: no-cache` is added alongside for HTTP/1.0-era proxies
/// that ignore `Cache-Control` — harmless on modern stacks, defensive
/// against the long tail.
///
/// **Referrer-Policy: no-referrer** overrides the global
/// `strict-origin-when-cross-origin`. The magic-link URL itself
/// contains the secret in the path; even "origin only" disclosure to
/// a third party narrows the bearer's anonymity. Today the templates
/// only carry a same-origin `<a href="/">` link, but defense-in-depth
/// closes the door against any future external reference.
///
/// **X-Robots-Tag: noindex, nofollow** so search engines never index
/// a leaked magic-link URL (e.g. one that ended up in a wiki page or
/// pastebin). Crawlers that respect the directive skip both the
/// indexing and the link-following side effect.
///
/// Global security headers (CSP / X-Frame-Options /
/// X-Content-Type-Options / Permissions-Policy) come from the
/// app-wide layer in `main.rs`; this router doesn't re-set them.
///
/// **Why no CSRF on the resend POST?** The endpoint takes no body
/// and no auth — only the token in the URL path. A cross-site form
/// submission would dispatch a magic-link mail to the *registered
/// recipient* (which the attacker has no influence over), capped by
/// the per-IP and per-target-email rate limits. There's no side
/// effect the attacker can direct anywhere they benefit from, so a
/// CSRF token would protect nothing.
pub fn magic_link_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/magic/v1/{token}", get(redeem_magic_link))
        .route("/magic/v1/{token}/resend", post(resend_magic_link))
        .layer(SetResponseHeaderLayer::overriding(
            CACHE_CONTROL,
            HeaderValue::from_static("no-store, no-cache, must-revalidate, max-age=0"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            PRAGMA,
            HeaderValue::from_static("no-cache"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-robots-tag"),
            HeaderValue::from_static("noindex, nofollow"),
        ))
}

#[derive(Debug, Deserialize)]
struct RedeemQuery {
    /// PR 22: `?confirm=1` means the user clicked the cross-browser
    /// confirmation prompt's Continue button. The service skips the
    /// challenge-cookie check on this re-entry.
    #[serde(default)]
    confirm: Option<String>,
}

#[utoipa::path(
    get,
    path = "/magic/v1/{token}",
    params(("token" = String, Path, description = "Opaque magic-link token")),
    responses(
        (status = 200, description = "Cross-browser confirmation prompt (HTML page)"),
        (status = 302, description = "Redemption succeeded — redirects to the resource or to /shared-with-me"),
        (status = 410, description = "Token is unknown, expired, or already used"),
        (status = 503, description = "Magic-link feature is not configured on this server"),
    ),
    tag = "magic-link",
)]
async fn redeem_magic_link(
    State(state): State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Path(token): Path<String>,
    Query(query): Query<RedeemQuery>,
    RequestLocale(locale): RequestLocale,
    headers: HeaderMap,
) -> Response {
    let Some(auth_svc) = state.auth_service.as_ref() else {
        return service_unavailable_page(&state, &locale).await;
    };

    // PR 22 browser binding: read the per-request challenge from the
    // cookie (set by `POST /api/auth/magic-link/send` on the originating
    // browser). The service compares it to the token's stored
    // challenge. `confirm=1` means the user just clicked through the
    // cross-browser prompt and is fine redeeming from a different
    // browser anyway.
    let incoming_challenge =
        cookie_auth::extract_cookie_value(&headers, cookie_auth::MAGIC_REQUEST_COOKIE);
    let cross_browser_confirmed = query
        .confirm
        .as_deref()
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    // Capture IP + UA for the newly minted session row (admin sessions
    // panel renders these; NULLs would show as "—").
    let client_ip = crate::interfaces::middleware::trusted_proxy::client_ip_from_parts(
        &headers,
        Some(peer),
        false,
    );
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    match auth_svc
        .auth_application_service
        .redeem_magic_link(
            &token,
            incoming_challenge.as_deref(),
            cross_browser_confirmed,
            Some(client_ip),
            user_agent,
        )
        .await
    {
        Ok(MagicLinkRedeemResult::Allowed(redemption)) => {
            build_success_response(&state, *redemption)
        }
        Ok(MagicLinkRedeemResult::NeedsCrossBrowserConfirm) => {
            cross_browser_confirmation_page(&state, &locale, &token).await
        }
        Err(e) => {
            // Log the cause for ops; the user gets a generic page so the
            // outcome can't be used as an enumeration oracle.
            tracing::info!(
                target: "audit",
                event = "magic_link.redemption_failed",
                error_kind = ?e.kind,
                error = %e.message,
            );
            match e.kind {
                ErrorKind::NotImplemented => service_unavailable_page(&state, &locale).await,
                ErrorKind::NotFound | ErrorKind::AccessDenied => {
                    expired_or_used_page(&state, &locale, &token).await
                }
                _ => internal_error_page(&state, &locale).await,
            }
        }
    }
}

/// `POST /magic/v1/{token}/resend` — one-click handler behind the
/// "Send a fresh link" button on the 410-Gone page.
///
/// The token in the URL serves as a *recipient-discovery key*, never as
/// a credential: the server looks up the (expired or used) row, walks
/// to the owning user, and dispatches a fresh login-via-email magic-
/// link to that user's email. The endpoint never trusts client-supplied
/// email and never echoes the resolved address back, so the resend
/// URL is safe to leave in browser history.
///
/// Anti-abuse:
///   - **Per-source-IP** (200/h, shared with `/api/auth/magic-link/send`)
///     bounds burst from a single attacker.
///   - **Per-target-email** (5/h, also shared) caps actual mail volume
///     to the recipient regardless of how many IPs hammer the endpoint.
///   - **Uniform response** on every outcome — rate-limited, no-account,
///     SMTP-failed, succeeded — so the page shape is not an oracle.
///   - **Audit log** carries the truth via the `auth.magic_link_send`
///     events emitted by `MagicLinkInviteService::send_login_link`.
async fn resend_magic_link(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    RequestLocale(locale): RequestLocale,
    req: axum::http::Request<axum::body::Body>,
) -> Response {
    let confirmation_state = state.clone();
    let confirmation_locale = locale.clone();
    let confirmation = move || {
        let s = confirmation_state.clone();
        let l = confirmation_locale.clone();
        async move { resend_confirmation_page(&s, &l).await }
    };

    let Some(invite_svc) = state.magic_link_invite_service.as_ref() else {
        return service_unavailable_page(&state, &locale).await;
    };

    let client_ip = extract_client_ip(&req);

    // Per-IP backstop runs unconditionally — burns through the budget
    // even when the token doesn't resolve, so the endpoint can't be
    // used to spread probes thin across many tokens.
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
            "Per-IP rate limit exceeded on /magic/v1/{{token}}/resend"
        );
        return confirmation().await;
    }

    let hint = match invite_svc.lookup_resend_recipient(&token).await {
        Ok(Some(h)) => h,
        _ => {
            // Unknown / pending / deactivated — uniform response so the
            // outcome is not an oracle for "is this a known token".
            return confirmation().await;
        }
    };

    // Per-target-email cap — keyed on the recipient we just resolved.
    // Locks the mail volume to a single recipient regardless of how
    // many distinct IPs the attacker spreads across.
    if state
        .magic_link_send_per_email_rate_limiter
        .check_and_increment(&hint.email)
        .is_err()
    {
        tracing::warn!(
            target: "audit",
            event = "auth.magic_link_send",
            reason = "rate_limited_email",
            ip = %client_ip,
            "Per-target-email rate limit exceeded on /magic/v1/{{token}}/resend"
        );
        return confirmation().await;
    }

    let challenge = cookie_auth::generate_magic_request_challenge();
    let login_ttl_secs = (state.core.config.magic_link.login_ttl_minutes * 60) as i64;

    // Service swallows operational outcomes and audits the truth; we
    // surface only DB / unexpected errors as 500.
    if let Err(e) = invite_svc.send_login_link(&hint.email, &challenge).await {
        tracing::error!(
            target: "audit",
            event = "auth.magic_link_send",
            reason = "internal_error",
            error = %e.message,
            "Resend dispatch failed for an unexpected reason"
        );
        return resend_failure_page(&state, &locale).await;
    }

    let mut response = confirmation().await;
    cookie_auth::append_magic_request_cookie(response.headers_mut(), &challenge, login_ttl_secs);
    response
}

// ═══════════════════════════════════════════════════════════════════════════
// Template structs
// ═══════════════════════════════════════════════════════════════════════════
//
// Each user-visible page maps to one askama-derived struct. The strings
// they hold are already-resolved translations — the templates themselves
// are pure layout (HTML structure + escaping), no conditional locale
// logic. That keeps the template language minimal and pushes all i18n
// concerns to the call site, where we already have async + an
// I18nApplicationService handle.

#[derive(Template)]
#[template(path = "magic_link/page_expired_or_used.html")]
struct ExpiredOrUsedTemplate {
    locale_code: String,
    title: String,
    body: String,
    return_link: String,
    /// `Some` when the row was recoverable (status = expired or used,
    /// owning user still active) and we want to render the resend
    /// button. `None` for unknown/pending/deactivated tokens — the page
    /// then matches the generic shape, no oracle.
    resend: Option<ResendOffer>,
}

struct ResendOffer {
    action_url: String,
    button_label: String,
}

#[derive(Template)]
#[template(path = "magic_link/page_cross_browser_confirm.html")]
struct CrossBrowserConfirmTemplate {
    locale_code: String,
    title: String,
    body: String,
    warning: String,
    confirm_url: String,
    continue_label: String,
}

#[derive(Template)]
#[template(path = "magic_link/page_resend_confirmation.html")]
struct ResendConfirmationTemplate {
    locale_code: String,
    title: String,
    body: String,
    return_link: String,
}

#[derive(Template)]
#[template(path = "magic_link/page_generic_error.html")]
struct GenericErrorTemplate {
    locale_code: String,
    title: String,
    body: String,
    return_link: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// Page builders
// ═══════════════════════════════════════════════════════════════════════════
//
// Helpers that resolve the user-visible strings via i18n, instantiate
// the template, and wrap the rendered HTML in an axum Response with
// the right status + Content-Type. The status is the caller's choice,
// the locale comes from the `RequestLocale` extractor.

/// Pre-resolve the strings shared across nearly every page (the title
/// fallback and "Return to OxiCloud" footer link). Keeps every builder
/// terse.
async fn translate(state: &Arc<AppState>, locale: &Locale, key: &str) -> String {
    state
        .applications
        .i18n_service
        .translate(key, Some(locale.clone()))
        .await
        .unwrap_or_else(|_| key.to_string())
}

async fn translate_args(
    state: &Arc<AppState>,
    locale: &Locale,
    key: &str,
    args: &[(&str, &str)],
) -> String {
    state
        .applications
        .i18n_service
        .translate_args(key, Some(locale.clone()), args)
        .await
        .unwrap_or_else(|_| key.to_string())
}

async fn expired_or_used_page(state: &Arc<AppState>, locale: &Locale, token: &str) -> Response {
    let hint = match state.magic_link_invite_service.as_ref() {
        Some(svc) => svc.lookup_resend_recipient(token).await.ok().flatten(),
        None => None,
    };

    let (title, body, resend) = if let Some(hint) = hint {
        (
            translate(state, locale, "server.magic_link.page.expired_title").await,
            translate(state, locale, "server.magic_link.page.expired_body").await,
            Some(ResendOffer {
                action_url: format!("/magic/v1/{}/resend", token),
                button_label: translate_args(
                    state,
                    locale,
                    "server.magic_link.page.resend_to",
                    &[("email", &hint.masked_email)],
                )
                .await,
            }),
        )
    } else {
        // Generic "no longer valid" page — the body conveys both
        // outcomes (expired or used) in one sentence to defeat the
        // oracle.
        (
            translate(state, locale, "server.magic_link.page.expired_title").await,
            translate(state, locale, "server.magic_link.page.generic_unavailable").await,
            None,
        )
    };

    let template = ExpiredOrUsedTemplate {
        locale_code: locale.as_str().to_string(),
        title,
        body,
        return_link: translate(state, locale, "server.magic_link.page.return_link").await,
        resend,
    };
    render(StatusCode::GONE, template)
}

async fn cross_browser_confirmation_page(
    state: &Arc<AppState>,
    locale: &Locale,
    token: &str,
) -> Response {
    let template = CrossBrowserConfirmTemplate {
        locale_code: locale.as_str().to_string(),
        title: translate(state, locale, "server.magic_link.page.cross_browser_title").await,
        body: translate(state, locale, "server.magic_link.page.cross_browser_body").await,
        warning: translate(
            state,
            locale,
            "server.magic_link.page.cross_browser_warning",
        )
        .await,
        confirm_url: format!("/magic/v1/{}?confirm=1", token),
        continue_label: translate(
            state,
            locale,
            "server.magic_link.page.cross_browser_continue",
        )
        .await,
    };
    render(StatusCode::OK, template)
}

async fn resend_confirmation_page(state: &Arc<AppState>, locale: &Locale) -> Response {
    let template = ResendConfirmationTemplate {
        locale_code: locale.as_str().to_string(),
        title: translate(
            state,
            locale,
            "server.magic_link.page.resend_confirmation_title",
        )
        .await,
        body: translate(
            state,
            locale,
            "server.magic_link.page.resend_confirmation_body",
        )
        .await,
        return_link: translate(state, locale, "server.magic_link.page.return_link").await,
    };
    render(StatusCode::OK, template)
}

async fn service_unavailable_page(state: &Arc<AppState>, locale: &Locale) -> Response {
    let template = GenericErrorTemplate {
        locale_code: locale.as_str().to_string(),
        title: translate(state, locale, "server.magic_link.page.expired_title").await,
        body: translate(state, locale, "server.magic_link.page.service_unavailable").await,
        return_link: translate(state, locale, "server.magic_link.page.return_link").await,
    };
    render(StatusCode::SERVICE_UNAVAILABLE, template)
}

async fn internal_error_page(state: &Arc<AppState>, locale: &Locale) -> Response {
    let template = GenericErrorTemplate {
        locale_code: locale.as_str().to_string(),
        title: translate(state, locale, "server.magic_link.page.expired_title").await,
        body: translate(state, locale, "server.magic_link.page.internal_error").await,
        return_link: translate(state, locale, "server.magic_link.page.return_link").await,
    };
    render(StatusCode::INTERNAL_SERVER_ERROR, template)
}

async fn resend_failure_page(state: &Arc<AppState>, locale: &Locale) -> Response {
    let template = GenericErrorTemplate {
        locale_code: locale.as_str().to_string(),
        title: translate(state, locale, "server.magic_link.page.expired_title").await,
        body: translate(state, locale, "server.magic_link.page.resend_failure").await,
        return_link: translate(state, locale, "server.magic_link.page.return_link").await,
    };
    render(StatusCode::INTERNAL_SERVER_ERROR, template)
}

/// Render an askama template into a UTF-8 HTML response with the given
/// status. Template render failures only happen when a hand-edited
/// template references a field that doesn't exist on the struct, which
/// would have failed at compile time — but we still log + return a
/// minimal fallback rather than panic in production.
fn render<T: Template>(status: StatusCode, template: T) -> Response {
    match template.render() {
        Ok(body) => {
            let mut response = (status, body).into_response();
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            response
        }
        Err(e) => {
            tracing::error!(
                target: "audit",
                event = "magic_link.template_render_failed",
                error = %e,
                "askama render failed — template definition out of sync with caller"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error rendering page.",
            )
                .into_response()
        }
    }
}

fn build_success_response(state: &Arc<AppState>, redemption: MagicLinkRedemption) -> Response {
    let target = redirect_target(&redemption);

    let mut response = (StatusCode::FOUND, [(LOCATION, target.as_str())]).into_response();

    cookie_auth::append_auth_cookies(
        response.headers_mut(),
        &redemption.auth.access_token,
        &redemption.auth.refresh_token,
        redemption.auth.expires_in,
        state.core.config.auth.refresh_token_expiry_secs,
    );
    cookie_auth::append_csrf_cookie(response.headers_mut(), redemption.auth.expires_in);
    // Seed the SPA's DPoP-nonce cache so the first bound request after
    // the redirect (typically the `bindDpopIfPossible` POST or the layout's
    // `session.load()` probe) has a valid nonce and doesn't eat a
    // `use_dpop_nonce` challenge → retry cycle. Cookie survives the 302
    // follow (Set-Cookie is applied by the browser across redirects,
    // unlike other response headers). No-op when `dpop_mode = off`.
    cookie_auth::maybe_append_dpop_nonce_cookie(
        response.headers_mut(),
        &state.dpop_nonce_service,
        state.core.config.auth.dpop_mode,
    );
    // Clear the request-challenge cookie — it's single-use and we don't
    // want a stale value on the browser confusing a later flow.
    cookie_auth::append_clear_magic_request_cookie(response.headers_mut());

    response
}

/// Build the SPA route the redemption should land on.
///
/// - **Resource token** (folder invitation): deep-link into the folder
///   view. SvelteKit `files/[...path]` accepts folder IDs as path
///   segments (see `frontend/src/routes/files/[...path]/+page.svelte`
///   — `goto(resolve(`/files/${folder.id}`))`).
/// - **NULL-resource token + external user**: land on `/shared-with-me`
///   (their entry point — they own no folders themselves).
/// - **NULL-resource token + internal user**: land on `/files` (the
///   user has a home folder; the "shared with me" view would be empty
///   on first signup, so home is the better welcome). Internal users
///   on NULL-resource tokens come from the email-only-signup welcome
///   path (PR 18) or from a magic-link they requested themselves
///   while password-eligible-and-lenient-mode (PR 19).
///
/// Historical: pre-SvelteKit these were hash routes
/// (`/#/files`, `/#/sharedwithme`, `/#/files/folder/{id}`) served by the
/// legacy vanilla frontend. Landing on those now serves the legacy
/// shell (with old meta-CSP + inline scripts) instead of the SPA and
/// triggers a CSP violation on modern deployments.
fn redirect_target(redemption: &MagicLinkRedemption) -> String {
    match (redemption.resource_kind, redemption.resource_id) {
        (Some(MagicLinkResourceKind::Folder), Some(folder_id)) => {
            format!("/files/{}", folder_id)
        }
        _ if redemption.auth.user.is_external => "/shared-with-me".to_string(),
        _ => "/files".to_string(),
    }
}
