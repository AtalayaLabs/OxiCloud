//! Purge expired `auth.device_codes` rows on a schedule.
//!
//! **Why a job and not a `tokio::spawn`.** Same argument as
//! `uploads_cleanup_service`: an operator looking at a growing table
//! needs a run history, a count, and an admin trigger. A detached task
//! gives them none of that. A plain [`JobHandler`] gives all three.
//!
//! **Why not recoverable.** `delete_expired` is a single bounded
//! `DELETE … WHERE expires_at < NOW() AND status IN ('pending',
//! 'expired')` — no cursor to persist, a halted pass is retried
//! losslessly on the next tick. A recoverable run would add rows and
//! machinery for information nobody needs.
//!
//! **What goes stale without this.** Every `POST /api/auth/device/
//! authorize` that is never approved AND never polled to `/token`
//! success leaves a `status='pending'` row behind. All read queries
//! already filter by `expires_at > NOW()`, so correctness is not
//! affected — but the table grows unbounded, and `/authorize` is
//! public, so the growth path is attacker-reachable.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::infrastructure::repositories::pg::DeviceCodePgRepository;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs, Mutates};

pub struct DeviceCodesCleanup {
    device_code_storage: Arc<DeviceCodePgRepository>,
}

impl DeviceCodesCleanup {
    pub const JOB_NAME: &'static str = "device_codes_cleanup";

    pub fn new(device_code_storage: Arc<DeviceCodePgRepository>) -> Self {
        Self {
            device_code_storage,
        }
    }

    /// Every 15 minutes. `DEVICE_CODE_LIFETIME_SECS` is 900s, so this
    /// cadence bounds a dead row's residence to roughly one TTL on top
    /// of its own expiry — plenty tight for a DB-side cleanup, and
    /// matches the cadence `/token` polls run at from the client side.
    fn interval() -> Duration {
        Duration::from_secs(900)
    }

    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        registry
            .register(self.clone(), Some(Self::interval()), None)
            .await;
        self
    }
}

#[async_trait]
impl JobHandler for DeviceCodesCleanup {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Removes expired RFC 8628 device-code ceremony rows from \
         `auth.device_codes` — codes whose `expires_at` has passed and \
         whose status is still `pending` or `expired` (i.e. never \
         approved AND never polled to success). All read paths already \
         filter by expiry so correctness is unaffected, but without \
         this the table grows forever since `POST /authorize` is public."
    }

    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        use crate::application::ports::auth_ports::DeviceCodeStoragePort;
        match self.device_code_storage.delete_expired().await {
            Ok(deleted) => JobOutcome::ok_with(
                deleted,
                serde_json::json!({
                    "rows_deleted": deleted,
                }),
            ),
            Err(e) => JobOutcome::err(format!("device_codes cleanup failed: {e}")),
        }
    }
}
