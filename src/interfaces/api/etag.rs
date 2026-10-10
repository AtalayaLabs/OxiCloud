//! Shared `If-None-Match` revalidation helpers for `/resources` listings.
//!
//! Pairs with [`CursorListResponse::weak_etag`](crate::application::dtos::cursor::CursorListResponse::weak_etag):
//! the DTO layer computes a page-identity ETag from `(cursor, limit,
//! fresh_signal, row_count, next_cursor)`; this module turns that
//! string into HTTP wire shape (`ETag` header + `Cache-Control:
//! private, no-cache` + 304 revalidation).
//!
//! §2 of `docs/plan/photos-resources-migration.md`. Each `/resources`
//! handler calls [`if_none_match_matches`] against the request's
//! `If-None-Match` header BEFORE serialising the response body — a
//! cached-client revalidation returns an empty 304 instead of
//! reshipping every tile on an unchanged page.
//!
//! Three call-site LoC per handler. The `/api/photos/resources`
//! handler is the first adopter; favorites / recents / trash / folder
//! contents retrofit independently later.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::response::IntoResponse;

/// Return `true` when the caller's `If-None-Match` header matches the
/// freshly computed ETag exactly.
///
/// Strict equality: no weak/strong normalisation, no multi-value
/// parsing. The ETags produced by [`CursorListResponse::weak_etag`]
/// are a single quoted token with no `W/` prefix, so echoed verbatim
/// by every well-behaved HTTP cache. Clients that send `*` as the
/// precondition get `false` here — those are "if the resource exists
/// at all" semantics that don't fit the listing-revalidation shape;
/// the handler then returns 200 with a fresh body (the correct
/// response to `If-None-Match: *` on a GET of a listing).
pub fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|client| client == etag)
        .unwrap_or(false)
}

/// Empty 304 response — echoes the ETag back and stamps
/// `Cache-Control: private, no-cache` so the browser-level HTTP
/// cache treats the response as revalidate-every-time.
///
/// `private` keeps the entry out of any shared cache (reverse
/// proxies, CDNs): a listing is user-scoped, and a shared cache
/// would reveal one user's page shape to another on a URL hash
/// collision. `no-cache` is deliberately NOT `no-store` — we want
/// the browser to KEEP the body around for the next
/// `If-None-Match` round-trip, we just don't want it served
/// without revalidation.
pub fn not_modified(etag: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, "private, no-cache")
        .body(Body::empty())
        .expect("static builder values are always valid")
}

/// Stamp `ETag` + `Cache-Control: private, no-cache` headers onto an
/// existing response. Caller owns body construction; this helper
/// only installs the two headers that pair with
/// [`not_modified`]'s 304 path.
///
/// A malformed `etag` (containing non-ASCII or control bytes) would
/// panic the `HeaderValue::from_str` conversion — these come from
/// `format!("\"{:x}\"", …)` which is ASCII-only by construction, so
/// we treat the error as unreachable and `expect` it.
pub fn with_cache_headers(response: impl IntoResponse, etag: &str) -> Response<Body> {
    let mut resp = response.into_response();
    let h = resp.headers_mut();
    h.insert(
        header::ETAG,
        HeaderValue::from_str(etag).expect("ETag strings are ASCII hex"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    resp
}
