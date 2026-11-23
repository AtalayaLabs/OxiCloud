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
    Access, Axis, Condition, MinPrincipal, MinRole, Op, Policy, Scope,
};

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
    scope: Option<(Scope, Access)>,
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
                Condition::Scope(s, a) => match acc.scope {
                    // Same area: tighten the access. `ReadWrite` wins,
                    // which `Access`'s ordering already encodes.
                    Some((prev, prev_access)) if prev == s => {
                        acc.scope = Some((s, prev_access.max(a)));
                    }
                    Some((first, _)) => {
                        return Err(PolicyError::ConflictingScopes {
                            op,
                            first,
                            second: s,
                        });
                    }
                    None => acc.scope = Some((s, a)),
                },
            }
        }
    }

    // Defaults applied now, not during the fold — see the module doc.
    let policy = Policy {
        role: acc.role,
        principal: acc.principal.unwrap_or_default(),
        scope: acc.scope,
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
        if let Some((scope, _)) = policy.scope {
            return Err(PolicyError::PublicWithScope {
                op,
                principal: policy.principal,
                scope,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADMIN: Op = Op::Rest("GET", "/api/admin/sessions");

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
        assert_eq!(p.scope, Some((Scope::Security, Access::ReadWrite)));
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
        assert_eq!(p.scope, Some((Scope::Files, Access::ReadOnly)));
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

        assert_eq!(p.scope, Some((Scope::Security, Access::ReadWrite)));
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

    /// Every operation kind must render identifiably in a boot message,
    /// because the message is the only thing an implementor gets — and
    /// three of the four surfaces have no route to grep for.
    #[test]
    fn every_op_kind_names_itself_in_a_refusal() {
        for op in [
            Op::Rest("MOVE", "/api/files/{id}"),
            Op::Dav("PROPFIND"),
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
