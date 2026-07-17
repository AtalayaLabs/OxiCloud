//! Shared shape for RFC 6578 incremental sync-collection deltas.
//!
//! WebDAV (folders/files), CalDAV (calendar events), and CardDAV (contacts)
//! each get their own change-log repository trait and table (real FKs per
//! domain — see the `*_sync_changes` migrations and
//! `domain/repositories/folder_sync_change_repository.rs`), but every one
//! of those traits returns `SyncDelta<M>` built from these two types, so
//! the response-building and depth/token-validation logic in the handlers
//! is written once against this shape and reused by all three protocols
//! instead of growing a fourth/fifth bespoke copy.

use uuid::Uuid;

use crate::domain::entities::sync_token::SyncToken;

/// One change-log entry, resolved against current state where the member
/// still exists.
#[derive(Debug, Clone)]
pub enum SyncChange<M> {
    /// Member was created, updated, or restored from trash since the
    /// client's token — carries the current DTO so the handler can render
    /// it exactly like a normal PROPFIND/REPORT entry.
    Upserted(M),
    /// Member was deleted (hard delete, or trashed) since the client's
    /// token. `href_hint` is the last-known leaf name/path segment,
    /// captured at tombstone time, so the handler can render an RFC 6578
    /// §3.7 `<D:status>HTTP/1.1 404 Not Found</D:status>` sub-response
    /// without needing the member row to still exist.
    Deleted { member_id: Uuid, href_hint: String },
}

/// A page of changes for one collection, since one sync-token, paired with
/// the token the client should present on its *next* poll.
#[derive(Debug, Clone)]
pub struct SyncDelta<M> {
    pub changes: Vec<SyncChange<M>>,
    pub new_token: SyncToken,
}
