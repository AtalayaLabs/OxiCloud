//! Common types shared across the scheduler module.
//!
//! Nothing here talks to the DB or the async runtime — pure data
//! definitions so downstream modules (handler, registry, engine) can
//! import without dragging in transitive dependencies. See
//! `docs/plan/job-registry.md` Part 1 for the design rationale.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Per-dispatch parameters passed from the caller (scheduler tick or
/// admin trigger) into [`JobHandler::run`](super::handler::JobHandler::run).
///
/// Deliberately a struct — not a bare `bool` — so we don't churn every
/// handler signature the next time a job needs another knob. Grows by
/// addition; renaming a field is a breaking change to admin scripts
/// that pass query params, so treat like SQL columns.
///
/// **Handlers that don't understand a given arg silently ignore it.**
/// No error path just because a caller set an unused flag — that would
/// leak per-job semantics into callers who don't need to know.
///
/// Semantics of `force`, per job:
/// - `dedup_gc` — skip the orphan grace window (grace = 0).
/// - `grant_cleanup` — grace = 0.
/// - Others (trash_cleanup, usage_reconcile, …) — ignored.
///
/// Semantics of `deep`, per job:
/// - `consistency_batch` — propagate to sub-jobs; only `storage_consistency`
///   currently respects it. Wraps the "run all consistency checks
///   including the slow ones" case behind the same job_name lock as
///   the normal batch (Ed's Option B, 2026-07-29).
/// - `storage_consistency` (future) — enables per-blob re-BLAKE3 (bitrot
///   detection) + mime sniff alongside the fast orphan check.
/// - Others — ignored.
///
/// Semantics of `storage`, per job (added for the multi-entry storage
/// design — see `docs/plan/storage-multi-entry.md`):
/// - `backend_migration` — the NAME of the target storage entry to
///   copy blobs INTO. Required on a Fresh run (handler refuses
///   without it); ignored on a Resumed run (target read from the
///   persisted `params.target_name`).
/// - `blobs_consistency` / `backend_consistency` (slice 7) — the NAME
///   of the entry to probe instead of the currently-active backend.
///   `None` falls through to the live backend (today's behaviour).
/// - Others — ignored.
///
/// Semantics of `repair` (added 2026-10-17 for the refcount fix):
/// - `blobs_consistency` / `manifests_consistency` — when `true`,
///   after each `refcount_mismatch` / `manifest_refcount_mismatch`
///   finding is recorded, apply the corrective UPDATE that sets the
///   stored counter to the auditor's computed `actual_ref_count`.
///   Content-safe: the row itself is fine, only the counter is
///   wrong. Race-safe: each UPDATE recomputes the auditor formula
///   in the same statement, so a concurrent write can't leave a
///   stale value. Default `false` preserves discovery-only
///   behaviour. Also propagates through `consistency_batch` to
///   both tenants — one `?repair=true` call fixes both counters.
/// - Others — ignored.
#[derive(Debug, Clone, Default)]
pub struct JobRunArgs {
    pub force: bool,
    pub deep: bool,
    pub storage: Option<String>,
    pub repair: bool,
}

/// Uniform outcome the supervisor logs and stores for every job dispatch.
///
/// Two variants, deliberately. Distinguishing *why* a job failed
/// (handler returned Err, `tokio::time::timeout` tripped,
/// `catch_unwind` caught a panic) is a **diagnostic** concern — it
/// belongs in a `cause` tracing field the supervisor sets, not in a
/// control-flow branch every consumer of `match outcome` has to
/// think about. See `docs/plan/job-registry.md` Part 1 §JobOutcome.
///
/// `Ok::count` is the row/record count the job reports as its primary
/// scalar (rows scanned, blobs migrated, thumbnails checked). `extra`
/// is a free-form JSON blob for job-specific fields the caller wants
/// surfaced to `oxicloud::scheduler` log lines.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum JobOutcome {
    Ok {
        count: u64,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        extra: serde_json::Value,
    },
    /// `Err` is a struct variant (not tuple-newtype) so it composes
    /// with `#[serde(tag = "outcome")]`. Serde's internal tagging
    /// refuses to serialise a tuple variant wrapping a bare String
    /// — the tag has nowhere to live. The struct form `{ message }`
    /// lets serde emit `{"outcome":"err","message":"..."}` cleanly.
    Err { message: String },
}

impl JobOutcome {
    /// Ok with no extras — the common case for jobs that only report a count.
    pub fn ok(count: u64) -> Self {
        JobOutcome::Ok {
            count,
            extra: serde_json::Value::Null,
        }
    }

    /// Ok with a JSON `extra` payload. Use `serde_json::json!({...})`
    /// at call sites for readability.
    pub fn ok_with(count: u64, extra: serde_json::Value) -> Self {
        JobOutcome::Ok { count, extra }
    }

    /// Convenience constructor for `Err` — call-site ergonomics
    /// match the retired tuple form.
    pub fn err(message: impl Into<String>) -> Self {
        JobOutcome::Err {
            message: message.into(),
        }
    }

    /// Terse discriminant for logs / metrics: `"ok"` | `"err"`.
    pub fn kind(&self) -> &'static str {
        match self {
            JobOutcome::Ok { .. } => "ok",
            JobOutcome::Err { .. } => "err",
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, JobOutcome::Ok { .. })
    }
}

/// Diagnostic reason the supervisor attaches to the `cause` tracing
/// field when a job's outcome is [`JobOutcome::Err`]. Never persisted
/// as a first-class column — it's a log field only.
///
/// Handlers never construct this; the supervisor derives it from
/// which failure path fired:
/// - [`ErrCause::Handler`] — the handler returned `Err(_)` itself.
/// - [`ErrCause::Timeout`] — `tokio::time::timeout` tripped on the
///   registered `ScheduledJob.timeout` wall-clock cap.
/// - [`ErrCause::Panicked`] — `JoinHandle` returned a panic error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrCause {
    Handler,
    Timeout,
    Panicked,
}

impl ErrCause {
    /// Stable label for the `cause` tracing field. Log aggregators key
    /// on these — renaming here IS a breaking change to any dashboard
    /// filtering on `cause = "handler"`.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrCause::Handler => "handler",
            ErrCause::Timeout => "timeout",
            ErrCause::Panicked => "panicked",
        }
    }
}

impl fmt::Display for ErrCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joboutcome_kind_label() {
        assert_eq!(JobOutcome::ok(0).kind(), "ok");
        assert_eq!(JobOutcome::err("boom").kind(), "err");
    }

    #[test]
    fn errcause_labels_stable() {
        assert_eq!(ErrCause::Handler.as_str(), "handler");
        assert_eq!(ErrCause::Timeout.as_str(), "timeout");
        assert_eq!(ErrCause::Panicked.as_str(), "panicked");
    }
}
