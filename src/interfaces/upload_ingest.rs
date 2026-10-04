//! Shared streaming upload ingestion: request body → CDC chunk store.
//!
//! Used by every upload surface (REST multipart, native WebDAV PUT,
//! NextCloud PUT, chunked-upload assembly, WOPI PutFile) so none of them
//! buffers the full body in RAM **or spools it to a temp file**. The bytes
//! flow straight into [`BlobHandler::store_from_stream`], which chunks
//! (FastCDC), hashes (BLAKE3) and dedup-checks them while they arrive —
//! each uploaded byte touches the disk at most once, and not at all when
//! the store already has its chunk.
//!
//! MIME refinement happens in-flight: when the claimed Content-Type is
//! generic, the first bytes are peeked off the stream for magic-byte
//! detection before being forwarded unchanged.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::body::Body;
use bytes::Bytes;
use futures::stream::{self, Stream, StreamExt, TryStreamExt};
use http_body_util::BodyStream;
// The `Digest` trait (re-exported by both `md5` and `sha2` from the
// `digest` crate) gives `Md5` and `Sha256` their `new` / `update` /
// `finalize` methods. Importing once via `sha2` covers both —
// otherwise every call site would need fully-qualified
// `<md5::Md5 as md5::Digest>::…` syntax.
use sha2::Digest as _;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::application::ports::chunked_upload_ports::ChecksumAlg;
use crate::application::ports::file_ports::StoredBlob;
use crate::common::mime_detect::{MAGIC_BYTES_LEN, is_generic_mime, refine_content_type};
use crate::infrastructure::services::blob_handler::BlobHandler;
use crate::interfaces::errors::AppError;

/// Content stored in the chunk store by one upload ingest.
///
/// The ingest holds ONE blob reference; pass [`IngestedBlob::stored`] to the
/// upload service (which takes ownership of the reference) or hand it back
/// via [`discard_ingested`] when the upload is rejected after the fact.
pub struct IngestedBlob {
    /// BLAKE3 of the full content (the blob/manifest key).
    pub hash: String,
    /// Total bytes ingested.
    pub size: u64,
    /// Refined content type (claimed type or magic-byte detection).
    pub content_type: String,
    /// `false` when the exact content already existed (dedup hit).
    pub is_new_blob: bool,
    /// Bytes that did not need to be transferred to storage (dedup hit).
    pub bytes_saved: u64,
}

impl IngestedBlob {
    /// The blob reference to hand to the upload service.
    pub fn stored(&self) -> StoredBlob {
        StoredBlob {
            hash: self.hash.clone(),
            size: self.size,
            is_new_blob: self.is_new_blob,
        }
    }
}

/// Hand back the blob reference taken by a successful ingest when the upload
/// is rejected after the fact (quota exceeded, checksum mismatch, …).
pub async fn discard_ingested(dedup: &BlobHandler, blob: &IngestedBlob) {
    if let Err(e) = dedup.remove_reference(&blob.hash).await {
        tracing::warn!(
            "Failed to release blob reference of rejected upload {}: {e}",
            &blob.hash[..blob.hash.len().min(12)]
        );
    }
}

/// Shared mutable tee for computing a client-requested checksum during the
/// ingest pass (REST chunked uploads) — no post-store re-read needed.
pub type ChecksumTee = Arc<StdMutex<Option<IncrementalHasher>>>;

/// A byte-range stream paired with its known length, used by
/// [`ingest_range_patch_to_cas`] for the untouched prefix/suffix either
/// side of a PATCH edit.
pub type RangeSegment = (
    Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    u64,
);

/// Size cap and expected-length validation for [`ingest_range_patch_to_cas`].
pub struct PatchIngestBudget {
    /// Caps the size of the *edit* (the request body), not the whole
    /// spliced file — see the field's use in [`ingest_range_patch_to_cas`].
    pub max_bytes: usize,
    /// Declared `X-Update-Range` span, `None` for `append`. Validated
    /// against the actual streamed body length, not `Content-Length`.
    pub expected_body_len: Option<u64>,
}

/// Create a checksum tee for [`ingest_stream_to_cas`].
pub fn checksum_tee(alg: ChecksumAlg) -> ChecksumTee {
    Arc::new(StdMutex::new(Some(IncrementalHasher::new(alg))))
}

/// Finalize a checksum tee into its lowercase hex digest.
pub fn finalize_checksum_tee(tee: &ChecksumTee) -> Option<String> {
    tee.lock()
        .ok()
        .and_then(|mut h| h.take())
        .map(IncrementalHasher::finalize_hex)
}

/// Out-of-band state observed by the stream adapters while the dedup engine
/// consumes the stream — lets the caller map an opaque engine error back to
/// the precise HTTP failure (413 vs 400).
struct IngestFlags {
    too_large: AtomicBool,
    source_error: StdMutex<Option<String>>,
}

/// Stream a request body (or any byte stream) into the CDC chunk store.
///
/// Single pass: size-cap enforcement, optional checksum tee, MIME sniffing
/// (first [`MAGIC_BYTES_LEN`] bytes, only when `claimed_type` is generic)
/// and the CDC chunk/hash/store pipeline all run while the bytes arrive.
/// Peak heap is bounded by the dedup engine (~9 MiB) regardless of size.
///
/// On error nothing stays referenced — the engine compensates internally.
pub async fn ingest_stream_to_cas<S, E>(
    source: S,
    dedup: &Arc<BlobHandler>,
    filename: &str,
    claimed_type: &str,
    max_bytes: usize,
    checksum: Option<ChecksumTee>,
) -> Result<IngestedBlob, AppError>
where
    S: Stream<Item = Result<Bytes, E>> + Send,
    E: std::fmt::Display,
{
    let flags = Arc::new(IngestFlags {
        too_large: AtomicBool::new(false),
        source_error: StdMutex::new(None),
    });

    // ── Adapter: cap + checksum tee + error capture ──────────────
    let adapter_flags = flags.clone();
    let mut total: usize = 0;
    let counted = source.map(move |item| match item {
        Ok(bytes) => {
            total += bytes.len();
            if total > max_bytes {
                adapter_flags.too_large.store(true, Ordering::Relaxed);
                return Err(std::io::Error::other("upload exceeds size cap"));
            }
            if let Some(tee) = &checksum
                && let Ok(mut hasher) = tee.lock()
                && let Some(hasher) = hasher.as_mut()
            {
                hasher.update(&bytes);
            }
            Ok(bytes)
        }
        Err(e) => {
            let message = e.to_string();
            if let Ok(mut slot) = adapter_flags.source_error.lock() {
                *slot = Some(message.clone());
            }
            Err(std::io::Error::other(message))
        }
    });
    // `fuse` is load-bearing: when the source is shorter than the MIME peek
    // (< MAGIC_BYTES_LEN), the peek loop drains it to None and the `chain`
    // below polls it once more — non-fused sources (e.g. `stream::unfold`,
    // as used for multipart fields) panic on a post-None poll.
    let mut counted = Box::pin(counted.fuse());

    // ── In-flight MIME sniff (only when the claimed type is generic) ──
    let mut head: Vec<Result<Bytes, std::io::Error>> = Vec::new();
    let content_type = if is_generic_mime(claimed_type) {
        let mut head_len = 0usize;
        while head_len < MAGIC_BYTES_LEN {
            match counted.next().await {
                Some(Ok(bytes)) => {
                    head_len += bytes.len();
                    head.push(Ok(bytes));
                }
                Some(Err(e)) => {
                    head.push(Err(e));
                    break;
                }
                None => break,
            }
        }
        let mut magic = Vec::with_capacity(head_len.min(MAGIC_BYTES_LEN));
        for item in head.iter().flatten() {
            let take = (MAGIC_BYTES_LEN - magic.len()).min(item.len());
            magic.extend_from_slice(&item[..take]);
            if magic.len() >= MAGIC_BYTES_LEN {
                break;
            }
        }
        refine_content_type(&magic, filename, claimed_type)
    } else {
        claimed_type.to_string()
    };

    // ── Store: peeked head + remainder, one continuous stream ────
    let full_stream = stream::iter(head).chain(counted);
    let result = dedup
        .store_from_stream(full_stream, Some(content_type.clone()))
        .await;

    match result {
        Ok(stored) => {
            let is_new_blob = !stored.was_deduplicated();
            let bytes_saved = match &stored {
                crate::application::ports::dedup_ports::DedupResultDto::ExistingBlob {
                    saved_bytes,
                    ..
                } => *saved_bytes,
                _ => 0,
            };
            Ok(IngestedBlob {
                hash: stored.hash().to_string(),
                size: stored.size(),
                content_type,
                is_new_blob,
                bytes_saved,
            })
        }
        Err(e) => {
            if flags.too_large.load(Ordering::Relaxed) {
                return Err(AppError::payload_too_large(format!(
                    "Upload body exceeds the direct-PUT cap ({max_bytes} bytes). \
                     Use the chunked-upload protocol (REST: `/api/uploads/...`, \
                     NextCloud: `/remote.php/dav/uploads/...`) for files larger than this. \
                     Chunked uploads are resumable on transient failure."
                )));
            }
            let source_error = flags.source_error.lock().ok().and_then(|s| s.clone());
            if let Some(message) = source_error {
                return Err(AppError::bad_request(format!(
                    "Failed to read request body: {message}"
                )));
            }
            Err(AppError::from(e))
        }
    }
}

/// [`ingest_stream_to_cas`] for an HTTP request body.
pub async fn ingest_body_to_cas(
    body: Body,
    dedup: &Arc<BlobHandler>,
    filename: &str,
    claimed_type: &str,
    max_bytes: usize,
) -> Result<IngestedBlob, AppError> {
    let source = BodyStream::new(body).filter_map(|item| async move {
        match item {
            Ok(frame) => frame.into_data().ok().map(Ok),
            Err(e) => Some(Err(e)),
        }
    });
    ingest_stream_to_cas(source, dedup, filename, claimed_type, max_bytes, None).await
}

/// Splice a PATCH request body ([RFC 5789]) between the file's untouched
/// `prefix`/`suffix` byte ranges and ingest the result as one continuous
/// stream into the CDC chunk store.
///
/// `prefix`/`suffix` are `stream::empty()`-backed when the edit starts at
/// byte 0 or reaches EOF respectively — callers build the real ranges from
/// [`FileRetrievalUseCase::get_file_range_stream_with_perms`](crate::application::ports::file_ports::FileRetrievalUseCase::get_file_range_stream_with_perms).
/// Because FastCDC chunking is content-defined rather than offset-defined,
/// unedited chunks on either side of the edit typically dedup for free.
///
/// Each of `prefix`/`suffix` is paired with its known byte length (from the
/// file's size and the requested range — callers compute it, not this
/// function). `budget.max_bytes` bounds the size of the *edit* (the request
/// body) — it is widened by the prefix/suffix lengths before being applied
/// to the combined stream, so that already-stored, untouched bytes being
/// re-ingested unchanged don't count against the cap. Without this, any
/// PATCH against a file at or above `max_bytes` would be rejected
/// regardless of how small the edit itself is.
///
/// `budget.expected_body_len`, when `Some`, is validated against the
/// *actual* number of body bytes streamed — not the client-supplied
/// `Content-Length` header, which chunked-transfer-encoded requests may
/// omit entirely. Counting the real bytes means a request that declares
/// `X-Update-Range: bytes=5-9` (a 5-byte span) but streams a
/// differently-sized body is caught regardless of whether `Content-Length`
/// was present. On mismatch the already-ingested blob is discarded and
/// never reaches the atomic store, so a rejected PATCH can't leave stray
/// unreferenced content.
///
/// [RFC 5789]: https://www.rfc-editor.org/rfc/rfc5789
pub async fn ingest_range_patch_to_cas(
    prefix: RangeSegment,
    body: Body,
    suffix: RangeSegment,
    dedup: &Arc<BlobHandler>,
    filename: &str,
    claimed_type: &str,
    budget: PatchIngestBudget,
) -> Result<IngestedBlob, AppError> {
    let PatchIngestBudget {
        max_bytes,
        expected_body_len,
    } = budget;
    let (prefix_stream, prefix_len) = prefix;
    let (suffix_stream, suffix_len) = suffix;
    let body_len = Arc::new(AtomicU64::new(0));
    let counter = body_len.clone();
    let body_stream = BodyStream::new(body).filter_map(move |item| {
        let counter = counter.clone();
        async move {
            match item {
                Ok(frame) => {
                    let bytes = frame.into_data().ok()?;
                    counter.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    Some(Ok(bytes))
                }
                Err(e) => Some(Err(std::io::Error::other(e.to_string()))),
            }
        }
    });
    let effective_max = max_bytes.saturating_add((prefix_len + suffix_len) as usize);
    let combined = prefix_stream.chain(body_stream).chain(suffix_stream);
    let ingested =
        ingest_stream_to_cas(combined, dedup, filename, claimed_type, effective_max, None)
            .await
            .map_err(|e| {
                if e.error_type == "PayloadTooLarge" {
                    AppError::payload_too_large(format!(
                        "PATCH body exceeds the direct-PATCH edit-size cap ({max_bytes} bytes). \
                     Use the chunked-upload protocol for edits larger than this."
                    ))
                } else {
                    e
                }
            })?;

    if let Some(expected) = expected_body_len {
        let actual = body_len.load(Ordering::Relaxed);
        if actual != expected {
            discard_ingested(dedup, &ingested).await;
            return Err(AppError::bad_request(format!(
                "PATCH body ({actual} bytes) does not match the X-Update-Range span ({expected} bytes)"
            )));
        }
    }

    Ok(ingested)
}

/// Adapt a multipart field into a byte stream for [`ingest_stream_to_cas`].
///
/// Terminates after the first error — multipart fields are not resumable.
pub fn multipart_field_stream(
    field: axum::extract::multipart::Field<'_>,
) -> impl Stream<Item = Result<Bytes, axum::extract::multipart::MultipartError>> + Send + '_ {
    stream::unfold((field, false), |(mut field, done)| async move {
        if done {
            return None;
        }
        match field.chunk().await {
            Ok(Some(bytes)) => Some((Ok(bytes), (field, false))),
            Ok(None) => None,
            Err(e) => Some((Err(e), (field, true))),
        }
    })
}

/// Concatenate already-uploaded chunk part files into one byte stream, in
/// the given order — feeds chunked-upload assembly into the CDC store
/// without ever materializing an assembled file on disk.
pub fn stream_from_files(
    paths: Vec<PathBuf>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    // 512 KiB per poll: each ReaderStream poll on a tokio::fs::File is one
    // blocking-pool dispatch + one read(2) of the buffer size. The old
    // 64 KiB buffer paid 8x the dispatches/syscalls of every other blob
    // read path (STREAM_CHUNK_SIZE = 256 KiB) for the single read pass
    // over every completed chunked upload (benches/UPLOAD-SPOOL.md).
    stream::iter(paths.into_iter().map(Ok::<_, std::io::Error>))
        .and_then(|path| async move {
            tokio::fs::File::open(path)
                .await
                .map(|file| ReaderStream::with_capacity(file, 512 * 1024))
        })
        .try_flatten()
}

/// Result of a streamed write to a caller-supplied path.
pub struct StreamedToPath {
    /// Total bytes written.
    pub bytes_written: u64,
    /// `true` when the destination did not exist before this call — the
    /// open itself detects it (`create_new` + AlreadyExists fallback), so
    /// retry-detection callers don't need a separate `stat` per chunk.
    pub created_fresh: bool,
    /// Lowercase hex digest, populated only when `checksum_alg=Some(_)`
    /// was passed. The algorithm is identified by [`StreamedToPath::alg`].
    pub checksum_hex: Option<String>,
    /// Algorithm used to compute `checksum_hex`. Echoed back so the
    /// caller can include it in audit logs or response headers.
    pub alg: Option<ChecksumAlg>,
}

/// Stream an HTTP request body directly to a known destination file,
/// enforcing `max_bytes` as a hard size limit.
///
/// Used by the chunked-upload PUT handlers — each chunk has a
/// deterministic on-disk path (`NextcloudChunkedUploadService::safe_chunk_path`
/// for the NC surface, `ChunkedUploadService::prepare_chunk` for the
/// REST surface), so there's no spool/move dance. Peak heap is ~one
/// HTTP frame regardless of chunk size or `max_bytes`.
///
/// `checksum_alg` is the optional client-requested integrity check
/// (default `md5` per the legacy `Content-MD5` contract; `blake3`
/// available for forward-compat). When `Some`, the hash is computed
/// incrementally during streaming — no extra disk read for verification.
///
/// On size overflow the partial file is removed before the function
/// returns, so a client retry against the same chunk name starts from
/// a clean slate. On any other I/O error the partial file is also
/// removed and the error surfaces — callers can assume the path is
/// either fully written or absent.
pub async fn stream_body_to_path(
    body: Body,
    path: &Path,
    max_bytes: usize,
    checksum_alg: Option<ChecksumAlg>,
) -> Result<StreamedToPath, AppError> {
    // BufWriter coalesces the per-HTTP-frame writes (~16-64 KiB each) into
    // 512 KiB write(2)s — a bare tokio File dispatches one blocking-pool op
    // per frame (benches/UPLOAD-SPOOL.md). Same capacity as the dedup
    // handler's spool loop. On the error paths below the partial file is
    // removed, so silently dropping unflushed buffer contents is fine.
    //
    // `create_new` first: the common fresh-chunk case stays one open AND
    // doubles as the retry probe (AlreadyExists → truncate-open), so callers
    // that need overwrite detection no longer pay a separate stat per chunk.
    let (file, created_fresh) = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(f) => (f, true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let f = tokio::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)
                .await
                .map_err(|e| AppError::internal_error(format!("Failed to open chunk file: {e}")))?;
            (f, false)
        }
        Err(e) => {
            return Err(AppError::internal_error(format!(
                "Failed to open chunk file: {e}"
            )));
        }
    };
    let mut file = tokio::io::BufWriter::with_capacity(512 * 1024, file);

    let mut total_bytes: usize = 0;
    let mut stream = BodyStream::new(body);
    let mut hasher = checksum_alg.map(IncrementalHasher::new);

    while let Some(frame_result) = stream.next().await {
        let frame = match frame_result {
            Ok(f) => f,
            Err(e) => {
                drop(file);
                let _ = tokio::fs::remove_file(path).await;
                return Err(AppError::bad_request(format!(
                    "Failed to read request body: {e}"
                )));
            }
        };
        if let Some(chunk) = frame.data_ref() {
            total_bytes += chunk.len();
            if total_bytes > max_bytes {
                drop(file);
                let _ = tokio::fs::remove_file(path).await;
                return Err(AppError::payload_too_large(format!(
                    "Chunk exceeds maximum size of {max_bytes} bytes"
                )));
            }
            if let Some(h) = hasher.as_mut() {
                h.update(chunk);
            }
            if let Err(e) = file.write_all(chunk).await {
                drop(file);
                let _ = tokio::fs::remove_file(path).await;
                return Err(AppError::internal_error(format!(
                    "Failed to write chunk: {e}"
                )));
            }
        }
    }
    file.flush()
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to flush chunk file: {e}")))?;
    drop(file);

    Ok(StreamedToPath {
        bytes_written: total_bytes as u64,
        created_fresh,
        checksum_hex: hasher.map(IncrementalHasher::finalize_hex),
        alg: checksum_alg,
    })
}

/// Algorithm-agnostic incremental hasher used by [`stream_body_to_path`]
/// and the [`ChecksumTee`] of chunked-upload completion.
/// Per-frame `update` is sub-millisecond for all three algorithms at the
/// 64 KB frame sizes axum's body stream produces, so we don't need
/// `spawn_blocking` (which the old buffered path used because it hashed
/// the full multi-MB chunk in one shot).
pub enum IncrementalHasher {
    Md5(md5::Md5),
    Sha256(sha2::Sha256),
    // Boxing — blake3::Hasher is ~1.7 KB on the stack while md5::Md5
    // (~100 bytes) and sha2::Sha256 (~100 bytes) are tiny; boxing the
    // outlier keeps the enum size proportional to the common case
    // rather than the worst case.
    Blake3(Box<blake3::Hasher>),
}

impl IncrementalHasher {
    fn new(alg: ChecksumAlg) -> Self {
        match alg {
            ChecksumAlg::Md5 => Self::Md5(md5::Md5::new()),
            ChecksumAlg::Sha256 => Self::Sha256(sha2::Sha256::new()),
            ChecksumAlg::Blake3 => Self::Blake3(Box::new(blake3::Hasher::new())),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Md5(h) => h.update(bytes),
            Self::Sha256(h) => h.update(bytes),
            Self::Blake3(h) => {
                h.update(bytes);
            }
        }
    }

    fn finalize_hex(self) -> String {
        match self {
            Self::Md5(h) => crate::common::fmt::hex_lower(&h.finalize()),
            Self::Sha256(h) => crate::common::fmt::hex_lower(&h.finalize()),
            Self::Blake3(h) => h.finalize().to_hex().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test]
    async fn stream_body_to_path_caps_oversized() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("chunk");

        // 5 MiB body, 4 MiB cap → must reject.
        let body = Body::from(Bytes::from(vec![0u8; 5 * 1024 * 1024]));
        let result = stream_body_to_path(body, &path, 4 * 1024 * 1024, None).await;
        assert!(
            result.is_err(),
            "expected PayloadTooLarge, got Ok(bytes_written={})",
            result.ok().map(|r| r.bytes_written).unwrap_or(0)
        );
        // Partial file must be removed on rejection.
        assert!(
            !path.exists(),
            "rejected chunk file should be removed, but {} still exists",
            path.display()
        );
    }

    #[tokio::test]
    async fn stream_body_to_path_accepts_under_cap() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("chunk");

        let body = Body::from(Bytes::from(vec![1u8; 1024 * 1024])); // 1 MiB
        let result = stream_body_to_path(body, &path, 4 * 1024 * 1024, None).await;
        let outcome = result.expect("should succeed");
        assert_eq!(outcome.bytes_written, 1024 * 1024);
        assert!(outcome.checksum_hex.is_none(), "no alg requested → no hash");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn stream_body_to_path_caps_at_exact_boundary() {
        // Edge case: body exactly equal to cap should succeed; cap+1 must fail.
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("chunk");

        let body = Body::from(Bytes::from(vec![1u8; 100]));
        let outcome = stream_body_to_path(body, &path, 100, None)
            .await
            .expect("100 bytes at 100-byte cap should succeed");
        assert_eq!(outcome.bytes_written, 100);

        let path2 = temp_dir.path().join("chunk2");
        let body = Body::from(Bytes::from(vec![1u8; 101]));
        assert!(
            stream_body_to_path(body, &path2, 100, None).await.is_err(),
            "101 bytes at 100-byte cap must reject"
        );
        assert!(!path2.exists());
    }

    #[tokio::test]
    async fn ingest_rejects_oversized_before_touching_storage() {
        // 1 KiB body against a 100-byte cap: the adapter must abort the
        // stream before any flush, so the stub dedup service (which cannot
        // reach PG) is never asked to settle a batch.
        let dedup = Arc::new(BlobHandler::new_stub());
        let source = stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(vec![0u8; 1024]))]);

        let result = ingest_stream_to_cas(
            source,
            &dedup,
            "file.bin",
            "application/octet-stream",
            100,
            None,
        )
        .await;

        let err = result.err().expect("oversized body must be rejected");
        assert_eq!(err.status_code, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn ingest_surfaces_source_errors_as_bad_request() {
        let dedup = Arc::new(BlobHandler::new_stub());
        let source = stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"partial")),
            Err(std::io::Error::other("connection reset by peer")),
        ]);

        let result =
            ingest_stream_to_cas(source, &dedup, "file.bin", "text/plain", usize::MAX, None).await;

        let err = result.err().expect("source error must surface");
        assert_eq!(err.status_code, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            err.message.contains("connection reset by peer"),
            "original cause must be preserved: {}",
            err.message
        );
    }

    /// Regression: a source shorter than the MIME peek (< MAGIC_BYTES_LEN)
    /// is drained to None during sniffing and then polled once more by the
    /// `chain` that re-attaches the peeked head. Non-fused sources — like
    /// the `stream::unfold` used for multipart fields — panic on that
    /// post-None poll ("Unfold must not be polled after it returned
    /// `Poll::Ready(None)`") unless the ingest fuses the stream first.
    /// The stub dedup service can't reach PG, so an orderly `Err` (not a
    /// panic) proves the stream layer survived.
    #[tokio::test]
    async fn ingest_short_stream_with_generic_mime_does_not_repoll_source() {
        let dedup = Arc::new(BlobHandler::new_stub());
        let source = stream::unfold(false, |done| async move {
            if done {
                None
            } else {
                Some((Ok::<_, std::io::Error>(Bytes::from_static(b"tiny")), true))
            }
        });

        let result = ingest_stream_to_cas(
            source,
            &dedup,
            "tiny.bin",
            "application/octet-stream",
            usize::MAX,
            None,
        )
        .await;

        assert!(
            result.is_err(),
            "stub DB must reject the store — but only AFTER the stream \
             layer survived the post-peek poll"
        );
    }

    #[tokio::test]
    async fn stream_from_files_concatenates_in_order() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let a = temp_dir.path().join("a");
        let b = temp_dir.path().join("b");
        tokio::fs::write(&a, b"Hello, ").await.unwrap();
        tokio::fs::write(&b, b"World!").await.unwrap();

        let mut out = Vec::new();
        let s = stream_from_files(vec![a, b]);
        futures::pin_mut!(s);
        while let Some(chunk) = s.next().await {
            out.extend_from_slice(&chunk.expect("read"));
        }
        assert_eq!(out, b"Hello, World!");
    }

    #[tokio::test]
    async fn checksum_tee_roundtrip() {
        let tee = checksum_tee(ChecksumAlg::Md5);
        if let Ok(mut h) = tee.lock()
            && let Some(h) = h.as_mut()
        {
            h.update(b"hello world");
        }
        let hex = finalize_checksum_tee(&tee).expect("digest");
        assert_eq!(hex, "5eb63bbbe01eeed093cb22bb8f5acdc3");
        assert!(
            finalize_checksum_tee(&tee).is_none(),
            "second finalize returns None"
        );
    }
}
