/// Observer notified by [`BlobHandler`] when a blob is stored for the first
/// time or permanently removed (ref_count reaches zero).
///
/// Register with [`BlobLifecycleService`] during DI wiring; it fans out to all
/// registered hooks. Every implementor **must** provide both methods —
/// use an explicit one-liner noop for events the implementor does not care about.
/// This forces conscious acknowledgement of every lifecycle event rather than
/// silent omission.
///
/// All methods are synchronous. Background work must be spawned inside the
/// implementor via `tokio::spawn`; the calling service never awaits hook
/// completion.
pub trait BlobLifecycleHook: Send + Sync {
    /// Called after a genuinely new blob has been written to storage (no dedup
    /// hit — first time this content hash is seen).
    ///
    /// `blob_hash` — BLAKE3 hex identifying the blob.
    /// `content_type` — MIME type if known at write time, `None` otherwise.
    fn on_blob_created(&self, blob_hash: &str, content_type: Option<&str>);

    /// Called after a blob's ref_count reaches zero and it has been permanently
    /// removed from storage.
    ///
    /// `blob_hash` — BLAKE3 hex identifying the (now deleted) blob.
    fn on_blob_deleted(&self, blob_hash: &str);
}
