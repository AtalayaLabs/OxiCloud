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
//! All OPAQUE messages are opaque byte blobs, round-tripped through
//! `serde_json` as a `String` field. The server emits **URL-safe-no-pad**
//! base64 (`-`/`_`, no `=`) via `B64.encode(...)` — the WASM client
//! (`@serenity-kit/opaque`) rejects standard base64 with an
//! `Invalid symbol` error on the first `+`/`/`. On decode the server
//! accepts BOTH flavours via `decode_opaque_b64` so the Rust
//! `opaque-hurl-helper` test binary (which emits standard) still
//! round-trips. Asymmetry is intentional: URL-safe is the compatible
//! superset for the client mix we support.
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
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::{
    STANDARD as B64_STANDARD, URL_SAFE_NO_PAD as B64_URL_NO_PAD,
};

/// Decode base64 payloads received from OPAQUE clients, accepting BOTH
/// standard (`+`/`/`, padded) and URL-safe-no-pad (`-`/`_`, no padding)
/// alphabets. `@serenity-kit/opaque` (the WASM client the SPA uses)
/// emits URL-safe-no-pad; the `opaque-hurl-helper` binary and the
/// original spec docs use standard. Accepting both means neither
/// side has to renormalize.
fn decode_opaque_b64(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let trimmed = input.trim();
    B64_STANDARD
        .decode(trimmed)
        .or_else(|_| B64_URL_NO_PAD.decode(trimmed))
}

/// Encode OPAQUE payloads emitted BY the server. Emits **URL-safe-no-pad**
/// (`-`/`_`, no `=`) because the WASM client (`@serenity-kit/opaque` —
/// the SPA's OPAQUE library) decodes strictly as URL-safe-no-pad and
/// rejects standard base64 with an `Invalid symbol` error on the first
/// `+`/`/` in the payload. Rust `opaque-hurl-helper` and any other
/// client parses through `decode_opaque_b64` above which accepts BOTH
/// flavours, so this direction is asymmetric on purpose — URL-safe is
/// the compatible superset for the client mix we support.
const B64: base64::engine::general_purpose::GeneralPurpose = B64_URL_NO_PAD;
use opaque_ke::{
    CredentialFinalization, CredentialRequest, RegistrationRequest, RegistrationUpload,
    ServerLoginStartParameters, ServerRegistration,
};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::dtos::user_dto::AuthResponseDto;
use crate::common::di::AppState;
use crate::infrastructure::services::opaque_login_exchange::{ExchangeId, OpaqueLoginExchange};
use crate::infrastructure::services::opaque_service::{OpaqueService, OxiCloudSuite};
use crate::interfaces::api::cookie_auth;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::CurrentUserId;

/// Session-required OPAQUE routes. Callers layer the auth + CSRF
/// middlewares in `main.rs` (mirrors [`auth_protected_routes`]).
/// Session-required OPAQUE register routes. Mount under
/// `/api/auth/opaque/register` in `main.rs` with the auth + CSRF
/// layer stack (mirrors [`auth_protected_routes`]).
///
/// **The mount prefix MUST be distinct from `opaque_login_routes`'s
/// prefix.** Nesting both routers at the same `/api/auth` node
/// causes axum's `.layer()` to cross-apply between siblings on
/// shared prefixes — the login endpoints would then inherit
/// register's auth+CSRF middleware and 403 every unauthenticated
/// KE1. Separate prefixes side-step the composition rule cleanly.
pub fn opaque_register_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/start", post(register_start))
        .route("/finish", post(register_finish))
}

/// Public (unauthenticated) OPAQUE login routes. Mount under
/// `/api/auth/opaque/login` with the same `rate_limit_login`
/// layer used on legacy `/api/auth/login` so an attacker can't
/// halve the per-identity budget by spraying both endpoints. See
/// [`opaque_register_routes`] on why the prefix must be distinct.
///
/// `lookup` lives here (not on the params mount) so it shares the
/// login rate limiter — without that, an attacker could use it as a
/// cheaper user-existence probe than legacy login. Anti-enum on the
/// response shape closes the primary information leak; the rate
/// limit closes the secondary "how fast can I ask" leak.
pub fn opaque_login_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/lookup", post(login_lookup))
        .route("/ke1", post(login_ke1))
        .route("/ke3", post(login_ke3))
}

/// Public config-publish endpoint. Mount under `/api/auth/opaque`
/// with NO rate limit — it's a static config read the SPA hits
/// once at page load (cache-friendly). Kept distinct from the
/// login mount to avoid layering the login rate limiter on a read
/// that isn't a login attempt.
pub fn opaque_params_routes() -> Router<Arc<AppState>> {
    Router::new().route("/params", get(opaque_params))
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
///
/// `ksf*` fields carry the Argon2id parameters the client actually
/// used at `startRegistration`/`finishRegistration` time. Server
/// persists them per-envelope so future changes to the server's
/// `OpaqueConfig::ksf_*` don't invalidate this envelope. Optional
/// on the wire (older clients that predate per-envelope storage
/// omit them; server falls back to its current config in that case).
#[derive(Debug, Deserialize, ToSchema)]
pub struct OpaqueRegisterFinishDto {
    #[serde(rename = "registrationRecord")]
    pub registration_record: String,
    #[serde(rename = "ciphersuiteVersion")]
    pub ciphersuite_version: i16,
    #[serde(rename = "ksfMemoryKib", default)]
    pub ksf_memory_kib: Option<u32>,
    #[serde(rename = "ksfIterations", default)]
    pub ksf_iterations: Option<u32>,
    #[serde(rename = "ksfParallelism", default)]
    pub ksf_parallelism: Option<u32>,
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

    let req_bytes = decode_opaque_b64(&dto.registration_request).map_err(|e| {
        tracing::info!(
            target: "audit",
            event = "opaque.register_start_rejected",
            reason = "malformed_base64",
            user_id = %user_id,
            error = %e,
            "👮🏻‍♂️ OPAQUE register/start rejected: registrationRequest is not valid base64"
        );
        malformed("registrationRequest is not valid base64")
    })?;
    let req = RegistrationRequest::<OxiCloudSuite>::deserialize(&req_bytes).map_err(|e| {
        tracing::info!(
            target: "audit",
            event = "opaque.register_start_rejected",
            reason = "malformed_registration_request",
            user_id = %user_id,
            error = %e,
            "👮🏻‍♂️ OPAQUE register/start rejected: RegistrationRequest deserialize failed"
        );
        malformed("registrationRequest failed to deserialize")
    })?;

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

    let record_bytes = decode_opaque_b64(&dto.registration_record)
        .map_err(|_| malformed("registrationRecord is not valid base64"))?;
    let record = RegistrationUpload::<OxiCloudSuite>::deserialize(&record_bytes)
        .map_err(|_| malformed("registrationRecord failed to deserialize"))?;

    // `ServerRegistration::finish` in opaque-ke 3.x is pure — it
    // packages the client's upload into a persistable form. No
    // server-side state, no side effect. We write those bytes as the
    // envelope; login-time `ServerLogin::start` will read them back.
    let stored = ServerRegistration::<OxiCloudSuite>::finish(record);
    let envelope_bytes = stored.serialize();

    // KSF params the client used. Client-declared per migration
    // 20261005000000; older clients omit and we fall back to the
    // server's CURRENT config values (best guess: the client fetched
    // /params right before registering, so current-config is what it
    // saw). Persisting the exact per-envelope values means future
    // config changes don't break this envelope on login.
    let ksf = crate::application::ports::opaque_ports::StoredKsf {
        memory_kib: dto
            .ksf_memory_kib
            .unwrap_or_else(|| svc.config_ksf_memory_kib()),
        iterations: dto
            .ksf_iterations
            .unwrap_or_else(|| svc.config_ksf_iterations()),
        parallelism: dto
            .ksf_parallelism
            .unwrap_or_else(|| svc.config_ksf_parallelism()),
    };

    repo.write_registration(user_id, &envelope_bytes, svc.ciphersuite_version(), ksf)
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

// ── Login: lookup (Phase 3) ──────────────────────────────────────────

/// Client → server on lookup. Same identifier shape as
/// [`OpaqueLoginKe1Dto::user_identifier`] and legacy
/// `/api/auth/login`'s `username` field: `@` in the input dispatches
/// to email lookup, absence → username lookup.
#[derive(Debug, Deserialize, ToSchema)]
pub struct OpaqueLookupDto {
    #[serde(rename = "userIdentifier")]
    pub user_identifier: String,
}

/// Server → client on lookup. The SPA reads `hasOpaque` to decide
/// whether to run OPAQUE login (KE1/KE3) or fall back to legacy
/// `/api/auth/login` (which then silently registers via the Phase 2
/// hook).
///
/// **Anti-enum invariant** — this shape MUST be identical whether
/// the user exists or not:
///
///   * unknown user           → `hasOpaque: false`
///   * user, no envelope      → `hasOpaque: false`
///   * user with envelope     → `hasOpaque: true`
///
/// A probing attacker cannot distinguish "unknown" from "known but
/// unregistered" from the response body. The only signal an
/// attacker gets is "known with envelope" vs "everything else,"
/// which reveals adoption progress but NOT identity existence.
#[derive(Debug, Serialize, ToSchema)]
pub struct OpaqueLookupResponse {
    #[serde(rename = "hasOpaque")]
    pub has_opaque: bool,
    /// KSF parameters this user's envelope was minted under. Present
    /// only when `has_opaque = true`. The client MUST use these values
    /// (not the ones from `GET /params`) on the login handshake — the
    /// envelope's OPRF derivation was bound to them at register time
    /// and a mismatch will fail the AKE with `InvalidCredentials`.
    ///
    /// `None` when: (a) `has_opaque = false` (nothing to publish),
    /// or (b) the envelope predates per-envelope-KSF storage
    /// (migration `20261005000000`) — in which case the client falls
    /// back to `/params` values, which is the same behaviour as
    /// before per-envelope storage existed.
    ///
    /// Anti-enum note: the presence of this field ONLY signals what
    /// `has_opaque` already signals (positive existence). Value
    /// differences across users could reveal timing of registration
    /// but not identity — same low-severity leak as the existing
    /// per-identifier probe, no additional exposure.
    #[serde(rename = "ksf", skip_serializing_if = "Option::is_none")]
    pub ksf: Option<OpaqueKsfParams>,
}

/// Resolve `userIdentifier` → envelope-existence check. Used by the
/// SPA login form to branch between OPAQUE and legacy on submit.
///
/// Rate-limited via the shared `login_limiter` (mount layer in
/// `main.rs`), so lookup can't be used as a cheap enumeration probe.
#[utoipa::path(
    post,
    path = "/api/auth/opaque/login/lookup",
    request_body = OpaqueLookupDto,
    responses(
        (status = 200, description = "Whether the identifier resolves to a user with an OPAQUE envelope", body = OpaqueLookupResponse),
        (status = 400, description = "Malformed request body"),
        (status = 503, description = "OPAQUE service not configured"),
    ),
    tag = "auth"
)]
pub async fn login_lookup(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<OpaqueLookupDto>,
) -> Result<impl IntoResponse, AppError> {
    let _svc = require_opaque_service(&state)?;
    let repo = require_opaque_repo(&state)?;
    let auth = require_auth_application_service(&state)?;

    let identifier = dto.user_identifier.trim();
    if identifier.is_empty() {
        return Err(malformed("userIdentifier is empty"));
    }

    // Resolve the identifier → user_id → envelope presence + KSF.
    // Any miss (unknown user, user without envelope, DB blip) collapses
    // to `hasOpaque: false, ksf: None` — the anti-enum contract on the
    // wire shape. No audit event here: a successful lookup isn't a
    // login attempt, and logging every miss would flood the channel
    // without adding signal (rate limiter already caps volume;
    // enumeration attempts show up in the login-lockout / rate-limit
    // metrics).
    //
    // KSF fallback: if the envelope has NULL KSF (predates per-envelope
    // storage migration 20261005000000), we return `ksf: None` — the
    // client then uses the server's current `/params` values, which is
    // the pre-per-envelope-storage behaviour.
    let (has_opaque, ksf) = match auth.lookup_user_for_login(identifier).await {
        Ok(user) => match repo.read_registration(user.id()).await {
            Ok(Some(stored)) => {
                let ksf = stored.ksf.map(|k| OpaqueKsfParams {
                    memory_kib: k.memory_kib,
                    iterations: k.iterations,
                    parallelism: k.parallelism,
                });
                (true, ksf)
            }
            _ => (false, None),
        },
        Err(_) => (false, None),
    };

    Ok(Json(OpaqueLookupResponse { has_opaque, ksf }))
}

// ── Login: KE1 + KE3 ─────────────────────────────────────────────────

/// Client → server on KE1. `userIdentifier` is the same string the
/// SPA sends to legacy `/api/auth/login` — email if it contains `@`,
/// username otherwise (server dispatches on `@`, same as legacy).
/// `startLoginRequest` is the base64-encoded output of
/// `ClientLogin::start(...).message`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct OpaqueLoginKe1Dto {
    #[serde(rename = "userIdentifier")]
    pub user_identifier: String,
    #[serde(rename = "startLoginRequest")]
    pub start_login_request: String,
}

/// Server → client on KE1. `exchangeId` is an opaque handle the
/// client MUST echo back on KE3 (single-use, 60 s TTL); the client
/// can't derive server state from it. `loginResponse` is the
/// base64-encoded output of `ServerLogin::start(...).message`.
#[derive(Debug, Serialize, ToSchema)]
pub struct OpaqueLoginKe1Response {
    #[serde(rename = "exchangeId")]
    #[schema(value_type = String, format = "uuid")]
    pub exchange_id: ExchangeId,
    #[serde(rename = "loginResponse")]
    pub login_response: String,
}

/// Client → server on KE3. `exchangeId` matches what KE1 handed
/// back; `finishLoginRequest` is the base64-encoded output of
/// `ClientLogin::finish(...).message`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct OpaqueLoginKe3Dto {
    #[serde(rename = "exchangeId")]
    #[schema(value_type = String, format = "uuid")]
    pub exchange_id: ExchangeId,
    #[serde(rename = "finishLoginRequest")]
    pub finish_login_request: String,
    /// DPoP JWK thumbprint the client generated at page load. When
    /// present, binds the new session to a browser-held keypair (RFC
    /// 9449). Absent → session created unbound. See `docs/plan/dpop.md`.
    #[serde(default, rename = "dpopJkt", alias = "dpop_jkt")]
    pub dpop_jkt: Option<String>,
}

/// KE1: user lookup → envelope fetch → `ServerLogin::start` → stash
/// state under a fresh exchange_id → return handle + response bytes.
///
/// ## Anti-enumeration
///
/// A KE1 that CAN'T be honestly answered (unknown user, or user
/// exists but has no OPAQUE envelope yet) MUST look identical to a
/// KE1 that CAN. opaque-ke's `ServerLogin::start` supports a "dummy"
/// branch (`password_file = None`) that generates a well-formed
/// KE2 response indistinguishable from the real branch. The
/// dummy-branch KE3 will fail at the client-side `ClientLogin::finish`
/// (wrong MAC), never reaching the server — so KE3 for the
/// non-existent user is symmetric with KE3 for a wrong passphrase.
///
/// User is NOT looked up via `_with_perms` — this is the auth
/// bootstrap; there's no caller identity yet. We use the same
/// dispatch as legacy `/api/auth/login`.
#[utoipa::path(
    post,
    path = "/api/auth/opaque/login/ke1",
    request_body = OpaqueLoginKe1Dto,
    responses(
        (status = 200, description = "Login response + single-use exchange handle",
            body = OpaqueLoginKe1Response),
        (status = 400, description = "Malformed request payload"),
        (status = 503, description = "OPAQUE service not configured"),
    ),
    tag = "auth"
)]
pub async fn login_ke1(
    State(state): State<Arc<AppState>>,
    Json(dto): Json<OpaqueLoginKe1Dto>,
) -> Result<impl IntoResponse, AppError> {
    let svc = require_opaque_service(&state)?;
    let repo = require_opaque_repo(&state)?;
    let exchange = require_opaque_exchange(&state)?;

    let cred_bytes = decode_opaque_b64(&dto.start_login_request)
        .map_err(|_| malformed("startLoginRequest is not valid base64"))?;
    let cred_request = CredentialRequest::<OxiCloudSuite>::deserialize(&cred_bytes)
        .map_err(|_| malformed("startLoginRequest failed to deserialize"))?;

    // Resolve identifier → user_id, then fetch the envelope. Both
    // steps can fail (unknown user, no envelope) — collapse into a
    // single Option so `ServerLogin::start` sees the anti-enum
    // shape cleanly.
    let (user_bytes, password_file) = resolve_user_and_envelope(&state, &dto.user_identifier)
        .await
        .unwrap_or_else(|| (dto.user_identifier.as_bytes().to_vec(), None));

    // `ServerLogin::start(..., Some(file), ...)` runs the real
    // handshake; `None` runs the dummy branch that still produces a
    // well-formed KE2 tied to the same suite so a probing attacker
    // can't distinguish the two paths from response shape or timing
    // (opaque-ke pads the dummy branch to match).
    let mut server_rng = OsRng;
    let started = opaque_ke::ServerLogin::start(
        &mut server_rng,
        svc.setup(),
        password_file,
        cred_request,
        &user_bytes,
        ServerLoginStartParameters::default(),
    )
    .map_err(|e| {
        // A start error at this stage is a genuine protocol failure
        // (bad KE1 payload, ciphersuite mismatch in the wire bytes).
        // We STILL respond 400 rather than 200-with-dummy — 200
        // response would let the attacker distinguish "protocol
        // error" from "unknown user"; the caller getting a 400 has
        // to try a different KE1 anyway.
        tracing::info!(
            target: "audit",
            event = "opaque.login_ke1_rejected",
            reason = "server_start_error",
            attempted_identifier = %dto.user_identifier,
            error = %e,
            "👮🏻‍♂️ OPAQUE KE1 rejected: protocol error"
        );
        AppError::new(
            StatusCode::BAD_REQUEST,
            "OPAQUE login start failed",
            "OpaqueMalformedRequest",
        )
    })?;

    // Silence unused-warning for repo when the branch above returns
    // None — repo IS used inside `resolve_user_and_envelope` (via
    // `state.opaque_repo`) but the closure hides that from the
    // compiler's flow analysis.
    let _ = &repo;

    // Stash user_id alongside ServerLogin so KE3 knows which account
    // just proved possession of the passphrase without having to
    // re-parse the AKE payload (opaque-ke doesn't hand the identifier
    // back on `finish` — it's checked implicitly against the KE1
    // state's expected identifier).
    let user_id = user_id_from_bytes(&user_bytes);
    let response_b64 = B64.encode(started.message.serialize());
    let exchange_id = exchange.store(started.state, user_id);
    Ok(Json(OpaqueLoginKe1Response {
        exchange_id,
        login_response: response_b64,
    }))
}

/// KE3: consume exchange state → `ServerLogin::finish` → mint session.
///
/// On success:
///
///   * Stamp `opaque_migrated_at` (Phase 3 signal that this user
///     has completed at least one OPAQUE login — future legacy
///     login attempts will be refused once Phase 4 flips to
///     `opaque_only`).
///   * Mint access + refresh tokens under a fresh session family,
///     via `AuthApplicationService::mint_session_for_authenticated_user`.
///   * Return the shared `AuthResponseDto` shape identical to
///     `POST /api/auth/login` — the SPA consumes both paths through
///     one downstream handler.
///
/// On failure (bad passphrase, replay, expired exchange, unknown
/// exchange_id): 401 `InvalidCredentials` — the SAME shape for every
/// failure branch so attackers can't distinguish "expired" from
/// "wrong password" from "id already consumed".
#[utoipa::path(
    post,
    path = "/api/auth/opaque/login/ke3",
    request_body = OpaqueLoginKe3Dto,
    responses(
        (status = 200, description = "Session issued", body = AuthResponseDto),
        (status = 400, description = "Malformed request payload"),
        (status = 401, description = "Invalid credentials"),
        (status = 503, description = "OPAQUE service not configured"),
    ),
    tag = "auth"
)]
pub async fn login_ke3(
    State(state): State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(dto): Json<OpaqueLoginKe3Dto>,
) -> Result<impl IntoResponse, AppError> {
    let _svc = require_opaque_service(&state)?;
    let repo = require_opaque_repo(&state)?;
    let exchange = require_opaque_exchange(&state)?;
    let auth = require_auth_application_service(&state)?;

    // Atomic take FIRST — before touching the payload. Two reasons:
    //
    //   1. Anti-enum: an unknown / expired / already-consumed
    //      exchange_id returns 401 `InvalidCredentials` regardless
    //      of payload shape, matching the wrong-passphrase case
    //      exactly. If we parsed the payload first, a malformed
    //      body would 400 EVEN for an unknown id — leaking the fact
    //      that some KE3 shapes are valid vs invalid.
    //   2. Anti-replay: consuming the exchange first guarantees the
    //      handle is single-use even if the caller sends garbage
    //      afterwards — an attacker with a stolen id can't spam
    //      "try shapes until one parses" against the same handle.
    let stash = exchange.take(dto.exchange_id).ok_or_else(|| {
        tracing::info!(
            target: "audit",
            event = "opaque.login_ke3_rejected",
            reason = "unknown_or_expired_exchange",
            exchange_id = %dto.exchange_id,
            "👮🏻‍♂️ OPAQUE KE3 rejected: unknown/expired/replayed exchange_id"
        );
        invalid_credentials()
    })?;

    let cred_bytes = decode_opaque_b64(&dto.finish_login_request)
        .map_err(|_| malformed("finishLoginRequest is not valid base64"))?;
    let cred_final = CredentialFinalization::<OxiCloudSuite>::deserialize(&cred_bytes)
        .map_err(|_| malformed("finishLoginRequest failed to deserialize"))?;

    // If the AKE integrity check fails (wrong passphrase, dummy-branch
    // KE1 for an unknown user, tampered bytes), `finish` errors and
    // we return the same 401 shape as an unknown exchange_id above.
    let _finished = stash.state.finish(cred_final).map_err(|e| {
        tracing::info!(
            target: "audit",
            event = "opaque.login_ke3_rejected",
            reason = "ake_check_failed",
            exchange_id = %dto.exchange_id,
            error = %e,
            "👮🏻‍♂️ OPAQUE KE3 rejected: AKE / passphrase check failed"
        );
        invalid_credentials()
    })?;

    // Both sides now agree on a shared session_key. The current
    // implementation does NOT derive the bearer token from it —
    // reusing the existing token minter keeps the session shape
    // identical to legacy login. Cryptographically tying the
    // access_token to `session_key` via HKDF is a follow-up (see
    // docs/plan/opaque.md §Step 5 notes).
    //
    // The user_id comes from the KE1-side stash (resolved from the
    // client's identifier before the dummy-vs-real branch), not from
    // the AKE payload. Anti-enum: the dummy branch has
    // `stash.user_id = None`; a dummy KE3 that somehow reached this
    // point (should not — it fails at `finish` above) returns the
    // same InvalidCredentials shape.
    let user_id = stash.user_id.ok_or_else(invalid_credentials)?;

    // Fetch the user entity — needed by mint_session_for_authenticated_user
    // (it calls dispatch_login + register_login + generates tokens
    // from the user's role/email/etc.).
    let user = auth.get_user_entity(user_id).await.map_err(|_| {
        // User row vanished between KE1's envelope fetch and now
        // (delete race). Same shape as bad passphrase — never
        // leak "you passed the crypto but the account is gone."
        tracing::warn!(
            target: "audit",
            event = "opaque.login_ke3_rejected",
            reason = "user_gone_after_ke3",
            user_id = %user_id,
            "👮🏻‍♂️ OPAQUE KE3: user disappeared between KE1 and KE3"
        );
        invalid_credentials()
    })?;

    // Capture client IP + User-Agent so `sessions.ip_address` /
    // `user_agent` land populated instead of NULL (admin panel would
    // otherwise render "—"). Both are per-session and only refresh
    // on rotation, matching the login pattern.
    let client_ip = crate::interfaces::middleware::trusted_proxy::client_ip_from_parts(
        &headers,
        Some(peer),
        false,
    );
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Mint the session BEFORE stamping opaque_migrated_at — if the
    // session mint fails (rare, but not impossible under DB failure),
    // we don't want to have flipped the migration flag for a user
    // whose login didn't actually complete.
    let session = auth
        .mint_session_for_authenticated_user(user, dto.dpop_jkt, Some(client_ip), user_agent)
        .await
        .map_err(AppError::from)?;

    if let Err(e) = repo.mark_migrated(user_id).await {
        // Non-fatal: session is already issued, user is logged in.
        // Log at warn so ops sees any consistent trend, but don't
        // fail the response — the mark is idempotent so a later
        // login will retry.
        tracing::warn!(
            target: "audit",
            event = "opaque.mark_migrated_deferred",
            user_id = %user_id,
            error = %e,
            "OPAQUE login succeeded but mark_migrated failed — will retry on next login"
        );
    }

    tracing::info!(
        target: "audit",
        event = "opaque.login_ok",
        user_id = %user_id,
        "OPAQUE login completed"
    );

    // Mint the HttpOnly auth cookies + double-submit CSRF cookie —
    // the SAME shape the legacy `/api/auth/login` handler emits. The
    // SPA reads the CSRF cookie into an `X-CSRF-Token` header on every
    // mutating request, and gates its "login succeeded" branch on the
    // cookie being present (`csrfCookiePresent()` in the login page).
    // Returning just `Json(session)` without touching cookies — the
    // shape this handler used before — silently broke the browser
    // login flow: session tokens were valid but no cookies landed, so
    // the SPA reported "Login succeeded but the browser rejected the
    // session cookie" even on http+cookie_secure=false deployments.
    let mut response = (StatusCode::OK, Json(&session)).into_response();
    cookie_auth::append_auth_cookies(
        response.headers_mut(),
        &session.access_token,
        &session.refresh_token,
        session.expires_in,
        state.core.config.auth.refresh_token_expiry_secs,
    );
    cookie_auth::append_csrf_cookie(response.headers_mut(), session.expires_in);
    Ok(response)
}

// ── Params publish ───────────────────────────────────────────────────

/// Client-side Argon2id parameters the SPA must feed to
/// `@serenity-kit/opaque` on `finishRegistration` / `finishLogin`.
/// Values MUST match the server's `OpaqueConfig::ksf_*` — the
/// handshake fails to derive matching keys otherwise, so publishing
/// these is what keeps client and server in lock-step across param
/// bumps.
#[derive(Debug, Serialize, ToSchema)]
pub struct OpaqueKsfParams {
    #[serde(rename = "memoryKib")]
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

/// Payload of `GET /api/auth/opaque/params`. `enabled = false` when
/// the OPAQUE substrate is not wired for this deployment (mode=off,
/// or password auth disabled — the same cross-check
/// `OpaqueConfig::effective_mode` runs). SPA gates the OPAQUE code
/// path on this flag; when false, it falls back to legacy password
/// auth as if OPAQUE didn't exist.
///
/// `ciphersuiteVersion` + `ksf` are ALWAYS populated (safe defaults
/// even when `enabled = false`) so a client that ignored `enabled`
/// wouldn't nil-deref.
#[derive(Debug, Serialize, ToSchema)]
pub struct OpaqueParamsResponse {
    pub enabled: bool,
    #[serde(rename = "ciphersuiteVersion")]
    pub ciphersuite_version: i16,
    pub ksf: OpaqueKsfParams,
}

/// Publish the OPAQUE client config. Safe to call unauthenticated
/// (nothing about individual users is returned) and cache-friendly
/// (the response only changes when the operator rotates env vars).
///
/// Note: `Cache-Control` is deliberately unset — the SPA fetches
/// this once at page load, and if the operator rotates the KSF
/// params mid-flight, we want the change to be picked up on the
/// next SPA reload rather than lingering behind an intermediary
/// cache.
#[utoipa::path(
    get,
    path = "/api/auth/opaque/params",
    responses(
        (status = 200, description = "OPAQUE client config", body = OpaqueParamsResponse),
    ),
    tag = "auth"
)]
pub async fn opaque_params(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Reads from OpaqueService when substrate is wired; falls back
    // to the OpaqueConfig defaults otherwise so an
    // `enabled=false` payload still has plausible-shape numeric
    // fields (the SPA logic just short-circuits on the flag).
    let (enabled, ciphersuite_version, ksf_memory_kib, ksf_iterations, ksf_parallelism) =
        match state.opaque_service.as_ref() {
            Some(svc) => (
                true,
                svc.ciphersuite_version(),
                svc.config_ksf_memory_kib(),
                svc.config_ksf_iterations(),
                svc.config_ksf_parallelism(),
            ),
            None => {
                // Substrate off — publish safe defaults matching
                // `OpaqueConfig::default()` so a curious client can
                // still parse the payload cleanly.
                let cfg = crate::common::config::OpaqueConfig::default();
                (
                    false,
                    cfg.ciphersuite_version,
                    cfg.ksf_memory_kib,
                    cfg.ksf_iterations,
                    cfg.ksf_parallelism,
                )
            }
        };

    Json(OpaqueParamsResponse {
        enabled,
        ciphersuite_version,
        ksf: OpaqueKsfParams {
            memory_kib: ksf_memory_kib,
            iterations: ksf_iterations,
            parallelism: ksf_parallelism,
        },
    })
}

// ── Small helpers ────────────────────────────────────────────────────

fn require_opaque_exchange(state: &Arc<AppState>) -> Result<Arc<OpaqueLoginExchange>, AppError> {
    state.opaque_login_exchange.clone().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "OPAQUE login-exchange cache is not wired",
            "OpaqueDisabled",
        )
    })
}

fn require_auth_application_service(
    state: &Arc<AppState>,
) -> Result<
    Arc<crate::application::services::auth_application_service::AuthApplicationService>,
    AppError,
> {
    Ok(state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication service not configured"))?
        .auth_application_service
        .clone())
}

/// Resolve KE1's `userIdentifier` to the OPAQUE server-side user
/// identifier + the stored envelope. Returns `None` (silently)
/// when either the user doesn't exist or has no envelope on file —
/// both branches converge to the anti-enum dummy-KE1 path.
///
/// The tuple's first element (`Vec<u8>`) is what we pass to
/// `ServerLogin::start` as the OPAQUE user identifier. For real users
/// we use `user_id.as_bytes()` (matches the register handler); for
/// the unknown-user branch we hash the CLAIMED identifier so the
/// dummy branch's identifier bytes are still deterministic
/// per-attempt (opaque-ke uses this in the AKE derivation).
async fn resolve_user_and_envelope(
    state: &Arc<AppState>,
    identifier: &str,
) -> Option<(Vec<u8>, Option<ServerRegistration<OxiCloudSuite>>)> {
    let auth = state.auth_service.as_ref()?;
    let repo = state.opaque_repo.as_ref()?;

    let user = auth
        .auth_application_service
        .lookup_user_for_login(identifier)
        .await
        .ok()?;

    let stored = repo.read_registration(user.id()).await.ok().flatten();
    let password_file =
        stored.and_then(|s| ServerRegistration::<OxiCloudSuite>::deserialize(&s.envelope).ok());

    Some((user.id().as_bytes().to_vec(), password_file))
}

/// Recover a UUID from the OPAQUE user identifier bytes IF they're
/// the 16-byte shape written by `resolve_user_and_envelope`. For the
/// dummy branch (identifier = raw caller-supplied string), the bytes
/// are almost never 16 long, so this returns `None` — that's the
/// correct signal to KE1's callers ("don't stamp a user_id in the
/// stash for the dummy branch").
fn user_id_from_bytes(bytes: &[u8]) -> Option<Uuid> {
    let arr: [u8; 16] = bytes.try_into().ok()?;
    Some(Uuid::from_bytes(arr))
}

fn invalid_credentials() -> AppError {
    AppError::new(
        StatusCode::UNAUTHORIZED,
        "Invalid credentials",
        "InvalidCredentials",
    )
}

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
