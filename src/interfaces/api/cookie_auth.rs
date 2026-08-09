//! HttpOnly cookie helpers for secure token transport.
//!
//! Tokens are set as `HttpOnly; SameSite=Lax` cookies so that
//! browser-based JavaScript cannot read them (mitigates XSS token theft).
//! The `Secure` flag is controlled by the `OXICLOUD_COOKIE_SECURE` env var
//! (default: auto-detect from `OXICLOUD_BASE_URL`).
//!
//! A companion **non-HttpOnly** CSRF cookie (`oxicloud_csrf`) is set
//! alongside the auth cookies.  The frontend must read it and echo its
//! value back as `X-CSRF-Token` on every state-changing request.
//! A middleware (`csrf_middleware`) validates the match.
//!
//! DAV clients continue to use `Authorization: Basic` with app passwords
//! and are completely unaffected by this mechanism.

use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, HeaderValue};

/// Cookie name for the JWT access token.
pub const ACCESS_COOKIE: &str = "oxicloud_access";
/// Cookie name for the opaque refresh token.
pub const REFRESH_COOKIE: &str = "oxicloud_refresh";
/// Cookie name for the CSRF double-submit token (readable by JS).
pub const CSRF_COOKIE: &str = "oxicloud_csrf";
/// Header the frontend must send with the CSRF token value.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Per-request challenge cookie for browser-bound magic-link
/// redemption (PR 22). Set by `POST /api/auth/magic-link/send` on
/// the requesting browser; checked by `GET /magic/v1/{token}` against
/// the token row's `request_challenge` column. Limited to `/magic`
/// so it only travels back on the redemption endpoint.
pub const MAGIC_REQUEST_COOKIE: &str = "oxicloud_magic_request";
/// One-shot **non-HttpOnly** cookie carrying a fresh `DPoP-Nonce` value
/// on login-success responses (POST OPAQUE/legacy and 302 OIDC/magic-link
/// redirects alike). The SPA reads it on mount and seeds the client-side
/// nonce cache, so the very first bound request under `DPOP=required`
/// doesn't have to eat a 401 `use_dpop_nonce` challenge before its retry
/// succeeds. Short TTL: nonces rotate server-side every ~30 s, and this
/// cookie is single-shot (cleared by the SPA after reading).
pub const DPOP_NONCE_COOKIE: &str = "oxicloud_dpop_nonce";
/// TTL for the DPoP-nonce hand-off cookie. 60 s is well within the
/// server-side pool TTL, and if the SPA doesn't read it within a
/// minute the client either lacks DPoP support (harmless waste) or
/// is broken (a stale cookie doesn't hurt — the middleware challenge-
/// retry still kicks in on first use).
const DPOP_NONCE_COOKIE_MAX_AGE_SECS: i64 = 60;

/// Whether the `Secure` flag should be set on cookies.
///
/// Resolution order:
/// 1. `OXICLOUD_COOKIE_SECURE=true|false` — explicit override.
/// 2. `OXICLOUD_BASE_URL` starts with `https` → `true`.
/// 3. `OXICLOUD_BASE_URL` starts with `http` → `false`.
/// 4. **Default: `false`** for compatibility with HTTP deployments
///    (Docker, local development). Set `OXICLOUD_COOKIE_SECURE=true`
///    explicitly for production HTTPS environments.
pub fn is_cookie_secure() -> bool {
    cookie_secure()
}

/// Memoised [`resolve_cookie_secure`]. The flag is a pure function of two
/// process-invariant env vars, yet a single login used to re-resolve it
/// ~4× (two auth cookies + the CSRF cookie + the handler's own probe) —
/// each call paying the env-lock syscalls and re-emitting the same
/// "⚠️ SECURITY" log line. Resolve once, log once.
fn cookie_secure() -> bool {
    static COOKIE_SECURE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *COOKIE_SECURE.get_or_init(resolve_cookie_secure)
}

fn resolve_cookie_secure() -> bool {
    if let Ok(v) = std::env::var("OXICLOUD_COOKIE_SECURE") {
        let secure = v == "true" || v == "1";
        if !secure {
            tracing::warn!(
                "⚠️  SECURITY: OXICLOUD_COOKIE_SECURE is explicitly disabled — \
                 cookies will be sent over plain HTTP. \
                 Do NOT use this in production."
            );
        }
        return secure;
    }
    // Auto-detect from base URL, defaulting to insecure for compatibility
    match std::env::var("OXICLOUD_BASE_URL") {
        Ok(url) if url.starts_with("https") => true,
        Ok(url) if url.starts_with("http://") => {
            tracing::info!(
                "⚠️  SECURITY: OXICLOUD_BASE_URL is HTTP — cookie Secure flag is OFF. \
                 Set OXICLOUD_COOKIE_SECURE=true to override if your proxy terminates TLS."
            );
            false
        }
        _ => {
            // Default to false for compatibility with HTTP deployments
            tracing::info!(
                "⚠️  SECURITY: OXICLOUD_BASE_URL not set — defaulting to non-secure cookies \
                 for HTTP compatibility. Set OXICLOUD_COOKIE_SECURE=true for HTTPS deployments."
            );
            false
        }
    }
}

/// Build a `Set-Cookie` header value.
fn build_cookie(name: &str, value: &str, path: &str, max_age_secs: i64, same_site: &str) -> String {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    format!(
        "{name}={value}; HttpOnly; SameSite={same_site}; Path={path}; Max-Age={max_age_secs}{secure}",
    )
}

/// Append `Set-Cookie` headers for both access and refresh tokens.
///
/// The access cookie covers all paths (`/`) because the API lives under
/// `/api`, CalDAV under `/caldav`, WebDAV under `/webdav`, etc.
///
/// The refresh cookie is restricted to `/api/auth` so it is only sent
/// when the client explicitly calls the refresh or logout endpoints.
pub fn append_auth_cookies(
    headers: &mut HeaderMap,
    access_token: &str,
    refresh_token: &str,
    access_expiry_secs: i64,
    refresh_expiry_secs: i64,
) {
    if let Ok(val) = HeaderValue::from_str(&build_cookie(
        ACCESS_COOKIE,
        access_token,
        "/",
        access_expiry_secs,
        "Lax", // Lax: cookie is sent on top-level navigations (links from other sites)
    )) {
        headers.append(SET_COOKIE, val);
    }
    if let Ok(val) = HeaderValue::from_str(&build_cookie(
        REFRESH_COOKIE,
        refresh_token,
        "/api/auth",
        refresh_expiry_secs,
        "Strict", // Strict: refresh endpoint is never reached via cross-site navigation
    )) {
        headers.append(SET_COOKIE, val);
    }
}

/// Append `Set-Cookie` headers that immediately expire both auth cookies,
/// effectively logging the user out on the browser side.
pub fn append_clear_cookies(headers: &mut HeaderMap) {
    for (name, path) in [(ACCESS_COOKIE, "/"), (REFRESH_COOKIE, "/api/auth")] {
        let secure = if cookie_secure() { "; Secure" } else { "" };
        let val = format!("{name}=; HttpOnly; SameSite=Lax; Path={path}; Max-Age=0{secure}",);
        if let Ok(hv) = HeaderValue::from_str(&val) {
            headers.append(SET_COOKIE, hv);
        }
    }
}

/// Extract a named cookie value from the `Cookie` request header,
/// borrowing from the header map. Callers that only compare or parse the
/// value (CSRF check) avoid the per-request copy.
pub fn extract_cookie_str<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    let cookie_header = headers.get(axum::http::header::COOKIE)?;
    let cookie_str = cookie_header.to_str().ok()?;

    for pair in cookie_str.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair.strip_prefix(name) {
            let val = val.strip_prefix('=')?;
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

/// Extract a named cookie value from the `Cookie` request header.
pub fn extract_cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    extract_cookie_str(headers, name).map(str::to_string)
}

// ────────────────────────────────────────────────────────────
// CSRF double-submit cookie helpers
// ────────────────────────────────────────────────────────────

/// Generate a cryptographically random CSRF token (128-bit UUIDv4, hex-like).
pub fn generate_csrf_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Build a **non-HttpOnly** CSRF cookie so that frontend JS can read it
/// via `document.cookie` and echo it back in the `X-CSRF-Token` header.
fn build_csrf_cookie(value: &str, max_age_secs: i64) -> String {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    format!("{CSRF_COOKIE}={value}; SameSite=Lax; Path=/; Max-Age={max_age_secs}{secure}",)
}

/// Append a CSRF double-submit cookie alongside the auth cookies.
/// Should be called in every endpoint that also sets auth cookies.
pub fn append_csrf_cookie(headers: &mut HeaderMap, access_expiry_secs: i64) {
    let token = generate_csrf_token();
    if let Ok(val) = HeaderValue::from_str(&build_csrf_cookie(&token, access_expiry_secs)) {
        headers.append(SET_COOKIE, val);
    }
}

/// Generate a per-request challenge for the magic-link browser
/// binding (PR 22). 128-bit UUIDv4 — same shape as `generate_csrf_token`,
/// plenty of entropy to make brute-force matching infeasible during
/// the 10-minute login TTL. The value is set as a cookie on the
/// originating browser AND mirrored into the token row so the
/// redemption endpoint can compare them.
pub fn generate_magic_request_challenge() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Append the `oxicloud_magic_request` cookie that binds a
/// login-via-email magic-link to the originating browser (PR 22).
/// HttpOnly + SameSite=Strict + Path=/magic — only sent back when
/// the user clicks the redemption link, never on cross-site
/// navigations. `value` is a random URL-safe string the handler
/// also mirrors into `auth.magic_link_tokens.request_challenge`.
pub fn append_magic_request_cookie(headers: &mut HeaderMap, value: &str, max_age_secs: i64) {
    if let Ok(val) = HeaderValue::from_str(&build_cookie(
        MAGIC_REQUEST_COOKIE,
        value,
        "/magic",
        max_age_secs,
        "Strict",
    )) {
        headers.append(SET_COOKIE, val);
    }
}

/// Clear the `oxicloud_magic_request` cookie after redemption — the
/// challenge is single-use, so we don't want a stale cookie on the
/// browser confusing a later flow.
pub fn append_clear_magic_request_cookie(headers: &mut HeaderMap) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let val = format!(
        "{MAGIC_REQUEST_COOKIE}=; HttpOnly; SameSite=Strict; Path=/magic; Max-Age=0{secure}",
    );
    if let Ok(hv) = HeaderValue::from_str(&val) {
        headers.append(SET_COOKIE, hv);
    }
}

/// Stamp the DPoP-nonce hand-off cookie alongside the auth cookies on
/// any login-success response — but only when DPoP is actually enforced
/// server-side, otherwise the cookie is dead weight the client would
/// read and discard. Uses `state.dpop_nonce_service.current_or_rotate()`
/// under the hood — the SAME nonce the middleware would stamp on any
/// authenticated response, so a subsequent bound request presenting a
/// proof with `nonce = <this value>` validates against the live pool
/// without ever touching a `use_dpop_nonce` retry.
///
/// SameSite=Strict + Path=/: cookie only travels back to our origin, on
/// any route the SPA might land on after login. Survives the 302 follow
/// on OIDC / magic-link flows (Set-Cookie IS applied across redirects).
pub fn maybe_append_dpop_nonce_cookie(
    headers: &mut HeaderMap,
    nonce_service: &crate::infrastructure::services::dpop_nonce_service::DpopNonceService,
    dpop_mode: crate::common::config::DpopMode,
) {
    if matches!(dpop_mode, crate::common::config::DpopMode::Off) {
        return;
    }
    let value = nonce_service.current_or_rotate();
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let val = format!(
        "{DPOP_NONCE_COOKIE}={value}; SameSite=Strict; Path=/; Max-Age={DPOP_NONCE_COOKIE_MAX_AGE_SECS}{secure}",
    );
    if let Ok(hv) = HeaderValue::from_str(&val) {
        headers.append(SET_COOKIE, hv);
    }
}

/// Clear the CSRF cookie (on logout).
pub fn append_clear_csrf_cookie(headers: &mut HeaderMap) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let val = format!("{CSRF_COOKIE}=; SameSite=Lax; Path=/; Max-Age=0{secure}",);
    if let Ok(hv) = HeaderValue::from_str(&val) {
        headers.append(SET_COOKIE, hv);
    }
}
