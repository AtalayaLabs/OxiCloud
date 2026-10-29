//! `GET /api/status` — authenticated runtime status.
//!
//! Carries the full [`HeaderPayload`] — the exact same shape the
//! `X-Server-Status` response header stamps on every authenticated
//! response. Three uses:
//!
//! 1. **Boot hydration** for authenticated SPA sessions — the FE
//!    reads this once at mount so the live-status store starts with
//!    a non-stale snapshot rather than waiting for the first other
//!    request's header to arrive.
//! 2. **`ServerStatusChanged` refetch target** — when the message
//!    bus pushes a version-diff event, the FE calls this endpoint
//!    to pick up the new state. Pre-consolidation it called
//!    `/api/config`, which also carried the runtime payload; now
//!    `/api/config.server_status` is the public-safe subset so a
//!    FE refetch would silently miss migration / rotation / holder
//!    changes without this endpoint.
//! 3. **Infrastructure-topology privacy boundary.** Fields like
//!    `migration.target` (backend name) and `holder.display`
//!    (operator-authored lock label) stay off `/api/config`
//!    because that endpoint is unauthenticated; a drive-by probe
//!    reading them would learn the deployment's internal naming.
//!    Requiring auth here reserves them for logged-in callers.
//!
//! Auth is enforced by the router — this handler is registered
//! inside `create_api_routes`, which carries the JWT auth
//! middleware. An unauthenticated probe hits 401 before reaching
//! the handler body. See `routes.rs::create_api_routes`.

use std::sync::Arc;

use axum::{Json, extract::State};

use crate::common::di::AppState;
use crate::interfaces::middleware::server_status::{HeaderPayload, build_header_payload};

/// `GET /api/status` — return the full runtime server-status
/// payload. Shape identical to the `X-Server-Status` response
/// header the middleware stamps on every authenticated response.
#[utoipa::path(
    get,
    path = "/api/status",
    tag = "status",
    responses(
        (status = 200, description = "Server runtime status", body = HeaderPayload),
        (status = 401, description = "Authentication required"),
    ),
    security(("bearerAuth" = [])),
)]
pub async fn get_server_status(State(state): State<Arc<AppState>>) -> Json<HeaderPayload> {
    Json(build_header_payload(&state))
}
