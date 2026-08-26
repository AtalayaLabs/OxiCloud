use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use rayon::prelude::*;
/**
 * Thumbnail Generation Service
 *
 * Generates and manages image thumbnails for fast gallery previews.
 *
 * Features:
 * - Background thumbnail generation after upload
 * - Multiple sizes (icon 150x150, preview 800x600)
 * - JPEG output (lossy q=80) for compact thumbnails
 * - Lock-free moka cache with weight-based eviction
 * - Lazy generation on first request if not pre-generated
 * - Timeout protection for large image processing
 */
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::application::ports::thumbnail_ports::{
    ThumbnailFormat, ThumbnailPort, ThumbnailSize as PortThumbnailSize, ThumbnailStatsDto,
};
use crate::application::ports::video_frame_ports::VideoFramePort;
use crate::domain::errors::{DomainError, ErrorKind};
use crate::infrastructure::services::dedup_service::DedupService;

/// Thumbnail sizes supported by the system
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbnailSize {
    /// Small icon for file listings (150x150)
    Icon,
    /// Medium preview for gallery view (400x400)
    Preview,
    /// Large preview for detail view (800x800)
    Large,
}

impl ThumbnailSize {
    /// Get the maximum dimension for this size
    pub fn max_dimension(&self) -> u32 {
        match self {
            ThumbnailSize::Icon => 150,
            ThumbnailSize::Preview => 400,
            ThumbnailSize::Large => 800,
        }
    }

    /// Get the directory name for this size
    pub fn dir_name(&self) -> &'static str {
        match self {
            ThumbnailSize::Icon => "icon",
            ThumbnailSize::Preview => "preview",
            ThumbnailSize::Large => "large",
        }
    }

    /// Get all thumbnail sizes
    pub fn all() -> &'static [ThumbnailSize] {
        &[
            ThumbnailSize::Icon,
            ThumbnailSize::Preview,
            ThumbnailSize::Large,
        ]
    }
}

/// Cache key for the in-RAM thumbnail tier (moka). Includes `format` so WebP
/// and the JPEG fallback for the same (content, size) are distinct entries
/// (no cross-format collision).
///
/// Not to be confused with the two other caches on this path: the sidecar
/// files under `thumbnails_root` (already blob-hash keyed), and
/// `CachedBlobBackend`, the on-disk LRU in front of a remote blob backend
/// that only comes into play when a derived blob is read through the dedup
/// stack.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ThumbnailCacheKey {
    /// Content hash for rendered thumbnails; `ext-{file_id}` for
    /// client-uploaded video frames, which really are per-file.
    ///
    /// Keying on the hash rather than the file id is what makes the tier
    /// coherent with the HTTP ETag. When a file's content is replaced its
    /// id survives, so a file-keyed entry stayed valid-looking and had to
    /// be invalidated explicitly — which `on_file_updated` does from a
    /// spawned task, leaving a window where the response carried the NEW
    /// ETag over the OLD bytes. Since the response is `immutable` with a
    /// one-year max-age, a client landing in that window cached stale
    /// bytes permanently. Content keying removes the window rather than
    /// narrowing it: new content is a different key, so it cannot hit.
    ///
    /// It also stops N copies of one photo occupying N entries for
    /// identical bytes.
    ///
    /// The two namespaces cannot collide: hashes are 64 hex characters,
    /// and the external form carries an `ext-` prefix and a UUID.
    id: String,
    size: ThumbnailSize,
    format: ThumbnailFormat,
}

impl ThumbnailCacheKey {
    /// Rendered thumbnail — keyed by the source content hash.
    fn content(hash: &str, size: ThumbnailSize, format: ThumbnailFormat) -> Self {
        Self {
            id: hash.to_string(),
            size,
            format,
        }
    }

    /// Client-uploaded video frame — genuinely per-file, always JPEG.
    fn external(file_id: &str, size: ThumbnailSize) -> Self {
        Self {
            id: format!("ext-{file_id}"),
            size,
            format: ThumbnailFormat::Jpeg,
        }
    }
}

/// Maximum pixel count before rejecting decode (50 megapixels → ~200 MB RGBA).
/// Images above this are silently skipped — protects against single-image OOM.
const MAX_DECODE_PIXELS: u64 = 50_000_000;

/// JPEG quality for the fallback codec.
const JPEG_QUALITY: u8 = 80;
/// Lossy WebP quality for the primary codec. q82 lands within ~0.005 SSIM of
/// JPEG q80 (imperceptible at thumbnail scale) while encoding markedly smaller:
/// ~60% on the smooth photo-realistic bench corpus, and a more modest but still
/// substantial win (~25-40%) expected on real photos with edges/text/foliage.
/// Raise for more fidelity, lower for more bandwidth savings — see the E1 sweep
/// in `examples/bench_thumbnails_mem`.
const WEBP_QUALITY: f32 = 82.0;

/// Environment override for the decode-concurrency cap (ops tuning).
const DECODE_CONCURRENCY_ENV: &str = "OXICLOUD_THUMBNAIL_DECODE_CONCURRENCY";

/// Wall-clock cap for streaming a video blob to a temp file before frame
/// extraction. Bounds a stalled remote object-store read so the background task
/// (and its temp file) can't hang forever.
const STREAM_TO_TEMP_TIMEOUT: Duration = Duration::from_secs(120);

/// Compute max concurrent thumbnail decode operations at runtime.
///
/// Uses all available CPUs (min 2). Before shrink-on-load each decode
/// materialised the full-resolution RGBA bitmap (~96 MB for a 12 MP photo), so
/// concurrency was halved to keep peak RAM in check. Decodes are now DCT-shrunk
/// to the thumbnail size (~18–25 MB regardless of source resolution), so the RAM
/// ceiling no longer forces throttling and we can saturate every core. Override
/// with `OXICLOUD_THUMBNAIL_DECODE_CONCURRENCY`. `available_parallelism()`
/// respects cgroup limits (Docker/K8s) and CPU affinity masks.
fn max_concurrent_decodes() -> usize {
    if let Some(n) = std::env::var(DECODE_CONCURRENCY_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 1)
    {
        return n;
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cpus.max(2)
}

/// Thumbnail service for generating and caching image thumbnails
pub struct ThumbnailService {
    /// Root path for thumbnail storage
    thumbnails_root: PathBuf,
    /// Lock-free concurrent cache (moka) with weight-based eviction
    cache: moka::future::Cache<ThumbnailCacheKey, Bytes>,
    /// Configured maximum cache weight (for stats reporting)
    max_cache_bytes: u64,
    /// Limits how many images are decoded in parallel. Post shrink-on-load each
    /// decode is bounded (~18–25 MB), so this now exists mainly to avoid CPU
    /// oversubscription rather than to cap RAM; defaults to all cores.
    decode_semaphore: Arc<Semaphore>,
    /// Timeout for thumbnail generation operations to prevent hanging on large images.
    /// Defaults to 30 seconds.
    generation_timeout: Duration,
}

impl ThumbnailService {
    /// Create a new thumbnail service
    ///
    /// # Arguments
    /// * `storage_root` - Root path of file storage
    /// * `max_cache_entries` - (ignored — moka uses weight-based eviction)
    /// * `max_cache_bytes` - Maximum total bytes to cache
    /// * `generation_timeout` - Timeout for thumbnail generation operations
    pub fn new(
        storage_root: &Path,
        max_cache_entries: usize,
        max_cache_bytes: usize,
        generation_timeout: Option<Duration>,
    ) -> Self {
        let thumbnails_root = storage_root.join(".thumbnails");

        // Ignore max_cache_entries — weight-based eviction is more accurate
        // for variable-size thumbnails than entry-count limits.
        let _ = max_cache_entries;

        // No time_to_live — thumbnails are immutable (content never changes
        // for a given file_id).  Eviction is purely weight-based: when the
        // cache exceeds max_cache_bytes the lightest entries are dropped.
        // On eviction the thumbnail is still on disk; the next request
        // promotes it back with a single async read (~0.1 ms).
        let cache = moka::future::Cache::builder()
            .max_capacity(max_cache_bytes as u64)
            .weigher(|_key: &ThumbnailCacheKey, value: &Bytes| -> u32 {
                value.len().min(u32::MAX as usize) as u32
            })
            .build();

        Self {
            thumbnails_root,
            cache,
            max_cache_bytes: max_cache_bytes as u64,
            decode_semaphore: Arc::new(Semaphore::new(max_concurrent_decodes())),
            generation_timeout: generation_timeout.unwrap_or(Duration::from_secs(30)),
        }
    }

    /// Initialize the thumbnail directories
    pub async fn initialize(&self) -> std::io::Result<()> {
        for size in ThumbnailSize::all() {
            let dir = self.thumbnails_root.join(size.dir_name());
            fs::create_dir_all(&dir).await?;
        }
        tracing::info!(
            "🖼️ Thumbnail service initialized at {:?}",
            self.thumbnails_root
        );
        Ok(())
    }

    /// Check if a file is an image that can have thumbnails
    pub fn is_supported_image(mime_type: &str) -> bool {
        matches!(
            mime_type,
            "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/webp"
        )
    }

    /// Get the path where a thumbnail would be stored (keyed by blob hash for
    /// dedup; the extension encodes the format: `.webp` primary, `.jpg` fallback).
    /// This is the single source of truth for blob-hash thumbnail paths.
    fn get_thumbnail_path(
        &self,
        blob_hash: &str,
        size: ThumbnailSize,
        format: ThumbnailFormat,
    ) -> PathBuf {
        self.thumbnails_root
            .join(size.dir_name())
            .join(format!("{}.{}", blob_hash, format.ext()))
    }

    /// Persist a freshly rendered thumbnail to every durable tier.
    ///
    /// **One place that knows what persisting a thumbnail means.** Before
    /// this, four render paths each wrote the sidecar and exactly one also
    /// recorded the `content_derived_blobs` row, so an on-demand render — a
    /// cache miss, a size never generated, an evicted sidecar — produced
    /// state the migration could never see. That is not untidy, it breaks
    /// the migration's premise: `thumb_derived_import` would never reach an
    /// empty tail, and the gate for deleting the sidecar would never open.
    ///
    /// Scope is deliberately *durable* tiers only. The moka entry is left to
    /// callers because several persist through `cache.entry().or_insert_with`,
    /// which already owns the insert; doing it here too would write twice.
    ///
    /// Dual-write is the interim setting, not the destination. Once the
    /// derived tier is authoritative and the imports have drained, dropping
    /// the sidecar becomes a one-line change *here* rather than four edits
    /// spread across the file — which is the point of consolidating first.
    ///
    /// Both writes are best-effort and logged: the bytes are already rendered
    /// and about to be served, so a persistence failure must cost a
    /// re-render later, never the response now. `dedup: None` means
    /// sidecar-only — a caller that could not supply one, which is visible at
    /// the call site rather than hidden as a missing line.
    async fn persist_rendered(
        &self,
        blob_hash: &str,
        size: ThumbnailSize,
        format: ThumbnailFormat,
        bytes: &Bytes,
        dedup: Option<&DedupService>,
    ) {
        let thumb_path = self.get_thumbnail_path(blob_hash, size, format);
        if let Some(parent) = thumb_path.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        if let Err(e) = fs::write(&thumb_path, bytes).await {
            tracing::warn!(
                "Failed to save thumbnail sidecar {} {:?}: {e}",
                &blob_hash[..blob_hash.len().min(12)],
                size
            );
        }

        if let Some(dedup) = dedup
            && let Err(e) = dedup
                .store_derived_blob(
                    blob_hash,
                    "thumbnail",
                    size.dir_name(),
                    format.mime(),
                    bytes.clone(),
                )
                .await
        {
            tracing::warn!(
                target: "oxicloud::dedup",
                error = %e,
                "failed to record derived blob for {} {:?}",
                &blob_hash[..blob_hash.len().min(12)],
                size,
            );
        }
    }

    /// Get a thumbnail, generating it if needed.
    ///
    /// # Arguments
    /// * `file_id` - ID of the original file (used as moka cache key)
    /// * `blob_hash` - Content hash of the file (used as disk key for dedup)
    /// * `size` - Desired thumbnail size
    /// * `original_path` - Path to the original image file
    ///
    /// # Returns
    /// Bytes of the thumbnail image (JPEG format)
    pub async fn get_thumbnail(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: ThumbnailSize,
        format: ThumbnailFormat,
        original_path: &Path,
    ) -> Result<Bytes, ThumbnailError> {
        let cache_key = ThumbnailCacheKey::content(blob_hash, size, format);

        let thumb_path = self.get_thumbnail_path(blob_hash, size, format);
        let original_owned = original_path.to_path_buf();
        let file_id_owned = file_id.to_string();
        let blob_hash_owned = blob_hash.to_string();

        // Moka's entry().or_insert_with() guarantees that for the same key
        // only ONE init closure runs; concurrent callers await the same
        // computation instead of stampeding (thundering-herd protection).
        let entry = self
            .cache
            .entry(cache_key)
            .or_insert_with(async {
                // 1. Try loading from disk
                if let Ok(data) = fs::read(&thumb_path).await {
                    tracing::debug!(
                        "💾 Thumbnail loaded from disk: {} {:?}",
                        file_id_owned,
                        size
                    );
                    return Bytes::from(data);
                }

                // 2. Generate thumbnail (CPU-bound, runs in spawn_blocking)
                tracing::info!("🎨 Generating thumbnail: {} {:?}", file_id_owned, size);
                match self.generate_thumbnail(&original_owned, size, format).await {
                    Ok(bytes) => {
                        // `None`: renders from an on-disk original and holds
                        // no DedupService, so sidecar-only. Visible here
                        // rather than absent.
                        self.persist_rendered(&blob_hash_owned, size, format, &bytes, None)
                            .await;
                        bytes
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Thumbnail generation failed for {} {:?}: {e}",
                            file_id_owned,
                            size
                        );
                        // Return empty sentinel — will be evicted quickly by
                        // the weigher (weight 0) and retried on next request.
                        Bytes::new()
                    }
                }
            })
            .await;

        let bytes = entry.into_value();
        if bytes.is_empty() {
            return Err(ThumbnailError::ImageError(
                "Thumbnail generation failed".to_string(),
            ));
        }

        tracing::debug!("🔥 Thumbnail served: {} {:?}", file_id, size);
        Ok(bytes)
    }

    /// Get a thumbnail from raw image bytes, generating it if needed.
    ///
    /// This is the storage-model-safe entrypoint for CDC/manifest-backed
    /// blobs where no single local source file exists on disk. Prefer
    /// [`Self::get_thumbnail_from_blob`] on request paths — it defers the
    /// full blob read until a decode permit is held, so a stampede of
    /// cache misses cannot stack one source image per request in RAM.
    pub async fn get_thumbnail_from_bytes(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: ThumbnailSize,
        format: ThumbnailFormat,
        original_data: Bytes,
    ) -> Result<Bytes, ThumbnailError> {
        let cache_key = ThumbnailCacheKey::content(blob_hash, size, format);

        let thumb_path = self.get_thumbnail_path(blob_hash, size, format);
        let file_id_owned = file_id.to_string();
        let blob_hash_owned = blob_hash.to_string();

        let entry = self
            .cache
            .entry(cache_key)
            .or_insert_with(async move {
                if let Ok(data) = fs::read(&thumb_path).await {
                    tracing::debug!(
                        "💾 Thumbnail loaded from disk: {} {:?}",
                        file_id_owned,
                        size
                    );
                    return Bytes::from(data);
                }

                let Ok(_permit) = self.decode_semaphore.acquire().await else {
                    tracing::warn!("Decode semaphore closed, skipping {}", file_id_owned);
                    return Bytes::new();
                };
                // `None`: this entry point takes the original bytes directly
                // and has no DedupService, so it persists sidecar-only. The
                // gap is visible here rather than hidden as a missing write,
                // and closing it means threading dedup in from its callers.
                self.generate_and_persist(
                    &file_id_owned,
                    &blob_hash_owned,
                    size,
                    format,
                    original_data,
                    None,
                )
                .await
            })
            .await;

        let bytes = entry.into_value();
        if bytes.is_empty() {
            return Err(ThumbnailError::ImageError(
                "Thumbnail generation failed".to_string(),
            ));
        }

        tracing::debug!("🔥 Thumbnail served: {} {:?}", file_id, size);
        Ok(bytes)
    }

    /// Get a thumbnail for a content-addressed blob, generating it if needed.
    ///
    /// Request-path entrypoint: on a memory+disk cache miss the source blob
    /// is read **after** a decode permit is acquired, so peak RAM under a
    /// thumbnail stampede is `permits × image size` instead of
    /// `in-flight requests × image size`. moka's per-key init additionally
    /// collapses concurrent requests for the same thumbnail into one read.
    pub async fn get_thumbnail_from_blob(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: ThumbnailSize,
        format: ThumbnailFormat,
        dedup: Arc<DedupService>,
    ) -> Result<Bytes, ThumbnailError> {
        let cache_key = ThumbnailCacheKey::content(blob_hash, size, format);

        let thumb_path = self.get_thumbnail_path(blob_hash, size, format);
        let file_id_owned = file_id.to_string();
        let blob_hash_owned = blob_hash.to_string();

        let entry = self
            .cache
            .entry(cache_key)
            .or_insert_with(async move {
                if let Ok(data) = fs::read(&thumb_path).await {
                    tracing::debug!(
                        "💾 Thumbnail loaded from disk: {} {:?}",
                        file_id_owned,
                        size
                    );
                    return Bytes::from(data);
                }

                let Ok(_permit) = self.decode_semaphore.acquire().await else {
                    tracing::warn!("Decode semaphore closed, skipping {}", file_id_owned);
                    return Bytes::new();
                };
                let original_data = match dedup.read_blob_bytes(&blob_hash_owned).await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to read blob for thumbnail {} {:?}: {e}",
                            file_id_owned,
                            size
                        );
                        return Bytes::new();
                    }
                };
                // The on-demand render the REST handler falls through to on a
                // cache miss — the busiest path that previously wrote a
                // sidecar and no row. `dedup` is already in scope here, so
                // dual-writing costs nothing.
                self.generate_and_persist(
                    &file_id_owned,
                    &blob_hash_owned,
                    size,
                    format,
                    original_data,
                    Some(dedup.as_ref()),
                )
                .await
            })
            .await;

        let bytes = entry.into_value();
        if bytes.is_empty() {
            return Err(ThumbnailError::ImageError(
                "Thumbnail generation failed".to_string(),
            ));
        }

        tracing::debug!("🔥 Thumbnail served: {} {:?}", file_id, size);
        Ok(bytes)
    }

    /// Decode `original_data` into one thumbnail size, persist it to its
    /// blob-keyed disk path, and return the encoded bytes — empty `Bytes`
    /// on failure (moka's zero-weight negative-entry convention).
    ///
    /// Callers must hold a `decode_semaphore` permit.
    async fn generate_and_persist(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: ThumbnailSize,
        format: ThumbnailFormat,
        original_data: Bytes,
        dedup: Option<&DedupService>,
    ) -> Bytes {
        tracing::info!("🎨 Generating thumbnail: {} {:?}", file_id, size);
        match Self::generate_thumbnail_from_data(
            original_data,
            size,
            format,
            self.generation_timeout,
        )
        .await
        {
            Ok(bytes) => {
                self.persist_rendered(blob_hash, size, format, &bytes, dedup)
                    .await;
                bytes
            }
            Err(e) => {
                tracing::warn!(
                    "Thumbnail generation failed for {} {:?}: {e}",
                    file_id,
                    size
                );
                Bytes::new()
            }
        }
    }

    /// Try to serve a thumbnail from cache only (memory → disk).
    ///
    /// Unlike `get_thumbnail`, this does **not** generate a new thumbnail.
    /// Useful for non-image file types (videos) where a client-generated
    /// thumbnail may have been uploaded previously.
    ///
    /// `blob_hash` is used to locate the file on disk (dedup-aware).
    /// If `None`, only the in-memory cache is checked (used for video
    /// thumbnails where blob_hash is not yet resolved).
    /// Identity of the bytes a thumbnail request will serve — the body of its
    /// HTTP ETag.
    ///
    /// Mirrors the tier precedence in [`Self::get_cached_thumbnail`], because
    /// an ETag that names a different tier than the one answering is worse
    /// than a coarse one: it lets two resources serving different bytes share
    /// a validator, and a shared cache may then hand either to either.
    ///
    /// * An **attached** blob wins, and its own hash is the identity. Nothing
    ///   else works: uploading a preview does not change the file's content,
    ///   so a source-keyed ETag would not change either — and with
    ///   `immutable` set, clients would never revalidate. Worse, a copy
    ///   inherits the source hash, so an original and a copy carrying
    ///   *different* uploaded previews would collide on one ETag.
    /// * Otherwise a **derived** blob's own hash. Strictly better than
    ///   source-keying, never worse: the pair is written from the same bytes,
    ///   and where they can diverge — a sidecar re-rendered while the derived
    ///   row stays pinned by `ON CONFLICT DO NOTHING` — a source-keyed ETag
    ///   is wrong too, because the renderer is not part of the key.
    /// * Otherwise the source-keyed form, which still identifies a render of
    ///   known content at a known size and format.
    ///
    /// Known gap: a legacy `ext-{file_id}.jpg` with no `file_attached_blobs`
    /// row yet falls through to the source-keyed form, so those bytes keep
    /// today's coarse validator until the import backfills the row. No worse
    /// than current behaviour, and it disappears with the migration.
    pub async fn thumbnail_content_id(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: ThumbnailSize,
        format: ThumbnailFormat,
        dedup: Option<&DedupService>,
    ) -> String {
        if let Some(dedup) = dedup {
            if let Some(attached) = dedup
                .find_attached_blob(file_id, "preview", size.dir_name())
                .await
            {
                return attached.blob_hash;
            }
            if let Some(derived) = dedup
                .find_derived_blob(blob_hash, "thumbnail", size.dir_name())
                .await
            {
                return derived.blob_hash;
            }
        }
        format!(
            "thumb-{}-{}-{}",
            blob_hash,
            size.dir_name(),
            format.as_str()
        )
    }

    /// Drain a blob through the dedup stack into memory.
    ///
    /// Shared by the attached and derived tiers — the only difference between
    /// them is which table produced the hash, so the read itself belongs in
    /// one place. Returns `None` on a read fault rather than propagating: a
    /// missing satellite must degrade to the next tier, never break a gallery.
    async fn read_blob_to_bytes(
        dedup: &DedupService,
        blob_hash: &str,
        file_id: &str,
        size: ThumbnailSize,
    ) -> Option<Bytes> {
        use futures::StreamExt;
        let mut stream = dedup.read_blob_stream(blob_hash).await.ok()?;
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(part) => buf.extend_from_slice(&part),
                Err(e) => {
                    tracing::warn!(
                        target: "oxicloud::dedup",
                        error = %e,
                        "thumbnail blob read failed for {} {:?}",
                        file_id,
                        size,
                    );
                    return None;
                }
            }
        }
        Some(Bytes::from(buf))
    }

    pub async fn get_cached_thumbnail(
        &self,
        file_id: &str,
        blob_hash: Option<&str>,
        size: ThumbnailSize,
        format: ThumbnailFormat,
        // Concrete, and optional: `ThumbnailPort` is never used as a trait
        // object (checked), and `DedupPort` uses native `async fn` so it is
        // not dyn-compatible anyway. `None` means sidecar-only — exactly
        // today's behaviour, which is what the port impl wants.
        dedup: Option<&DedupService>,
    ) -> Option<Bytes> {
        // A file-specific override beats anything derived from the content,
        // and that has to hold at EVERY tier — including RAM. Checking the
        // content-keyed entry first would let a previously-rendered
        // thumbnail shadow a preview the user has since uploaded: the render
        // is cached under `content(hash)`, the upload lands under
        // `external(file_id)`, and the content key would win forever.
        //
        // So the order is: per-file RAM, per-file disk, per-file DB, then the
        // content-keyed tiers. Same precedence as the disk tiers below, just
        // applied one level up.

        // 1. Per-file override in RAM (uploaded preview / video frame).
        if let Some(bytes) = self
            .cache
            .get(&ThumbnailCacheKey::external(file_id, size))
            .await
            && !bytes.is_empty()
        {
            return Some(bytes);
        }

        // 2. Check disk for external (video-frame) thumbnails stored by file_id.
        //    These are JPEG-only (ext-{file_id}.jpg) regardless of requested
        //    format; the byte-sniffing Content-Type makes serving correct.
        let ext_path = self
            .thumbnails_root
            .join(size.dir_name())
            .join(format!("ext-{}.jpg", file_id));
        if let Ok(data) = fs::read(&ext_path).await {
            let bytes = Bytes::from(data);
            // Cache under a Jpeg-pinned key: these bytes are always JPEG, so the
            // key's format must describe them. Inserting under `cache_key` (whose
            // format is the *requested* format, possibly Webp) would store JPEG
            // bytes behind a Webp key — a latent cross-format invariant violation.
            let ext_key = ThumbnailCacheKey::external(file_id, size);
            self.cache.insert(ext_key, bytes.clone()).await;
            return Some(bytes);
        }

        // 2b. Bytes the USER attached to this file, if any.
        //
        // Ahead of every content-derived tier below on purpose: an uploaded
        // preview is an explicit choice about THIS file and must beat
        // anything the server would render from its content. It is also the
        // only tier a copy can inherit — the `ext-` sidecar above is keyed by
        // file_id and is not copied, so without this branch a copied file
        // silently falls back to a rendered thumbnail, or to none at all for
        // a PDF that has no server-side render path.
        if let Some(dedup) = dedup
            && let Some(attached) = dedup
                .find_attached_blob(file_id, "preview", size.dir_name())
                .await
            && let Some(bytes) =
                Self::read_blob_to_bytes(dedup, &attached.blob_hash, file_id, size).await
        {
            // Cached under the per-file key: these bytes belong to this file,
            // not to its content, so a content key would leak them to every
            // other file sharing that content — the poisoning the file-keyed
            // table exists to prevent.
            self.cache
                .insert(ThumbnailCacheKey::external(file_id, size), bytes.clone())
                .await;
            return Some(bytes);
        }

        // 3. Content-keyed RAM tier. Below the per-file tiers by the rule
        //    above; still ahead of every disk read.
        //
        //    A caller that did not resolve the hash cannot consult it and
        //    falls through to disk. That is correct rather than merely
        //    acceptable: a file-id key here would be the stale entry content
        //    keying exists to avoid. Both HTTP handlers resolve the hash to
        //    build the ETag, so the fall-through is confined to internal
        //    callers that never had one.
        let hash = blob_hash?;
        if let Some(bytes) = self
            .cache
            .get(&ThumbnailCacheKey::content(hash, size, format))
            .await
            && !bytes.is_empty()
        {
            return Some(bytes);
        }

        // 4. Check disk for blob-hash thumbnails (needs blob_hash to locate)
        let thumb_path = self.get_thumbnail_path(hash, size, format);
        if let Ok(data) = fs::read(&thumb_path).await {
            let bytes = Bytes::from(data);
            // Populate in-memory cache for next hit
            self.cache
                .insert(
                    ThumbnailCacheKey::content(hash, size, format),
                    bytes.clone(),
                )
                .await;
            return Some(bytes);
        }

        // 4. Tier-3 derived blob. Deliberately LAST while the sidecar still
        // exists: for every thumbnail already on disk this branch is never
        // reached, so the DB stays off the hot path and a fault here cannot
        // break a working gallery. It answers only what disk cannot — another
        // instance's render, or a box whose sidecar was never populated.
        //
        // The order flips (derived blob first, sidecar as fallback) in the
        // release that removes the sidecar; see docs/plan/derived-blobs.md.
        let dedup = dedup?;
        let derived = dedup
            .find_derived_blob(hash, "thumbnail", size.dir_name())
            .await?;
        let bytes = Self::read_blob_to_bytes(dedup, &derived.blob_hash, file_id, size).await?;
        self.cache
            .insert(
                ThumbnailCacheKey::content(hash, size, format),
                bytes.clone(),
            )
            .await;
        Some(bytes)
    }

    /// Store an externally-generated thumbnail (e.g. client-side video frame).
    ///
    /// **Fast path**: if the payload is already a correctly-sized JPEG, it is
    /// stored as-is — zero decode, zero encode.  The browser pre-scales the
    /// canvas to 400 px and sends JPEG, so this fast path is hit on every
    /// normal video-thumbnail upload.
    ///
    /// **Slow path**: decode → optional resize → re-encode to JPEG q=80.
    /// Only triggered when a client sends an oversized or non-JPEG image.
    ///
    /// External thumbnails (video frames) are stored by `file_id` since
    /// they are client-generated and not dedup-able.
    pub async fn store_external_thumbnail(
        &self,
        file_id: &str,
        size: ThumbnailSize,
        data: Bytes,
    ) -> Result<Bytes, ThumbnailError> {
        let max_dim = size.max_dimension();

        // Validate + optionally re-encode in blocking thread
        let jpeg_bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, ThumbnailError> {
            // ── Fast path: already a correctly-sized JPEG ─────────────
            // JPEG files start with SOI marker 0xFF 0xD8.
            if data.len() >= 2
                && data[0] == 0xFF
                && data[1] == 0xD8
                && let Ok(reader) =
                    image::ImageReader::new(std::io::Cursor::new(&data)).with_guessed_format()
                && let Ok((w, h)) = reader.into_dimensions()
                && w <= max_dim
                && h <= max_dim
            {
                // Already JPEG at correct size — zero-copy store
                return Ok(data.to_vec());
            }

            // ── Slow path: decode, resize, re-encode to JPEG ─────────
            let img = image::load_from_memory(&data)
                .map_err(|e| ThumbnailError::ImageError(format!("Invalid image data: {e}")))?;

            let (w, h) = (img.width(), img.height());
            let img = if w > max_dim || h > max_dim {
                let filter = FilterType::CatmullRom;
                if w > h {
                    let ratio = max_dim as f32 / w as f32;
                    img.resize(max_dim, (h as f32 * ratio) as u32, filter)
                } else {
                    let ratio = max_dim as f32 / h as f32;
                    img.resize((w as f32 * ratio) as u32, max_dim, filter)
                }
            } else {
                img
            };

            let rgb = img.to_rgb8();
            let mut buffer = Vec::new();
            let encoder = JpegEncoder::new_with_quality(&mut buffer, 80);
            rgb.write_with_encoder(encoder)
                .map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
            Ok(buffer)
        })
        .await
        .map_err(|e| ThumbnailError::TaskError(e.to_string()))??;

        let bytes = Bytes::from(jpeg_bytes);

        // External thumbnails are stored by file_id (not dedup-able)
        let thumb_path = self
            .thumbnails_root
            .join(size.dir_name())
            .join(format!("ext-{}.jpg", file_id));
        if let Some(parent) = thumb_path.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        fs::write(&thumb_path, &bytes)
            .await
            .map_err(|e| ThumbnailError::IoError(e.to_string()))?;

        // Populate in-memory cache (external thumbnails are JPEG)
        let cache_key = ThumbnailCacheKey::external(file_id, size);
        self.cache.insert(cache_key, bytes.clone()).await;

        tracing::info!("✅ Stored external thumbnail: {} {:?}", file_id, size);
        Ok(bytes)
    }

    /// Cheap magic-byte check for a JPEG container (SOI + first marker).
    fn is_jpeg(data: &[u8]) -> bool {
        data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF
    }

    /// Decode `data` into a `DynamicImage` sized for a thumbnail whose longest
    /// side is `target_long`, with EXIF orientation already applied.
    ///
    /// For JPEG this is **shrink-on-load**: the decoder emits the image at the
    /// smallest DCT scale (1/8·1/4·1/2·1/1) whose longest axis is still ≥
    /// `target_long`, so a 12 MP photo decodes ~16× fewer pixels for an 800 px
    /// thumbnail — and the full-resolution bitmap (the dominant time/RAM cost)
    /// is never materialised. PNG/GIF/WebP (no DCT scaling) and unusual JPEG
    /// colour spaces (CMYK / 16-bit grey) fall back to a full decode.
    fn decode_oriented(
        data: &[u8],
        target_long: u32,
    ) -> Result<image::DynamicImage, ThumbnailError> {
        // JPEG fast path: shrink-on-load. A non-JPEG, or a JPEG colour space we
        // don't map (CMYK / 16-bit grey → `None`), falls through to a full decode.
        if Self::is_jpeg(data)
            && let Some(img) = Self::decode_jpeg_scaled(data, target_long)?
        {
            return Ok(Self::apply_exif_orientation(data, img));
        }

        let (w, h) = image::ImageReader::new(std::io::Cursor::new(data))
            .with_guessed_format()
            .map_err(|e| ThumbnailError::ImageError(e.to_string()))?
            .into_dimensions()
            .map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
        if (w as u64) * (h as u64) > MAX_DECODE_PIXELS {
            return Err(ThumbnailError::ImageError(format!(
                "Image too large for thumbnail: {w}×{h} ({} MP, max {MAX_DECODE_PIXELS})",
                w as u64 * h as u64 / 1_000_000
            )));
        }
        let img =
            image::load_from_memory(data).map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
        Ok(Self::apply_exif_orientation(data, img))
    }

    /// JPEG shrink-on-load. Returns `Ok(None)` for colour spaces we don't map
    /// (CMYK / 16-bit grey), signalling the caller to fall back to a full decode.
    fn decode_jpeg_scaled(
        data: &[u8],
        target_long: u32,
    ) -> Result<Option<image::DynamicImage>, ThumbnailError> {
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(data));
        // read_info() first so the dimension guard sees the *original* size.
        decoder
            .read_info()
            .map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
        let info = decoder
            .info()
            .ok_or_else(|| ThumbnailError::ImageError("missing JPEG metadata".into()))?;
        let (orig_w, orig_h) = (info.width as u64, info.height as u64);
        if orig_w * orig_h > MAX_DECODE_PIXELS {
            return Err(ThumbnailError::ImageError(format!(
                "Image too large for thumbnail: {orig_w}×{orig_h} ({} MP, max {MAX_DECODE_PIXELS})",
                orig_w * orig_h / 1_000_000
            )));
        }

        // Request a `target_long` square box: jpeg-decoder picks the smallest
        // scale whose longest axis is still ≥ target_long (its "≥ in at least
        // one axis" rule reduces to the long axis since it dominates), so the
        // later resample step only ever downscales — never upscales/blurs.
        let req = target_long.min(u16::MAX as u32) as u16;
        let (sw, sh) = decoder
            .scale(req, req)
            .map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
        let pixels = decoder
            .decode()
            .map_err(|e| ThumbnailError::ImageError(e.to_string()))?;

        let (sw, sh) = (sw as u32, sh as u32);
        let img = match decoder.info().map(|i| i.pixel_format) {
            Some(jpeg_decoder::PixelFormat::RGB24) => {
                image::RgbImage::from_raw(sw, sh, pixels).map(image::DynamicImage::ImageRgb8)
            }
            Some(jpeg_decoder::PixelFormat::L8) => {
                image::GrayImage::from_raw(sw, sh, pixels).map(image::DynamicImage::ImageLuma8)
            }
            _ => None,
        };
        Ok(img)
    }

    /// Read EXIF orientation from the original bytes and rotate/flip the image.
    /// (Applied after shrink-on-load, so the rotation works on the small bitmap.)
    fn apply_exif_orientation(data: &[u8], img: image::DynamicImage) -> image::DynamicImage {
        use crate::infrastructure::services::exif_service::{ExifService, apply_orientation};
        let orientation = ExifService::extract(data)
            .and_then(|m| m.orientation)
            .unwrap_or(1);
        apply_orientation(img, orientation)
    }

    /// Aspect-ratio-preserving target dimensions so the longest side equals
    /// `max_dim` (clamped to ≥1 to keep the SIMD resizer happy on extreme ratios).
    fn fit_dims(src_w: u32, src_h: u32, max_dim: u32) -> (u32, u32) {
        // Never upsample. A sub-`max_dim` source (a favicon, a 1×1
        // pixel, an avatar under the Icon size) is encoded at its
        // original resolution — upscaling wastes bytes AND, on sources
        // smaller than the convolution kernel's support radius,
        // `fast_image_resize`'s Lanczos3/CatmullRom paths reject the
        // resize outright, surfacing as `ImageError("...")` from
        // `encode_thumbnail`.
        let max_dim = max_dim.min(src_w.max(src_h));
        if src_w > src_h {
            let ratio = max_dim as f32 / src_w as f32;
            (max_dim, ((src_h as f32 * ratio) as u32).max(1))
        } else {
            let ratio = max_dim as f32 / src_h as f32;
            (((src_w as f32 * ratio) as u32).max(1), max_dim)
        }
    }

    /// Resampling filter per size: Bilinear for the tiny icon (cheap, output is
    /// 150 px), Lanczos3 for preview/large (highest quality, SIMD-fast here).
    fn filter_for(size: ThumbnailSize) -> fast_image_resize::FilterType {
        use fast_image_resize::FilterType;
        match size {
            ThumbnailSize::Icon => FilterType::Bilinear,
            ThumbnailSize::Preview | ThumbnailSize::Large => FilterType::Lanczos3,
        }
    }

    /// SIMD-resize an RGB8 source to `dst_w×dst_h` (via `fast_image_resize`,
    /// AVX2/SSE4.1/NEON) and encode the result as a q80 JPEG.
    fn encode_thumbnail(
        src_rgb: &[u8],
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
        filter: fast_image_resize::FilterType,
        format: ThumbnailFormat,
    ) -> Result<Vec<u8>, ThumbnailError> {
        use fast_image_resize::images::{Image, ImageRef};
        use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

        // Lanczos3 is ideal for downscaling but rings on edges when upscaling;
        // fall back to a smooth bicubic (CatmullRom) whenever the target is
        // larger than the source on either axis.
        let filter = if dst_w > src_w || dst_h > src_h {
            FilterType::CatmullRom
        } else {
            filter
        };

        // Skip Resizer when the target already matches the source. Two
        // reasons: (1) `fit_dims` caps at source size so any src<=max_dim
        // path lands here; (2) fast_image_resize's convolution kernels
        // reject sources smaller than their support radius, so a 1×1
        // pass-through would still fail even at src==dst.
        let resized = if src_w == dst_w && src_h == dst_h {
            src_rgb.to_vec()
        } else {
            let src = ImageRef::new(src_w, src_h, src_rgb, PixelType::U8x3)
                .map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
            let mut dst = Image::new(dst_w, dst_h, PixelType::U8x3);
            let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter));
            Resizer::new()
                .resize(&src, &mut dst, &opts)
                .map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
            dst.into_vec()
        };
        match format {
            ThumbnailFormat::Jpeg => {
                let rgb = image::RgbImage::from_raw(dst_w, dst_h, resized).ok_or_else(|| {
                    ThumbnailError::ImageError("resize buffer size mismatch".into())
                })?;
                let mut buffer = Vec::new();
                let encoder = JpegEncoder::new_with_quality(&mut buffer, JPEG_QUALITY);
                rgb.write_with_encoder(encoder)
                    .map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
                Ok(buffer)
            }
            ThumbnailFormat::Webp => {
                // libwebp lossy — ~25-30% smaller than JPEG q80 at equal SSIM.
                Ok(webp::Encoder::from_rgb(&resized, dst_w, dst_h)
                    .encode(WEBP_QUALITY)
                    .to_vec())
            }
        }
    }

    fn render_thumbnail_from_data(
        data: &[u8],
        size: ThumbnailSize,
        format: ThumbnailFormat,
    ) -> Result<Vec<u8>, ThumbnailError> {
        let max_dim = size.max_dimension();
        let rgb = Self::decode_oriented(data, max_dim)?.into_rgb8();
        let (sw, sh) = (rgb.width(), rgb.height());
        let (nw, nh) = Self::fit_dims(sw, sh, max_dim);
        Self::encode_thumbnail(rgb.as_raw(), sw, sh, nw, nh, Self::filter_for(size), format)
    }

    fn render_all_thumbnails_from_data(
        data: &[u8],
        format: ThumbnailFormat,
    ) -> Result<Vec<(ThumbnailSize, Bytes)>, ThumbnailError> {
        // Decode once, shrunk-on-load for the largest size (800 px), and convert
        // to RGB8 once; all three sizes are then SIMD-resampled from this single
        // shared bitmap (the RGB conversion is no longer repeated per size).
        let rgb = Self::decode_oriented(data, ThumbnailSize::Large.max_dimension())?.into_rgb8();
        let (sw, sh) = (rgb.width(), rgb.height());
        let src = rgb.as_raw().as_slice();

        ThumbnailSize::all()
            .par_iter()
            .map(|&size| {
                let (nw, nh) = Self::fit_dims(sw, sh, size.max_dimension());
                let buf =
                    Self::encode_thumbnail(src, sw, sh, nw, nh, Self::filter_for(size), format)?;
                Ok((size, Bytes::from(buf)))
            })
            .collect::<Result<Vec<_>, ThumbnailError>>()
    }

    async fn generate_thumbnail_from_data(
        original_data: Bytes,
        size: ThumbnailSize,
        format: ThumbnailFormat,
        timeout_duration: Duration,
    ) -> Result<Bytes, ThumbnailError> {
        let spawn_result = tokio::task::spawn_blocking(move || {
            Self::render_thumbnail_from_data(original_data.as_ref(), size, format)
        });

        let result = timeout(timeout_duration, spawn_result)
            .await
            .map_err(|_| {
                ThumbnailError::TaskError(format!(
                    "Thumbnail generation timed out after {:?}",
                    timeout_duration
                ))
            })?
            .map_err(|e| ThumbnailError::TaskError(e.to_string()))?;

        result.map(Bytes::from)
    }

    /// Generate a thumbnail from an image file.
    ///
    /// Concurrency is bounded by `decode_semaphore` to prevent OOM when
    /// many images are uploaded simultaneously. Resolution is also
    /// capped at `MAX_DECODE_PIXELS` to reject pathologically large images.
    /// After decoding, the encoded image buffer is explicitly dropped before
    /// processing to minimize peak memory usage.
    /// A timeout prevents the operation from hanging indefinitely on large images.
    async fn generate_thumbnail(
        &self,
        original_path: &Path,
        size: ThumbnailSize,
        format: ThumbnailFormat,
    ) -> Result<Bytes, ThumbnailError> {
        let path = original_path.to_path_buf();
        let timeout_duration = self.generation_timeout;

        // Acquire semaphore permit — bounds peak RAM from concurrent decodes
        let _permit = self
            .decode_semaphore
            .acquire()
            .await
            .map_err(|_| ThumbnailError::TaskError("Decode semaphore closed".into()))?;

        // Run image processing in blocking thread pool with timeout
        let spawn_result =
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>, ThumbnailError> {
                let data =
                    std::fs::read(&path).map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
                Self::render_thumbnail_from_data(&data, size, format)
            });

        // Apply timeout to prevent hanging on large images
        let result = timeout(timeout_duration, spawn_result)
            .await
            .map_err(|_| {
                ThumbnailError::TaskError(format!(
                    "Thumbnail generation timed out after {:?}",
                    timeout_duration
                ))
            })?
            .map_err(|e| ThumbnailError::TaskError(e.to_string()))?;

        result.map(Bytes::from)
    }

    /// Generate all thumbnail sizes for a file in the background.
    ///
    /// Thumbnails are stored on disk keyed by `blob_hash`, so duplicate
    /// uploads with the same content share a single set of thumbnails.
    /// If thumbnails already exist for this `blob_hash`, only the moka
    /// cache is populated (zero CPU for image processing).
    ///
    /// Loads the image **once** and produces all 3 sizes (Icon, Preview,
    /// Large) inside a single `spawn_blocking` call. This avoids 3×
    /// I/O reads and 3× JPEG/PNG decode — reducing CPU time by ~45%
    /// and peak RAM from ~540 MB to ~180 MB for concurrent uploads.
    /// The encoded image buffer is explicitly dropped after decoding
    /// to further reduce peak memory by the size of the original file.
    pub fn generate_all_sizes_background(
        self: Arc<Self>,
        file_id: String,
        blob_hash: String,
        original_path: PathBuf,
    ) {
        tokio::spawn(async move {
            tracing::info!("🖼️ Background thumbnail generation starting: {}", file_id);

            // ── Fast path: blob-hash thumbnails already exist on disk ────
            // If another file with the same content was already uploaded,
            // the thumbnails exist. Just populate the moka cache for this
            // file_id and skip image processing entirely.
            let all_exist = {
                let mut ok = true;
                for size in ThumbnailSize::all() {
                    let thumb_path =
                        self.get_thumbnail_path(&blob_hash, *size, ThumbnailFormat::Webp);
                    if fs::metadata(&thumb_path).await.is_err() {
                        ok = false;
                        break;
                    }
                }
                ok
            };
            if all_exist {
                for size in ThumbnailSize::all() {
                    let thumb_path =
                        self.get_thumbnail_path(&blob_hash, *size, ThumbnailFormat::Webp);
                    if let Ok(data) = fs::read(&thumb_path).await {
                        let cache_key =
                            ThumbnailCacheKey::content(&blob_hash, *size, ThumbnailFormat::Webp);
                        self.cache.insert(cache_key, Bytes::from(data)).await;
                    }
                }
                tracing::info!(
                    "🖼️ Thumbnail dedup hit for {} (blob {}): skipped generation",
                    file_id,
                    &blob_hash[..12]
                );
                return;
            }

            // Acquire semaphore permit — bounds peak RAM from concurrent decodes
            let _permit = match self.decode_semaphore.acquire().await {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(
                        "Decode semaphore closed, skipping thumbnails for {}",
                        file_id
                    );
                    return;
                }
            };

            let path = original_path.clone();

            // Single spawn_blocking: read the file once, then run the shared
            // render path (shrink-on-load decode + SIMD resize) — identical to
            // the blob variant, so this path gets both optimisations and there
            // is no duplicated decode/resize logic.
            let results = tokio::task::spawn_blocking(move || {
                let data =
                    std::fs::read(&path).map_err(|e| ThumbnailError::ImageError(e.to_string()))?;
                Self::render_all_thumbnails_from_data(&data, ThumbnailFormat::Webp)
            })
            .await;

            // Flatten JoinError + inner ThumbnailError
            let thumbnails = match results {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => {
                    tracing::warn!("Thumbnail generation failed for {}: {}", file_id, e);
                    return;
                }
                Err(e) => {
                    tracing::warn!("Thumbnail task panicked for {}: {}", file_id, e);
                    return;
                }
            };

            // Save each size to disk and populate moka — both keyed by
            // blob_hash, so the two tiers agree and a copy shares them.
            for (size, bytes) in thumbnails {
                // `None`: this variant renders from a path and holds no
                // DedupService — `generate_all_sizes_background_from_blob` is
                // the one that does. Sidecar-only, visibly so.
                self.persist_rendered(&blob_hash, size, ThumbnailFormat::Webp, &bytes, None)
                    .await;
                {
                    // Populate in-memory cache for instant first-hit serving
                    let cache_key =
                        ThumbnailCacheKey::content(&blob_hash, size, ThumbnailFormat::Webp);
                    self.cache.insert(cache_key, bytes).await;
                    tracing::debug!("✅ Generated thumbnail: {} {:?}", file_id, size);
                }
            }

            tracing::info!("✅ Background thumbnail generation complete: {}", file_id);
        });
    }

    /// Generate all thumbnail sizes in the background for a content-addressed
    /// blob (CDC/manifest-safe — no physical source file required).
    ///
    /// The source blob is read **after** the decode permit is acquired, so N
    /// concurrent uploads queue as N small tasks, not N full images in RAM:
    /// peak memory is `permits × image size` regardless of upload concurrency.
    pub fn generate_all_sizes_background_from_blob(
        self: Arc<Self>,
        file_id: String,
        blob_hash: String,
        dedup: Arc<DedupService>,
    ) {
        tokio::spawn(async move {
            tracing::info!("🖼️ Background thumbnail generation starting: {}", file_id);

            // Guard: if the blob was deleted before this task ran, cleanup_if_orphaned
            // already fired with no thumbnails on disk — writing them now would leak them.
            // Use the DB check (manifest + blobs tables) as the authoritative source.
            if !dedup.blob_exists(&blob_hash).await {
                tracing::debug!(
                    "Blob {}… deleted before thumbnail task ran, skipping",
                    &blob_hash[..blob_hash.len().min(12)]
                );
                return;
            }

            let all_exist = {
                let mut ok = true;
                for size in ThumbnailSize::all() {
                    let thumb_path =
                        self.get_thumbnail_path(&blob_hash, *size, ThumbnailFormat::Webp);
                    if fs::metadata(&thumb_path).await.is_err() {
                        ok = false;
                        break;
                    }
                }
                ok
            };
            if all_exist {
                for size in ThumbnailSize::all() {
                    let thumb_path =
                        self.get_thumbnail_path(&blob_hash, *size, ThumbnailFormat::Webp);
                    if let Ok(data) = fs::read(&thumb_path).await {
                        let cache_key =
                            ThumbnailCacheKey::content(&blob_hash, *size, ThumbnailFormat::Webp);
                        self.cache.insert(cache_key, Bytes::from(data)).await;
                    }
                }
                tracing::info!(
                    "🖼️ Thumbnail dedup hit for {} (blob {}): skipped generation",
                    file_id,
                    &blob_hash[..12]
                );
                return;
            }

            let _permit = match self.decode_semaphore.acquire().await {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(
                        "Decode semaphore closed, skipping thumbnails for {}",
                        file_id
                    );
                    return;
                }
            };

            // Read the source only now that a permit bounds how many of
            // these full-image buffers can exist at once.
            let original_data = match dedup.read_blob_bytes(&blob_hash).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(
                        "Failed to read blob for thumbnail generation {}: {}",
                        file_id,
                        e
                    );
                    return;
                }
            };

            self.render_and_persist_all_webp(&file_id, &blob_hash, original_data, Some(&dedup))
                .await;

            tracing::info!("✅ Background thumbnail generation complete: {}", file_id);
        });
    }

    /// Render every size as WebP from a decoded source image and persist them by
    /// blob_hash (disk `{hash}.webp` + moka). Shared by the image upload path and
    /// the video path (which passes the extracted frame as the source), so both
    /// produce identical, dedup-able, content-negotiable thumbnails.
    /// `dedup` is `Some` on every path that has a handle, which is every
    /// eager background path. When present each rendered size is ALSO stored
    /// as a derived blob and recorded in `storage.content_derived_blobs`.
    ///
    /// The sidecar write is deliberately kept: this slice fills the table
    /// while reads still come from disk, so a rollback at any point leaves
    /// working thumbnails and the table can be inspected against real data
    /// before anything depends on it. See `docs/plan/derived-blobs.md`.
    async fn render_and_persist_all_webp(
        &self,
        file_id: &str,
        blob_hash: &str,
        source: Bytes,
        dedup: Option<&DedupService>,
    ) {
        let results = tokio::task::spawn_blocking(move || {
            Self::render_all_thumbnails_from_data(source.as_ref(), ThumbnailFormat::Webp)
        })
        .await;

        let thumbnails = match results {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                tracing::warn!("Thumbnail generation failed for {}: {}", file_id, e);
                return;
            }
            Err(e) => {
                tracing::warn!("Thumbnail task panicked for {}: {}", file_id, e);
                return;
            }
        };

        for (size, bytes) in thumbnails {
            // Was the only path that wrote both tiers, with its own copy of
            // the logic. Now the same `persist_rendered` every other render
            // path uses, so there is one definition of what persisting means
            // and the interim dual-write can be retired in one place.
            self.persist_rendered(blob_hash, size, ThumbnailFormat::Webp, &bytes, dedup)
                .await;

            let cache_key = ThumbnailCacheKey::content(blob_hash, size, ThumbnailFormat::Webp);
            self.cache.insert(cache_key, bytes).await;
            tracing::debug!("✅ Generated thumbnail: {} {:?}", file_id, size);
        }
    }

    /// Eagerly generate WebP thumbnails for a freshly-uploaded video.
    ///
    /// Streams the (decrypted, reassembled) blob to a temp file — bounded by
    /// `max_bytes` so a giant upload never materialises in full — extracts one
    /// representative frame via `video_frame`, then runs it through the same
    /// WebP/blob-hash pipeline as photos. Any miss or failure leaves the video
    /// without a thumbnail (the prior behaviour), never an error to the user.
    pub fn generate_video_thumbnails_background(
        self: Arc<Self>,
        file_id: String,
        blob_hash: String,
        dedup: Arc<DedupService>,
        video_frame: Arc<dyn VideoFramePort>,
        max_bytes: u64,
    ) {
        tokio::spawn(async move {
            // Blob deleted before this task ran → nothing to do.
            if !dedup.blob_exists(&blob_hash).await {
                return;
            }

            // Dedup hit: thumbnails for this content already exist on disk — just
            // warm the moka cache (zero CPU), mirroring the image path.
            let all_exist = {
                let mut ok = true;
                for size in ThumbnailSize::all() {
                    let p = self.get_thumbnail_path(&blob_hash, *size, ThumbnailFormat::Webp);
                    if fs::metadata(&p).await.is_err() {
                        ok = false;
                        break;
                    }
                }
                ok
            };
            if all_exist {
                for size in ThumbnailSize::all() {
                    let p = self.get_thumbnail_path(&blob_hash, *size, ThumbnailFormat::Webp);
                    if let Ok(data) = fs::read(&p).await {
                        let key =
                            ThumbnailCacheKey::content(&blob_hash, *size, ThumbnailFormat::Webp);
                        self.cache.insert(key, Bytes::from(data)).await;
                    }
                }
                return;
            }

            // ffmpeg needs a seekable file, and the blob may be encrypted/chunked
            // on disk — stream through the normal decrypting read path into a
            // bounded temp file (on the data volume) rather than reading the raw
            // blob path. Bounded by size (max_bytes) AND time, so a stalled remote
            // backend can't hang the task or leak its temp file forever.
            let tmp = match tokio::time::timeout(
                STREAM_TO_TEMP_TIMEOUT,
                Self::stream_blob_to_temp(&dedup, &blob_hash, max_bytes, &self.thumbnails_root),
            )
            .await
            {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => {
                    tracing::warn!("🎬 video thumb: stream {} failed: {}", file_id, e);
                    return;
                }
                Err(_) => {
                    tracing::warn!("🎬 video thumb: stream {} timed out", file_id);
                    return;
                }
            };

            let frame = match video_frame.extract_frame(tmp.path()).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::info!("🎬 video thumb: extract {} skipped: {}", file_id, e);
                    return;
                }
            };
            drop(tmp); // remove the temp video promptly; we have the frame bytes

            // Bound the CPU-bound decode+resize with the same decode_semaphore the
            // image path uses (the ffmpeg extractor's own permit covered only
            // extraction and is already released), so concurrent video uploads
            // can't spawn unbounded render jobs.
            let _permit = match self.decode_semaphore.acquire().await {
                Ok(p) => p,
                Err(_) => return,
            };
            self.render_and_persist_all_webp(&file_id, &blob_hash, frame, Some(&dedup))
                .await;
            tracing::info!("✅ Video thumbnail generation complete: {}", file_id);
        });
    }

    /// Stream a blob through the decrypting/CDC-aware read path into a temp file,
    /// aborting if it exceeds `max_bytes`. The returned handle deletes the file
    /// on drop. Bounded memory: chunks are written straight to disk, never
    /// buffered whole.
    async fn stream_blob_to_temp(
        dedup: &DedupService,
        blob_hash: &str,
        max_bytes: u64,
        temp_dir: &Path,
    ) -> Result<tempfile::NamedTempFile, DomainError> {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;

        let mut stream = dedup.read_blob_stream(blob_hash).await?;
        // Colocate the temp video with the data volume (sized for blobs) instead
        // of the OS temp dir, which can be small or a tmpfs in containers.
        let tmp = tempfile::Builder::new()
            .prefix("oxithumb-")
            .tempfile_in(temp_dir)
            .map_err(|e| DomainError::internal_error("VideoFrame", format!("temp file: {e}")))?;
        let mut out = tokio::fs::File::create(tmp.path())
            .await
            .map_err(|e| DomainError::internal_error("VideoFrame", format!("temp open: {e}")))?;

        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                DomainError::internal_error("VideoFrame", format!("blob read: {e}"))
            })?;
            written += chunk.len() as u64;
            if written > max_bytes {
                return Err(DomainError::internal_error(
                    "VideoFrame",
                    format!("video exceeds {max_bytes}-byte thumbnail cap"),
                ));
            }
            out.write_all(&chunk).await.map_err(|e| {
                DomainError::internal_error("VideoFrame", format!("temp write: {e}"))
            })?;
        }
        out.flush()
            .await
            .map_err(|e| DomainError::internal_error("VideoFrame", format!("temp flush: {e}")))?;
        drop(out); // close our write handle before ffmpeg opens the path
        Ok(tmp)
    }

    /// Delete the per-file thumbnail artifacts for a file.
    ///
    /// Only the external (video-frame) entries are file-keyed, so only those
    /// are removed — from moka and from disk. Rendered thumbnails, in both
    /// tiers, are keyed by blob_hash and may be shared with any other file
    /// holding the same content; they are reclaimed by
    /// `delete_blob_thumbnails` when the blob itself is garbage-collected.
    ///
    /// Content keying is also why this no longer has to win a race. When a
    /// file's content is replaced the rendered entries are unreachable by
    /// construction — a new hash is a new key — rather than needing explicit
    /// invalidation before the next request arrives.
    pub async fn delete_thumbnails(&self, file_id: &str) -> Result<(), ThumbnailError> {
        for size in ThumbnailSize::all() {
            // Only the external (per-file) entry needs invalidating. Rendered
            // thumbnails are keyed by content hash, so replacing a file's
            // content yields a different key and the old entry simply cannot
            // be hit again — which is the point: correctness no longer depends
            // on this call winning a race against the next request. And on
            // deletion the entry stays valid for any other file sharing that
            // content, so dropping it would only cost a re-read.
            self.cache
                .invalidate(&ThumbnailCacheKey::external(file_id, *size))
                .await;

            // Remove external (video-frame) thumbnails stored by file_id (JPEG-only)
            let ext_path = self
                .thumbnails_root
                .join(size.dir_name())
                .join(format!("ext-{}.jpg", file_id));
            if fs::metadata(&ext_path).await.is_ok() {
                let _ = fs::remove_file(&ext_path).await;
            }
        }

        tracing::debug!("🗑️ Invalidated thumbnail cache for: {}", file_id);
        Ok(())
    }

    /// Remove orphaned blob-hash thumbnails whose blob no longer exists.
    ///
    /// Call during blob garbage collection: pass the hash of the blob
    /// being deleted and the corresponding thumbnails are removed from disk.
    pub async fn delete_blob_thumbnails(&self, blob_hash: &str) {
        for size in ThumbnailSize::all() {
            // Delete both the primary WebP and any lazily-materialized JPEG.
            for format in [ThumbnailFormat::Webp, ThumbnailFormat::Jpeg] {
                let path = self.get_thumbnail_path(blob_hash, *size, format);
                if fs::metadata(&path).await.is_ok() {
                    let _ = fs::remove_file(&path).await;
                }
            }
        }
        tracing::debug!(
            "🗑️ Deleted blob thumbnails for hash: {}…",
            &blob_hash[..blob_hash.len().min(12)]
        );
    }

    /// Get cache statistics
    pub async fn get_stats(&self) -> ThumbnailStats {
        ThumbnailStats {
            cached_thumbnails: self.cache.entry_count() as usize,
            cache_size_bytes: self.cache.weighted_size() as usize,
            max_cache_bytes: self.max_cache_bytes as usize,
        }
    }
}

// ─── FileLifecycleHook + BlobLifecycleHook ───────────────────────────────────

/// Wires all thumbnail side-effects into the file and blob lifecycle.
///
/// Registered once on both [`FileLifecycleService`] and [`BlobLifecycleService`]
/// during DI. Handles thumbnail generation, invalidation, and cleanup.
pub struct ThumbnailRefreshHook {
    thumbnail: Arc<ThumbnailService>,
    dedup: Arc<DedupService>,
    /// Video frame extractor (ffmpeg, or a no-op when unavailable/disabled).
    video_frame: Arc<dyn VideoFramePort>,
    /// Max bytes streamed to a temp file for video frame extraction; larger
    /// videos are skipped (no thumbnail) rather than materialised in full.
    video_max_bytes: u64,
}

impl ThumbnailRefreshHook {
    pub fn new(
        thumbnail: Arc<ThumbnailService>,
        dedup: Arc<DedupService>,
        video_frame: Arc<dyn VideoFramePort>,
        video_max_bytes: u64,
    ) -> Self {
        Self {
            thumbnail,
            dedup,
            video_frame,
            video_max_bytes,
        }
    }
}

impl crate::application::ports::file_lifecycle::FileLifecycleHook for ThumbnailRefreshHook {
    fn on_file_created(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        is_new_blob: bool,
    ) {
        // Blob-hash thumbnail already exists on disk when is_new_blob=false — skip.
        if !is_new_blob {
            return;
        }
        if ThumbnailService::is_supported_image(content_type) {
            self.thumbnail
                .clone()
                .generate_all_sizes_background_from_blob(
                    file_id.to_string(),
                    blob_hash.to_string(),
                    self.dedup.clone(),
                );
        } else if self.video_frame.is_supported_video(content_type) {
            // Videos: extract a frame server-side (ffmpeg) and run it through the
            // same WebP/blob-hash pipeline — eager, off the request path.
            self.thumbnail.clone().generate_video_thumbnails_background(
                file_id.to_string(),
                blob_hash.to_string(),
                self.dedup.clone(),
                self.video_frame.clone(),
                self.video_max_bytes,
            );
        }
    }

    fn on_file_copied(
        &self,
        _file_id: &str,
        _blob_hash: &str,
        _content_type: &str,
        _source_file_id: &str,
    ) {
        // Thumbnails are keyed by blob_hash on disk — the copy shares them automatically.
    }

    fn on_file_updated(&self, file_id: &str, blob_hash: &str, content_type: &str) {
        if !ThumbnailService::is_supported_image(content_type) {
            return;
        }
        let thumbnail = self.thumbnail.clone();
        let file_id = file_id.to_string();
        let blob_hash = blob_hash.to_string();
        let dedup = self.dedup.clone();
        tokio::spawn(async move {
            if let Err(e) = thumbnail.delete_thumbnails(&file_id).await {
                tracing::warn!(
                    "Failed to invalidate thumbnail cache for {}: {}",
                    file_id,
                    e
                );
            }
            thumbnail.generate_all_sizes_background_from_blob(file_id, blob_hash, dedup);
        });
    }

    fn on_file_deleted(&self, file_id: &str) {
        let thumbnail = self.thumbnail.clone();
        let file_id = file_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = thumbnail.delete_thumbnails(&file_id).await {
                tracing::warn!("Failed to delete thumbnails for file {}: {}", file_id, e);
            }
        });
    }
}

// BlobLifecycleHook is implemented on ThumbnailService (not ThumbnailRefreshHook)
// to avoid a circular Arc: DedupService→BlobLifecycleService→ThumbnailRefreshHook→DedupService.
// ThumbnailService does not hold DedupService so no cycle exists.

// ─── BlobLifecycleHook ───────────────────────────────────────────────────────

impl crate::application::ports::blob_lifecycle::BlobLifecycleHook for ThumbnailService {
    fn on_blob_created(&self, _blob_hash: &str, _content_type: Option<&str>) {
        // Thumbnail generation is driven by file-level events (on_file_created).
    }

    fn on_blob_deleted(&self, blob_hash: &str) {
        // delete_blob_thumbnails only needs thumbnails_root — capture it to avoid Arc cycle.
        let root = self.thumbnails_root.clone();
        let blob_hash = blob_hash.to_string();
        tokio::spawn(async move {
            for size in ThumbnailSize::all() {
                // Delete both the primary WebP and any lazy JPEG fallback.
                for format in [ThumbnailFormat::Webp, ThumbnailFormat::Jpeg] {
                    let path =
                        root.join(size.dir_name())
                            .join(format!("{}.{}", blob_hash, format.ext()));
                    if tokio::fs::metadata(&path).await.is_ok() {
                        let _ = tokio::fs::remove_file(&path).await;
                    }
                }
            }
            tracing::debug!(
                "🗑️ Deleted blob thumbnails for hash: {}…",
                &blob_hash[..blob_hash.len().min(12)]
            );
        });
    }
}

// ─── Port implementation ─────────────────────────────────────────────────────

/// Convert port ThumbnailSize to infra ThumbnailSize.
impl From<PortThumbnailSize> for ThumbnailSize {
    fn from(size: PortThumbnailSize) -> Self {
        match size {
            PortThumbnailSize::Icon => ThumbnailSize::Icon,
            PortThumbnailSize::Preview => ThumbnailSize::Preview,
            PortThumbnailSize::Large => ThumbnailSize::Large,
        }
    }
}

impl ThumbnailPort for ThumbnailService {
    fn is_supported_image(&self, mime_type: &str) -> bool {
        ThumbnailService::is_supported_image(mime_type)
    }

    async fn get_thumbnail(
        &self,
        file_id: &str,
        blob_hash: &str,
        size: PortThumbnailSize,
        original_path: &Path,
    ) -> Result<Bytes, DomainError> {
        // Abstract port lookup defaults to the primary (WebP) format. The REST
        // handler uses the concrete service with Accept-derived format instead.
        self.get_thumbnail(
            file_id,
            blob_hash,
            size.into(),
            ThumbnailFormat::Webp,
            original_path,
        )
        .await
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "Thumbnail", e.to_string()))
    }

    fn generate_all_sizes_background(
        self: Arc<Self>,
        file_id: String,
        blob_hash: String,
        original_path: PathBuf,
    ) {
        ThumbnailService::generate_all_sizes_background(self, file_id, blob_hash, original_path)
    }

    async fn delete_thumbnails(&self, file_id: &str) -> Result<(), DomainError> {
        self.delete_thumbnails(file_id)
            .await
            .map_err(|e| DomainError::new(ErrorKind::InternalError, "Thumbnail", e.to_string()))
    }

    async fn get_cached_thumbnail(
        &self,
        file_id: &str,
        blob_hash: Option<&str>,
        size: PortThumbnailSize,
    ) -> Option<Bytes> {
        // `None` — the abstract port has no DedupService handle, so it stays
        // sidecar-only. Callers wanting the tier-3 fallback use the concrete
        // method, which both handlers already do.
        self.get_cached_thumbnail(file_id, blob_hash, size.into(), ThumbnailFormat::Webp, None)
            .await
    }

    async fn store_external_thumbnail(
        &self,
        file_id: &str,
        size: PortThumbnailSize,
        data: Bytes,
    ) -> Result<Bytes, DomainError> {
        self.store_external_thumbnail(file_id, size.into(), data)
            .await
            .map_err(|e| DomainError::new(ErrorKind::InternalError, "Thumbnail", e.to_string()))
    }

    async fn get_stats(&self) -> ThumbnailStatsDto {
        let stats = self.get_stats().await;
        ThumbnailStatsDto {
            cached_thumbnails: stats.cached_thumbnails,
            cache_size_bytes: stats.cache_size_bytes,
            max_cache_bytes: stats.max_cache_bytes,
        }
    }
}

/// Benchmark-only public surface (Phase 0 perf harness).
///
/// The real render functions are private (`render_thumbnail_from_data` /
/// `render_all_thumbnails_from_data`). Benches and examples are separate crate
/// targets and can only see `pub` items, so these thin wrappers — gated behind
/// the `bench` feature so they never exist in production builds — expose the
/// exact CPU-bound work (decode → orientation → resize → JPEG encode) for
/// before/after measurement. Errors are flattened to `String` to avoid leaking
/// `ThumbnailError` into the public API.
#[cfg(feature = "bench")]
impl ThumbnailService {
    /// Render a single thumbnail size as JPEG (back-compat shim for existing benches).
    pub fn bench_render_thumbnail(data: &[u8], size: ThumbnailSize) -> Result<Vec<u8>, String> {
        Self::bench_render_thumbnail_fmt(data, size, ThumbnailFormat::Jpeg)
    }

    /// Render all sizes in one decode as JPEG, returning each size's byte length.
    pub fn bench_render_all(data: &[u8]) -> Result<Vec<(ThumbnailSize, usize)>, String> {
        Self::bench_render_all_fmt(data, ThumbnailFormat::Jpeg)
    }

    /// Render a single thumbnail size in `format`, returning the encoded bytes
    /// (for the codec comparison: JPEG vs WebP bytes + SSIM-vs-source).
    pub fn bench_render_thumbnail_fmt(
        data: &[u8],
        size: ThumbnailSize,
        format: ThumbnailFormat,
    ) -> Result<Vec<u8>, String> {
        Self::render_thumbnail_from_data(data, size, format).map_err(|e| e.to_string())
    }

    /// Render all sizes in one decode in `format`, returning each size's byte length.
    pub fn bench_render_all_fmt(
        data: &[u8],
        format: ThumbnailFormat,
    ) -> Result<Vec<(ThumbnailSize, usize)>, String> {
        Self::render_all_thumbnails_from_data(data, format)
            .map(|v| v.into_iter().map(|(s, b)| (s, b.len())).collect())
            .map_err(|e| e.to_string())
    }

    /// Render a thumbnail to WebP at an explicit quality — used by the codec
    /// benchmark to sweep WebP quality and find the one matching JPEG q80 SSIM.
    pub fn bench_render_webp_at(
        data: &[u8],
        size: ThumbnailSize,
        quality: f32,
    ) -> Result<Vec<u8>, String> {
        use fast_image_resize::images::{Image, ImageRef};
        use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
        let rgb = Self::decode_oriented(data, size.max_dimension())
            .map_err(|e| e.to_string())?
            .into_rgb8();
        let (sw, sh) = (rgb.width(), rgb.height());
        let (dw, dh) = Self::fit_dims(sw, sh, size.max_dimension());
        let filter = if dw > sw || dh > sh {
            FilterType::CatmullRom
        } else {
            Self::filter_for(size)
        };
        let src =
            ImageRef::new(sw, sh, rgb.as_raw(), PixelType::U8x3).map_err(|e| e.to_string())?;
        let mut dst = Image::new(dw, dh, PixelType::U8x3);
        Resizer::new()
            .resize(
                &src,
                &mut dst,
                &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter)),
            )
            .map_err(|e| e.to_string())?;
        Ok(webp::Encoder::from_rgb(&dst.into_vec(), dw, dh)
            .encode(quality)
            .to_vec())
    }
}

/// Thumbnail service errors
#[derive(Debug, thiserror::Error)]
pub enum ThumbnailError {
    #[error("IO error: {0}")]
    IoError(String),

    #[error("Image processing error: {0}")]
    ImageError(String),

    #[error("Task error: {0}")]
    TaskError(String),

    #[error("Unsupported image format")]
    UnsupportedFormat,
}

/// Statistics about the thumbnail cache
#[derive(Debug, Clone)]
pub struct ThumbnailStats {
    pub cached_thumbnails: usize,
    pub cache_size_bytes: usize,
    pub max_cache_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

    // A 1×1 transparent RGBA PNG — same fixture the E2E seed uploads.
    // Verifies that `fit_dims` never upsamples and `encode_thumbnail`
    // skips the convolution kernel for src==dst, both of which would
    // otherwise surface as `ImageError` from every downstream size/
    // format pair. Note: this base64 must be a valid PNG (correct chunk
    // CRCs) — the `image` crate rejects malformed CRCs even where
    // ImageMagick's `identify` is lenient. Regenerate with:
    //   python3 -c "…" | pbcopy  # see tests/e2e/spa/helpers.ts for the snippet
    const PIXEL_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

    #[test]
    fn render_thumbnail_handles_1x1_source_for_every_size_and_format() {
        let png = B64.decode(PIXEL_PNG_B64).unwrap();
        for &size in ThumbnailSize::all() {
            for &format in &[ThumbnailFormat::Jpeg, ThumbnailFormat::Webp] {
                let out = ThumbnailService::render_thumbnail_from_data(&png, size, format)
                    .unwrap_or_else(|e| {
                        panic!("render_thumbnail_from_data({size:?}, {format:?}) failed: {e}")
                    });
                assert!(!out.is_empty(), "empty output for {size:?} {format:?}");
            }
        }
    }
}
