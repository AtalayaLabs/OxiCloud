//! `ops_banner_expiry` scheduled job — periodic sweep of expired banners.
//!
//! Walks the in-memory banner list, drops entries whose `expires_at`
//! has passed, and re-persists the pruned list. Users whose tabs are
//! actively polling pick the change up on their next API call via
//! the X-Server-Status header-diff path (the pruned list hashes to a
//! different `banners_version`, which triggers a `/api/config`
//! refetch client-side).
//!
//! ## Why a periodic job
//!
//! Expired banners are also pruned opportunistically on the next
//! write to the collection (any admin create/update/delete, via
//! [`OpsBannerService::prune_expired`]). A quiet instance with no
//! admin activity would leave an expired banner visible to users
//! until an admin happens to click something — this job catches
//! those cases.
//!
//! ## Non-recoverable on purpose
//!
//! One-shot sweep over a bounded list (≤ [`MAX_BANNERS`] = 10).
//! The whole operation is DB-only, finishes in milliseconds, and
//! produces no findings. No cursor to persist, no retry window to
//! save — a plain `JobHandler` is enough.
//!
//! ## Cadence
//!
//! Every 10 minutes. Finer wouldn't buy anything (banners are
//! scheduled by the hour in practice); coarser leaves a stale
//! banner visible for up to the interval.
//!
//! ## No WS broadcast from here
//!
//! Earlier revisions attempted to push `ServerStatusChanged` on the
//! message bus when the sweep removed anything, so sibling tabs
//! would refetch immediately. That required threading
//! `Arc<AppState>` into the job service — more wiring than the UX
//! wins justify for a sweep that runs every 10 minutes. If a
//! future need calls for sub-second expiry visibility, the hook
//! point is here.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::application::services::ops_banner_service::OpsBannerService;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs, Mutates};

pub struct OpsBannerExpiryService {
    banners: Arc<OpsBannerService>,
}

impl OpsBannerExpiryService {
    pub const JOB_NAME: &'static str = "ops_banner_expiry";

    pub fn new(banners: Arc<OpsBannerService>) -> Self {
        Self { banners }
    }

    /// Cadence — 10 min. See module header for the rationale.
    fn interval() -> Duration {
        Duration::from_secs(10 * 60)
    }

    /// Chainable self-registration — one line in DI.
    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        registry
            .register(self.clone(), Some(Self::interval()), None)
            .await;
        self
    }
}

#[async_trait]
impl JobHandler for OpsBannerExpiryService {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Removes operator-authored banners whose `expires_at` has passed. \
         Fires every 10 minutes so a stale banner on a quiet instance (no \
         admin activity) still disappears without an operator having to \
         click delete. Users pick the change up via the X-Server-Status \
         header-diff path on their next API call."
    }

    fn mutates(&self) -> Mutates {
        // Pruning is a DELETE from the admin_settings JSON blob.
        // Report accurately even when a given run finds nothing to
        // purge — the capability is what the admin UI gates on, not
        // the per-run outcome.
        Mutates::Always
    }

    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        match self.banners.prune_expired().await {
            Ok(0) => JobOutcome::ok_with(0, serde_json::json!({ "pruned": 0 })),
            Ok(n) => {
                tracing::info!(
                    target: "audit",
                    event = "ops_banner.expiry_sweep",
                    pruned = n,
                    "🧹 ops banner expiry sweep removed {n} expired banner(s)"
                );
                JobOutcome::ok_with(n as u64, serde_json::json!({ "pruned": n }))
            }
            Err(e) => JobOutcome::err(format!("ops_banner_expiry: {e}")),
        }
    }
}
