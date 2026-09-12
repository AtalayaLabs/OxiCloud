//! Ticket issuance for browser WebSocket authentication.
//!
//! `POST /api/rt/ticket` — issues a one-shot 30 s ticket for the
//! authenticated caller. Runs under the full `/api/*` middleware
//! stack (auth + DPoP), so the caller proves possession of the
//! session AND (when the session is DPoP-bound) the DPoP key on the
//! same request. The ticket then substitutes for that proof on the
//! next WS upgrade.
//!
//! See `src/infrastructure/services/rt_ticket_store.rs` for the
//! store semantics and `docs/plan/message-bus.md § F` for the
//! architectural context.

use std::sync::Arc;

use axum::{Json, extract::State};
use serde::Serialize;

use crate::common::di::AppState;
use crate::infrastructure::services::rt_ticket_store::{SUBPROTOCOL_PREFIX, TICKET_TTL};
use crate::interfaces::middleware::auth::CurrentUserId;

/// Response body for `POST /api/rt/ticket`. Deliberately minimal —
/// callers only need the token string; the TTL is echoed so the FE
/// doesn't hard-code the 30 s constant on its side.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RtTicketResponse {
    /// Opaque single-use token. Present in the WS upgrade as
    /// `Sec-WebSocket-Protocol: oxi.ticket.<ticket>` (the prefix is
    /// baked in by both sides — see [`SUBPROTOCOL_PREFIX`]).
    pub ticket: String,

    /// Seconds until this ticket expires server-side. Consumers should
    /// open the WS immediately; a 30 s bound leaves generous headroom
    /// for the handshake without letting a captured ticket live long.
    pub expires_in_seconds: u64,

    /// Full `Sec-WebSocket-Protocol` value the client MUST pass on the
    /// upgrade. Included pre-assembled so a FE bug can't emit the
    /// wrong prefix and blow the handshake in a way that looks like a
    /// server-side denial.
    pub subprotocol: String,
}

/// Issue a fresh ticket for the authenticated caller. Idempotent from
/// the caller's perspective — each call mints a new token — but
/// each ticket is single-use once redeemed by the WS handler.
///
/// No rate limiting today: even a mildly abusive client would just
/// fill the ticket store with entries that reap in 30 s. If ever
/// necessary, add a per-caller_id token bucket alongside the auth
/// middleware limits.
#[utoipa::path(
    post,
    path = "/api/rt/ticket",
    tag = "message-bus",
    responses(
        (status = 200, description = "Ticket issued", body = RtTicketResponse),
        (status = 401, description = "Unauthenticated"),
    ),
    security(("bearerAuth" = []))
)]
pub async fn issue_rt_ticket(
    CurrentUserId(caller_id): CurrentUserId,
    State(state): State<Arc<AppState>>,
) -> Json<RtTicketResponse> {
    let ticket = state.rt_ticket_store.issue(caller_id);
    let ticket_str = ticket.to_string();
    tracing::debug!(
        target: "oxicloud::message_bus",
        event = "message_bus.ticket_issued",
        caller_id = %caller_id,
        "🎫 rt.ticket issued",
    );
    Json(RtTicketResponse {
        subprotocol: format!("{SUBPROTOCOL_PREFIX}{ticket_str}"),
        ticket: ticket_str,
        expires_in_seconds: TICKET_TTL.as_secs(),
    })
}
