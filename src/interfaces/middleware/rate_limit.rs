//! IP-based rate limiting middleware for authentication endpoints.
//!
//! Uses `moka` TTL caches (already a project dependency) to track request
//! counts per client IP.  Each protected endpoint group gets its own
//! [`RateLimiter`] instance with independently tuneable limits.
//!
//! The middleware extracts the client IP from (in order):
//! 1. `X-Forwarded-For` header (first entry — set by reverse proxies)
//! 2. `X-Real-Ip` header
//! 3. The TCP peer address from the connection info
//!
//! When the limit is exceeded a `429 Too Many Requests` response is returned
//! with a `Retry-After` header indicating how many seconds to wait.

use axum::{
    extract::ConnectInfo,
    http::{HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use moka::sync::Cache;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Cached value of `OXICLOUD_TRUST_PROXY_HEADERS` env var.
/// Read once on first access, never again — avoids a syscall per request.
static TRUST_PROXY: OnceLock<bool> = OnceLock::new();

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
    /// * `max_requests` — ceiling per IP within the window
    /// * `window_secs`  — sliding window duration
    /// * `max_entries`  — upper bound on tracked IPs (evicts LRU when exceeded)
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
        let key = ip.to_string();
        // moka's entry API lets us atomically read-modify-write.
        // On first access the entry is inserted with count = 1 and the TTL
        // starts. Subsequent accesses within the window increment the count.
        let count = self.cache.entry(key).or_insert_with(|| 0).into_value() + 1;

        // Write back the incremented value.  Because `or_insert_with` returns
        // the *existing* value when the key was already present, we must always
        // re-insert so the counter actually advances. The TTL of the **first**
        // insert still governs eviction because moka uses insert-time TTL.
        // However, on re-insert moka resets the TTL — for rate limiting this
        // is fine because it means the window "slides" forward on activity.
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
/// Proxy headers (`X-Forwarded-For`, `X-Real-Ip`) are only trusted when
/// `OXICLOUD_TRUST_PROXY_HEADERS=true` is set.  Without a trusted reverse
/// proxy in front of the app, an attacker can spoof these headers to bypass
/// rate limiting.
pub fn extract_client_ip<B>(req: &Request<B>) -> String {
    extract_client_ip_from_parts(
        req.headers(),
        req.extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| &c.0),
    )
}

/// Same as [`extract_client_ip`], but operates on already-extracted axum parts
/// (a `&HeaderMap` plus an optional TCP peer). Handlers that don't take a full
/// `Request<B>` (e.g. those that consume the body via `Json<…>`) can still
/// derive a stable client identifier with this entry point.
pub fn extract_client_ip_from_parts(
    headers: &axum::http::HeaderMap,
    peer: Option<&SocketAddr>,
) -> String {
    let trust_proxy = *TRUST_PROXY.get_or_init(|| {
        std::env::var("OXICLOUD_TRUST_PROXY_HEADERS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false)
    });

    if trust_proxy {
        // 1. X-Forwarded-For (first entry — closest to the client)
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
            && let Some(first) = xff.split(',').next()
        {
            let ip = first.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }

        // 2. X-Real-Ip
        if let Some(xri) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            let ip = xri.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
    }

    // 3. TCP peer (ConnectInfo extension set by axum::serve)
    if let Some(addr) = peer {
        return addr.ip().to_string();
    }

    // Fallback — should never happen behind axum::serve
    "unknown".to_string()
}

/// Build a rate-limit response with the standard `Retry-After` header.
fn too_many_requests(retry_after: u64) -> Response {
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
    let ip = extract_client_ip(&req);
    match limiter.check_and_increment(&ip) {
        Ok(_) => next.run(req).await,
        Err(()) => {
            tracing::warn!(
                ip = %ip,
                "Rate limit exceeded on login endpoint"
            );
            too_many_requests(limiter.retry_after())
        }
    }
}

/// Axum middleware: rate-limit registration attempts.
pub async fn rate_limit_register(
    State(limiter): axum::extract::State<Arc<RateLimiter>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let ip = extract_client_ip(&req);
    match limiter.check_and_increment(&ip) {
        Ok(_) => next.run(req).await,
        Err(()) => {
            tracing::warn!(
                ip = %ip,
                "Rate limit exceeded on register endpoint"
            );
            too_many_requests(limiter.retry_after())
        }
    }
}

/// Axum middleware: rate-limit token refresh attempts.
pub async fn rate_limit_refresh(
    State(limiter): axum::extract::State<Arc<RateLimiter>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let ip = extract_client_ip(&req);
    match limiter.check_and_increment(&ip) {
        Ok(_) => next.run(req).await,
        Err(()) => {
            tracing::warn!(
                ip = %ip,
                "Rate limit exceeded on refresh endpoint"
            );
            too_many_requests(limiter.retry_after())
        }
    }
}

use axum::extract::State;
