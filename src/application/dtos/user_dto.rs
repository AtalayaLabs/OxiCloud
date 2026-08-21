use crate::domain::entities::user::User;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

// ────────────────────────────────────────────────────────────────────────
// Three-layer user DTO family — see docs/plan/userdto-refactor.md.
//
// `PublicUserDto`  — public identity. Every authenticated caller may see it.
//                    Returned by /api/users/{id}, share responses, group
//                    members, magic-link invitees, recipient enrichment.
// `FullUserDto`    — `{ user: PublicUserDto, ...admin+self extras }`.
//                    Returned as `Vec<FullUserDto>` by /api/admin/users;
//                    embedded in `SelfUserDto`. Closest DTO to the
//                    `auth.users` row.
// `SelfUserDto`    — `{ full: FullUserDto, ...self-only extras }`. Returned
//                    by /api/auth/me and by every AuthResponseDto path.
//
// Adding a field? Decide by audience:
//   * Any authenticated caller may see it about another user → `PublicUserDto`.
//   * Only admin (about another user) AND self (about self) → `FullUserDto`.
//   * Only self about themselves → `SelfUserDto`.
// ────────────────────────────────────────────────────────────────────────

/// Public identity — what any authenticated caller may see about ANOTHER
/// user. Returned by `/api/users/{id}` and everywhere a user is
/// referenced by another surface (share responses, group members,
/// magic-link invitees, recipient enrichment).
///
/// This is the audience-narrowest DTO: adding a field here means every
/// authenticated caller can see it about every visible user. Fields that
/// are meaningful only to the subject themselves (preferences, session
/// state) or only to an admin (auth adoption signals) belong on
/// [`SelfUserDto`] or [`FullUserDto`] respectively.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicUserDto {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub email: String,
    /// Role string ("admin" | "user"). Kept public because the sharee /
    /// group-member vignette renders an admin badge.
    pub role: String,
    /// Avatar payload (base64 data-URI up to 512 KiB). Public so a share
    /// picker can render the recipient's face directly. Will move to a
    /// dedicated avatar endpoint in a future refactor — this shape is
    /// transitional.
    pub image: Option<String>,
    /// `true` for grant-only external recipients (magic-link, OIDC-only,
    /// future OCM federated). Renders the "external" badge on the vignette.
    pub is_external: bool,
    /// Optional first/given name. Social identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    /// Optional last/family name. Social identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    /// Presence signal — TRUE when the server observed a request on any
    /// of this user's non-revoked sessions within the last
    /// [`ONLINE_WINDOW`](crate::application::dtos::session_dto::ONLINE_WINDOW)
    /// (5 min). Sourced from an EXISTS subquery when the DTO is built
    /// from a list-projection path; single-user endpoints that don't
    /// enrich presence ship `false`.
    #[serde(default)]
    pub is_online: bool,
}

/// Full user record — public identity + all fields BOTH an admin
/// (viewing another user) AND the subject themselves may see. Returned
/// as `Vec<FullUserDto>` by `/api/admin/users`; embedded in
/// [`SelfUserDto`] for `/api/auth/me`.
///
/// This is the DTO closest to the underlying `auth.users` row. Adding a
/// field here means an admin looking at any user can see it, and the
/// subject themselves can see it in their `/me` response — but the field
/// stays off the public [`PublicUserDto`] surface.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FullUserDto {
    /// Public identity — same set every authenticated caller sees.
    pub user: PublicUserDto,
    /// Which trust chain minted this user's federation identity —
    /// `"oidc" | "ocm" | "magic_link"` — or `None` for pure local users.
    /// Kept off `PublicUserDto` because a peer's federation kind is a
    /// soft org-affiliation leak; only self + admin need it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub federation_kind: Option<String>,
    /// The authority that minted this user's `federation_subject` —
    /// issuer URL for OIDC (id_token `iss` claim), peer domain for OCM,
    /// `None` for local users. Same rationale as `federation_kind`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub federation_issuer: Option<String>,
    /// Subject's own locale preference. Only THEY or an admin managing
    /// them needs this — other callers use their own locale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_locale: Option<String>,
    /// When the user first demonstrated control of their email. Trust
    /// signal — meaningful to admin (auditing verification status) and
    /// to self (own record), but not to a share picker rendering a
    /// vignette.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_verified_at: Option<DateTime<Utc>>,
    /// Row bookkeeping.
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Activity signal — private to the subject; admin sees it too.
    pub last_login_at: Option<DateTime<Utc>>,
    /// Account-active flag — a deactivated user couldn't reach `/me`
    /// anyway, but admin needs to see it.
    pub active: bool,
    /// Storage quotas — personal financials. Admin manages others';
    /// self sees own.
    pub storage_quota_bytes: i64,
    pub storage_used_bytes: i64,
    /// TRUE when the account has a server-verifiable password
    /// (`password_hash IS NOT NULL`). Kept off `PublicUserDto` because
    /// per-user auth adoption leaks through directory endpoints.
    pub has_password: bool,
    /// TRUE when the user has an OPAQUE envelope on file.
    pub opaque_registered: bool,
    /// TRUE when the user has completed ≥1 successful OPAQUE login.
    /// Distinct from `opaque_registered`: an admin can invalidate the
    /// envelope leaving the user registered=false but with historical
    /// migrated=true.
    pub opaque_migrated: bool,
}

/// Self view — everything the caller may see about themselves.
/// Returned by `/api/auth/me` and by every `AuthResponseDto` path
/// (login / refresh / OIDC callback / magic-link redemption).
///
/// Composed on top of [`FullUserDto`] so `/me` and `/admin/users` share
/// the SAME "full profile" contract for the fields both need — new
/// self+admin-visible fields go on `FullUserDto` and both endpoints get
/// them together. Fields here are pure self-scoped state: preferences,
/// session-scoped flags, and caller-scoped permissions.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SelfUserDto {
    /// Full profile — same shape as one row of `/api/admin/users`.
    pub full: FullUserDto,
    /// Opaque UI preferences bag — my own UI state. Cross-device store
    /// for pure UI toggles (view mode, sidebar collapse, hide dotfiles,
    /// …). The server never inspects the contents. Always present on
    /// the wire; empty bag is `{}`, never `null`.
    pub ui_preferences: serde_json::Value,
    /// Whether I want share-notification emails.
    pub notify_on_share: bool,
    /// Session-scoped: my current session carries a DPoP thumbprint.
    /// SPA reads this on `session.load()` to skip a redundant
    /// `POST /api/auth/dpop/bind` when the session is already bound.
    pub is_dpop_bound: bool,
    /// Admin-set temp-password gate — SPA nav guard blocks everything
    /// but `/change-password` until this flips back. Cleared by a
    /// successful `POST /api/auth/change-password`.
    pub force_password_change: bool,
    /// Caller-scoped permission: can I edit my own avatar? `false` for
    /// OIDC users whose avatar comes from the IdP. Only meaningful when
    /// caller == subject; nonsense on any other DTO.
    pub can_edit_image: bool,
}

impl From<User> for PublicUserDto {
    fn from(user: User) -> Self {
        let role = format!("{}", user.role());
        let p = user.into_parts();
        Self {
            id: p.id.to_string(),
            username: p.username,
            email: p.email,
            role,
            image: p.image,
            is_external: p.is_external,
            given_name: p.given_name,
            family_name: p.family_name,
            // Single-user paths that don't enrich presence ship `false`.
            // List projections (admin users, sharees enriched with
            // presence) build via FullUserDto::build below, which
            // overrides this from UserDerivedFlags.
            is_online: false,
        }
    }
}

impl FullUserDto {
    /// Construct a `FullUserDto` from a `User` entity plus the DB-derived
    /// flags the entity doesn't carry (`has_password`, OPAQUE flags,
    /// `is_online`). Both are typically produced together by the users
    /// list repo projection.
    ///
    /// Not a `From` impl because it takes two arguments; not a `From
    /// <(User, UserDerivedFlags)>` because that reads awkwardly at
    /// callsites — `FullUserDto::build(user, flags)` is clearer.
    pub fn build(
        user: User,
        flags: crate::domain::repositories::user_repository::UserDerivedFlags,
    ) -> Self {
        let role = format!("{}", user.role());
        let p = user.into_parts();
        Self {
            user: PublicUserDto {
                id: p.id.to_string(),
                username: p.username,
                email: p.email,
                role,
                image: p.image,
                is_external: p.is_external,
                given_name: p.given_name,
                family_name: p.family_name,
                is_online: flags.is_online,
            },
            federation_kind: p.federation_kind.map(|k| k.as_str().to_string()),
            federation_issuer: p.federation_issuer,
            preferred_locale: p.preferred_locale,
            email_verified_at: p.email_verified_at,
            created_at: p.created_at,
            updated_at: p.updated_at,
            last_login_at: p.last_login_at,
            active: p.active,
            storage_quota_bytes: p.storage_quota_bytes,
            storage_used_bytes: p.storage_used_bytes,
            has_password: flags.has_password,
            opaque_registered: flags.opaque_registered,
            opaque_migrated: flags.opaque_migrated,
        }
    }
}

impl SelfUserDto {
    /// Assemble the `/me` response from a `FullUserDto` plus the two
    /// session-scoped booleans that can't be derived from `User` alone:
    /// the caller's DPoP-binding state (from the JWT `cnf.jkt` claim)
    /// and the admin-set force-password-change flag (from the auth
    /// service's cache).
    ///
    /// The other self-only fields (`ui_preferences`, `notify_on_share`,
    /// `can_edit_image`) come from `User` and are read off the entity
    /// before it's moved into the FullUserDto; this method takes those
    /// as explicit parameters so the caller can decide when to read
    /// them (typically at the same point they read the DPoP-binding
    /// state).
    pub fn build(
        full: FullUserDto,
        ui_preferences: serde_json::Value,
        notify_on_share: bool,
        is_dpop_bound: bool,
        force_password_change: bool,
        can_edit_image: bool,
    ) -> Self {
        Self {
            full,
            ui_preferences,
            notify_on_share,
            is_dpop_bound,
            force_password_change,
            can_edit_image,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// End of three-layer user DTO family.
// ────────────────────────────────────────────────────────────────────────

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
    /// Full self view — identical shape to `/api/auth/me`. Every
    /// login / refresh / OIDC-callback / magic-link redemption ships
    /// this so the SPA's post-auth state matches its post-`/me` state
    /// (no UI race between `AuthResponseDto` and the first `/me`
    /// fetch). See `docs/plan/userdto-refactor.md` § Endpoint mapping.
    pub user: SelfUserDto,
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
    /// `PublicUserDto.federation_issuer` equals this `issuer`, render
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

#[cfg(test)]
mod three_layer_quarantine {
    use super::*;
    use serde_json::Value;

    /// Structural-quarantine guard for `SelfUserDto`. The self-only
    /// bag (`ui_preferences`, `notify_on_share`, `is_dpop_bound`,
    /// `force_password_change`, `can_edit_image`) MUST live at the
    /// top level, NOT nested inside `.full` or `.full.user`. If a
    /// future refactor accidentally moves one of them down, the
    /// wire shape leaks it through every `PublicUserDto` /
    /// `FullUserDto` emitter (share responses, group members,
    /// `/api/admin/users`, magic-link invitees) — exactly what the
    /// three-layer split exists to prevent. Fails loudly here.
    #[test]
    fn self_only_fields_stay_at_top_level_of_self_user_dto() {
        let self_dto = SelfUserDto {
            full: FullUserDto {
                user: PublicUserDto {
                    id: "00000000-0000-0000-0000-000000000001".into(),
                    username: None,
                    email: "self@example.invalid".into(),
                    role: "user".into(),
                    image: None,
                    is_external: false,
                    given_name: None,
                    family_name: None,
                    is_online: false,
                },
                federation_kind: None,
                federation_issuer: None,
                preferred_locale: None,
                email_verified_at: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                last_login_at: None,
                active: true,
                storage_quota_bytes: 0,
                storage_used_bytes: 0,
                has_password: true,
                opaque_registered: false,
                opaque_migrated: false,
            },
            ui_preferences: serde_json::json!({}),
            notify_on_share: true,
            is_dpop_bound: false,
            force_password_change: false,
            can_edit_image: true,
        };
        let json: Value = serde_json::to_value(&self_dto).expect("SelfUserDto serialises");
        assert!(
            json.get("ui_preferences").is_some(),
            "top-level ui_preferences"
        );
        assert!(
            json.get("full")
                .expect("full block")
                .get("ui_preferences")
                .is_none(),
            "ui_preferences must NOT appear inside `.full`"
        );
        assert!(
            json.pointer("/full/user/ui_preferences").is_none(),
            "ui_preferences must NOT appear inside `.full.user`"
        );
        // Same guard for the other self-only fields.
        for k in [
            "notify_on_share",
            "is_dpop_bound",
            "force_password_change",
            "can_edit_image",
        ] {
            assert!(json.get(k).is_some(), "{k} at top level");
            assert!(
                json.pointer(&format!("/full/{k}")).is_none(),
                "{k} must NOT nest in .full"
            );
            assert!(
                json.pointer(&format!("/full/user/{k}")).is_none(),
                "{k} must NOT nest in .full.user"
            );
        }
    }

    /// Structural-quarantine guard for `FullUserDto`. Admin-visible
    /// extras (`has_password`, OPAQUE flags, `federation_*`,
    /// `last_login_at`, `active`, quotas, `preferred_locale`,
    /// `email_verified_at`) MUST live at the top level of
    /// `FullUserDto`, NOT inside `.user`. If a future refactor
    /// accidentally lifts one of them onto `PublicUserDto` (the
    /// embedded `user` field), it leaks through `/api/users/{id}`
    /// and every other public directory endpoint.
    #[test]
    fn admin_only_fields_stay_at_top_level_of_full_user_dto() {
        let full = FullUserDto {
            user: PublicUserDto {
                id: "00000000-0000-0000-0000-000000000002".into(),
                username: Some("bob".into()),
                email: "bob@example.invalid".into(),
                role: "user".into(),
                image: None,
                is_external: false,
                given_name: None,
                family_name: None,
                is_online: false,
            },
            federation_kind: None,
            federation_issuer: None,
            preferred_locale: None,
            email_verified_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_login_at: None,
            active: true,
            storage_quota_bytes: 10_737_418_240,
            storage_used_bytes: 0,
            has_password: true,
            opaque_registered: false,
            opaque_migrated: false,
        };
        let json: Value = serde_json::to_value(&full).expect("FullUserDto serialises");
        for k in [
            "has_password",
            "opaque_registered",
            "opaque_migrated",
            "last_login_at",
            "active",
            "storage_quota_bytes",
            "storage_used_bytes",
        ] {
            assert!(json.get(k).is_some(), "{k} at top level of FullUserDto");
            assert!(
                json.pointer(&format!("/user/{k}")).is_none(),
                "{k} must NOT nest in .user"
            );
        }
    }
}
