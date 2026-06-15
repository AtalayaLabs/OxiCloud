//! Azure Blob Storage Backend — stores blobs in an Azure Storage container.
//!
//! Authenticates via Account Name + Account Key (or SAS token).
//! Blob key scheme mirrors local/S3: `{2-char-prefix}/{hash}.blob`.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use azure_storage::StorageCredentials;
use azure_storage_blobs::prelude::*;
use bytes::Bytes;
use futures::StreamExt;
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

        let container_client = ClientBuilder::new(&config.account_name, credentials)
            .container_client(&config.container);

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

            client.put_block_blob(data.to_vec()).await.map_err(|e| {
                DomainError::internal_error("Azure", format!("Failed to upload blob {hash}: {e}"))
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
            let client = self.blob_client(&hash);

            let mut result_data: Vec<u8> = Vec::new();
            let mut stream = client.get().into_stream();

            while let Some(response) = stream.next().await {
                let response = response.map_err(|e| {
                    DomainError::new(
                        ErrorKind::NotFound,
                        "Azure",
                        format!("Failed to get blob {hash}: {e}"),
                    )
                })?;
                let mut body = response.data;
                while let Some(chunk) = body.next().await {
                    let chunk = chunk.map_err(|e| {
                        DomainError::internal_error("Azure", format!("Stream read error: {e}"))
                    })?;
                    result_data.extend_from_slice(&chunk);
                }
            }

            let stream: BlobStream = Box::pin(futures::stream::once(async move {
                Ok(Bytes::from(result_data))
            }));
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

            let mut result_data: Vec<u8> = Vec::new();
            let mut stream = client.get().range(range).into_stream();

            while let Some(response) = stream.next().await {
                let response = response.map_err(|e| {
                    DomainError::new(
                        ErrorKind::NotFound,
                        "Azure",
                        format!("Failed to get blob range {hash}: {e}"),
                    )
                })?;
                let mut body = response.data;
                while let Some(chunk) = body.next().await {
                    let chunk = chunk.map_err(|e| {
                        DomainError::internal_error(
                            "Azure",
                            format!("Stream range read error: {e}"),
                        )
                    })?;
                    result_data.extend_from_slice(&chunk);
                }
            }

            let stream: BlobStream = Box::pin(futures::stream::once(async move {
                Ok(Bytes::from(result_data))
            }));
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
}
