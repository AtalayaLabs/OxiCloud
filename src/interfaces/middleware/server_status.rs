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
/// Public because `GET /api/config` returns the same shape as the
/// initial hydration snapshot for FE stores — the endpoint mirrors
/// whatever the header carries so the client has a single wire
/// vocabulary to render. Frontend treats the value as opaque JSON
/// and pattern-matches on the fields it currently understands;
/// adding a field is additive.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct HeaderPayload {
    pub readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration: Option<ProgressHeader>,
    /// K3: independent of `readonly` — rotation does NOT engage the
    /// app-wide read-only flag, so the frontend needs a distinct
    /// signal to know "rotation is running, show the rotation
    /// banner instead of migration banner".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation: Option<ProgressHeader>,
    /// Public-safe projection of the backend write-lock holder. One
    /// of the four `BackendWriteLockReason` variants (migration /
    /// backup / rotation / external) is engaged when `readonly =
    /// true`; the FE banner picks its caption from `holder.display`
    /// instead of guessing from the (optional) `migration` /
    /// `rotation` progress sub-objects.
    ///
    /// Only `kind` + `display` are exposed here; `admin_id`,
    /// `expires_at`, `source`/`target` details stay admin-only on
    /// `/api/admin/storage/write-lock`. `display` is server-formatted
    /// (`BackendWriteLockReason::display()`), so the operator's
    /// free-form `label` on an External hold shows through unredacted —
    /// same disclosure boundary the migration banner has today for
    /// source/target entry names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<HolderHeader>,

    /// Short hash of the current `ops_banner_service` list. Carried
    /// on the X-Server-Status header on every API response — the
    /// FE compares against its cached version and, on diff, refetches
    /// `/api/config` to pick up `banners`. Kept to 16 hex chars so
    /// the header stays small on every request.
    ///
    /// Only serialized when at least one banner is live (either
    /// scheduled or visible). An empty list means "no banners
    /// anywhere" and the field is omitted — one less thing on the
    /// wire for the common case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banners_version: Option<String>,

    /// Public-filtered list of operator-authored banners (entries
    /// whose `starts_at` is in the past or absent). Full objects —
    /// severity + body-per-locale + optional starts_at.
    ///
    /// ONLY populated in the `/api/config` body (via
    /// [`build_header_payload`]) — the X-Server-Status response
    /// header carries just `banners_version` because an admin
    /// could post a banner with 10 locale translations and we
    /// should not re-ship that on every API response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banners: Option<Vec<crate::application::services::ops_banner_service::OpsBanner>>,
}

/// Public-safe lock-holder projection carried in the X-Server-Status
/// header + `/api/config` body. See [`HeaderPayload::holder`].
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct HolderHeader {
    /// Stable machine key — `"migration" | "backup" | "rotation" | "external"`.
    /// Clients switch on this to pick an icon/colour per variant.
    pub kind: String,
    /// Human-readable one-liner, formatted server-side by
    /// `BackendWriteLockReason::display()`. Safe to render raw.
    pub display: String,
}

/// Shared progress shape used by both `migration` and `rotation`
/// header fields — same struct name, same JSON field names. Frontend
/// treats them identically at the render layer.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct ProgressHeader {
    // `target` is owned here — the RwLock guard is released before
    // serialisation, so a borrowed slice wouldn't survive. Names
    // are small (`[a-z0-9_-]{1,32}`) so the copy is trivial.
    pub target: String,
    pub migrated: u64,
    pub total: u64,
    pub percent: u8,
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

/// Build the same [`HeaderPayload`] the middleware stamps into the
/// `X-Server-Status` header, without touching a response. Used by
/// `GET /api/config` so the client sees the exact shape the header
/// would carry at that moment — no drift, no dual serialisers.
///
/// Cost model matches the middleware:
/// - Hot path (nothing active) returns `readonly: false` with no
///   allocations for the progress sub-objects.
/// - Cold path allocates the progress rows exactly once each.
pub fn build_header_payload(state: &AppState) -> HeaderPayload {
    let readonly = state.migration_readonly.load(Ordering::Relaxed);

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
    let rotation = state
        .rotation_progress
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(ProgressHeader::from_snapshot);
    // Project the typed holder down to its public-safe pair. Only
    // populated when the gate is actually held — a free gate under
    // a stale legacy bool would still send `readonly: true` with no
    // holder, which is strictly more honest than fabricating one.
    // Project to the PUBLIC display — strips the operator-authored
    // label from an External hold so the generic payload every
    // authenticated user sees via /api/config doesn't leak internal
    // ops notes. The full typed holder (with raw label) stays on the
    // admin-only `/api/admin/storage/write-lock` surface.
    let holder = state.backend_write_gate.held_by().map(|r| HolderHeader {
        kind: r.kind().to_string(),
        display: r.public_display(),
    });
    // Banner state. Admin API mutations bump the version via the
    // service's write-side commit; the FE compares versions and
    // refetches `/api/config` on diff. `banners` here is the
    // public-filtered list (ops authored, `starts_at <= now`).
    let banners_list = state.ops_banner_service.list_public();
    let (banners_version, banners) = if banners_list.is_empty() {
        (None, None)
    } else {
        (Some(state.ops_banner_service.version()), Some(banners_list))
    };

    HeaderPayload {
        readonly,
        migration,
        rotation,
        holder,
        banners_version,
        banners,
    }
}

/// Short hash of the current server-status payload. Carried on the
/// `MessageBusEvent::ServerStatusChanged { version }` push so FE
/// clients can compare against their last-seen value and refetch
/// `/api/config` only on a real diff.
///
/// Hashes the entire [`HeaderPayload`] by serializing it to JSON and
/// taking the first 16 hex chars of its BLAKE3. The payload shape is
/// stable JSON (BTreeMap-serialized banners, deterministic field
/// order), so the same observable state always hashes the same.
/// Collision risk at 2^-64 — comfortably below "false negative on a
/// genuine change" being noticeable.
pub fn compute_server_status_version(state: &AppState) -> String {
    let payload = build_header_payload(state);
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let full = blake3::hash(&bytes).to_hex().to_string();
    full[..16].to_string()
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

    // Banner presence check. Any live banner (including scheduled)
    // means we have a version to stamp.
    let any_banner = !state.ops_banner_service.list_all().is_empty();

    let mut response = next.run(request).await;

    // Stamp the header on EVERY response — even when nothing is
    // going on server-side. The payload is ~20 bytes
    // (`{"readonly":false}`) and the ~1µs cost is negligible against
    // the surrounding request work.
    //
    // Why we don't early-return any more: the FE relies on header
    // ABSENCE to mean "response path outside the middleware stack"
    // (/api/auth/*, /api/wopi/*, etc., all nested outside the layer
    // per `create_api_routes`). The previous "early-return on empty
    // state" rule collided with that signal: a middleware response
    // with nothing-happening looked identical to an auth response
    // that never saw the middleware, and the FE wiped its banner
    // store on an unrelated `PATCH /api/auth/me/profile`. Stamping
    // the header unconditionally restores the invariant "header
    // present ⇔ this response went through the status middleware".

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
        // Only pull the typed holder when `readonly` fired —
        // rotation-only state (no readonly) still means writes are
        // allowed, so there is no gate holder to project.
        let holder = if readonly {
            state.backend_write_gate.held_by().map(|r| HolderHeader {
                kind: r.kind().to_string(),
                display: r.display(),
            })
        } else {
            None
        };
        // Header-only variant — carries the version hash so the FE
        // knows when to refetch, but never the full banner list.
        // The list lives on `/api/config` body only.
        let banners_version = if any_banner {
            Some(state.ops_banner_service.version())
        } else {
            None
        };
        HeaderPayload {
            readonly,
            migration,
            rotation,
            holder,
            banners_version,
            banners: None,
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
