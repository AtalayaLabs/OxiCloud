//! Azure Blob Storage Backend — stores blobs in an Azure Storage container.
//!
//! Authenticates via Account Name + Account Key (or SAS token).
//! Blob key scheme mirrors local/S3: `{2-char-prefix}/{hash}.blob`.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use azure_storage::StorageCredentials;
use azure_storage_blobs::prelude::*;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use tokio::fs;

use crate::application::ports::blob_storage_ports::{
    BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use crate::common::config::AzureStorageConfig;
use crate::domain::errors::{DomainError, ErrorKind};

/// Azure Blob Storage backend.
pub struct AzureBlobBackend {
    container_client: ContainerClient,
    container_name: String,
}

impl AzureBlobBackend {
    /// Build a new Azure backend from configuration.
    pub fn new(config: &AzureStorageConfig) -> Self {
        let credentials = if let Some(ref sas) = config.sas_token {
            StorageCredentials::sas_token(sas).expect("Invalid SAS token")
        } else {
            StorageCredentials::access_key(&config.account_name, config.account_key.clone())
        };

        // Custom endpoint (Azurite emulator / private deployment /
        // benches) mirrors S3's `endpoint_url`; default is the public
        // cloud URL derived from the account name.
        let container_client = match &config.endpoint_url {
            Some(uri) => ClientBuilder::with_location(
                azure_storage::CloudLocation::Custom {
                    account: config.account_name.clone(),
                    uri: uri.trim_end_matches('/').to_string(),
                },
                credentials,
            )
            .container_client(&config.container),
            None => ClientBuilder::new(&config.account_name, credentials)
                .container_client(&config.container),
        };

        Self {
            container_client,
            container_name: config.container.clone(),
        }
    }

    /// Compute the blob name for a given hash.
    fn blob_name(hash: &str) -> String {
        let prefix = &hash[0..2];
        format!("{prefix}/{hash}.blob")
    }

    /// Get a `BlobClient` for a given hash.
    fn blob_client(&self, hash: &str) -> BlobClient {
        self.container_client.blob_client(Self::blob_name(hash))
    }
}

impl BlobStorageBackend for AzureBlobBackend {
    fn initialize(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        Box::pin(async move {
            // Verify container exists by getting its properties
            self.container_client.get_properties().await.map_err(|e| {
                DomainError::internal_error(
                    "Azure",
                    format!("Cannot access container '{}': {}", self.container_name, e),
                )
            })?;

            tracing::info!(
                "Azure blob backend initialized: container={}",
                self.container_name
            );
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
            let client = self.blob_client(&hash);

            // Check if blob already exists (idempotent)
            if client.get_properties().await.is_ok() {
                let file_size = fs::metadata(&source_path)
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "Azure",
                            format!("Failed to stat source file: {e}"),
                        )
                    })?
                    .len();
                let _ = fs::remove_file(&source_path).await;
                return Ok(file_size);
            }

            // Read file and upload as block blob
            let data = fs::read(&source_path).await.map_err(|e| {
                DomainError::internal_error("Azure", format!("Failed to read source: {e}"))
            })?;
            let file_size = data.len() as u64;

            client.put_block_blob(data).await.map_err(|e| {
                DomainError::internal_error("Azure", format!("Failed to upload blob {hash}: {e}"))
            })?;

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
            let client = self.blob_client(&hash);
            let size = data.len() as u64;

            // Idempotent: skip if exists
            if client.get_properties().await.is_ok() {
                return Ok(size);
            }

            // `Bytes` converts into `azure_core::Body` by reference count —
            // the old `data.to_vec()` copied every chunk once more.
            client.put_block_blob(data).await.map_err(|e| {
                DomainError::internal_error("Azure", format!("Failed to upload blob {hash}: {e}"))
            })?;

            Ok(size)
        })
    }

    /// Dedup settle path: PUT unconditionally. Content-addressed keys make
    /// re-PUTs idempotent, so the `get_properties` probe
    /// `put_blob_from_bytes` pays is a pure extra round-trip on every NEW
    /// chunk (2 RTTs -> 1, benches/S3-PUT.md — same shape as S3).
    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);
            let size = data.len() as u64;
            client.put_block_blob(data).await.map_err(|e| {
                DomainError::internal_error("Azure", format!("Failed to upload blob {hash}: {e}"))
            })?;
            Ok(size)
        })
    }

    /// Atomic overwrite path used by `backend_rotate` and
    /// `backend_migration` when re-writing an already-present blob
    /// under a new head key/format. Trait default delegates to
    /// `put_blob_from_bytes` which `get_properties`-probes and
    /// silently skips — exactly wrong for the rotate/migrate use
    /// case (the whole point is to replace the existing bytes).
    /// Override delegates to the same unconditional PUT as
    /// `put_blob_from_bytes_unsynced` — Azure's PUT is durable on
    /// return, no separate sync barrier needed.
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
            let client = self.blob_client(&hash);

            // The old implementation drained the ENTIRE blob into one
            // `Vec<u8>` before yielding a single mega-chunk — whole-blob
            // RAM residency per reader, and with `read_prefetch() = 8`
            // up to 8 entire chunk-blobs resident at once during CDC
            // reassembly. Now the SDK's page/body streams forward
            // directly. The FIRST page is still awaited eagerly so a
            // missing blob surfaces as the same up-front NotFound the
            // old code produced; later pages/chunks map to io::Error
            // items like every other backend's stream.
            let mut pages = client.get().into_stream();
            let first = match pages.next().await {
                Some(Ok(response)) => response,
                Some(Err(e)) => {
                    return Err(DomainError::new(
                        ErrorKind::NotFound,
                        "Azure",
                        format!("Failed to get blob {hash}: {e}"),
                    ));
                }
                None => {
                    let empty: BlobStream =
                        Box::pin(futures::stream::once(async move { Ok(Bytes::new()) }));
                    return Ok(empty);
                }
            };

            let first_body = first.data.map(|chunk| {
                chunk.map_err(|e| std::io::Error::other(format!("Stream read error: {e}")))
            });
            let tail = pages
                .map(|page| match page {
                    Ok(response) => Ok(response.data.map(|chunk| {
                        chunk.map_err(|e| std::io::Error::other(format!("Stream read error: {e}")))
                    })),
                    Err(e) => Err(std::io::Error::other(format!(
                        "Failed to get blob page: {e}"
                    ))),
                })
                .try_flatten();
            let stream: BlobStream = Box::pin(first_body.chain(tail));
            Ok(stream)
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
            let client = self.blob_client(&hash);

            let range = match end {
                Some(e) => azure_core::request_options::Range::new(start, e),
                None => azure_core::request_options::Range::new(start, u64::MAX),
            };

            // Same forwarding shape as `get_blob_stream` — a ranged read
            // doubly so: the caller explicitly asked NOT to pay for the
            // whole blob, yet the old code buffered the full range.
            let mut pages = client.get().range(range).into_stream();
            let first = match pages.next().await {
                Some(Ok(response)) => response,
                Some(Err(e)) => {
                    return Err(DomainError::new(
                        ErrorKind::NotFound,
                        "Azure",
                        format!("Failed to get blob range {hash}: {e}"),
                    ));
                }
                None => {
                    let empty: BlobStream =
                        Box::pin(futures::stream::once(async move { Ok(Bytes::new()) }));
                    return Ok(empty);
                }
            };

            let first_body = first.data.map(|chunk| {
                chunk.map_err(|e| std::io::Error::other(format!("Stream range read error: {e}")))
            });
            let tail = pages
                .map(|page| match page {
                    Ok(response) => Ok(response.data.map(|chunk| {
                        chunk.map_err(|e| {
                            std::io::Error::other(format!("Stream range read error: {e}"))
                        })
                    })),
                    Err(e) => Err(std::io::Error::other(format!(
                        "Failed to get blob range page: {e}"
                    ))),
                })
                .try_flatten();
            let stream: BlobStream = Box::pin(first_body.chain(tail));
            Ok(stream)
        })
    }

    fn delete_blob(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);

            // Azure delete is not fully idempotent — 404 is expected for missing blobs
            match client.delete().await {
                Ok(_) => Ok(()),
                Err(e) => {
                    // If 404, treat as success (idempotent)
                    let status = e.as_http_error().map(|h| h.status());
                    if status == Some(azure_core::StatusCode::NotFound) {
                        Ok(())
                    } else {
                        Err(DomainError::internal_error(
                            "Azure",
                            format!("Failed to delete blob {hash}: {e}"),
                        ))
                    }
                }
            }
        })
    }

    fn blob_exists(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);
            match client.get_properties().await {
                Ok(_) => Ok(true),
                Err(e) => {
                    let status = e.as_http_error().map(|h| h.status());
                    if status == Some(azure_core::StatusCode::NotFound) {
                        Ok(false)
                    } else {
                        Err(DomainError::internal_error(
                            "Azure",
                            format!("Failed to check blob {hash}: {e}"),
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
            let client = self.blob_client(&hash);
            let props = client.get_properties().await.map_err(|e| {
                DomainError::new(
                    ErrorKind::NotFound,
                    "Azure",
                    format!("Failed to stat blob {hash}: {e}"),
                )
            })?;
            Ok(props.blob.properties.content_length)
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        Box::pin(async move {
            match self.container_client.get_properties().await {
                Ok(_) => Ok(StorageHealthStatus {
                    connected: true,
                    backend_type: "azure".to_string(),
                    message: format!("Azure container '{}' is accessible", self.container_name),
                    available_bytes: None,
                }),
                Err(e) => Ok(StorageHealthStatus {
                    connected: false,
                    backend_type: "azure".to_string(),
                    message: format!(
                        "Azure container '{}' is not accessible: {}",
                        self.container_name, e
                    ),
                    available_bytes: None,
                }),
            }
        })
    }

    fn backend_type(&self) -> &'static str {
        "azure"
    }

    /// Remote object store: overlap chunk GETs to hide per-request latency.
    fn read_prefetch(&self) -> usize {
        8
    }

    fn local_blob_path(&self, _hash: &str) -> Option<PathBuf> {
        None
    }

    // TODO: implement `list_blob_hashes` via
    // `container_client.list_blobs()` (`azure_storage_blobs`
    // paginator). Same filter as local + S3 impls:
    // `<xx>/<64-hex>.blob` naming. Currently inherits the trait
    // default which returns `operation_not_supported` — the
    // `backend_consistency` tenant handles that by emitting a
    // single run-level `backend_unenumerable` finding and
    // completing without per-blob probes. Ship as a follow-up once
    // there's an Azure test environment to validate against.
}
