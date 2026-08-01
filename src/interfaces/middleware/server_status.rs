//! Middleware that stamps `X-Server-Status` on every response.
//!
//! Consumed by the frontend `apiFetch` wrapper — every API round-trip
//! carries the current server maintenance state back to the client
//! (no polling, no dedicated endpoint). The banner in the app shell
//! subscribes to a store the wrapper updates and shows/hides itself
//! reactively. See `docs/plan/storage-multi-entry.md` §"Read-only mode"
//! for the broader design.
//!
//! ## Cost model
//!
//! On the *hot path* (no migration running — the ~100% case in normal
//! operation) this middleware does:
//!   1. one `AtomicBool::load(Relaxed)` — sub-nanosecond;
//!   2. an early return when `false`.
//!
//! No allocation, no lock, no formatting. Adds no measurable latency
//! at any user count.
//!
//! On the *cold path* (migration in progress) this middleware does:
//!   1. the atomic load above;
//!   2. one `RwLock::read` (uncontended — writers are the migration
//!      handler, one per batch every ~100 blobs);
//!   3. one small `serde_json::to_string` call on a 4-field struct
//!      (a few dozen bytes);
//!   4. one header insertion.
//!
//! Total per-request work in this branch: microseconds.

use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::common::di::AppState;

/// Name of the response header the frontend reads. Kept short — an
/// admin browser session may keep this header around in every open
/// tab's dev-tools network view during a migration; the value is
/// small JSON but the name should not add bloat.
pub const SERVER_STATUS_HEADER: &str = "x-server-status";

/// Compact JSON shape written into the header. Fields are documented
/// in `common::migration_progress::MigrationProgress`.
///
/// Kept internal so the wire format can evolve. Frontend treats the
/// header as opaque JSON and pattern-matches on the fields it
/// currently understands.
#[derive(serde::Serialize)]
struct HeaderPayload {
    readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    migration: Option<MigrationHeader>,
}

#[derive(serde::Serialize)]
struct MigrationHeader {
    // `target` is owned here — the RwLock guard is released before
    // serialisation, so a borrowed slice wouldn't survive. Names
    // are small (`[a-z0-9_-]{1,32}`) so the copy is trivial.
    target: String,
    migrated: u64,
    total: u64,
    percent: u8,
}

pub async fn server_status_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    // Hot-path fast return. When no migration is running the flag is
    // false and there's nothing to emit — a bare atomic load and out.
    let readonly = state.migration_readonly.load(Ordering::Relaxed);
    let mut response = next.run(request).await;
    if !readonly {
        return response;
    }

    // Cold path — build the payload from the shared progress
    // snapshot. If the snapshot is absent (readonly is true but the
    // handler hasn't seeded progress yet, or a restart-during-
    // migration scenario) we still emit `readonly: true` so the
    // banner shows — the frontend renders a "maintenance in progress"
    // message even when specific numbers aren't available.
    let payload = {
        let guard = state
            .migration_progress
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        HeaderPayload {
            readonly: true,
            migration: guard.as_ref().map(|p| MigrationHeader {
                target: p.target_name.clone(),
                migrated: p.migrated_blobs,
                total: p.total_blobs,
                percent: p.percent,
            }),
        }
    };

    // `serde_json::to_string` on this 4-field struct is a few
    // dozen-byte allocation — negligible against the response body.
    // A serialize failure here would be a programming bug (all
    // fields are trivially serializable), so we degrade to a
    // minimal `readonly: true` string rather than skipping the
    // header entirely.
    let value =
        serde_json::to_string(&payload).unwrap_or_else(|_| r#"{"readonly":true}"#.to_string());
    if let Ok(header_value) = HeaderValue::from_str(&value) {
        response
            .headers_mut()
            .insert(SERVER_STATUS_HEADER, header_value);
    }
    response
}
