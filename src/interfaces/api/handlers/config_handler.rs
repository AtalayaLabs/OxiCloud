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
//!   (CARGO_PKG_VERSION + git SHA). Public build metadata.
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
    /// Server version — `CARGO_PKG_VERSION` from `Cargo.toml`. Matches
    /// what `GET /api/version` returns.
    pub version: &'static str,

    /// Feature flags — which subsystems the server has enabled.
    /// Clients gate optional UI on these (e.g. hide the notification
    /// bell if `features.message_bus` is false, since the bell would
    /// have no delivery channel).
    pub features: FeaturesDto,

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
        version: env!("CARGO_PKG_VERSION"),
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
        },
        server_status: build_header_payload(&state),
    })
}
