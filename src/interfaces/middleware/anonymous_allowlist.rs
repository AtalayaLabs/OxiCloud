//! Confine anonymous (public-share) sessions to an explicit set of routes.
//!
//! ## Why this exists when the extractors already refuse anonymous
//!
//! `AuthUser` and `CurrentUserId` reject `role = anonymous`, which closes
//! every handler that takes one — currently all of them. That is a claim
//! about ~200 handler signatures, verifiable only by checking ~200 handler
//! signatures, and true only until someone adds the 201st.
//!
//! This layer makes the opposite, stronger claim: **an anonymous session may
//! reach these paths and no others**, verifiable by reading one constant. A
//! route added later without a principal extractor is denied by default
//! rather than exposed by default.
//!
//! The two are deliberately independent. The extractor guard does not rely
//! on this layer being positioned correctly, and this layer does not rely on
//! handlers taking the right extractor. Neither is load-bearing alone.
//!
//! ## Scope
//!
//! The rule is narrow on purpose:
//!
//! ```text
//! role == anonymous  AND  matched route ∉ ALLOWLIST   →  403
//! ```
//!
//! `user` and `admin` traffic is untouched — this cannot become a general
//! router-level authorization mechanism by accident, because it has nothing
//! to say about any other principal.
//!
//! ## Matching
//!
//! Matching is on axum's [`MatchedPath`] — the route *pattern*
//! (`/api/files/{id}`), never the concrete URI (`/api/files/abc-123`).
//! Pattern matching means no string surgery, no prefix confusion, and no
//! risk that `/api/files/../admin/users` resolves to something the list did
//! not intend.
//!
//! `MatchedPath` is only populated once a route has matched, so this must be
//! installed with `route_layer` (inner, post-routing) and not `layer`
//! (outer, pre-routing). If it is ever absent the request is **denied** —
//! the failure mode of misplacing this layer is anonymous access breaking
//! loudly, not leaking quietly.

use axum::{
    extract::{MatchedPath, Request},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use crate::application::dtos::user_dto::CurrentUser;

/// Route patterns an anonymous public-share session may reach.
///
/// Kept as route patterns so they can be compared directly against
/// `MatchedPath`, and so this list can be cross-checked against the
/// `share:read` security declarations in the OpenAPI spec.
///
/// `share:read` names a CAPABILITY, not a caller: these are the reads a share
/// token permits. An empty scopes array stays the implicit `role:user`
/// default, and `role:admin` marks the admin nest — only the two exceptional
/// cases are declared, because the risk lives in the exceptions.
///
/// Adding an entry is a decision to expose that endpoint to anyone holding
/// a share link. It is not a place to add something "just to make a test
/// pass".
pub const ANONYMOUS_ALLOWLIST: &[&str] = &[
    "/api/folders/{id}",
    "/api/folders/{id}/resources",
    "/api/folders/{id}/ancestors",
    "/api/folders/{id}/download",
    "/api/files/{id}",
    "/api/files/{id}/thumbnail/{size}",
    "/api/files/{id}/metadata",
];

/// The decision, split from the middleware so it is testable without a
/// router, a request or a session.
///
/// `GET` only. Every allowlisted route is a read, and a method check here
/// means a future `PUT` mounted on an already-listed path — the shape
/// `get(get_thumbnail).put(upload_thumbnail)` already in the tree — does not
/// silently inherit anonymous access.
pub fn anonymous_may_reach(matched_path: Option<&str>, method: &Method) -> bool {
    if method != Method::GET {
        return false;
    }
    // Absent `MatchedPath` means this layer ran before routing, or on a
    // fallback. Either way we cannot tell what was matched, so we refuse.
    let Some(path) = matched_path else {
        return false;
    };
    ANONYMOUS_ALLOWLIST.contains(&path)
}

/// `route_layer` middleware enforcing [`anonymous_may_reach`].
///
/// Runs after `auth_middleware` has inserted `Arc<CurrentUser>`, so the role
/// is available. Requests with no principal fall through untouched —
/// authentication is not this layer's job.
pub async fn anonymous_allowlist_layer(req: Request, next: Next) -> Response {
    let Some(current_user) = req.extensions().get::<Arc<CurrentUser>>().cloned() else {
        return next.run(req).await;
    };
    if !current_user.is_anonymous() {
        return next.run(req).await;
    }

    let matched = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned());

    if anonymous_may_reach(matched.as_deref(), req.method()) {
        return next.run(req).await;
    }

    tracing::info!(
        target: "audit",
        event = "authz.denied",
        reason = "anonymous_route_not_allowlisted",
        caller_id = %current_user.id,
        method = %req.method(),
        matched_path = matched.as_deref().unwrap_or("<unmatched>"),
        "👮🏻‍♂️ anonymous session refused a route outside the share allowlist",
    );

    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": "This endpoint is not available to public share visitors"
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlisted_reads_are_permitted() {
        for path in ANONYMOUS_ALLOWLIST {
            assert!(
                anonymous_may_reach(Some(path), &Method::GET),
                "{path} is on the allowlist and must be reachable",
            );
        }
    }

    /// The point of the layer: everything not listed is refused, including
    /// the endpoints an anonymous visitor would most like to reach.
    #[test]
    fn everything_else_is_refused() {
        for path in [
            "/api/users/{id}",
            "/api/admin/users",
            "/api/groups/search",
            "/api/address-books",
            "/api/auth/app-passwords",
            "/api/rt/ticket",
            "/api/trash/resources",
            "/api/search",
            "/api/batch/download",
            "/api/wopi/editor-url",
            "/api/folders",
            "/api/files",
        ] {
            assert!(
                !anonymous_may_reach(Some(path), &Method::GET),
                "{path} is not allowlisted and must be refused",
            );
        }
    }

    /// A concrete URI must never match. Matching is on the route pattern,
    /// so if `MatchedPath` were ever replaced with `uri().path()` this test
    /// fails rather than the allowlist silently matching nothing (which
    /// would deny everything) or, worse, matching by prefix.
    #[test]
    fn concrete_uris_do_not_match_patterns() {
        assert!(!anonymous_may_reach(
            Some("/api/files/abc-123/thumbnail/preview"),
            &Method::GET
        ));
        assert!(!anonymous_may_reach(
            Some("/api/folders/abc-123"),
            &Method::GET
        ));
    }

    /// An allowlisted path is allowlisted for GET only. `routes.rs` already
    /// mounts `get(get_thumbnail).put(upload_thumbnail)` on one path, so a
    /// pattern being listed must not hand over its other verbs.
    #[test]
    fn non_get_methods_are_refused_even_on_allowlisted_paths() {
        for method in [
            Method::PUT,
            Method::POST,
            Method::DELETE,
            Method::PATCH,
            Method::HEAD,
        ] {
            assert!(
                !anonymous_may_reach(Some("/api/files/{id}/thumbnail/{size}"), &method),
                "{method} on an allowlisted path must still be refused",
            );
        }
    }

    /// Fail closed. If this layer is ever installed with `layer` instead of
    /// `route_layer`, `MatchedPath` is absent for every request and
    /// anonymous access breaks visibly — rather than silently allowing.
    #[test]
    fn a_missing_matched_path_is_refused() {
        assert!(!anonymous_may_reach(None, &Method::GET));
    }
}
