//! DPoP proof enforcement middleware (RFC 9449).
//!
//! Runs AFTER the auth middleware — reads the `CurrentUser`
//! extension to know the caller has an authenticated session, and
//! validates the `DPoP` header against the session's stored JWK
//! thumbprint (`session.dpop_jkt`).
//!
//! Mode dispatch (from `OXICLOUD_DPOP_MODE`):
//!   * **Off** — pass-through, no work. Safe default.
//!   * **Opportunistic** — verify when present, allow when absent.
//!     Rollout mode: catches client bugs before enforcement.
//!   * **Required** — bound sessions MUST present a valid proof.
//!     Unbound sessions (`dpop_jkt IS NULL`) remain exempt (app
//!     passwords, legacy).
//!
//! Failure response shape mirrors RFC 9449 §7.1:
//!   * generic bad proof → `401` + `WWW-Authenticate: DPoP
//!     error="invalid_dpop_proof"`
//!   * nonce missing / stale (Gate 5b) → `401` +
//!     `WWW-Authenticate: DPoP error="use_dpop_nonce"` +
//!     `DPoP-Nonce: <fresh>` — client retries once, transparently.
//!
//! Body is JSON `{"error_type": "DpopVerificationFailed"}` in both
//! cases (anti-enumeration: same shape regardless of reason; the
//! audit line carries the machine-readable reason).
//!
//! Every response — success OR failure — also gets a `DPoP-Nonce`
//! header pointing at the currently-fresh server-issued nonce.
//! Clients cache it; the next request presents it and skips the
//! challenge round trip.
//!
//! Replay detection: after nonce validation succeeds, the
//! `(nonce, jti)` pair is recorded in a moka LRU. A second proof
//! carrying the same `(nonce, jti)` — the classic replay window —
//! fires `dpop.replay_detected` and returns 401 with the standard
//! `invalid_dpop_proof` error shape.

use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use crate::common::config::DpopMode;
use crate::common::di::AppState;
use crate::infrastructure::services::dpop_verifier::{
    DpopRequestContext, DpopVerifyError, verify as verify_proof,
};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::CurrentUser;

/// Content-serving GETs the browser fetches directly from `<a href>`,
/// `<img src>`, `<video src>`, `<a download>` — no JS in the loop, so
/// there's no way to attach a DPoP proof (the fetch interceptor never
/// runs). Exempt them from the "bound session missing proof" hard
/// enforcement so the SPA can still show thumbnails, play videos, and
/// download files after DPoP=required flips on.
///
/// Security posture: an attacker with a stolen cookie can GET these
/// URLs BUT ONLY if they already know a specific 128-bit UUID.
/// Every listing / discovery endpoint (`GET /api/folders/<id>/children`,
/// `/search`, `/api/files/by-hash`, `/api/photos`, …) still requires
/// DPoP because it's called via `apiFetch`. So a bare stolen cookie
/// gives the attacker "download the exact IDs you already know" —
/// effectively nothing without prior knowledge.
///
/// Proofs that ARE sent on these paths (e.g. an image preloader that
/// went through `apiFetch` and blob'd) still get fully verified —
/// this only bypasses the missing-proof reject, not the verifier
/// itself.
///
/// Long-term Option B (signed short-lived URL tokens) would let us
/// remove this allowlist entirely; tracked as a separate task.
fn is_content_serve_get(method: &axum::http::Method, path: &str) -> bool {
    if method != axum::http::Method::GET {
        return false;
    }
    // Note: `path` is nest-stripped by axum (`/api` prefix removed by
    // the `/api` nest), so we match against the inner segment.
    // `/files/<uuid>` (download / inline)
    // `/files/<uuid>/thumbnail/<size>` (thumbnails)
    // `/folders/<uuid>/download` (zip download)
    // `/photos/<uuid>/preview` (photo preview, if present)
    matches_content_path(path)
}

fn matches_content_path(path: &str) -> bool {
    // Collect segments into a fixed stack buffer, then match all
    // allowed shapes as slice patterns — reads left-to-right like
    // the paths themselves. Any path deeper than the buffer or not
    // matching a listed shape falls through to `false`. No alloc,
    // no regex, no leading-slash surprises.
    //
    // UUID guards (`if looks_like_uuid(id)`) disambiguate
    // `/files/<uuid>` (content) from `/files/by-hash` (a listing
    // endpoint that MUST stay behind DPoP for anti-enumeration).
    // Plugin slugs aren't UUIDs so the SSE arm just accepts any
    // segment there — the endpoint enforces its own admin AuthZ.
    let mut buf: [&str; 6] = [""; 6];
    let mut n = 0;
    for seg in path.trim_start_matches('/').split('/') {
        if n == buf.len() {
            return false;
        }
        buf[n] = seg;
        n += 1;
    }
    match &buf[..n] {
        // SSE: EventSource can't attach headers → cookie-only auth
        // (RFC 9449 known gap for streaming). Same posture as content-
        // serve — attacker still needs the exact target id.
        ["admin", "plugins", _id, "logs", "stream"] => true,
        // Content-serve GETs — browser fetches directly from
        // `<a href>`, `img src`, `video src`, `<a download>`.
        ["files", id] if looks_like_uuid(id) => true,
        ["files", id, "thumbnail", _size] if looks_like_uuid(id) => true,
        ["folders", id, "download"] if looks_like_uuid(id) => true,
        ["photos", id, "preview"] if looks_like_uuid(id) => true,
        _ => false,
    }
}

/// Cheap UUID-shape check: 36 chars, hyphens at positions 8, 13, 18,
/// 23, everything else hex. Not a full parse (nothing here needs the
/// bytes) — just enough to distinguish `<uuid>` from named endpoints
/// like `by-hash`, `upload`, `search`.
fn looks_like_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    s.bytes().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

/// Resolve the request's external `(scheme, host)` — what the client
/// sees the URL as, which is what its DPoP proof's `htu` was built
/// from. Behind a reverse proxy, the internal request scheme +
/// authority differ from the external ones; without normalising here
/// the verifier fires `wrong_htu` on every request.
///
/// Priority chain (RFC 7239-adjacent — mirror what oxicloud audit
/// spans use for `client_ip`):
///   1. `X-Forwarded-Proto` + `X-Forwarded-Host`
///   2. `Host` header with scheme inferred from `is_https` request
///   3. Fallback (`http` + `localhost`) — dev-only, unrepresentative
///
/// NB: no trust-boundary check here. If your deployment lets
/// arbitrary clients set `X-Forwarded-*`, they can already forge
/// audit-log IPs everywhere else — that's an operator responsibility
/// solved by the trusted-proxy config, not this helper.
fn external_scheme_host(headers: &HeaderMap) -> (String, String) {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_owned())
        .unwrap_or_else(|| "http".to_owned());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_owned())
        .unwrap_or_else(|| "localhost".to_owned());
    (scheme, host)
}

/// Middleware entry point mounted on authenticated `/api/*` subtrees.
pub async fn require_dpop_layer(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let mode = state.core.config.auth.dpop_mode;
    if mode == DpopMode::Off {
        return next.run(request).await;
    }
    let nonce_service = state.dpop_nonce_service.clone();
    let replay_cache = state.dpop_replay_cache.clone();

    // No authenticated user → pass through (upstream auth layer
    // already handled or will handle the 401). We only concern
    // ourselves with proof-carrying requests on authenticated paths.
    let Some(current_user) = request.extensions().get::<Arc<CurrentUser>>().cloned() else {
        return next.run(request).await;
    };

    let dpop_header = request
        .headers()
        .get("DPoP")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Gate 9 enforcement — the session's binding (from the JWT's
    // `cnf.jkt` claim, populated at token mint time from
    // `session.dpop_jkt`) tells us whether a proof is REQUIRED:
    //
    //   * unbound session (`dpop_jkt IS NONE`) — proof optional.
    //     Covers app passwords, NC clients, pre-DPoP sessions.
    //   * bound session — proof MANDATORY in required mode; a
    //     warning-only signal in opportunistic mode so operators
    //     can spot stale SPA versions before flipping enforcement.
    let expected_jkt = current_user.dpop_jkt.as_deref();
    // Diagnostic fields shared by both branches — `referer` is
    // usually the smoking gun for "which SPA page sent this?";
    // `user_agent` helps distinguish SPA (`Mozilla/…`), Node-side
    // Playwright helper (`node`), and legacy client (blank).
    let req_method = request.method().to_string();
    let req_path = request.uri().path().to_owned();
    let req_referer = request
        .headers()
        .get("referer")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let req_user_agent = request
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let Some(proof) = dpop_header else {
        match (mode, expected_jkt) {
            (DpopMode::Required, Some(_))
                if !is_content_serve_get(request.method(), request.uri().path()) =>
            {
                // Distinct event name: `dpop.proof_missing` means
                // no header was on the wire at all — no verification
                // happened. `dpop.verify_failed` is reserved for
                // proofs that WERE present but failed cryptographic
                // or claim checks. Log aggregators key off `event`
                // separately from `reason`, so the split matters.
                tracing::info!(
                    target: "audit",
                    event = "dpop.proof_missing",
                    reason = "proof_missing_on_bound_session",
                    caller_id = %current_user.id,
                    method = %req_method,
                    path = %req_path,
                    referer = %req_referer,
                    user_agent = %req_user_agent,
                    "👮🏻‍♂️ DPoP required: bound session request has no proof",
                );
                return nonce_challenge_response(&nonce_service);
            }
            (DpopMode::Opportunistic, Some(_)) => {
                // Warning-only — telemetry for the rollout window.
                // Emit the signal so operators can decide when to
                // flip default to `required`; the request still
                // completes so old clients don't break.
                tracing::info!(
                    target: "audit",
                    event = "dpop.header_missing_but_session_bound",
                    caller_id = %current_user.id,
                    method = %req_method,
                    path = %req_path,
                    referer = %req_referer,
                    user_agent = %req_user_agent,
                    "⚠️  DPoP: bound session sent request without a proof",
                );
            }
            _ => { /* unbound session or off mode — nothing to do */ }
        }
        let response = next.run(request).await;
        return stamp_current_nonce(response, &nonce_service);
    };

    // Build canonical htu — external scheme + host (`X-Forwarded-*`
    // aware) + OriginalUri path (nest-strip-safe). Query stripped
    // per RFC 9449 §4.2.
    let (scheme, host) = external_scheme_host(request.headers());
    let path = request
        .extensions()
        .get::<OriginalUri>()
        .map(|u| u.0.path().to_owned())
        .unwrap_or_else(|| request.uri().path().to_owned());
    let htu = format!("{scheme}://{host}{path}");

    let method = request.method().as_str().to_owned();
    let now_secs = chrono::Utc::now().timestamp();

    let ctx = DpopRequestContext {
        htm: &method,
        htu: &htu,
        now_secs,
        // Gate 9: pin to the session's binding when present. The
        // verifier returns `JktMismatch` if the proof's public
        // key thumbprint doesn't match — an attacker who stole a
        // bound cookie AND generated their own DPoP keypair fails
        // here. `None` means the session was minted unbound so
        // any well-formed proof passes the jkt check (still gets
        // htm/htu/nonce/replay verification).
        expected_jkt,
    };
    match verify_proof(&proof, &ctx) {
        Ok(verified) => {
            // Nonce validation: the verifier extracted the claim; if
            // present, it MUST be in our live pool. Absent → OK on
            // the bootstrap request, but the challenge below MUST
            // still fire so the very next request carries a nonce.
            let live_nonce = match verified.nonce.as_deref() {
                Some(n) if !nonce_service.is_valid(n) => {
                    tracing::info!(
                        target: "audit",
                        event = "dpop.verify_failed",
                        reason = "nonce_stale",
                        method = %method,
                        htu = %htu,
                        "👮🏻‍♂️ DPoP nonce stale — issuing challenge",
                    );
                    return nonce_challenge_response(&nonce_service);
                }
                None => {
                    // No nonce presented at all → challenge so the
                    // NEXT request carries one. The ±30s bootstrap
                    // window at the verifier means this one still
                    // succeeded, but we still want the client onto
                    // the nonce path immediately.
                    return nonce_challenge_response(&nonce_service);
                }
                Some(n) => n,
            };

            // Replay guard — nonce-scoped `jti` dedup. Runs AFTER
            // nonce validity so we don't populate the cache with
            // entries against a nonce that would 401 anyway (waste
            // of pool space; also lets an attacker probe expired
            // nonces without pressuring the cache).
            if !replay_cache.check_and_record(live_nonce, &verified.jti) {
                tracing::info!(
                    target: "audit",
                    event = "dpop.replay_detected",
                    method = %method,
                    htu = %htu,
                    jti = %verified.jti,
                    "👮🏻‍♂️ DPoP proof replayed — same (nonce, jti) seen twice",
                );
                return dpop_verification_failed_response(
                    DpopVerifyError::SignatureInvalid, // shape-only; audit line carries truth
                    &nonce_service,
                );
            }

            let response = next.run(request).await;
            stamp_current_nonce(response, &nonce_service)
        }
        Err(err) => {
            tracing::info!(
                target: "audit",
                event = "dpop.verify_failed",
                reason = err.reason(),
                method = %method,
                htu = %htu,
                "👮🏻‍♂️ DPoP proof rejected",
            );
            dpop_verification_failed_response(err, &nonce_service)
        }
    }
}

/// Stamp the currently-fresh nonce onto the outgoing response so
/// the client sees it and caches it for its next request. Called
/// on EVERY successful passthrough — the client's fetch interceptor
/// keeps its cached nonce in sync automatically.
fn stamp_current_nonce(
    mut response: Response,
    nonce_service: &crate::infrastructure::services::dpop_nonce_service::DpopNonceService,
) -> Response {
    let fresh = nonce_service.current_or_rotate();
    if let Ok(hv) = HeaderValue::from_str(&fresh) {
        response.headers_mut().insert("DPoP-Nonce", hv);
    }
    response
}

/// Build a `use_dpop_nonce` challenge response — 401 +
/// WWW-Authenticate + DPoP-Nonce carrying a fresh nonce. The SPA
/// fetch interceptor (Gate 4) auto-retries once with the new nonce
/// so users don't experience a visible failure.
fn nonce_challenge_response(
    nonce_service: &crate::infrastructure::services::dpop_nonce_service::DpopNonceService,
) -> Response {
    let mut resp = AppError::new(
        StatusCode::UNAUTHORIZED,
        "DPoP nonce required",
        "DpopVerificationFailed",
    )
    .into_response();
    resp.headers_mut().insert(
        "WWW-Authenticate",
        HeaderValue::from_static(r#"DPoP error="use_dpop_nonce""#),
    );
    let fresh = nonce_service.current_or_rotate();
    if let Ok(hv) = HeaderValue::from_str(&fresh) {
        resp.headers_mut().insert("DPoP-Nonce", hv);
    }
    resp
}

/// Build the standardised 401 response for a rejected DPoP proof.
/// Response shape: RFC 9449 §7.1 `WWW-Authenticate: DPoP error="…"`
/// plus OxiCloud's `error_type` JSON body so the SPA can key off it.
/// Also carries a fresh `DPoP-Nonce` so a client whose failure was
/// nonce-shaped (rare after this refactor, but future error paths
/// might need it) can retry immediately.
fn dpop_verification_failed_response(
    err: DpopVerifyError,
    nonce_service: &crate::infrastructure::services::dpop_nonce_service::DpopNonceService,
) -> Response {
    let mut resp = AppError::new(
        StatusCode::UNAUTHORIZED,
        "DPoP proof verification failed",
        "DpopVerificationFailed",
    )
    .into_response();
    // Static: `WWW-Authenticate` schemes stay stable across errors.
    // Only the audit `reason` field varies (already emitted).
    let www_auth = HeaderValue::from_static(r#"DPoP error="invalid_dpop_proof""#);
    resp.headers_mut().insert("WWW-Authenticate", www_auth);
    let fresh = nonce_service.current_or_rotate();
    if let Ok(hv) = HeaderValue::from_str(&fresh) {
        resp.headers_mut().insert("DPoP-Nonce", hv);
    }
    // Silence unused-parameter lint — err is captured in the audit
    // line at the callsite; this fn intentionally maps ALL failures
    // to the same client-facing shape (anti-enumeration).
    let _ = err;
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn external_scheme_host_prefers_forwarded_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        h.insert("x-forwarded-host", HeaderValue::from_static("oxi.example"));
        h.insert("host", HeaderValue::from_static("internal:8086"));
        assert_eq!(
            external_scheme_host(&h),
            ("https".to_owned(), "oxi.example".to_owned())
        );
    }

    #[test]
    fn external_scheme_host_falls_back_to_host_header() {
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("localhost:5173"));
        assert_eq!(
            external_scheme_host(&h),
            ("http".to_owned(), "localhost:5173".to_owned())
        );
    }

    #[test]
    fn external_scheme_host_takes_leftmost_of_forwarded_chain() {
        // Multiple hops → `X-Forwarded-*` becomes a comma-separated
        // list. RFC 7239 says the leftmost is the original client.
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        h.insert(
            "x-forwarded-host",
            HeaderValue::from_static("oxi.example, internal"),
        );
        assert_eq!(
            external_scheme_host(&h),
            ("https".to_owned(), "oxi.example".to_owned())
        );
    }

    #[test]
    fn external_scheme_host_defaults_when_empty() {
        let h = HeaderMap::new();
        assert_eq!(
            external_scheme_host(&h),
            ("http".to_owned(), "localhost".to_owned())
        );
    }

    // Content-serve allowlist — the paths browsers fetch directly
    // from anchors / img / video without JS in the loop. Under
    // `required` mode + bound session, missing-proof must NOT reject
    // these or the SPA breaks (downloads, thumbnails, video streaming
    // all silently 401).

    fn get(path: &str) -> bool {
        is_content_serve_get(&axum::http::Method::GET, path)
    }
    fn post(path: &str) -> bool {
        is_content_serve_get(&axum::http::Method::POST, path)
    }

    #[test]
    fn content_serve_matches_file_download() {
        assert!(get("/files/8f8e4390-1234-4a5b-8c9d-abcdef012345"));
    }

    #[test]
    fn content_serve_matches_thumbnail() {
        assert!(get("/files/8f8e4390-1234-4a5b-8c9d-abcdef012345/thumbnail/icon"));
        assert!(get("/files/8f8e4390-1234-4a5b-8c9d-abcdef012345/thumbnail/large"));
    }

    #[test]
    fn content_serve_matches_folder_zip() {
        assert!(get("/folders/8f8e4390-1234-4a5b-8c9d-abcdef012345/download"));
    }

    #[test]
    fn content_serve_matches_photo_preview() {
        assert!(get("/photos/8f8e4390-1234-4a5b-8c9d-abcdef012345/preview"));
    }

    #[test]
    fn content_serve_rejects_non_get() {
        // Attacker with stolen cookie can't mutate — POST/DELETE
        // to the SAME url still requires DPoP.
        assert!(!post("/files/8f8e4390-1234-4a5b-8c9d-abcdef012345"));
        assert!(!is_content_serve_get(
            &axum::http::Method::DELETE,
            "/files/8f8e4390-1234-4a5b-8c9d-abcdef012345"
        ));
    }

    #[test]
    fn content_serve_rejects_listing_endpoints() {
        // Discovery endpoints (children, by-hash, search) MUST NOT be
        // allowlisted — that's the whole security argument. Attacker
        // needs to already know the UUID; can't enumerate.
        assert!(!get("/folders/8f8e4390-1234-4a5b-8c9d-abcdef012345/children"));
        assert!(!get("/files/by-hash"));
        assert!(!get("/search"));
        assert!(!get("/auth/me"));
    }

    #[test]
    fn content_serve_rejects_root_or_id_only() {
        assert!(!get("/"));
        assert!(!get("/files"));
        assert!(!get("/folders"));
    }

    #[test]
    fn content_serve_matches_plugin_log_sse_stream() {
        // Server-Sent Events endpoint. Browser uses EventSource,
        // which can't attach custom headers — cookie-only auth is
        // the RFC 9449 known gap for streaming. Allowlisted.
        assert!(get("/admin/plugins/com.example.hello/logs/stream"));
        assert!(get("/admin/plugins/some-slug/logs/stream"));
    }

    #[test]
    fn content_serve_rejects_other_admin_plugin_endpoints() {
        // Only the SSE stream is allowlisted. Every other plugin
        // endpoint (list, install, uninstall, config) stays behind
        // DPoP for anti-enumeration and mutation protection.
        assert!(!get("/admin/plugins"));
        assert!(!get("/admin/plugins/some-slug"));
        assert!(!get("/admin/plugins/some-slug/logs"));
        assert!(!get("/admin/plugins/some-slug/config"));
        assert!(!post("/admin/plugins/some-slug/logs/stream"));
    }
}
