//! Image Transcoding Service - WebP On-Demand Conversion
//!
//! Automatically transcodes images to WebP format when the browser supports it,
//! reducing bandwidth by 30-50% compared to JPEG/PNG.
//!
//! Architecture:
//! - **Dedicated `rayon` thread pool** for CPU-bound transcoding (never blocks Tokio)
//! - **`moka` lock-free cache** for hot transcoded images (no write-lock on reads)
//! - Disk cache for persistence across restarts
//! - Supports PNG, GIF → WebP conversion (JPEG excluded — the encoder is
//!   lossless-only, so photos would come out larger; see `can_transcode`)
//! - Falls back to original if conversion fails or result is larger, and
//!   remembers that negative verdict (memory sentinel + disk marker) so the
//!   decode + encode is never repeated for the same file

use bytes::Bytes;
use image::ImageFormat;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::fs;

use crate::application::ports::transcode_ports::{
    ImageTranscodePort, OutputFormat as PortOutputFormat, TranscodeStatsDto,
};
use crate::domain::errors::{DomainError, ErrorKind};

/// Maximum file size for transcoding (5MB - larger files stream directly)
pub const MAX_TRANSCODE_SIZE: u64 = 5 * 1024 * 1024;

/// Minimum number of threads in the dedicated transcoding pool
const MIN_TRANSCODE_THREADS: usize = 2;

/// Compute the number of transcoding threads: half the available CPUs,
/// with a floor of `MIN_TRANSCODE_THREADS`.  `available_parallelism()`
/// respects cgroup limits (Docker/K8s) and CPU affinity masks.
fn transcode_thread_count() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(MIN_TRANSCODE_THREADS);
    (cpus / 2).max(MIN_TRANSCODE_THREADS)
}

/// Dedicated rayon thread pool for CPU-bound image transcoding.
/// Isolated from Tokio's blocking pool to prevent starvation of other I/O.
/// Thread count scales with available CPUs (half cores, min 2).
fn transcode_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = transcode_thread_count();
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|idx| format!("transcode-{idx}"))
            .build()
            .expect("Failed to create transcode thread pool")
    })
}

/// Supported output formats
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    WebP,
    // Future: AVIF, JPEG-XL
}

impl OutputFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::WebP => "webp",
        }
    }

    pub fn mime_type(&self) -> &'static str {
        match self {
            OutputFormat::WebP => "image/webp",
        }
    }
}

/// Result of checking browser support
#[derive(Debug)]
pub struct BrowserCapabilities {
    pub supports_webp: bool,
    pub supports_avif: bool,
}

impl BrowserCapabilities {
    /// Parse Accept header to determine browser image format support
    pub fn from_accept_header(accept: Option<&str>) -> Self {
        let accept = accept.unwrap_or("");
        Self {
            supports_webp: accept.contains("image/webp"),
            supports_avif: accept.contains("image/avif"),
        }
    }

    /// Get the best output format for this browser
    pub fn best_format(&self) -> Option<OutputFormat> {
        if self.supports_webp {
            Some(OutputFormat::WebP)
        } else {
            None
        }
    }
}

/// Lock-free transcoding statistics using atomics (no RwLock needed)
#[derive(Debug, Default)]
struct AtomicTranscodeStats {
    cache_hits: AtomicU64,
    disk_hits: AtomicU64,
    transcodes: AtomicU64,
    bytes_saved: AtomicU64,
    transcode_errors: AtomicU64,
    /// Decodes + encodes that produced something LARGER than the original.
    ///
    /// Counted separately because `transcodes` means "work that paid off"
    /// — it is incremented only on the success path, alongside
    /// `bytes_saved`. Without this counter the most expensive failure mode
    /// is invisible: the full decode and re-encode of a multi-megapixel
    /// image, repeated for every file sharing that content, producing
    /// nothing. That is precisely the cost the persisted negative verdict
    /// exists to eliminate, so it needs to be measurable before and after.
    not_beneficial: AtomicU64,
}

/// Snapshot of transcoding statistics
#[derive(Debug, Default, Clone)]
pub struct TranscodeStats {
    pub cache_hits: u64,
    pub disk_hits: u64,
    pub transcodes: u64,
    pub bytes_saved: u64,
    pub transcode_errors: u64,
    pub not_beneficial: u64,
}

impl AtomicTranscodeStats {
    fn snapshot(&self) -> TranscodeStats {
        TranscodeStats {
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            disk_hits: self.disk_hits.load(Ordering::Relaxed),
            transcodes: self.transcodes.load(Ordering::Relaxed),
            bytes_saved: self.bytes_saved.load(Ordering::Relaxed),
            transcode_errors: self.transcode_errors.load(Ordering::Relaxed),
            not_beneficial: self.not_beneficial.load(Ordering::Relaxed),
        }
    }
}

/// Image Transcoding Service
///
/// Uses a dedicated `rayon` thread pool for CPU-bound work and `moka` for
/// lock-free concurrent caching with automatic weight-based eviction.
pub struct ImageTranscodeService {
    /// Cache directory for transcoded images on disk
    cache_dir: PathBuf,
    /// Lock-free concurrent cache (moka) — no write-lock on reads
    memory_cache: moka::future::Cache<String, Bytes>,
    /// Lock-free statistics
    stats: Arc<AtomicTranscodeStats>,
    /// The derived tier, attached after construction.
    ///
    /// A constructor parameter would be cleaner but does not fit: DI builds
    /// this service before `DedupService` exists, and reordering is worse
    /// than a one-shot — the transcode service is needed by the retrieval
    /// path, which is wired early. `ThumbnailService` met the same wall and
    /// took a per-call parameter instead; that does not work here because
    /// the caller (`FileRetrievalService`) holds no dedup handle either, so
    /// threading one through would push the dependency into a service that
    /// has no other use for it.
    ///
    /// `OnceLock` rather than a `Mutex`: set exactly once at boot, read on
    /// every request, never replaced.
    dedup: OnceLock<Arc<crate::infrastructure::services::dedup_service::DedupService>>,
    /// Whether `.transcoded/` still exists, probed once by
    /// [`Self::initialize`]. `false` short-circuits the local-cache reads
    /// without a syscall.
    ///
    /// Starts `true` so a service constructed without `initialize` (tests)
    /// behaves as before. Failing open is the safe direction: the wrong
    /// value costs syscalls, the opposite would hide cached entries that
    /// are still there.
    legacy_cache: AtomicBool,
}

impl ImageTranscodeService {
    /// Create new transcoding service
    ///
    /// - `storage_root`: base path for disk cache
    /// - `max_cache_entries`: max number of transcoded images in memory
    /// - `max_memory_bytes`: max total bytes for in-memory cache
    pub fn new(storage_root: &Path, max_cache_entries: usize, max_memory_bytes: usize) -> Self {
        let cache_dir = storage_root.join(".transcoded");

        // Build moka cache with weight-based eviction (by content size)
        let memory_cache = moka::future::Cache::builder()
            .max_capacity(max_memory_bytes as u64)
            .weigher(|_key: &String, value: &Bytes| -> u32 {
                // Weight = byte size, capped to u32::MAX
                value.len().min(u32::MAX as usize) as u32
            })
            .time_to_live(std::time::Duration::from_secs(600)) // 10 min TTL for freshness
            .build();

        // Ignore max_cache_entries — moka uses weight-based eviction, which is
        // more accurate than entry-count limits for variable-size images.
        let _ = max_cache_entries;

        Self {
            cache_dir,
            memory_cache,
            stats: Arc::new(AtomicTranscodeStats::default()),
            dedup: OnceLock::new(),
            legacy_cache: AtomicBool::new(true),
        }
    }

    /// Attach the derived tier. Called once from DI, after `DedupService`
    /// exists. Until then — and in tests that never call it — the service
    /// behaves exactly as before, reading and writing only its local cache.
    pub fn attach_dedup(
        &self,
        dedup: Arc<crate::infrastructure::services::dedup_service::DedupService>,
    ) {
        if self.dedup.set(dedup).is_err() {
            tracing::warn!(
                target: "oxicloud::transcode",
                "attach_dedup called twice — the first handle is kept"
            );
        }
    }

    /// The `content_derived_blobs.kind` for everything this service writes.
    const DERIVED_KIND: &'static str = "transcode";

    /// Memory-cache key: by CONTENT when the caller supplied a hash, by
    /// file id only when it could not.
    ///
    /// Transcoding is a pure function of the source bytes, so file keying
    /// was always the wrong axis for this cache — it just predated the
    /// content-keyed tier. Two files with identical content held two
    /// entries for identical bytes, and the second file missed RAM and
    /// paid a DB lookup plus a blob read to fetch what was already in
    /// memory under another key.
    ///
    /// The `c:` / `f:` prefixes keep the two namespaces disjoint. A
    /// 64-hex hash and a UUID cannot collide in practice, but relying on
    /// "in practice" for a cache key is how a file ends up served another
    /// file's bytes.
    ///
    /// Same shape as `ThumbnailCacheKey`'s `content` / `external` split,
    /// for the same reason: hash-less callers (external mounts) have no
    /// content identity to key on, so they keep the per-file entry.
    fn cache_key(source_hash: Option<&str>, file_id: &str, format: OutputFormat) -> String {
        match source_hash {
            Some(hash) => format!("c:{}:{}", hash, format.extension()),
            None => format!("f:{}:{}", file_id, format.extension()),
        }
    }

    /// Initialize the service.
    ///
    /// Still creates the local cache directories, because this service DOES
    /// still write them — unlike `ThumbnailService`, whose sidecar writes
    /// are gone. When the transcode write path moves fully to the derived
    /// tier, these two `create_dir_all` calls have to go at the same time:
    /// leaving them would recreate the tree on every boot and make the
    /// absence that `transcode_import` works toward unreachable, which is
    /// exactly the bug that kept `.thumbnails/` alive across restarts.
    pub async fn initialize(&self) -> std::io::Result<()> {
        // Probes, does NOT create.
        //
        // Creating the tree at boot is what kept `.thumbnails/` alive across
        // restarts: the import removed it, the next boot put it back, and
        // the absence the read path gates on was unreachable by
        // construction. The write path below already calls `create_dir_all`
        // on the parent before writing, so nothing needs it created eagerly
        // — the only thing eager creation achieved was defeating the drain.
        //
        // One `stat` on the root, cached for the process lifetime. It can
        // only be stale in the harmless direction: a drain completing
        // mid-life leaves the flag true until restart, costing the same
        // failed opens as before. It never goes false while entries remain,
        // because only `transcode_import` removes the tree and it removes
        // the whole thing at once.
        let present = fs::metadata(&self.cache_dir).await.is_ok();
        self.legacy_cache.store(present, Ordering::Relaxed);

        tracing::info!(
            "🖼️ Image transcode service initialized (rayon pool: {} threads)",
            transcode_thread_count(),
        );
        if present {
            tracing::info!(
                target: "oxicloud::transcode",
                event = "transcode.legacy_cache_present",
                path = ?self.cache_dir,
                "legacy transcode cache present — reads fall back to it. Run \
                 transcode_import with ?repair=true to drain it."
            );
        }
        Ok(())
    }

    /// Whether the legacy local cache is worth touching.
    ///
    /// Unlike the thumbnail tiers this may legitimately never reach `false`:
    /// callers with no content hash (external mounts) cannot use the
    /// content-keyed tier at all, so they still read and write here. On an
    /// install without such mounts the directory drains once and stays
    /// gone; on one with them it persists, and that is correct rather than
    /// a stalled migration.
    fn legacy_cache_active(&self) -> bool {
        self.legacy_cache.load(Ordering::Relaxed)
    }

    /// Check if a mime type can be transcoded.
    ///
    /// JPEG is deliberately excluded: the `image` crate's WebP encoder is
    /// lossless-only, and losslessly re-encoding an already-lossy photo
    /// almost always produces a LARGER file — so every JPEG download paid
    /// a full decode + encode (hundreds of ms of CPU) only to discard the
    /// result. PNG/GIF → lossless WebP genuinely shrinks. Re-add JPEG only
    /// together with a lossy WebP encoder.
    pub fn can_transcode(mime_type: &str) -> bool {
        matches!(mime_type, "image/png" | "image/gif")
    }

    /// Check if transcoding should be attempted based on file size and type
    pub fn should_transcode(mime_type: &str, file_size: u64) -> bool {
        Self::can_transcode(mime_type) && file_size <= MAX_TRANSCODE_SIZE
    }

    /// Get transcoded version of an image.
    /// Returns `(content, mime_type, was_transcoded)`.
    ///
    /// Accepts `Bytes` (ref-counted) so callers avoid copying the buffer.
    /// Cloning `Bytes` is O(1) — only an atomic increment.
    ///
    /// `source_hash` is the BLAKE3 of the ORIGINAL content — the key the
    /// derived tier uses. `None` falls back to the local cache alone, which
    /// is what happens for callers that have no hash (external mounts) and
    /// what the whole service did before the derived tier existed.
    ///
    /// It is a parameter rather than something computed here on purpose:
    /// hashing `original_content` per request would be a BLAKE3 over the
    /// whole file on every GET, and the caller already has the value.
    pub async fn get_transcoded(
        &self,
        file_id: &str,
        source_hash: Option<&str>,
        original_content: Bytes,
        original_mime: &str,
        target_format: OutputFormat,
    ) -> Result<(Bytes, String, bool), String> {
        let cache_key = Self::cache_key(source_hash, file_id, target_format);

        // ── 1. Fast path: moka memory cache (lock-free read) ──
        // An empty-Bytes entry is the negative sentinel: "transcoding this
        // file is not beneficial — serve the original". Without it, every
        // GET of such an image repeated the full decode + encode just to
        // discard the result again.
        if let Some(cached) = self.memory_cache.get(&cache_key).await {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            if cached.is_empty() {
                tracing::debug!("🔥 Transcode negative cache HIT: {}", file_id);
                return Ok((original_content, original_mime.to_string(), false));
            }
            tracing::debug!("🔥 Transcode memory cache HIT: {}", file_id);
            return Ok((cached, target_format.mime_type().to_string(), true));
        }

        // ── 2. Slow path: single-flight coalescing ──
        // A viral image requested as WebP by N clients at once would otherwise
        // run N identical disk reads + CPU transcodes, saturating the rayon
        // pool and inflating tail latency. `try_get_with` collapses every
        // concurrent miss for this key into ONE `compute_transcode`; the other
        // callers await its result. The cached value (transcoded bytes, or the
        // empty negative sentinel) is what gets stored.
        let original_for_loader = original_content.clone(); // O(1) ref-count bump
        let cached = self
            .memory_cache
            .try_get_with(cache_key, async {
                self.compute_transcode(
                    file_id,
                    source_hash,
                    original_for_loader,
                    original_mime,
                    target_format,
                )
                .await
            })
            .await
            // try_get_with shares one `Arc<String>` across waiters; DomainError
            // here is just a String, so hand callers an owned clone.
            .map_err(|shared: Arc<String>| (*shared).clone())?;

        if cached.is_empty() {
            Ok((original_content, original_mime.to_string(), false))
        } else {
            Ok((cached, target_format.mime_type().to_string(), true))
        }
    }

    /// Compute the value to cache for `(file_id, target_format)`: either the
    /// transcoded WebP bytes, or an **empty `Bytes` negative sentinel** meaning
    /// "the result wasn't smaller — serve the original". Runs the disk-cache
    /// lookups and the CPU transcode, and is invoked at most once per key,
    /// guarded by [`Self::get_transcoded`]'s `try_get_with` single-flight.
    async fn compute_transcode(
        &self,
        file_id: &str,
        source_hash: Option<&str>,
        original_content: Bytes,
        original_mime: &str,
        target_format: OutputFormat,
    ) -> Result<Bytes, String> {
        // ── Derived tier, ahead of the local cache ──
        //
        // Content-keyed, so it is shared across every file with these bytes
        // and survives both a restart and a backend migration — neither of
        // which the local `.transcoded/` tree does. Read first for the same
        // reason the thumbnail read-order flip put it first: the local tree
        // is the legacy tier being drained, and a fallback that is consulted
        // first never stops being load-bearing.
        let derived = match (source_hash, self.dedup.get()) {
            (Some(hash), Some(dedup)) => Some((hash, dedup)),
            _ => None,
        };
        if let Some((hash, dedup)) = derived {
            use crate::application::ports::dedup_ports::DerivedLookup;
            match dedup
                .lookup_derived(hash, Self::DERIVED_KIND, target_format.extension())
                .await
            {
                DerivedLookup::Found(r) => match dedup.read_blob_bytes(&r.blob_hash).await {
                    Ok(bytes) if !bytes.is_empty() => {
                        self.stats.disk_hits.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!("🧱 Transcode derived tier HIT: {}", file_id);
                        return Ok(bytes);
                    }
                    // The row promised bytes that are gone or empty. Fall
                    // through and re-derive rather than serving nothing —
                    // a transcode is a pure function of its source, so this
                    // is recoverable by construction. `satellites_consistency`
                    // reports the dangling row separately.
                    _ => tracing::warn!(
                        target: "oxicloud::transcode",
                        source_hash = %hash,
                        blob_hash = %r.blob_hash,
                        "derived transcode row points at unreadable bytes; re-deriving"
                    ),
                },
                // Known not worth transcoding for this content. This is the
                // whole point of persisting the verdict: without it every GET
                // repeats a full decode + encode to throw the result away.
                DerivedLookup::NotDerivable => {
                    self.stats.disk_hits.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!("🧱 Transcode negative derived row HIT: {}", file_id);
                    return Ok(Bytes::new());
                }
                DerivedLookup::Missing => {}
            }
        }

        // ── Legacy local cache (async fs) ──
        //
        // Drained by `transcode_import`; kept as a fallback until it is gone.
        let cache_path = self.get_cache_path(file_id, target_format);
        if self.legacy_cache_active() && tokio::fs::try_exists(&cache_path).await.unwrap_or(false) {
            match fs::read(&cache_path).await {
                Ok(data) => {
                    self.stats.disk_hits.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!("💾 Transcode disk cache HIT: {}", file_id);
                    return Ok(Bytes::from(data));
                }
                Err(e) => {
                    tracing::warn!("Failed to read cached transcode: {}", e);
                }
            }
        }

        // ── Negative verdict persisted on disk (survives restarts) ──
        let skip_marker = self.get_skip_marker_path(file_id, target_format);
        if self.legacy_cache_active() && tokio::fs::try_exists(&skip_marker).await.unwrap_or(false)
        {
            self.stats.disk_hits.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("💾 Transcode negative disk marker HIT: {}", file_id);
            return Ok(Bytes::new());
        }

        // ── Transcode on dedicated rayon pool (never blocks Tokio) ──
        let content_for_rayon = original_content.clone(); // O(1) ref-count bump
        let mime_owned = original_mime.to_string();

        let (tx, rx) = tokio::sync::oneshot::channel();

        transcode_pool().spawn(move || {
            let result = transcode_image_blocking(&content_for_rayon, &mime_owned, target_format);
            let _ = tx.send(result);
        });

        let transcoded = rx
            .await
            .map_err(|_| "Transcode task was cancelled".to_string())??;

        let transcoded_bytes = Bytes::from(transcoded);

        // ── Evaluate savings ──
        let original_size = original_content.len();
        let transcoded_size = transcoded_bytes.len();

        if transcoded_size >= original_size {
            // Counted here, not with `transcodes` — the work happened but
            // paid nothing, and conflating the two would hide the cost this
            // whole negative-verdict mechanism exists to stop paying.
            self.stats.not_beneficial.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                "⚠️ Transcode not beneficial for {}: {} -> {} bytes",
                file_id,
                original_size,
                transcoded_size
            );
            // Remember the verdict so the next GET does not repeat the decode
            // + encode. The caller caches the empty-Bytes sentinel in RAM
            // (10 min TTL); this row is what makes it survive eviction, a
            // restart, and a move to another instance.
            //
            // Safe to persist because it is deterministic in the CONTENT:
            // these exact bytes will always re-encode larger. A timeout or a
            // read error would not be — those return `Err` above and are
            // deliberately not recorded, since a momentary failure written
            // here would mark a perfectly transcodable image as hopeless
            // with nothing to ever retry it.
            match derived {
                Some((hash, dedup)) => {
                    if let Err(e) = dedup
                        .store_derived_negative(hash, Self::DERIVED_KIND, target_format.extension())
                        .await
                    {
                        tracing::warn!(
                            target: "oxicloud::transcode",
                            source_hash = %hash,
                            error = %e,
                            "failed to persist negative transcode verdict; it will be recomputed"
                        );
                    }
                }
                // Hash-less callers still get the zero-byte marker, for the
                // same reason they still get the local cache write: the
                // content-keyed tier cannot hold a verdict for content it
                // cannot name. Dropping this would make every external-mount
                // GET of a non-shrinking image re-decode once moka's TTL
                // expires.
                None => {
                    let marker = self.get_skip_marker_path(file_id, target_format);
                    tokio::spawn(async move {
                        if let Some(parent) = marker.parent() {
                            let _ = fs::create_dir_all(parent).await;
                        }
                        if let Err(e) = fs::write(&marker, b"").await {
                            tracing::warn!("Failed to persist transcode skip marker: {}", e);
                        }
                    });
                }
            }
            return Ok(Bytes::new());
        }

        let saved = original_size - transcoded_size;

        // ── Persist ──
        //
        // Derived tier when we have a source hash, local cache otherwise.
        // Not both: writing the sidecar too would mean `transcode_import`
        // chases a tail that keeps being refilled, which is the trap the
        // thumbnail migration hit — four render paths wrote the sidecar and
        // one wrote the row, so the tail never emptied.
        //
        // The local write survives only for hash-less callers (external
        // mounts), which the derived tier cannot serve at all. When those
        // gain a hash this branch goes, and `initialize`'s `create_dir_all`
        // calls go with it.
        match derived {
            Some((hash, dedup)) => {
                let dedup = dedup.clone();
                let hash = hash.to_string();
                let variant = target_format.extension().to_string();
                let mime = target_format.mime_type().to_string();
                let bytes = transcoded_bytes.clone();
                // Awaited, NOT spawned.
                //
                // Fire-and-forget looked free — the bytes are already on
                // their way to the client — but it raced its own purpose. A
                // second request for the SAME content arriving before the
                // spawn lands finds no row, re-runs the whole decode +
                // encode, and stores the identical blob again. The point of
                // keying by content is that identical content is derived
                // once; a write that has not landed yet cannot deliver
                // that, and the window is milliseconds wide precisely when
                // it matters most (a page loading many images at once).
                //
                // Caught by `transcode_cache.hurl`, which asserts the second
                // distinct file with identical bytes does not re-transcode
                // — it had been passing on timing luck.
                //
                // The cost is bounded: this path has just spent a full
                // decode and re-encode, so one blob write is marginal
                // beside it, and it only runs on a genuine miss.
                if let Err(e) = dedup
                    .store_derived_blob(&hash, Self::DERIVED_KIND, &variant, &mime, bytes)
                    .await
                {
                    tracing::warn!(
                        target: "oxicloud::transcode",
                        source_hash = %hash,
                        error = %e,
                        "failed to store derived transcode; it will be recomputed"
                    );
                }
            }
            None => {
                let cache_path_clone = cache_path.clone();
                let transcoded_for_disk = transcoded_bytes.clone();
                tokio::spawn(async move {
                    if let Some(parent) = cache_path_clone.parent() {
                        let _ = fs::create_dir_all(parent).await;
                    }
                    if let Err(e) = fs::write(&cache_path_clone, &transcoded_for_disk).await {
                        tracing::warn!("Failed to cache transcoded image: {}", e);
                    }
                });
            }
        }

        // ── Update stats (lock-free atomics) ──
        self.stats.transcodes.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_saved
            .fetch_add(saved as u64, Ordering::Relaxed);

        tracing::info!(
            "✨ Transcoded {}: {} -> {} bytes ({:.1}% smaller)",
            file_id,
            original_size,
            transcoded_size,
            (1.0 - transcoded_size as f64 / original_size as f64) * 100.0
        );

        Ok(transcoded_bytes)
    }

    /// Get path for cached transcoded file
    fn get_cache_path(&self, file_id: &str, format: OutputFormat) -> PathBuf {
        self.cache_dir
            .join(format.extension())
            .join(format!("{}.{}", file_id, format.extension()))
    }

    /// Path of the zero-byte marker recording a negative transcode verdict
    /// ("result was not smaller — serve the original").
    fn get_skip_marker_path(&self, file_id: &str, format: OutputFormat) -> PathBuf {
        self.cache_dir.join(format.extension()).join(format!(
            "{}.{}.skip",
            file_id,
            format.extension()
        ))
    }

    /// Invalidate cached transcodes for a file
    pub async fn invalidate(&self, file_id: &str) {
        // Only the FILE-keyed entry, deliberately.
        //
        // Content-keyed entries must not be dropped here: this file's
        // content changing says nothing about the other files sharing the
        // old bytes, and evicting theirs would make one user's edit cost
        // everyone else a re-transcode. They need no eviction anyway —
        // new content is a new hash, so the old key is simply never
        // consulted again, and moka's TTL reclaims it.
        //
        // What remains here is the fallback entry for hash-less callers,
        // plus the legacy on-disk pair, which are genuinely per-file.
        let cache_key = Self::cache_key(None, file_id, OutputFormat::WebP);
        self.memory_cache.invalidate(&cache_key).await;

        let cache_path = self.get_cache_path(file_id, OutputFormat::WebP);
        let _ = fs::remove_file(&cache_path).await;
        let skip_marker = self.get_skip_marker_path(file_id, OutputFormat::WebP);
        let _ = fs::remove_file(&skip_marker).await;
    }

    /// Get transcoding statistics
    pub async fn get_stats(&self) -> TranscodeStats {
        self.stats.snapshot()
    }

    /// Clear all caches
    pub async fn clear_cache(&self) -> std::io::Result<()> {
        self.memory_cache.invalidate_all();

        if tokio::fs::try_exists(&self.cache_dir)
            .await
            .unwrap_or(false)
        {
            fs::remove_dir_all(&self.cache_dir).await?;
            fs::create_dir_all(&self.cache_dir).await?;
            fs::create_dir_all(self.cache_dir.join("webp")).await?;
        }

        Ok(())
    }
}

// ─── CPU-bound transcoding (runs on rayon, never on Tokio) ───────────────────

#[cfg(test)]
mod fixture_premise {
    //! Pins the property `tests/api/transcode_cache.hurl` is built on: one
    //! fixture WebP shrinks, one it does not.
    //!
    //! The negative half was hard to come by and the reason is worth
    //! recording. Synthetic images do not reproduce it — flat colour goes
    //! 4780 → 186 bytes, a gradient 24852 → 102, and even uniform RGBA
    //! noise still loses by ~242 bytes at any size, a margin that is
    //! constant in absolute terms and so never flips.
    //!
    //! Two things have to be true at once, and only real content does
    //! both. The encoder here is the `image` crate's own minimal VP8L
    //! writer, not libwebp — it does none of libwebp's search over
    //! predictors, colour transforms and Huffman groups — so it only wins
    //! where redundancy is extreme enough that any encoder finds it. And
    //! the original has to be near PNG-optimal, which a screenshot from a
    //! real capture tool is: a 2× Retina UI is long identical runs, flat
    //! panels and sharp edges, exactly what PNG's scanline filters plus
    //! zlib were designed around.
    //!
    //! So the negative verdict this service persists is partly a property
    //! of THIS encoder, not of the content. Swapping in libwebp would
    //! likely flip most of these to positive and leave the stored negative
    //! rows stale — an encoder change has to purge them.

    use super::*;

    fn webp_len(path: &str) -> (usize, usize) {
        let png = std::fs::read(path).expect("fixture present");
        let webp =
            transcode_image_blocking(&Bytes::from(png.clone()), "image/png", OutputFormat::WebP)
                .expect("fixture decodes");
        (png.len(), webp.len())
    }

    /// If this ever fails, the hurl scenario's negative half has silently
    /// become a second positive test — it would still pass while checking
    /// nothing it was written to check.
    #[test]
    fn screenshot_fixture_is_a_genuine_negative() {
        let (png, webp) = webp_len("tests/fixtures/negative-cache-transcode.png");
        assert!(
            webp >= png,
            "negative-cache-transcode.png no longer defeats the WebP encoder: \
             png={png} webp={webp}"
        );
    }

    #[test]
    fn flat_colour_fixture_is_a_genuine_positive() {
        let (png, webp) = webp_len("tests/fixtures/red-image.png");
        assert!(
            webp < png,
            "red-image.png stopped shrinking: png={png} webp={webp}"
        );
    }
}

/// Perform actual image transcoding. This is a pure CPU function — safe to call
/// from `rayon::spawn` or `spawn_blocking`.
fn transcode_image_blocking(
    content: &[u8],
    original_mime: &str,
    target_format: OutputFormat,
) -> Result<Vec<u8>, String> {
    let input_format = match original_mime {
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        "image/gif" => ImageFormat::Gif,
        _ => return Err(format!("Unsupported input format: {}", original_mime)),
    };

    let img = image::load_from_memory_with_format(content, input_format)
        .map_err(|e| format!("Failed to decode image: {}", e))?;

    match target_format {
        OutputFormat::WebP => {
            let mut buffer = Vec::new();
            let mut cursor = std::io::Cursor::new(&mut buffer);
            img.write_to(&mut cursor, ImageFormat::WebP)
                .map_err(|e| format!("Failed to encode WebP: {}", e))?;
            Ok(buffer)
        }
    }
}

// ─── Port implementation ─────────────────────────────────────────────────────

/// Convert port OutputFormat to infra OutputFormat.
impl From<PortOutputFormat> for OutputFormat {
    fn from(fmt: PortOutputFormat) -> Self {
        match fmt {
            PortOutputFormat::WebP => OutputFormat::WebP,
        }
    }
}

impl ImageTranscodePort for ImageTranscodeService {
    fn can_transcode(&self, mime_type: &str) -> bool {
        ImageTranscodeService::can_transcode(mime_type)
    }

    fn should_transcode(&self, mime_type: &str, file_size: u64) -> bool {
        ImageTranscodeService::should_transcode(mime_type, file_size)
    }

    async fn get_transcoded(
        &self,
        file_id: &str,
        source_hash: Option<&str>,
        original_content: Bytes,
        original_mime: &str,
        target_format: PortOutputFormat,
    ) -> Result<(Bytes, String, bool), DomainError> {
        self.get_transcoded(
            file_id,
            source_hash,
            original_content,
            original_mime,
            target_format.into(),
        )
        .await
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "ImageTranscode", e))
    }

    async fn invalidate(&self, file_id: &str) {
        self.invalidate(file_id).await
    }

    async fn get_stats(&self) -> TranscodeStatsDto {
        let stats = self.get_stats().await;
        TranscodeStatsDto {
            cache_hits: stats.cache_hits,
            disk_hits: stats.disk_hits,
            transcodes: stats.transcodes,
            bytes_saved: stats.bytes_saved,
            transcode_errors: stats.transcode_errors,
        }
    }

    async fn clear_cache(&self) -> Result<(), DomainError> {
        self.clear_cache().await.map_err(DomainError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_browser_capabilities() {
        // Chrome/Firefox with WebP support
        let caps = BrowserCapabilities::from_accept_header(Some(
            "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
        ));
        assert!(caps.supports_webp);
        assert!(caps.supports_avif);

        // Safari without WebP (old)
        let caps = BrowserCapabilities::from_accept_header(Some(
            "image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5",
        ));
        assert!(!caps.supports_webp);

        // No header
        let caps = BrowserCapabilities::from_accept_header(None);
        assert!(!caps.supports_webp);
    }

    #[test]
    fn test_can_transcode() {
        assert!(ImageTranscodeService::can_transcode("image/png"));
        assert!(ImageTranscodeService::can_transcode("image/gif"));
        // JPEG excluded: the lossless-only WebP encoder makes photos LARGER
        assert!(!ImageTranscodeService::can_transcode("image/jpeg"));
        assert!(!ImageTranscodeService::can_transcode("image/webp"));
        assert!(!ImageTranscodeService::can_transcode("image/svg+xml"));
        assert!(!ImageTranscodeService::can_transcode("application/pdf"));
    }

    #[test]
    fn test_should_transcode() {
        // Small PNG - yes
        assert!(ImageTranscodeService::should_transcode(
            "image/png",
            1024 * 1024
        ));

        // Large PNG - no (too big)
        assert!(!ImageTranscodeService::should_transcode(
            "image/png",
            10 * 1024 * 1024
        ));

        // JPEG - no (lossless-only encoder, result would be larger)
        assert!(!ImageTranscodeService::should_transcode(
            "image/jpeg",
            1024 * 1024
        ));

        // WebP - no (already optimal)
        assert!(!ImageTranscodeService::should_transcode(
            "image/webp",
            1024 * 1024
        ));
    }

    #[test]
    fn test_transcode_pool_initializes() {
        // Verify the pool can be created without panic
        let pool = transcode_pool();
        assert_eq!(pool.current_num_threads(), transcode_thread_count());
    }
}
