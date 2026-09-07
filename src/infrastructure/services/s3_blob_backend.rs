//! S3-Compatible Blob Backend — stores blobs in any S3-compatible object store.
//!
//! Supports AWS S3, Backblaze B2, Cloudflare R2, MinIO, DigitalOcean Spaces,
//! Wasabi, and any other service that implements the S3 API.

use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::fs;
use tokio_util::io::ReaderStream;

use crate::application::ports::blob_storage_ports::{
    BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use crate::common::config::S3StorageConfig;
use crate::domain::errors::{DomainError, ErrorKind};

/// S3-compatible blob storage backend.
///
/// Blobs are stored as objects with key `{2-char-prefix}/{hash}.blob`,
/// mirroring the local filesystem layout for consistency.
pub struct S3BlobBackend {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3BlobBackend {
    /// Build a new S3 backend from configuration.
    ///
    /// Supports custom endpoints for non-AWS providers (Backblaze B2,
    /// MinIO, Cloudflare R2, etc.).
    pub fn new(config: &S3StorageConfig) -> Self {
        let credentials = aws_sdk_s3::config::Credentials::new(
            &config.access_key,
            &config.secret_key,
            None,
            None,
            "oxicloud",
        );

        let mut builder = aws_sdk_s3::config::Builder::new()
            .region(aws_sdk_s3::config::Region::new(config.region.clone()))
            .credentials_provider(credentials)
            .behavior_version_latest();

        if let Some(ref endpoint) = config.endpoint_url {
            builder = builder.endpoint_url(endpoint);
        }

        if config.force_path_style {
            builder = builder.force_path_style(true);
        }

        let client = aws_sdk_s3::Client::from_conf(builder.build());

        Self {
            client,
            bucket: config.bucket.clone(),
        }
    }

    /// Compute the S3 object key for a given hash.
    fn object_key(hash: &str) -> String {
        let prefix = &hash[0..2];
        format!("{}/{}.blob", prefix, hash)
    }

    /// Inverse of [`Self::object_key`] — the hash a key names, or `None`
    /// when the key is not one we wrote.
    ///
    /// Deliberately strict, and paired with `object_key` so the round-trip
    /// stays honest. Enumeration passes no prefix to S3, so this filter is
    /// the *only* thing separating our namespace from everything else in
    /// the bucket; a lenient match would feed a non-hash into
    /// `object_key`, which slices `[0..2]` and would produce a nonsense
    /// resume position.
    fn hash_from_object_key(key: &str) -> Option<String> {
        let (prefix, rest) = key.split_once('/')?;
        if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let stem = rest.strip_suffix(".blob")?;
        if stem.len() != 64 || !stem.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        // The shard must be the hash's own first two characters, or
        // `object_key(hash)` would not reproduce this key.
        if !stem.starts_with(prefix) {
            return None;
        }
        Some(stem.to_string())
    }
}

impl BlobStorageBackend for S3BlobBackend {
    fn initialize(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        Box::pin(async move {
            // Verify bucket exists and is accessible
            self.client
                .head_bucket()
                .bucket(&self.bucket)
                .send()
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "S3",
                        format!("Cannot access bucket '{}': {}", self.bucket, e),
                    )
                })?;

            tracing::info!("S3 blob backend initialized: bucket={}", self.bucket);
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
            let key = Self::object_key(&hash);

            // Check if object already exists (idempotent)
            let exists = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .is_ok();

            if exists {
                // Blob already in S3 — remove local source and return size
                let file_size = fs::metadata(&source_path)
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "S3",
                            format!("Failed to stat source file: {}", e),
                        )
                    })?
                    .len();
                let _ = fs::remove_file(&source_path).await;
                return Ok(file_size);
            }

            // Upload from local file
            let body = ByteStream::from_path(&source_path).await.map_err(|e| {
                DomainError::internal_error("S3", format!("Failed to read source file: {}", e))
            })?;

            let file_size = fs::metadata(&source_path)
                .await
                .map_err(|e| {
                    DomainError::internal_error("S3", format!("Failed to stat source file: {}", e))
                })?
                .len();

            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(body)
                .send()
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "S3",
                        format!("Failed to upload blob {}: {}", hash, e),
                    )
                })?;

            // Clean up local source after successful upload
            let _ = fs::remove_file(&source_path).await;

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
            let key = Self::object_key(&hash);
            let size = data.len() as u64;

            // Idempotent: skip if already exists
            if self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .is_ok()
            {
                return Ok(size);
            }

            let body = ByteStream::from(data);
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(body)
                .send()
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "S3",
                        format!("Failed to upload blob {}: {}", hash, e),
                    )
                })?;

            Ok(size)
        })
    }

    /// Dedup settle path: PUT unconditionally. Keys are content-addressed
    /// (BLAKE3), so a re-PUT writes identical bytes — overwrite-safe
    /// idempotency without the HEAD probe `put_blob_from_bytes` pays. The
    /// dedup layer already filtered out chunks the database knows about,
    /// so the probe was a pure extra round-trip on every NEW chunk of
    /// every upload (2 RTTs -> 1, benches/S3-PUT.md).
    ///
    /// Shares the body with `put_blob_from_bytes_replace` below —
    /// S3 PUT is durable on return, so "unsynced" and "replace"
    /// collapse to the same semantics here (unlike Local, where
    /// `_replace` needs tempfile-rename + fsync).
    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let key = Self::object_key(&hash);
            let size = data.len() as u64;
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(ByteStream::from(data))
                .send()
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "S3",
                        format!("Failed to upload blob {}: {}", hash, e),
                    )
                })?;
            Ok(size)
        })
    }

    /// Atomic overwrite path used by `backend_rotate` and
    /// `backend_migration` when re-writing an already-present blob
    /// under a new head key/format. Trait default delegates to
    /// `put_blob_from_bytes` which HEAD-probes and silently skips —
    /// exactly wrong for the rotate/migrate use case (the whole
    /// point is to replace the existing bytes). Override delegates
    /// to the same unconditional PUT as `put_blob_from_bytes_unsynced`
    /// — S3's PUT is durable on return, no separate sync barrier
    /// needed.
    fn put_blob_from_bytes_replace(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        self.put_blob_from_bytes_unsynced(hash, data)
    }

    fn get_blob_stream(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let hash = hash.to_owned();
        Box::pin(async move {
            let key = Self::object_key(&hash);

            let output = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    DomainError::new(
                        ErrorKind::NotFound,
                        "S3",
                        format!("Failed to get blob {}: {}", hash, e),
                    )
                })?;

            // Convert S3 ByteStream into a Stream<Item = Result<Bytes, io::Error>>
            // via AsyncRead adapter
            let reader = output.body.into_async_read();
            Ok(Box::pin(ReaderStream::with_capacity(reader, 256 * 1024)) as BlobStream)
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
            let key = Self::object_key(&hash);

            let range = match end {
                Some(end_pos) => format!("bytes={}-{}", start, end_pos.saturating_sub(1)),
                None => format!("bytes={}-", start),
            };

            let output = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .range(range)
                .send()
                .await
                .map_err(|e| {
                    DomainError::new(
                        ErrorKind::NotFound,
                        "S3",
                        format!("Failed to get blob range {}: {}", hash, e),
                    )
                })?;

            let reader = output.body.into_async_read();
            Ok(Box::pin(ReaderStream::with_capacity(reader, 256 * 1024)) as BlobStream)
        })
    }

    fn delete_blob(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let key = Self::object_key(&hash);

            // S3 DeleteObject is already idempotent (returns 204 even if not found)
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    DomainError::internal_error(
                        "S3",
                        format!("Failed to delete blob {}: {}", hash, e),
                    )
                })?;

            Ok(())
        })
    }

    fn blob_exists(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let key = Self::object_key(&hash);

            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
            {
                Ok(_) => Ok(true),
                Err(e) => {
                    // Check if it's a 404 (not found) vs an actual error
                    let service_err = e.into_service_error();
                    if service_err.is_not_found() {
                        Ok(false)
                    } else {
                        Err(DomainError::internal_error(
                            "S3",
                            format!("Failed to check blob {}: {}", hash, service_err),
                        ))
                    }
                }
            }
        })
    }

    fn blob_size(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let key = Self::object_key(&hash);

            let output = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    DomainError::new(
                        ErrorKind::NotFound,
                        "S3",
                        format!("Failed to stat blob {}: {}", hash, e),
                    )
                })?;

            Ok(output.content_length().unwrap_or(0) as u64)
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        Box::pin(async move {
            match self.client.head_bucket().bucket(&self.bucket).send().await {
                Ok(_) => Ok(StorageHealthStatus {
                    connected: true,
                    backend_type: "s3".to_string(),
                    message: format!("S3 bucket '{}' is accessible", self.bucket),
                    available_bytes: None,
                }),
                Err(e) => {
                    // The AWS SDK's `Display` impl on `SdkError` says
                    // just "service error" for anything the service
                    // returned. The real cause — signature mismatch,
                    // 301 redirect (wrong region), 403 (missing IAM),
                    // hostname unresolvable — lives on the wrapped
                    // `ServiceError` / `DispatchFailure` / raw response.
                    // Peel it apart so the admin UI + audit stream see
                    // the actionable message, not the tautology.
                    let detail = format_s3_error(&e);
                    tracing::warn!(
                        target: "audit",
                        event = "storage.s3.health_check_failed",
                        bucket = %self.bucket,
                        error = %detail,
                        error_debug = ?e,
                        "S3 health check on `{}` failed",
                        self.bucket,
                    );
                    Ok(StorageHealthStatus {
                        connected: false,
                        backend_type: "s3".to_string(),
                        message: format!("S3 bucket '{}' is not accessible: {detail}", self.bucket),
                        available_bytes: None,
                    })
                }
            }
        })
    }

    fn backend_type(&self) -> &'static str {
        "s3"
    }

    /// Remote object store: overlap chunk GETs to hide per-request latency.
    fn read_prefetch(&self) -> usize {
        8
    }

    fn local_blob_path(&self, _hash: &str) -> Option<PathBuf> {
        None // Remote backend — no local path
    }

    /// Enumerate blobs via S3 `ListObjectsV2`, in ascending hash order.
    ///
    /// The cursor is a **hash**, per the port contract — resumed via
    /// `StartAfter`, not a continuation token. That is what lets a caller
    /// resume the backend side of a merge-join from a checkpoint it
    /// already holds; a continuation token would force re-enumeration
    /// from the start on every resume.
    ///
    /// No prefix is passed to S3, so the scan covers the whole bucket and
    /// [`Self::hash_from_object_key`] does the filtering. Keys that are
    /// not ours come back as `unknowns` rather than being dropped, so an
    /// operator can see what is sharing the bucket. **On a bucket shared
    /// with other workloads that means every foreign object is reported
    /// as an unknown on every sweep** — give OxiCloud its own bucket, or
    /// expect the noise.
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

        Box::pin(async move {
            // A page's cursor can only be the last blob hash on it, because
            // the contract says the cursor IS a hash and `StartAfter` needs
            // `object_key()` applied to it. A page holding only foreign keys
            // therefore yields no cursor — and returning `None` there would
            // end enumeration while the bucket still has objects, making an
            // audit job under-report. That is the worst failure shape for a
            // check whose entire purpose is finding missing data.
            //
            // So keep listing until the accumulated page holds at least one
            // blob, or the bucket is exhausted. The continuation token is
            // used only INSIDE this call and never escapes as a cursor.
            // Bounded on foreign keys accumulated rather than on requests
            // made: the request count scales with the caller's `limit`, so a
            // request cap would fire on a healthy bucket merely because the
            // caller paged finely.
            const MAX_UNKNOWNS: usize = 10_000;

            let mut blobs: Vec<BackendBlobEntry> = Vec::new();
            let mut unknowns: Vec<BackendUnknownEntry> = Vec::new();
            let mut continuation: Option<String> = None;
            let mut requests = 0usize;
            // Assigned on every path through the loop body before any exit.
            let mut truncated;

            loop {
                let mut req = self
                    .client
                    .list_objects_v2()
                    .bucket(&self.bucket)
                    .max_keys(limit.min(1000) as i32);
                match (&continuation, &cursor) {
                    // Mid-loop: continue exactly where the last inner
                    // request stopped.
                    (Some(token), _) => req = req.continuation_token(token),
                    // First request: resume after the caller's hash.
                    (None, Some(c)) => req = req.start_after(Self::object_key(c)),
                    (None, None) => {}
                }

                let resp = req.send().await.map_err(|e| {
                    DomainError::new(
                        ErrorKind::InternalError,
                        "Blob",
                        format!("S3 ListObjectsV2 failed: {e}"),
                    )
                })?;

                requests += 1;
                truncated = resp.is_truncated.unwrap_or(false);
                continuation = resp.next_continuation_token;

                for obj in resp.contents.unwrap_or_default() {
                    let Some(key) = obj.key else { continue };
                    let mtime = obj.last_modified.and_then(|ts| {
                        chrono::DateTime::<chrono::Utc>::from_timestamp(
                            ts.secs(),
                            ts.subsec_nanos(),
                        )
                    });

                    match Self::hash_from_object_key(&key) {
                        Some(hash) => blobs.push(BackendBlobEntry { hash, mtime }),
                        // Not ours: a spool file, a sidecar, or another
                        // workload sharing the bucket. Surfaced rather than
                        // dropped so operators can see it; the recovery
                        // framework decides per pattern how to act.
                        None => unknowns.push(BackendUnknownEntry { path: key, mtime }),
                    }
                }

                if !blobs.is_empty() || !truncated {
                    break;
                }

                // `is_truncated` with no token is a protocol violation, and a
                // huge run of foreign keys means we would buffer the bucket to
                // find one blob. Neither can produce a valid cursor, so fail
                // loudly: a visible job failure beats a sweep that silently
                // reports "no missing blobs" having read a fraction of them.
                if continuation.is_none() || unknowns.len() >= MAX_UNKNOWNS {
                    return Err(DomainError::new(
                        ErrorKind::InternalError,
                        "Blob",
                        format!(
                            "S3 enumeration stalled after {requests} request(s) and {} \
                             non-blob key(s) without reaching a blob, so no resume cursor \
                             can be produced. Bucket '{}' likely holds a large foreign \
                             namespace — give OxiCloud a dedicated bucket.",
                            unknowns.len(),
                            self.bucket,
                        ),
                    ));
                }
            }

            // Always a real hash: the loop above only exits with an empty
            // `blobs` when the listing is exhausted, and then there is
            // nothing to resume from.
            let next_cursor = if truncated {
                blobs.last().map(|entry| entry.hash.clone())
            } else {
                None
            };

            Ok(BlobListPage {
                blobs,
                unknowns,
                next_cursor,
            })
        })
    }
}

/// Extract an actionable error string from an aws-sdk-s3 error.
///
/// `SdkError::Display` renders literally `"service error"` when the
/// service returned a structured error, which is worse than useless
/// in the admin UI. This helper walks the error chain and produces:
///
/// - `NotFound` — bucket doesn't exist (or the credentials can't see it).
/// - `<code>: <message>` — the S3-specific error code + message the
///   service returned (e.g. `InvalidAccessKeyId: The AWS Access Key
///   Id you provided does not exist`, `SignatureDoesNotMatch: The
///   request signature we calculated does not match the signature`,
///   `PermanentRedirect: The bucket you are attempting to access must
///   be addressed using the specified endpoint`).
/// - `network error: <cause>` — DNS / TLS / TCP dispatch failure.
/// - `timeout` — request timed out.
/// - `unknown SDK error: <debug>` — anything else, with the full
///   `Debug` output so the operator + audit stream see the real cause
///   instead of `"service error"`.
fn format_s3_error<E>(err: &aws_sdk_s3::error::SdkError<E>) -> String
where
    E: aws_sdk_s3::error::ProvideErrorMetadata + std::fmt::Debug,
{
    use aws_sdk_s3::error::SdkError;

    match err {
        SdkError::ServiceError(svc) => {
            let inner = svc.err();
            let meta = inner.meta();
            let code = meta.code().unwrap_or("<no-code>");
            let msg = meta.message().unwrap_or("");
            // A 404 for HeadBucket surfaces as `NotFound` on the
            // typed error — normalise the string so callers filter
            // on it easily.
            if code.eq_ignore_ascii_case("NotFound") || code == "404" {
                return "NotFound (bucket doesn't exist or no permission to see it)".to_string();
            }
            if msg.is_empty() {
                format!("{code} (HTTP {})", svc.raw().status().as_u16())
            } else {
                format!("{code}: {msg}")
            }
        }
        SdkError::DispatchFailure(d) => {
            // DNS / TLS / connection refused / TCP reset land here.
            if d.is_io() {
                format!("network I/O error: {:?}", d.as_connector_error())
            } else if d.is_timeout() {
                "network timeout during dispatch".to_string()
            } else if d.is_user() {
                format!("client-side dispatch failure: {d:?}")
            } else {
                format!("dispatch failure: {d:?}")
            }
        }
        SdkError::TimeoutError(_) => "timeout".to_string(),
        SdkError::ResponseError(r) => {
            format!(
                "malformed response (HTTP {}): {r:?}",
                r.raw().status().as_u16()
            )
        }
        SdkError::ConstructionFailure(c) => format!("request construction failed: {c:?}"),
        _ => format!("unknown SDK error: {err:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";

    /// The enumeration cursor is fed straight back into `object_key`, so a
    /// key that does not round-trip would resume at the wrong position.
    #[test]
    fn object_key_round_trips_through_hash_from_object_key() {
        let key = S3BlobBackend::object_key(H);
        assert_eq!(key, format!("0a/{H}.blob"));
        assert_eq!(
            S3BlobBackend::hash_from_object_key(&key).as_deref(),
            Some(H)
        );
    }

    /// Each of these previously risked being treated as a hash and sliced
    /// `[0..2]` to build a resume position.
    #[test]
    fn non_canonical_keys_are_rejected() {
        let cases = [
            "0a/junk.tmp".to_string(),        // spool file
            "junk.tmp".to_string(),           // no shard
            "0a/junk".to_string(),            // no suffix
            "thumbnails/abc.jpg".to_string(), // foreign namespace
            format!("0a/{H}.blob.corrupt"),   // sidecar
            format!("0a/{H}"),                // suffix missing
            format!("zz/{H}.blob"),           // non-hex shard
            format!("ff/{H}.blob"),           // shard != hash prefix
            format!("0a/{}.blob", &H[..63]),  // wrong length
        ];
        for key in &cases {
            assert_eq!(
                S3BlobBackend::hash_from_object_key(key),
                None,
                "must not be read as a blob: {key}"
            );
        }
    }
}
