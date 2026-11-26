//! Scope-gate vocabulary — the types an operation declares and the engine
//! evaluates. Designed in `docs/plan/scope-gates.md`; this is PR 1 step 2,
//! and it is deliberately INERT: nothing reads a `Policy` yet, no route
//! declares a `Condition`, and no credential carries a `ScopeSet`.
//!
//! The three axes these serve, and which question each answers:
//!
//! | axis  | question | enforced |
//! |-------|----------|----------|
//! | Gate  | may this KIND of caller reach this surface? | router / this module |
//! | Scope | was the CREDENTIAL issued for this area? | this module (PR 2) |
//! | Grant | may this caller act on THIS resource? | `authorization_ports` |
//!
//! Only the first two live here. A grant names a resource and needs a DB
//! read; a gate and a scope are answered from the caller and its token, so
//! they are field reads (§ Lookup and checking cost).
//!
//! Integrity checks — JWT signature, DPoP proof, CSRF — are NOT axes and
//! are absent on purpose: the operation has no opinion about them, and
//! their failure means the request is untrustworthy rather than
//! unauthorized (§ Not axes: integrity checks).

use crate::domain::entities::user::{CallerKind, UserRole};

/// What the compiled table is keyed by. NOT a route: on three of the four
/// surfaces a route is not the unit of authorization, because each has a
/// single `any` handler and the operation is chosen by dispatch inside it
/// (§ Keyed by operation, not by route).
///
/// This is where the bugs were, not a generalisation for its own sake: the
/// AWARENESS frame kind (`36c8c6a37`) and the CardDAV door to
/// `create_address_book` were both ungated precisely because neither has a
/// route to hang a layer on.
///
/// `&'static str` throughout: every operation is known at compile time, so
/// the table needs no allocation and can be built in a `const` context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Op {
    /// REST — 279 of these, one per operation. `(method, path)`, where
    /// `path` is the axum route template, so `/api/files/{id}` and not a
    /// resolved path.
    Rest(&'static str, &'static str),
    /// A DAV operation, identified by AREA and ACCESS — not by surface
    /// and method.
    ///
    /// The method carries exactly one authorization-relevant bit: whether
    /// it reads or writes. Nothing in the gate distinguishes `PROPFIND`
    /// from `GET`, and nothing in the scope does either — only `files:ro`
    /// from `files:rw`. So keying by method would spend 12 × 3 = 36
    /// declarations to encode six facts.
    ///
    /// The AREA rather than the protocol surface, because a protocol
    /// vocabulary (`webdav`, `caldav`, …) "would name the same permissions
    /// twice" (§ Areas, not protocols). It also closes a door by
    /// construction instead of by remembering: `/webdav` and
    /// `/remote.php/dav/files` are both files-area DAV doors, so ONE
    /// declaration covers both — and "a gate on one door is a hole in the
    /// other" is how `create_address_book` went wrong.
    ///
    /// A method needing its own policy (say `MOVE` requiring elevation)
    /// would be a new variant, not a reshaping of this one — § 1: a new
    /// axis cannot change how existing declarations fold.
    Dav(Scope, Access),
    /// The `rt.*` method name inside `/api/rt/ws`.
    Ws(&'static str),
    /// The collab binary frame kind (`0x01`, `0x02`, SYNC, AWARENESS).
    CollabFrame(u8),
}

impl std::fmt::Display for Op {
    /// Used in BOOT error messages, so it has to name the operation the
    /// way an implementor would search for it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::Rest(m, p) => write!(f, "{m} {p}"),
            Op::Dav(area, access) => write!(f, "DAV {area:?}:{access:?}"),
            Op::Ws(m) => write!(f, "WS {m}"),
            Op::CollabFrame(k) => write!(f, "collab frame {k:#04x}"),
        }
    }
}

/// ONE requirement on ONE axis. An operation declares a SLICE of these,
/// which the compile step folds root→leaf into a total [`Policy`].
///
/// A slice is partial and order-free by design: a nest declares what it
/// knows, a leaf adds what it needs, and neither has to restate the other.
/// Every variant TIGHTENS except [`Condition::public`] and
/// [`Condition::visitor_allowed`], which widen — and whose conflict with a
/// role is a boot error, because `UserRole::Anonymous` is never stored in
/// `auth.users`, making "anonymous AND at least admin" unsatisfiable
/// rather than merely odd (§ 2b).
///
/// So a slice cannot express a silent contradiction. The contradictions it
/// CAN express are refused at startup, not at request time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    Role(MinRole),
    Principal(MinPrincipal),
    Scope(Scope, Access),
    // Elevated,  // with auth.sessions.elevated_at — see `Elevation` below
}

impl Condition {
    pub const fn role(r: MinRole) -> Self {
        Condition::Role(r)
    }

    /// No credential at all. The whole public surface, greppable —
    /// `/api/auth/login`, `/api/setup`, `/api/s/{token}` are public by
    /// MOUNT today (they sit outside `auth_middleware`), which is why the
    /// plan wants this declared rather than inferred from the router.
    pub const fn public() -> Self {
        Condition::Principal(MinPrincipal::Public)
    }

    /// A public-share session (the ring) suffices. Also admits a user, and
    /// must: `/api/files/{id}` serves both.
    pub const fn visitor_allowed() -> Self {
        Condition::Principal(MinPrincipal::Visitor)
    }

    pub const fn scope(s: Scope, a: Access) -> Self {
        Condition::Scope(s, a)
    }

    /// Which axis this constrains. The compile step needs it to detect a
    /// repeated axis within ONE slice (a boot error — the declaration is
    /// ambiguous) as distinct from a repeat across a nest CHAIN (legal
    /// inheritance, resolved by the fold).
    pub const fn axis(self) -> Axis {
        match self {
            Condition::Role(_) => Axis::Role,
            Condition::Principal(_) => Axis::Principal,
            Condition::Scope(..) => Axis::Scope,
        }
    }
}

/// Which of a [`Policy`]'s fields a [`Condition`] resolves. Exists so the
/// compile step can say "this slice declares Role twice" without matching
/// on payloads it does not care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Role,
    Principal,
    Scope,
}

/// What the engine EVALUATES: the declared [`Condition`]s folded
/// root→leaf. TOTAL — the compile step resolves every axis, so no
/// `Option` survives as "undecided" and the per-request check is field
/// reads rather than a search.
///
/// These are properties of the CALLER, never client-requestable, with one
/// exception that is marked as such: `scope` is checked against the
/// credential's [`ScopeSet`], which the client asked for at mint time and
/// cannot change afterwards (§ Three axes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// `None` = any authenticated principal — NOT "no requirement", since
    /// `principal` still has to be satisfied. Folds by MAXIMUM; a
    /// declaration that would widen an inherited role is a boot error.
    pub role: Option<MinRole>,

    /// The least privileged principal admitted. NOT an `Option`: unlike
    /// role there is no "any" answer, because every operation admits some
    /// least principal. The default being `User` is what makes
    /// "no declaration ⇒ authentication required" FALL OUT of the type
    /// rather than be enforced by a rule someone must remember
    /// (§ Authentication by default).
    pub principal: MinPrincipal,

    /// Resolved scope requirement. Its own AXIS rather than a Gate-axis
    /// condition, because it is checked against the credential instead of
    /// the user row — a distinction that matters the moment the same user
    /// holds both a session and an app password.
    pub scope: ScopeReq,
    // pub elevation: Option<Elevation>,  // with auth.sessions.elevated_at
}

impl Default for Policy {
    /// The policy of an operation that declares NOTHING: an authenticated
    /// user, no role floor, and an UNSATISFIABLE scope.
    ///
    /// The two axes reach safety by opposite routes, which is the design
    /// rather than an inconsistency. `principal` defaults to a real
    /// requirement, so a forgotten declaration fails closed with a 401.
    /// `scope` has no such candidate — "no scope required" would be
    /// fail-open — so it defaults to `Undefined`, which spec generation
    /// refuses. See [`ScopeReq`].
    fn default() -> Self {
        Self {
            role: None,
            principal: MinPrincipal::User,
            scope: ScopeReq::Undefined,
        }
    }
}

/// What an operation requires on the SCOPE axis — a TOTAL enum, because
/// the absence of a requirement must be WRITTEN rather than fallen into.
///
/// The gate axis can have a safe default: `MinPrincipal::User` is itself a
/// requirement, and it happens to be the common case. Scope has no such
/// candidate value. The only plausible default is "no scope required",
/// which is fail-open — and that is precisely the live bug this axis
/// exists to close, where a credential restricted to `files` reaches a
/// surface nobody classified. So instead of choosing a default, the
/// absence is made unrepresentable:
///
/// - [`ScopeReq::Undefined`] is the default and is UNSATISFIABLE. Spec
///   generation refuses it, so an unclassified operation fails CI rather
///   than shipping reachable. Not a boot check: the operation list comes
///   from the specs while a feature-gated router only exists when its
///   feature is on, so a stock deployment with `enable_photos=false`
///   would refuse to start over a feature the operator turned off.
/// - [`ScopeReq::None`] is "no scope required", DECLARED — greppable, and
///   written the way `public()` is written on the principal axis.
/// - [`ScopeReq::Area`] names the area and the access.
///
/// **Not `Option<(Scope, Access)>` plus a `Scope::None` variant**, which
/// would be the `CallerKind` mistake repeated: `Option::None` and
/// `Scope::None` would both be reachable and mean different things.
/// Naming all three states is what makes the wrong one unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScopeReq {
    /// Nothing declared. Unsatisfiable — see the type doc. `#[default]`
    /// rather than a hand-written `impl`, so the unsatisfiable state is
    /// visibly the default at the point the variants are declared.
    #[default]
    Undefined,
    /// No scope required, stated deliberately.
    None,
    /// This area, at least this access.
    Area(Scope, Access),
}

impl ScopeReq {
    /// The area and access, when one is required.
    ///
    /// `Undefined` and `None` both yield `Option::None` — they differ in
    /// what the COMPILE step does about them, not in what a satisfied
    /// request needs, so a caller checking "is there an area to verify?"
    /// treats them alike.
    #[must_use]
    pub fn area(self) -> Option<(Scope, Access)> {
        match self {
            Self::Area(s, a) => Some((s, a)),
            Self::Undefined | Self::None => Option::None,
        }
    }
}

/// From the USER row, re-read live per request (the 30 s flags cache, not
/// the JWT claim — a demoted admin must lose access without re-logging).
///
/// ORDERED: each variant implies the one before it, which is exactly what
/// makes the fold a `max` rather than a merge.
///
/// `Admin ⇒ Internal` is PROVABLE rather than conventional: the
/// `users_external_not_privileged` CHECK forbids
/// `is_external AND role <> 'user'`, so an admin cannot be external.
/// That is what makes dropping `Internal` beside `Admin` sound, and
/// `admin_implies_internal_depends_on_a_db_check` below pins the
/// dependency — if that constraint is ever dropped, this reduction
/// silently starts admitting external admins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MinRole {
    /// Not `is_external`. The weakest of the three, since every internal
    /// role satisfies it.
    Internal,
    /// `role.at_least(Admin)` — Owner satisfies it too.
    Admin,
    /// `UserRole::Owner`. NOT `storage.grant_role`, which is a different
    /// enum with overlapping names (§ GrantRole vs DriveRole).
    Owner,
}

impl MinRole {
    /// Does this caller clear the floor?
    ///
    /// Takes a [`CallerKind`] rather than a `UserRole` on purpose: an
    /// external user's role IS `user`, so a `UserRole` alone cannot answer
    /// `Internal` — which is the whole reason the caller carries a kind.
    pub fn satisfied_by(self, kind: CallerKind) -> bool {
        match self {
            MinRole::Internal => kind.is_internal(),
            MinRole::Admin => kind.is_internal() && kind.role().at_least(UserRole::Admin),
            MinRole::Owner => kind.is_internal() && kind.role().at_least(UserRole::Owner),
        }
    }
}

/// The LEAST privileged principal an operation admits. THREE states, not
/// two — "no credential at all" and "a share session" are different things
/// today and were both called *anonymous*, which is how a visitor ended up
/// having to be labelled `is_external: true` before `CallerKind` existed:
///
///   * `Public`  — no credential. Public by MOUNT today, outside
///     `auth_middleware`.
///   * `Visitor` — a public-share session exists, carrying a `visitor_id`
///     that matches no `auth.users` row. Confined to the operations that
///     declare `visitor_allowed()` — seven GETs, and nothing else.
///   * `User`    — an authenticated user session. The DEFAULT.
///
/// ORDERED ascending in strictness so the fold takes the MAX, like
/// [`MinRole`]. Admitting a visitor therefore also admits a user, which it
/// must, since `/api/files/{id}` serves both.
///
/// The password challenge on a protected share is NOT a fourth state: it
/// runs before the ring is minted, so by the time any operation sees the
/// caller there is simply a ring. No operation has an opinion about
/// whether the share had a password — that is an admission-phase concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum MinPrincipal {
    Public,
    Visitor,
    #[default]
    User,
}

impl MinPrincipal {
    /// Does this caller clear the bar? `None` means no credential was
    /// presented at all, which only `Public` admits.
    ///
    /// Absence of a caller IS public, which is why there is no
    /// `CallerKind::Public` variant to compare against.
    pub fn satisfied_by(self, kind: Option<CallerKind>) -> bool {
        match (self, kind) {
            // No credential: only a public operation admits it.
            (MinPrincipal::Public, None) => true,
            (_, None) => false,
            // A credential always clears Public and Visitor; `User`
            // additionally refuses a visitor, who has no user row.
            (MinPrincipal::Public | MinPrincipal::Visitor, Some(_)) => true,
            (MinPrincipal::User, Some(k)) => !k.is_visitor(),
        }
    }
}

/// What the CREDENTIAL was issued for. Client-requestable and frozen at
/// mint: `effective = (requested ?? type_default) ∩ type_maximum`, which
/// is why credential KIND is a mint-time input and not a request-time
/// condition.
///
/// NOT named `Resource`, which the GRANT axis owns — and whose variants
/// carry an id, so `Resource::Calendar(Uuid)` is *one* calendar where
/// `Scope::Calendar` is the calendar *area*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    Files,
    Calendar,
    Contacts,
    Photos,
    Music,
    Sharing,
    /// Profile and preferences — Google's "Account" tab.
    Account,
    /// Password, identities, credentials — Google's "Security" tab.
    /// Absent from every non-interactive ceiling, which is the whole
    /// mechanism by which an app password cannot change a password: its
    /// `ScopeSet` lacks `security`, not because something noticed it was
    /// an app password.
    Security,
    // Mail,  // reserved — the parser REJECTS it until a mailbox exists
}

impl Scope {
    /// Every scope, for building a ceiling. Hand-written rather than
    /// derived so adding a variant without deciding its place in a
    /// ceiling is a compile error in `ALL`, not a silently-omitted scope.
    pub const ALL: [Scope; 8] = [
        Scope::Files,
        Scope::Calendar,
        Scope::Contacts,
        Scope::Photos,
        Scope::Music,
        Scope::Sharing,
        Scope::Account,
        Scope::Security,
    ];

    /// Bit position in a [`ScopeSet`] mask.
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

/// Two bitmasks rather than a map: unlike the role condition this is on
/// paths with no DB round trip to hide behind, so a `HashMap` lookup and
/// its allocation would be the dominant cost of the check.
///
/// Eight scopes fit a `u8`, and **`rw` implies `ro` by construction** —
/// the invariant `writable ⊆ readable` is maintained on insert rather than
/// asserted on read, so no caller can observe a set that grants write
/// without read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScopeSet {
    readable: u8,
    /// Always a subset of `readable` — see the type doc.
    writable: u8,
}

impl ScopeSet {
    /// The empty set: grants nothing. NOT the grandfathering default —
    /// a credential minted before scopes existed gets [`ScopeSet::full`],
    /// because "absent means FULL, everywhere" (§ Rollout). Reading an
    /// absent claim as empty would 401 every token in flight.
    pub const fn empty() -> Self {
        Self {
            readable: 0,
            writable: 0,
        }
    }

    /// Every scope, read-write. The grandfathering value for a credential
    /// with no scope information.
    pub const fn full() -> Self {
        Self {
            readable: u8::MAX,
            writable: u8::MAX,
        }
    }

    /// Grant `scope` at `access`, maintaining `writable ⊆ readable`.
    pub fn insert(&mut self, scope: Scope, access: Access) {
        self.readable |= scope.bit();
        if access == Access::ReadWrite {
            self.writable |= scope.bit();
        }
    }

    #[must_use]
    pub fn with(mut self, scope: Scope, access: Access) -> Self {
        self.insert(scope, access);
        self
    }

    /// Does this set permit `scope` at `access`?
    pub const fn allows(self, scope: Scope, access: Access) -> bool {
        let bit = scope.bit();
        match access {
            Access::ReadOnly => self.readable & bit != 0,
            Access::ReadWrite => self.writable & bit != 0,
        }
    }
}

// Elevation is NOT a v1 axis — there is no elevation state in the schema
// yet. Designed in `session-elevation.md`, which adds
// `auth.sessions.elevated_at`. Its `CurrentCaller` prerequisite is DONE:
// the caller carries `token: PresentedToken`, so this will read
// `caller.token.session_id` without a second lookup.
//
// DPoP binding is deliberately NOT here: it is an integrity check, so no
// operation has an opinion about it (§ Not axes).
//
// pub enum Elevation { Required }

#[cfg(test)]
mod tests {
    use super::*;

    /// The default policy is what an operation gets by FORGETTING to
    /// declare one, so it has to be the safe end of every axis. If this
    /// ever flips to `Public`, every undeclared operation silently opens.
    #[test]
    fn default_policy_demands_an_authenticated_user() {
        let p = Policy::default();
        assert_eq!(p.principal, MinPrincipal::User);
        assert_eq!(p.role, None);
        assert_eq!(p.scope, ScopeReq::Undefined);
        assert!(!p.principal.satisfied_by(None));
    }

    /// Both ladders fold by `max`, which is only sound while they are
    /// ordered weakest-first. A reordered variant list would make the
    /// fold silently pick the WEAKER of two requirements.
    #[test]
    fn the_ladders_are_ordered_weakest_first() {
        assert!(MinRole::Internal < MinRole::Admin);
        assert!(MinRole::Admin < MinRole::Owner);
        assert!(MinPrincipal::Public < MinPrincipal::Visitor);
        assert!(MinPrincipal::Visitor < MinPrincipal::User);

        // What the fold will do, stated as the property it relies on.
        assert_eq!(MinRole::Internal.max(MinRole::Owner), MinRole::Owner);
        assert_eq!(
            MinPrincipal::Public.max(MinPrincipal::User),
            MinPrincipal::User
        );
    }

    /// `MinRole::Internal` cannot be answered from a `UserRole` alone —
    /// an external user's role IS `user`. This is the test that would
    /// have caught collapsing `satisfied_by` to a role comparison.
    #[test]
    fn internal_refuses_an_external_whose_role_is_user() {
        let external = CallerKind::External;
        let internal = CallerKind::Internal(UserRole::User);

        assert_eq!(external.role(), internal.role(), "same role, by design");
        assert!(!MinRole::Internal.satisfied_by(external));
        assert!(MinRole::Internal.satisfied_by(internal));
    }

    /// `Admin ⇒ Internal` is a claim about the DATABASE, not about this
    /// enum: `users_external_not_privileged` forbids an external admin,
    /// which is what lets `MinRole::Admin` skip asking about externality
    /// separately. Pinned because dropping that CHECK would make the
    /// reduction wrong in a way no type would catch.
    ///
    /// The migration is `20261027000000_external_not_privileged.sql`.
    #[test]
    fn admin_implies_internal_depends_on_a_db_check() {
        // The state this asserts is unreachable is `External` holding a
        // privileged role — which `CallerKind` makes unrepresentable, and
        // the CHECK makes unstorable. Both halves must hold.
        assert_eq!(CallerKind::External.role(), UserRole::User);
        assert!(!MinRole::Admin.satisfied_by(CallerKind::External));
        assert!(MinRole::Admin.satisfied_by(CallerKind::Internal(UserRole::Admin)));
        assert!(
            MinRole::Admin.satisfied_by(CallerKind::Internal(UserRole::Owner)),
            "Owner outranks Admin — six sites once denied the owner by \
             comparing to \"admin\" literally"
        );
    }

    /// A visitor is admitted by `Visitor` and refused by `User`; absence
    /// of a caller is admitted ONLY by `Public`. The three-state ladder
    /// exists for exactly this distinction, which a bool could not make.
    #[test]
    fn the_three_principal_states_are_distinguishable() {
        let visitor = Some(CallerKind::Visitor);
        let user = Some(CallerKind::Internal(UserRole::User));

        assert!(MinPrincipal::Public.satisfied_by(None));
        assert!(!MinPrincipal::Visitor.satisfied_by(None));
        assert!(!MinPrincipal::User.satisfied_by(None));

        assert!(MinPrincipal::Visitor.satisfied_by(visitor));
        assert!(!MinPrincipal::User.satisfied_by(visitor));

        // Admitting a visitor must also admit a user — `/api/files/{id}`
        // serves both, so this is a requirement and not a nicety.
        assert!(MinPrincipal::Visitor.satisfied_by(user));
        assert!(MinPrincipal::User.satisfied_by(user));
    }

    /// `rw ⊆ ro` is maintained on INSERT, so no reader can observe a set
    /// that grants write without read — including when only `ReadWrite`
    /// was ever inserted.
    #[test]
    fn write_access_always_implies_read() {
        let s = ScopeSet::empty().with(Scope::Files, Access::ReadWrite);
        assert!(s.allows(Scope::Files, Access::ReadWrite));
        assert!(s.allows(Scope::Files, Access::ReadOnly));

        let ro = ScopeSet::empty().with(Scope::Calendar, Access::ReadOnly);
        assert!(ro.allows(Scope::Calendar, Access::ReadOnly));
        assert!(
            !ro.allows(Scope::Calendar, Access::ReadWrite),
            "read-only must not widen to write"
        );

        // Untouched scopes grant nothing.
        assert!(!s.allows(Scope::Security, Access::ReadOnly));
    }

    /// Every scope must fit the `u8` masks, and each must occupy its OWN
    /// bit. A ninth variant added without widening the mask would alias
    /// onto `Files` and silently grant it.
    #[test]
    fn every_scope_has_a_distinct_bit_within_u8() {
        assert_eq!(Scope::ALL.len(), 8, "a u8 mask holds exactly 8 scopes");

        let mut seen = ScopeSet::empty();
        for s in Scope::ALL {
            assert!(
                !seen.allows(s, Access::ReadOnly),
                "{s:?} aliases onto a bit already taken"
            );
            seen.insert(s, Access::ReadOnly);
        }
        assert_eq!(seen.readable, u8::MAX, "all 8 bits used, none spare");
    }

    /// `full()` is the GRANDFATHERING value, not `empty()`. Reading an
    /// absent scope claim as empty would 401 every credential minted
    /// before the claim existed (§ Absent means FULL, everywhere).
    #[test]
    fn full_is_the_grandfathering_default_not_empty() {
        let grandfathered = ScopeSet::full();
        for s in Scope::ALL {
            assert!(grandfathered.allows(s, Access::ReadWrite), "{s:?}");
        }
        assert!(!ScopeSet::empty().allows(Scope::Files, Access::ReadOnly));
    }

    /// The compile step tells a repeated axis within ONE slice (a boot
    /// error) from a repeat across a nest CHAIN (legal inheritance). It
    /// needs the axis without caring about the payload.
    #[test]
    fn axis_identifies_the_field_a_condition_resolves() {
        assert_eq!(Condition::role(MinRole::Admin).axis(), Axis::Role);
        assert_eq!(Condition::public().axis(), Axis::Principal);
        assert_eq!(Condition::visitor_allowed().axis(), Axis::Principal);
        assert_eq!(
            Condition::scope(Scope::Security, Access::ReadWrite).axis(),
            Axis::Scope
        );

        // `public()` and `visitor_allowed()` share an axis, which is why
        // declaring both in one slice is ambiguous rather than additive.
        assert_eq!(
            Condition::public().axis(),
            Condition::visitor_allowed().axis()
        );
    }
}
