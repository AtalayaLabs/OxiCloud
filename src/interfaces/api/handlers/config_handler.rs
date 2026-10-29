//! `GET /api/config` — public server-configuration discovery.
//!
//! Advertises the subset of `AppState` a client needs to know at boot:
//! feature flags (which optional systems are enabled), server version,
//! and the current server-status snapshot (matches whatever the
//! `X-Server-Status` header carries live). Everything auth-related
//! stays under `GET /api/auth/oidc/providers` — the two endpoints are
//! sibling capability advertisements, not one canonical thing.
//!
//! # Scope
//!
//! Only fields with **no privacy implications**:
//!
//! - `features.*` — boolean matrix of enabled subsystems (message bus,
//!   trash, search, sharing, quotas, plugins, WOPI). Same information
//!   any logged-in caller could infer from probing endpoints; giving
//!   it up front is a UX win.
//! - `version` — same string the `/api/version` endpoint returns
//!   (`OXICLOUD_VERSION` from `build.rs`). Public build metadata.
//! - `server_status` — a snapshot of the mutable server-status state
//!   (maintenance mode, degraded mode, etc.). Same shape the
//!   `X-Server-Status` header stamps on every response; this endpoint
//!   just lets the FE hydrate the store at boot without waiting for
//!   the first authenticated response.
//!
//! Anything requiring auth (per-user preferences, admin-visible
//! deployment secrets, session state) does NOT go here — those live
//! on `/api/auth/me` or `/api/admin/*`.

use std::sync::Arc;

use axum::{Json, extract::State};
use serde::Serialize;

use crate::common::di::AppState;

/// Server-configuration DTO. Additive over time — clients ignore
/// unknown fields, and no field is ever repurposed (same discipline
/// as JSON-RPC error codes on the message bus).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ServerConfigDto {
    /// Server version — `OXICLOUD_VERSION` derived by `build.rs` from
    /// the git tag / `GITHUB_REF_NAME` / `git describe`. Matches what
    /// `GET /api/version` returns. Cargo.toml stays pinned at `0.0.0`;
    /// this string is the canonical build identity.
    pub version: &'static str,

    /// Feature flags — which subsystems the server has enabled.
    /// Clients gate optional UI on these (e.g. hide the notification
    /// bell if `features.message_bus` is false, since the bell would
    /// have no delivery channel).
    pub features: FeaturesDto,

    /// Auth-related tunables the SPA needs to gate submit locally.
    /// Composes with `/api/auth/oidc/providers` — that endpoint lists
    /// federation options; this one carries the numeric thresholds
    /// (password length today, more later) that the form has to check
    /// before firing the request.
    pub auth: AuthDto,

    /// `true` once the deployment has completed first-run setup
    /// (admin exists OR the admin-settings DB flag is set). FE
    /// login page gates: `false` → render the admin-setup wizard;
    /// `true` → render the login form. Replaces the dedicated
    /// `/api/auth/status` endpoint, which was retired on the same
    /// branch as this field (the other three fields it carried
    /// were already redundant with `/api/config` and `/api/admin/*`).
    pub initialized: bool,
    // `server_status` is deliberately NOT on `/api/config`. The
    // whole runtime-status surface — readonly, migration, rotation,
    // holder, banners — lives on authenticated `GET /api/status`
    // + the `X-Server-Status` response header, so no runtime state
    // (and no operator-authored banner text) leaks to anonymous
    // callers. If the server is in maintenance mode, login
    // attempts are rejected with a maintenance reason by the auth
    // handler — that's the signal path, not a pre-auth banner.
}

/// Auth block within [`ServerConfigDto`]. Non-privacy-implicating
/// tunables the SPA needs at boot so form validation matches server
/// enforcement, and the complete per-method enablement matrix the
/// login page uses to pick which forms + buttons to render.
///
/// Sub-objects per auth concern (one of `registration` /
/// `password` / `magic_link` / `oidc`) rather than flat booleans so
/// each method's future knobs land as siblings inside its own
/// sub-object without widening `AuthDto` itself. The ONLY flat
/// fields at the top are cross-cutting policies (`require_verified_email`
/// gates both registration AND login) and operator-tuned scalars
/// the SPA forms gate submit on (`min_password_length`, `opaque_mode`).
///
/// Replaces the pre-consolidation surface where `/api/auth/oidc/providers`
/// returned the same four auth-method flags under flat names
/// (`password_login_enabled`, `magic_link_login_enabled`,
/// `require_verified_email`, `auto_redirect_to_oidc`). That endpoint
/// was retired in favour of this one.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AuthDto {
    /// Minimum password length (UTF-8 bytes). Mirrors
    /// `AuthConfig::min_password_length`, which every server-side
    /// password-bearing endpoint uses. The SPA gates submit on the
    /// same value so users see the failure instantly instead of
    /// after a round-trip. See AtalayaLabs/OxiCloud#677.
    pub min_password_length: u8,

    /// OPAQUE deployment mode — one of `"off"`, `"migrate"`,
    /// `"opaque_only"`. Mirrors `AuthConfig::opaque.mode` (env
    /// `OXICLOUD_AUTH_OPAQUE_MODE`). Login already discovers this
    /// per-user via `POST /api/auth/opaque/login/lookup`; exposing
    /// the server-wide mode here is for future consumers (admin
    /// panel status indicators, OPAQUE-native setup / register
    /// flows). Login callers should NOT branch on this value —
    /// keep using the per-user lookup so mixed populations during
    /// migration stay honest.
    pub opaque_mode: &'static str,

    /// `true` iff `OXICLOUD_REQUIRE_VERIFIED_EMAIL` is set.
    /// Cross-cutting policy — gates both registration (new users
    /// must verify) and login (existing users without a verified
    /// address are refused). Lives at top level because it isn't
    /// owned by any single sub-object. The FE uses this hint to
    /// explain the `EmailNotVerified` login response and to nudge
    /// new users toward the magic-link verification path straight
    /// after signup.
    pub require_verified_email: bool,

    /// Public self-registration flow. See [`RegistrationDto`].
    pub registration: RegistrationDto,

    /// Password-login method. See [`PasswordAuthDto`].
    pub password: PasswordAuthDto,

    /// Magic-link login + welcome flows. See [`MagicLinkAuthDto`].
    pub magic_link: MagicLinkAuthDto,

    /// OIDC / SSO federation. See [`OidcAuthDto`].
    pub oidc: OidcAuthDto,
}

/// Public-registration block within [`AuthDto`]. Reflects the
/// precedence rule of
/// `AdminSettingsService::get_registration_enabled` (env
/// `OXICLOUD_DISABLE_REGISTRATION` > DB
/// `admin_settings.registration_enabled` > default-true), but through
/// the `AppState::registration_enabled` atomic — seeded at boot,
/// flipped at admin-write time, so `/api/config` never disagrees with
/// the enforcement in `POST /api/auth/register`. The FE login page
/// gates the "Create one" button on this; subsequent admin toggles
/// propagate because `ServerStatusChanged`'s version hash covers this
/// field (see `compute_server_status_version`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RegistrationDto {
    /// `true` when `POST /api/auth/register` is accepting requests.
    /// `false` when disabled by env var or admin UI.
    pub enabled: bool,
}

/// Password-login block within [`AuthDto`]. Mirrors the policy
/// `AuthApplicationService::is_password_login_allowed()` — composes
/// `OXICLOUD_AUTH_METHODS` with the AuthZ cross-check (an OIDC-master
/// deployment turns password login off even when the env list carries
/// `password`). FE login page uses this to decide whether to render
/// the password form at all.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PasswordAuthDto {
    /// `true` when the server accepts password-based login.
    pub enabled: bool,
}

/// Magic-link login block within [`AuthDto`]. Mirrors
/// `AuthApplicationService::is_magic_link_login_allowed()`, which
/// composes `OXICLOUD_AUTH_METHODS` membership + SMTP wiring + the
/// "OIDC-master → no magic-link login" hard rule. FE login page
/// shows the magic-link form iff this is true.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct MagicLinkAuthDto {
    /// `true` when the server accepts magic-link login requests.
    pub enabled: bool,
}

/// OIDC / SSO block within [`AuthDto`]. Carries the enablement
/// flag plus a list of configured providers. Today OxiCloud
/// supports one OIDC provider per deployment, so `providers` is
/// either a one-element list or empty — but the array shape is
/// future-proof for the planned multi-provider work (one deployment
/// federating multiple IdPs) and lets the FE loop over providers
/// without an array-vs-scalar refactor later.
///
/// Deployment-wide policies (`enabled`, `auto_redirect`) stay on
/// the sub-object top; per-provider identity (`issuer`,
/// `provider_name`, `authorize_endpoint`) lives inside each
/// [`OidcProviderDto`].
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OidcAuthDto {
    /// `true` when OIDC federation is configured + enabled.
    /// When `false`, `providers` is empty and the FE hides every
    /// SSO affordance.
    pub enabled: bool,

    /// `true` iff the effective allowlist is `[Oidc]` AND the
    /// `auto_redirect_if_standalone_oidc` policy is set. FE uses
    /// this to decide whether to auto-redirect to the authorize
    /// endpoint on login-page mount (true) or show a
    /// click-to-continue button (false). Default `false` — the
    /// safe posture that avoids redirect loops when the IdP is
    /// degraded.
    pub auto_redirect: bool,

    /// Configured providers. Length is 0 (OIDC off) or 1 (single
    /// provider, today's reality); FE treats >1 as a future
    /// multi-provider deployment and renders one button per entry.
    pub providers: Vec<OidcProviderDto>,
}

/// A single OIDC provider inside [`OidcAuthDto::providers`]. All
/// fields are per-provider; deployment-wide flags stay on the
/// parent sub-object.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OidcProviderDto {
    /// Authoritative issuer URL for this provider — same value
    /// that lands on `auth.users.federation_issuer` for users
    /// JIT-provisioned via it. The FE compares a user's
    /// `federation_issuer` against this to decide whether to
    /// render the human-friendly `provider_name` instead of the
    /// raw issuer URL. Doubles as the stable key when iterating
    /// providers client-side (unique per deployment).
    pub issuer: String,

    /// Human-readable name (e.g. "Google", "Keycloak — Main Realm").
    pub provider_name: String,

    /// Browser-facing navigation target for the sign-in button —
    /// the SPA sets `window.location` to this verbatim. Carries
    /// the deployment base path (`OXICLOUD_BASE_PATH`-prefixed).
    pub authorize_endpoint: String,
}

/// Feature-flag block within [`ServerConfigDto`]. One boolean per
/// optional subsystem. Adding a new feature: append a field with a
/// default that matches the server-side default; NEVER remove a field
/// (client code may depend on the absence of a `false` value to mean
/// "unknown").
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct FeaturesDto {
    /// Message bus over WebSocket. When `false`, `/api/rt/ws` and
    /// `/api/rt/ticket` are not registered — clients skip WS setup
    /// entirely. See `FeaturesConfig::enable_message_bus`.
    pub message_bus: bool,
    /// Recycle bin / soft-delete flow. When `false`, deletes are
    /// permanent — no `/api/trash` endpoint. See
    /// `FeaturesConfig::enable_trash`.
    pub trash: bool,
    /// Full-text and metadata search (`/api/search/*`). See
    /// `FeaturesConfig::enable_search`.
    pub search: bool,
    /// File sharing (public share links + user-to-user grants). See
    /// `FeaturesConfig::enable_file_sharing`.
    pub sharing: bool,
    // NOTE: no `quotas` field. The former `enable_user_storage_quotas`
    // flag was removed (dead config with zero consumers). Actual
    // per-user quotas are set via the admin panel and resolved by
    // `StorageUsageService` unconditionally.
    /// Music player + playlists. See `FeaturesConfig::enable_music`.
    pub music: bool,
    /// Photo-map ("Places") tab. See `FeaturesConfig::enable_places`.
    pub places: bool,
    /// Face detection + identity clustering ("People"). Biometric —
    /// OFF by default. See `FeaturesConfig::enable_faces`.
    pub faces: bool,
    /// Server-side video-thumbnail generation via ffmpeg. See
    /// `FeaturesConfig::enable_video_thumbnails`.
    pub video_thumbnails: bool,
    /// Admin-configured external filesystem mounts. See
    /// `FeaturesConfig::enable_external_mounts`.
    pub external_mounts: bool,
    /// Collaborative `.md` editing over the message-bus WebSocket
    /// (Yjs CRDT). When `false`, the FE hides the "New markdown"
    /// menu entry and falls back to the plain `.md` viewer when
    /// opening one. See `FeaturesConfig::enable_markdown_collab`.
    pub markdown_collab: bool,
    /// Shared-worker-backed message bus — one `/api/rt/ws`
    /// connection per BROWSER rather than per tab. Server-side wire
    /// shape is identical; the flag gates FE behaviour only.
    /// Default `true` (opt-out). See
    /// `FeaturesConfig::enable_shared_worker_bus` and
    /// `docs/plan/message-bus-via-shared-worker.md` for the rollout
    /// story and fallback matrix.
    pub shared_worker_bus: bool,
}

/// `GET /api/config` — return the public server-configuration
/// snapshot. Unauthenticated. No cache header — values change on
/// server-restart / feature-toggle / status flip, and the endpoint
/// is called at most once per SPA boot per client. Adding a short
/// `Cache-Control` TTL later is safe if load ever becomes a concern.
#[utoipa::path(
    get,
    path = "/api/config",
    tag = "config",
    responses(
        (status = 200, description = "Public server configuration", body = ServerConfigDto),
    ),
)]
pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<ServerConfigDto> {
    let f = &state.core.config.features;

    // Auth-method matrix lives on the AuthApplicationService — it
    // composes `OXICLOUD_AUTH_METHODS` with cross-rules
    // (OIDC-master-disables-magic-link, SMTP-missing-disables-magic-link,
    // etc.) that a flat env read would miss. When no auth service is
    // configured (`state.auth_service.is_none()`), fall back to a
    // "everything off" matrix so `/api/config` stays reachable at
    // boot even if a misconfiguration kept the auth service from
    // being built — the FE will then hide every auth form, which is
    // the correct degraded UX.
    let auth_app = state
        .auth_service
        .as_ref()
        .map(|s| s.auth_application_service.clone());

    // First-run initialization probe — ported from the retired
    // `/api/auth/status` handler. The DB flag is the authoritative
    // source; the `admin_count > 0` fallback covers legacy
    // deployments where the flag wasn't backfilled but at least
    // one admin exists. The FE login page gates the setup wizard
    // on this: `false` → render the setup form, `true` → render
    // the login form. Defaults to `true` on service-missing so a
    // misconfigured instance doesn't trap users in setup mode.
    let initialized = {
        let db_initialized = match state.admin_settings_service.as_ref() {
            Some(admin_svc) => admin_svc.is_system_initialized().await,
            None => false,
        };
        let admin_count = match auth_app.as_ref() {
            Some(auth) => auth.count_admin_users().await.unwrap_or(0),
            None => 0,
        };
        db_initialized || admin_count > 0
    };
    let password_login_enabled = auth_app
        .as_ref()
        .map(|a| a.is_password_login_allowed())
        .unwrap_or(false);
    let magic_link_login_enabled = auth_app
        .as_ref()
        .map(|a| a.is_magic_link_login_allowed())
        .unwrap_or(false);
    let require_verified_email = auth_app
        .as_ref()
        .map(|a| a.require_verified_email())
        .unwrap_or(false);
    let auto_redirect_to_oidc = auth_app
        .as_ref()
        .map(|a| a.auto_redirect_to_oidc())
        .unwrap_or(false);
    let oidc_enabled = auth_app.as_ref().map(|a| a.oidc_enabled()).unwrap_or(false);

    // OIDC provider list — a one-element array today (single-provider
    // reality), an empty one when OIDC is disabled, forward-compatible
    // with the planned multi-provider work. Prefer the DISCOVERY
    // document's cached issuer over the operator-typed `issuer_url`
    // (same reasoning as the retired `/api/auth/oidc/providers`
    // handler had: discovery publishes the canonical value, config
    // only points at where discovery lives). Cache-only lookup to
    // keep this unauthenticated endpoint from becoming a DoS
    // amplifier against the IdP.
    let oidc_providers: Vec<OidcProviderDto> =
        if oidc_enabled && let Some(auth_app) = auth_app.as_ref() {
            let config = auth_app.oidc_config();
            let provider_name = config
                .as_ref()
                .map(|c| c.provider_name.clone())
                .unwrap_or_default();
            let issuer = auth_app
                .oidc_service()
                .and_then(|svc| svc.cached_issuer())
                .or_else(|| config.map(|c| c.issuer_url))
                .unwrap_or_default();
            let authorize_endpoint = state.core.config.prefixed_path("/api/auth/oidc/authorize");
            vec![OidcProviderDto {
                issuer,
                provider_name,
                authorize_endpoint,
            }]
        } else {
            Vec::new()
        };

    Json(ServerConfigDto {
        version: env!("OXICLOUD_VERSION"),
        initialized,
        features: FeaturesDto {
            message_bus: f.enable_message_bus,
            trash: f.enable_trash,
            search: f.enable_search,
            sharing: f.enable_file_sharing,
            music: f.enable_music,
            places: f.enable_places,
            faces: f.enable_faces,
            video_thumbnails: f.enable_video_thumbnails,
            external_mounts: f.enable_external_mounts,
            markdown_collab: f.enable_markdown_collab,
            shared_worker_bus: f.enable_shared_worker_bus,
        },
        auth: AuthDto {
            min_password_length: state.core.config.auth.min_password_length,
            // OpaqueConfig lives at `config.opaque`, not `config.auth.opaque`
            // — top-level sibling of `auth`. Grouping the mode under
            // `auth` on the wire is a UX choice for FE consumers who
            // expect all identity-related tunables side-by-side.
            opaque_mode: state.core.config.opaque.mode.as_str(),
            require_verified_email,
            registration: RegistrationDto {
                enabled: state
                    .registration_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
            },
            password: PasswordAuthDto {
                enabled: password_login_enabled,
            },
            magic_link: MagicLinkAuthDto {
                enabled: magic_link_login_enabled,
            },
            oidc: OidcAuthDto {
                enabled: oidc_enabled,
                auto_redirect: auto_redirect_to_oidc,
                providers: oidc_providers,
            },
        },
    })
}
