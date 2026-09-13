//! Per-request tracing primitives + the [`access_log!`] macro.
//!
//! ## What lives here
//!
//! - [`UuidRequestId`] — generates a UUID v7 per request for
//!   `tower_http::request_id::SetRequestIdLayer`.
//! - [`ClientIpMakeSpan`] — creates the per-request `req` span with
//!   `request_id`, `client_ip`, `method`, `uri`, and deferred
//!   `user_id` / `chroot_id` fields. The auth middlewares fill the
//!   deferred fields via `Span::current().record(...)`.
//! - [`access_log!`] — macro that produces an `axum::middleware`
//!   layer emitting one log event per request at a fixed tracing
//!   target. Attach to each sub-router at its mount site.
//!
//! ## Why targets are declared at mount sites (not by URI prefix)
//!
//! The router topology — `Router::nest`, `merge`, `route` — already
//! describes "this group of routes belongs to surface X". Re-deriving
//! that grouping inside the middleware by URI-prefix matching would
//! duplicate the topology and silently drift when routes are added,
//! moved, or renamed. Declaring the target right next to the
//! `nest()` / `merge()` call keeps the two in lockstep: a route
//! group can't be reached except through its mount site, and the
//! mount site is now the single source of truth for its log target.
//!
//! ## Available targets
//!
//! Targets use Rust's `::` module-path separator so
//! `tracing_subscriber::EnvFilter` recognises the hierarchy:
//! `RUST_LOG=http=info` enables every sub-target; override a single
//! one with `RUST_LOG=http=warn,http::api::auth=info`.
//!
//! Status-class → tracing level mapping (see [`access_log!`] for
//! details):
//! - `INFO`     — 2xx/3xx + 4xx + 5xx (full access trace)
//! - `WARN`     — 4xx + 5xx          (default — `http=warn`)
//! - `ERROR`    — 5xx only           (5xx-only firehose)
//!
//! Conventional targets:
//! - `http::api` — REST API under `/api/*`.
//! - `http::api::auth` — auth surface (login, refresh, app-pw, OIDC,
//!   device-auth). High-value for security operators.
//! - `http::nextcloud` — NextCloud-flavoured surface (`/remote.php`,
//!   `/ocs`, `/status.php`, `/login/v2`, `/index.php/204`).
//! - `http::dav` — CalDAV / CardDAV / WebDAV + RFC 6764 discovery.
//! - `http::wopi` — WOPI host protocol (M365 / Collabora).
//! - `http::probe` — `/health`, `/ready`, `/version`, `/openapi.json`.
//! - `http::web` — HTML pages + magic-link redemption.
//! - `http::static` — `ServeDir` fallback (CSS/JS/images at bare URLs).
//! - `http` — bare catch-all for routes that didn't get an explicit
//!   layer (loud signal that wiring is missing).

use std::time::Duration;
use tower_http::request_id::{MakeRequestId, RequestId};
use tower_http::trace::MakeSpan;
use tracing::Span;
use uuid::Uuid;

// ─── Request ID generator ────────────────────────────────────────────────────

/// Generates a UUID v7 (fast, timed, sortable) for each request.
///
/// Used with [`tower_http::request_id::SetRequestIdLayer`]:
/// ```ignore
/// .layer(SetRequestIdLayer::x_request_id(UuidRequestId))
/// ```
#[derive(Clone, Debug, Default)]
pub struct UuidRequestId;

impl MakeRequestId for UuidRequestId {
    fn make_request_id<B>(&mut self, _request: &axum::http::Request<B>) -> Option<RequestId> {
        // Stack-encode the UUID: `to_string()` allocated an intermediate
        // String per request just for HeaderValue to copy it again.
        let mut buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
        let id = Uuid::now_v7();
        axum::http::HeaderValue::from_str(id.hyphenated().encode_lower(&mut buf))
            .ok()
            .map(RequestId::new)
    }
}

// ─── Span factory ────────────────────────────────────────────────────────────

/// Implements [`MakeSpan`] so every HTTP request span carries
/// `request_id`, `client_ip`, `method`, `uri`, and deferred
/// `user_id` / `chroot_id` slots.
///
/// `request_id` is read from the `x-request-id` header set by
/// [`tower_http::request_id::SetRequestIdLayer`] (which must wrap this layer).
#[derive(Clone, Debug, Default)]
pub struct ClientIpMakeSpan;

impl<B> MakeSpan<B> for ClientIpMakeSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> Span {
        // Borrow-only IP resolution: the span records `client_ip` via `%ip`
        // (Display), so a `ClientIpDisplay` that renders straight into the
        // span's field storage avoids the per-request `String` the owned
        // `client_ip()` allocated (benches/ROUND13.md §H2).
        let ip = super::trusted_proxy::client_ip_display(request, true);
        let request_id = request
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");

        tracing::info_span!(
            "req",
            request_id = request_id,
            client_ip  = %ip,
            method     = %request.method(),
            uri        = %request.uri().path(),
            user_id    = tracing::field::Empty,

            // Set INSTEAD of `user_id` for a public-share (anonymous)
            // session — `storage.shares.id`, filled by `auth_middleware`.
            //
            // These two are mutually exclusive on purpose. An anonymous
            // session's token subject is a fresh per-visit uuid that matches
            // no `auth.users` row, so recording it as `user_id` would hand
            // operators an id that looks lookupable and is not. A line
            // carrying `share_id` says what the caller actually is.
            share_id   = tracing::field::Empty,

            // The Nextcloud chroot folder id, set by `basic_auth_middleware` (will be the Drive Id in the future).
            chroot_id  = tracing::field::Empty,
        )
    }
}

// ─── Access log macro ────────────────────────────────────────────────────────

/// Returns an [`axum::middleware`] layer that emits one log event
/// per request at a fixed tracing `target`.
///
/// **Attach at the mount site of each route group**, so the target
/// is declared next to the `nest()` / `merge()` it applies to:
///
/// ```ignore
/// use oxicloud::access_log;
///
/// app = app
///     .merge(health_routes.layer(access_log!("http::probe")))
///     .merge(magic_link_router.layer(access_log!("http::web")))
///     .nest("/api/auth", auth_router.layer(access_log!("http::api::auth")))
///     .nest("/api",      api_router.layer(access_log!("http::api")))
///     .merge(webdav_router.layer(access_log!("http::dav")))
///     .nest("/wopi",     wopi_protocol.layer(access_log!("http::wopi")))
///     .merge(web_routes.layer(access_log!("http::web")));
/// ```
///
/// Level by status class: `2xx`/`3xx` → `INFO`, `4xx` → `WARN`,
/// `5xx` → `ERROR`. With the default `RUST_LOG=…,http=warn`, only
/// 4xx and 5xx are emitted; bump to `http=info` for full request
/// tracing or narrow to `http=error` for 5xx only.
///
/// Each event inherits `request_id`, `client_ip`, `method`, `uri`,
/// `user_id`, `chroot_id` from the surrounding `req` span (created
/// by [`ClientIpMakeSpan`] at the `TraceLayer` site).
///
/// ## Why a macro
///
/// `tracing::info!(target: …)` requires the target argument to be a
/// **literal** at the macro expansion site — runtime variables are
/// rejected. The macro embeds the literal target into a `from_fn`
/// closure, so no runtime dispatch table is needed; the call site
/// is also the literal site.
#[macro_export]
macro_rules! access_log {
    ($target:literal) => {
        ::axum::middleware::from_fn(
            |req: ::axum::extract::Request, next: ::axum::middleware::Next| async move {
                // Cheaply hold the `user-agent` HeaderValue (a
                // `bytes::Bytes` clone — one atomic increment, no
                // allocation) so we can still read it after `req` is
                // moved into `next.run`. The `&str` view + format is
                // deferred to inside the per-level `enabled!`
                // branches, so no work is wasted when the filter
                // rejects the event (default `RUST_LOG=…,http=warn`
                // → 2xx/3xx never format).
                let user_agent_hv = req
                    .headers()
                    .get(::axum::http::header::USER_AGENT)
                    .cloned();
                let start = ::std::time::Instant::now();
                let response = next.run(req).await;
                let status = response.status().as_u16();
                let latency_ms = start.elapsed().as_millis() as u64;

                // Per-status-class emission. `enabled!` is an
                // ~5 ns atomic-load + comparison; below it we
                // extract `&str` views from the still-live
                // HeaderValues without allocating.
                //
                // Level mapping (shifted one rung up from the
                // historical DEBUG/INFO/WARN ladder so the default
                // `http=warn` keeps 4xx+5xx and `http=error`
                // narrows to 5xx only):
                //   5xx      → ERROR  ("server_error")
                //   4xx      → WARN   ("client_error")
                //   2xx/3xx  → INFO   ("ok")
                //
                // `content_length` is 0 for streamed bodies
                // (chunked transfer-encoding sets no Content-Length)
                // — operators reading the log should interpret `0`
                // as "empty OR streamed", not literally zero bytes.
                if status >= 500 {
                    if ::tracing::enabled!(target: $target, ::tracing::Level::ERROR) {
                        let user_agent = user_agent_hv
                            .as_ref()
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        let content_type = response
                            .headers()
                            .get(::axum::http::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        let content_length = response
                            .headers()
                            .get(::axum::http::header::CONTENT_LENGTH)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0);
                        ::tracing::error!(
                            target: $target,
                            status,
                            latency_ms,
                            content_length,
                            content_type,
                            user_agent,
                            "server_error"
                        );
                    }
                } else if status >= 400 {
                    if ::tracing::enabled!(target: $target, ::tracing::Level::WARN) {
                        let user_agent = user_agent_hv
                            .as_ref()
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        let content_type = response
                            .headers()
                            .get(::axum::http::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        let content_length = response
                            .headers()
                            .get(::axum::http::header::CONTENT_LENGTH)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0);
                        ::tracing::warn!(
                            target: $target,
                            status,
                            latency_ms,
                            content_length,
                            content_type,
                            user_agent,
                            "client_error"
                        );
                    }
                } else if ::tracing::enabled!(target: $target, ::tracing::Level::INFO) {
                    let user_agent = user_agent_hv
                        .as_ref()
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let content_type = response
                        .headers()
                        .get(::axum::http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let content_length = response
                        .headers()
                        .get(::axum::http::header::CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    ::tracing::info!(
                        target: $target,
                        status,
                        latency_ms,
                        content_length,
                        content_type,
                        user_agent,
                        "ok"
                    );
                }
                response
            },
        )
    };
}

// Re-export the macro at this module path so `use
// crate::interfaces::middleware::trace_span::access_log` works in
// addition to `crate::access_log` (which `#[macro_export]` provides).
pub use access_log;

// Convenience for tests / callers that want the same latency unit
// the macro uses.
#[doc(hidden)]
pub fn latency_ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::latency_ms;
    use std::time::Duration;

    #[test]
    fn latency_ms_is_millis() {
        assert_eq!(latency_ms(Duration::from_millis(0)), 0);
        assert_eq!(latency_ms(Duration::from_millis(7)), 7);
        assert_eq!(latency_ms(Duration::from_secs(3)), 3000);
    }
}
