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
    /// This area, with the ACCESS derived from the operation's method at
    /// compile time — see [`Condition::scope_by_verb`].
    ScopeByVerb(Scope),
    /// No scope required, declared. Distinct from declaring nothing,
    /// which leaves [`ScopeReq::Undefined`].
    Scopeless,
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

    /// This area at this access, stated.
    pub const fn scope(s: Scope, a: Access) -> Self {
        Condition::Scope(s, a)
    }

    /// This area, with the access taken from the operation's METHOD:
    /// `ReadOnly` for a read method, `ReadWrite` otherwise.
    ///
    /// What a prefix declares, because it lets ONE declaration cover a
    /// subtree whose verbs differ — ten declarations instead of thirty.
    /// Resolved during compile, where `Op::Rest` carries the method, so
    /// the compiled `Policy` holds a plain `Access` and nothing is
    /// inferred while serving a request.
    ///
    /// **The verb is a heuristic, and it lies in both directions.** Where
    /// the method does not describe the EFFECT, state the access with
    /// [`Condition::scope`] instead:
    ///
    /// - a credential-minting GET reads as harmless exactly when it is
    ///   not, and the FIRST question is whether the thing it mints can be
    ///   narrowed. `GET /api/wopi/editor-url` hands back a WOPI token
    ///   usable for `POST /wopi/files/{id}/contents`; it was declared
    ///   `files:rw` for exactly that reason, until the token's
    ///   `can_write` bit started intersecting the caller's scope — at
    ///   which point `files:ro` became correct, because a read-only
    ///   credential now receives a token that cannot write. **Narrow what
    ///   the mint HANDS OUT before tightening who may reach it**: the
    ///   override costs a legitimate capability (opening a document
    ///   read-only) while narrowing costs nothing. State the access
    ///   explicitly only where the effect genuinely cannot be bounded;
    /// - a read posted for its body — `POST /api/batch/files/get`,
    ///   `/api/batch/download`, `/api/dedup/check-batch` — would demand
    ///   `rw` and lock a read-only credential out of a read.
    pub const fn scope_by_verb(s: Scope) -> Self {
        Condition::ScopeByVerb(s)
    }

    /// No scope required, DECLARED — the way [`Condition::public`] is
    /// written on the principal axis rather than fallen into.
    ///
    /// Not the same as declaring nothing: that leaves
    /// [`ScopeReq::Undefined`], which spec generation refuses. This is how
    /// an operation says it needs no area — `/api/config`,
    /// `/api/auth/logout`, the login flows.
    pub const fn scopeless() -> Self {
        Condition::Scopeless
    }

    /// Which axis this constrains. The compile step needs it to detect a
    /// repeated axis within ONE slice (a boot error — the declaration is
    /// ambiguous) as distinct from a repeat across a nest CHAIN (legal
    /// inheritance, resolved by the fold).
    pub const fn axis(self) -> Axis {
        match self {
            Condition::Role(_) => Axis::Role,
            Condition::Principal(_) => Axis::Principal,
            Condition::Scope(..) | Condition::ScopeByVerb(_) | Condition::Scopeless => Axis::Scope,
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

impl Op {
    /// The HTTP method, for the operations that have one.
    ///
    /// `Op::Dav` carries an [`Access`] directly rather than a method, and
    /// the WS/collab operations have no method at all — so they report the
    /// WRITE-implying default. That is the safe direction, and it is also
    /// moot: nothing resolves `scope_by_verb` for them, because a DAV
    /// declaration states its access.
    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            Self::Rest(m, _) => m,
            _ => "",
        }
    }
}

impl Access {
    /// The access an HTTP METHOD implies: `ReadOnly` for the read
    /// methods, `ReadWrite` for everything else.
    ///
    /// **Fail-closed by omission.** An unlisted or future method is a
    /// write, so it demands the stronger scope. The opposite default
    /// would let a method nobody classified through on a read-only
    /// credential.
    ///
    /// `LOCK` and `UNLOCK` are therefore writes — they mutate lock state
    /// — which is the classification most likely to be got wrong by
    /// reading the verb alone.
    ///
    /// Serves BOTH axes of the engine: it resolves
    /// [`Condition::scope_by_verb`] for REST during compile, and it
    /// classifies a DAV request's method at the gate. The DAV side had its
    /// own copy of this list until the two were found to be the same five
    /// methods.
    /// The wire token: `ro` or `rw`.
    ///
    /// One spelling, shared by [`ScopeSet::to_wire`] and the audit lines,
    /// so a log field and a scope string never disagree about what this
    /// access is called.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }

    #[must_use]
    pub const fn from_method(method: &str) -> Self {
        match method.as_bytes() {
            b"GET" | b"HEAD" | b"OPTIONS" | b"PROPFIND" | b"REPORT" | b"SEARCH" => Self::ReadOnly,
            _ => Self::ReadWrite,
        }
    }
}

/// Serialised as the wire string, so a `ScopeSet` in a log line or a
/// debug dump reads as `oxicloud:files:rw,oxicloud:calendar:ro` rather
/// than as two integers nobody can decode by eye.
impl serde::Serialize for ScopeSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_wire())
    }
}

impl<'de> serde::Deserialize<'de> for ScopeSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        // The EMPTY STRING is the wire form of the empty set, so it must
        // round-trip — distinct from `ScopeSet::parse`, which rejects an
        // empty input because there a client sent a `scope` parameter
        // with nothing in it. Same text, two entry points, two correct
        // answers: here it is our own output being read back, there it is
        // a client bug.
        if raw.trim().is_empty() {
            return Ok(Self::empty());
        }
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// What KIND of credential a scope set is being minted for.
///
/// Credential kind is a MINT-time input, not a request-time axis. Every
/// ceiling below is applied once, when the credential is created — so
/// nothing at request time has to notice what kind of credential it is
/// holding, and the two cannot drift. § Credential-kind gating argues
/// this at length; the short version is that a request-time condition
/// would be a second implementation of the same decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// Basic-auth credential for DAV and NC clients — DAVx⁵,
    /// Thunderbird, rclone, NC desktop.
    AppPassword,
    /// RFC 8628 device grant, for clients that cannot open a browser.
    DeviceFlow,
    /// The browser's own session. Never requests scopes.
    InteractiveSession,
}

impl CredentialKind {
    /// What this kind gets when it asks for nothing.
    ///
    /// An omitted scope does NOT mean every scope — that is the
    /// fail-open shape this axis exists to remove. RFC 6749 §3.3 requires
    /// an omitted `scope` to resolve to a PRE-DEFINED default or fail the
    /// request, and this is that default.
    #[must_use]
    pub fn default_scopes(self) -> ScopeSet {
        match self {
            // What a sync client needs: NC desktop wants files, DAVx⁵
            // wants calendars and contacts, and every NC client reads the
            // roster — the share dialog's recipient picker and the OCS
            // profile lookup both do.
            //
            // `sharing` is READ-ONLY here, and it is the one area whose
            // access differs from the rest. Least privilege decides it:
            // all three OCS routes that declare `sharing` are GETs, so
            // `:ro` satisfies every door that exists, and widening later
            // is the safe direction while narrowing breaks clients in the
            // field.
            //
            // It was withheld entirely at first, on the reasoning that
            // `sharing` enumerates the user roster. That line was drawn
            // in the wrong place, and `nc_admin_views_other_user.hurl`
            // said so: the ceiling exists to prevent ESCALATION, not to
            // withhold capability the human already has. `security` and
            // `admin` are escalation — minting another credential,
            // administering the instance. Reading the roster is not: the
            // user behind this credential can do it from the web UI, so
            // withholding it removed nothing from them and only broke
            // NextCloud compatibility.
            Self::AppPassword | Self::DeviceFlow => ScopeSet::empty()
                .with(Scope::Files, Access::ReadWrite)
                .with(Scope::Calendar, Access::ReadWrite)
                .with(Scope::Contacts, Access::ReadWrite)
                .with(Scope::Sharing, Access::ReadOnly),
            Self::InteractiveSession => ScopeSet::full(),
        }
    }

    /// The most this kind may EVER hold, however much it asks for.
    ///
    /// An ALLOWLIST, not a denylist: a credential kind gets only what its
    /// arm names. A kind added later without thought about `security`
    /// therefore gets it withheld, which is the safe direction to be
    /// careless in.
    #[must_use]
    pub fn ceiling(self) -> ScopeSet {
        match self {
            // No `security`, so an app password cannot mint or revoke
            // another one — not because anything notices it is an app
            // password, but because its set lacks the area those routes
            // require. No `admin`: a role is read from the USER row, so
            // an admin's app password carries the admin role and only the
            // scope axis stops it administering the instance. No
            // `account` — avatars are scopeless, so nothing needs it.
            // `sharing` IS granted, at `:ro`; see `default_scopes`.
            Self::AppPassword => Self::AppPassword.default_scopes(),
            // Same, plus `account`: a device-flow client is interactive
            // enough to consent to profile access on the verify screen.
            Self::DeviceFlow => Self::DeviceFlow
                .default_scopes()
                .with(Scope::Account, Access::ReadWrite),
            Self::InteractiveSession => ScopeSet::full(),
        }
    }

    /// The set a credential of this kind actually gets:
    ///
    /// ```text
    /// effective = (requested ?? default) ∩ ceiling
    /// ```
    ///
    /// Both halves bounded, so neither an omitted scope nor a greedy
    /// request can escalate. A request for more than the ceiling is
    /// SILENTLY narrowed rather than refused, which is RFC 6749 §3.3's
    /// prescribed behaviour ("the authorization server MAY fully or
    /// partially ignore the scope requested") and is also the only option
    /// that lets a client ask for a superset without having to know each
    /// kind's ceiling.
    ///
    /// A MALFORMED request is still an error — narrowing applies to what
    /// a client may have, never to what it managed to say.
    pub fn effective(self, requested: Option<&str>) -> Result<ScopeSet, ScopeParseError> {
        let asked = match requested {
            Some(raw) => ScopeSet::parse(raw)?,
            None => self.default_scopes(),
        };
        Ok(asked.intersect(self.ceiling()))
    }
}

/// Why a wire scope string was refused.
///
/// Every variant is a REFUSAL, never a silent narrowing. RFC 6749 §3.3
/// allows a server to ignore unrecognised scope values; this does not,
/// because the failure mode is a client that believes it holds access it
/// does not. A partial parse that resolves the one token it recognised is
/// worse than a clean error — see § Wire format on `scope=openid profile
/// email`, where `profile` would have matched with the opposite meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeParseError {
    /// Present but empty. NOT treated as "use the default": silently
    /// defaulting hides a client bug, and a zero-scope credential would
    /// be useless anyway.
    Empty,
    /// No `oxicloud:` prefix. These values live in the OAuth scope
    /// namespace, where unprefixed names are reserved — so an unprefixed
    /// token is not an OxiCloud resource at all.
    Unprefixed { token: String },
    /// Prefixed, but the area is not one of ours.
    UnknownArea { name: String },
    /// A name RESERVED for a resource that does not exist yet.
    Reserved { name: String },
    /// The access qualifier was neither `ro` nor `rw`.
    UnknownAccess { token: String },
}

impl std::fmt::Display for ScopeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(
                f,
                "scope is present but empty — omit it to take the default, \
                 or name the areas wanted"
            ),
            Self::Unprefixed { token } => write!(
                f,
                "`{token}` has no `oxicloud:` prefix — OxiCloud scopes are \
                 namespaced so they cannot collide with OIDC's reserved names"
            ),
            Self::UnknownArea { name } => {
                write!(f, "`{name}` is not an OxiCloud area")
            }
            Self::Reserved { name } => write!(
                f,
                "`{name}` is reserved for a resource that does not exist yet"
            ),
            Self::UnknownAccess { token } => write!(
                f,
                "`{token}` ends in an unknown access — use `:ro`, `:rw`, or \
                 omit it for read-write"
            ),
        }
    }
}

impl std::error::Error for ScopeParseError {}

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
    /// These areas, each at least the given access. A SET, because a
    /// leaf cannot change an inherited requirement — only append to it,
    /// so an operation under two declarations requires both.
    Areas(ScopeSet),
}

impl ScopeReq {
    /// The areas required, if any.
    ///
    /// `Undefined` and `None` both yield `Option::None` — they differ in
    /// what the COMPILE step does about them, not in what a satisfied
    /// request needs, so a caller checking "is there anything to verify?"
    /// treats them alike.
    #[must_use]
    pub fn areas(self) -> Option<ScopeSet> {
        match self {
            Self::Areas(set) => Some(set),
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
    /// The BIOMETRIC face index — `/api/people/*`, and nothing else.
    ///
    /// The one media surface with an area of its own. Photos and music do
    /// NOT have scopes: they are ways to DISPLAY files, so withholding
    /// them withholds nothing a `files` credential cannot already read
    /// through `/api/files` (EXIF coordinates live in the bytes). A face
    /// index is different — pixel analysis produces data the bytes do not
    /// contain, and `merge`/`recluster` mutate it. So the app-password
    /// ceiling withholds biometrics by construction, for the same reason
    /// `enable_faces` is opt-in.
    Faces,
    /// The instance CONTROL surface — `/api/admin/*` and the `/api/groups`
    /// mutations. Not a data area, and that is why it exists.
    ///
    /// `MinRole::Admin` cannot stand in for it: a role is read from the
    /// USER row, so an admin's app password carries the admin role and a
    /// credential created for DAVx⁵ could administer the instance.
    /// Withheld from every non-interactive ceiling.
    Admin,
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
    /// The wire and display name, WITHOUT the `oxicloud:` prefix.
    ///
    /// One table, used by the parser, the dump and the OpenAPI marker —
    /// `interfaces::api::area_name` delegates here rather than keeping a
    /// second copy. A name that appears twice is a name that can disagree
    /// with itself.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Calendar => "calendar",
            Self::Contacts => "contacts",
            Self::Faces => "faces",
            Self::Admin => "admin",
            Self::Sharing => "sharing",
            Self::Account => "account",
            Self::Security => "security",
        }
    }

    /// The PRE-AREA protocol vocabulary, accepted for compatibility.
    ///
    /// `webdav` / `caldav` / `carddav` were the whole scope vocabulary
    /// before areas, and the old default — `webdav,caldav,carddav` —
    /// appeared in every app-password creation response. So any
    /// automation that read `scopes` back and echoed it later sends
    /// these, and two of this repo's own shell suites hardcoded
    /// `"webdav"`, which is fair evidence of what a human reading the
    /// API writes.
    ///
    /// **This does not weaken the namespacing argument.** That argument
    /// is about names we do NOT own: `profile` is reserved by OIDC and
    /// would match an OxiCloud area with the opposite meaning, so it
    /// must keep failing. These three we own outright, so aliasing
    /// exactly them costs nothing — unknown and reserved names still
    /// reject.
    ///
    /// Unqualified only. `webdav:ro` was never valid, so it is not
    /// invented here; the legacy vocabulary had no access dimension and
    /// every one of these tokens meant full access.
    ///
    /// Deprecated: remove at NEXT-MAJOR, once the
    /// `authz.scope_legacy_vocabulary` warning has gone quiet in the
    /// field.
    #[must_use]
    pub const fn from_legacy_protocol(token: &str) -> Option<(Self, Access)> {
        match token.as_bytes() {
            b"webdav" => Some((Self::Files, Access::ReadWrite)),
            b"caldav" => Some((Self::Calendar, Access::ReadWrite)),
            b"carddav" => Some((Self::Contacts, Access::ReadWrite)),
            _ => None,
        }
    }

    /// The area a wire name denotes, or an error naming why not.
    ///
    /// `mail` is REJECTED rather than unknown, and the distinction is
    /// deliberate: a reserved name that parses but grants nothing would
    /// let a client believe it had asked for something. There is no
    /// mailbox, so asking for one is a client bug worth reporting.
    pub fn parse(name: &str) -> Result<Self, ScopeParseError> {
        match name {
            "files" => Ok(Self::Files),
            "calendar" => Ok(Self::Calendar),
            "contacts" => Ok(Self::Contacts),
            "faces" => Ok(Self::Faces),
            "admin" => Ok(Self::Admin),
            "sharing" => Ok(Self::Sharing),
            "account" => Ok(Self::Account),
            "security" => Ok(Self::Security),
            "mail" => Err(ScopeParseError::Reserved {
                name: name.to_owned(),
            }),
            _ => Err(ScopeParseError::UnknownArea {
                name: name.to_owned(),
            }),
        }
    }

    /// Every scope, for building a ceiling. Hand-written rather than
    /// derived so adding a variant without deciding its place in a
    /// ceiling is a compile error in `ALL`, not a silently-omitted scope.
    pub const ALL: [Scope; 8] = [
        Scope::Files,
        Scope::Calendar,
        Scope::Contacts,
        Scope::Faces,
        Scope::Admin,
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
    /// What a public-share session (the RING) may ever carry: `files:ro`
    /// and nothing else.
    ///
    /// Read-only because a share IS read-only — `domain::entities::Share`
    /// has `max_downloads` and `download_count` and no upload or write
    /// permission at all, so there is no such thing as a write-capable
    /// ring to provide for.
    ///
    /// `files` and nothing else because a ring holds a TOKEN for one
    /// shared item; it has no account behind it, so `account`,
    /// `security`, `admin`, `sharing`, `calendar`, `contacts` and `faces`
    /// are not merely ungranted but unreachable.
    ///
    /// The compile step uses this to refuse a visitor-admitting operation
    /// that requires more — which turns "a ring cannot satisfy this" from
    /// a runtime 403 into a generation failure.
    pub const fn ring() -> Self {
        Self {
            readable: Scope::Files.bit(),
            writable: 0,
        }
    }

    /// Every (area, access) this set carries, weakest-first by area.
    ///
    /// For rendering a requirement — the dump and the spec both publish
    /// the resolved set — and for naming the first unmet one in a denial.
    pub fn iter(self) -> impl Iterator<Item = (Scope, Access)> {
        Scope::ALL.into_iter().filter_map(move |s| {
            if self.allows(s, Access::ReadWrite) {
                Some((s, Access::ReadWrite))
            } else if self.allows(s, Access::ReadOnly) {
                Some((s, Access::ReadOnly))
            } else {
                None
            }
        })
    }

    /// Parse a wire scope string into a set.
    ///
    /// ```text
    /// oxicloud:files:rw oxicloud:calendar:ro     // RFC 6749 §3.3 form
    /// oxicloud:files:rw,oxicloud:calendar:ro     // what our clients send
    /// ```
    ///
    /// **Both delimiters**, because the two disagree today and the client
    /// is not at fault: RFC 6749 specifies space-delimited, and
    /// `DeviceAuthorizeRequestDto` has always documented and parsed
    /// commas. Accepting both costs one `matches!` and breaks nobody.
    ///
    /// **Access defaults to `:rw` when unqualified**, which is forced
    /// rather than chosen: the rows this replaced carried no qualifier
    /// and those credentials have full write access today, so defaulting
    /// to `:ro` would retroactively break every sync client in the field.
    /// It also matches convention — Google's bare `drive` is full access
    /// and `drive.readonly` is the explicit narrowing.
    ///
    /// Every malformed token is an ERROR, never a silent skip. See
    /// [`ScopeParseError`] on why a partial parse is the worse failure.
    pub fn parse(raw: &str) -> Result<Self, ScopeParseError> {
        let tokens: Vec<&str> = raw
            .split([' ', ',', '\t'])
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            return Err(ScopeParseError::Empty);
        }

        let mut out = Self::empty();
        for token in tokens {
            // The pre-area vocabulary, checked BEFORE the prefix rule
            // because these tokens are unprefixed by definition.
            if let Some((scope, access)) = Scope::from_legacy_protocol(token) {
                tracing::warn!(
                    target: "audit",
                    event = "authz.scope_legacy_vocabulary",
                    reason = "protocol_token_aliased",
                    token = token,
                    resolved = %format!("oxicloud:{}:{}", scope.name(), access.as_str()),
                    "👮🏻‍♂️ deprecated protocol scope accepted — update the \
                     caller to the area vocabulary; this alias goes away \
                     at the next major",
                );
                out.insert(scope, access);
                continue;
            }

            let rest =
                token
                    .strip_prefix("oxicloud:")
                    .ok_or_else(|| ScopeParseError::Unprefixed {
                        token: token.to_owned(),
                    })?;

            // `area` or `area:access`. Splitting from the RIGHT would be
            // wrong: an area name never contains a colon, but a future
            // one might, and the access is always the last segment.
            let (area, access) = match rest.rsplit_once(':') {
                Some((area, "ro")) => (area, Access::ReadOnly),
                Some((area, "rw")) => (area, Access::ReadWrite),
                // A trailing segment that is not an access is either a
                // typo or an area we do not know — report the whole
                // token, since reporting the fragment would be cryptic.
                Some(_) => {
                    return Err(ScopeParseError::UnknownAccess {
                        token: token.to_owned(),
                    });
                }
                None => (rest, Access::ReadWrite),
            };

            out.insert(Scope::parse(area)?, access);
        }
        Ok(out)
    }

    /// Render back to the wire form, for round-tripping and for logs.
    ///
    /// Comma-delimited and fully qualified, so the output is accepted by
    /// [`Self::parse`] and says exactly what it means — a bare `files`
    /// would mean `files:rw` but a reader should not have to know that.
    #[must_use]
    pub fn to_wire(self) -> String {
        self.iter()
            .map(|(s, a)| format!("oxicloud:{}:{}", s.name(), a.as_str()))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Everything in BOTH sets, per area and per access.
    ///
    /// The mask form makes this two `&`s, and the `writable ⊆ readable`
    /// invariant survives because intersecting two sets that hold it
    /// cannot produce one that does not.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self {
            readable: self.readable & other.readable,
            writable: self.writable & other.writable,
        }
    }

    /// Resolve a credential's stored or claimed scope string.
    ///
    /// One helper for all four sources, because the `None` rule is the
    /// subtle part and must not be re-decided per call site.
    ///
    /// * `None` → EVERY scope. Grandfathering, matching the `sid` claim's
    ///   precedent: a token or row predating the scope column lacks it,
    ///   and reading absence as "no scopes" would lock out every
    ///   credential in flight on the deploy that introduced it. An
    ///   interactive session also legitimately passes `None` — its
    ///   ceiling IS everything.
    /// * malformed → NO scopes, plus an audit line. This is the one
    ///   fail-CLOSED direction here, and it is safe to take because the
    ///   mint validates: a stored value that will not parse means the
    ///   column was written by something other than
    ///   `CredentialKind::effective`, so refusing is the honest answer.
    ///   Granting everything on a parse failure would make corrupting the
    ///   column an escalation.
    #[must_use]
    pub fn from_credential(raw: Option<&str>) -> Self {
        match raw {
            None => Self::full(),
            Some(value) => ScopeSet::parse(value).unwrap_or_else(|error| {
                tracing::warn!(
                    target: "audit",
                    event = "authz.scope_unparseable",
                    reason = "stored_value_rejected",
                    scope_raw = %value,
                    error = %error,
                    "👮🏻‍♂️ credential scope will not parse — refusing every \
                     area; the mint validates, so this column was written \
                     by something else"
                );
                Self::empty()
            }),
        }
    }

    /// Does this set satisfy everything `required` asks for?
    ///
    /// A superset test, which is what makes the APPEND-only fold work: a
    /// policy accumulates requirements down the chain, and a credential
    /// must carry all of them. Both sides are the same type on purpose —
    /// a requirement and a grant are the same shape, so the check is two
    /// mask comparisons rather than a loop with a special case.
    #[must_use]
    pub const fn satisfies(self, required: Self) -> bool {
        (self.readable & required.readable) == required.readable
            && (self.writable & required.writable) == required.writable
    }

    /// The first (area, access) in `required` that this set does NOT
    /// carry, for naming in a denial.
    #[must_use]
    pub fn first_unmet(self, required: Self) -> Option<(Scope, Access)> {
        required.iter().find(|&(s, a)| !self.allows(s, a))
    }

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
mod parse_tests {
    use super::*;

    /// Both delimiters, because the two disagree today and neither client
    /// is at fault: RFC 6749 §3.3 says space, `DeviceAuthorizeRequestDto`
    /// has always documented comma.
    #[test]
    fn space_and_comma_both_parse() {
        let want = ScopeSet::empty()
            .with(Scope::Files, Access::ReadWrite)
            .with(Scope::Calendar, Access::ReadOnly);

        for raw in [
            "oxicloud:files:rw oxicloud:calendar:ro",
            "oxicloud:files:rw,oxicloud:calendar:ro",
            "oxicloud:files:rw, oxicloud:calendar:ro",
        ] {
            assert_eq!(ScopeSet::parse(raw).expect(raw), want, "{raw}");
        }
    }

    /// An unqualified area is READ-WRITE, and this is forced rather than
    /// chosen: the rows this vocabulary replaced carried no qualifier and
    /// those credentials have full write access today, so `:ro` would
    /// retroactively break every sync client in the field.
    #[test]
    fn an_unqualified_area_is_read_write() {
        let set = ScopeSet::parse("oxicloud:files").expect("parses");
        assert!(set.allows(Scope::Files, Access::ReadWrite));
    }

    /// Every malformed token is an ERROR, never a silent skip.
    ///
    /// § Wire format's worked case is why: a client sending the textbook
    /// `scope=openid profile email` must fail cleanly, because `profile`
    /// is a reserved OIDC name that would otherwise have matched with the
    /// OPPOSITE meaning — read-only identity claims there, write access
    /// to user data here. A partial parse that resolves the one token it
    /// recognised is the worst outcome available.
    #[test]
    fn every_malformed_token_is_refused_not_skipped() {
        assert_eq!(ScopeSet::parse(""), Err(ScopeParseError::Empty));
        assert_eq!(ScopeSet::parse("   "), Err(ScopeParseError::Empty));

        // The OIDC collision case, end to end.
        assert!(matches!(
            ScopeSet::parse("openid profile email"),
            Err(ScopeParseError::Unprefixed { .. })
        ));
        // Even alongside a token that WOULD have parsed.
        assert!(matches!(
            ScopeSet::parse("oxicloud:files:rw profile"),
            Err(ScopeParseError::Unprefixed { .. })
        ));

        assert!(matches!(
            ScopeSet::parse("oxicloud:mail"),
            Err(ScopeParseError::Reserved { .. })
        ));
        assert!(matches!(
            ScopeSet::parse("oxicloud:nonsense"),
            Err(ScopeParseError::UnknownArea { .. })
        ));
        assert!(matches!(
            ScopeSet::parse("oxicloud:files:readonly"),
            Err(ScopeParseError::UnknownAccess { .. })
        ));
    }

    /// `to_wire` round-trips and is fully qualified — a bare `files`
    /// would mean `files:rw`, and a reader should not have to know that.
    #[test]
    fn the_wire_form_round_trips_fully_qualified() {
        let set = ScopeSet::parse("oxicloud:files,oxicloud:calendar:ro").expect("parses");
        let wire = set.to_wire();
        assert!(wire.contains("oxicloud:files:rw"), "{wire}");
        assert!(wire.contains("oxicloud:calendar:ro"), "{wire}");
        assert_eq!(ScopeSet::parse(&wire).expect("re-parses"), set);
    }

    /// `effective = (requested ?? default) ∩ ceiling` — both halves
    /// bounded, so neither an omitted scope nor a greedy request can
    /// escalate.
    #[test]
    fn a_greedy_request_is_narrowed_to_the_ceiling() {
        // Everything, asked for by an app password.
        let got = CredentialKind::AppPassword
            .effective(Some(
                "oxicloud:files:rw oxicloud:security:rw oxicloud:admin:rw",
            ))
            .expect("parses");

        assert!(got.allows(Scope::Files, Access::ReadWrite));
        assert!(
            !got.allows(Scope::Security, Access::ReadOnly),
            "an app password must never hold `security` — it is how one \
             would mint another"
        );
        assert!(
            !got.allows(Scope::Admin, Access::ReadOnly),
            "an admin's app password carries the admin ROLE, so only the \
             scope axis stops it administering the instance"
        );
    }

    /// An omitted scope takes the kind's DEFAULT, not everything. That
    /// fail-open shape is the bug this axis exists to close.
    #[test]
    fn an_omitted_scope_is_the_default_not_everything() {
        let got = CredentialKind::AppPassword
            .effective(None)
            .expect("defaults");

        for area in [Scope::Files, Scope::Calendar, Scope::Contacts] {
            assert!(got.allows(area, Access::ReadWrite), "{area:?}");
        }
        // `sharing` is the one asymmetric area: READ but not write.
        //
        // Read, because every NC client needs the roster — the share
        // dialog's recipient picker and the OCS profile lookup both read
        // it, and withholding it broke `nc_admin_views_other_user.hurl`.
        // The ceiling exists to prevent ESCALATION, not to withhold
        // capability the human already has in the browser.
        //
        // Not write, because all three OCS routes that declare `sharing`
        // are GETs, so `:ro` satisfies every door that exists. Widening
        // later is safe; narrowing breaks clients in the field.
        assert!(got.allows(Scope::Sharing, Access::ReadOnly));
        assert!(
            !got.allows(Scope::Sharing, Access::ReadWrite),
            "`sharing` is granted READ-ONLY — least privilege, and no \
             declared OCS route needs the write"
        );

        // The escalation areas, withheld outright: `security` is how a
        // credential would mint another, `admin` how it would administer
        // the instance.
        for area in [Scope::Security, Scope::Admin, Scope::Account] {
            assert!(
                !got.allows(area, Access::ReadOnly),
                "{area:?} must be withheld"
            );
        }
    }

    /// A device-flow credential may hold `account`; an app password may
    /// not. The difference is consent: the device verify screen shows the
    /// user what is being granted, an app password dialog does not.
    #[test]
    fn the_device_flow_ceiling_admits_account_and_the_app_password_one_does_not() {
        let device = CredentialKind::DeviceFlow
            .effective(Some("oxicloud:account:ro"))
            .expect("parses");
        assert!(device.allows(Scope::Account, Access::ReadOnly));

        let app = CredentialKind::AppPassword
            .effective(Some("oxicloud:account:ro"))
            .expect("parses");
        assert!(!app.allows(Scope::Account, Access::ReadOnly));
    }

    /// **Absence is PERMISSIVE, and only here.** Every token and row
    /// minted before the scope claim existed lacks it, so reading absence
    /// as "no scopes" would 401 every credential in flight on the deploy
    /// that introduced the column. A future reader tempted to "fix" this
    /// to fail closed should change the ROLLOUT instead — mint the claim
    /// everywhere, let the old tokens expire, then tighten.
    #[test]
    fn an_absent_scope_means_every_scope() {
        assert_eq!(ScopeSet::from_credential(None), ScopeSet::full());
    }

    /// A stored value that will not parse fails CLOSED — the one
    /// fail-closed direction in this resolver, and safe to take because
    /// the mint validates. A column that holds nonsense was written by
    /// something other than `CredentialKind::effective`, so granting
    /// everything on a parse failure would make corrupting it an
    /// escalation.
    #[test]
    fn an_unparseable_stored_scope_grants_nothing() {
        // NOT `webdav,caldav`, which this test used until the legacy
        // aliases landed — that vocabulary parses now, by design, so it
        // no longer demonstrates anything about the failure path.
        let got = ScopeSet::from_credential(Some("nonsense-not-a-scope"));
        assert_eq!(
            got,
            ScopeSet::empty(),
            "a stored value that will not parse must grant NOTHING \
             rather than everything — the mint validates, so a column \
             holding this was written by something else, and granting \
             everything would make corrupting it an escalation"
        );
    }

    /// The pre-area vocabulary is ACCEPTED, and resolves to the areas it
    /// used to mean.
    ///
    /// The old default — `webdav,caldav,carddav` — appeared in every
    /// app-password creation response, so automation that read `scopes`
    /// back and echoed it later sends exactly this. Two of this repo's
    /// own shell suites hardcoded `"webdav"` and broke at mint, which is
    /// what prompted the alias.
    #[test]
    fn the_legacy_protocol_vocabulary_still_works() {
        let got = ScopeSet::parse("webdav,caldav,carddav").expect("aliased");
        assert!(got.allows(Scope::Files, Access::ReadWrite));
        assert!(got.allows(Scope::Calendar, Access::ReadWrite));
        assert!(got.allows(Scope::Contacts, Access::ReadWrite));

        // Full access, because that is what these tokens always meant —
        // the legacy vocabulary had no access dimension.
        let one = ScopeSet::parse("webdav").expect("aliased");
        assert!(one.allows(Scope::Files, Access::ReadWrite));
        assert!(!one.allows(Scope::Calendar, Access::ReadOnly));

        // Mixable with the new vocabulary, so a caller can migrate one
        // token at a time.
        let mixed = ScopeSet::parse("webdav,oxicloud:calendar:ro").expect("aliased");
        assert!(mixed.allows(Scope::Files, Access::ReadWrite));
        assert!(mixed.allows(Scope::Calendar, Access::ReadOnly));
        assert!(!mixed.allows(Scope::Calendar, Access::ReadWrite));
    }

    /// Aliasing three names we OWN does not weaken the namespacing rule
    /// for names we do not.
    ///
    /// This is the whole reason the prefix exists: `profile` is reserved
    /// by OIDC and would match an OxiCloud area with the OPPOSITE
    /// meaning — read-only identity claims there, write access to user
    /// data here. It must keep failing, and so must an invented
    /// access on a legacy token, which was never valid.
    #[test]
    fn the_aliases_do_not_open_the_namespace() {
        assert!(matches!(
            ScopeSet::parse("openid profile email"),
            Err(ScopeParseError::Unprefixed { .. })
        ));
        assert!(matches!(
            ScopeSet::parse("webdav,profile"),
            Err(ScopeParseError::Unprefixed { .. })
        ));
        // `webdav:ro` never existed, so the alias does not invent it.
        assert!(matches!(
            ScopeSet::parse("webdav:ro"),
            Err(ScopeParseError::Unprefixed { .. })
        ));
    }

    /// The ordinary case: a stored wire string resolves to its set.
    #[test]
    fn a_stored_scope_resolves_to_its_set() {
        let got = ScopeSet::from_credential(Some("oxicloud:files:ro"));
        assert!(got.allows(Scope::Files, Access::ReadOnly));
        assert!(!got.allows(Scope::Files, Access::ReadWrite));
        assert!(!got.allows(Scope::Calendar, Access::ReadOnly));
    }

    /// The visitor ceiling and the COMPILE-time check must stay equal:
    /// `auth_middleware` stamps `ScopeSet::ring()` on a ring-holder, and
    /// `reject_unsatisfiable` refuses any visitor-admitting route that
    /// requires more than the same constant. If these two ever disagree,
    /// generation accepts a route no visitor can actually use.
    #[test]
    fn the_visitor_ring_is_exactly_files_read_only() {
        let ring = ScopeSet::ring();
        assert!(ring.allows(Scope::Files, Access::ReadOnly));
        assert!(!ring.allows(Scope::Files, Access::ReadWrite));
        for area in [
            Scope::Calendar,
            Scope::Contacts,
            Scope::Sharing,
            Scope::Account,
        ] {
            assert!(!ring.allows(area, Access::ReadOnly), "{area:?}");
        }
    }

    /// A malformed request is an ERROR even for a kind whose ceiling
    /// would have narrowed it away. Narrowing applies to what a client
    /// may HAVE, never to what it managed to SAY.
    #[test]
    fn narrowing_does_not_excuse_a_malformed_request() {
        assert!(
            CredentialKind::AppPassword
                .effective(Some("oxicloud:security:nonsense"))
                .is_err()
        );
    }
}

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
