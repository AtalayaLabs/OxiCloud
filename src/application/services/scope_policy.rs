//! The COMPILE step — fold a chain of declarations root→leaf into one
//! total [`Policy`], or refuse to start.
//!
//! PR 1 step 3a of `docs/plan/scope-gates.md`, and still inert: nothing
//! calls `compile` from the router yet. This half is pure, so it is fully
//! testable without a server — which is the reason to land it on its own.
//!
//! Why a compile step exists at all: a stack of independent layers has no
//! moment where every declaration is in one place, so it cannot notice
//! that two of them contradict. It resolves a contradiction to
//! "unreachable" in silence. A fold has to walk every declaration anyway,
//! so it can refuse instead — "won't start" rather than "mysteriously
//! 403s" (§ 2b, and the second tier of § Maximise deny-by-default).
//!
//! **An omitted axis is INHERITED, not defaulted.** Each axis stays
//! undeclared through the fold and takes its default only at the end.
//! That is what makes `visitor_allowed()` legal on `/api/files/{id}`
//! beneath an `/api` nest that never declared a principal: the leaf is the
//! FIRST declaration on that axis, not a widening of one. A widening is
//! refused only when a parent explicitly declared something stricter.

use crate::application::ports::scope_ports::{
    Access, Axis, Condition, MinPrincipal, MinRole, Op, Policy, Scope, ScopeReq, ScopeSet,
};
use crate::domain::entities::user::CallerKind;

/// A declaration that cannot be compiled. Every variant is a BOOT error:
/// the server refuses to start, per `feedback_fail_fast_config` — a boot
/// panic beats a silent skip, because a skipped gate is invisible until
/// someone exploits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    /// `public()` or `visitor_allowed()` together with any role.
    ///
    /// Structurally unsatisfiable rather than merely odd:
    /// `UserRole::Anonymous` is never stored in `auth.users` — it exists
    /// only on a session and its JWT, and `from_stored` deliberately
    /// cannot produce it (`user.rs`). So no role can ever be satisfied by
    /// a caller this policy admits, the operation is unreachable, and one
    /// of the two declarations is a lie.
    ///
    /// This is the refusal worth having. `public()` on a route under
    /// `/admin` means "make this public" while the nest means "admins
    /// only"; whichever wins, the other was wrong, and stacked layers
    /// resolve it to "unreachable" without telling anyone.
    PublicWithRole {
        op: Op,
        principal: MinPrincipal,
        role: MinRole,
    },

    /// `public()` or `visitor_allowed()` together with a scope. A
    /// share-ring visitor carries no `ScopeSet`, so the requirement can
    /// never be met.
    PublicWithScope {
        op: Op,
        principal: MinPrincipal,
        scope: Scope,
    },

    /// A role declaration that WIDENS its parent's: `Owner` nest,
    /// `Internal` child. The fold is `max`, so the child's intent would be
    /// silently discarded — better to refuse than to ignore.
    RoleWidens {
        op: Op,
        parent: MinRole,
        child: MinRole,
    },

    /// A child declaring `scopeless()` under a parent that required an
    /// AREA. The scope axis folds by tightening like the other two, so
    /// "no scope required" cannot be reached from "this area required" —
    /// and silently widening it is precisely the fail-open this axis
    /// exists to prevent.
    ///
    /// Only an area parent triggers it. Repeating `scopeless()` down a
    /// chain, or naming an area under a scopeless parent, are both the
    /// legitimate direction.
    ScopeWidens { op: Op, parent: Scope },

    /// A principal declaration that WIDENS an explicit parent's. Same
    /// reasoning as [`Self::RoleWidens`]; the fold is also a `max`.
    ///
    /// Only an EXPLICIT parent declaration triggers this. A leaf declaring
    /// `visitor_allowed()` under a nest that declared nothing is the first
    /// declaration on the axis, which is the common and legitimate case.
    PrincipalWidens {
        op: Op,
        parent: MinPrincipal,
        child: MinPrincipal,
    },

    /// The same axis twice in ONE declaration:
    /// `[role(Admin), role(Owner)]` has no legitimate meaning — someone
    /// wrote two things where they meant one.
    ///
    /// Across a nest CHAIN the same pair is ordinary inheritance and folds
    /// to `Owner`. Folding is for chains, not for the slice, which is why
    /// this is checked per slice BEFORE the fold.
    RepeatedAxis { op: Op, axis: Axis },

    /// Two different scopes required by one chain.
    ///
    /// § 2. Compile says scope folds as "union of what is required", but
    /// [`Policy::scope`] holds a single pair, so a union of two DIFFERENT
    /// areas has nowhere to live. Refused rather than silently dropping
    /// one: an operation that genuinely needs two areas is a signal the
    /// field should become a `ScopeSet`, and that decision should be
    /// forced rather than papered over.
    ConflictingScopes { op: Op, first: Scope, second: Scope },
    /// Two declarations on ONE site — § 2b's last row. Ambiguous rather
    /// than additive: repeated builder calls conventionally OVERRIDE, so
    /// accumulating would read as the second replacing the first.
    /// Conditions that belong together go in one slice.
    ///
    /// `site` is pre-rendered rather than typed, so this error stays in
    /// the pure compile module while `Site` lives with the registry that
    /// owns it.
    DuplicateDeclaration { site: String, count: usize },

    /// A declaration that matches no live operation — § 2b's "a policy
    /// naming no live operation".
    ///
    /// Load-bearing rather than belt-and-braces: `axum::Router` cannot be
    /// asked which paths it holds, so a declaration for a typo'd or
    /// renamed path is otherwise indistinguishable from one that applies,
    /// and it silently stops gating anything. Nothing else in the design
    /// catches it.
    OrphanedDeclaration { site: String },
    // Reserved, pending `auth.sessions.elevated_at`:
    //
    // PublicWithElevation — elevation is a human re-proving themselves
    // against an account, and a public caller has no account. Cannot be
    // written yet because `Condition` has no `Elevated` variant.
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::PublicWithRole {
                op,
                principal,
                role,
            } => write!(
                f,
                "`{op}` admits {principal:?} but requires role {role:?}: \
                 unsatisfiable, because an anonymous caller has no \
                 `auth.users` row to hold a role. Drop one of the two."
            ),
            PolicyError::PublicWithScope {
                op,
                principal,
                scope,
            } => write!(
                f,
                "`{op}` admits {principal:?} but requires scope \
                 {scope:?}: unsatisfiable, because a share-ring visitor \
                 carries no scope set."
            ),
            PolicyError::RoleWidens { op, parent, child } => write!(
                f,
                "`{op}` declares role {child:?}, widening the inherited \
                 {parent:?}. The fold is a max, so {child:?} would be \
                 discarded silently."
            ),
            PolicyError::PrincipalWidens { op, parent, child } => write!(
                f,
                "`{op}` declares principal {child:?}, widening the \
                 explicitly inherited {parent:?}. The fold is a max, so \
                 {child:?} would be discarded silently."
            ),
            PolicyError::RepeatedAxis { op, axis } => write!(
                f,
                "`{op}` declares the {axis:?} axis twice in ONE slice. \
                 Put one condition per axis; inheritance across a nest \
                 chain is what folds."
            ),
            PolicyError::DuplicateDeclaration { site, count } => write!(
                f,
                "{site} carries {count} policy declarations. A repeated \
                 declaration would read as overriding the first, not \
                 adding to it — put every condition in one slice."
            ),
            PolicyError::OrphanedDeclaration { site } => write!(
                f,
                "{site} matches no live operation — a renamed or deleted \
                 route, or a typo. It gates nothing, and axum cannot be \
                 asked which paths exist, so this check is the only thing \
                 that would notice."
            ),
            PolicyError::ScopeWidens { op, parent } => write!(
                f,
                "{op}: a child declares scopeless() under a parent requiring \
                 {parent:?} — the scope axis tightens, it does not widen. \
                 Name the area, or remove the parent's declaration."
            ),
            PolicyError::ConflictingScopes { op, first, second } => write!(
                f,
                "`{op}` requires two scopes, {first:?} and {second:?}, \
                 but a policy holds one. If this is real, `Policy::scope` \
                 needs to become a `ScopeSet`."
            ),
        }
    }
}

impl std::error::Error for PolicyError {}

/// Accumulator: each axis is `None` until some slice declares it, so the
/// defaults can be applied at the END. See the module doc on why that
/// distinction is load-bearing.
#[derive(Default)]
struct Folded {
    role: Option<MinRole>,
    principal: Option<MinPrincipal>,
    /// `None` = nothing declared on this axis yet, which resolves to
    /// [`ScopeReq::Undefined`]. `Some(ScopeReq::None)` is the DIFFERENT
    /// state of having declared that no scope is required.
    scope: Option<ScopeReq>,
}

/// Fold ONE area declaration into the accumulator.
///
/// Shared by `Condition::Scope` and `Condition::ScopeByVerb`, which differ
/// only in where the access came from — so the conflict and tightening
/// rules live in one place rather than twice.
fn fold_scope(op: Op, acc: &mut Option<ScopeReq>, s: Scope, a: Access) -> Result<(), PolicyError> {
    match *acc {
        // Same area: tighten the access. `ReadWrite` wins, which
        // `Access`'s ordering already encodes.
        Some(ScopeReq::Area(prev, prev_access)) if prev == s => {
            *acc = Some(ScopeReq::Area(s, prev_access.max(a)));
        }
        Some(ScopeReq::Area(first, _)) => {
            return Err(PolicyError::ConflictingScopes {
                op,
                first,
                second: s,
            });
        }
        // A parent declaring "none required" may be TIGHTENED by a child
        // naming an area — the direction the other axes also allow.
        Some(ScopeReq::None) | Some(ScopeReq::Undefined) | None => {
            *acc = Some(ScopeReq::Area(s, a));
        }
    }
    Ok(())
}

/// Fold `chain` — ordered ROOT first, leaf last — into one total
/// [`Policy`].
///
/// `op` is carried only so a refusal can name the operation an
/// implementor has to go and fix.
///
/// Runs once at startup, never per request: § 3. Evaluate is field reads
/// against the result, and it runs once rather than once per nesting
/// level.
pub fn compile(op: Op, chain: &[&[Condition]]) -> Result<Policy, PolicyError> {
    let mut acc = Folded::default();

    for slice in chain {
        // Per-slice FIRST: a repeated axis within one declaration is
        // ambiguous, and folding it would hide that by resolving it.
        reject_repeated_axis(op, slice)?;

        for &cond in *slice {
            match cond {
                Condition::Role(r) => {
                    if let Some(parent) = acc.role
                        && r < parent
                    {
                        return Err(PolicyError::RoleWidens {
                            op,
                            parent,
                            child: r,
                        });
                    }
                    acc.role = Some(acc.role.map_or(r, |p| p.max(r)));
                }
                Condition::Principal(p) => {
                    if let Some(parent) = acc.principal
                        && p < parent
                    {
                        return Err(PolicyError::PrincipalWidens {
                            op,
                            parent,
                            child: p,
                        });
                    }
                    acc.principal = Some(acc.principal.map_or(p, |q| q.max(p)));
                }
                Condition::Scope(s, a) => fold_scope(op, &mut acc.scope, s, a)?,
                // Resolved HERE rather than at request time: `op` carries
                // the method, so the compiled policy holds a plain
                // `Access` and evaluate stays field reads.
                Condition::ScopeByVerb(s) => {
                    let a = Access::from_method(op.method());
                    fold_scope(op, &mut acc.scope, s, a)?;
                }
                Condition::Scopeless => match acc.scope {
                    // A parent that required an area cannot be WIDENED to
                    // "none required" by a child — the same rule the role
                    // and principal axes enforce with a `max`.
                    Some(ScopeReq::Area(first, _)) => {
                        return Err(PolicyError::ScopeWidens { op, parent: first });
                    }
                    // Repeating it down the chain is harmless.
                    Some(ScopeReq::None) | Some(ScopeReq::Undefined) | None => {
                        acc.scope = Some(ScopeReq::None);
                    }
                },
            }
        }
    }

    // Defaults applied now, not during the fold — see the module doc.
    let policy = Policy {
        role: acc.role,
        principal: acc.principal.unwrap_or_default(),
        // `None` from the fold means NOTHING was declared, which is
        // `Undefined` — unsatisfiable, and refused by spec generation.
        // `ScopeReq::None` is a different state that only
        // `Condition::scopeless()` can produce (PR 2 step 2).
        scope: acc.scope.unwrap_or(ScopeReq::Undefined),
    };

    reject_unsatisfiable(op, &policy)?;
    Ok(policy)
}

/// The same axis twice in one slice. Quadratic, over slices of two or
/// three conditions, once at startup — a `HashSet` here would cost more
/// than it saves.
fn reject_repeated_axis(op: Op, slice: &[Condition]) -> Result<(), PolicyError> {
    for (i, a) in slice.iter().enumerate() {
        for b in &slice[i + 1..] {
            if a.axis() == b.axis() {
                return Err(PolicyError::RepeatedAxis { op, axis: a.axis() });
            }
        }
    }
    Ok(())
}

/// Contradictions visible only once every axis is resolved, so they are
/// checked on the FOLDED policy rather than per slice: the role may come
/// from a nest and the `public()` from the leaf, which is exactly the case
/// § 2b calls the one worth having.
fn reject_unsatisfiable(op: Op, policy: &Policy) -> Result<(), PolicyError> {
    // `User` is the only principal with an `auth.users` row behind it.
    if policy.principal < MinPrincipal::User {
        if let Some(role) = policy.role {
            return Err(PolicyError::PublicWithRole {
                op,
                principal: policy.principal,
                role,
            });
        }
        if let Some((scope, _)) = policy.scope.area() {
            return Err(PolicyError::PublicWithScope {
                op,
                principal: policy.principal,
                scope,
            });
        }
    }
    Ok(())
}

/// Why a caller was refused. Returned by [`evaluate`] instead of an
/// `AppError`, because `AppError` lives in `interfaces/` and the
/// dependency points inward — the HTTP mapping belongs to the layer that
/// speaks HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// No credential at all, on an operation that needs one.
    AuthenticationRequired,
    /// A share-ring visitor on a user-only operation.
    VisitorRefused,
    /// Authenticated, but below the role floor. Carries the floor so the
    /// audit line can name the event the old layer emitted.
    RoleInsufficient { required: MinRole },
    /// The credential was not minted for this area.
    ScopeNotGranted { scope: Scope, access: Access },
}

impl Denial {
    /// The `(event, reason)` pair to log.
    ///
    /// **The engine emits its OWN vocabulary rather than reproducing the
    /// five pairs the folded layers used.** An earlier version mapped
    /// each denial back to its layer's pair to keep aggregators matching;
    /// that was the wrong call for three reasons:
    ///
    /// - AGENTS.md's rule is "a new denial cause gets a new `reason`,
    ///   never a repurposed one" — and a policy denial IS a new cause, a
    ///   different mechanism, so a new reason is the compliant answer
    ///   rather than a break of stability.
    /// - the mapping was already wrong for `MinRole::Owner`: it borrowed
    ///   `server_owner.transfer_rejected` because there is exactly ONE
    ///   Owner site today, so a second would have emitted an
    ///   ownership-transfer event for an unrelated refusal.
    /// - it encoded dead history. Nobody would later know why
    ///   `MinRole::Internal` emitted `authz.external_user_blocked`.
    ///
    /// `event` stays `authz.denied` for every gate refusal, so a filter
    /// on the `authz.*` domain keeps working; scope gets
    /// `authz.scope_denied`, which § Wire contracts fixed in advance.
    /// The ROLE is a structured field, not part of the reason — one
    /// reason per cause, with `required` carrying which floor was missed.
    ///
    /// Every reason reads `<what>_required`, so the vocabulary is one
    /// shape rather than a mix of imperatives and nouns.
    ///
    /// Retired by this change, and worth listing for release notes:
    /// `authz.admin_denied`/`not_admin`,
    /// `authz.external_user_blocked`/`internal_only_surface`,
    /// `server_owner.transfer_rejected`/`caller_not_owner`,
    /// `authz.denied`/`anonymous_not_authenticated`,
    /// `authz.denied`/`anonymous_route_not_allowlisted`.
    pub const fn audit(self) -> (&'static str, &'static str) {
        match self {
            Denial::AuthenticationRequired => ("authz.denied", "authentication_required"),
            Denial::VisitorRefused => ("authz.denied", "user_session_required"),
            // Not a "higher role": `Internal` is "not external", a
            // different question from a rank, so it gets its own reason.
            Denial::RoleInsufficient {
                required: MinRole::Internal,
            } => ("authz.denied", "internal_user_required"),
            Denial::RoleInsufficient { .. } => ("authz.denied", "higher_role_required"),
            Denial::ScopeNotGranted { .. } => ("authz.scope_denied", "scope_not_granted"),
        }
    }

    /// The floor that was missed, for the `required` field. `None` where
    /// the denial is not about a role.
    pub const fn required_role(self) -> Option<MinRole> {
        match self {
            Denial::RoleInsufficient { required } => Some(required),
            _ => None,
        }
    }
}

/// Check one caller against one compiled [`Policy`].
///
/// Field reads only — § 3. Evaluate. The role's cached lookup already
/// happened in `auth_middleware`, so this adds nothing per request, and it
/// runs ONCE rather than once per nesting level the way a layer stack
/// does.
///
/// `caller` is `None` when no credential was presented. `scopes` is the
/// credential's granted set, and `None` means the credential carries no
/// scope information — which grandfathers to FULL, never empty
/// (§ Absent means FULL, everywhere). Reading an absent claim as empty
/// would refuse every token minted before the claim existed.
///
/// Order matters for the message, not the outcome: principal is checked
/// before role, so a caller with no credential is told to authenticate
/// rather than told their role is too low.
pub fn evaluate(
    policy: &Policy,
    caller: Option<CallerKind>,
    scopes: Option<ScopeSet>,
) -> Result<(), Denial> {
    if !policy.principal.satisfied_by(caller) {
        return Err(match caller {
            None => Denial::AuthenticationRequired,
            // A credential exists but is not a user's: the only way to
            // fail the principal check with one in hand.
            Some(_) => Denial::VisitorRefused,
        });
    }

    if let Some(required) = policy.role {
        // `None` already failed the principal check above unless the
        // policy admits `Public`, and a public operation with a role is a
        // boot error — so a missing caller here cannot satisfy a role.
        let satisfied = caller.is_some_and(|k| required.satisfied_by(k));
        if !satisfied {
            return Err(Denial::RoleInsufficient { required });
        }
    }

    // `Undefined` is deliberately NOT refused here yet.
    //
    // It is the default, so refusing it before any area is declared would
    // 403 every request on the instance. The refusal lands with the
    // declarations, in PR 2 step 4 — as a GENERATION failure, with a
    // runtime deny beside it as defence in depth. Until then `Undefined`
    // and `None` behave alike, which `ScopeReq::area()` encodes.
    if let Some((scope, access)) = policy.scope.area() {
        // No scope set means a credential minted before the claim
        // existed, and those predate any restriction — § Defaults.
        let granted = scopes.unwrap_or_else(ScopeSet::full);
        if !granted.allows(scope, access) {
            return Err(Denial::ScopeNotGranted { scope, access });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::user::UserRole;

    const ADMIN: Op = Op::Rest("GET", "/api/admin/sessions");

    /// `scope_by_verb` resolves from the OPERATION's method, so one
    /// prefix declaration covers a subtree whose verbs differ. This is
    /// what makes ten area declarations enough instead of thirty.
    #[test]
    fn scope_by_verb_reads_the_operations_method() {
        let decl = [Condition::scope_by_verb(Scope::Files)];

        let read = compile(Op::Rest("GET", "/api/files/{id}"), &[&decl]).expect("compiles");
        assert_eq!(read.scope, ScopeReq::Area(Scope::Files, Access::ReadOnly));

        let write = compile(Op::Rest("DELETE", "/api/files/{id}"), &[&decl]).expect("compiles");
        assert_eq!(write.scope, ScopeReq::Area(Scope::Files, Access::ReadWrite));
    }

    /// An unknown method is a WRITE, and `LOCK` is the case most likely
    /// to be misread as one: it mutates lock state. Fail-closed by
    /// omission — the opposite default would admit a method nobody
    /// classified on a read-only credential.
    #[test]
    fn an_unclassified_method_demands_the_stronger_scope() {
        assert_eq!(Access::from_method("LOCK"), Access::ReadWrite);
        assert_eq!(Access::from_method("UNLOCK"), Access::ReadWrite);
        assert_eq!(Access::from_method("WEIRD"), Access::ReadWrite);
        assert_eq!(Access::from_method("PROPFIND"), Access::ReadOnly);
        assert_eq!(Access::from_method("REPORT"), Access::ReadOnly);
    }

    /// `scopeless()` is a DECLARED state, not the absence of one — which
    /// is the whole reason `ScopeReq` has three variants. A reader of the
    /// dump, and the step-4 generation check, both need to tell "nobody
    /// classified this" from "no scope required, deliberately".
    #[test]
    fn scopeless_is_distinct_from_declaring_nothing() {
        let declared = compile(ADMIN, &[&[Condition::scopeless()]]).expect("compiles");
        assert_eq!(declared.scope, ScopeReq::None);

        let silent = compile(ADMIN, &[]).expect("compiles");
        assert_eq!(silent.scope, ScopeReq::Undefined);
    }

    /// The scope axis TIGHTENS down a chain, like role and principal.
    ///
    /// A parent saying "no scope required" may be narrowed by a child
    /// naming an area; the reverse is refused, because silently widening
    /// an area requirement to "none" is exactly the fail-open this axis
    /// exists to close.
    #[test]
    fn the_scope_axis_tightens_but_does_not_widen() {
        let tightened = compile(
            ADMIN,
            &[&[Condition::scopeless()], &[Condition::scope_by_verb(Scope::Files)]],
        )
        .expect("narrowing a scopeless parent is legal");
        assert_eq!(tightened.scope, ScopeReq::Area(Scope::Files, Access::ReadOnly));

        let widened = compile(
            ADMIN,
            &[
                &[Condition::scope(Scope::Files, Access::ReadWrite)],
                &[Condition::scopeless()],
            ],
        );
        assert!(
            matches!(
                widened,
                Err(PolicyError::ScopeWidens {
                    parent: Scope::Files,
                    ..
                })
            ),
            "scopeless() under an area parent must be refused, not resolved: {widened:?}"
        );
    }

    /// An operation that declares NOTHING compiles to "an authenticated
    /// user", which is the whole of § Authentication by default. If this
    /// ever yields `Public`, every undeclared operation silently opens.
    #[test]
    fn an_empty_chain_demands_an_authenticated_user() {
        let p = compile(ADMIN, &[]).expect("empty is legal, not an error");
        assert_eq!(p, Policy::default());
        assert_eq!(p.principal, MinPrincipal::User);
    }

    /// The point of the fold: a nest declares the role, a leaf adds a
    /// different axis, and the leaf does not have to restate — nor
    /// thereby overtighten — what the nest said.
    #[test]
    fn a_leaf_inherits_the_nests_axes_without_restating_them() {
        let p = compile(
            ADMIN,
            &[
                &[Condition::role(MinRole::Admin)],
                &[Condition::scope(Scope::Security, Access::ReadWrite)],
            ],
        )
        .expect("adding a second axis is ordinary inheritance");

        assert_eq!(p.role, Some(MinRole::Admin));
        assert_eq!(p.scope, ScopeReq::Area(Scope::Security, Access::ReadWrite));
        assert_eq!(p.principal, MinPrincipal::User);
    }

    /// The same axis twice across a CHAIN is inheritance and folds to the
    /// stricter — the `/admin` → `/admin/fake-route` case from § 1.
    #[test]
    fn tightening_the_same_axis_down_a_chain_folds_to_the_max() {
        let p = compile(
            Op::Rest("POST", "/api/admin/fake-route"),
            &[
                &[Condition::role(MinRole::Admin)],
                &[Condition::role(MinRole::Owner)],
            ],
        )
        .expect("a child may tighten");

        assert_eq!(p.role, Some(MinRole::Owner));
    }

    /// The same axis twice in ONE slice is ambiguous: someone wrote two
    /// things where they meant one. Checked before the fold, because
    /// folding would resolve it and hide the mistake.
    #[test]
    fn the_same_axis_twice_in_one_slice_is_refused() {
        let err = compile(
            ADMIN,
            &[&[
                Condition::role(MinRole::Admin),
                Condition::role(MinRole::Owner),
            ]],
        )
        .expect_err("two roles in one slice has no meaning");

        assert_eq!(
            err,
            PolicyError::RepeatedAxis {
                op: ADMIN,
                axis: Axis::Role
            }
        );
    }

    /// `public()` and `visitor_allowed()` share the Principal axis, so
    /// declaring both in one slice is ambiguous rather than additive.
    #[test]
    fn public_and_visitor_allowed_in_one_slice_is_refused() {
        let err = compile(
            Op::Rest("GET", "/api/files/{id}"),
            &[&[Condition::public(), Condition::visitor_allowed()]],
        )
        .expect_err("both name the same axis");

        assert!(matches!(
            err,
            PolicyError::RepeatedAxis {
                axis: Axis::Principal,
                ..
            }
        ));
    }

    /// A child may not widen a role the parent set. The fold is a `max`,
    /// so without this the child's declaration would vanish silently.
    #[test]
    fn widening_a_role_is_a_boot_error_not_a_silent_discard() {
        let err = compile(
            ADMIN,
            &[
                &[Condition::role(MinRole::Owner)],
                &[Condition::role(MinRole::Internal)],
            ],
        )
        .expect_err("Internal under Owner is a widening");

        assert_eq!(
            err,
            PolicyError::RoleWidens {
                op: ADMIN,
                parent: MinRole::Owner,
                child: MinRole::Internal
            }
        );
    }

    /// THE refusal worth having: `public()` on a leaf under an `/admin`
    /// nest. Neither declaration is wrong on its own, which is why only a
    /// step that sees both can catch it — stacked layers resolve this to
    /// an unreachable route in silence.
    #[test]
    fn public_under_an_admin_nest_refuses_to_boot() {
        let err = compile(
            Op::Rest("GET", "/api/admin/public-thing"),
            &[&[Condition::role(MinRole::Admin)], &[Condition::public()]],
        )
        .expect_err("no caller can be both anonymous and an admin");

        assert!(matches!(
            err,
            PolicyError::PublicWithRole {
                principal: MinPrincipal::Public,
                role: MinRole::Admin,
                ..
            }
        ));

        // And the message has to name the operation, since a boot panic
        // is all the implementor gets.
        assert!(err.to_string().contains("/api/admin/public-thing"));
    }

    /// A visitor carries no `ScopeSet`, so a scope requirement can never
    /// be met — the same unsatisfiability as the role case.
    #[test]
    fn a_visitor_with_a_scope_requirement_refuses_to_boot() {
        let err = compile(
            Op::Rest("GET", "/api/files/{id}"),
            &[&[
                Condition::visitor_allowed(),
                Condition::scope(Scope::Files, Access::ReadOnly),
            ]],
        )
        .expect_err("a ring has no scope set");

        assert!(matches!(
            err,
            PolicyError::PublicWithScope {
                principal: MinPrincipal::Visitor,
                scope: Scope::Files,
                ..
            }
        ));
    }

    /// The case that makes "omitted means INHERITED, not defaulted"
    /// load-bearing. `/api/files/{id}` must serve share visitors, and its
    /// `/api` nest declares no principal — so the leaf is the FIRST
    /// declaration on that axis, not a widening of the `User` default.
    ///
    /// Had the default been applied during the fold instead of after,
    /// this would be a boot error and the route would be undeclarable.
    #[test]
    fn visitor_allowed_is_legal_when_no_parent_declared_a_principal() {
        let p = compile(
            Op::Rest("GET", "/api/files/{id}"),
            &[&[], &[Condition::visitor_allowed()]],
        )
        .expect("the leaf is the first declaration on this axis");

        assert_eq!(p.principal, MinPrincipal::Visitor);
        assert_eq!(p.role, None);
    }

    /// But an EXPLICIT parent still cannot be widened — that is the
    /// difference between inheriting an unset axis and overriding a set
    /// one.
    #[test]
    fn widening_an_explicit_principal_is_refused() {
        let op = Op::Rest("GET", "/api/private/thing");
        let err = compile(
            op,
            &[
                &[Condition::Principal(MinPrincipal::User)],
                &[Condition::visitor_allowed()],
            ],
        )
        .expect_err("User was declared, so Visitor widens it");

        assert_eq!(
            err,
            PolicyError::PrincipalWidens {
                op,
                parent: MinPrincipal::User,
                child: MinPrincipal::Visitor
            }
        );
    }

    /// Not a contradiction, and pinned as such: an admin whose credential
    /// is scoped `files:ro`. Role and scope INTERSECT by design — the
    /// admin simply cannot use that credential for admin work (§ 2b).
    #[test]
    fn a_role_plus_a_narrow_scope_is_not_a_contradiction() {
        let p = compile(
            ADMIN,
            &[&[
                Condition::role(MinRole::Admin),
                Condition::scope(Scope::Files, Access::ReadOnly),
            ]],
        )
        .expect("role and scope are different axes and intersect");

        assert_eq!(p.role, Some(MinRole::Admin));
        assert_eq!(p.scope, ScopeReq::Area(Scope::Files, Access::ReadOnly));
    }

    /// The same area twice tightens access: `ro` then `rw` yields `rw`.
    #[test]
    fn the_same_scope_twice_tightens_to_the_wider_access() {
        let p = compile(
            Op::Rest("PUT", "/api/auth/password"),
            &[
                &[Condition::scope(Scope::Security, Access::ReadOnly)],
                &[Condition::scope(Scope::Security, Access::ReadWrite)],
            ],
        )
        .expect("same area, stricter access");

        assert_eq!(p.scope, ScopeReq::Area(Scope::Security, Access::ReadWrite));
    }

    /// Two DIFFERENT areas in one chain are refused rather than silently
    /// reduced to one. § 2. Compile says scope folds as a union, but
    /// `Policy::scope` holds a single pair — so a real two-area operation
    /// should force that field to become a `ScopeSet` rather than lose a
    /// requirement here.
    #[test]
    fn two_different_scopes_are_refused_rather_than_one_dropped() {
        let op = Op::Rest("POST", "/api/two-areas");
        let err = compile(
            op,
            &[
                &[Condition::scope(Scope::Files, Access::ReadOnly)],
                &[Condition::scope(Scope::Calendar, Access::ReadOnly)],
            ],
        )
        .expect_err("a policy holds one scope");

        assert_eq!(
            err,
            PolicyError::ConflictingScopes {
                op,
                first: Scope::Files,
                second: Scope::Calendar
            }
        );
    }

    /// Declaration order within a chain must not change the result, or a
    /// reviewer cannot reason about a nest without reading every leaf.
    /// (Order ACROSS the chain is root→leaf and meaningful; order of
    /// conditions in one slice is not.)
    #[test]
    fn condition_order_within_a_slice_does_not_matter() {
        let a = compile(
            ADMIN,
            &[&[
                Condition::role(MinRole::Admin),
                Condition::scope(Scope::Security, Access::ReadWrite),
            ]],
        )
        .expect("a");
        let b = compile(
            ADMIN,
            &[&[
                Condition::scope(Scope::Security, Access::ReadWrite),
                Condition::role(MinRole::Admin),
            ]],
        )
        .expect("b");

        assert_eq!(a, b);
    }

    // ───────────────────────── evaluate ─────────────────────────

    const USER: Option<CallerKind> = Some(CallerKind::Internal(UserRole::User));
    const ADMIN_CALLER: Option<CallerKind> = Some(CallerKind::Internal(UserRole::Admin));
    const OWNER_CALLER: Option<CallerKind> = Some(CallerKind::Internal(UserRole::Owner));
    const EXTERNAL: Option<CallerKind> = Some(CallerKind::External);
    const VISITOR: Option<CallerKind> = Some(CallerKind::Visitor);

    /// The default policy: a session and nothing more. Anonymous is
    /// refused, every authenticated kind that has a user row passes.
    #[test]
    fn the_default_policy_admits_any_user_and_no_one_else() {
        let p = Policy::default();

        assert_eq!(
            evaluate(&p, None, None),
            Err(Denial::AuthenticationRequired)
        );
        assert_eq!(evaluate(&p, VISITOR, None), Err(Denial::VisitorRefused));

        for who in [USER, ADMIN_CALLER, OWNER_CALLER, EXTERNAL] {
            assert_eq!(evaluate(&p, who, None), Ok(()), "{who:?}");
        }
    }

    /// `MinRole::Internal` refuses an EXTERNAL user whose role is `user`,
    /// which is the whole reason the caller carries a kind rather than a
    /// role — and exactly what `require_internal_user_layer` does today.
    #[test]
    fn internal_refuses_an_external_and_a_visitor_but_not_a_user() {
        let p = Policy {
            role: Some(MinRole::Internal),
            ..Policy::default()
        };

        assert_eq!(
            evaluate(&p, EXTERNAL, None),
            Err(Denial::RoleInsufficient {
                required: MinRole::Internal
            })
        );
        // A visitor fails earlier, on the principal axis.
        assert_eq!(evaluate(&p, VISITOR, None), Err(Denial::VisitorRefused));
        assert_eq!(evaluate(&p, USER, None), Ok(()));
        assert_eq!(evaluate(&p, ADMIN_CALLER, None), Ok(()));
    }

    /// Owner outranks admin. Six sites once denied the owner by comparing
    /// a role string to `"admin"` literally, so the direction is pinned.
    #[test]
    fn the_role_ladder_runs_upward() {
        let admin_only = Policy {
            role: Some(MinRole::Admin),
            ..Policy::default()
        };
        assert!(evaluate(&admin_only, USER, None).is_err());
        assert_eq!(evaluate(&admin_only, ADMIN_CALLER, None), Ok(()));
        assert_eq!(evaluate(&admin_only, OWNER_CALLER, None), Ok(()));

        let owner_only = Policy {
            role: Some(MinRole::Owner),
            ..Policy::default()
        };
        assert_eq!(
            evaluate(&owner_only, ADMIN_CALLER, None),
            Err(Denial::RoleInsufficient {
                required: MinRole::Owner
            })
        );
        assert_eq!(evaluate(&owner_only, OWNER_CALLER, None), Ok(()));
    }

    /// A public operation admits a caller who presents nothing — and
    /// admits one who presents something too, since `Public` is the
    /// weakest bar rather than a requirement to be anonymous.
    #[test]
    fn a_public_policy_admits_everyone_including_a_caller() {
        let p = Policy {
            principal: MinPrincipal::Public,
            ..Policy::default()
        };
        for who in [None, VISITOR, USER, EXTERNAL] {
            assert_eq!(evaluate(&p, who, None), Ok(()), "{who:?}");
        }
    }

    /// `visitor_allowed()` admits the ring AND a user, which it must:
    /// `/api/files/{id}` serves both.
    #[test]
    fn visitor_allowed_admits_both_the_ring_and_a_user() {
        let p = Policy {
            principal: MinPrincipal::Visitor,
            ..Policy::default()
        };
        assert_eq!(evaluate(&p, VISITOR, None), Ok(()));
        assert_eq!(evaluate(&p, USER, None), Ok(()));
        assert_eq!(
            evaluate(&p, None, None),
            Err(Denial::AuthenticationRequired)
        );
    }

    /// An absent scope set grandfathers to FULL, never empty
    /// (§ Absent means FULL, everywhere). Reading it as empty would
    /// refuse every credential minted before the claim existed — the
    /// whole population, on the day enforcement ships.
    #[test]
    fn an_absent_scope_set_grandfathers_to_full() {
        let p = Policy {
            scope: ScopeReq::Area(Scope::Security, Access::ReadWrite),
            ..Policy::default()
        };

        assert_eq!(evaluate(&p, USER, None), Ok(()), "absent must not deny");
        assert_eq!(
            evaluate(&p, USER, Some(ScopeSet::empty())),
            Err(Denial::ScopeNotGranted {
                scope: Scope::Security,
                access: Access::ReadWrite
            }),
            "an EXPLICITLY empty set is a different thing from an absent one"
        );
        assert_eq!(
            evaluate(
                &p,
                USER,
                Some(ScopeSet::empty().with(Scope::Security, Access::ReadWrite))
            ),
            Ok(())
        );
    }

    /// The mechanism by which an app password cannot change a password:
    /// its set lacks `security`. Nothing inspects the credential KIND.
    #[test]
    fn a_scope_narrowed_credential_is_refused_without_naming_its_kind() {
        let p = Policy {
            scope: ScopeReq::Area(Scope::Security, Access::ReadWrite),
            ..Policy::default()
        };
        let app_password = ScopeSet::empty()
            .with(Scope::Files, Access::ReadWrite)
            .with(Scope::Calendar, Access::ReadWrite);

        assert!(evaluate(&p, ADMIN_CALLER, Some(app_password)).is_err());

        // Read-only where write is required is the same refusal.
        let files_rw = Policy {
            scope: ScopeReq::Area(Scope::Files, Access::ReadWrite),
            ..Policy::default()
        };
        let ro = ScopeSet::empty().with(Scope::Files, Access::ReadOnly);
        assert_eq!(
            evaluate(&files_rw, USER, Some(ro)),
            Err(Denial::ScopeNotGranted {
                scope: Scope::Files,
                access: Access::ReadWrite
            })
        );
    }

    /// Principal is checked BEFORE role, so a caller with no credential is
    /// told to authenticate rather than told their role is too low. Only
    /// the message differs, but the wrong one sends an implementor hunting
    /// a role problem that does not exist.
    #[test]
    fn principal_is_reported_before_role() {
        let p = Policy {
            role: Some(MinRole::Admin),
            ..Policy::default()
        };
        assert_eq!(
            evaluate(&p, None, None),
            Err(Denial::AuthenticationRequired)
        );
    }

    /// The engine's OWN audit vocabulary: one `reason` per cause, with
    /// the role as a structured field rather than baked into the reason.
    ///
    /// Pinned because `reason` is what aggregators filter on, so these
    /// strings are a contract — "do not reword across releases"
    /// (AGENTS.md). `event` stays inside the `authz.*` domain, so a
    /// filter on that keeps working across the fold.
    #[test]
    fn each_denial_has_one_stable_reason_and_no_role_in_it() {
        assert_eq!(
            Denial::AuthenticationRequired.audit(),
            ("authz.denied", "authentication_required")
        );
        assert_eq!(
            Denial::VisitorRefused.audit(),
            ("authz.denied", "user_session_required")
        );
        // `Internal` is "not external", not a rank — its own reason.
        assert_eq!(
            Denial::RoleInsufficient {
                required: MinRole::Internal
            }
            .audit(),
            ("authz.denied", "internal_user_required")
        );
        // Admin and Owner share a reason; which floor was missed is the
        // `required` FIELD. A reason per role would multiply the
        // vocabulary for no gain, and the previous attempt to do that is
        // how `MinRole::Owner` ended up borrowing
        // `server_owner.transfer_rejected` — an event about one specific
        // operation, which a second Owner site would have emitted
        // wrongly.
        for role in [MinRole::Admin, MinRole::Owner] {
            let d = Denial::RoleInsufficient { required: role };
            assert_eq!(d.audit(), ("authz.denied", "higher_role_required"));
            assert_eq!(d.required_role(), Some(role));
        }
        assert_eq!(
            Denial::ScopeNotGranted {
                scope: Scope::Files,
                access: Access::ReadOnly
            }
            .audit(),
            ("authz.scope_denied", "scope_not_granted"),
            "§ Wire contracts fixed this one in advance"
        );

        // Every gate refusal stays in the `authz.*` domain aggregators
        // already filter, so the fold does not strand a filter.
        for d in [
            Denial::AuthenticationRequired,
            Denial::VisitorRefused,
            Denial::RoleInsufficient {
                required: MinRole::Admin,
            },
        ] {
            assert!(d.audit().0.starts_with("authz."), "{d:?}");
        }
        assert_eq!(Denial::AuthenticationRequired.required_role(), None);
    }

    /// Every operation kind must render identifiably in a boot message,
    /// because the message is the only thing an implementor gets — and
    /// three of the four surfaces have no route to grep for.
    #[test]
    fn every_op_kind_names_itself_in_a_refusal() {
        for op in [
            Op::Rest("MOVE", "/api/files/{id}"),
            Op::Dav(Scope::Files, Access::ReadOnly),
            Op::Ws("rt.subscribe"),
            Op::CollabFrame(0x02),
        ] {
            let err = compile(
                op,
                &[&[Condition::public(), Condition::role(MinRole::Admin)]],
            )
            .expect_err("unsatisfiable");
            let msg = err.to_string();
            assert!(
                msg.contains(&op.to_string()),
                "{op} missing from its own refusal: {msg}"
            );
        }
    }
}
