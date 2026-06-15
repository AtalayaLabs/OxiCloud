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
                Err(e) => Ok(StorageHealthStatus {
                    connected: false,
                    backend_type: "s3".to_string(),
                    message: format!("S3 bucket '{}' is not accessible: {}", self.bucket, e),
                    available_bytes: None,
                }),
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
}
