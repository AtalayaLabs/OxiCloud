//! The policy gate — one table lookup, one `evaluate`, per request.
//!
//! PR 1 step 4-iii. This is the first part of the work that ENFORCES, and
//! it is deliberately applied ALONGSIDE the layers it will replace rather
//! than instead of them (§ Rollout). Shadow order matters:
//!
//! - if the transcription is too TIGHT anywhere, this layer refuses a
//!   caller the old layer would have admitted, and `gate_matrix.hurl`'s
//!   positives fail loudly;
//! - if it is too LOOSE, the old layer still refuses, so nothing opens
//!   while the mistake is found.
//!
//! Swapping instead of shadowing would make those two cases
//! indistinguishable until something was already exploitable.
//!
//! # Why the resolver is not a precomputed `PolicyTable`
//!
//! § 2 says "compile once at startup", and that is where the VALIDATION
//! happens — `policy_declarations`'s test compiles every operation in
//! `openapi.json` and refuses to pass on a boot error or an orphan. What
//! this layer does per request is resolve ONE operation's chain, which is
//! a prefix scan over ~20 declarations. Precomputing the whole table at
//! boot would need the operation list at boot, i.e. parsing `openapi.json`
//! in the server, to save a scan that is orders of magnitude below the DB
//! round trips on every path it guards. If it ever shows up in a profile,
//! a cache keyed by `Op` goes here and changes nothing else.

use std::sync::LazyLock;

use axum::extract::MatchedPath;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use crate::application::dtos::user_dto::CurrentCaller;
use crate::application::ports::scope_ports::Op;
use crate::application::services::scope_policy::{Denial, evaluate};
use crate::application::services::scope_table::PolicyRegistry;
use crate::common::di::AppState;
use crate::interfaces::api::policy_declarations::declarations;
use crate::interfaces::errors::AppError;

/// The declarations, built once. Static data, so there is nothing to fail
/// at construction — the declarations' VALIDITY is asserted by
/// `policy_declarations`'s test against the real operation list, which is
/// where a boot refusal would surface.
static REGISTRY: LazyLock<PolicyRegistry> = LazyLock::new(declarations);

/// The DAV surfaces, whose operation identity is the METHOD rather than
/// the path: each has a single `any` handler, so every request to it
/// matches the same route template and a path-keyed lookup could not tell
/// `PROPFIND` from `DELETE`.
const DAV_PREFIXES: [&str; 3] = ["/webdav", "/caldav", "/carddav"];

/// Which operation a request IS.
///
/// Uses `MatchedPath` — the route TEMPLATE (`/api/files/{id}`), not the
/// resolved path — so a declaration written against the template matches.
/// Without a `MatchedPath` the request reached no route, and there is
/// nothing to authorize.
fn op_for(request: &Request) -> Option<Op> {
    let matched = request.extensions().get::<MatchedPath>()?;
    op_from_parts(request.method().as_str(), matched.as_str())
}

/// The resolution itself, free of axum so it can be tested directly —
/// `MatchedPath` has no public constructor.
fn op_from_parts(method: &str, matched: &str) -> Option<Op> {
    if DAV_PREFIXES.iter().any(|p| matched.starts_with(p)) {
        return Some(Op::Dav(leak(method)));
    }
    Some(Op::Rest(leak(method), leak(matched)))
}

/// `Op` holds `&'static str` because every operation is known at compile
/// time and the table should allocate nothing. A request's matched path
/// is also effectively static — it is one of a fixed set of templates —
/// but it arrives as a borrow, so it is interned here.
///
/// Bounded by the number of route templates × methods, reached within the
/// first requests after boot, which is why this is a leak rather than a
/// cache: the alternative is a lock on a hot path to store strings that
/// are never freed anyway.
fn leak(s: &str) -> &'static str {
    // A `Box::leak` per request would grow without bound, so intern.
    use std::collections::HashSet;
    use std::sync::Mutex;
    static INTERNED: LazyLock<Mutex<HashSet<&'static str>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    let mut set = INTERNED.lock().expect("interner poisoned");
    if let Some(found) = set.get(s) {
        return found;
    }
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}

/// Map a [`Denial`] to its HTTP shape.
///
/// `AppError` lives here rather than in the application layer, which is
/// why `evaluate` returns a typed denial — the dependency points inward
/// and the HTTP mapping belongs to the layer that speaks HTTP.
fn deny(denial: Denial) -> Response {
    let (status, message) = match denial {
        // No credential: the caller may simply not have sent one.
        Denial::AuthenticationRequired => (StatusCode::UNAUTHORIZED, "Authentication required"),
        // A credential exists and is not enough, so 403 — matching what
        // `require_internal_user_layer` and the admin middleware answer
        // today.
        Denial::VisitorRefused => (
            StatusCode::FORBIDDEN,
            "This endpoint requires a user session",
        ),
        Denial::RoleInsufficient { .. } => (
            StatusCode::FORBIDDEN,
            "You do not have permission to access this endpoint",
        ),
        Denial::ScopeNotGranted { .. } => (
            StatusCode::FORBIDDEN,
            "This credential was not issued for this area",
        ),
    };
    AppError::new(
        status,
        message,
        status.canonical_reason().unwrap_or("Error"),
    )
    .into_response()
}

/// Evaluate the compiled policy for this request's operation.
///
/// Runs INSIDE `auth_middleware` (i.e. layered before it, so it runs
/// after), because it reads the `CurrentCaller` that middleware inserts.
/// A missing caller therefore means "no credential was presented", which
/// only a `Public` policy admits — not "pass through", which is the
/// fail-open arm `require_internal_user_layer` still carries.
pub async fn policy_gate_layer(
    State(_state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(op) = op_for(&request) else {
        // No matched route: nothing to authorize, and the router will
        // answer 404 on its own.
        return next.run(request).await;
    };

    let policy = match REGISTRY.compile_one(op) {
        Ok(p) => p,
        Err(e) => {
            // A declaration that cannot compile is a BOOT error, caught
            // by `policy_declarations`'s test. Reaching it here means the
            // declarations changed without that test running, so fail
            // closed and say so loudly.
            tracing::error!(
                target: "audit",
                event = "authz.policy_uncompilable",
                reason = "declaration_invalid",
                op = %op,
                error = %e,
                "👮🏻‍♂️ policy failed to compile at request time — failing closed",
            );
            return deny(Denial::AuthenticationRequired);
        }
    };

    let caller = request.extensions().get::<Arc<CurrentCaller>>().cloned();
    let kind = caller.as_ref().map(|c| c.kind);

    // `None` scopes: no credential carries a scope set yet, and absent
    // grandfathers to FULL (§ Absent means FULL, everywhere). PR 2 reads
    // the credential's real set here.
    if let Err(denial) = evaluate(&policy, kind, None) {
        let (event, reason) = denial.audit();
        tracing::info!(
            target: "audit",
            event = event,
            reason = reason,
            op = %op,
            caller_id = caller.as_ref().map(|c| c.id.to_string()).unwrap_or_default(),
            "👮🏻‍♂️ policy gate refused the caller",
        );
        return deny(denial);
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::scope_ports::{MinPrincipal, MinRole, Policy};

    /// A DAV request resolves to its METHOD, not its path: all three
    /// surfaces have one `any` handler, so a path-keyed lookup could not
    /// tell `PROPFIND` from `DELETE`. This is the mapping that makes the
    /// AWARENESS-class bug impossible to repeat.
    #[test]
    fn dav_requests_resolve_to_the_method() {
        for (prefix, method) in [
            ("/webdav/{*path}", "PROPFIND"),
            ("/caldav/{*path}", "REPORT"),
            ("/carddav/", "DELETE"),
        ] {
            let op = resolve(method, prefix);
            assert_eq!(op, Some(Op::Dav(method)), "{prefix} {method}");
        }
    }

    /// REST resolves to (method, TEMPLATE). The template is what a
    /// declaration is written against, so a resolved path would match
    /// nothing.
    #[test]
    fn rest_requests_resolve_to_method_and_template() {
        assert_eq!(
            resolve("GET", "/api/files/{id}"),
            Some(Op::Rest("GET", "/api/files/{id}"))
        );
    }

    /// The declared gates come back through the resolver, so the layer
    /// enforces what `policy_declarations` transcribed.
    #[test]
    fn the_resolver_returns_the_transcribed_policies() {
        let dav = REGISTRY.compile_one(Op::Dav("PROPFIND")).expect("compiles");
        assert_eq!(dav.role, Some(MinRole::Internal));

        let admin = REGISTRY
            .compile_one(Op::Rest("GET", "/api/admin/users"))
            .expect("compiles");
        assert_eq!(admin.role, Some(MinRole::Admin));

        let transfer = REGISTRY
            .compile_one(Op::Rest("POST", "/api/admin/transfer-ownership"))
            .expect("compiles");
        assert_eq!(transfer.role, Some(MinRole::Owner));

        let allowlisted = REGISTRY
            .compile_one(Op::Rest("GET", "/api/files/{id}"))
            .expect("compiles");
        assert_eq!(allowlisted.principal, MinPrincipal::Visitor);

        let undeclared = REGISTRY
            .compile_one(Op::Rest("GET", "/api/folders"))
            .expect("compiles");
        assert_eq!(undeclared, Policy::default());
    }

    /// Denials keep today's status codes: 401 only when no credential was
    /// presented, 403 once one was and is not enough. The gate matrix
    /// asserts the same thing from outside; this pins it without a server.
    #[test]
    fn denials_keep_the_status_codes_the_layers_answer_today() {
        for (denial, expected) in [
            (Denial::AuthenticationRequired, StatusCode::UNAUTHORIZED),
            (Denial::VisitorRefused, StatusCode::FORBIDDEN),
            (
                Denial::RoleInsufficient {
                    required: MinRole::Internal,
                },
                StatusCode::FORBIDDEN,
            ),
        ] {
            assert_eq!(deny(denial).status(), expected, "{denial:?}");
        }
    }

    /// Interning is what keeps `Op`'s `&'static str` honest without a
    /// per-request leak: the same path must come back as the same
    /// pointer.
    #[test]
    fn interning_reuses_one_allocation_per_distinct_path() {
        let a = leak("/api/test/intern");
        let b = leak("/api/test/intern");
        assert_eq!(a.as_ptr(), b.as_ptr());
    }

    fn resolve(method: &str, template: &str) -> Option<Op> {
        op_from_parts(method, template)
    }
}
