//! Shared in-memory snapshot of the running storage-migration.
//!
//! Consumed by the server-status middleware to build the
//! `X-Server-Status` response header on every authenticated
//! request. That header lets every logged-in user's session banner
//! show current maintenance progress without any polling —
//! the state travels back on the piggyback of whatever API call
//! the user was going to make anyway.
//!
//! Written by the migration handler on each batch checkpoint (a
//! cheap `RwLock::write` + a small struct copy — no DB access on
//! the request path). Cleared on `RunOutcome::Completed` /
//! `Paused` / `Failed`. `None` means "no migration is running";
//! middleware omits the header entirely in that case.

use serde::Serialize;

/// One snapshot of a running migration. Every field is a scalar so
/// the whole struct copies cheaply under the `RwLock::write` guard.
#[derive(Debug, Clone, Serialize)]
pub struct MigrationProgress {
    /// The entry name blobs are being copied INTO. Used by the
    /// user-facing banner text so admins/users know what the
    /// server is switching to.
    pub target_name: String,
    /// Blobs migrated so far this run. Starts at 0 on a Fresh
    /// open; on Resume the checkpointed value is loaded from the
    /// run row's `stats.scanned_count`.
    pub migrated_blobs: u64,
    /// Total blobs in the current DB snapshot. Captured once at
    /// run start via `SELECT COUNT(*) FROM storage.blobs`. Doesn't
    /// change during the run — new uploads are refused while
    /// read-only is engaged, so the denominator stays honest.
    pub total_blobs: u64,
    /// Convenience: `migrated_blobs * 100 / total_blobs`, clamped
    /// to 0..=100. Middleware could compute it but it's tiny and
    /// makes the JSON payload obvious.
    pub percent: u8,
}

impl MigrationProgress {
    pub fn new(target_name: String, total_blobs: u64) -> Self {
        Self {
            target_name,
            migrated_blobs: 0,
            total_blobs,
            percent: 0,
        }
    }

    /// Update the counter + recompute `percent`. Called by the
    /// migration handler after each batch checkpoint.
    pub fn bump(&mut self, migrated_delta: u64) {
        self.migrated_blobs = self.migrated_blobs.saturating_add(migrated_delta);
        self.recompute_percent();
    }

    fn recompute_percent(&mut self) {
        self.percent = if self.total_blobs == 0 {
            0
        } else {
            ((self.migrated_blobs.min(self.total_blobs) as u128 * 100) / self.total_blobs as u128)
                as u8
        };
    }
}
