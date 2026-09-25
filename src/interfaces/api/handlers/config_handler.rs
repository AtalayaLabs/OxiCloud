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
use crate::interfaces::middleware::server_status::{HeaderPayload, build_header_payload};

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

    /// Live server-status snapshot — exact same shape and field
    /// names as the `X-Server-Status` response header. Clients use
    /// this to hydrate their reactive store at boot; subsequent live
    /// changes propagate through the header on every other request
    /// (the middleware and this endpoint share `build_header_payload`
    /// so drift is impossible). Non-optional so the client always
    /// has a definite value; `readonly: false` with no `migration`
    /// or `rotation` is the "everything nominal" case.
    pub server_status: HeaderPayload,
}

/// Auth block within [`ServerConfigDto`]. Non-privacy-implicating
/// tunables the SPA needs at boot so form validation matches server
/// enforcement.
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
    Json(ServerConfigDto {
        version: env!("OXICLOUD_VERSION"),
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
        },
        auth: AuthDto {
            min_password_length: state.core.config.auth.min_password_length,
            // OpaqueConfig lives at `config.opaque`, not `config.auth.opaque`
            // — top-level sibling of `auth`. Grouping the mode under
            // `auth` on the wire is a UX choice for FE consumers who
            // expect all identity-related tunables side-by-side.
            opaque_mode: state.core.config.opaque.mode.as_str(),
        },
        server_status: build_header_payload(&state),
    })
}
