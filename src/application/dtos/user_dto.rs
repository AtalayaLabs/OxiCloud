use crate::domain::entities::user::User;
use crate::domain::repositories::user_repository::UserListEntry;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserDto {
    pub id: String,
    /// Optional handle. `None` for users who have not claimed one
    /// (externals, fresh email-only signups). Frontend display callers
    /// should walk `username → given/family → email` as their fallback
    /// chain. Omitted from JSON when None (consistent with the existing
    /// given_name / family_name fields).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub email: String,
    pub role: String,
    pub storage_quota_bytes: i64,
    pub storage_used_bytes: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub active: bool,
    /// Which trust chain minted this user's federation identity —
    /// `"oidc" | "ocm" | "magic_link"` — or `None` for pure local
    /// users. Load-bearing for "is this user OIDC?"-shape predicates:
    /// use `federation_kind == "oidc"` rather than string-scraping
    /// `federation_issuer`. Serialized only when populated.
    ///
    /// Mirrors `auth.users.federation_kind` verbatim — same name at
    /// DB, entity, and wire layers so there's no translation to reason
    /// about. See docs/plan/ocm.md § Identity & auth model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub federation_kind: Option<String>,
    /// The authority that mints this user's `federation_subject` —
    /// issuer URL for OIDC (id_token `iss` claim), peer domain for
    /// OCM, `null` for local users (password / OPAQUE only).
    ///
    /// Renamed from `auth_provider` (which was a `String` with the
    /// sentinel `"local"` for non-federated users, and a human-readable
    /// label like `"MockSSO"` before Phase B). This shape mirrors the
    /// `auth.users.federation_issuer` column directly: nullable when
    /// there's no federation involved. FE predicates for "is this user
    /// federated?" should read `federation_kind`, not
    /// string-compare this value.
    ///
    /// When populated, FE code that wants a friendly display label
    /// looks this value up against `OidcProviderInfoDto.issuer →
    /// provider_name` to render the deployment's configured display
    /// name; falls back to the raw issuer for foreign IdPs / legacy
    /// rows still holding a pre-Phase-B label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub federation_issuer: Option<String>,
    pub image: Option<String>,
    pub can_edit_image: bool,
    /// `true` for grant-only external recipients (magic-link, OIDC-only,
    /// future OCM federated). External users have no home folder and
    /// can't own storage; their quota is always 0. Internal users
    /// default to `false`.
    pub is_external: bool,
    /// Optional first/given name. Populated from the OIDC `given_name`
    /// claim at JIT provisioning, or via a profile-edit endpoint.
    /// `None` until explicitly set — `skip_serializing_if = "Option::is_none"`
    /// keeps the wire format compact for the common case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    /// Optional last/family name. Same provenance + serde rules as
    /// `given_name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    /// When the user first demonstrated control of their email (PR 23).
    /// `None` = unverified (omitted from JSON). Stamped on the first
    /// successful magic-link redemption or OIDC JIT with verified
    /// claim. Idempotent — the original timestamp is preserved on
    /// subsequent verifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_verified_at: Option<DateTime<Utc>>,
    /// User-chosen locale for server-rendered surfaces (emails,
    /// future authenticated HTML). `None` = no preference (the server
    /// resolves to `OXICLOUD_DEFAULT_LOCALE` when rendering). Round-trips
    /// through `/api/auth/me` and `PATCH /api/auth/me/profile`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_locale: Option<String>,
    /// Whether the user wants an email when someone shares a resource
    /// with them. `true` (default) = receive share-notification mails;
    /// `false` = grants are still created but no email is sent. Honored
    /// only on the plain-notification path — magic-link first-invitations
    /// to brand-new external users always send, otherwise the recipient
    /// could never claim the share. Round-trips through `/api/auth/me`
    /// and `PATCH /api/auth/me/profile`.
    pub notify_on_share: bool,
    /// Opaque UI preferences bag. Cross-device store for pure UI
    /// toggles (hide dotfiles, view mode, sidebar collapse, …). The
    /// server never inspects the contents — this DTO field just echoes
    /// what was PATCHed via `PATCH /api/auth/me/profile`. Shape is a
    /// JSON object; the frontend defines the keys it cares about (see
    /// `frontend/src/lib/stores/preferences.svelte.ts`). Always present
    /// on the wire; empty bag is `{}`, never `null`.
    pub ui_preferences: serde_json::Value,
    /// Mirrors `auth.users.force_password_change_at_next_login`. Set
    /// TRUE by the admin password-reset flow (see
    /// `AuthApplicationService::admin_reset_password`) and cleared by
    /// a successful self-service `POST /api/auth/change-password`.
    ///
    /// Populated only by the `/api/auth/me` handler and the login
    /// response minter (via a distinct code path). `From<User>` — used
    /// by admin listings, share-recipient responses, group-member DTOs,
    /// etc. — leaves it at `false`. The flag is a per-session-account
    /// concern (does *this* user need to change their password before
    /// they can proceed?), not a general user attribute worth
    /// surfacing on every list row.
    ///
    /// The load-bearing consumer is the SPA's session store: on
    /// startup and after every refresh, `/me` returns the current
    /// flag value and the SPA's nav-guard blocks navigation to
    /// anything but the change-password surface until it flips
    /// back to false. Backend enforcement is separate (see the
    /// `require_no_password_change_pending` middleware) — this DTO
    /// field is what the SPA reads to render the mandatory-mode UI.
    #[serde(default)]
    pub force_password_change: bool,
    /// TRUE when the account has a local Argon2id `password_hash` on
    /// file. Distinct from `federation_kind`: an OIDC-linked account
    /// (`federation_kind == "oidc"`) can ALSO carry a local password if
    /// it was set at signup or later — a hybrid posture. The SPA
    /// gates the profile page's change-password card on this flag,
    /// so hybrid users can rotate their local password even though
    /// they normally sign in via SSO.
    ///
    /// Populated only by the `/api/auth/me` handler. `From<User>` in
    /// this file leaves it `false` — other UserDto emitters (admin
    /// listings, share-recipient responses, group members) do not
    /// need to surface per-user credential state.
    #[serde(default)]
    pub has_password: bool,
    /// TRUE when the caller's current session carries a DPoP JWK
    /// thumbprint (`session.dpop_jkt IS NOT NULL`). Sourced from the
    /// caller's JWT `cnf.jkt` claim — `is_some()` means the session
    /// was bound at token-mint time.
    ///
    /// Populated only by the `/api/auth/me` handler; other UserDto
    /// emitters leave it `false`. The SPA reads this on `session.load()`
    /// to skip a redundant `POST /api/auth/dpop/bind` call when the
    /// session is already bound (which would 409 and log noisily under
    /// the audit stream — see the `already_bound` reject). Only the
    /// OIDC / magic-link redirect flows land here as `false` on first
    /// visit; password login binds at session-mint time so the very
    /// first `/me` after login already reports `true`.
    #[serde(default)]
    pub is_dpop_bound: bool,
}

/// Compact row returned by the paginated admin user table.
///
/// Account-detail fields deliberately do not appear here.  In particular,
/// omitting `image` and `ui_preferences` prevents a 100-row page from turning
/// into tens of MiB when users have uploaded avatars.  `GET /api/admin/users/:id`
/// remains the full-detail endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AdminUserSummaryDto {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub email: String,
    pub role: String,
    pub storage_quota_bytes: i64,
    pub storage_used_bytes: i64,
    pub last_login_at: Option<DateTime<Utc>>,
    pub active: bool,
    /// See `UserDto::federation_kind` — same semantics, same wire spelling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub federation_kind: Option<String>,
    /// See `UserDto::federation_issuer` — same semantics, same wire spelling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub federation_issuer: Option<String>,
    pub is_external: bool,
    /// TRUE when the user has a server-verifiable password on file
    /// (`password_hash IS NOT NULL`). The admin table uses this
    /// alongside `federation_issuer` and `opaque_registered` to render
    /// the user's full capability set: a `password` chip lights up
    /// here, an OIDC provider name renders the SSO badge, an
    /// envelope-on-file flips the OPAQUE chip. A user with none of
    /// the three is passwordless (magic-link only — the SPA renders
    /// a distinct `passwordless` chip in that case). Admin-only
    /// exposure — see the DTO doc for why this isn't on `UserDto`.
    #[serde(default)]
    pub has_password: bool,
    /// Mirrors `UserListEntry::opaque_registered` — TRUE when the user
    /// has an OPAQUE envelope on file. Surfaced on the admin table so
    /// operators can see per-user rollout progress during the
    /// migration window. **Admin-only exposure**: this field is NOT
    /// on `UserDto` — putting it there would leak adoption status
    /// through every user-directory-adjacent endpoint (share targets,
    /// group members, invite listings). `#[serde(default)]` keeps
    /// older SPA builds tolerant of the added field.
    #[serde(default)]
    pub opaque_registered: bool,
    /// Mirrors `UserListEntry::opaque_migrated` — TRUE when the user
    /// has completed at least one successful OPAQUE login. Distinct
    /// from `opaque_registered`: an admin can invalidate the envelope
    /// (`clear_registration`) leaving the user registered=false but
    /// with a historical migrated=true; the SPA's admin table shows
    /// both so this operational nuance is visible.
    #[serde(default)]
    pub opaque_migrated: bool,
}

impl From<UserListEntry> for AdminUserSummaryDto {
    fn from(entry: UserListEntry) -> Self {
        Self {
            id: entry.id.to_string(),
            username: entry.username,
            email: entry.email,
            role: entry.role.to_string(),
            storage_quota_bytes: entry.storage_quota_bytes,
            storage_used_bytes: entry.storage_used_bytes,
            last_login_at: entry.last_login_at,
            active: entry.active,
            federation_kind: entry.federation_kind,
            federation_issuer: entry.federation_issuer,
            is_external: entry.is_external,
            has_password: entry.has_password,
            opaque_registered: entry.opaque_registered,
            opaque_migrated: entry.opaque_migrated,
        }
    }
}

impl From<User> for UserDto {
    fn from(user: User) -> Self {
        // `user` is owned and dropped here, so every owned field is MOVED out
        // via `into_parts` rather than cloned through the borrowing accessors —
        // the accessor form deep-cloned `image` (a data URI up to 512 KiB) and
        // the whole `ui_preferences` JSON tree on every `/api/auth/me` and admin
        // user listing (benches/ROUND20.md §A2). The two derived values read the
        // entity before the move.
        let role = format!("{}", user.role());
        let can_edit_image = !user.is_oidc_user();
        // has_password is derivable from the entity — read before the
        // move. Cheap (bool from Option::is_some), no extra DB round-
        // trip, so From<User> can populate it uniformly rather than
        // leaving it false and requiring per-call-site backfill.
        let has_password = user.has_password();
        let p = user.into_parts();
        Self {
            id: p.id.to_string(),
            username: p.username,
            email: p.email,
            role,
            storage_quota_bytes: p.storage_quota_bytes,
            storage_used_bytes: p.storage_used_bytes,
            created_at: p.created_at,
            updated_at: p.updated_at,
            last_login_at: p.last_login_at,
            active: p.active,
            // NULL on both fields for local users (no federation wired).
            // FE predicates use `!!federation_kind` for "is federated?" —
            // no "local" sentinel string; the null tells the whole story.
            federation_kind: p.federation_kind.map(|k| k.as_str().to_string()),
            federation_issuer: p.federation_issuer,
            image: p.image,
            can_edit_image,
            is_external: p.is_external,
            given_name: p.given_name,
            family_name: p.family_name,
            email_verified_at: p.email_verified_at,
            preferred_locale: p.preferred_locale,
            notify_on_share: p.notify_on_share,
            ui_preferences: p.ui_preferences,
            // Defaults to false. The `/me` handler + the login-response
            // minter populate this via a distinct code path (a
            // repo read that goes through the auth service's cache);
            // admin listings and other UserDto consumers deliberately
            // leave it false — the flag is per-session-account state,
            // not a general user attribute.
            force_password_change: false,
            has_password,
            // Populated only by `/api/auth/me` — the handler overlays
            // the caller's session's actual DPoP binding state after
            // this `From<User>` runs. Other UserDto emitters leave
            // this at `false` (they lack session context).
            is_dpop_bound: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct LoginDto {
    /// Identifier the user typed. Accepts BOTH a username (no `@`) and
    /// an email address (`@` present). The server dispatches on
    /// `@`-in-input: with `@` it looks up by email; without, by
    /// username. The two namespaces are provably disjoint (PR 16
    /// forbids `@` in usernames), so a single field handles both
    /// without ambiguity. The frontend submits whatever the user
    /// typed in the "Username or email" field as-is.
    pub username: String,
    pub password: String,
    /// DPoP JWK thumbprint the client generated at page load. When
    /// present, binds the new session to a browser-held keypair so
    /// stealing the cookie without the private key is useless (RFC
    /// 9449). Absent → session is created unbound (fail-open per the
    /// `docs/plan/dpop.md` threat model). Malformed → 400.
    #[serde(default, rename = "dpop_jkt", alias = "dpopJkt")]
    pub dpop_jkt: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct RegisterDto {
    /// Optional handle (2-64 chars, no `@`). When omitted, the user can
    /// claim one later via the profile-edit endpoint. Users without a
    /// username cannot use NextCloud clients or create app passwords
    /// (Basic-Auth resolves users by username); web UI / native API
    /// works fine without one.
    #[serde(default)]
    pub username: Option<String>,
    pub email: String,
    /// Optional password (≥8 chars when present). When omitted, a
    /// welcome magic-link is mailed to `email` for first-session
    /// bootstrap. The user can later set a password via the
    /// change-password endpoint to switch to classic username/email +
    /// password login.
    #[serde(default)]
    pub password: Option<String>,
}

/// DTO for the one-time initial admin setup endpoint (`/api/setup`).
/// Available only when the system is not yet initialized (no admin exists).
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct SetupAdminDto {
    pub username: String,
    pub email: String,
    pub password: String,
}

/// Partial-update body for `PATCH /api/auth/me/profile` (PR 24).
///
/// Each field is **optional**:
/// - **absent** → no change to that field.
/// - **present** → set / claim.
///
/// **Username is claim-once, immutable.** This endpoint accepts
/// `username` only when the caller currently has none — passing it
/// when one is already claimed is rejected with `409 UsernameImmutable`.
/// The immutability avoids the NextCloud / DAV client breakage that
/// would otherwise come from renaming (paths under
/// `/remote.php/dav/files/{user}/…` and the `verify_url_user` check
/// both bake the username in as a stable identifier). If a user really
/// typoed their handle and needs to fix it, an admin override is the
/// escape hatch.
///
/// **Given / family name** are freely settable. Any non-empty value
/// replaces the current one. Clearing back to `None` is out of scope
/// for v1.
///
/// **OIDC-linked users are rejected wholesale with 403** — their
/// profile fields are managed at the IdP. The IdP is the source of
/// truth; mirroring writes here would just create a divergence.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema, Default)]
pub struct UpdateProfileDto {
    /// Handle to claim (2-64 chars, `[A-Za-z0-9._-]+`, no `@`).
    /// Accepted only when the caller currently has no username. Once
    /// claimed the handle is permanent for the lifetime of the
    /// account; subsequent attempts to set or change it via this
    /// endpoint are rejected with 409. Admin override (via the
    /// admin-create-user / admin-update-user surface, future PR) is
    /// the escape hatch for genuine typos.
    #[serde(default)]
    pub username: Option<String>,
    /// New first/given name. Any non-empty value sets/replaces the
    /// current value. Absent → no change.
    #[serde(default)]
    pub given_name: Option<String>,
    /// New last/family name. Same semantics as `given_name`.
    #[serde(default)]
    pub family_name: Option<String>,
    /// New preferred locale (BCP-47 shape, e.g. `"fr"`, `"zh-TW"`).
    /// Must resolve against the server's `LocaleRegistry` — unknown
    /// codes are rejected with 400. Pass an empty string to clear the
    /// preference back to the server default (the application layer
    /// normalises `""` → `None`).
    #[serde(default)]
    pub preferred_locale: Option<String>,
    /// Whether to receive an email when someone shares a resource with
    /// the user. Absent → no change (existing setting preserved). Pass
    /// `true` to opt in, `false` to opt out. Honored only on the
    /// plain-notification path; magic-link first-invitations to externals
    /// always send.
    #[serde(default)]
    pub notify_on_share: Option<bool>,
    /// Partial patch into the opaque UI preferences bag. **Must be a
    /// JSON object.** Applied via a SHALLOW merge on the server:
    /// keys present here overwrite existing top-level keys; keys not
    /// present survive. A key value of `null` REMOVES that key from
    /// the bag (implemented via `jsonb_strip_nulls` after the merge).
    ///
    /// Example: current bag `{"a":1,"b":2}`, patch `{"b":3,"c":4}`
    /// → merged `{"a":1,"b":3,"c":4}`. Patch `{"a":null}` → `{"b":2}`.
    ///
    /// Absent → no change to the bag. This is a UI-only surface;
    /// server never inspects the keys.
    #[serde(default)]
    pub ui_preferences: Option<serde_json::Value>,
}

impl UpdateProfileDto {
    /// True when the patch touches at least one field whose source of
    /// truth is an external identity provider — currently `given_name`
    /// and `family_name`. For OIDC-managed users the auth service
    /// refuses the whole patch when this returns true (IdP pushes those
    /// fields on every login; editing them here would be silently
    /// overwritten). Local-only fields — `ui_preferences`,
    /// `notify_on_share`, `preferred_locale`, the claim-once `username`
    /// (never re-synced from the IdP) — return `false` so an OIDC user
    /// can still change their view mode, share-mail opt-in, locale,
    /// etc. Add future IdP-authoritative fields (e.g. `email`, `image`)
    /// here if they land in this DTO.
    pub fn touches_idp_managed_fields(&self) -> bool {
        self.given_name.is_some() || self.family_name.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AuthResponseDto {
    pub user: UserDto,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
    /// When `true`, the caller must be routed to the change-password
    /// flow before any other action. Set on the login response for
    /// users whose `auth.users.force_password_change_at_next_login`
    /// column is TRUE — the admin password-reset flow flips that
    /// column atomically alongside `clear_registration` so admin-set
    /// passwords remain temporary until the user picks their own.
    /// Cleared by a successful `POST /api/auth/change-password`.
    ///
    /// SPA policy: if this is `true`, redirect to `/settings/password`
    /// (or the equivalent) immediately after the login handler settles.
    /// Backend does not gate any endpoints on this flag — it's a
    /// soft-enforcement signal; a client that ignores it keeps its
    /// session, but the responsibility falls on the SPA to route
    /// correctly. Backend enforcement (session scope claim) is a
    /// possible follow-up if the soft path proves insufficient.
    #[serde(default)]
    pub force_password_change: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ChangePasswordDto {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RefreshTokenDto {
    pub refresh_token: String,
}

/// Body for `POST /api/auth/upgrade-to-internal`. Converts an
/// authenticated external user into an internal user with their own
/// personal drive.
///
/// `password` is optional — semantics decided per deployment:
///   * If `magic_link` is in `OXICLOUD_AUTH_METHODS` (and OIDC isn't
///     enabled) → password can be omitted; user remains magic-link-only
///     for login after upgrade.
///   * Otherwise → password is required; refusal returns 400
///     `error_type = "PasswordRequired"`. Without it the upgraded user
///     would have no login path.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpgradeToInternalDto {
    #[serde(default)]
    pub password: Option<String>,
}

/// Authenticated current user data (for use in application services)
///
/// Built once per authenticated request in the auth middlewares.
/// `username`/`email` are `Arc<str>` (refcount-bump clones from the cached
/// `TokenClaims` / Basic-auth cache — JSON shape unchanged) and `role` is an
/// inline `SmolStr` ("admin"/"user" fit the 23-byte inline buffer, so the
/// per-request live-role render allocates nothing).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct CurrentUser {
    pub id: Uuid,
    #[schema(value_type = String)]
    pub username: Arc<str>,
    #[schema(value_type = String)]
    pub email: Arc<str>,
    #[schema(value_type = String)]
    pub role: SmolStr,
    /// DPoP session-binding thumbprint threaded from the JWT's
    /// RFC 9449 §5 `cnf.jkt` claim. `None` for unbound sessions
    /// (app passwords, NC clients, pre-DPoP). The DPoP middleware
    /// reads it to enforce "bound → proof required" from an
    /// already-validated token — no session-row lookup on the
    /// hot path (see `docs/plan/dpop.md` Gate 9).
    #[serde(skip)]
    pub dpop_jkt: Option<String>,
}

// ============================================================================
// App Password DTOs
// ============================================================================

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateAppPasswordDto {
    pub label: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AppPasswordCreatedDto {
    pub id: String,
    pub label: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AppPasswordDto {
    pub id: String,
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

// ============================================================================
// OIDC DTOs
// ============================================================================

/// Response with the OIDC authorization URL for client redirect
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OidcAuthorizeResponseDto {
    pub authorize_url: String,
    pub state: String,
}

/// Query parameters received on the OIDC callback
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OidcCallbackQueryDto {
    pub code: String,
    pub state: String,
}

/// Request body for the OIDC one-time code exchange endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OidcExchangeDto {
    pub code: String,
}

/// Information about available OIDC providers + self-service auth
/// methods enabled on the deployment. Consumed by the login page to
/// decide which forms/buttons to render.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OidcProviderInfoDto {
    pub enabled: bool,
    /// The authoritative issuer URL for THIS deployment's OIDC config —
    /// same value that lands on `auth.users.federation_issuer` for
    /// users JIT-provisioned via this IdP.
    ///
    /// Populated so the frontend can resolve display: when
    /// `UserDto.federation_issuer` equals this `issuer`, render
    /// `provider_name` as the human-friendly label (avoids showing raw
    /// issuer URLs like `https://sso.example.com/realms/main` in the
    /// admin badge / profile view). Falls back to the raw issuer when
    /// there's no match — happens for legacy rows not yet lazy-rebound,
    /// or (future) users linked to a different IdP than the currently
    /// configured one.
    ///
    /// Empty string when OIDC is disabled on this deployment.
    #[serde(default)]
    pub issuer: String,
    pub provider_name: String,
    pub authorize_endpoint: String,
    pub password_login_enabled: bool,
    /// True iff the server accepts magic-link login requests
    /// (`OXICLOUD_AUTH_METHODS` includes `magic_link` AND SMTP is
    /// configured). Frontend renders the magic-link form when true.
    #[serde(default)]
    pub magic_link_login_enabled: bool,
    /// True iff `OXICLOUD_REQUIRE_VERIFIED_EMAIL` is set. Frontend uses
    /// this hint to explain the `EmailNotVerified` login response and
    /// to nudge new users toward the magic-link verification path
    /// straight after signup.
    #[serde(default)]
    pub require_verified_email: bool,
    /// True iff the effective allowlist is `[Oidc]` AND the
    /// `auto_redirect_if_standalone_oidc` policy is set. Frontend
    /// uses this to decide whether to auto-redirect to the authorize
    /// endpoint on login-page mount (true) or show a click-to-continue
    /// button (false). Default false — the safe posture that avoids
    /// redirect loops when the IdP is degraded.
    #[serde(default)]
    pub auto_redirect_to_oidc: bool,
}

/// Claims extracted from the validated OIDC ID token
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OidcUserInfoDto {
    pub sub: String,
    pub preferred_username: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub groups: Vec<String>,
}
