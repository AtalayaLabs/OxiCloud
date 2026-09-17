use chrono::{DateTime, Utc};
use uuid::Uuid;

// Re-export entity errors from the centralized module
pub use super::entity_errors::{UserError, UserResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// We'll handle conversion manually for now until the type is properly set up in the database
pub enum UserRole {
    /// The server owner: an administrator that other administrators cannot
    /// act on. Outranks [`Self::Admin`], so it satisfies every admin gate
    /// without those gates naming it.
    ///
    /// At most one row may hold it today (`idx_users_single_owner`), but
    /// that is policy rather than something this type assumes — nothing
    /// here would need to change to allow several. See
    /// `docs/plan/role-hierarchy-owner.md`.
    Owner,
    Admin,
    User,
    /// A public-share visitor. **Never stored in `auth.users`** — it exists
    /// only on a session and its JWT, and `from_stored` deliberately cannot
    /// produce it. See `docs/plan/rationalize-publicshare.md`.
    Anonymous,
}

impl UserRole {
    /// Canonical wire/DB spelling — the single source the `Display` impl
    /// and every hot-path role render go through (no format machinery).
    pub fn as_str(self) -> &'static str {
        match self {
            UserRole::Owner => "owner",
            UserRole::Admin => "admin",
            UserRole::User => "user",
            UserRole::Anonymous => "anonymous",
        }
    }

    /// Parse a role that is legitimately stored on `auth.users`.
    ///
    /// Returns `None` for anything else — including `"anonymous"`, which has
    /// no user row by construction. Callers reading the database previously
    /// used `_ => UserRole::User`, which is fail-OPEN: an unrecognised value
    /// silently became a real user. Use this instead and decide explicitly.
    pub fn from_stored(raw: &str) -> Option<Self> {
        match raw {
            "owner" => Some(UserRole::Owner),
            "admin" => Some(UserRole::Admin),
            "user" => Some(UserRole::User),
            _ => None,
        }
    }

    /// Parse a role as carried on a **session** — a JWT claim or
    /// `CurrentUser`. Unlike [`Self::from_stored`] this can yield
    /// `Anonymous`: a share visitor has a session role but no user row.
    pub fn from_session(raw: &str) -> Option<Self> {
        match raw {
            "anonymous" => Some(UserRole::Anonymous),
            other => UserRole::from_stored(other),
        }
    }

    /// Privilege order: `Anonymous` < `User` < `Admin` < `Owner`.
    ///
    /// Deliberately an explicit rank rather than a derived `Ord` — a derive
    /// follows declaration order, so reordering the variants would silently
    /// invert every comparison.
    ///
    /// Contiguous on purpose. Gaps "reserved for future roles" buy nothing:
    /// adding a variant means revisiting every `match` on this enum anyway,
    /// and the integers appear in no wire format.
    pub fn rank(self) -> u8 {
        match self {
            UserRole::Anonymous => 0,
            UserRole::User => 1,
            UserRole::Admin => 2,
            UserRole::Owner => 3,
        }
    }

    /// True when this role satisfies a `min` requirement. The comparison
    /// behind `require_role`.
    pub fn at_least(self, min: UserRole) -> bool {
        self.rank() >= min.rank()
    }

    /// True when `self` may act on a user holding `target`.
    ///
    /// **Strict**, where [`Self::at_least`] is inclusive — that difference
    /// is the whole hierarchy. An admin meets an admin-level requirement
    /// (`at_least`) but may not act upon another admin (`outranks`), which
    /// is what stops one rogue admin from locking out the rest.
    ///
    /// Self-directed actions do not go through here: changing your own
    /// password is a `/me` operation, not an administrative one.
    pub fn outranks(self, target: UserRole) -> bool {
        self.rank() > target.rank()
    }

    /// True for a principal with no `auth.users` row behind it.
    pub fn is_anonymous(self) -> bool {
        matches!(self, UserRole::Anonymous)
    }

    /// Does this **stringly-typed** role meet `min`?
    ///
    /// The role crosses several boundaries as a plain `String` — the JWT
    /// claim, `CurrentUser.role`, `PublicUserDto.role`, the live-role
    /// string — and before the Owner role existed, six sites compared
    /// those strings to `"admin"` literally. Every one silently denied the
    /// owner, who outranks admin: the admin API, the admin middleware, the
    /// NextCloud OCS group list, group management, dedup ref-counts, and
    /// shared-drive creation. The API test suite caught it as
    /// `authz.admin_denied … role=owner`.
    ///
    /// Compare ranks, never spellings. An unparseable role is refused:
    /// this is a gate, so the unknown answer is "no".
    pub fn str_at_least(raw: &str, min: UserRole) -> bool {
        UserRole::from_session(raw).is_some_and(|role| role.at_least(min))
    }

    /// True for any role carrying more authority than a regular user.
    ///
    /// The external-identity guards ask this question — "may an
    /// IdP-provisioned account hold this role?" — and they MUST ask it
    /// this way rather than comparing against `Admin`. `Admin` was
    /// historically the only privileged role, so the guards were written
    /// as `role == Admin`; a role added *above* Admin would then walk
    /// straight through them, letting a federated identity hold the most
    /// privileged account on the instance.
    ///
    /// Phrased as a negative ("more than a plain user") this stays correct
    /// for roles that do not exist yet. Do not respell it as a list.
    /// See `docs/plan/role-hierarchy-owner.md`.
    pub fn is_privileged(self) -> bool {
        self.rank() > UserRole::User.rank()
    }
}

impl std::fmt::Display for UserRole {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The trust chain that owns a user's identity. NULL on `auth.users`
/// means "pure local user, no external federation" — the common case for
/// password/OPAQUE accounts.
///
/// See `docs/plan/ocm.md § Identity & auth model` for the full model
/// including the `(federation_kind, federation_issuer, federation_subject)`
/// composite identity key.
///
/// - `Oidc`: authenticated via an OIDC provider; `federation_issuer` =
///   the id_token `iss` claim, `federation_subject` = the `sub` claim.
///   Note: legacy rows may still hold the OXICLOUD_OIDC_PROVIDER_NAME
///   display label as `federation_issuer` until Phase B of the rename
///   backfills them to real issuer URLs.
/// - `Ocm`: OCM 1.1 federated principal (future — `docs/plan/ocm.md`).
///   `federation_issuer` = peer domain, `federation_subject` = federated
///   address (e.g. `alice@remote.example.com`).
/// - `MagicLink`: external invitee whose only auth is mailbox
///   possession. Both `federation_issuer` and `federation_subject`
///   remain NULL for this kind — the identity is the local `email`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FederationKind {
    MagicLink,
    Ocm,
    Oidc,
}

impl FederationKind {
    /// Canonical DB / wire spelling — matches the CHECK constraint on
    /// `auth.users.federation_kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            FederationKind::MagicLink => "magic_link",
            FederationKind::Ocm => "ocm",
            FederationKind::Oidc => "oidc",
        }
    }

    /// Parse the DB / wire spelling. Returns `None` for anything other
    /// than the three canonical values — the CHECK constraint on the
    /// column and the enum variants are the source of truth.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "magic_link" => Some(FederationKind::MagicLink),
            "ocm" => Some(FederationKind::Ocm),
            "oidc" => Some(FederationKind::Oidc),
            _ => None,
        }
    }
}

impl std::fmt::Display for FederationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Authorization-relevant account flags, fetched without the heavyweight
/// profile columns. The full user row drags `image` along — a data URI of
/// up to 512 KiB — which per-request guards (`require_internal_user`,
/// `require_admin_user`, the NC Basic Auth external check) must never pay
/// for just to read a boolean or a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserFlags {
    pub role: UserRole,
    pub is_external: bool,
    pub active: bool,
    /// Mirrors `auth.users.force_password_change_at_next_login`. TRUE
    /// after an admin password-reset; the `require_no_password_change_pending`
    /// middleware refuses every authenticated endpoint except the
    /// change-password / me / logout / refresh allowlist while it's set.
    /// Cached alongside the other flags so per-request enforcement
    /// doesn't add a DB round-trip. Eagerly invalidated by
    /// `admin_reset_password` (flip to TRUE) and `change_password`
    /// (flip to FALSE) so the gate lifts within one round-trip.
    pub force_password_change: bool,
}

#[derive(Debug, Clone)]
pub struct User {
    id: Uuid,
    /// Optional handle (2-64 chars, no `@`). NULL for users created via
    /// email-invitation (`is_external = true`) and for users who have
    /// not yet claimed a handle (PR-18 email-only signups). When set, it
    /// must satisfy `validate_username` and must NOT contain `@` —
    /// keeping the username and email namespaces provably disjoint.
    username: Option<String>,
    email: String,
    /// Optional Argon2 password hash. NULL when the user has no password
    /// (externals, OIDC-only users, email-only signups awaiting their
    /// welcome magic-link). After PR 16 this column carries no sentinel
    /// strings — `is_some()` means "real argon2 hash"; `None` means "no
    /// password configured".
    password_hash: Option<String>,
    role: UserRole,
    storage_quota_bytes: i64,
    storage_used_bytes: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_login_at: Option<DateTime<Utc>>,
    active: bool,
    /// Which trust chain minted the (issuer, subject) pair. `None` on
    /// pure local users (password/OPAQUE). See [`FederationKind`] for the
    /// three federation flavours.
    federation_kind: Option<FederationKind>,
    federation_issuer: Option<String>,
    federation_subject: Option<String>,
    image: Option<String>,
    /// TRUE = grant-only external recipient (magic-link, OIDC-only, OCM
    /// federated). FALSE = storage-owning internal user. Hooks that
    /// provision per-user resources (home folder, default calendar, …)
    /// must short-circuit when `is_external` is TRUE — see tip #2 in
    /// `application/ports/user_lifecycle.rs`. The DB CHECK constraint
    /// `users_external_no_storage` is the schema-level safety net.
    is_external: bool,
    /// Optional human-readable first/given name. Populated from OIDC
    /// standard claim `given_name` at JIT provisioning, or via the
    /// profile-edit endpoint. External users start with `None`.
    given_name: Option<String>,
    /// Optional human-readable last/family name. Populated from OIDC
    /// standard claim `family_name` at JIT provisioning, or via the
    /// profile-edit endpoint. External users start with `None`.
    family_name: Option<String>,
    /// When the user demonstrated control of their email address (PR 23).
    /// `None` = unverified. `Some(ts)` = timestamp of the first proof,
    /// preserved across subsequent verifications.
    ///
    /// Set on successful magic-link redemption (invitation OR
    /// login-via-email — clicking the link proves the inbox is theirs)
    /// or on OIDC JIT with `email_verified=true` claim. Classic password
    /// signups stay `None` until the user goes through a magic-link
    /// flow. PR 23 ships the signal only — future policy PRs gate
    /// features (uploads, shares, etc.) on this column.
    email_verified_at: Option<DateTime<Utc>>,
    /// User-chosen locale for server-rendered surfaces (transactional
    /// emails, future authenticated HTML pages). `None` = no preference,
    /// resolves to `OXICLOUD_DEFAULT_LOCALE` at use time. Set by:
    /// - the frontend language switcher (PATCH /api/auth/me/profile),
    /// - the OIDC JIT path at provisioning **only**, never re-applied
    ///   on subsequent logins (a UI choice always wins over the IdP),
    /// - the magic-link invitation flow, which copies the inviter's
    ///   value into the new external user's row.
    ///
    /// Schema-level CHECK enforces a textual BCP-47 shape; the
    /// application layer is the authoritative gatekeeper against the
    /// `LocaleRegistry`.
    preferred_locale: Option<String>,
    /// Per-user opt-out for share-notification emails (PR N1). TRUE =
    /// receive a mail when someone grants access to a resource (default);
    /// FALSE = grant still recorded but `RecipientNotificationService`
    /// returns `NotApplicable { recipient_opted_out }` and no mail is
    /// sent. Bypassed for magic-link first-invitations to external users
    /// — the link is their only way to claim the share, so suppressing
    /// it would lock them out. Once an external becomes a real account
    /// and opts out, subsequent shares from other granters honor the
    /// flag.
    notify_on_share: bool,
    /// Opaque UI preferences bag (PR — this session). Stored as JSONB
    /// on `auth.users.ui_preferences`; the server NEVER inspects the
    /// contents. This is the SPA's cross-device backing store for pure
    /// UI toggles (hide-dotfiles, view mode, sidebar collapse, …).
    ///
    /// Merge semantics live in the repo layer: `PATCH /me/profile` does
    /// a SHALLOW merge via `ui_preferences || $1::jsonb`, so partial
    /// writes from one device don't clobber keys set on another.
    ///
    /// Load-bearing rule: if a preference EVER becomes something the
    /// server reads (like `preferred_locale` did), promote it out of
    /// this bag into a typed column. Keep this field for UI-only
    /// toggles.
    ///
    /// Invariant: always a JSON object (enforced by the schema CHECK
    /// `users_ui_preferences_is_object`). Empty bag is `{}`, never
    /// `null` or missing.
    ui_preferences: serde_json::Value,
}

/// Owned decomposition of a [`User`] (mirrors `FileParts` / `FolderParts` /
/// `ContactParts`). Lets a consumer MOVE the heap fields out instead of cloning
/// them through the borrowing accessors — notably `image` (a data URI up to
/// 512 KiB) and `ui_preferences` (a JSON tree). See `PublicUserDto::from`
/// (benches/ROUND20.md §A2).
pub struct UserParts {
    pub id: Uuid,
    pub username: Option<String>,
    pub email: String,
    pub password_hash: Option<String>,
    pub role: UserRole,
    pub storage_quota_bytes: i64,
    pub storage_used_bytes: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub active: bool,
    pub federation_kind: Option<FederationKind>,
    pub federation_issuer: Option<String>,
    pub federation_subject: Option<String>,
    pub image: Option<String>,
    pub is_external: bool,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub email_verified_at: Option<DateTime<Utc>>,
    pub preferred_locale: Option<String>,
    pub notify_on_share: bool,
    pub ui_preferences: serde_json::Value,
}

impl User {
    /// Decompose into [`UserParts`], moving every owned field out. The
    /// exhaustive destructure is compiler-checked, so a future field can't be
    /// silently dropped.
    pub fn into_parts(self) -> UserParts {
        let User {
            id,
            username,
            email,
            password_hash,
            role,
            storage_quota_bytes,
            storage_used_bytes,
            created_at,
            updated_at,
            last_login_at,
            active,
            federation_kind,
            federation_issuer,
            federation_subject,
            image,
            is_external,
            given_name,
            family_name,
            email_verified_at,
            preferred_locale,
            notify_on_share,
            ui_preferences,
        } = self;
        UserParts {
            id,
            username,
            email,
            password_hash,
            role,
            storage_quota_bytes,
            storage_used_bytes,
            created_at,
            updated_at,
            last_login_at,
            active,
            federation_kind,
            federation_issuer,
            federation_subject,
            image,
            is_external,
            given_name,
            family_name,
            email_verified_at,
            preferred_locale,
            notify_on_share,
            ui_preferences,
        }
    }

    /// Create a new user.
    ///
    /// One unified constructor for every kind of user (internal, OIDC-linked,
    /// external). The credential slots and the `is_external` marker are all
    /// caller-controlled — what makes a user "OIDC" is `federation_subject =
    /// Some(_)`, what makes them "external" is `is_external = true`. There
    /// are no hidden sentinel values; an absent credential is `None`.
    ///
    /// # Arguments
    /// * `email` — required, must satisfy `validate_email`
    /// * `username` — optional handle (2-64 chars, no `@`)
    /// * `password_hash` — pre-hashed via PasswordHasherPort, or `None` if
    ///   the user has no password yet (magic-link or OIDC bootstrap)
    /// * `federation_issuer`, `federation_subject` — both `Some` when the user is
    ///   linked to an external IdP, both `None` otherwise
    /// * `role` — `Admin` is rejected when `is_external = true` (mirrors the
    ///   `users_external_not_admin` DB CHECK constraint)
    /// * `storage_quota_bytes` — caller-set; external callers should pass 0
    ///   to satisfy the `users_external_no_storage` invariant
    /// * `is_external` — TRUE for grant-only recipients (magic-link, OCM)
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        email: String,
        username: Option<String>,
        password_hash: Option<String>,
        federation_kind: Option<FederationKind>,
        federation_issuer: Option<String>,
        federation_subject: Option<String>,
        role: UserRole,
        storage_quota_bytes: i64,
        is_external: bool,
    ) -> UserResult<Self> {
        Self::validate_email(&email)?;
        // Shadow `username` with the canonical (trimmed, lowercased)
        // form returned by `validate_username`. Every downstream write
        // consumes the shadowed binding, so the row that lands in the
        // DB is always the normalised value. See
        // `docs/plan/username-lowercase.md`.
        let username = match username {
            Some(u) => Some(Self::validate_username(&u)?),
            None => None,
        };
        if let Some(ref h) = password_hash
            && h.is_empty()
        {
            return Err(UserError::InvalidPassword(
                "Password hash cannot be empty".to_string(),
            ));
        }
        // Schema-level CHECKs are mirrored at the entity layer so callers
        // get a typed error instead of an opaque DB rejection.
        if is_external && role.is_privileged() {
            return Err(UserError::ValidationError(format!(
                "External users cannot hold the {role} role"
            )));
        }
        if is_external && storage_quota_bytes != 0 {
            return Err(UserError::ValidationError(
                "External users must have storage_quota_bytes = 0".to_string(),
            ));
        }
        // Federation linkage is all-or-nothing: both issuer and subject set,
        // or neither. The DB has a UNIQUE index on
        // (federation_kind, federation_issuer, federation_subject) WHERE
        // federation_kind IS NOT NULL; partial state would corrupt that.
        //
        // For kinds that don't carry an authority-issued subject
        // (MagicLink today — identity is the local email), both fields
        // stay None even when `federation_kind` is set. The check below
        // only fires on inconsistent partial state.
        if federation_issuer.is_some() != federation_subject.is_some() {
            return Err(UserError::ValidationError(
                "federation_issuer and federation_subject must both be set or both be None"
                    .to_string(),
            ));
        }
        // If either field is set, federation_kind MUST also be set — the
        // schema keys anti-duplicate on the composite (kind, issuer,
        // subject) and a NULL kind would defeat the uniqueness.
        if federation_issuer.is_some() && federation_kind.is_none() {
            return Err(UserError::ValidationError(
                "federation_kind is required when federation_issuer/subject are set".to_string(),
            ));
        }

        let now = Utc::now();
        Ok(Self {
            id: Uuid::new_v4(),
            username,
            email,
            password_hash,
            role,
            storage_quota_bytes,
            storage_used_bytes: 0,
            created_at: now,
            updated_at: now,
            last_login_at: None,
            active: true,
            federation_kind,
            federation_issuer,
            federation_subject,
            image: None,
            is_external,
            given_name: None,
            family_name: None,
            // PR 23: unverified at creation. Stamped on the first
            // magic-link redemption or OIDC JIT (where the IdP has
            // already confirmed the email).
            email_verified_at: None,
            // PR C: no locale preference at creation. OIDC JIT, the
            // language switcher, or invitation-time inheritance fill
            // this in later. NULL resolves to OXICLOUD_DEFAULT_LOCALE.
            preferred_locale: None,
            // PR N1: default to opted-in. The profile checkbox is the
            // user-facing toggle; the column default in
            // `users_notify_on_share` mirrors this for rows reconstructed
            // from disk without going through `new`.
            notify_on_share: true,
            // Empty bag on creation. The SPA writes into it via
            // `PATCH /me/profile { ui_preferences: {...} }` after
            // login. Never NULL — the DB CHECK enforces JSON object
            // shape.
            ui_preferences: serde_json::json!({}),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_data(
        id: Uuid,
        username: Option<String>,
        email: String,
        password_hash: Option<String>,
        role: UserRole,
        storage_quota_bytes: i64,
        storage_used_bytes: i64,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        last_login_at: Option<DateTime<Utc>>,
        active: bool,
    ) -> Self {
        Self {
            id,
            username,
            email,
            password_hash,
            role,
            storage_quota_bytes,
            storage_used_bytes,
            created_at,
            updated_at,
            last_login_at,
            active,
            federation_kind: None,
            federation_issuer: None,
            federation_subject: None,
            image: None,
            // `from_data` is the minimal-args reconstruction path used by
            // tests and JWT-claim-based principal hydration (which doesn't
            // carry `is_external`). Default to FALSE — JWT-validated
            // principals are existing internal users; magic-link external
            // sessions take a different path that hydrates from DB via
            // `from_data_full`.
            is_external: false,
            given_name: None,
            family_name: None,
            email_verified_at: None,
            preferred_locale: None,
            notify_on_share: true,
            ui_preferences: serde_json::json!({}),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_data_full(
        id: Uuid,
        username: Option<String>,
        email: String,
        password_hash: Option<String>,
        role: UserRole,
        storage_quota_bytes: i64,
        storage_used_bytes: i64,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        last_login_at: Option<DateTime<Utc>>,
        active: bool,
        federation_kind: Option<FederationKind>,
        federation_issuer: Option<String>,
        federation_subject: Option<String>,
        image: Option<String>,
        is_external: bool,
        given_name: Option<String>,
        family_name: Option<String>,
        email_verified_at: Option<DateTime<Utc>>,
        preferred_locale: Option<String>,
        notify_on_share: bool,
        // Opaque UI-preferences bag. Callers reading from the DB pass
        // `row.get("ui_preferences")`; tests that don't care can pass
        // `serde_json::json!({})`.
        ui_preferences: serde_json::Value,
    ) -> Self {
        Self {
            id,
            username,
            email,
            password_hash,
            role,
            storage_quota_bytes,
            storage_used_bytes,
            created_at,
            updated_at,
            last_login_at,
            active,
            federation_kind,
            federation_issuer,
            federation_subject,
            image,
            is_external,
            given_name,
            family_name,
            email_verified_at,
            preferred_locale,
            notify_on_share,
            ui_preferences,
        }
    }

    // Getters
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// The user's chosen handle. `None` for users who have not claimed
    /// one (externals, fresh email-only signups). Display callers should
    /// fall back through `given_name`/`family_name` to `email` when this
    /// is `None`.
    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    pub fn email(&self) -> &str {
        &self.email
    }

    pub fn role(&self) -> UserRole {
        self.role
    }

    pub fn storage_quota_bytes(&self) -> i64 {
        self.storage_quota_bytes
    }

    pub fn storage_used_bytes(&self) -> i64 {
        self.storage_used_bytes
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    pub fn last_login_at(&self) -> Option<DateTime<Utc>> {
        self.last_login_at
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The Argon2 password hash, or `None` when the user has no password
    /// configured (externals, OIDC-only users, post-PR-18 email-only
    /// signups). `verify_password` callers must short-circuit to
    /// "invalid credentials" when this is `None`.
    pub fn password_hash(&self) -> Option<&str> {
        self.password_hash.as_deref()
    }

    /// Convenience: does the user have a real password configured?
    pub fn has_password(&self) -> bool {
        self.password_hash.is_some()
    }

    /// Best-effort label for audit-log interpolation. Returns the
    /// username when set; falls back to the user_id otherwise. Always
    /// implements `Display` (returns `String`) so audit lines can stay
    /// `username = %user.display_for_audit()` regardless of whether the
    /// user has claimed a handle. Reserve this for `target: "audit"`
    /// lines — user-facing display callers should walk the
    /// `username → given/family → email` fallback chain themselves.
    pub fn display_for_audit(&self) -> String {
        match &self.username {
            Some(u) => u.clone(),
            None => self.id.to_string(),
        }
    }

    /// Rich, user-facing display label for notification surfaces
    /// (transactional emails, share invitations, "Alice <a@x.com>
    /// shared X with you" — anywhere a human is reading the line).
    ///
    /// `with_email` controls whether the address is appended as
    /// `" <email>"` after the name part:
    /// - `true`  — best for the email **body** ("Alice Smith
    ///   <alice@example.com> shared a folder with you"), where the
    ///   extra identifier is helpful at a glance.
    /// - `false` — best for the **subject line** and other compact
    ///   contexts where dragging the email into a 80-char inbox row
    ///   would be noise ("Alice Smith shared a folder with you").
    ///
    /// Priority order (mirrors RFC 5322 display-name conventions). The
    /// `<email>` decoration in cases 1 and 3 is omitted when
    /// `with_email` is false:
    ///
    /// 1. `"Given Family"` (+ ` <email>`) — full name; the most
    ///    informative form.
    /// 2. `"username"`     (+ ` <email>`) — handle; the typical case
    ///    for password / OIDC users without first/last claims.
    /// 3. `email`                          — last-resort fallback. The
    ///    raw email address is always present for non-OCM users and is
    ///    the unambiguous identifier. Returned regardless of
    ///    `with_email` since it IS the label here.
    /// 4. shortened UUID                   — failure mode (no email,
    ///    no username, no given/family — shouldn't happen with current
    ///    schema invariants but kept defensive for OCM-federated rows).
    ///
    /// External users provisioned via magic-link typically have only an
    /// email and fall through to branch 3. Internal users with OIDC
    /// JIT often have given/family from the IdP claims → branch 1.
    /// Sister of [`Self::display_for_audit`], which deliberately
    /// returns a *less* identifying label for log lines.
    pub fn display_full(&self, with_email: bool) -> String {
        let g = self.given_name.as_deref();
        let f = self.family_name.as_deref();
        let u = self.username.as_deref();
        let has_email = !self.email.is_empty();

        if let (Some(g), Some(f)) = (g, f) {
            if with_email && has_email {
                return format!("{} {} <{}>", g, f, self.email);
            }
            return format!("{} {}", g, f);
        }
        if let Some(u) = u {
            if with_email && has_email {
                return format!("{} <{}>", u, self.email);
            }
            return u.to_string();
        }
        if has_email {
            return self.email.clone();
        }
        format!("{}…", &self.id.to_string()[..8])
    }

    pub fn federation_kind(&self) -> Option<FederationKind> {
        self.federation_kind
    }

    pub fn federation_issuer(&self) -> Option<&str> {
        self.federation_issuer.as_deref()
    }

    pub fn federation_subject(&self) -> Option<&str> {
        self.federation_subject.as_deref()
    }

    pub fn image(&self) -> Option<&str> {
        self.image.as_deref()
    }

    /// `TRUE` for grant-only external recipients (magic-link, OIDC-only,
    /// OCM federated). Hooks provisioning per-user resources must
    /// short-circuit when this returns `true` — see tip #2 in
    /// `application/ports/user_lifecycle.rs`.
    pub fn is_external(&self) -> bool {
        self.is_external
    }

    pub fn given_name(&self) -> Option<&str> {
        self.given_name.as_deref()
    }

    pub fn family_name(&self) -> Option<&str> {
        self.family_name.as_deref()
    }

    /// When the user first demonstrated control of their email (PR 23).
    /// `None` = unverified. See `mark_email_verified` for the trigger
    /// points (magic-link redemption, OIDC JIT with verified claim).
    pub fn email_verified_at(&self) -> Option<DateTime<Utc>> {
        self.email_verified_at
    }

    /// `true` iff the user has demonstrated control of their email.
    /// Convenience wrapper over `email_verified_at().is_some()`.
    pub fn is_email_verified(&self) -> bool {
        self.email_verified_at.is_some()
    }

    /// Stamp the first proof-of-email-control timestamp. **Idempotent**:
    /// if `email_verified_at` is already `Some`, this is a no-op so
    /// re-verifications preserve the original time. Call from the
    /// magic-link redemption path and from OIDC JIT when the IdP
    /// confirms the email.
    pub fn mark_email_verified(&mut self) {
        if self.email_verified_at.is_none() {
            let now = Utc::now();
            self.email_verified_at = Some(now);
            self.updated_at = now;
        }
    }

    /// Promote a currently-external user to an internal account.
    /// Atomically flips the invariant-linked fields:
    ///   * `is_external`  → false
    ///   * `password_hash` → provided (Some) or preserved (None)
    ///   * `storage_quota_bytes` → quota (external users had 0; DB CHECK
    ///     `users_external_no_storage` enforces the pair before this call
    ///     and would refuse a non-zero quota on an external row — the
    ///     write MUST flip `is_external` first, which happens
    ///     transactionally at persist time via the sqlx UPDATE).
    ///
    /// Password is `Option<String>` because the service allows password-
    /// less upgrades when magic-link login is available on the
    /// deployment. When `None`, `password_hash` stays as it was (either
    /// NULL, or a hash left over from an admin-created invitation —
    /// externals don't authenticate with it either way).
    ///
    /// Refuses if the caller is already internal — the upgrade path
    /// only makes sense on `is_external = true` users. Service pre-
    /// checks `user.is_external()` before calling; this guard is
    /// belt-and-braces against a race.
    ///
    /// Admin combo is impossible by construction: external + admin was
    /// refused at creation (see `User::new`), so a promoted external
    /// user always retains their `UserRole::User` — role isn't changed.
    pub fn promote_to_internal(
        &mut self,
        password_hash: Option<String>,
        storage_quota_bytes: i64,
    ) -> UserResult<()> {
        if !self.is_external {
            return Err(UserError::AlreadyInternal);
        }
        self.is_external = false;
        if let Some(hash) = password_hash {
            self.password_hash = Some(hash);
        }
        self.storage_quota_bytes = storage_quota_bytes;
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn set_image(&mut self, image: Option<String>) {
        self.image = image;
        self.updated_at = Utc::now();
    }

    pub fn set_given_name(&mut self, given_name: Option<String>) {
        self.given_name = given_name;
        self.updated_at = Utc::now();
    }

    pub fn set_family_name(&mut self, family_name: Option<String>) {
        self.family_name = family_name;
        self.updated_at = Utc::now();
    }

    /// Borrow the user's stored locale code (e.g. `"fr"`, `"zh-TW"`),
    /// if any. The application layer is expected to feed this through
    /// `LocaleRegistry::parse_or_default` before rendering, so an
    /// orphaned code from a since-removed locale falls back gracefully
    /// instead of triggering a translation error.
    pub fn preferred_locale(&self) -> Option<&str> {
        self.preferred_locale.as_deref()
    }

    /// Set or clear the user's preferred locale. The caller is
    /// responsible for having already validated the code against the
    /// `LocaleRegistry` — at the entity layer we treat the field as
    /// opaque text, the way we do for `given_name` / `family_name`.
    /// Passing `None` clears the preference (subsequent renders fall
    /// back to the server default).
    pub fn set_preferred_locale(&mut self, locale: Option<String>) {
        self.preferred_locale = locale;
        self.updated_at = Utc::now();
    }

    /// Whether this user wants to receive an email when someone grants
    /// them access to a resource. `RecipientNotificationService` checks
    /// this on the plain-notification arm; magic-link first-invitations
    /// to external users bypass it (otherwise the recipient could never
    /// claim the share). Defaults TRUE for both the entity constructor
    /// and the schema column.
    pub fn notify_on_share(&self) -> bool {
        self.notify_on_share
    }

    /// Flip the share-notification preference. The caller is expected
    /// to have already validated input shape (the field is a boolean,
    /// so there is no work beyond storage). Bumps `updated_at`.
    pub fn set_notify_on_share(&mut self, notify: bool) {
        self.notify_on_share = notify;
        self.updated_at = Utc::now();
    }

    /// Opaque UI preferences bag. Read-only accessor for the DTO
    /// conversion; mutation goes through the repo's shallow-merge SQL
    /// (`UserPgRepository::update_ui_preferences`) rather than a
    /// setter here — the DB is authoritative on the merged state
    /// because two devices can PATCH concurrently and the merge has
    /// to happen at write time, not at read time.
    pub fn ui_preferences(&self) -> &serde_json::Value {
        &self.ui_preferences
    }

    /// Claim or change the username. Runs the same validation as the
    /// constructor — callers must still ensure uniqueness at the repo
    /// level. Bumps `updated_at`. Used by the post-create profile-edit
    /// endpoint so a user who started with `None` can claim a handle
    /// later, or change to a different one. The home folder name is NOT
    /// renamed: it was display text at creation; the folder is owned
    /// by `user_id`.
    pub fn set_username(&mut self, new_username: String) -> UserResult<()> {
        // Canonical form (trim + lowercase) — see
        // `validate_username`. Callers can pass any case; we store
        // the normalised value.
        self.username = Some(Self::validate_username(&new_username)?);
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Unset the username (return to `None`). Use sparingly — most
    /// users keep their handle once claimed. Mainly here so admin
    /// tooling can clear a problematic handle without deleting the
    /// account.
    pub fn clear_username(&mut self) {
        self.username = None;
        self.updated_at = Utc::now();
    }

    /// Returns true if this is an OIDC-only user (no password)
    pub fn is_oidc_user(&self) -> bool {
        self.federation_issuer.is_some()
    }

    /// Returns true iff this user has any non-magic-link authentication
    /// method available — either a real password hash, or a linked OIDC
    /// subject. Magic-link eligibility for "no other credential" mode is
    /// the negation of this; the `OXICLOUD_MAGIC_LINK_OPEN_TO_PASSWORD_USERS`
    /// flag widens the policy at the service layer (`magic_link_eligibility`).
    pub fn has_login_credential(&self) -> bool {
        self.password_hash.is_some() || self.federation_subject.is_some()
    }

    /// Set the password hash. The new password must be hashed externally
    /// via `PasswordHasherPort` before calling this. Passing `None`
    /// clears the password (e.g. when a user opts back into magic-link-only
    /// auth).
    pub fn update_password_hash(&mut self, new_hash: Option<String>) {
        self.password_hash = new_hash;
        self.updated_at = Utc::now();
    }

    // Update storage usage
    pub fn update_storage_used(&mut self, storage_used_bytes: i64) {
        self.storage_used_bytes = storage_used_bytes;
        self.updated_at = Utc::now();
    }

    // Register login
    pub fn register_login(&mut self) {
        let now = Utc::now();
        self.last_login_at = Some(now);
        self.updated_at = now;
    }

    // Deactivate user
    pub fn deactivate(&mut self) {
        self.active = false;
        self.updated_at = Utc::now();
    }

    // Activate user
    pub fn activate(&mut self) {
        self.active = true;
        self.updated_at = Utc::now();
    }

    // ── Shared validation helpers ──────────────────────────────────────

    /// Usernames are 2-64 chars of `[A-Za-z0-9._-]`. The `@` character is
    /// explicitly forbidden — keeping the username and email namespaces
    /// provably disjoint is what closes the cross-collision attack class
    /// described in the auth-simplification plan (a user can never claim
    /// a handle that shadows another user's email). No leading/trailing
    /// dot or hyphen. The character set also prevents XSS payloads from
    /// being stored as usernames.
    /// Validate AND canonicalise a username.
    ///
    /// Two normalisations run first, before every check:
    /// - `trim()` — strip whitespace clients may have added.
    /// - `to_ascii_lowercase()` — usernames are case-insensitive
    ///   identifiers. Users type `Alice`, `ALICE`, `alice` on
    ///   different clients; all three refer to the same account.
    ///   ASCII-only by construction (charset check below), so
    ///   `to_ascii_lowercase` is deterministic and locale-safe —
    ///   no Unicode case-folding surprises (Turkish dotted-I,
    ///   German ß, Greek final sigma, NFC vs NFD).
    ///
    /// Returns the canonical form on success. Every entity write
    /// site consumes the returned string — because the signature
    /// changed from `Result<()>` to `Result<String>`, any caller
    /// that ignored the result is now a compile error. That's
    /// what forces every write path through the normaliser.
    ///
    /// See `docs/plan/username-lowercase.md` for the full design.
    pub fn validate_username(username: &str) -> UserResult<String> {
        let normalized = username.trim().to_ascii_lowercase();

        let len = normalized.chars().count();
        if !(2..=64).contains(&len) {
            return Err(UserError::InvalidUsername(
                "Username must be between 2 and 64 characters".to_string(),
            ));
        }
        if normalized.contains('@') {
            return Err(UserError::InvalidUsername(
                "Username must not contain '@' — use the email field for email addresses"
                    .to_string(),
            ));
        }
        if !normalized
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(UserError::InvalidUsername(
                "Username may only contain letters, digits, hyphens, underscores, and dots"
                    .to_string(),
            ));
        }
        if normalized.starts_with('.')
            || normalized.starts_with('-')
            || normalized.ends_with('.')
            || normalized.ends_with('-')
        {
            return Err(UserError::InvalidUsername(
                "Username must not start or end with a dot or hyphen".to_string(),
            ));
        }
        Ok(normalized)
    }

    /// Basic but meaningful email validation:
    /// - Must contain exactly one `@`
    /// - Local part and domain must be non-empty
    /// - Domain must contain at least one dot
    /// - No angle brackets, spaces, or other characters used in XSS payloads
    fn validate_email(email: &str) -> UserResult<()> {
        let parts: Vec<&str> = email.splitn(2, '@').collect();
        if parts.len() != 2 {
            return Err(UserError::ValidationError(
                "Invalid email: missing @".to_string(),
            ));
        }
        let (local, domain) = (parts[0], parts[1]);
        if local.is_empty() || domain.is_empty() {
            return Err(UserError::ValidationError(
                "Invalid email: empty local part or domain".to_string(),
            ));
        }
        if !domain.contains('.') {
            return Err(UserError::ValidationError(
                "Invalid email: domain must contain a dot".to_string(),
            ));
        }
        // Reject characters commonly used in XSS / header injection
        let forbidden = [
            '<', '>', '"', '\'', '\\', ' ', '\t', '\n', '\r', '(', ')', ',', ';',
        ];
        if email.chars().any(|c| forbidden.contains(&c)) {
            return Err(UserError::ValidationError(
                "Invalid email: contains forbidden characters".to_string(),
            ));
        }
        if email.len() > 254 {
            return Err(UserError::ValidationError(
                "Invalid email: too long (max 254 characters)".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod role_tests {
    use super::*;

    /// The privilege order `require_role` depends on. Written out rather
    /// than derived, so a variant reorder cannot silently invert it.
    #[test]
    fn anonymous_is_below_user_is_below_admin_is_below_owner() {
        assert!(UserRole::Anonymous.rank() < UserRole::User.rank());
        assert!(UserRole::User.rank() < UserRole::Admin.rank());
        assert!(UserRole::Admin.rank() < UserRole::Owner.rank());

        // The owner satisfies every admin gate without those gates naming
        // them — the property that let `Owner` be added without touching
        // `require_system_admin`, the admin middleware, or the admin
        // service methods.
        assert!(UserRole::Owner.at_least(UserRole::Admin));
        assert!(UserRole::Owner.at_least(UserRole::User));

        // An admin satisfies a "user or better" requirement…
        assert!(UserRole::Admin.at_least(UserRole::User));
        assert!(UserRole::User.at_least(UserRole::User));
        // …and anonymous satisfies neither.
        assert!(!UserRole::Anonymous.at_least(UserRole::User));
        assert!(!UserRole::Anonymous.at_least(UserRole::Admin));
        // The one that matters most: anonymous is never admin.
        assert!(!UserRole::Anonymous.at_least(UserRole::Admin));
    }

    /// The external-identity guards ask `is_privileged`, and they must
    /// keep meaning "more than a plain user" as the roster grows — a role
    /// added above `Admin` has to be caught by the same test that catches
    /// `Admin` today, without anyone remembering to update it.
    ///
    /// Stated as the property rather than as cases: everything ranked
    /// above `User` is privileged, everything at or below is not. A new
    /// variant is covered the moment it has a rank.
    /// `outranks` is STRICT where `at_least` is inclusive, and that
    /// difference is the hierarchy: an admin meets an admin requirement but
    /// may not act on a peer. Without it, one rogue admin can demote every
    /// other admin — the situation issue #690 exists to end.
    #[test]
    fn outranking_is_strict_so_peers_cannot_act_on_each_other() {
        assert!(!UserRole::Admin.outranks(UserRole::Admin));
        assert!(!UserRole::User.outranks(UserRole::User));
        assert!(!UserRole::Owner.outranks(UserRole::Owner));

        assert!(UserRole::Owner.outranks(UserRole::Admin));
        assert!(UserRole::Admin.outranks(UserRole::User));

        // Nothing reaches the owner from below. Stated for every role
        // rather than just admin, because this is the protection the whole
        // feature exists to provide.
        for role in [UserRole::Anonymous, UserRole::User, UserRole::Admin] {
            assert!(
                !role.outranks(UserRole::Owner),
                "{role} must not be able to act on the owner",
            );
        }
    }

    /// The roster grew, so the round-trip has to cover `'owner'` in both
    /// directions. A stored role read back as anything else is the silent
    /// failure Step 0 removed: a privileged account quietly becoming a
    /// plain user, with no error anywhere.
    #[test]
    fn owner_round_trips_through_both_parsers() {
        assert_eq!(UserRole::Owner.as_str(), "owner");
        assert_eq!(UserRole::from_stored("owner"), Some(UserRole::Owner));
        // Inherited by the session parser, so an owner's own JWT does not
        // downgrade them on every request.
        assert_eq!(UserRole::from_session("owner"), Some(UserRole::Owner));

        for role in [UserRole::Owner, UserRole::Admin, UserRole::User] {
            assert_eq!(
                UserRole::from_stored(role.as_str()),
                Some(role),
                "{role} must survive a store/load round-trip",
            );
        }
    }

    #[test]
    fn privileged_means_outranks_a_plain_user() {
        for role in [
            UserRole::Anonymous,
            UserRole::User,
            UserRole::Admin,
            UserRole::Owner,
        ] {
            assert_eq!(
                role.is_privileged(),
                role.rank() > UserRole::User.rank(),
                "{role} disagrees with its own rank about being privileged",
            );
        }

        // Spelled out too, because these are the answers the SQL CHECK
        // `NOT (is_external AND role <> 'user')` has to agree with.
        assert!(UserRole::Admin.is_privileged());
        assert!(!UserRole::User.is_privileged());
        assert!(!UserRole::Anonymous.is_privileged());
    }

    /// `anonymous` has no `auth.users` row, so it must be unparseable from
    /// stored data. The old `_ => UserRole::User` default was fail-open:
    /// an unrecognised value became a real user.
    #[test]
    fn anonymous_is_not_parseable_from_storage() {
        assert_eq!(UserRole::from_stored("admin"), Some(UserRole::Admin));
        assert_eq!(UserRole::from_stored("user"), Some(UserRole::User));
        assert_eq!(UserRole::from_stored("anonymous"), None);
        assert_eq!(UserRole::from_stored("Admin"), None);
        assert_eq!(UserRole::from_stored(""), None);
    }

    /// The wire spelling is a contract — the JWT claim, the audit lines and
    /// the OpenAPI pseudo-scopes all key off it.
    #[test]
    fn wire_spelling_is_stable() {
        assert_eq!(UserRole::Anonymous.as_str(), "anonymous");
        assert_eq!(UserRole::User.as_str(), "user");
        assert_eq!(UserRole::Admin.as_str(), "admin");
        assert!(UserRole::Anonymous.is_anonymous());
        assert!(!UserRole::User.is_anonymous());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_user(
        username: Option<&str>,
        given: Option<&str>,
        family: Option<&str>,
        email: &str,
    ) -> User {
        User::from_data_full(
            Uuid::new_v4(),
            username.map(str::to_string),
            email.to_string(),
            None,
            UserRole::User,
            0,
            0,
            Utc::now(),
            Utc::now(),
            None,
            true,
            None, // federation_kind
            None, // federation_issuer
            None, // federation_subject
            None, // image
            false,
            given.map(str::to_string),
            family.map(str::to_string),
            None,
            None,
            true,
            serde_json::json!({}),
        )
    }

    #[test]
    fn display_full_given_family_with_email() {
        let u = build_user(Some("alice"), Some("Alice"), Some("Smith"), "alice@x.com");
        assert_eq!(u.display_full(true), "Alice Smith <alice@x.com>");
        assert_eq!(u.display_full(false), "Alice Smith");
    }

    #[test]
    fn display_full_given_family_takes_priority_over_username() {
        // Even when the username is set, the full name is more informative
        // and wins. The username surfaces only as part of the address.
        let u = build_user(Some("admin"), Some("Bob"), Some("Jones"), "bob@x.com");
        assert_eq!(u.display_full(true), "Bob Jones <bob@x.com>");
        assert_eq!(u.display_full(false), "Bob Jones");
    }

    #[test]
    fn display_full_username_only() {
        // The "admin" case the user observed: no given/family on the
        // bootstrap admin user. With email → "admin <admin@x.com>";
        // without → just "admin" (compact form for subject lines).
        let u = build_user(Some("admin"), None, None, "admin@x.com");
        assert_eq!(u.display_full(true), "admin <admin@x.com>");
        assert_eq!(u.display_full(false), "admin");
    }

    #[test]
    fn display_full_partial_name_falls_through_to_username() {
        // Given without family (or vice versa) is NOT "rich enough" to
        // use; we walk to the next priority instead of producing a
        // "First <email>" half-name.
        let u = build_user(Some("carol"), Some("Carol"), None, "carol@x.com");
        assert_eq!(u.display_full(true), "carol <carol@x.com>");
        assert_eq!(u.display_full(false), "carol");
    }

    #[test]
    fn display_full_email_only() {
        // External users provisioned via magic-link typically have no
        // username and no given/family — only the email is present.
        // `with_email` is moot here: the email IS the label.
        let u = build_user(None, None, None, "external@x.com");
        assert_eq!(u.display_full(true), "external@x.com");
        assert_eq!(u.display_full(false), "external@x.com");
    }

    #[test]
    fn display_full_partial_name_no_username_falls_to_email() {
        // Lone given_name without family AND without username → falls
        // all the way through to the raw email.
        let u = build_user(None, Some("Solo"), None, "solo@x.com");
        assert_eq!(u.display_full(true), "solo@x.com");
        assert_eq!(u.display_full(false), "solo@x.com");
    }

    // ── validate_username: normalization + rules ─────────────────────────────
    //
    // Post-lowercase-migration `validate_username` returns the canonical
    // (trimmed, lowercased) form on success. Every write-site consumes
    // that returned string via the shadow in `User::new` /
    // `set_username`, so the invariant "usernames in `auth.users` are
    // always canonical" is enforced at the domain boundary.
    //
    // The rules that DON'T change (charset, length, no leading/trailing
    // dot or hyphen, no `@`) get their coverage here too so a future
    // rewrite of `validate_username` can't regress them silently.

    #[test]
    fn validate_username_lowercases_and_trims() {
        // Uppercase in the middle → canonical form is lowercase.
        assert_eq!(User::validate_username("Alice").unwrap(), "alice");
        // All-uppercase.
        assert_eq!(User::validate_username("ALICE").unwrap(), "alice");
        // Whitespace around a mixed-case name → both stripped.
        assert_eq!(User::validate_username("  Alice  ").unwrap(), "alice");
        // Already-canonical passes through unchanged.
        assert_eq!(User::validate_username("alice").unwrap(), "alice");
    }

    #[test]
    fn validate_username_charset_and_boundary_rules_survive_normalization() {
        // Trailing hyphen — still rejected after the case-fold.
        assert!(User::validate_username("alice-").is_err());
        // Leading dot.
        assert!(User::validate_username(".alice").is_err());
        // Whitespace INSIDE the name (not just around it) — the
        // charset check rejects space characters.
        assert!(User::validate_username("Al ice").is_err());
        // Non-ASCII letter — usernames are ASCII-only.
        assert!(User::validate_username("Álice").is_err());
        // `@` is forbidden (disjoint namespace with email lookup).
        assert!(User::validate_username("alice@example").is_err());
    }

    #[test]
    fn validate_username_length_bounds_apply_after_trim() {
        // Two-char minimum satisfied AFTER trim.
        assert_eq!(User::validate_username("  ab  ").unwrap(), "ab");
        // Below the minimum after trim.
        assert!(User::validate_username("  a  ").is_err());
        // Above the maximum after trim.
        let too_long = "a".repeat(65);
        assert!(User::validate_username(&too_long).is_err());
    }
}
