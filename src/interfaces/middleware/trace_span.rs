//! Custom [`MakeSpan`], [`OnResponse`], and [`MakeRequestId`] for request tracing.
//!
//! [`UuidRequestId`] — generates a UUID v4 per request for `SetRequestIdLayer`.
//!
//! [`ClientIpMakeSpan`] — records `request_id`, `client_ip`, `method`, `uri`,
//! and a placeholder `user_id` (filled by auth middleware) on every request span.
//!
//! [`LogBadRequest`] — emits a WARN for every HTTP 400 response, inheriting
//! all span fields so the log line includes request ID, IP, user, method, URI.

use axum::http::{HeaderValue, Request, Response, StatusCode};
use std::time::Duration;
use tower_http::request_id::{MakeRequestId, RequestId};
use tower_http::trace::{MakeSpan, OnResponse};
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
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        let id = Uuid::now_v7().to_string();
        HeaderValue::from_str(&id).ok().map(RequestId::new)
    }
}

// ─── Span factory ────────────────────────────────────────────────────────────

/// Implements [`MakeSpan`] so that every HTTP request span carries
/// `request_id`, `client_ip`, `method`, `uri`, and a deferred `user_id`.
///
/// `request_id` is read from the `x-request-id` header set by
/// [`tower_http::request_id::SetRequestIdLayer`] (which must wrap this layer).
#[derive(Clone, Debug, Default)]
pub struct ClientIpMakeSpan;

impl<B> MakeSpan<B> for ClientIpMakeSpan {
    fn make_span(&mut self, request: &Request<B>) -> Span {
        let ip = super::trusted_proxy::client_ip(request, true);
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
        )
    }
}

// ─── Response observer ───────────────────────────────────────────────────────

/// Implements [`OnResponse`]: emits a WARN log for every HTTP 400 response.
#[derive(Clone, Debug, Default)]
pub struct LogBadRequest;

impl<B> OnResponse<B> for LogBadRequest {
    fn on_response(self, response: &Response<B>, latency: Duration, _span: &Span) {
        if response.status() == StatusCode::BAD_REQUEST {
            tracing::warn!(
                status = 400,
                latency_ms = latency.as_millis(),
                "bad request",
            );
        }
    }
}
