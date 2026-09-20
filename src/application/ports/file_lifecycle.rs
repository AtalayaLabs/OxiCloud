/// Origin of an `on_file_updated` event.
///
/// External writers (REST upload replace, WebDAV PUT, WOPI PutFile,
/// Nextcloud chunked upload finalization) supply
/// [`WriteSource::External`]. The collab actor's flush-to-blob writer
/// supplies [`WriteSource::CollabFlush`] — that's the ONE write path
/// that's already going THROUGH the CRDT session, so hooks that fire
/// eviction on external writes must ignore this variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteSource {
    /// A non-collab actor replaced the file's blob. The in-memory
    /// CRDT (if any) is now stale relative to the on-disk truth —
    /// the collab-eviction hook uses this to invalidate live
    /// sessions with `reason: "external_write"`.
    External,
    /// The collab session's own flush wrote its accumulated Y.Doc
    /// state to the blob. Not stale by definition; the eviction
    /// hook MUST NOT fire.
    CollabFlush,
}

/// Observer notified by file services when a file record is created, copied,
/// updated, or permanently deleted.
///
/// Register with [`FileLifecycleService`] during DI wiring; it fans out to all
/// registered hooks. Every implementor **must** provide all four methods —
/// use an explicit one-liner noop for events the implementor does not care about.
/// This forces conscious acknowledgement of every lifecycle event rather than
/// silent omission.
///
/// All methods are synchronous. Background work must be spawned inside the
/// implementor via `tokio::spawn`; the calling service never awaits hook
/// completion.
pub trait FileLifecycleHook: Send + Sync {
    /// Called after a new file record has been persisted.
    ///
    /// `file_id` — opaque file UUID string.
    /// `blob_hash` — BLAKE3 hex of the content blob.
    /// `content_type` — MIME type.
    /// `is_new_blob` — `true` if the blob was stored for the first time (no
    /// dedup hit); `false` if the blob already existed (re-upload of identical
    /// content). Implementors can use this to skip re-generating artefacts that
    /// are keyed by `blob_hash` and already exist on disk.
    ///
    /// For explicit file copies use [`on_file_copied`] instead — it supplies
    /// the source file id so per-file metadata can be cloned directly.
    fn on_file_created(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        is_new_blob: bool,
    );

    /// Called after a file has been created as an explicit copy of an existing file.
    ///
    /// `file_id` — opaque file UUID string of the **new** copy.
    /// `blob_hash` — BLAKE3 hex of the shared content blob.
    /// `content_type` — MIME type.
    /// `source_file_id` — opaque file UUID string of the **original** file.
    ///
    /// Implementors may use `source_file_id` to efficiently clone per-file
    /// metadata (audio tags, etc.) from the original rather than re-deriving
    /// it from the blob. If the original has not yet been processed, fall back
    /// to a blob-hash-based lookup or schedule a retry — the implementor owns
    /// race handling.
    fn on_file_copied(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        source_file_id: &str,
    );

    /// Called after an existing file's blob has been replaced (WebDAV PUT
    /// overwrite, WOPI PutFile, Nextcloud chunked upload finalization,
    /// or the collab actor's flush-to-blob).
    ///
    /// `file_id` — opaque file UUID string.
    /// `blob_hash` — BLAKE3 hex of the **new** blob.
    /// `content_type` — MIME type of the new content.
    /// `source` — who caused the write. External writers pass
    ///   [`WriteSource::External`]; the collab flusher passes
    ///   [`WriteSource::CollabFlush`]. Hooks that need to distinguish
    ///   "user or another protocol wrote this" from "collab bailed
    ///   its own CRDT to disk" branch on this value — notably the
    ///   collab-eviction hook, which must NOT evict on its own
    ///   flush (that would tear down every session on every
    ///   keystroke's debounced write).
    fn on_file_updated(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        source: WriteSource,
    );

    /// Called after a file record has been permanently removed (direct delete
    /// or emptied from trash).
    ///
    /// NOTE: due to deduplication the blob may still exist if other files
    /// reference it. Use [`BlobLifecycleHook::on_blob_deleted`] when your
    /// side-effect is content-addressed (e.g. removing blob-keyed thumbnails).
    ///
    /// `file_id` — opaque file UUID string.
    fn on_file_deleted(&self, file_id: &str);
}
