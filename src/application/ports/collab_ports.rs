//! Outbound ports for the collaborative-doc-sessions substrate (Phase A C1).
//!
//! One row per file that has ever hosted a collab session lives in
//! `collab.doc_sessions` — see the migration `20261027000000_collab_doc_sessions.sql`
//! and the plan doc [`docs/plan/markdown-collab.md`] § Data model.
//!
//! This module declares three narrow ports so the
//! [`crate::application::services::collab_session_service::CollabSessionService`]
//! can be exercised without a live database or a live
//! `FileManagementService`:
//!
//! - [`DocSessionRepository`] — CRDT-state persistence. Load / save /
//!   delete a `collab.doc_sessions` row keyed on `file_id`.
//! - [`DocContentReader`] — reads the current file blob text on FIRST
//!   attach, seeding a fresh CRDT doc when no session row exists yet.
//! - [`DocContentWriter`] — writes the current CRDT text back to the
//!   file blob on debounced flush.
//!
//! The reader / writer traits deliberately DON'T pull `FileManagementService`
//! into the port layer; the concrete impl bridges to it in the
//! infrastructure crate. This keeps unit tests over the CRDT lifecycle
//! (apply → compact → flush) trivially mockable.
//!
//! # Phase A scope
//!
//! C1 (this file's consumer, `CollabSessionService`) only exercises the
//! actor + persistence. Bus fan-out and WS binary-frame routing land in
//! C2 — those live in `interfaces/api/handlers/rt_ws.rs` and don't
//! touch these ports.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::common::errors::DomainError;

/// One `collab.doc_sessions` row. Reconstituted by the service on
/// attach to seed the in-memory `yrs::Doc` actor.
#[derive(Debug, Clone)]
pub struct StoredDocSession {
    pub file_id: Uuid,
    /// Serialised `yrs::Doc` snapshot — the persistent CRDT state
    /// baseline. Non-compacted updates ride on top and are folded in
    /// at compaction time (see `updates_since_snapshot`).
    pub state: Vec<u8>,
    /// Yjs state vector snapshot, cached alongside `state` so
    /// reconnect probes get a sync-step-1 reply without re-parsing the
    /// doc every time.
    pub state_vector: Vec<u8>,
    /// Count of `apply_update` calls layered on top of `state` since
    /// the last compaction. The service compacts (re-serialises `state`
    /// as a single snapshot) when this passes the configured threshold.
    pub updates_since_snapshot: i32,
    /// Content hash of the last successful flush-to-blob write. `None`
    /// on rows that have never been flushed (freshly-seeded from a blob
    /// whose text has not yet been rewritten). Compared against the
    /// current CRDT text hash to short-circuit no-op flushes.
    pub last_flushed_content_hash: Option<String>,
    pub last_flushed_at: Option<DateTime<Utc>>,
    pub last_activity_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Persistence port for `collab.doc_sessions`.
///
/// Reads are used on session attach (load-or-seed); writes are the
/// service's snapshot-compaction and flush-metadata updates.
#[cfg_attr(feature = "test_utils", mockall::automock)]
#[async_trait]
pub trait DocSessionRepository: Send + Sync + 'static {
    /// Load the persisted CRDT state for `file_id`. Returns `None`
    /// when the row is absent (first-ever attach — caller seeds a
    /// fresh doc from the blob text).
    async fn load(&self, file_id: Uuid) -> Result<Option<StoredDocSession>, DomainError>;

    /// Upsert the CRDT snapshot (state + state_vector) for `file_id`
    /// and reset `updates_since_snapshot` to 0. Called after
    /// compaction — the whole snapshot rotates atomically. Bumps
    /// `last_activity_at` to `NOW()`.
    async fn save_snapshot(
        &self,
        file_id: Uuid,
        state: &[u8],
        state_vector: &[u8],
    ) -> Result<(), DomainError>;

    /// Bump `updates_since_snapshot` and `last_activity_at`. Called
    /// after every applied update to keep the "when was this doc last
    /// touched" clock honest — the idle-GC scan uses `last_activity_at`.
    async fn touch_after_update(&self, file_id: Uuid) -> Result<(), DomainError>;

    /// Stamp `last_flushed_content_hash` + `last_flushed_at` after a
    /// successful blob write. Elides on the flush path when the CRDT
    /// text hash matches the stamped value (no-op flush).
    async fn record_flush(&self, file_id: Uuid, content_hash: &str) -> Result<(), DomainError>;

    /// Remove the row for `file_id`. Called by idle-GC after a final
    /// flush, and by the file-delete cascade path (though the FK's
    /// `ON DELETE CASCADE` already handles the latter — this method
    /// exists for the GC and for tests).
    async fn delete(&self, file_id: Uuid) -> Result<(), DomainError>;
}

/// Reads the current file blob text. Used ONLY on the first-ever
/// attach when no `collab.doc_sessions` row exists yet — the service
/// seeds a fresh CRDT doc with the current file content.
///
/// Subsequent reads never hit this port; the CRDT actor is authoritative
/// once seeded.
///
/// The `caller_id` parameter is required by `FileManagementService`'s
/// AuthZ layer — the concrete impl passes it through so the read is
/// audited as "collab-service on behalf of user X". C1's unit tests
/// use a stub that ignores it.
#[cfg_attr(feature = "test_utils", mockall::automock)]
#[async_trait]
pub trait DocContentReader: Send + Sync + 'static {
    async fn read_content(&self, caller_id: Uuid, file_id: Uuid) -> Result<Vec<u8>, DomainError>;
}

/// Writes the current CRDT text back to the file blob on debounced
/// flush. The concrete impl bridges to `FileManagementService::write_content`
/// (or its equivalent) so dedup, versioning, quota, and audit all
/// stay in the normal file-write pipeline.
///
/// Returns the content hash of the written blob so the caller can
/// stamp `record_flush(hash)` and short-circuit future no-op flushes.
#[cfg_attr(feature = "test_utils", mockall::automock)]
#[async_trait]
pub trait DocContentWriter: Send + Sync + 'static {
    async fn write_content(
        &self,
        caller_id: Uuid,
        file_id: Uuid,
        content: Vec<u8>,
    ) -> Result<String, DomainError>;
}
