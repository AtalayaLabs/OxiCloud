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
//! a prefix scan over 67 declarations — it was ~20 when this was written,
//! and PR 1 tripled it by moving declarations out of the central table and
//! onto their routes, so expect it to keep growing.
//!
//! Precomputing the whole table at boot needs the operation list at boot.
//! That is cheaper than this comment used to claim — `ApiDoc::openapi()`
//! builds the spec IN PROCESS, so no file has to be parsed — but it buys
//! a scan that is still orders of magnitude below the DB round trips on
//! every path it guards. If it shows up in a profile, a cache keyed by
//! `Op` goes here and changes nothing else.
//!
//! What did matter was the LOCK, not the scan: `leak` interned the method
//! and the template through a global `Mutex`, so every gated request
//! serialized on it. A scan costs each request a little; a global lock
//! costs the whole server its concurrency. See `leak` and
//! `method_static`.

use std::sync::LazyLock;

use axum::extract::MatchedPath;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use crate::application::dtos::user_dto::CurrentCaller;
use crate::application::ports::scope_ports::{Access, Op};
use crate::application::services::scope_policy::{Denial, evaluate};
use crate::application::services::scope_table::PolicyRegistry;
use crate::common::di::AppState;
use crate::interfaces::api::policy_declarations::{dav_area, declarations};
use crate::interfaces::errors::AppError;

/// The declarations, built once. Static data, so there is nothing to fail
/// at construction — the declarations' VALIDITY is asserted by
/// `policy_declarations`'s test against the real operation list, which is
/// where a boot refusal would surface.
static INSTALLED: std::sync::OnceLock<PolicyRegistry> = std::sync::OnceLock::new();

/// Install the declarations collected while building the router.
///
/// Called once from `main.rs` with `create_api_routes`'s registry rebased
/// under `/api` and the surface-level declarations absorbed. Before this
/// runs — and in unit tests, which build their own routers — the gate
/// falls back to `declarations()`, the table for operations that have no
/// route to declare against.
pub fn install_registry(registry: PolicyRegistry) {
    if INSTALLED.set(registry).is_err() {
        tracing::warn!(
            target: "oxicloud::authz",
            "policy registry installed twice — the first one stands"
        );
    }
}

fn registry() -> &'static PolicyRegistry {
    static FALLBACK: LazyLock<PolicyRegistry> = LazyLock::new(declarations);
    INSTALLED.get().unwrap_or(&FALLBACK)
}

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
    // A DAV request is identified by AREA and ACCESS, not by surface and
    // method: the method carries one authorization-relevant bit, and the
    // area rather than the protocol is what a scope names. Six `Op`s
    // therefore replace thirty-six.
    //
    // `DAV_AREAS` lists the NATIVE surfaces only. The NextCloud doors
    // reach the same areas but no folded layer covers them, so they
    // resolve as REST and keep the safe default — giving them the DAV
    // declaration would ADD a gate rather than fold one.
    //
    // No pattern matching here — `matched` is the route TEMPLATE string
    // axum hands back (`/webdav/{*path}`), and `dav_area` prefix-matches
    // it. The template is what REST needs, since a declaration is written
    // against `/api/files/{id}` and never against a resolved path.
    if let Some(area) = dav_area(matched) {
        return Some(Op::Dav(area, Access::from_method(method)));
    }
    Some(Op::Rest(method_static(method), leak(matched)))
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
/// The HTTP method as a `&'static str`, WITHOUT touching the interner.
///
/// Methods are a closed set, so a match is both exact and free. This used
/// to go through `leak`, which means every gated request took a global
/// mutex to discover that "GET" is "GET" — on the listing and thumbnail
/// paths, i.e. the ones AGENTS.md names as hot.
fn method_static(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        // WebDAV / CalDAV / CardDAV.
        "PROPFIND" => "PROPFIND",
        "PROPPATCH" => "PROPPATCH",
        "MKCOL" => "MKCOL",
        "COPY" => "COPY",
        "MOVE" => "MOVE",
        "LOCK" => "LOCK",
        "UNLOCK" => "UNLOCK",
        "REPORT" => "REPORT",
        "MKCALENDAR" => "MKCALENDAR",
        // Anything else falls back to the interner. It MUST still produce
        // an `Op`: `op_from_parts` returning `None` makes the gate pass
        // the request THROUGH, so returning `None` for an unrecognised
        // method would let an exotic one skip the gate entirely. An
        // unknown method matches no declaration and so compiles to the
        // default policy, which is what refuses it — the same as before.
        other => leak(other),
    }
}

/// Intern a route TEMPLATE so it can live in an `Op<'static>`.
///
/// Templates are a fixed set fixed at compile time, so after the first
/// request to each route the set never changes again. That makes this a
/// read-mostly structure, and it is an `RwLock` for exactly that reason:
/// a `Mutex` here serialized EVERY gated request behind one lock, which
/// is a throughput ceiling rather than a latency cost — invisible in a
/// single-request measurement and sharply visible under concurrency.
fn leak(s: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::RwLock;
    static INTERNED: LazyLock<RwLock<HashSet<&'static str>>> =
        LazyLock::new(|| RwLock::new(HashSet::new()));

    // The fast path, taken by every request after the first to a given
    // route: a SHARED read, so concurrent requests do not queue.
    if let Some(found) = INTERNED.read().expect("interner poisoned").get(s) {
        return found;
    }

    let mut set = INTERNED.write().expect("interner poisoned");
    // Re-check: another thread may have interned it between the two locks.
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

    let policy = match registry().compile_one(op) {
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
            // The floor that was missed, as a FIELD rather than baked
            // into `reason` — one reason per cause keeps the vocabulary
            // small and leaves the detail queryable.
            required = denial.required_role().map(|r| format!("{r:?}")),
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

    /// An unrecognised method must STILL produce an `Op`.
    ///
    /// Caught while removing the interner's global mutex from the request
    /// path: `method_static` was first written to return `Option` and give
    /// `None` for anything outside the known set, on the reasoning that an
    /// unknown method matches no declaration anyway. It does not fail
    /// closed — `policy_gate_layer` treats `None` as "no route matched"
    /// and runs the handler — so an exotic method would have skipped the
    /// gate entirely. An unknown method must instead compile to the
    /// DEFAULT policy, which is what refuses it.
    #[test]
    fn an_unrecognised_method_still_yields_an_op() {
        let op = op_from_parts("WEIRD", "/api/files/{id}");
        assert!(
            matches!(op, Some(Op::Rest("WEIRD", "/api/files/{id}"))),
            "an unknown method must still produce an Op so the gate \
             applies: {op:?}"
        );

        // And the policy it compiles to must demand a user, not nothing.
        let policy = declarations()
            .compile_one(op.expect("an op"))
            .expect("the default compiles");
        assert_eq!(
            policy.principal,
            crate::application::ports::scope_ports::MinPrincipal::User,
            "an undeclared operation must inherit the default policy"
        );
    }
    use crate::application::ports::scope_ports::{Access, MinRole, Policy, Scope};

    /// A DAV request resolves to its METHOD, not its path: all three
    /// surfaces have one `any` handler, so a path-keyed lookup could not
    /// tell `PROPFIND` from `DELETE`. This is the mapping that makes the
    /// AWARENESS-class bug impossible to repeat.
    #[test]
    fn dav_requests_resolve_to_an_area_and_an_access() {
        for (prefix, method, area, access) in [
            (
                "/webdav/{*path}",
                "PROPFIND",
                Scope::Files,
                Access::ReadOnly,
            ),
            ("/webdav/{*path}", "PUT", Scope::Files, Access::ReadWrite),
            (
                "/caldav/{*path}",
                "REPORT",
                Scope::Calendar,
                Access::ReadOnly,
            ),
            ("/carddav/", "DELETE", Scope::Contacts, Access::ReadWrite),
            // LOCK mutates lock state, so it is a WRITE — the
            // classification most easily got wrong by reading the verb.
            ("/webdav/{*path}", "LOCK", Scope::Files, Access::ReadWrite),
            // An unlisted or future method is a write: fail-closed, so it
            // demands the stronger scope rather than slipping through on
            // a read-only credential.
            (
                "/webdav/{*path}",
                "FUTUREVERB",
                Scope::Files,
                Access::ReadWrite,
            ),
        ] {
            let op = resolve(method, prefix);
            assert_eq!(op, Some(Op::Dav(area, access)), "{prefix} {method}");
        }
    }

    /// The NextCloud DAV doors resolve to the SAME `Op::Dav` as native
    /// `/webdav`, so one declaration covers both protocols.
    ///
    /// This test previously asserted the opposite, and deliberately: PR 1
    /// was a fold, and giving these doors the DAV declaration would have
    /// ADDED a gate they did not have — a behaviour change wearing a
    /// transcription's clothes. The pin existed so a tidy-up could not
    /// make PR 1 do it quietly.
    ///
    /// PR 2 does it openly, because the scope axis is useless otherwise:
    /// `/remote.php/dav/files` is where an app password actually GOES, so
    /// leaving it unmapped would enforce the `files` ceiling on the
    /// browser API and ignore it on the sync surface. That also resolves
    /// the asymmetry `DAV_AREAS` documents — native `/webdav` refused an
    /// external user and the NC mirror did not.
    ///
    /// Mapped by LEAF, not by `/remote.php/dav`: a bare prefix would
    /// capture `/remote.php/dav/avatars`, and an avatar is a profile
    /// image — `account`, not `files`.
    #[test]
    fn the_nextcloud_dav_doors_map_to_the_same_dav_operations() {
        use crate::application::ports::scope_ports::Scope;

        // The files mirror, read and write.
        assert_eq!(
            resolve("PROPFIND", "/remote.php/dav/files/{user}"),
            Some(Op::Dav(Scope::Files, Access::ReadOnly)),
        );
        assert_eq!(
            resolve("GET", "/remote.php/webdav/{*subpath}"),
            Some(Op::Dav(Scope::Files, Access::ReadOnly)),
        );
        assert_eq!(
            resolve("PUT", "/remote.php/dav/files/{user}/{*subpath}"),
            Some(Op::Dav(Scope::Files, Access::ReadWrite)),
        );
        // Trash and chunked uploads are files too.
        assert_eq!(
            resolve("DELETE", "/remote.php/dav/trashbin/{user}/{*subpath}"),
            Some(Op::Dav(Scope::Files, Access::ReadWrite)),
        );
        assert_eq!(
            resolve("PUT", "/remote.php/dav/uploads/{user}/{upload_id}"),
            Some(Op::Dav(Scope::Files, Access::ReadWrite)),
        );

        // And the one that must NOT be files: an avatar is a profile
        // image. A bare `/remote.php/dav` prefix would have swallowed it.
        assert_eq!(
            resolve("GET", "/remote.php/dav/avatars/{user}/{size}"),
            Some(Op::Dav(Scope::Account, Access::ReadOnly)),
        );
    }

    /// The DAV prefix match is SEGMENT-aware, so a lookalike route cannot
    /// inherit a DAV area — the same class of mistake as `/api/admin`
    /// capturing `/api/administrators`. Hardening: no such route exists,
    /// and the alternative is relying on nobody adding one.
    #[test]
    fn a_lookalike_path_does_not_inherit_a_dav_area() {
        assert_eq!(
            resolve("GET", "/webdavious/thing"),
            Some(Op::Rest("GET", "/webdavious/thing")),
            "a lookalike must resolve as REST, not as a DAV area"
        );
        // The real surfaces still resolve, including the bare prefix.
        assert_eq!(
            resolve("GET", "/webdav"),
            Some(Op::Dav(Scope::Files, Access::ReadOnly))
        );
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
        let dav = registry()
            .compile_one(Op::Dav(Scope::Files, Access::ReadOnly))
            .expect("compiles");
        assert_eq!(dav.role, Some(MinRole::Internal));

        // `/api/admin` is declared at its nest in `routes.rs` now, so it
        // is absent from the fallback table a unit test sees. The
        // transfer-ownership declaration below is still here, because a
        // per-METHOD gate cannot be written as `.policy()`.

        // `transfer-ownership` is declared at its route now, so it is not
        // in the fallback table a unit test sees.

        // The visitor declarations live at their routes now, so the
        // fallback table a unit test sees does not hold them.

        let undeclared = registry()
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

    /// **The layer ORDER, pinned.** The gate must run AFTER the middleware
    /// that inserts the `CurrentCaller`, which in tower means it is
    /// `.layer()`ed BEFORE it — later layers are outermost and run first.
    ///
    /// Nothing in the type system enforces that: a `Layer` composes with
    /// any other `Layer`, so a reordering compiles. What it does do is
    /// fail LOUDLY rather than silently — a gate running before auth sees
    /// no caller on every request and answers 401 to everyone, including
    /// callers who presented a valid credential. So the failure is
    /// unmissable in production; this test makes it unmissable in `cargo
    /// test` instead.
    ///
    /// Both orders are asserted, because a test of only the right one
    /// would still pass if the gate stopped reading the caller at all.
    #[tokio::test]
    async fn the_gate_must_be_layered_before_the_caller_is_inserted() {
        use axum::body::Body;
        use axum::{Router, routing::get};
        use tower::ServiceExt;

        async fn ok() -> &'static str {
            "reached the handler"
        }

        // Stands in for `auth_middleware`: inserts a caller.
        async fn insert_caller(mut request: Request, next: Next) -> Response {
            request.extensions_mut().insert(Arc::new(CurrentCaller {
                id: uuid::Uuid::new_v4(),
                username: Arc::from("alice"),
                email: Arc::from("alice@example.com"),
                kind: crate::domain::entities::user::CallerKind::Internal(
                    crate::domain::entities::user::UserRole::User,
                ),
                dpop_jkt: None,
                token: Default::default(),
            }));
            next.run(request).await
        }

        // `/api/admin/users` is declared `MinRole::Admin`, so a plain user
        // is refused on the merits — use an UNDECLARED path, whose policy
        // is the default `principal=User`. Then the only thing that can
        // refuse is a missing caller.
        let gate = axum::middleware::from_fn(|req: Request, next: Next| async move {
            // `State` is unused by the gate, so call the inner logic
            // directly rather than building an `AppState`.
            gate_without_state(req, next).await
        });

        // RIGHT: caller inserted outermost (layered last) → runs first.
        let right: Router<()> = Router::new()
            .route("/api/anything", get(ok))
            .route_layer(gate.clone())
            .layer(axum::middleware::from_fn(insert_caller));

        let res = right
            .oneshot(
                Request::builder()
                    .uri("/api/anything")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "with auth outermost the gate sees the caller"
        );

        // WRONG: the gate outermost, so it runs BEFORE the caller exists.
        let wrong: Router<()> = Router::new()
            .route("/api/anything", get(ok))
            .route_layer(axum::middleware::from_fn(insert_caller))
            .layer(gate);

        let res = wrong
            .oneshot(
                Request::builder()
                    .uri("/api/anything")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "a gate running before auth must refuse everyone — which is \
             how a misordering announces itself rather than hiding"
        );
    }

    /// **`MatchedPath` includes NEST prefixes, and every declaration
    /// depends on it.** `/api/admin` is declared as a prefix while the
    /// admin router is nested at `/admin` inside one nested at `/api`, so
    /// if `MatchedPath` reported only `/users` the lookup would miss and
    /// every admin route would silently fall back to the default policy.
    ///
    /// In shadow that is masked — `require_admin` still refuses — which
    /// is exactly why it needs a test rather than a matrix run: the gate
    /// would be doing nothing and everything would still be green.
    ///
    /// A plain user against a nested `/api/admin/users` must be REFUSED.
    /// If the prefix were lost, the policy would be the default
    /// (`principal = User`) and this would pass with 200.
    #[tokio::test]
    async fn the_matched_path_carries_nest_prefixes() {
        use axum::body::Body;
        use axum::{Router, routing::get};
        use tower::ServiceExt;

        async fn ok() -> &'static str {
            "reached the handler"
        }

        async fn insert_plain_user(mut request: Request, next: Next) -> Response {
            request.extensions_mut().insert(Arc::new(CurrentCaller {
                id: uuid::Uuid::new_v4(),
                username: Arc::from("plain"),
                email: Arc::from("plain@example.com"),
                kind: crate::domain::entities::user::CallerKind::Internal(
                    crate::domain::entities::user::UserRole::User,
                ),
                dpop_jkt: None,
                token: Default::default(),
            }));
            next.run(request).await
        }

        // A LOCAL registry, because nothing with a REST path is declared
        // centrally any more — the fallback table holds only the DAV
        // operations. That keeps this a test of PREFIX HANDLING rather
        // than of production wiring, which is what it was always for.
        let mut local = crate::application::services::scope_table::PolicyRegistry::new();
        local.declare(
            crate::application::services::scope_table::Site::RestPrefix("/api/admin"),
            vec![crate::application::ports::scope_ports::Condition::role(
                MinRole::Admin,
            )],
        );
        let local = Arc::new(local);

        let admin: Router<()> =
            Router::new()
                .route("/sessions", get(ok))
                .route_layer(axum::middleware::from_fn(
                    move |request: Request, next: Next| {
                        let registry = Arc::clone(&local);
                        async move {
                            let Some(op) = op_for(&request) else {
                                return next.run(request).await;
                            };
                            let policy = registry.compile_one(op).expect("compiles");
                            let kind = request
                                .extensions()
                                .get::<Arc<CurrentCaller>>()
                                .map(|c| c.kind);
                            match evaluate(&policy, kind, None) {
                                Ok(()) => next.run(request).await,
                                Err(d) => deny(d),
                            }
                        }
                    },
                ));
        let api: Router<()> = Router::new().nest("/admin", admin);
        let app: Router<()> = Router::new()
            .nest("/api", api)
            .layer(axum::middleware::from_fn(insert_plain_user));

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/admin/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            res.status(),
            StatusCode::FORBIDDEN,
            "a plain user must be refused at /api/admin/sessions — a 200 \
             here means the nest prefixes were lost and the `/api/admin` \
             declaration never matched"
        );
    }

    /// The gate's logic without the `State` it never reads, so a test can
    /// compose it without building an `AppState`.
    async fn gate_without_state(request: Request, next: Next) -> Response {
        let Some(op) = op_for(&request) else {
            return next.run(request).await;
        };
        let policy = registry().compile_one(op).expect("declarations compile");
        let kind = request
            .extensions()
            .get::<Arc<CurrentCaller>>()
            .map(|c| c.kind);
        match evaluate(&policy, kind, None) {
            Ok(()) => next.run(request).await,
            Err(d) => deny(d),
        }
    }
}

#[cfg(test)]
mod nest_layer_semantics {
    //! Does nesting two routers at the SAME prefix cross-apply their
    //! layers? `main.rs` mounts three routers at `/api` — setup, public
    //! and protected — and a comment there says distinct sub-prefixes are
    //! used "so axum doesn't cross-apply middleware between the two
    //! branches".
    //!
    //! If that is true of `nest`, adding the policy gate to the protected
    //! router would gate `/api/auth/login` too and refuse every login. The
    //! answer decides whether `public()` declarations must land first, so
    //! it is tested rather than reasoned about.

    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::middleware::Next;
    use axum::response::Response;
    use axum::{Router, routing::get};
    use tower::ServiceExt;

    async fn ok() -> &'static str {
        "ok"
    }

    async fn refuse_everything(_request: Request, _next: Next) -> Response {
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn a_layer_on_one_nested_router_does_not_reach_a_sibling() {
        let gated: Router<()> = Router::new()
            .route("/protected", get(ok))
            .layer(axum::middleware::from_fn(refuse_everything));
        let open: Router<()> = Router::new().route("/login", get(ok));

        let app: Router<()> = Router::new().nest("/api", gated).nest("/api", open);

        let gated_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            gated_res.status(),
            StatusCode::FORBIDDEN,
            "its own layer applies"
        );

        let open_res = app
            .oneshot(
                Request::builder()
                    .uri("/api/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            open_res.status(),
            StatusCode::OK,
            "a sibling nested at the same prefix must NOT inherit the layer"
        );
    }
}
