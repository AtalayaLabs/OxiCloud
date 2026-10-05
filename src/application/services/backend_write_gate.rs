//! Global backend write-lock gate.
//!
//! One primitive, several reasons. When ANY of migration / backup /
//! rotation / an external op holds it, two things change app-wide:
//!
//! 1. User writes are refused by [`PgAclEngine`] (AuthZ reads the
//!    atomic fast-path and returns `AccessDenied` for every mutating
//!    permission check).
//! 2. Background jobs that touch the storage backend
//!    (`backend_reclaim`, `backend_rechunk`, `backend_rotate`,
//!    `thumb_attached_import`, `thumb_derived_import`,
//!    `transcode_import`, `satellites_consistency`) defer their next
//!    tick instead of running. The scheduler prologue reads the gate
//!    on dispatch; a deferred tick records a `job.deferred`
//!    `JobOutcome::Ok` with `extra.reason = "backend_write_locked"`
//!    and advances `next_run_at` by one interval.
//!
//! The design doc behind this lives inline with the field doc on
//! [`BackendWriteLockReason`] — one place per variant, nowhere else.
//!
//! Historical note: a predecessor single-purpose `Arc<AtomicBool>`
//! called `migration_readonly` served the migration case only. This
//! gate subsumes that bool: the inner `fast_path` atomic is still
//! kept as the AuthZ hot-path reader (AuthZ runs on every request,
//! taking a `RwLock` there would be a measurable regression), but
//! the atomic is now driven BY the gate's acquire/release — not
//! poked directly.
//!
//! The DB persistence shape stays backward-compatible: the existing
//! `admin_settings.storage.migration_readonly` text key continues to
//! be read at boot and seeded as `Migration { source: "legacy",
//! target: "legacy" }` when the row is `"true"`. New writes go to a
//! sibling key `storage.backend_write_lock_holder` carrying the full
//! typed reason as JSON, so a restart recovers both "is held" and
//! "why" — no more opaque "server is read-only" banner on a crashed
//! external backup.

use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Why the backend write-lock is held.
///
/// One variant per operation shape. The admin API's `External`
/// variant covers operator-driven quiesce (restic / borg / filesystem
/// snapshot) — anything OxiCloud itself doesn't know how to perform.
///
/// Serialized as JSON into
/// `admin_settings.storage.backend_write_lock_holder` for restart
/// survival. A missing or malformed value is treated as "not held",
/// never as a wedge — a corrupt DB row must never block writes
/// forever.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendWriteLockReason {
    /// `backend_migration` is draining content from one entry to
    /// another. Must hold across pause — the hash-ordered cursor is
    /// only valid while nothing writes. See the memory
    /// `project_migration_readonly_pause_semantics`.
    Migration { source: String, target: String },

    /// `backend_backup` (future job) is copying content out to a
    /// backup target. May hold or release on pause depending on
    /// whether the backup walks a frozen snapshot (release OK) or
    /// the live source (must hold).
    Backup {
        destination: String,
        started_at: DateTime<Utc>,
    },

    /// `backend_rotate` is re-encrypting with a new key. Same
    /// hold/release call as backup.
    Rotation { entry: String },

    /// Operator-driven lock for an external quiesce — `/admin/storage`
    /// UI, admin API. The `admin_id` identifies the user who holds
    /// the lock; `label` is a free-form display string ("nightly
    /// restic to NAS", "manual S3 snapshot before rebalance"). The
    /// `acquired_at` + `expires_at` pair prevents a forgotten lock
    /// from freezing writes forever if the operator's cron script
    /// dies — see [`EXTERNAL_LOCK_MAX_HOLD`].
    External {
        admin_id: Uuid,
        label: String,
        acquired_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    },
}

impl BackendWriteLockReason {
    /// Short stable label for logs, audit lines and the UI banner.
    /// Never reword across releases — log aggregators key off it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Migration { .. } => "migration",
            Self::Backup { .. } => "backup",
            Self::Rotation { .. } => "rotation",
            Self::External { .. } => "external",
        }
    }

    /// Human-readable one-line summary for the server-status
    /// banner. Keep terse — this renders inline next to a lock
    /// icon on every page.
    pub fn display(&self) -> String {
        match self {
            Self::Migration { source, target } => {
                format!("Migrating storage: {source} → {target}")
            }
            Self::Backup { destination, .. } => format!("Backing up to {destination}"),
            Self::Rotation { entry } => format!("Rotating storage key on {entry}"),
            Self::External { label, .. } => {
                if label.is_empty() {
                    "External maintenance in progress".to_string()
                } else {
                    format!("Maintenance: {label}")
                }
            }
        }
    }
}

/// Maximum duration an `External` lock can be held without a renew.
/// Keeps a forgotten admin lock from wedging the server forever if
/// the operator's cron script dies between acquire and release. A
/// release call before expiry is the happy path; expiry is a safety
/// net, not a schedule — hitting it logs a warning. 6h matches the
/// upper bound of a typical overnight backup window plus margin.
pub const EXTERNAL_LOCK_MAX_HOLD: chrono::Duration = chrono::Duration::hours(6);

/// Already-held error returned by [`BackendWriteGate::try_acquire`].
#[derive(Clone, Debug)]
pub struct AlreadyHeldBy(pub BackendWriteLockReason);

impl std::fmt::Display for AlreadyHeldBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "backend write-lock already held by {}", self.0.kind())
    }
}

impl std::error::Error for AlreadyHeldBy {}

/// The gate itself. Thread-safe, cheap to clone (both fields are
/// `Arc`), reads via the atomic fast-path, mutations via the
/// `RwLock`.
pub struct BackendWriteGate {
    /// Reason the gate is held, or `None` if free. Written under
    /// `RwLock` so acquire/release are atomic with respect to the
    /// atomic (see `fast_path`).
    reason: RwLock<Option<BackendWriteLockReason>>,

    /// Fast-path flag mirrored on every acquire/release. AuthZ reads
    /// this on every mutating permission check — making it an atomic
    /// instead of an `RwLock` read matters here. The invariant:
    /// `fast_path.load(Relaxed) == reason.read().is_some()` is
    /// maintained by the acquire/release methods never racing
    /// against themselves (the `RwLock` write side serialises them).
    fast_path: Arc<AtomicBool>,
}

impl BackendWriteGate {
    /// Construct a free gate. Boot code calls
    /// [`Self::new_seeded_from_db`] instead.
    pub fn new() -> Self {
        Self {
            reason: RwLock::new(None),
            fast_path: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Construct a gate from a boot-time reason — the DB
    /// persistence layer passes in whatever it loaded. `None` = not
    /// held, `Some(reason)` = held and `fast_path` set to true. The
    /// caller is responsible for whatever boot-clear rule applies
    /// afterwards (see the migration-readonly clear path in `di.rs`).
    pub fn new_seeded(initial: Option<BackendWriteLockReason>) -> Self {
        let held = initial.is_some();
        Self {
            reason: RwLock::new(initial),
            fast_path: Arc::new(AtomicBool::new(held)),
        }
    }

    /// Shared Arc to the fast-path atomic. Pass this to any reader
    /// that was previously holding an `Arc<AtomicBool>` directly
    /// (AuthZ, the server-status middleware). The gate still owns
    /// the writer side; readers only `.load(Relaxed)`.
    pub fn fast_path_handle(&self) -> Arc<AtomicBool> {
        self.fast_path.clone()
    }

    /// `true` while any reason is held. Equivalent to
    /// `held_by().is_some()` but one atomic load instead of an
    /// `RwLock` read — use this on the hot path.
    pub fn is_held(&self) -> bool {
        self.fast_path.load(Ordering::Relaxed)
    }

    /// The current holder's reason, or `None` if free. Takes a
    /// `RwLock` read; use [`Self::is_held`] on the hot path.
    pub fn held_by(&self) -> Option<BackendWriteLockReason> {
        self.reason
            .read()
            .expect("BackendWriteGate reason poisoned")
            .clone()
    }

    /// Try to acquire the gate. On success, `fast_path` flips to
    /// true and `reason` is stored. On failure (already held), the
    /// current holder is returned so the caller can surface it in
    /// an error message or a 409 response. One holder at a time —
    /// a migration cannot start while an external lock is held, and
    /// vice versa.
    pub fn try_acquire(&self, new_reason: BackendWriteLockReason) -> Result<(), AlreadyHeldBy> {
        let mut guard = self
            .reason
            .write()
            .expect("BackendWriteGate reason poisoned");
        if let Some(current) = guard.as_ref() {
            return Err(AlreadyHeldBy(current.clone()));
        }
        *guard = Some(new_reason);
        self.fast_path.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Release the gate unconditionally. Used by the migration
    /// service's terminal-cancel + success-swap paths, by the admin
    /// API's release endpoint, and by the boot-clear rule.
    ///
    /// Returns the reason that was held, if any — callers that need
    /// to audit "what did we just release?" use it.
    pub fn release(&self) -> Option<BackendWriteLockReason> {
        let mut guard = self
            .reason
            .write()
            .expect("BackendWriteGate reason poisoned");
        let prev = guard.take();
        self.fast_path.store(false, Ordering::Relaxed);
        prev
    }

    /// Replace the current reason with a new one — same holder is
    /// updating details (e.g., migration swapping its source/target
    /// strings mid-run, or an `External` lock renewing its
    /// `expires_at`). Fails if the gate is not held.
    pub fn try_update(&self, new_reason: BackendWriteLockReason) -> Result<(), NotHeld> {
        let mut guard = self
            .reason
            .write()
            .expect("BackendWriteGate reason poisoned");
        if guard.is_none() {
            return Err(NotHeld);
        }
        *guard = Some(new_reason);
        // fast_path stays true; both before and after
        Ok(())
    }
}

impl Default for BackendWriteGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Returned by [`BackendWriteGate::try_update`] when the gate is not
/// held.
#[derive(Clone, Debug)]
pub struct NotHeld;

impl std::fmt::Display for NotHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "backend write-lock is not held")
    }
}

impl std::error::Error for NotHeld {}

#[cfg(test)]
mod tests {
    use super::*;

    fn migration() -> BackendWriteLockReason {
        BackendWriteLockReason::Migration {
            source: "s3-a".into(),
            target: "s3-b".into(),
        }
    }

    #[test]
    fn free_gate_accepts_acquire() {
        let gate = BackendWriteGate::new();
        assert!(!gate.is_held());
        assert!(gate.try_acquire(migration()).is_ok());
        assert!(gate.is_held());
        assert_eq!(gate.held_by().unwrap().kind(), "migration");
    }

    #[test]
    fn second_acquire_returns_current_holder() {
        let gate = BackendWriteGate::new();
        gate.try_acquire(migration()).unwrap();
        let err = gate
            .try_acquire(BackendWriteLockReason::External {
                admin_id: Uuid::nil(),
                label: "nightly backup".into(),
                acquired_at: Utc::now(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
            })
            .unwrap_err();
        // The caller receives the current migration reason, not the
        // one they tried to install — so a 409 can surface "held by
        // whom" to the operator.
        assert_eq!(err.0.kind(), "migration");
    }

    #[test]
    fn release_frees_the_gate() {
        let gate = BackendWriteGate::new();
        gate.try_acquire(migration()).unwrap();
        let prev = gate.release().unwrap();
        assert_eq!(prev.kind(), "migration");
        assert!(!gate.is_held());
        // A fresh acquire now succeeds.
        gate.try_acquire(migration()).unwrap();
    }

    #[test]
    fn fast_path_mirrors_reason() {
        let gate = BackendWriteGate::new();
        let fast = gate.fast_path_handle();
        assert!(!fast.load(Ordering::Relaxed));
        gate.try_acquire(migration()).unwrap();
        assert!(fast.load(Ordering::Relaxed));
        gate.release();
        assert!(!fast.load(Ordering::Relaxed));
    }

    #[test]
    fn seeded_from_held_state_matches_both_fields() {
        let gate = BackendWriteGate::new_seeded(Some(migration()));
        assert!(gate.is_held());
        assert!(gate.fast_path_handle().load(Ordering::Relaxed));
        assert_eq!(gate.held_by().unwrap().kind(), "migration");
    }

    #[test]
    fn try_update_requires_held() {
        let gate = BackendWriteGate::new();
        assert!(gate.try_update(migration()).is_err());
        gate.try_acquire(migration()).unwrap();
        gate.try_update(BackendWriteLockReason::Rotation {
            entry: "s3-a".into(),
        })
        .unwrap();
        assert_eq!(gate.held_by().unwrap().kind(), "rotation");
    }
}
