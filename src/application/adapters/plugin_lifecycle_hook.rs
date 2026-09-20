//! Bridges the existing [`FileLifecycleHook`] fan-out to the plugin runtime.
//!
//! `FileLifecycleService` already notifies hooks on every file create/update.
//! This adapter turns those notifications into `file.uploaded` plugin events.
//! Because the hook signature carries only `file_id` (not path/size), it looks
//! the metadata up via [`FileRetrievalUseCase::get_file`] — off the request
//! path, and skipped entirely when no plugin subscribes.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::ports::file_lifecycle::FileLifecycleHook;
use crate::application::ports::file_ports::FileRetrievalUseCase;
use crate::application::ports::plugin_ports::{
    EVENT_FILE_UPLOADED, PluginDispatchPort, PluginEvent,
};
use crate::application::services::FileRetrievalService;

/// Lifecycle hook that forwards file create/update events to subscribed plugins.
pub struct PluginLifecycleHook {
    dispatch: Arc<dyn PluginDispatchPort>,
    retrieval: Arc<FileRetrievalService>,
}

impl PluginLifecycleHook {
    pub fn new(
        dispatch: Arc<dyn PluginDispatchPort>,
        retrieval: Arc<FileRetrievalService>,
    ) -> Self {
        Self {
            dispatch,
            retrieval,
        }
    }

    /// Look up the file's metadata and dispatch a `file.uploaded` event. Cheap
    /// early-out when nothing subscribes; otherwise the DB read and the plugin
    /// run happen on a background task, never blocking the caller.
    fn dispatch_upload(&self, file_id: &str) {
        if !self.dispatch.has_subscribers(EVENT_FILE_UPLOADED) {
            return;
        }
        let dispatch = self.dispatch.clone();
        let retrieval = self.retrieval.clone();
        let file_id = file_id.to_string();

        tokio::spawn(async move {
            let dto = match retrieval.get_file(&file_id).await {
                Ok(dto) => dto,
                Err(e) => {
                    tracing::warn!(
                        target: "oxicloud::plugins",
                        file_id = %file_id,
                        error = %e,
                        "plugin bridge: file metadata lookup failed; skipping dispatch"
                    );
                    return;
                }
            };

            dispatch.dispatch(PluginEvent {
                name: EVENT_FILE_UPLOADED,
                // Post-D7 the wire DTO no longer carries `owner_id`;
                // §14 `created_by` provenance is the equivalent signal
                // (who put the file in the system).
                user_id: dto.created_by.map(|u| u.to_string()),
                invocation_id: Uuid::new_v4().to_string(),
                payload: serde_json::json!({
                    "path": dto.path,
                    "size": dto.size,
                    "mime": dto.mime_type.to_string(),
                }),
            });
        });
    }
}

impl FileLifecycleHook for PluginLifecycleHook {
    fn on_file_created(
        &self,
        file_id: &str,
        _blob_hash: &str,
        _content_type: &str,
        _is_new_blob: bool,
    ) {
        self.dispatch_upload(file_id);
    }

    fn on_file_updated(
        &self,
        file_id: &str,
        _blob_hash: &str,
        _content_type: &str,
        _source: crate::application::ports::file_lifecycle::WriteSource,
    ) {
        self.dispatch_upload(file_id);
    }

    // A copy creates a new file record, but its content already existed and was
    // already observed on its original upload; M0 does not re-dispatch for it.
    fn on_file_copied(
        &self,
        _file_id: &str,
        _blob_hash: &str,
        _content_type: &str,
        _source_file_id: &str,
    ) {
    }

    fn on_file_deleted(&self, _file_id: &str) {}
}
