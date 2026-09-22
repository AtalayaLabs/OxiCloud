use std::sync::Arc;

use crate::application::ports::file_lifecycle::{FileLifecycleHook, WriteSource};

/// Composite dispatcher for file lifecycle events.
///
/// Aggregates all [`FileLifecycleHook`] implementations and fans out each
/// event to every registered handler. Services hold a single
/// `Arc<FileLifecycleService>` — new handlers are added once, in DI, without
/// touching the services themselves.
pub struct FileLifecycleService {
    hooks: Vec<Arc<dyn FileLifecycleHook>>,
    /// Late-bound hook slot — populated after the service is
    /// already `Arc`-wrapped and shared. Same rationale as
    /// `FileManagementService::collab_session_service`: the collab
    /// service is built later in the DI graph than this one, and
    /// reshuffling the whole graph for one hookup is more churn
    /// than a `OnceLock` — see
    /// [`Self::set_collab_evict_hook`]. Empty when the collab
    /// feature is off, which is a silent no-op inside the fan-out.
    collab_evict_hook: std::sync::OnceLock<Arc<dyn FileLifecycleHook>>,
}

impl Default for FileLifecycleService {
    fn default() -> Self {
        Self::new()
    }
}

impl FileLifecycleService {
    pub fn new() -> Self {
        Self {
            hooks: Vec::new(),
            collab_evict_hook: std::sync::OnceLock::new(),
        }
    }

    pub fn with_hook(mut self, hook: Arc<dyn FileLifecycleHook>) -> Self {
        self.hooks.push(hook);
        self
    }

    /// Late-bind the collab eviction hook. Called from DI after the
    /// collab session service is constructed. Idempotent — a second
    /// call after the first is silently ignored (matches
    /// `OnceLock::set`'s semantics; boot code never double-registers).
    /// A missing binding is a normal steady state when the collab
    /// feature is off.
    pub fn set_collab_evict_hook(&self, hook: Arc<dyn FileLifecycleHook>) {
        let _ = self.collab_evict_hook.set(hook);
    }
}

impl FileLifecycleHook for FileLifecycleService {
    fn on_file_created(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        is_new_blob: bool,
    ) {
        for hook in &self.hooks {
            hook.on_file_created(file_id, blob_hash, content_type, is_new_blob);
        }
        if let Some(extra) = self.collab_evict_hook.get() {
            extra.on_file_created(file_id, blob_hash, content_type, is_new_blob);
        }
    }

    fn on_file_copied(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        source_file_id: &str,
    ) {
        for hook in &self.hooks {
            hook.on_file_copied(file_id, blob_hash, content_type, source_file_id);
        }
        if let Some(extra) = self.collab_evict_hook.get() {
            extra.on_file_copied(file_id, blob_hash, content_type, source_file_id);
        }
    }

    fn on_file_updated(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        source: WriteSource,
    ) {
        for hook in &self.hooks {
            hook.on_file_updated(file_id, blob_hash, content_type, source);
        }
        if let Some(extra) = self.collab_evict_hook.get() {
            extra.on_file_updated(file_id, blob_hash, content_type, source);
        }
    }

    fn on_file_deleted(&self, file_id: &str) {
        for hook in &self.hooks {
            hook.on_file_deleted(file_id);
        }
        if let Some(extra) = self.collab_evict_hook.get() {
            extra.on_file_deleted(file_id);
        }
    }
}
