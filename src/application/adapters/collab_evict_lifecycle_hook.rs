//! Bridges [`FileLifecycleHook`] events into the collab-session
//! eviction path.
//!
//! When a file's blob is replaced by anything OTHER than the collab
//! actor's own flush — a WebDAV PUT overwrite, WOPI PutFile, a REST
//! upload replace, a chunked-upload finalization — every live
//! collaborative session on that file is now editing a Yjs Doc whose
//! state has diverged from the on-disk truth. Continuing to accept
//! updates would produce garbage on the next flush and silently
//! discard the external write.
//!
//! Fix: evict every attached session with `reason: "external_write"`.
//! The WS handler emits `rt.revoked` on the collab topic; the FE's
//! `CollabDoc` transitions to `disconnected`, reconfigures the
//! read-only compartment (see the read-only slice), and awaits the
//! caller's next attach — which will re-seed cleanly from the fresh
//! blob.
//!
//! The [`WriteSource`] discriminator on `on_file_updated` is what
//! makes this safe: the collab flusher itself calls the hook with
//! `WriteSource::CollabFlush`, and this hook short-circuits on that
//! branch. Every external writer (`FileUploadService`'s two
//! replace paths, and by extension WebDAV / WOPI / chunked upload,
//! which all go through it) passes `WriteSource::External`.

use std::sync::Arc;

use tracing::warn;
use uuid::Uuid;

use crate::application::ports::file_lifecycle::{FileLifecycleHook, WriteSource};
use crate::application::services::collab_session_service::CollabSessionService;

/// Hook that fires an eviction on every external content replace.
///
/// Registered in DI only when the collab feature is enabled — with
/// collab off, no live sessions exist and the eviction call is a
/// no-op anyway; skipping the hook keeps the lifecycle fan-out
/// slightly leaner.
pub struct CollabEvictLifecycleHook {
    collab: Arc<CollabSessionService>,
}

impl CollabEvictLifecycleHook {
    pub fn new(collab: Arc<CollabSessionService>) -> Self {
        Self { collab }
    }
}

impl FileLifecycleHook for CollabEvictLifecycleHook {
    fn on_file_created(
        &self,
        _file_id: &str,
        _blob_hash: &str,
        _content_type: &str,
        _is_new_blob: bool,
    ) {
        // A brand-new file cannot have an existing collab session
        // — the actor spawns on first attach, which happens after
        // the row exists.
    }

    fn on_file_copied(
        &self,
        _file_id: &str,
        _blob_hash: &str,
        _content_type: &str,
        _source_file_id: &str,
    ) {
        // A copy creates a new file id — nobody has attached to it
        // yet, so nothing to evict.
    }

    fn on_file_updated(
        &self,
        file_id: &str,
        _blob_hash: &str,
        _content_type: &str,
        source: WriteSource,
    ) {
        // The whole reason `WriteSource` exists: skip evictions the
        // collab flusher itself caused. Every keystroke's debounced
        // flush lands here as `CollabFlush`; evicting on those would
        // kill the very session that just wrote.
        if matches!(source, WriteSource::CollabFlush) {
            return;
        }
        let Ok(uuid) = file_id.parse::<Uuid>() else {
            warn!(
                target: "oxicloud::collab",
                file_id = %file_id,
                "on_file_updated: invalid file_id UUID — skipping eviction",
            );
            return;
        };
        // `on_file_updated` is a sync fire-and-forget contract
        // (see the trait doc: "Background work must be spawned
        // inside the implementor via tokio::spawn; the calling
        // service never awaits hook completion"). `evict_sessions_for_file`
        // is async — spawn onto the runtime.
        let collab = self.collab.clone();
        tokio::spawn(async move {
            collab.evict_sessions_for_file(uuid, "external_write").await;
        });
    }

    fn on_file_deleted(&self, _file_id: &str) {
        // Delete is handled directly by
        // `FileManagementService::delete_and_cleanup_with_perms`
        // (see the file-scoped eviction slice) — it calls
        // `evict_sessions_for_file` with reason "resource_deleted"
        // BEFORE the storage row goes away, which is stronger than
        // reacting to `on_file_deleted` after the fact.
    }
}
