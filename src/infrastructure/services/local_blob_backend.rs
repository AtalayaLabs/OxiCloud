//! Local Filesystem Blob Backend — stores blobs under `.blobs/{prefix}/{hash}.blob`.
//!
//! This is the default backend and a direct extraction of the filesystem I/O
//! that previously lived inside `DedupService`.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::fs::{self, File};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::application::ports::blob_storage_ports::{
    BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use crate::domain::errors::{DomainError, ErrorKind};

/// Fsync the directory containing `child_path` so a preceding rename
/// or create on `child_path` becomes durable across power loss.
///
/// On Linux this issues `fsync(2)` on the directory file descriptor —
/// the canonical "make the dirent change durable" idiom. macOS does
/// the same but only persists to the disk controller (true persistence
/// would need `fcntl(F_FULLFSYNC)`, which tokio doesn't expose). On
/// Windows, opening a directory needs `FILE_FLAG_BACKUP_SEMANTICS` that
/// tokio's `File::open` doesn't set; that platform falls through to
/// `Ok(())` after a debug log.
///
/// Best-effort by design: a failure here is logged but does NOT fail
/// the upload, because the blob file itself was just `sync_all`'d and
/// is durable on its own. Worst case post-crash recovery: a rename
/// "reverts" to the un-renamed name (or stays renamed); the dedup-GC
/// cleanup pass handles either side.
async fn fsync_parent_dir(child_path: &Path) {
    let Some(parent) = child_path.parent() else {
        return;
    };
    let parent = parent.to_owned();
    // std::fs (synchronous) opens directories reliably on Linux/macOS;
    // do it on the blocking pool so we don't park the tokio worker.
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let dir = std::fs::File::open(&parent)?;
        dir.sync_all()
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(
                error = %e,
                path = %child_path.display(),
                "Blob parent-dir fsync failed (rename durability not guaranteed)"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %child_path.display(),
                "Blob parent-dir fsync task join failed"
            );
        }
    }
}

/// Chunk size for streaming file reads (256 KB).
const STREAM_CHUNK_SIZE: usize = 256 * 1024;

/// Max parallel blocking tasks for the [`fsync_paths_parallel`] sweep.
///
/// Concurrent fsyncs let journaling filesystems coalesce barriers (ext4
/// merges parallel fsyncs into shared journal commits), so a sweep over
/// thousands of chunk files costs a small fraction of issuing the same
/// fsyncs sequentially.
const SYNC_SWEEP_CONCURRENCY: usize = 16;

/// Fsync every path in `paths`, spread over up to
/// [`SYNC_SWEEP_CONCURRENCY`] blocking-pool tasks.
///
/// `strict` mirrors the two durability tiers already present in this
/// module: blob *files* must be durable (hard error on failure, like
/// `put_blob_from_bytes`), while *directory* fsyncs are best-effort
/// (logged warning, like [`fsync_parent_dir`]) — directories can't be
/// opened for fsync on every platform.
async fn fsync_paths_parallel(paths: Vec<PathBuf>, strict: bool) -> Result<(), DomainError> {
    if paths.is_empty() {
        return Ok(());
    }
    let group_size = paths.len().div_ceil(SYNC_SWEEP_CONCURRENCY);
    let task_count = paths.len().min(SYNC_SWEEP_CONCURRENCY);
    let mut source = paths.into_iter();
    let mut tasks = Vec::with_capacity(task_count);
    loop {
        // `paths` is owned by this function. Move each PathBuf into its task
        // group instead of cloning every allocation merely to satisfy the
        // blocking task's `'static` lifetime.
        let group: Vec<PathBuf> = source.by_ref().take(group_size).collect();
        if group.is_empty() {
            break;
        }
        tasks.push(tokio::task::spawn_blocking(
            move || -> Result<(), (PathBuf, std::io::Error)> {
                for path in &group {
                    let result = std::fs::File::open(path).and_then(|f| f.sync_all());
                    if let Err(e) = result {
                        if strict {
                            return Err((path.clone(), e));
                        }
                        tracing::warn!(
                            error = %e,
                            path = %path.display(),
                            "Blob sync sweep: best-effort fsync failed"
                        );
                    }
                }
                Ok(())
            },
        ));
    }
    for task in tasks {
        task.await
            .map_err(|e| DomainError::internal_error("Blob", format!("sync sweep join: {e}")))?
            .map_err(|(path, e)| {
                DomainError::internal_error(
                    "Blob",
                    format!("sync sweep fsync of {} failed: {e}", path.display()),
                )
            })?;
    }
    Ok(())
}

#[inline]
fn hex_prefix_symbol(byte: u8) -> Option<usize> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as usize),
        b'a'..=b'f' => Some((byte - b'a' + 10) as usize),
        // Preserve the exact directory spelling.  On a case-sensitive
        // filesystem `af/` and `AF/` are different durability domains; folding
        // them into one bitmap slot could omit one parent-directory fsync.
        b'A'..=b'F' => Some((byte - b'A' + 16) as usize),
        _ => None,
    }
}

#[inline]
fn hash_prefix_slot(hash: &str) -> Option<usize> {
    let bytes = hash.as_bytes();
    Some(hex_prefix_symbol(*bytes.first()?)? * 22 + hex_prefix_symbol(*bytes.get(1)?)?)
}

/// Create `blob_path` and write `data` into it.
///
/// Returns the open file handle so the caller decides the durability tier
/// (fsync now vs. deferred batch sync), or `None` when the blob already
/// existed (idempotent skip — content-addressed, so identical by definition).
async fn write_blob_bytes(blob_path: &Path, data: &Bytes) -> Result<Option<File>, DomainError> {
    // One atomic O_CREAT|O_EXCL open replaces the old stat-then-create pair:
    // `AlreadyExists` IS the idempotent skip (content-addressed names mean an
    // existing file has identical content), saving a syscall + a blocking-pool
    // dispatch on every new chunk of every upload.
    let mut file = match fs::File::options()
        .write(true)
        .create_new(true)
        .open(blob_path)
        .await
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => {
            return Err(DomainError::internal_error(
                "Blob",
                format!("Failed to create blob file: {}", e),
            ));
        }
    };
    file.write_all(data).await.map_err(|e| {
        DomainError::internal_error("Blob", format!("Failed to write blob from bytes: {}", e))
    })?;
    Ok(Some(file))
}

/// Bench-only public wrapper (feature = "bench") over the private chunk
/// writer so `examples/bench_storage_micro.rs` can A/B the open strategy.
#[cfg(feature = "bench")]
pub async fn write_blob_bytes_for_bench(
    blob_path: &Path,
    data: &Bytes,
) -> Result<Option<File>, DomainError> {
    write_blob_bytes(blob_path, data).await
}

/// Compile-time lookup table for the 256 two-digit lowercase hex prefixes ("00"…"ff").
pub(crate) static HEX_PREFIXES: [&str; 256] = [
    "00", "01", "02", "03", "04", "05", "06", "07", "08", "09", "0a", "0b", "0c", "0d", "0e", "0f",
    "10", "11", "12", "13", "14", "15", "16", "17", "18", "19", "1a", "1b", "1c", "1d", "1e", "1f",
    "20", "21", "22", "23", "24", "25", "26", "27", "28", "29", "2a", "2b", "2c", "2d", "2e", "2f",
    "30", "31", "32", "33", "34", "35", "36", "37", "38", "39", "3a", "3b", "3c", "3d", "3e", "3f",
    "40", "41", "42", "43", "44", "45", "46", "47", "48", "49", "4a", "4b", "4c", "4d", "4e", "4f",
    "50", "51", "52", "53", "54", "55", "56", "57", "58", "59", "5a", "5b", "5c", "5d", "5e", "5f",
    "60", "61", "62", "63", "64", "65", "66", "67", "68", "69", "6a", "6b", "6c", "6d", "6e", "6f",
    "70", "71", "72", "73", "74", "75", "76", "77", "78", "79", "7a", "7b", "7c", "7d", "7e", "7f",
    "80", "81", "82", "83", "84", "85", "86", "87", "88", "89", "8a", "8b", "8c", "8d", "8e", "8f",
    "90", "91", "92", "93", "94", "95", "96", "97", "98", "99", "9a", "9b", "9c", "9d", "9e", "9f",
    "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "aa", "ab", "ac", "ad", "ae", "af",
    "b0", "b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9", "ba", "bb", "bc", "bd", "be", "bf",
    "c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9", "ca", "cb", "cc", "cd", "ce", "cf",
    "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9", "da", "db", "dc", "dd", "de", "df",
    "e0", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8", "e9", "ea", "eb", "ec", "ed", "ee", "ef",
    "f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "fa", "fb", "fc", "fd", "fe", "ff",
];

/// Local filesystem blob backend.
///
/// Blobs are stored under `blob_root/{2-char-prefix}/{hash}.blob`.
/// Temporary upload staging uses `temp_root/`.
pub struct LocalBlobBackend {
    blob_root: PathBuf,
    temp_root: PathBuf,
    /// Chunk read-ahead depth for CDC reassembly — see [`Self::new`].
    read_prefetch: usize,
}

/// Default chunk-open read-ahead for the local backend (overrides the trait's
/// conservative `1`).
///
/// Benchmarked with `examples/bench_blob_prefetch` on SSD-class storage: a small
/// read-ahead is the sweet spot for the *disk-bound* read paths — localhost/LAN
/// downloads and, importantly, the internal blob reads that drain as fast as the
/// disk delivers (thumbnail render, transcode, ZIP export, content extraction),
/// all of which flow through `DedupService::stream_chunks`'s `buffered(N)`.
///
/// Measured median throughput vs the old sequential `N=1`:
///   warm disk-bound  +11.8% (N=2)   cold disk-bound  +7.2% (N=2)
///   network-bound (throttled)  ≈ 0% — the consumer, not the disk, is the cap
///   N=16  −4.4% warm — fan-out past a couple turns one sequential read into
///         competing random I/O over scattered content-addressed chunk files.
///
/// `2` deliberately captures most of that gain at the lowest fan-out, because
/// `buffered(N)` here overlaps the per-chunk `File::open` (cheap on local disk),
/// not the data read, so deeper queues buy little and risk seek contention on
/// the spinning disks we can't bench here. Operators tune it via
/// `OXICLOUD_LOCAL_READ_PREFETCH` (set `1` on seek-bound HDDs to restore the old
/// strictly-sequential behaviour; raise it on fast NVMe arrays).
const DEFAULT_LOCAL_READ_PREFETCH: usize = 2;

impl LocalBlobBackend {
    /// Create a new local backend rooted at `storage_root`.
    ///
    /// Blob files go under `{storage_root}/.blobs/`, temp files under
    /// `{storage_root}/.dedup_temp/`.
    pub fn new(storage_root: &Path) -> Self {
        // Read-ahead depth: env override, else the benchmark-backed default.
        // Clamped to ≥1 so a bogus `0` can't stall reads (buffered(0) would
        // make no progress; `stream_chunks` also guards with `.max(1)`).
        let read_prefetch = std::env::var("OXICLOUD_LOCAL_READ_PREFETCH")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map(|n| n.max(1))
            .unwrap_or(DEFAULT_LOCAL_READ_PREFETCH);
        Self {
            blob_root: storage_root.join(".blobs"),
            temp_root: storage_root.join(".dedup_temp"),
            read_prefetch,
        }
    }

    /// Compute the filesystem path for a blob hash.
    pub fn blob_path(&self, hash: &str) -> PathBuf {
        let prefix = &hash[0..2];
        self.blob_root.join(prefix).join(format!("{}.blob", hash))
    }

    /// Return a reference to the blob root directory.
    pub fn blob_root(&self) -> &Path {
        &self.blob_root
    }
}

impl BlobStorageBackend for LocalBlobBackend {
    fn initialize(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        Box::pin(async move {
            fs::create_dir_all(&self.blob_root)
                .await
                .map_err(DomainError::from)?;
            fs::create_dir_all(&self.temp_root)
                .await
                .map_err(DomainError::from)?;

            // Create the 256 hash-prefix directories (00-ff)
            for prefix in &HEX_PREFIXES {
                fs::create_dir_all(self.blob_root.join(prefix))
                    .await
                    .map_err(DomainError::from)?;
            }
            Ok(())
        })
    }

    fn put_blob(
        &self,
        hash: &str,
        source_path: &Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        let source_path = source_path.to_owned();
        Box::pin(async move {
            let blob_path = self.blob_path(&hash);

            let file_size = fs::metadata(&source_path)
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "Blob",
                        format!("Failed to stat source file: {}", e),
                    )
                })?
                .len();

            // Idempotent: if blob already exists, just remove the source
            if fs::try_exists(&blob_path).await.unwrap_or(false) {
                let _ = fs::remove_file(&source_path).await;
                return Ok(file_size);
            }

            // Atomic rename (same filesystem).  Falls back to copy+delete for
            // cross-device moves (EXDEV errno 18).
            //
            // Durability boundary: the caller is responsible for having
            // sync_all'd the source file before invoking this function.
            // (The streaming upload path writes chunks via
            // `put_blob_from_bytes_unsynced` + a batched `sync_blobs`
            // sweep instead; this move-based entry point remains for
            // whole-file producers such as migration tooling and tests.)
            // We fsync the parent of `blob_path` AFTER the rename
            // so the dirent change itself becomes durable; without
            // that, a power loss can resurrect the old (unrenamed)
            // name even when the file contents survive.
            if let Err(e) = fs::rename(&source_path, &blob_path).await {
                if e.raw_os_error() == Some(18) {
                    // EXDEV — cross-device link. The copy() target is
                    // a fresh file we created, so fsync it before the
                    // parent-dir fsync below.
                    fs::copy(&source_path, &blob_path).await.map_err(|ce| {
                        DomainError::internal_error(
                            "Blob",
                            format!("Failed to copy file to blob store: {}", ce),
                        )
                    })?;
                    if let Ok(f) = fs::File::open(&blob_path).await {
                        let _ = f.sync_all().await;
                    }
                    let _ = fs::remove_file(&source_path).await;
                } else if fs::try_exists(&blob_path).await.unwrap_or(false) {
                    // Concurrent writer placed the blob — discard our copy
                    let _ = fs::remove_file(&source_path).await;
                    tracing::debug!("Blob placed by concurrent writer: {}", e);
                } else {
                    return Err(DomainError::internal_error(
                        "Blob",
                        format!("Failed to move file to blob store: {}", e),
                    ));
                }
            }

            fsync_parent_dir(&blob_path).await;

            Ok(file_size)
        })
    }

    fn put_blob_from_bytes(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let blob_path = self.blob_path(&hash);
            let size = data.len() as u64;

            // Same durability story as `put_blob`: the blob file is
            // fsync'd before the parent directory is, so both the content
            // and the dirent creation survive a power loss in the same
            // step. (tokio's `sync_all` flushes its internal buffer
            // before issuing the fsync.)
            if let Some(file) = write_blob_bytes(&blob_path, &data).await? {
                file.sync_all().await.map_err(|e| {
                    DomainError::internal_error("Blob", format!("Failed to fsync blob file: {}", e))
                })?;
                drop(file);
                fsync_parent_dir(&blob_path).await;
            }

            Ok(size)
        })
    }

    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let blob_path = self.blob_path(&hash);
            let size = data.len() as u64;

            if let Some(mut file) = write_blob_bytes(&blob_path, &data).await? {
                // flush surfaces write errors (e.g. ENOSPC) that tokio
                // would otherwise swallow on drop. It does NOT fsync —
                // durability comes from the caller's later `sync_blobs`.
                file.flush().await.map_err(|e| {
                    DomainError::internal_error("Blob", format!("Failed to flush blob file: {}", e))
                })?;
            }

            Ok(size)
        })
    }

    /// **Atomic replace**: write to a same-directory tempfile, fsync,
    /// then `rename(2)` over the target. `write_blob_bytes`'s
    /// `O_CREAT|O_EXCL` idempotent-skip (the right choice for uploads)
    /// silently no-ops when the target already exists — wrong for
    /// callers like `storage_rotate` that need the bytes to change.
    /// See the trait doc for the full picture.
    ///
    /// Tempfile lives beside the target under the same shard directory
    /// so `rename` is a cheap same-filesystem operation (never an
    /// EXDEV cross-device copy fallback). The tempfile name embeds
    /// the process pid + a monotonic counter so parallel replaces on
    /// the same hash from different tasks don't clobber each other.
    fn put_blob_from_bytes_replace(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let blob_path = self.blob_path(&hash);
            let size = data.len() as u64;

            // Tempfile in the SAME directory as the target → rename is
            // cheap same-filesystem, never EXDEV. Counter ensures
            // uniqueness under parallel replaces (rare — rotate is
            // sequential per-blob today, but future concurrency won't
            // corrupt).
            static REPLACE_COUNTER: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let counter = REPLACE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp_path = blob_path.with_file_name(format!(
                "{}.replace.{}.{}.tmp",
                hash,
                std::process::id(),
                counter
            ));

            // Create + write + fsync the tempfile. `create_new(true)`
            // stays here to catch the astronomically-unlikely case of
            // two tasks colliding on the same counter value (belt-and-
            // braces; the pid+counter naming already prevents it).
            {
                let mut tmp = fs::File::options()
                    .write(true)
                    .create_new(true)
                    .open(&tmp_path)
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "Blob",
                            format!("Failed to create replace-tmp: {}", e),
                        )
                    })?;
                if let Err(e) = tmp.write_all(&data).await {
                    let _ = fs::remove_file(&tmp_path).await;
                    return Err(DomainError::internal_error(
                        "Blob",
                        format!("Failed to write replace-tmp: {}", e),
                    ));
                }
                if let Err(e) = tmp.sync_all().await {
                    let _ = fs::remove_file(&tmp_path).await;
                    return Err(DomainError::internal_error(
                        "Blob",
                        format!("Failed to fsync replace-tmp: {}", e),
                    ));
                }
            }

            // Atomic replace. On POSIX `rename(2)` is atomic within a
            // filesystem — a concurrent reader sees either the old or
            // new bytes, never a truncated view. Older bytes drop out
            // as soon as no reader holds an open fd.
            if let Err(e) = fs::rename(&tmp_path, &blob_path).await {
                let _ = fs::remove_file(&tmp_path).await;
                return Err(DomainError::internal_error(
                    "Blob",
                    format!("Failed to atomically replace blob: {}", e),
                ));
            }

            // fsync the parent directory so the dirent change (i.e. the
            // rename result) survives a power loss, same discipline as
            // the create path in `put_blob_from_bytes`.
            fsync_parent_dir(&blob_path).await;

            Ok(size)
        })
    }

    fn sync_blobs(
        &self,
        hashes: &[String],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        if hashes.is_empty() {
            return Box::pin(async { Ok(()) });
        }
        let mut paths = Vec::with_capacity(hashes.len());
        let mut dirs = Vec::with_capacity(hashes.len().min(HEX_PREFIXES.len()));
        if let [hash] = hashes {
            // Common tiny upload: reuse the already-built path's parent.  This
            // preserves the old one-item cost and avoids zeroing a bitmap whose
            // O(1) advantage only starts once there is something to deduplicate.
            let path = self.blob_path(hash);
            if let Some(parent) = path.parent() {
                dirs.push(parent.to_owned());
            }
            paths.push(path);
        } else {
            // 10 digits + 6 lowercase + 6 uppercase symbols per position.  The
            // 484-byte bitmap is still stack-only/O(1), while preserving exact
            // parent paths on case-sensitive filesystems.
            let mut seen_prefix = [false; 22 * 22];
            for hash in hashes {
                paths.push(self.blob_path(hash));
                if let Some(slot) = hash_prefix_slot(hash) {
                    if !seen_prefix[slot] {
                        seen_prefix[slot] = true;
                        dirs.push(self.blob_root.join(&hash[..2]));
                    }
                } else {
                    // `blob_path` already requires an ASCII two-byte prefix, and
                    // content hashes are canonical hex.  Retain the old behaviour
                    // for a non-hex caller without panicking here: syncing a
                    // duplicate invalid parent is safer than silently omitting it.
                    dirs.push(self.blob_root.join(&hash[..2]));
                }
            }
        }
        Box::pin(async move {
            // Each distinct prefix directory is fsync'd exactly once —
            // chunks of one upload land in at most 256 prefix dirs, so
            // this replaces one dir fsync *per chunk* with ≤256 total.
            // Files first (hard requirement), then dirents (best-effort,
            // same tier as fsync_parent_dir).
            fsync_paths_parallel(paths, true).await?;
            fsync_paths_parallel(dirs, false).await?;
            Ok(())
        })
    }

    fn get_blob_stream(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let hash = hash.to_owned();
        Box::pin(async move {
            let blob_path = self.blob_path(&hash);
            let file = File::open(&blob_path).await.map_err(|e| {
                DomainError::new(
                    ErrorKind::NotFound,
                    "Blob",
                    format!("Failed to open blob {}: {}", hash, e),
                )
            })?;
            Ok(Box::pin(ReaderStream::with_capacity(file, STREAM_CHUNK_SIZE)) as BlobStream)
        })
    }

    fn get_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let hash = hash.to_owned();
        Box::pin(async move {
            let blob_path = self.blob_path(&hash);
            let mut file = File::open(&blob_path).await.map_err(|e| {
                DomainError::new(
                    ErrorKind::NotFound,
                    "Blob",
                    format!("Failed to open blob {}: {}", hash, e),
                )
            })?;

            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| {
                    DomainError::internal_error("Blob", format!("Failed to seek in blob: {}", e))
                })?;

            if let Some(end_pos) = end {
                use tokio::io::AsyncReadExt;
                let limit = end_pos.saturating_sub(start);
                let limited = file.take(limit);
                Ok(Box::pin(ReaderStream::with_capacity(limited, STREAM_CHUNK_SIZE)) as BlobStream)
            } else {
                Ok(Box::pin(ReaderStream::with_capacity(file, STREAM_CHUNK_SIZE)) as BlobStream)
            }
        })
    }

    fn delete_blob(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let blob_path = self.blob_path(&hash);
            match fs::remove_file(&blob_path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), // idempotent
                Err(e) => Err(DomainError::internal_error(
                    "Blob",
                    format!("Failed to delete blob {}: {}", hash, e),
                )),
            }
        })
    }

    fn blob_exists(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let blob_path = self.blob_path(&hash);
            Ok(fs::try_exists(&blob_path).await.unwrap_or(false))
        })
    }

    fn blob_size(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let blob_path = self.blob_path(&hash);
            let meta = fs::metadata(&blob_path).await.map_err(|e| {
                DomainError::new(
                    ErrorKind::NotFound,
                    "Blob",
                    format!("Failed to stat blob {}: {}", hash, e),
                )
            })?;
            Ok(meta.len())
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        Box::pin(async move {
            let writable = fs::metadata(&self.blob_root).await.is_ok();
            Ok(StorageHealthStatus {
                connected: writable,
                backend_type: "local".to_string(),
                message: if writable {
                    "Local filesystem is accessible".to_string()
                } else {
                    "Blob root directory is not accessible".to_string()
                },
                available_bytes: None,
            })
        })
    }

    fn backend_type(&self) -> &'static str {
        "local"
    }

    fn local_blob_path(&self, hash: &str) -> Option<PathBuf> {
        Some(self.blob_path(hash))
    }

    /// Local disk read-ahead for CDC reassembly. Overrides the trait default of
    /// `1` with a small benchmark-backed depth (default `2`, env-tunable via
    /// `OXICLOUD_LOCAL_READ_PREFETCH`). See [`DEFAULT_LOCAL_READ_PREFETCH`].
    fn read_prefetch(&self) -> usize {
        self.read_prefetch
    }

    /// Enumerate `.blob` files under `.blobs/<xx>/`. Cursor format:
    ///
    /// * `None` — start from the first shard (`00`) at file offset 0
    /// * `Some("<shard>/<hash>")` — resume: skip shards `< shard`
    ///   entirely, and within `shard` skip files whose hash `≤ hash`.
    ///
    /// Ordering: shards ascending (00–ff), files within a shard
    /// ascending by hash. Stable across calls given the sorting.
    ///
    /// Filter: basename must be exactly 64 hex chars + `.blob`. This
    /// excludes `.tmp` staging files, `.orig`/`.lost`/`.corrupt`
    /// sidecars from manual admin work, and any other non-canonical
    /// artefacts. Backend consistency scans the DB-registered
    /// content-addressable set only.
    fn list_blob_hashes(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        crate::application::ports::blob_storage_ports::BlobListPage,
                        DomainError,
                    >,
                > + Send
                + '_,
        >,
    > {
        use crate::application::ports::blob_storage_ports::{
            BackendBlobEntry, BackendUnknownEntry, BlobListPage,
        };

        let blob_root = self.blob_root.clone();
        Box::pin(async move {
            let (start_shard, start_after_hash): (String, Option<String>) = match cursor {
                None => (String::from("00"), None),
                Some(c) => match c.split_once('/') {
                    Some((sh, h)) => (sh.to_string(), Some(h.to_string())),
                    None => (c, None),
                },
            };

            let mut blobs: Vec<BackendBlobEntry> = Vec::with_capacity(limit);
            let mut unknowns: Vec<BackendUnknownEntry> = Vec::new();
            let mut next_cursor: Option<String> = None;

            for prefix in &HEX_PREFIXES {
                let prefix = *prefix;
                if prefix < start_shard.as_str() {
                    continue;
                }
                let shard_dir = blob_root.join(prefix);
                let mut entries = match fs::read_dir(&shard_dir).await {
                    Ok(e) => e,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        return Err(DomainError::new(
                            ErrorKind::InternalError,
                            "Blob",
                            format!("read shard {prefix}: {e}"),
                        ));
                    }
                };

                // Collect canonical blobs + unknowns for this shard.
                // The distinction is filename shape: `<64-hex>.blob`
                // → canonical blob; anything else → unknown sidecar.
                // Unknowns are captured with their full basename so
                // the tenant can surface them to operators as
                // informational notices (severity `anomaly`).
                let mut shard_blobs: Vec<(String, Option<DateTime<Utc>>)> = Vec::new();
                let mut shard_unknowns: Vec<(String, Option<DateTime<Utc>>)> = Vec::new();
                while let Some(dirent) = entries.next_entry().await.map_err(|e| {
                    DomainError::new(
                        ErrorKind::InternalError,
                        "Blob",
                        format!("read shard {prefix} entry: {e}"),
                    )
                })? {
                    let name = dirent.file_name();
                    let name_str = match name.to_str() {
                        Some(s) => s,
                        None => continue, // non-UTF8 filename — skip entirely
                    };
                    // Skip directories — the shard dir itself
                    // shouldn't contain any, but defensively.
                    if dirent
                        .file_type()
                        .await
                        .map(|t| t.is_dir())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let mtime = dirent
                        .metadata()
                        .await
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(DateTime::<Utc>::from);

                    // Canonical shape check: `<64-hex>.blob`.
                    let canonical = name_str
                        .strip_suffix(".blob")
                        .filter(|stem| {
                            stem.len() == 64 && stem.chars().all(|c| c.is_ascii_hexdigit())
                        })
                        .map(|s| s.to_string());

                    match canonical {
                        Some(hash) => shard_blobs.push((hash, mtime)),
                        None => shard_unknowns.push((name_str.to_string(), mtime)),
                    }
                }
                shard_blobs.sort_by(|a, b| a.0.cmp(&b.0));

                // Unknowns don't need cursor-precise ordering — they
                // ride alongside the blobs batch. Sort just for
                // stable operator-facing output.
                shard_unknowns.sort_by(|a, b| a.0.cmp(&b.0));
                for (name, mtime) in shard_unknowns {
                    unknowns.push(BackendUnknownEntry {
                        path: format!("{prefix}/{name}"),
                        mtime,
                    });
                }

                for (hash, mtime) in shard_blobs {
                    if prefix == start_shard.as_str()
                        && let Some(ref after) = start_after_hash
                        && hash.as_str() <= after.as_str()
                    {
                        continue;
                    }
                    if blobs.len() >= limit {
                        next_cursor = Some(format!(
                            "{}/{}",
                            prefix,
                            blobs.last().map(|e| e.hash.as_str()).unwrap_or("")
                        ));
                        return Ok(BlobListPage {
                            blobs,
                            unknowns,
                            next_cursor,
                        });
                    }
                    blobs.push(BackendBlobEntry { hash, mtime });
                }
            }

            Ok(BlobListPage {
                blobs,
                unknowns,
                next_cursor,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tempfile::TempDir;

    /// 64-char fake hash with the given 2-char prefix (selects the prefix dir).
    fn fake_hash(prefix: &str) -> String {
        format!("{prefix}{}", "0".repeat(62))
    }

    async fn read_blob(backend: &LocalBlobBackend, hash: &str) -> Vec<u8> {
        let mut stream = backend.get_blob_stream(hash).await.unwrap();
        let mut data = Vec::new();
        while let Some(chunk) = stream.next().await {
            data.extend_from_slice(&chunk.unwrap());
        }
        data
    }

    #[tokio::test]
    async fn unsynced_write_then_sync_blobs_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let backend = LocalBlobBackend::new(tmp.path());
        backend.initialize().await.unwrap();

        // Two different prefixes → exercises the distinct-parent-dir dedup.
        let h1 = fake_hash("aa");
        let h2 = fake_hash("bb");
        backend
            .put_blob_from_bytes_unsynced(&h1, Bytes::from_static(b"chunk one"))
            .await
            .unwrap();
        backend
            .put_blob_from_bytes_unsynced(&h2, Bytes::from_static(b"chunk two"))
            .await
            .unwrap();

        backend.sync_blobs(&[h1.clone(), h2.clone()]).await.unwrap();

        assert!(backend.blob_exists(&h1).await.unwrap());
        assert!(backend.blob_exists(&h2).await.unwrap());
        assert_eq!(read_blob(&backend, &h1).await, b"chunk one");
        assert_eq!(read_blob(&backend, &h2).await, b"chunk two");
    }

    #[tokio::test]
    async fn unsynced_write_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let backend = LocalBlobBackend::new(tmp.path());
        backend.initialize().await.unwrap();

        let hash = fake_hash("cc");
        let size1 = backend
            .put_blob_from_bytes_unsynced(&hash, Bytes::from_static(b"same content"))
            .await
            .unwrap();
        let size2 = backend
            .put_blob_from_bytes_unsynced(&hash, Bytes::from_static(b"same content"))
            .await
            .unwrap();

        assert_eq!(size1, size2);
        assert_eq!(read_blob(&backend, &hash).await, b"same content");
    }

    #[tokio::test]
    async fn sync_blobs_fails_on_missing_blob() {
        let tmp = TempDir::new().unwrap();
        let backend = LocalBlobBackend::new(tmp.path());
        backend.initialize().await.unwrap();

        let missing = fake_hash("dd");
        assert!(
            backend.sync_blobs(&[missing]).await.is_err(),
            "sweeping a never-written blob must fail — the caller would \
             otherwise insert a PG row for a chunk that doesn't exist"
        );
    }

    #[tokio::test]
    async fn sync_blobs_empty_is_noop() {
        let tmp = TempDir::new().unwrap();
        let backend = LocalBlobBackend::new(tmp.path());
        backend.initialize().await.unwrap();

        backend.sync_blobs(&[]).await.unwrap();
    }

    #[test]
    fn prefix_slots_cover_lowercase_hex_space_and_preserve_case() {
        let mut seen = [false; 22 * 22];
        for prefix in HEX_PREFIXES {
            let hash = format!("{prefix}{}", "0".repeat(62));
            let slot = hash_prefix_slot(&hash).unwrap();
            assert!(!seen[slot]);
            seen[slot] = true;
        }
        assert_eq!(seen.into_iter().filter(|value| *value).count(), 256);
        assert_ne!(
            hash_prefix_slot(&fake_hash("af")),
            hash_prefix_slot(&fake_hash("aF"))
        );
        assert_eq!(hash_prefix_slot("gg"), None);
    }
}
