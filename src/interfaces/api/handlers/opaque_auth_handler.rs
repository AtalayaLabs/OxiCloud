//! OPAQUE aPAKE (RFC 9807) HTTP handlers — Phase 1.
//!
//! Two round-trips per operation:
//!
//! ```text
//!   Registration (session-authenticated):
//!     POST /api/auth/opaque/register/start
//!          { registrationRequest: base64 }
//!       → { registrationResponse: base64 }
//!     POST /api/auth/opaque/register/finish
//!          { registrationRecord: base64, ciphersuiteVersion: i16 }
//!       → 204 No Content
//!
//!   Login (unauth) — LANDS IN A LATER STEP OF PHASE 1
//!     POST /api/auth/opaque/login/ke1
//!     POST /api/auth/opaque/login/ke3
//! ```
//!
//! ## Why session-authenticated for registration
//!
//! Registration binds an envelope to a user_id. That id has to come
//! from somewhere the server trusts — not from the request body
//! (attacker could bind an envelope to someone else's account) and
//! not from the OPAQUE handshake itself (the OPAQUE handshake IS
//! what we're bootstrapping; chicken-and-egg). The pragmatic answer,
//! matching Bitwarden / Proton / 1Password: the user proves identity
//! via the existing legacy password login (or magic-link, or OIDC)
//! ONCE, then registers their OPAQUE envelope from that session.
//! Phase 2 wires this as a silent hook after every legacy login.
//!
//! ## Payload encoding
//!
//! All OPAQUE messages are opaque byte blobs. We serialise them as
//! **standard base64** (not URL-safe, no padding-strip) because the
//! WASM client (`@serenity-kit/opaque`) emits the same shape and both
//! ends need to agree on one flavour. Round-tripped through
//! `serde_json` as a `String` field.
//!
//! ## Ciphersuite version handshake
//!
//! `register/finish` requires the client to echo back the
//! `ciphersuiteVersion` it minted the envelope under. That must match
//! the server's currently-configured version — a mismatch means the
//! client cached stale params (or the server rotated the suite). We
//! reject with `OpaqueCiphersuiteMismatch` so the SPA can prompt the
//! user to refresh and try again.
//!
//! ## What this handler does NOT do
//!
//! - Login endpoints (KE1 / KE3) — separate step in Phase 1.
//! - Silent-migration integration into legacy `/api/auth/login` —
//!   Phase 2 concern; this handler ships the ENDPOINT, migration
//!   plumbs the CALL SITE.
//! - Rate limiting — the register endpoints are session-authenticated,
//!   so an attacker would need a stolen session to reach them; the
//!   session's issuance path already carries its own rate limit.
//!   Login endpoints (once shipped) share a budget with legacy login.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use opaque_ke::{RegistrationRequest, RegistrationUpload, ServerRegistration};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::di::AppState;
use crate::infrastructure::services::opaque_service::{OpaqueService, OxiCloudSuite};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::CurrentUserId;

/// Session-required OPAQUE routes. Callers layer the auth + CSRF
/// middlewares in `main.rs` (mirrors [`auth_protected_routes`]).
pub fn opaque_register_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/opaque/register/start", post(register_start))
        .route("/opaque/register/finish", post(register_finish))
}

/// Client → server on register KE1. `registrationRequest` is the
/// base64-encoded output of the client's
/// `ClientRegistration::start(...).message`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct OpaqueRegisterStartDto {
    #[serde(rename = "registrationRequest")]
    pub registration_request: String,
}

/// Server → client on register KE1. `registrationResponse` is the
/// base64-encoded output of `ServerRegistration::start(...).message`.
/// Also echoes `ciphersuiteVersion` so the client can reject a
/// mid-flight suite change and abort before uploading.
#[derive(Debug, Serialize, ToSchema)]
pub struct OpaqueRegisterStartResponse {
    #[serde(rename = "registrationResponse")]
    pub registration_response: String,
    #[serde(rename = "ciphersuiteVersion")]
    pub ciphersuite_version: i16,
}

/// Client → server on register KE2. `registrationRecord` is the
/// base64-encoded output of `ClientRegistration::finish(...).message`;
/// `ciphersuiteVersion` is what the client believed it was minting
/// under (compared to the current server value — mismatch → 400).
#[derive(Debug, Deserialize, ToSchema)]
pub struct OpaqueRegisterFinishDto {
    #[serde(rename = "registrationRecord")]
    pub registration_record: String,
    #[serde(rename = "ciphersuiteVersion")]
    pub ciphersuite_version: i16,
}

/// KE1 (register/start): parse client `RegistrationRequest`, run
/// `ServerRegistration::start`, return the server response.
///
/// No state is persisted server-side by this call — registration is
/// stateless on the server between KE1 and KE2 (the `RegistrationRequest`
/// carries the client's OPRF blinding; the response includes the
/// server's static pubkey; the client alone knows the seed).
#[utoipa::path(
    post,
    path = "/api/auth/opaque/register/start",
    request_body = OpaqueRegisterStartDto,
    responses(
        (status = 200, description = "Server registration response", body = OpaqueRegisterStartResponse),
        (status = 400, description = "Malformed registration request"),
        (status = 401, description = "Not authenticated"),
        (status = 503, description = "OPAQUE service not configured"),
    ),
    security(("bearerAuth" = [])),
    tag = "auth"
)]
pub async fn register_start(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
    Json(dto): Json<OpaqueRegisterStartDto>,
) -> Result<impl IntoResponse, AppError> {
    let svc = require_opaque_service(&state)?;

    let req_bytes = B64
        .decode(dto.registration_request.trim())
        .map_err(|_| malformed("registrationRequest is not valid base64"))?;
    let req = RegistrationRequest::<OxiCloudSuite>::deserialize(&req_bytes)
        .map_err(|_| malformed("registrationRequest failed to deserialize"))?;

    // `user_id` (a UUID) is the OPAQUE server-side user identifier.
    // Encoded as the UUID's raw bytes so the same identifier bytes
    // appear on both sides regardless of string representation — the
    // client passes the same encoding via KE1's login step (Phase 1
    // login endpoints will mirror this decision).
    let user_bytes = user_id.as_bytes();

    let result =
        ServerRegistration::<OxiCloudSuite>::start(svc.setup(), req, user_bytes).map_err(|e| {
            tracing::warn!(
                target: "audit",
                event = "opaque.register_start_failed",
                reason = "server_start_error",
                user_id = %user_id,
                error = %e,
                "OPAQUE server registration start failed"
            );
            AppError::bad_request("OPAQUE server registration failed")
        })?;

    let response_b64 = B64.encode(result.message.serialize());
    Ok(Json(OpaqueRegisterStartResponse {
        registration_response: response_b64,
        ciphersuite_version: svc.ciphersuite_version(),
    }))
}

/// KE2 (register/finish): parse client `RegistrationUpload`, finalise
/// via `ServerRegistration::finish`, persist the resulting envelope
/// bytes. Idempotent-per-user: re-running finish overwrites the
/// existing envelope (COALESCEing `opaque_registered_at`).
#[utoipa::path(
    post,
    path = "/api/auth/opaque/register/finish",
    request_body = OpaqueRegisterFinishDto,
    responses(
        (status = 204, description = "Registration persisted"),
        (status = 400, description = "Malformed record or ciphersuite mismatch"),
        (status = 401, description = "Not authenticated"),
        (status = 503, description = "OPAQUE service not configured"),
    ),
    security(("bearerAuth" = [])),
    tag = "auth"
)]
pub async fn register_finish(
    State(state): State<Arc<AppState>>,
    CurrentUserId(user_id): CurrentUserId,
    Json(dto): Json<OpaqueRegisterFinishDto>,
) -> Result<impl IntoResponse, AppError> {
    let svc = require_opaque_service(&state)?;
    let repo = require_opaque_repo(&state)?;

    // Ciphersuite mismatch is a hard fail — the envelope the client
    // is about to upload would be unusable under the server's current
    // suite, so persisting it would just delay the failure to login
    // time. Reject at KE2 with a machine-readable error_type so the
    // SPA can prompt a page refresh + retry.
    if dto.ciphersuite_version != svc.ciphersuite_version() {
        tracing::info!(
            target: "audit",
            event = "opaque.register_rejected",
            reason = "ciphersuite_mismatch",
            user_id = %user_id,
            client_version = dto.ciphersuite_version,
            server_version = svc.ciphersuite_version(),
            "👮🏻‍♂️ OPAQUE ciphersuite mismatch — client must refresh params and retry"
        );
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "ciphersuiteVersion mismatch: client={}, server={}",
                dto.ciphersuite_version,
                svc.ciphersuite_version()
            ),
            "OpaqueCiphersuiteMismatch",
        ));
    }

    let record_bytes = B64
        .decode(dto.registration_record.trim())
        .map_err(|_| malformed("registrationRecord is not valid base64"))?;
    let record = RegistrationUpload::<OxiCloudSuite>::deserialize(&record_bytes)
        .map_err(|_| malformed("registrationRecord failed to deserialize"))?;

    // `ServerRegistration::finish` in opaque-ke 3.x is pure — it
    // packages the client's upload into a persistable form. No
    // server-side state, no side effect. We write those bytes as the
    // envelope; login-time `ServerLogin::start` will read them back.
    let stored = ServerRegistration::<OxiCloudSuite>::finish(record);
    let envelope_bytes = stored.serialize();

    repo.write_registration(user_id, &envelope_bytes, svc.ciphersuite_version())
        .await
        .map_err(|e| {
            tracing::error!(
                target: "audit",
                event = "opaque.register_persist_failed",
                reason = "repo_write_error",
                user_id = %user_id,
                error = %e,
                "OPAQUE envelope persistence failed"
            );
            AppError::internal_error("OPAQUE envelope persistence failed")
        })?;

    tracing::info!(
        target: "audit",
        event = "opaque.register_ok",
        user_id = %user_id,
        ciphersuite_version = svc.ciphersuite_version(),
        envelope_bytes = envelope_bytes.len(),
        "OPAQUE registration persisted"
    );

    Ok(StatusCode::NO_CONTENT)
}

// ── Small helpers ────────────────────────────────────────────────────

fn require_opaque_service(state: &Arc<AppState>) -> Result<Arc<OpaqueService>, AppError> {
    state.opaque_service.clone().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "OPAQUE is not enabled on this server",
            "OpaqueDisabled",
        )
    })
}

fn require_opaque_repo(
    state: &Arc<AppState>,
) -> Result<Arc<dyn crate::application::ports::opaque_ports::OpaqueRepositoryPort>, AppError> {
    state.opaque_repo.clone().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "OPAQUE persistence is not wired",
            "OpaqueDisabled",
        )
    })
}

fn malformed(msg: &'static str) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, msg, "OpaqueMalformedRequest")
}

// Discourage silent conversions of the `Uuid` extractor into `_` —
// forces future callers to acknowledge it.
#[allow(dead_code)]
fn _uuid_extractor_marker(u: Uuid) -> Uuid {
    u
}
