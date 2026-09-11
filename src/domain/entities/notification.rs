//! In-app notification — one durable row per recipient per event.
//!
//! Backs the bell UI. The message bus poke on
//! `user:{user_id}:notifications` is a fast path; the row is truth.
//! See `docs/plan/message-bus.md § Slice E` for the wire contract.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A stable kind slug. The FE routes on this string for icon / label /
/// action-button choice. New kinds are additive; **never repurpose an
/// existing value** — the FE reads it as an enum-like discriminant.
///
/// The initial set matches the plan's Slice-E ingester list. Additional
/// values are legal on the wire (an older FE ignores unknown kinds
/// gracefully by falling back to a generic bell row); we still keep the
/// canonical list here so the ingester callsites reach for symbolic
/// constants instead of literal strings.
///
/// The DB column is plain `TEXT` (see `migrations/20261026000000_notifications.sql`)
/// — no CHECK constraint. Adding a new kind is a code change only, no
/// migration, no downtime.
pub mod kind {
    /// A grant was created for the recipient user (they can now access
    /// a resource). Payload carries the resource id + role + granter.
    pub const SHARE_GRANTED: &str = "share_granted";

    /// A login succeeded from a device / IP fingerprint the user
    /// hasn't seen before. Payload carries the user-agent snippet
    /// and the coarsened location if available.
    pub const NEW_LOGIN_FROM_NEW_DEVICE: &str = "new_login_from_new_device";

    /// A background job triggered by the recipient user finished
    /// (success or failure). Payload carries the job name and
    /// `success: bool`. Clicking navigates to `/admin/jobs/<name>`.
    pub const JOB_COMPLETED_FOR_YOU: &str = "job_completed_for_you";

    /// The recipient's storage quota crossed a warning threshold
    /// (e.g. 80 %, 95 %). Payload carries `used_bytes` / `quota_bytes`
    /// and the crossed percentage.
    pub const STORAGE_QUOTA_THRESHOLD: &str = "storage_quota_threshold";
}

/// One notification row.
///
/// `payload` is a per-kind opaque JSON blob; the DB stays schema-free
/// so a new field never requires a migration. Callers deserialize it
/// against a kind-specific struct on the FE.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: Uuid,
    pub user_id: Uuid,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
    /// `None` = unread; `Some(t)` = when the user explicitly marked it
    /// read via `POST /api/notifications/{id}/read` or
    /// `POST /api/notifications/read-all`.
    pub read_at: Option<DateTime<Utc>>,
}

/// The service-layer input for [`NotificationService::create`]. Split
/// from [`Notification`] because `id` / `created_at` are DB-generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNotification {
    pub user_id: Uuid,
    pub kind: String,
    pub payload: serde_json::Value,
}
