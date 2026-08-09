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
            (DpopMode::Required, Some(_)) => {
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
                metrics::counter!("oxicloud_dpop_proof_missing_total").increment(1);
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
                metrics::counter!("oxicloud_dpop_header_missing_on_bound_session_total")
                    .increment(1);
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
                    metrics::counter!(
                        "oxicloud_dpop_verify_failed_total",
                        "reason" => "nonce_stale",
                    )
                    .increment(1);
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
                metrics::counter!("oxicloud_dpop_replay_detected_total").increment(1);
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
            metrics::counter!(
                "oxicloud_dpop_verify_failed_total",
                "reason" => err.reason(),
            )
            .increment(1);
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
///
/// Central counter emission (`oxicloud_dpop_nonce_challenges_issued_total`)
/// lives here rather than at each callsite — every challenge goes
/// through this helper by construction, so one increment covers all
/// three current paths (proof-missing, nonce-missing, nonce-stale).
fn nonce_challenge_response(
    nonce_service: &crate::infrastructure::services::dpop_nonce_service::DpopNonceService,
) -> Response {
    metrics::counter!("oxicloud_dpop_nonce_challenges_issued_total").increment(1);
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

}
