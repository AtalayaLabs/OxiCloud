//! Shared shape for RFC 6578 incremental sync-collection deltas.
//!
//! WebDAV (folders/files), CalDAV (calendar events), and CardDAV (contacts)
//! each get their own change-log repository trait and table — deliberately
//! WITHOUT a FK from the change-log row to its parent collection (see the
//! `*_sync_changes` migrations and
//! `domain/repositories/folder_sync_change_repository.rs` for why: a bulk
//! delete of a collection can outrun the tombstone insert for its own
//! members) — but every one of those traits returns `SyncDelta<M>` built
//! from these two types, so the response-building and depth/token-validation
//! logic in the handlers is written once against this shape and reused by
//! all three protocols instead of growing a fourth/fifth bespoke copy.
//!
//! **Assumption baked into this design: the `AuthorizationEngine` is
//! grant-only (additive).** A caller's set of visible collections can only
//! grow (explicit grant) or shrink back to nothing (grant removed/expired)
//! — there is no "deny" rule that can carve out a narrower view while a
//! broader grant still exists. `AuthorizationEngine::require` is re-checked
//! on every `list_changes_with_perms` call, so a caller who loses their
//! grant simply gets a 404 (anti-enumeration) on their next REPORT — no
//! proactive "you lost access, tear down your local copy" delta is ever
//! emitted to a client still holding an older token; they just stop being
//! able to sync. If deny rules are ever introduced, this needs revisiting:
//! today's model has no way to express "still has SOME access, but this
//! specific item became invisible," which a deny rule would require.

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
    /// token. `href_hint` is the last-known leaf name/path segment
    /// (unencoded, no trailing slash), captured at tombstone time, so the
    /// handler can render an RFC 6578 §3.7 `<D:status>HTTP/1.1 404 Not
    /// Found</D:status>` sub-response without needing the member row to
    /// still exist. `is_collection` tells the handler whether to append
    /// the trailing-slash collection-href convention (always `false` for
    /// CalDAV/CardDAV, whose members are never containers).
    Deleted {
        member_id: Uuid,
        href_hint: String,
        is_collection: bool,
    },
}

/// A page of changes for one collection, since one sync-token, paired with
/// the token the client should present on its *next* poll.
#[derive(Debug, Clone)]
pub struct SyncDelta<M> {
    pub changes: Vec<SyncChange<M>>,
    pub new_token: SyncToken,
}

impl<M> SyncDelta<M> {
    /// Splits `changes` into upserted DTOs and rendered-deleted hrefs
    /// (`base_href` + `href_hint`), for a collection whose members are
    /// never containers themselves (CalDAV events, CardDAV contacts —
    /// `is_collection` is always `false` for both). WebDAV's mixed
    /// folder/file collection needs a three-way split (subfolders/
    /// files/deleted) instead and keeps its own inline match.
    pub fn split_homogeneous(self, base_href: &str) -> (Vec<M>, Vec<String>) {
        let mut upserted = Vec::with_capacity(self.changes.len());
        let mut deleted = Vec::new();
        for change in self.changes {
            match change {
                SyncChange::Upserted(m) => upserted.push(m),
                SyncChange::Deleted { href_hint, .. } => {
                    deleted.push(format!("{base_href}{href_hint}"));
                }
            }
        }
        (upserted, deleted)
    }
}
