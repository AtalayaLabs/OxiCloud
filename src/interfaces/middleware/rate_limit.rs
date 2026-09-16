//! IP-based rate limiting middleware for authentication endpoints.
//!
//! Uses `moka` TTL caches (already a project dependency) to track request
//! counts per client IP.  Each protected endpoint group gets its own
//! [`RateLimiter`] instance with independently tuneable limits.
//!
//! Client IP resolution is delegated to [`super::trusted_proxy::client_ip`],
//! which honours `OXICLOUD_TRUST_PROXY_CIDR` for proxy-header forwarding.
//!
//! When the limit is exceeded a `429 Too Many Requests` response is returned
//! with a `Retry-After` header indicating how many seconds to wait.

use axum::{
    http::{HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use moka::sync::Cache;
use std::sync::Arc;
use std::time::Duration;

/// A simple sliding-window counter keyed by IP address.
///
/// Each key lives for `window` seconds; every request increments the counter.
/// Once the counter reaches `max_requests` the request is rejected.
#[derive(Clone)]
pub struct RateLimiter {
    /// Maps `IP -> request_count` with automatic TTL expiration.
    cache: Cache<String, u32>,
    /// Maximum requests allowed within the window.
    max_requests: u32,
    /// Window duration in seconds (also used for `Retry-After`).
    window_secs: u64,
}

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// * `max_requests`, ceiling per IP within the window
    /// * `window_secs` , sliding window duration
    /// * `max_entries` , upper bound on tracked IPs (evicts LRU when exceeded)
    pub fn new(max_requests: u32, window_secs: u64, max_entries: u64) -> Self {
        let cache = Cache::builder()
            .time_to_live(Duration::from_secs(window_secs))
            .max_capacity(max_entries)
            .build();
        Self {
            cache,
            max_requests,
            window_secs,
        }
    }

    /// Check whether the IP is allowed. Returns `Ok(current_count)` or
    /// `Err(StatusCode::TOO_MANY_REQUESTS)`.
    #[allow(clippy::result_unit_err)]
    pub fn check_and_increment(&self, ip: &str) -> Result<u32, ()> {
        // Lock-free read (borrows the key — no allocation), then one
        // write-back. The previous shape allocated the key TWICE and paid
        // a locking `entry()` op on top of the insert; moka's
        // `and_upsert_with` alternative benchmarked slower still
        // (benches/ROUND11.md §20). Read-then-write is not atomic, but it
        // never was — under a concurrent burst both shapes can undercount
        // the same way, which only makes the limiter marginally lenient,
        // never wrongly strict.
        let count = self.cache.get(ip).unwrap_or(0) + 1;

        // On re-insert moka resets the TTL; for rate limiting this is fine
        // because it means the window "slides" forward on activity.
        self.cache.insert(ip.to_string(), count);

        if count > self.max_requests {
            Err(())
        } else {
            Ok(count)
        }
    }

    /// Seconds the client should wait before retrying.
    pub fn retry_after(&self) -> u64 {
        self.window_secs
    }
}

// ─── Axum middleware factories ──────────────────────────────────────────────

/// Extract the most-likely real client IP from headers / connection info.
///
/// Proxy headers (`X-Forwarded-For`, `X-Real-Ip`) are only trusted when the
/// TCP peer address falls within `OXICLOUD_TRUST_PROXY_CIDR`.  Without a
/// configured CIDR list an attacker could spoof headers to bypass rate limiting.
pub fn extract_client_ip<B>(req: &Request<B>) -> String {
    super::trusted_proxy::client_ip(req, false)
}

/// Build a rate-limit response with the standard `Retry-After` header.
///
/// Public so handlers that do their own (non-middleware) rate checks —
/// e.g. the email-invite branch of `POST /api/grants`, where the limit
/// only applies to one subject variant — can return the same shape.
pub fn too_many_requests(retry_after: u64) -> Response {
    let body = serde_json::json!({
        "error": "Too many requests",
        "retry_after_secs": retry_after,
    });
    let mut resp = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
    if let Ok(val) = HeaderValue::from_str(&retry_after.to_string()) {
        resp.headers_mut().insert("retry-after", val);
    }
    resp
}

/// Shared body of every per-endpoint limiter below.
///
/// The public wrappers differ only in the `endpoint` they name in the warning,
/// so the decision lives here once: each endpoint gets its own `RateLimiter`
/// (its own budget), but they all enforce it identically.
async fn enforce(
    limiter: &RateLimiter,
    endpoint: &'static str,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let ip = extract_client_ip(&req);
    match limiter.check_and_increment(&ip) {
        Ok(_) => next.run(req).await,
        Err(()) => {
            tracing::warn!(ip = %ip, endpoint, "Rate limit exceeded");
            too_many_requests(limiter.retry_after())
        }
    }
}

/// Axum middleware: rate-limit login attempts.
///
/// Inject via:
/// ```ignore
/// .layer(axum::middleware::from_fn_with_state(limiter, rate_limit_login))
/// ```
pub async fn rate_limit_login(
    State(limiter): axum::extract::State<Arc<RateLimiter>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    enforce(&limiter, "login", req, next).await
}

/// Axum middleware: rate-limit registration attempts.
pub async fn rate_limit_register(
    State(limiter): axum::extract::State<Arc<RateLimiter>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    enforce(&limiter, "register", req, next).await
}

/// Axum middleware: rate-limit token refresh attempts.
pub async fn rate_limit_refresh(
    State(limiter): axum::extract::State<Arc<RateLimiter>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    enforce(&limiter, "refresh", req, next).await
}

/// Axum middleware: rate-limit share-password verification.
///
/// `POST /api/s/{token}/verify` is unauthenticated and runs Argon2id at the
/// configured memory cost per call, so it is both a password oracle and a
/// memory amplifier — the same two properties that earned `/api/auth/login` a
/// limiter. It is budgeted from `login_max_requests`/`login_window_secs` for
/// exactly that reason: a share password is a password, and an operator who
/// tightens the login budget means to tighten password guessing everywhere.
pub async fn rate_limit_share_verify(
    State(limiter): axum::extract::State<Arc<RateLimiter>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    enforce(&limiter, "share_verify", req, next).await
}

use axum::extract::State;
