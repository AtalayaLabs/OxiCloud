//! The operation TABLE — declarations collected, chains resolved, every
//! operation compiled once at startup, and the result RENDERABLE.
//!
//! PR 1 step 3b-i of `docs/plan/scope-gates.md`, and inert: no route
//! declares anything yet, so every table built here is one a test built.
//!
//! **The renderer is the point, not a convenience.** PR 1's acceptance is
//! "the compiled table before and after the fold must be IDENTICAL", and
//! that is unprovable without a deterministic dump. The snapshot has to be
//! taken while the gates still live in middleware and handlers, so this
//! lands BEFORE anything is folded. Without it PR 1 degrades to "the suite
//! is still green", which is the hoped-for-not-proven outcome the
//! acceptance row exists to rule out.
//!
//! Declarations are recorded against a [`Site`] rather than an operation,
//! because that is how an implementor writes them: a nest says
//! `/api/admin` is admin-only once, and every operation beneath inherits.
//! Resolving site → operation chain is this module's job.

use std::collections::BTreeMap;

use crate::application::ports::scope_ports::{Condition, Op, Policy, ScopeReq};
use crate::application::services::scope_policy::{PolicyError, compile};

/// Where a declaration was written.
///
/// Not an [`Op`]: a declaration usually covers MANY operations (an
/// `/api/admin` nest covers every route beneath it), and flattening that
/// at the declaration site is what the compile step exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Site {
    /// A REST path prefix — an axum nest or a single route template.
    /// Matched by SEGMENT, so `/api/admin` covers `/api/admin/sessions`
    /// and pointedly not `/api/administrators`.
    RestPrefix(&'static str),
    /// One exact operation. The only form available to DAV, WS and collab
    /// frames, which have no path to prefix.
    Exact(Op),
}

/// A compiled table: one total [`Policy`] per operation.
///
/// `BTreeMap`, not `HashMap` — the rendering has to be byte-stable across
/// runs for the snapshot to mean anything, and `Op` derives `Ord` for
/// exactly that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyTable(BTreeMap<Op, Policy>);

impl PolicyTable {
    pub fn get(&self, op: &Op) -> Option<&Policy> {
        self.0.get(op)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn ops(&self) -> impl Iterator<Item = &Op> {
        self.0.keys()
    }

    /// The DUMP. One line per operation, sorted, every axis spelled out
    /// including the ones that resolved to a default — a table that
    /// omitted defaults would hide exactly the change step 4 must not
    /// make.
    ///
    /// Stable by construction: `BTreeMap` order, fixed field order, no
    /// `Debug` of a type whose representation could change under us.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (op, p) in &self.0 {
            let role = match p.role {
                Some(r) => format!("{r:?}"),
                None => "-".to_string(),
            };
            // `?` for UNDEFINED and `-` for a declared none: the dump is
            // read to find what is not classified, so the two must not
            // look alike.
            let scope = match p.scope {
                // Every requirement, joined — an operation can require
                // more than one area now that a leaf appends rather than
                // replaces.
                ScopeReq::Areas(set) => set
                    .iter()
                    .map(|(s, a)| format!("{s:?}:{a:?}"))
                    .collect::<Vec<_>>()
                    .join("+"),
                ScopeReq::None => "-".to_string(),
                ScopeReq::Undefined => "?".to_string(),
            };
            out.push_str(&format!(
                "{op}\tprincipal={:?}\trole={role}\tscope={scope}\n",
                p.principal
            ));
        }
        out
    }
}

/// Collected declarations, before compilation.
#[derive(Debug, Default)]
pub struct PolicyRegistry {
    decls: Vec<(Site, Vec<Condition>)>,
}

impl PolicyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a declaration. Two declarations on the SAME site are
    /// refused at compile time, not here, so every duplicate is reported
    /// together with everything else rather than aborting on the first.
    pub fn declare(&mut self, site: Site, conditions: impl Into<Vec<Condition>>) -> &mut Self {
        self.decls.push((site, conditions.into()));
        self
    }

    /// Re-root every REST declaration under `prefix`.
    ///
    /// This is what lets `.policy()` be written at a route inside a
    /// router that does not know where it will be mounted: the inner
    /// router records `/` or `/{id}`, and the `nest` that mounts it
    /// rebases those to `/api/playlists` and `/api/playlists/{id}`.
    /// Without it, `.policy()` only works at top level, which is where
    /// almost no route lives.
    ///
    /// A `Site::Exact(Op::Rest(..))` IS rebased — that is how a
    /// per-verb declaration written at a route (`get(h).policy(..)`)
    /// reaches its full path. `Op::Dav`, `Op::Ws` and `Op::CollabFrame`
    /// are left alone: they have no path to prepend to.
    ///
    /// Paths are interned rather than leaked per call: this runs once per
    /// nest at startup, and `Op`/`Site` hold `&'static str` so the table
    /// allocates nothing afterwards.
    pub fn rebased_under(self, prefix: &'static str) -> Self {
        let prefix = prefix.trim_end_matches('/');
        let decls = self
            .decls
            .into_iter()
            .map(|(site, conds)| match site {
                Site::RestPrefix(p) => {
                    let joined = if p == "/" {
                        prefix.to_owned()
                    } else {
                        format!("{prefix}{p}")
                    };
                    (Site::RestPrefix(intern(&joined)), conds)
                }
                Site::Exact(Op::Rest(method, p)) => {
                    let joined = if p == "/" {
                        prefix.to_owned()
                    } else {
                        format!("{prefix}{p}")
                    };
                    (Site::Exact(Op::Rest(method, intern(&joined))), conds)
                }
                Site::Exact(_) => (site, conds),
            })
            .collect();
        Self { decls }
    }

    /// Absorb another registry's declarations, unchanged.
    pub fn absorb(&mut self, other: Self) -> &mut Self {
        self.decls.extend(other.decls);
        self
    }

    /// The chain for `op`, ordered ROOT first — which is what
    /// [`compile`] requires, and what makes "a child may tighten, not
    /// widen" meaningful.
    ///
    /// Shorter prefix = closer to the root, so sorting matching prefixes
    /// by length gives the nesting order without having to model the
    /// nesting itself.
    fn chain_for(&self, op: Op) -> Vec<&[Condition]> {
        let mut matched: Vec<(usize, &[Condition])> = Vec::new();

        for (site, conds) in &self.decls {
            match (site, op) {
                (Site::RestPrefix(prefix), Op::Rest(_, path)) => {
                    if path_covered_by(path, prefix) {
                        matched.push((prefix.len(), conds.as_slice()));
                    }
                }
                // A non-REST op has no path, so only an exact
                // declaration reaches it. If DAV ever needs a shared
                // base, that is a new `Site` variant and a new match
                // arm — it cannot change how existing sites resolve.
                (Site::Exact(declared), _) if *declared == op => {
                    matched.push((usize::MAX, conds.as_slice()));
                }
                _ => {}
            }
        }

        matched.sort_by_key(|(depth, _)| *depth);
        matched.into_iter().map(|(_, c)| c).collect()
    }

    /// Compile ONE operation's chain.
    ///
    /// Used on the request path, where only one operation matters. The
    /// whole-table validation — boot refusals and orphans — is
    /// `compile_all`, run against the spec's operation list.
    pub fn compile_one(&self, op: Op) -> Result<Policy, PolicyError> {
        let chain = self.chain_for(op);
        compile(op, &chain)
    }

    /// Compile every operation in `ops`.
    ///
    /// Reports ALL errors rather than the first: a boot failure that
    /// names one of nine bad declarations costs nine restarts to clear.
    ///
    /// `ops` is supplied rather than derived from the declarations,
    /// because the authoritative list is the specs — 279 REST operations
    /// in `openapi.json`, the `rt.*` methods in `asyncapi.json`. That is
    /// also what lets the cross-check run in both directions, which is
    /// step 3b-ii.
    pub fn compile_all(&self, ops: &[Op]) -> Result<PolicyTable, Vec<PolicyError>> {
        let mut errors = self.duplicate_sites();
        errors.extend(self.orphaned_sites(ops).into_iter().map(|site| {
            PolicyError::OrphanedDeclaration {
                site: format!("{site:?}"),
            }
        }));
        let mut table = BTreeMap::new();

        for &op in ops {
            let chain = self.chain_for(op);
            match compile(op, &chain) {
                Ok(policy) => {
                    table.insert(op, policy);
                }
                Err(e) => errors.push(e),
            }
        }

        if errors.is_empty() {
            Ok(PolicyTable(table))
        } else {
            Err(errors)
        }
    }

    /// Sites that matched NO operation in `ops` — § 2b's "a policy naming
    /// no live operation".
    ///
    /// This refusal is load-bearing rather than belt-and-braces, because
    /// `axum::Router` cannot be asked which paths it holds
    /// (`has_routes() -> bool` is the whole introspection surface). So a
    /// declaration for `/api/admni` — or for a path that was renamed last
    /// month — is otherwise indistinguishable from one that applies, and
    /// it silently stops gating anything. Nothing else in the design
    /// catches it.
    ///
    /// Reported against the SPEC's operation list, which is why
    /// `compile_all` takes one rather than deriving it from declarations.
    pub fn orphaned_sites(&self, ops: &[Op]) -> Vec<Site> {
        self.decls
            .iter()
            .map(|(site, _)| *site)
            .filter(|site| !ops.iter().any(|op| self.site_covers(*site, *op)))
            .collect()
    }

    fn site_covers(&self, site: Site, op: Op) -> bool {
        match (site, op) {
            (Site::RestPrefix(prefix), Op::Rest(_, path)) => path_covered_by(path, prefix),
            (Site::Exact(declared), _) => declared == op,
            _ => false,
        }
    }

    /// Two declarations on one site — § 2b's last row. Ambiguous rather
    /// than additive: repeated builder calls conventionally OVERRIDE, so
    /// accumulating would read as the second replacing the first.
    /// Conditions that belong together go in one slice.
    fn duplicate_sites(&self) -> Vec<PolicyError> {
        let mut seen: BTreeMap<Site, usize> = BTreeMap::new();
        for (site, _) in &self.decls {
            *seen.entry(*site).or_insert(0) += 1;
        }
        seen.into_iter()
            .filter(|(_, n)| *n > 1)
            .map(|(site, n)| PolicyError::DuplicateDeclaration {
                site: format!("{site:?}"),
                count: n,
            })
            .collect()
    }
}

/// One allocation per distinct path, for the `&'static str` that `Site`
/// and `Op` hold. Called once per nest at startup, never per request.
fn intern(s: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{LazyLock, Mutex};
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

/// Segment-aware prefix test.
///
/// `starts_with` alone is wrong in a way that would hand `/api/admin`'s
/// policy to `/api/administrators` — a widening with no error, which is
/// the exact failure class this whole mechanism exists to prevent.
fn path_covered_by(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    match path.strip_prefix(prefix) {
        // Exactly the prefix, or the prefix followed by a segment break.
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::scope_ports::{Access, MinPrincipal, MinRole, Scope};

    const ADMIN_ONLY: &[Condition] = &[Condition::role(MinRole::Admin)];
    const OWNER_ONLY: &[Condition] = &[Condition::role(MinRole::Owner)];
    const PUBLIC: &[Condition] = &[Condition::public()];
    const VISITOR: &[Condition] = &[Condition::visitor_allowed()];
    const SECURITY: &[Condition] = &[Condition::scope(Scope::Security, Access::ReadWrite)];

    /// The mechanism in one test: a nest declares admin ONCE and every
    /// operation beneath inherits it, without any of them restating it.
    #[test]
    fn a_nest_declaration_reaches_every_operation_beneath_it() {
        let mut reg = PolicyRegistry::new();
        reg.declare(Site::RestPrefix("/api/admin"), ADMIN_ONLY);

        let ops = [
            Op::Rest("GET", "/api/admin/sessions"),
            Op::Rest("DELETE", "/api/admin/sessions/{id}"),
            Op::Rest("GET", "/api/admin/users"),
        ];
        let table = reg.compile_all(&ops).expect("all compile");

        assert_eq!(table.len(), 3);
        for op in ops {
            assert_eq!(table.get(&op).unwrap().role, Some(MinRole::Admin));
        }
    }

    /// Prefix matching is by SEGMENT. `starts_with` would hand
    /// `/api/admin`'s policy to `/api/administrators` — a widening with
    /// no error, which is the failure class this mechanism exists to
    /// prevent, so it gets its own pin.
    #[test]
    fn a_prefix_does_not_leak_across_a_segment_boundary() {
        assert!(path_covered_by("/api/admin", "/api/admin"));
        assert!(path_covered_by("/api/admin/sessions", "/api/admin"));
        assert!(!path_covered_by("/api/administrators", "/api/admin"));
        assert!(!path_covered_by("/api/adm", "/api/admin"));

        let mut reg = PolicyRegistry::new();
        reg.declare(Site::RestPrefix("/api/admin"), ADMIN_ONLY);

        // A legitimately-covered op is included so the declaration is
        // not ORPHANED — otherwise that check fires and masks the
        // property under test.
        let impostor = Op::Rest("GET", "/api/administrators");
        let covered = Op::Rest("GET", "/api/admin/sessions");
        let table = reg.compile_all(&[impostor, covered]).expect("compiles");

        assert_eq!(
            table.get(&impostor).unwrap().role,
            None,
            "a lookalike path must NOT inherit the admin nest"
        );
    }

    /// Chains resolve root→leaf by prefix length, so a deeper
    /// declaration tightens one it inherits. Declaration ORDER in the
    /// registry must not matter — this records them leaf-first on
    /// purpose.
    #[test]
    fn deeper_prefixes_tighten_and_registration_order_is_irrelevant() {
        let mut reg = PolicyRegistry::new();
        reg.declare(Site::RestPrefix("/api/admin/danger"), OWNER_ONLY);
        reg.declare(Site::RestPrefix("/api/admin"), ADMIN_ONLY);

        let op = Op::Rest("POST", "/api/admin/danger/nuke");
        let table = reg.compile_all(&[op]).expect("tightening is legal");

        assert_eq!(table.get(&op).unwrap().role, Some(MinRole::Owner));
    }

    /// The inverse: a deeper declaration that WIDENS is refused, and the
    /// refusal survives the chain being assembled by prefix length
    /// rather than by explicit nesting.
    #[test]
    fn a_deeper_prefix_that_widens_is_refused_through_the_registry() {
        let mut reg = PolicyRegistry::new();
        reg.declare(Site::RestPrefix("/api/admin"), OWNER_ONLY);
        reg.declare(Site::RestPrefix("/api/admin/mild"), ADMIN_ONLY);

        let errs = reg
            .compile_all(&[Op::Rest("GET", "/api/admin/mild/thing")])
            .expect_err("Admin under Owner widens");

        assert!(matches!(errs[0], PolicyError::RoleWidens { .. }));
    }

    /// Non-REST operations take only an exact declaration — they have no
    /// path to prefix, which is the whole reason the table is keyed by
    /// operation rather than route.
    #[test]
    fn dav_ws_and_collab_ops_resolve_from_exact_declarations() {
        let mut reg = PolicyRegistry::new();
        reg.declare(
            Site::Exact(Op::Dav(Scope::Files, Access::ReadOnly)),
            ADMIN_ONLY,
        );
        reg.declare(Site::Exact(Op::Ws("rt.subscribe")), ADMIN_ONLY);
        reg.declare(Site::Exact(Op::CollabFrame(0x02)), ADMIN_ONLY);

        let ops = [
            Op::Dav(Scope::Files, Access::ReadOnly),
            Op::Ws("rt.subscribe"),
            Op::CollabFrame(0x02),
        ];
        let table = reg.compile_all(&ops).expect("all compile");

        for op in ops {
            assert_eq!(
                table.get(&op).unwrap().role,
                Some(MinRole::Admin),
                "{op} did not pick up its exact declaration"
            );
        }
    }

    /// A REST prefix must not reach a DAV or WS operation. They share no
    /// path space, and a prefix silently covering a frame kind would be
    /// the AWARENESS bug with extra steps.
    #[test]
    fn a_rest_prefix_never_reaches_a_non_rest_operation() {
        let mut reg = PolicyRegistry::new();
        reg.declare(Site::RestPrefix("/"), ADMIN_ONLY);

        // Plus one REST op, so the `/` declaration covers something and
        // the orphan check stays quiet.
        let table = reg
            .compile_all(&[
                Op::CollabFrame(0x01),
                Op::Ws("rt.subscribe"),
                Op::Rest("GET", "/api/anything"),
            ])
            .expect("compiles");

        for op in [Op::CollabFrame(0x01), Op::Ws("rt.subscribe")] {
            assert_eq!(
                table.get(&op).unwrap().role,
                None,
                "{op} inherited a REST prefix it shares no path space with"
            );
        }
    }

    /// § 2b's last row: two declarations on one site are ambiguous, since
    /// repeated builder calls conventionally override. Refused rather
    /// than accumulated.
    #[test]
    fn two_declarations_on_one_site_are_refused() {
        let mut reg = PolicyRegistry::new();
        reg.declare(Site::RestPrefix("/api/admin"), ADMIN_ONLY);
        reg.declare(Site::RestPrefix("/api/admin"), SECURITY);

        let errs = reg
            .compile_all(&[Op::Rest("GET", "/api/admin/x")])
            .expect_err("one site, two declarations");

        assert!(matches!(
            errs[0],
            PolicyError::DuplicateDeclaration { count: 2, .. }
        ));
    }

    /// § 2b's "a policy naming no live operation", and the check that
    /// found two tests in this very module declaring prefixes that gated
    /// nothing. It is the ONLY thing that would notice, because
    /// `axum::Router` cannot be asked which paths it holds — so a
    /// declaration for a renamed or typo'd path is otherwise
    /// indistinguishable from one that applies.
    #[test]
    fn a_declaration_covering_no_operation_is_reported() {
        let mut reg = PolicyRegistry::new();
        reg.declare(Site::RestPrefix("/api/admni"), ADMIN_ONLY.to_vec());

        let orphans = reg.orphaned_sites(&[Op::Rest("GET", "/api/admin/real")]);
        assert_eq!(orphans, vec![Site::RestPrefix("/api/admni")]);

        let errs = reg
            .compile_all(&[Op::Rest("GET", "/api/admin/real")])
            .expect_err("a typo'd prefix gates nothing");
        assert!(
            errs.iter()
                .any(|e| matches!(e, PolicyError::OrphanedDeclaration { .. }))
        );
    }

    /// An exact declaration for an operation that no longer exists — a
    /// deleted WS method or frame kind — is caught the same way.
    #[test]
    fn an_exact_declaration_for_a_dead_operation_is_reported() {
        let mut reg = PolicyRegistry::new();
        reg.declare(Site::Exact(Op::Ws("rt.removed")), ADMIN_ONLY.to_vec());

        let orphans = reg.orphaned_sites(&[Op::Ws("rt.subscribe")]);
        assert_eq!(orphans, vec![Site::Exact(Op::Ws("rt.removed"))]);
    }

    /// All errors at once. A boot failure naming one of nine bad
    /// declarations costs nine restarts to clear, which is a bad enough
    /// experience to be worth a test.
    #[test]
    fn every_error_is_reported_not_just_the_first() {
        let mut reg = PolicyRegistry::new();
        reg.declare(Site::RestPrefix("/api/a"), ADMIN_ONLY);
        reg.declare(Site::RestPrefix("/api/a/pub"), PUBLIC);
        reg.declare(Site::RestPrefix("/api/b"), OWNER_ONLY);
        reg.declare(Site::RestPrefix("/api/b/vis"), VISITOR);

        let errs = reg
            .compile_all(&[
                Op::Rest("GET", "/api/a/pub/thing"),
                Op::Rest("GET", "/api/b/vis/thing"),
            ])
            .expect_err("both chains are unsatisfiable");

        assert_eq!(errs.len(), 2, "got: {errs:?}");
        assert!(
            errs.iter()
                .all(|e| matches!(e, PolicyError::PublicWithRole { .. }))
        );
    }

    /// The acceptance mechanism itself. The dump must be byte-stable
    /// across runs and independent of declaration order, or comparing it
    /// before and after the fold proves nothing.
    #[test]
    fn the_dump_is_byte_stable_and_order_independent() {
        let ops = [
            Op::Rest("GET", "/api/admin/sessions"),
            Op::Rest("GET", "/api/files/{id}"),
            Op::Dav(Scope::Files, Access::ReadOnly),
            Op::Ws("rt.subscribe"),
        ];

        let mut a = PolicyRegistry::new();
        a.declare(Site::RestPrefix("/api/admin"), ADMIN_ONLY);
        a.declare(Site::RestPrefix("/api/files"), VISITOR);
        a.declare(
            Site::Exact(Op::Dav(Scope::Files, Access::ReadOnly)),
            ADMIN_ONLY,
        );

        // Same declarations, reversed.
        let mut b = PolicyRegistry::new();
        b.declare(
            Site::Exact(Op::Dav(Scope::Files, Access::ReadOnly)),
            ADMIN_ONLY,
        );
        b.declare(Site::RestPrefix("/api/files"), VISITOR);
        b.declare(Site::RestPrefix("/api/admin"), ADMIN_ONLY);

        let one = a.compile_all(&ops).expect("a").render();
        let two = b.compile_all(&ops).expect("b").render();

        assert_eq!(one, two, "declaration order changed the dump");
        assert_eq!(one, a.compile_all(&ops).expect("again").render());
    }

    /// Every axis is spelled out, defaults included. A dump that omitted
    /// a resolved default would hide precisely the change step 4 must not
    /// make — an operation quietly losing its `principal=User`.
    #[test]
    fn the_dump_spells_out_resolved_defaults() {
        let mut reg = PolicyRegistry::new();
        reg.declare(Site::RestPrefix("/api/admin"), ADMIN_ONLY);

        let undeclared = Op::Rest("GET", "/api/whatever");
        let dump = reg
            .compile_all(&[Op::Rest("GET", "/api/admin/x"), undeclared])
            .expect("compiles")
            .render();

        // `scope=?`, not `scope=-`. The dump's job is to show what is
        // NOT classified, so "nobody declared a scope" (unsatisfiable,
        // and a generation failure once PR 2 step 4 lands) must not read
        // the same as "no scope required, declared" — which renders `-`.
        // Collapsing the two is how an unclassified operation would hide
        // in a dump someone was reading to find exactly that.
        assert!(
            dump.contains("GET /api/whatever\tprincipal=User\trole=-\tscope=?"),
            "an undeclared op must render its defaults explicitly, with an \
             UNDEFINED scope distinguishable from a declared none:\n{dump}"
        );
        // Also `?`: declaring a ROLE says nothing about the scope axis,
        // so this operation is unclassified there too. PR 2 step 3 is
        // what turns every `?` in this dump into an area or a declared
        // none.
        assert!(dump.contains("GET /api/admin/x\tprincipal=User\trole=Admin\tscope=?"));

        // Sorted, so the diff of two dumps is readable.
        let lines: Vec<&str> = dump.lines().collect();
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_eq!(lines, sorted, "dump must be sorted");
    }

    /// An operation nobody declared still gets a policy — the SAFE one.
    /// This is § Authentication by default surviving the table layer:
    /// forgetting to declare yields a 401, not an open door.
    #[test]
    fn an_undeclared_operation_compiles_to_the_safe_default() {
        let reg = PolicyRegistry::new();
        let op = Op::Rest("GET", "/api/forgotten");
        let table = reg.compile_all(&[op]).expect("compiles");

        assert_eq!(table.get(&op), Some(&Policy::default()));
        assert_eq!(table.get(&op).unwrap().principal, MinPrincipal::User);
    }
}
