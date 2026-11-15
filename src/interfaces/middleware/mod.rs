pub mod admin;
pub mod anonymous_allowlist;
pub mod auth;
pub mod csrf;
pub mod dpop;
pub mod locale;
pub mod rate_limit;
pub mod security_headers;
pub mod server_status;
pub mod trace_span;
pub mod trusted_proxy;
pub mod user;

/// A 304 has no representation body to compress, even when its length is unknown.
pub fn has_representation_body(
    status: axum::http::StatusCode,
    _: axum::http::Version,
    _: &axum::http::HeaderMap,
    _: &axum::http::Extensions,
) -> bool {
    status != axum::http::StatusCode::NOT_MODIFIED
}
