//! `.policy([...])` — declaring a policy at the handler or the subtree.
//!
//! PR 1 step 3b-ii of `docs/plan/scope-gates.md`, and inert: `routes.rs`
//! does not use this yet, so nothing is declared and nothing is enforced.
//!
//! The syntax is the one worked out in
//! `docs/architecture/authorization-axes.md` § How the axes compose:
//!
//! ```ignore
//! // the whole subtree
//! let admin = admin_routes(state).policy([Condition::role(MinRole::Admin)]);
//!
//! // one route — the public exceptions, each greppable
//! .route("/login", post(login).policy([Condition::public()]))
//! ```
//!
//! # Why `.policy()` cannot record the path itself
//!
//! Neither a `Router` nor a `MethodRouter` knows where it will be
//! mounted, and `axum::Router`'s entire public introspection surface is
//! `has_routes() -> bool` — there is no way to enumerate routes or read a
//! prefix. So `.policy()` can only ATTACH conditions; the [`Site`] is
//! learned when the value reaches [`GatedRouter::nest`] or
//! [`GatedRouter::route`], where the caller is already writing the path
//! literal. The path therefore still appears exactly once.
//!
//! What no amount of API design can catch is a declaration for a path
//! that no longer has routes, since axum will not say which paths exist.
//! That is what § 2b's "a policy naming no live operation" is for, and
//! why that refusal is load-bearing rather than belt-and-braces.

use axum::Router;
use axum::routing::MethodRouter;

use crate::application::ports::scope_ports::{Condition, Op};
use crate::application::services::scope_table::{PolicyRegistry, Site};

/// A router or handler carrying its declared conditions, waiting for the
/// path that will name its [`Site`].
pub struct Gated<T> {
    inner: T,
    conditions: Vec<Condition>,
}

/// One HTTP METHOD on one path, with its policy.
///
/// Exists because axum's `get(handler)` returns a `MethodRouter` that has
/// FORGOTTEN it is a GET — and `.policy()` on that could only declare for
/// the whole path. A path-level declaration is how
/// `visitor_allowed()` on `/api/files/{id}` silently granted a share
/// visitor `DELETE`: three of the seven visitor-reachable paths also
/// serve a write.
///
/// So the verb constructors here remember their verb:
///
/// ```ignore
/// .route("/{id}", get(download).policy([Condition::visitor_allowed()]))
/// ```
///
/// A bare `MethodRouter` still works and declares nothing, which is the
/// common case — the default policy already demands a session.
pub struct Verb<S> {
    /// `None` for a bare `MethodRouter`: no verb was named, so a
    /// declaration (if any) covers the whole path.
    method: Option<&'static str>,
    inner: MethodRouter<S>,
    conditions: Vec<Condition>,
}

impl<S> Verb<S> {
    /// Attach the policy for THIS verb.
    #[must_use]
    pub fn policy<const N: usize>(mut self, conditions: [Condition; N]) -> Self {
        self.conditions = conditions.to_vec();
        self
    }
}

/// A bare `MethodRouter` is a `Verb` with no verb named and no policy.
impl<S> From<MethodRouter<S>> for Verb<S> {
    fn from(inner: MethodRouter<S>) -> Self {
        Verb {
            method: None,
            inner,
            conditions: Vec::new(),
        }
    }
}

macro_rules! verb {
    ($name:ident, $axum:ident, $method:literal) => {
        /// Like `axum::routing::
        #[doc = $method]
        /// `, but remembers its verb so `.policy([...])` declares for
        /// that method alone rather than for the whole path.
        pub fn $name<H, T, S>(handler: H) -> Verb<S>
        where
            H: axum::handler::Handler<T, S>,
            T: 'static,
            S: Clone + Send + Sync + 'static,
        {
            Verb {
                method: Some($method),
                inner: axum::routing::$axum(handler),
                conditions: Vec::new(),
            }
        }
    };
}

verb!(get, get, "GET");
verb!(post, post, "POST");
verb!(put, put, "PUT");
verb!(delete, delete, "DELETE");
verb!(patch, patch, "PATCH");

/// `.policy([...])` on a `Router` (the whole subtree) or a `MethodRouter`
/// (one route).
///
/// Deliberately NOT implemented on [`Gated<T>`], which makes
/// `.policy(..).policy(..)` a COMPILE error rather than § 2b's boot
/// error. Two declarations on one site are ambiguous — repeated builder
/// calls conventionally override, so accumulating would read as the
/// second replacing the first — and a contradiction the compiler refuses
/// beats one the server refuses at boot.
///
/// Conditions that belong together go in one slice, which is what the
/// array literal is for:
/// `.policy([role(Admin), scope(Security, ReadWrite)])`.
pub trait Policied: Sized {
    /// Attach a declaration. Takes an array so the call site reads as a
    /// slice of conditions rather than a borrow — allocated once at
    /// startup, never per request.
    fn policy<const N: usize>(self, conditions: [Condition; N]) -> Gated<Self> {
        Gated {
            inner: self,
            conditions: conditions.to_vec(),
        }
    }
}

impl<S> Policied for Router<S> where S: Clone + Send + Sync + 'static {}
// NOT implemented for `MethodRouter`: `.policy()` on one could only
// declare for the whole path, which is the bug that granted a visitor
// `DELETE /api/files/{id}`. Use the verb constructors above.

/// A `Router` and the declarations made against it, built together.
///
/// The registry is an explicit value rather than a process-global, so the
/// whole mechanism is testable without booting a server.
pub struct GatedRouter<S> {
    router: Router<S>,
    registry: PolicyRegistry,
}

impl<S> Default for GatedRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S> GatedRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            registry: PolicyRegistry::new(),
        }
    }

    /// Wrap an existing router, so a surface can be migrated one nest at
    /// a time rather than in one commit — which matters because
    /// `routes.rs` carries 279 operations across many nests.
    pub fn from_router(router: Router<S>) -> Self {
        Self {
            router,
            registry: PolicyRegistry::new(),
        }
    }

    /// `Router::nest`, recording the subtree's declaration if it carries
    /// one. One declaration covers every operation beneath `path`.
    ///
    /// Takes `impl Into<Gated<Router<S>>>`, so a bare `Router` nests with
    /// no declaration and its children inherit whatever is above. That is
    /// the deliberate consequence of `Policy::default()` being an
    /// authenticated user: an undeclared subtree is already closed to
    /// anonymous callers, so requiring a declaration everywhere would be
    /// busywork rather than safety.
    #[must_use]
    pub fn nest(mut self, path: &'static str, inner: impl Into<Gated<Router<S>>>) -> Self {
        let Gated { inner, conditions } = inner.into();
        if !conditions.is_empty() {
            self.registry.declare(Site::RestPrefix(path), conditions);
        }
        self.router = self.router.nest(path, inner);
        self
    }

    /// `Router::route`, recording the handler's declaration if it carries
    /// one.
    ///
    /// Note the granularity: a declaration covers every METHOD on the
    /// path, because a `MethodRouter` cannot be asked which methods it
    /// holds either. An operation needing a per-method policy declares it
    /// with [`Self::declare_op`].
    #[must_use]
    pub fn route(mut self, path: &'static str, verb: impl Into<Verb<S>>) -> Self {
        let Verb {
            method,
            inner,
            conditions,
        } = verb.into();
        if !conditions.is_empty() {
            // A named verb declares for that METHOD; an unnamed one (a
            // bare `MethodRouter`, or `.policy()` on one) covers the
            // whole path, which is only right when every method on it
            // shares the policy.
            let site = match method {
                Some(m) => Site::Exact(Op::Rest(m, path)),
                None => Site::RestPrefix(path),
            };
            self.registry.declare(site, conditions);
        }
        self.router = self.router.route(path, inner);
        self
    }

    /// `Router::merge`, absorbing the other side's declarations.
    ///
    /// Needed because `/{id}`'s GET and DELETE live in SEPARATE routers
    /// that are merged — so a per-verb declaration written in one must
    /// survive the merge.
    #[must_use]
    pub fn merge(mut self, other: GatedRouter<S>) -> Self {
        let (router, registry) = other.finish();
        self.registry.absorb(registry);
        self.router = self.router.merge(router);
        self
    }

    /// `Router::nest` for a router that carries declarations of its own,
    /// REBASING them under `path`.
    ///
    /// This is what lets `.policy()` be written at a route inside a
    /// router that does not know where it will be mounted.
    #[must_use]
    pub fn nest_gated(mut self, path: &'static str, inner: GatedRouter<S>) -> Self {
        let (router, registry) = inner.finish();
        self.registry.absorb(registry.rebased_under(path));
        self.router = self.router.nest(path, router);
        self
    }

    /// Declare for one exact operation, with no router involvement.
    ///
    /// The only form available to DAV, WS and collab frames: each has a
    /// single `any` handler, so there is no route to attach to and the
    /// operation is chosen by dispatch inside the handler. This is where
    /// the AWARENESS frame kind and the CardDAV door to
    /// `create_address_book` would have been gated.
    #[must_use]
    pub fn declare_op<const N: usize>(mut self, op: Op, conditions: [Condition; N]) -> Self {
        self.registry.declare(Site::Exact(op), conditions.to_vec());
        self
    }

    /// `Router::layer`, passed straight through: a layer is not a policy,
    /// and the two coexist on purpose — integrity checks (CSRF, DPoP) and
    /// scope narrowings stay layers because no operation has an opinion
    /// about them (§ Not axes).
    #[must_use]
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<axum::extract::Request> + Clone + Send + Sync + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Response:
            axum::response::IntoResponse + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Error:
            Into<std::convert::Infallible> + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Future: Send + 'static,
    {
        self.router = self.router.layer(layer);
        self
    }

    /// `Router::with_state`, carrying the declarations across the state
    /// change. The sub-routers in `routes.rs` each supply their own
    /// service state, so without this a declaration written at a route
    /// could not survive to the nest that names its path.
    #[must_use]
    pub fn with_state<S2>(self, state: S) -> GatedRouter<S2>
    where
        S2: Clone + Send + Sync + 'static,
    {
        GatedRouter {
            router: self.router.with_state(state),
            registry: self.registry,
        }
    }

    /// The router to serve, and the declarations to compile.
    ///
    /// Returns both rather than compiling here: compilation needs the
    /// authoritative operation list from the specs, which this type has
    /// no business loading.
    pub fn finish(self) -> (Router<S>, PolicyRegistry) {
        (self.router, self.registry)
    }
}

/// An undeclared router or handler is `Gated` with no conditions, so
/// `nest`/`route` take both shapes and the common case stays plain.
impl<T> From<T> for Gated<T> {
    fn from(inner: T) -> Self {
        Gated {
            inner,
            conditions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::scope_ports::{Access, MinPrincipal, MinRole, Policy, Scope};

    async fn noop() -> &'static str {
        "ok"
    }

    /// The doc's `/api/admin` example: one condition on the subtree,
    /// inherited by every operation beneath, path written once.
    #[test]
    fn a_subtree_declaration_is_inherited_by_everything_beneath() {
        let admin: Router<()> = Router::new()
            .route("/sessions", axum::routing::get(noop))
            .route("/settings/oidc", axum::routing::get(noop));

        let (_router, registry) = GatedRouter::<()>::new()
            .nest(
                "/api/admin",
                admin.policy([Condition::role(MinRole::Admin)]),
            )
            .finish();

        let ops = [
            Op::Rest("GET", "/api/admin/sessions"),
            Op::Rest("GET", "/api/admin/settings/oidc"),
        ];
        let table = registry.compile_all(&ops).expect("compiles");

        for op in ops {
            assert_eq!(table.get(&op).unwrap().role, Some(MinRole::Admin), "{op}");
        }
    }

    /// The doc's `/api/auth` example: public exceptions declared per
    /// route, each one greppable.
    #[test]
    fn a_handler_declaration_marks_one_route_public() {
        let (_router, registry) = GatedRouter::<()>::new()
            .route(
                "/api/auth/login",
                self::post(noop).policy([Condition::public()]),
            )
            .route("/api/auth/me", axum::routing::get(noop))
            .finish();

        let login = Op::Rest("POST", "/api/auth/login");
        let me = Op::Rest("GET", "/api/auth/me");
        let table = registry.compile_all(&[login, me]).expect("compiles");

        assert_eq!(
            table.get(&login).unwrap().principal,
            crate::application::ports::scope_ports::MinPrincipal::Public
        );
        assert_eq!(
            table.get(&me),
            Some(&Policy::default()),
            "an undeclared route keeps the safe default"
        );
    }

    /// A bare router or handler needs no declaration, which is the
    /// deliberate consequence of `Policy::default()` being an
    /// authenticated user — an undeclared operation is already closed to
    /// anonymous callers.
    #[test]
    fn undeclared_routers_and_handlers_need_no_policy_call() {
        let plain: Router<()> = Router::new().route("/thing", axum::routing::get(noop));

        let (_r, registry) = GatedRouter::<()>::new()
            .nest("/api/open", plain)
            .route("/api/bare", axum::routing::get(noop))
            .finish();

        for op in [
            Op::Rest("GET", "/api/open/thing"),
            Op::Rest("GET", "/api/bare"),
        ] {
            let table = registry.compile_all(&[op]).expect("compiles");
            assert_eq!(table.get(&op), Some(&Policy::default()), "{op}");
        }
    }

    /// Several conditions go in ONE slice — the array literal is what
    /// makes that the natural way to write it, and `.policy().policy()`
    /// does not compile, so there is no second way.
    #[test]
    fn several_conditions_belong_in_one_slice() {
        let (_r, registry) = GatedRouter::<()>::new()
            .route(
                "/api/auth/password",
                self::post(noop).policy([
                    Condition::role(MinRole::Internal),
                    Condition::scope(Scope::Security, Access::ReadWrite),
                ]),
            )
            .finish();

        let op = Op::Rest("POST", "/api/auth/password");
        let table = registry.compile_all(&[op]).expect("compiles");
        let p = table.get(&op).unwrap();

        assert_eq!(p.role, Some(MinRole::Internal));
        assert_eq!(p.scope, Some((Scope::Security, Access::ReadWrite)));
    }

    /// Non-REST surfaces declare by operation, since they have no route
    /// to attach to.
    #[test]
    fn non_rest_surfaces_declare_by_operation() {
        let (_r, registry) = GatedRouter::<()>::new()
            .declare_op(Op::CollabFrame(0x02), [Condition::role(MinRole::Internal)])
            .declare_op(
                Op::Dav(Scope::Files, Access::ReadOnly),
                [Condition::role(MinRole::Internal)],
            )
            .finish();

        let ops = [
            Op::CollabFrame(0x02),
            Op::Dav(Scope::Files, Access::ReadOnly),
        ];
        let table = registry.compile_all(&ops).expect("compiles");

        for op in ops {
            assert_eq!(
                table.get(&op).unwrap().role,
                Some(MinRole::Internal),
                "{op}"
            );
        }
    }

    /// A subtree saying admin and a leaf saying public is unsatisfiable,
    /// and the builder changes nothing about that — the contradiction is
    /// caught at compile, not resolved to an unreachable route.
    #[test]
    fn the_builder_adds_no_semantics_of_its_own() {
        // The nest holds a different path, because axum rejects an
        // overlapping route — so the contradicting leaf is declared
        // alongside it rather than inside it.
        let admin: Router<()> = Router::new().route("/sessions", axum::routing::get(noop));

        let (_r, registry) = GatedRouter::<()>::new()
            .nest(
                "/api/admin",
                admin.policy([Condition::role(MinRole::Admin)]),
            )
            .route(
                "/api/admin/oops",
                self::post(noop).policy([Condition::public()]),
            )
            .finish();

        let errs = registry
            .compile_all(&[Op::Rest("POST", "/api/admin/oops")])
            .expect_err("admin + public cannot both hold");

        assert!(errs.iter().any(|e| matches!(
            e,
            crate::application::services::scope_policy::PolicyError::PublicWithRole { .. }
        )));
    }

    /// **The shape `/api/files/{id}` actually has**, and the reason
    /// per-verb declarations exist.
    ///
    /// Its GET and its DELETE live in SEPARATE routers that are merged,
    /// then nested at `/files`, then rebased under `/api`. So a
    /// declaration written as `get(h).policy(..)` has to survive a merge
    /// AND two levels of rebasing and still name `GET` alone — because
    /// the allowlist is GET-only and the DELETE must keep the safe
    /// default. A path-level declaration here is what granted a visitor
    /// `DELETE /api/files/{id}`.
    #[test]
    fn the_visitor_declaration_does_not_leak_up_down_or_sideways() {
        let reads: GatedRouter<()> = GatedRouter::new()
            .route(
                "/{id}",
                self::get(noop).policy([Condition::visitor_allowed()]),
            )
            // The COLLECTION, deliberately undeclared: listing is not a
            // share-visitor operation.
            .route("/", axum::routing::get(noop))
            // A subpath, to prove an exact declaration does not spread
            // downward the way a prefix would.
            .route("/{id}/versions", axum::routing::get(noop));
        let writes: GatedRouter<()> =
            GatedRouter::new().route("/{id}", axum::routing::delete(noop));

        let files = reads.merge(writes);
        let api = GatedRouter::<()>::new().nest_gated("/files", files);
        let (_router, registry) = api.finish();
        let registry = registry.rebased_under("/api");

        let ops = [
            Op::Rest("GET", "/api/files/{id}"),
            Op::Rest("DELETE", "/api/files/{id}"),
            Op::Rest("GET", "/api/files"),
            Op::Rest("GET", "/api/files/{id}/versions"),
        ];
        let table = registry.compile_all(&ops).expect("compiles");

        // 1. the declared GET admits a visitor, after merge + nest + rebase
        assert_eq!(
            table.get(&ops[0]).unwrap().principal,
            MinPrincipal::Visitor,
            "the GET declaration must survive merge + nest + rebase"
        );

        // 2. the DELETE on the SAME path does NOT. This is the assertion
        //    a path-level declaration fails, and it is how a visitor was
        //    nearly handed `DELETE /api/files/{id}`.
        assert_eq!(
            table.get(&ops[1]),
            Some(&Policy::default()),
            "DELETE on the same path must keep the safe default"
        );

        // 3. the COLLECTION does not inherit it. A visitor holds a key to
        //    one share, not a listing of everything.
        assert_eq!(
            table.get(&ops[2]),
            Some(&Policy::default()),
            "/api/files must NOT be visitor-allowed — the declaration is \
             on the item, and must not leak up to its collection"
        );

        // 4. nor does a SUBPATH of the declared one. An exact declaration
        //    names one operation; a prefix would have swallowed this.
        assert_eq!(
            table.get(&ops[3]),
            Some(&Policy::default()),
            "a SUBPATH must NOT inherit the item's declaration — exact \
             means exact, where a prefix would have swallowed it"
        );
    }

    /// Two `route()` calls on ONE path with DIFFERENT verbs merge, which
    /// is what lets a chained `get(h).put(h2)` be split so each verb
    /// carries its own policy. axum panics only on a duplicate SAME
    /// method, so this is the difference between "overlapping" and
    /// "complementary" — asserted because `routes.rs` relies on it and a
    /// panic would land at router construction, which no unit test
    /// reaches.
    #[tokio::test]
    async fn two_routes_on_one_path_with_different_verbs_merge() {
        use axum::body::Body;
        use axum::extract::Request;
        use axum::http::StatusCode;
        use tower::ServiceExt;

        let (router, registry) = GatedRouter::<()>::new()
            .route(
                "/{id}",
                self::get(noop).policy([Condition::visitor_allowed()]),
            )
            .route("/{id}", axum::routing::put(noop))
            .finish();

        // Both verbs answer.
        for method in ["GET", "PUT"] {
            let res = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/x")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{method} must route");
        }

        // And only the GET is declared.
        let ops = [Op::Rest("GET", "/{id}"), Op::Rest("PUT", "/{id}")];
        let table = registry.compile_all(&ops).expect("compiles");
        assert_eq!(table.get(&ops[0]).unwrap().principal, MinPrincipal::Visitor);
        assert_eq!(table.get(&ops[1]), Some(&Policy::default()));
    }

    /// § 2b's "a policy naming no live operation". The only check that
    /// would notice a declaration for a renamed or typo'd path, because
    /// axum cannot be asked which paths exist.
    #[test]
    fn a_declaration_matching_no_operation_is_refused() {
        let (_r, registry) = GatedRouter::<()>::new()
            .route(
                "/api/admni/typo",
                get(noop).policy([Condition::role(MinRole::Admin)]),
            )
            .finish();

        let errs = registry
            .compile_all(&[Op::Rest("GET", "/api/admin/correct")])
            .expect_err("the declared path matches no live operation");

        assert!(errs.iter().any(|e| matches!(
            e,
            crate::application::services::scope_policy::PolicyError::OrphanedDeclaration { .. }
        )));
    }
}
