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
//! On the *hot path* (no migration AND no rotation running — the
//! ~100% case in normal operation) this middleware does:
//!   1. one `AtomicBool::load(Relaxed)` — sub-nanosecond;
//!   2. one `RwLock::read` on `rotation_progress` — uncontended;
//!   3. an early return when both are inactive.
//!
//! No allocation, no formatting on the hot path. The rotation-check
//! `RwLock::read` is cheap because writers only fire on batch
//! checkpoints (~every 100 blobs); worst-case contention is
//! sub-microsecond.
//!
//! On the *cold path* (migration OR rotation in progress) the
//! payload builder pulls the progress snapshot(s), formats a small
//! JSON struct (~a few dozen bytes) and inserts the header.
//!
//! Total per-request work on cold path: microseconds.

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
    migration: Option<ProgressHeader>,
    /// K3: independent of `readonly` — rotation does NOT engage the
    /// app-wide read-only flag, so the frontend needs a distinct
    /// signal to know "rotation is running, show the rotation
    /// banner instead of migration banner".
    #[serde(skip_serializing_if = "Option::is_none")]
    rotation: Option<ProgressHeader>,
}

/// Shared progress shape used by both `migration` and `rotation`
/// header fields — same struct name, same JSON field names. Frontend
/// treats them identically at the render layer.
#[derive(serde::Serialize)]
struct ProgressHeader {
    // `target` is owned here — the RwLock guard is released before
    // serialisation, so a borrowed slice wouldn't survive. Names
    // are small (`[a-z0-9_-]{1,32}`) so the copy is trivial.
    target: String,
    migrated: u64,
    total: u64,
    percent: u8,
}

impl ProgressHeader {
    fn from_snapshot(p: &crate::common::migration_progress::MigrationProgress) -> Self {
        Self {
            target: p.target_name.clone(),
            migrated: p.migrated_blobs,
            total: p.total_blobs,
            percent: p.percent,
        }
    }
}

pub async fn server_status_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let readonly = state.migration_readonly.load(Ordering::Relaxed);

    // Rotation snapshot check — cheap uncontended `read`; if `None`
    // and readonly is also false, hot-path returns without a header.
    let rotation_active = state
        .rotation_progress
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some();

    let mut response = next.run(request).await;
    if !readonly && !rotation_active {
        return response;
    }

    // Cold path — build the payload from whichever snapshots are
    // active. `readonly:true` fires the migration banner even if
    // the migration handler hasn't seeded its progress yet
    // (restart-mid-migration scenario). `rotation` is populated
    // independently.
    let payload = {
        let migration = if readonly {
            state
                .migration_progress
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(ProgressHeader::from_snapshot)
        } else {
            None
        };
        let rotation = if rotation_active {
            state
                .rotation_progress
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(ProgressHeader::from_snapshot)
        } else {
            None
        };
        HeaderPayload {
            readonly,
            migration,
            rotation,
        }
    };

    // `serde_json::to_string` on this struct is a few dozen-byte
    // allocation — negligible against the response body. A
    // serialize failure here would be a programming bug, so we
    // degrade to a minimal string rather than skipping the header.
    let value =
        serde_json::to_string(&payload).unwrap_or_else(|_| r#"{"readonly":false}"#.to_string());
    if let Ok(header_value) = HeaderValue::from_str(&value) {
        response
            .headers_mut()
            .insert(SERVER_STATUS_HEADER, header_value);
    }
    response
}
