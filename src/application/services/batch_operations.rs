use async_zip::base::write::ZipFileWriter;
use async_zip::{Compression, ZipEntryBuilder};
use futures::io::AsyncWriteExt as FuturesWriteExt;
use futures::{Future, StreamExt, stream};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::io::BufWriter;
use tracing::info;

use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::{FolderDto, MoveFolderDto};
use crate::application::ports::file_ports::{FileManagementUseCase, FileRetrievalUseCase};
use crate::application::ports::inbound::FolderUseCase;
use crate::application::ports::trash_ports::TrashUseCase;
use crate::application::services::file_management_service::FileManagementService;
use crate::application::services::file_retrieval_service::FileRetrievalService;
use crate::application::services::folder_service::FolderService;
use crate::application::services::trash_service::TrashService;
use crate::common::config::AppConfig;
use crate::common::errors::DomainError;
use uuid::Uuid;

/// Specific errors for batch operations
#[derive(Debug, Error)]
pub enum BatchOperationError {
    #[error("Domain error: {0}")]
    Domain(#[from] DomainError),

    #[error("Operation cancelled: {0}")]
    Cancelled(String),

    #[error("Concurrency limit exceeded: {0}")]
    ConcurrencyLimit(String),

    #[error("Batch operation error: {0} ({1} of {2} completed)")]
    PartialFailure(String, usize, usize),

    #[error("Internal error: {0}")]
    Internal(String),
}

/// Result of a batch operation with statistics
#[derive(Debug, Clone)]
pub struct BatchResult<T> {
    /// Successful results
    pub successful: Vec<T>,
    /// Failed operations with their errors
    pub failed: Vec<(String, String)>,
    /// Operation statistics
    pub stats: BatchStats,
}

/// Statistics of a batch operation
#[derive(Debug, Clone, Default)]
pub struct BatchStats {
    /// Total number of operations
    pub total: usize,
    /// Number of successful operations
    pub successful: usize,
    /// Number of failed operations
    pub failed: usize,
    /// Total execution time in milliseconds
    pub execution_time_ms: u128,
    /// Maximum concurrency reached
    pub max_concurrency: usize,
}

/// Batch operations service
pub struct BatchOperationService {
    file_retrieval: Arc<FileRetrievalService>,
    file_management: Arc<FileManagementService>,
    folder_service: Arc<FolderService>,
    trash_service: Option<Arc<TrashService>>,
    config: AppConfig,
}

impl BatchOperationService {
    /// Creates a new instance of the batch operations service
    pub fn new(
        file_retrieval: Arc<FileRetrievalService>,
        file_management: Arc<FileManagementService>,
        folder_service: Arc<FolderService>,
        config: AppConfig,
    ) -> Self {
        Self {
            file_retrieval,
            file_management,
            folder_service,
            trash_service: None,
            config,
        }
    }

    /// Creates a new instance with default configuration
    pub fn default(
        file_retrieval: Arc<FileRetrievalService>,
        file_management: Arc<FileManagementService>,
        folder_service: Arc<FolderService>,
    ) -> Self {
        Self::new(
            file_retrieval,
            file_management,
            folder_service,
            AppConfig::default(),
        )
    }

    /// Set the optional trash service (enables batch trash operations)
    pub fn with_trash_service(mut self, trash_service: Arc<TrashService>) -> Self {
        self.trash_service = Some(trash_service);
        self
    }

    /// Copies multiple files in parallel
    pub async fn copy_files(
        &self,
        file_ids: Vec<String>,
        target_folder_id: Option<String>,
        user_id: Uuid,
    ) -> Result<BatchResult<FileDto>, BatchOperationError> {
        info!("Starting batch copy of {} files", file_ids.len());
        let start_time = std::time::Instant::now();
        let max_concurrent = self.config.concurrency.max_concurrent_files;

        // Create result structure
        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: file_ids.len(),
                ..Default::default()
            },
        };

        // Arc<str> avoids N heap-clones of the same string
        let target_folder: Option<Arc<str>> = target_folder_id.map(|s| Arc::from(s.as_str()));

        // buffer_unordered materialises only max_concurrent futures at a time
        let mut operation_stream = stream::iter(file_ids.into_iter().map(|file_id| {
            let mgmt = self.file_management.clone();
            let target_folder = target_folder.clone();

            async move {
                let copy_result = mgmt
                    .copy_file_owned(&file_id, user_id, target_folder.map(|s| s.to_string()))
                    .await;
                (file_id, copy_result)
            }
        }))
        .buffer_unordered(max_concurrent);

        // Process results as they complete
        while let Some((file_id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(file) => {
                    result.successful.push(file);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    result.failed.push((file_id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        // Complete statistics
        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch copy completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Moves multiple files in parallel
    pub async fn move_files(
        &self,
        file_ids: Vec<String>,
        target_folder_id: Option<String>,
        user_id: Uuid,
    ) -> Result<BatchResult<FileDto>, BatchOperationError> {
        info!("Starting batch move of {} files", file_ids.len());
        let start_time = std::time::Instant::now();
        let max_concurrent = self.config.concurrency.max_concurrent_files;

        // Create result structure
        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: file_ids.len(),
                ..Default::default()
            },
        };

        let target_folder: Option<Arc<str>> = target_folder_id.map(|s| Arc::from(s.as_str()));

        let mut operation_stream = stream::iter(file_ids.into_iter().map(|file_id| {
            let mgmt = self.file_management.clone();
            let target_folder = target_folder.clone();

            async move {
                let move_result = mgmt
                    .move_file_owned(&file_id, user_id, target_folder.map(|s| s.to_string()))
                    .await;
                (file_id, move_result)
            }
        }))
        .buffer_unordered(max_concurrent);

        while let Some((file_id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(file) => {
                    result.successful.push(file);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    result.failed.push((file_id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        // Complete statistics
        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch move completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Deletes multiple files in parallel
    pub async fn delete_files(
        &self,
        file_ids: Vec<String>,
        user_id: Uuid,
    ) -> Result<BatchResult<String>, BatchOperationError> {
        info!("Starting batch deletion of {} files", file_ids.len());
        let start_time = std::time::Instant::now();

        // Create result structure
        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: file_ids.len(),
                ..Default::default()
            },
        };

        let mut operation_stream = stream::iter(file_ids.into_iter().map(|file_id| {
            let mgmt = self.file_management.clone();

            async move {
                let delete_result = mgmt.delete_file_owned(&file_id, user_id).await;
                let id_for_result = file_id.clone();
                (file_id, delete_result.map(|_| id_for_result))
            }
        }))
        .buffer_unordered(self.config.concurrency.max_concurrent_files);

        // Process results as they complete
        while let Some((file_id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(id) => {
                    result.successful.push(id);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    result.failed.push((file_id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        // Complete statistics
        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch deletion completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Loads multiple files in parallel (data in memory)
    pub async fn get_multiple_files(
        &self,
        file_ids: Vec<String>,
        user_id: Uuid,
    ) -> Result<BatchResult<FileDto>, BatchOperationError> {
        info!("Starting batch load of {} files", file_ids.len());
        let start_time = std::time::Instant::now();

        // Create result structure
        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: file_ids.len(),
                ..Default::default()
            },
        };

        let mut operation_stream = stream::iter(file_ids.into_iter().map(|file_id| {
            let retrieval = self.file_retrieval.clone();

            async move {
                let get_result = retrieval.get_file_owned(&file_id, user_id).await;
                (file_id, get_result)
            }
        }))
        .buffer_unordered(self.config.concurrency.max_concurrent_files);

        // Process results as they complete
        while let Some((file_id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(file) => {
                    result.successful.push(file);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    result.failed.push((file_id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        // Complete statistics
        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch load completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Deletes multiple folders in parallel
    pub async fn delete_folders(
        &self,
        folder_ids: Vec<String>,
        _recursive: bool,
        user_id: Uuid,
    ) -> Result<BatchResult<String>, BatchOperationError> {
        info!("Starting batch deletion of {} folders", folder_ids.len());
        let start_time = std::time::Instant::now();

        // Create result structure
        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: folder_ids.len(),
                ..Default::default()
            },
        };

        let mut operation_stream = stream::iter(folder_ids.into_iter().map(|folder_id| {
            let folder_service = self.folder_service.clone();

            async move {
                let delete_result = folder_service.delete_folder(&folder_id, user_id).await;
                let id_for_result = folder_id.clone();
                (folder_id, delete_result.map(|_| id_for_result))
            }
        }))
        .buffer_unordered(self.config.concurrency.max_concurrent_files);

        while let Some((folder_id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(id) => {
                    result.successful.push(id);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    result.failed.push((folder_id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        // Complete statistics
        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch folder deletion completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Moves multiple files to trash in parallel (soft delete)
    pub async fn trash_files(
        &self,
        file_ids: Vec<String>,
        user_id: Uuid,
    ) -> Result<BatchResult<String>, BatchOperationError> {
        let trash_service = self
            .trash_service
            .as_ref()
            .ok_or_else(|| BatchOperationError::Internal("Trash service not available".into()))?;

        info!("Starting batch trash of {} files", file_ids.len());
        let start_time = std::time::Instant::now();

        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: file_ids.len(),
                ..Default::default()
            },
        };

        let mut operation_stream = stream::iter(file_ids.into_iter().map(|file_id| {
            let trash = trash_service.clone();
            let uid = user_id;

            async move {
                let trash_result = trash.move_to_trash(&file_id, "file", uid).await;
                let id_for_result = file_id.clone();
                (file_id, trash_result.map(|_| id_for_result))
            }
        }))
        .buffer_unordered(self.config.concurrency.max_concurrent_files);

        while let Some((file_id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(id) => {
                    result.successful.push(id);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    tracing::debug!("Failed to trash file {}: {}", file_id, e);
                    result.failed.push((file_id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch trash files completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Moves multiple folders to trash in parallel (soft delete)
    pub async fn trash_folders(
        &self,
        folder_ids: Vec<String>,
        user_id: Uuid,
    ) -> Result<BatchResult<String>, BatchOperationError> {
        let trash_service = self
            .trash_service
            .as_ref()
            .ok_or_else(|| BatchOperationError::Internal("Trash service not available".into()))?;

        info!("Starting batch trash of {} folders", folder_ids.len());
        let start_time = std::time::Instant::now();

        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: folder_ids.len(),
                ..Default::default()
            },
        };

        let mut operation_stream = stream::iter(folder_ids.into_iter().map(|folder_id| {
            let trash = trash_service.clone();
            let uid = user_id;

            async move {
                let trash_result = trash.move_to_trash(&folder_id, "folder", uid).await;
                let id_for_result = folder_id.clone();
                (folder_id, trash_result.map(|_| id_for_result))
            }
        }))
        .buffer_unordered(self.config.concurrency.max_concurrent_files);

        while let Some((folder_id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(id) => {
                    result.successful.push(id);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    tracing::debug!("Failed to trash folder {}: {}", folder_id, e);
                    result.failed.push((folder_id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch trash folders completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Moves multiple folders to a target parent in parallel
    pub async fn move_folders(
        &self,
        folder_ids: Vec<String>,
        target_folder_id: Option<String>,
        user_id: Uuid,
    ) -> Result<BatchResult<FolderDto>, BatchOperationError> {
        info!("Starting batch move of {} folders", folder_ids.len());
        let start_time = std::time::Instant::now();

        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: folder_ids.len(),
                ..Default::default()
            },
        };

        let target: Option<Arc<str>> = target_folder_id.map(|s| Arc::from(s.as_str()));

        let mut operation_stream = stream::iter(folder_ids.into_iter().map(|folder_id| {
            let folder_service = self.folder_service.clone();
            let target = target.clone();

            async move {
                let dto = MoveFolderDto {
                    parent_id: target.map(|s| s.to_string()),
                };
                let move_result = folder_service.move_folder(&folder_id, dto, user_id).await;
                (folder_id, move_result)
            }
        }))
        .buffer_unordered(self.config.concurrency.max_concurrent_files);

        while let Some((folder_id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(folder) => {
                    result.successful.push(folder);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    result.failed.push((folder_id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch folder move completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Downloads multiple files/folders as a single ZIP archive.
    ///
    /// Writes the archive to a temporary file so RAM usage is O(buffer_size)
    /// regardless of total archive size.  The caller streams the resulting
    /// `NamedTempFile` to the client; the OS deletes it on drop.
    pub async fn download_zip(
        &self,
        file_ids: Vec<String>,
        folder_ids: Vec<String>,
        user_id: Uuid,
    ) -> Result<NamedTempFile, BatchOperationError> {
        info!(
            "Starting batch download: {} files, {} folders",
            file_ids.len(),
            folder_ids.len()
        );
        let start_time = std::time::Instant::now();

        // ── Open temp file + async ZIP writer (all writes go to disk) ────
        let temp = NamedTempFile::new()
            .map_err(|e| BatchOperationError::Internal(format!("temp file error: {}", e)))?;
        let tokio_file = tokio::fs::File::create(temp.path())
            .await
            .map_err(|e| BatchOperationError::Internal(format!("temp file open: {}", e)))?;
        let buf_writer = BufWriter::with_capacity(256 * 1024, tokio_file);
        let mut zip = ZipFileWriter::with_tokio(buf_writer);

        // ── Add individual files at the root of the ZIP ──────────────────
        for file_id in &file_ids {
            match self.file_retrieval.get_file_owned(file_id, user_id).await {
                Ok(file_dto) => {
                    if let Err(e) = self
                        .add_file_entry_streamed(&mut zip, file_id, &file_dto.name, user_id)
                        .await
                    {
                        info!("Could not add file {} to ZIP: {}", file_dto.name, e);
                    }
                }
                Err(e) => {
                    info!("Could not get file metadata {}: {}", file_id, e);
                }
            }
        }

        // ── Add folders as sub-trees (bulk subtree queries, not N+1) ─────
        for folder_id in &folder_ids {
            match self
                .folder_service
                .get_folder_owned(folder_id, user_id)
                .await
            {
                Ok(root_folder) => {
                    if let Err(e) = self
                        .add_folder_subtree_to_zip(&mut zip, folder_id, &root_folder, user_id)
                        .await
                    {
                        info!("Could not add folder {} to ZIP: {}", root_folder.name, e);
                    }
                }
                Err(e) => {
                    info!("Could not get folder {}: {}", folder_id, e);
                }
            }
        }

        // ── Finalize ─────────────────────────────────────────────────────
        let mut compat_writer = zip
            .close()
            .await
            .map_err(|e| BatchOperationError::Internal(format!("ZIP finalize error: {}", e)))?;
        compat_writer
            .close()
            .await
            .map_err(|e| BatchOperationError::Internal(format!("ZIP flush error: {}", e)))?;

        let file_size = temp.as_file().metadata().map(|m| m.len()).unwrap_or(0);

        info!(
            "Batch download ZIP created: {} bytes in {}ms",
            file_size,
            start_time.elapsed().as_millis()
        );

        Ok(temp)
    }

    /// Streams a single file into an async ZIP entry (~64 KB peak RAM per file).
    async fn add_file_entry_streamed(
        &self,
        zip: &mut ZipFileWriter<tokio_util::compat::Compat<BufWriter<tokio::fs::File>>>,
        file_id: &str,
        entry_name: &str,
        caller_id: Uuid,
    ) -> Result<(), BatchOperationError> {
        let entry = ZipEntryBuilder::new(entry_name.to_string().into(), Compression::Deflate);
        let mut writer = zip
            .write_entry_stream(entry)
            .await
            .map_err(|e| BatchOperationError::Internal(format!("zip entry start: {}", e)))?;

        let stream = self
            .file_retrieval
            .get_file_stream_owned(file_id, caller_id)
            .await
            .map_err(BatchOperationError::Domain)?;
        let mut stream = std::pin::Pin::from(stream);

        while let Some(chunk) = stream.next().await {
            let bytes =
                chunk.map_err(|e| BatchOperationError::Internal(format!("stream read: {}", e)))?;
            writer
                .write_all(&bytes)
                .await
                .map_err(|e| BatchOperationError::Internal(format!("zip chunk write: {}", e)))?;
        }

        writer
            .close()
            .await
            .map_err(|e| BatchOperationError::Internal(format!("zip entry close: {}", e)))?;
        Ok(())
    }

    /// Adds an entire folder subtree to the ZIP using 2 bulk SQL queries
    /// (ltree `<@`) instead of N+1 per-folder traversal.
    ///
    /// Files are streamed from the DB cursor — RAM for the file list is
    /// proportional to the number of *folders* (HashMap keys), not files.
    async fn add_folder_subtree_to_zip(
        &self,
        zip: &mut ZipFileWriter<tokio_util::compat::Compat<BufWriter<tokio::fs::File>>>,
        folder_id: &str,
        root_folder: &FolderDto,
        caller_id: Uuid,
    ) -> Result<(), BatchOperationError> {
        // Bulk-fetch folder tree (small — one entry per folder)
        let all_folders = self
            .folder_service
            .list_subtree_folders(folder_id)
            .await
            .map_err(BatchOperationError::Domain)?;

        // Stream files from DB cursor — O(1) per row
        let mut file_stream = self
            .file_retrieval
            .stream_files_in_subtree(folder_id)
            .await
            .map_err(BatchOperationError::Domain)?;

        // Group files by folder_id incrementally from the stream
        let mut files_by_folder: HashMap<String, Vec<FileDto>> =
            HashMap::with_capacity(all_folders.len());
        while let Some(file) = file_stream.next().await {
            let file = file.map_err(BatchOperationError::Domain)?;
            let fid = file.folder_id.clone().unwrap_or_default();
            files_by_folder.entry(fid).or_default().push(file);
        }

        // Build path mapping: folder_id → ZIP-relative path
        let root_path = root_folder.path.trim_end_matches('/');
        let folder_zip_path = |db_path: &str| -> String {
            let db_path = db_path.trim_end_matches('/');
            if db_path == root_path {
                root_folder.name.clone()
            } else {
                let suffix = db_path
                    .strip_prefix(root_path)
                    .unwrap_or(db_path)
                    .trim_start_matches('/');
                format!("{}/{}", root_folder.name, suffix)
            }
        };

        // Write folder + file entries (folders are sorted by path from DB)
        for folder in &all_folders {
            let zip_dir = format!("{}/", folder_zip_path(&folder.path));

            // Directory entry (Stored, zero-length body)
            let dir_entry = ZipEntryBuilder::new(zip_dir.clone().into(), Compression::Stored);
            if let Err(e) = zip.write_entry_whole(dir_entry, &[]).await {
                info!("Could not add folder entry {}: {}", zip_dir, e);
            }

            // Stream files belonging to this folder
            if let Some(files) = files_by_folder.get(&folder.id) {
                for file in files {
                    let file_path = format!("{}{}", zip_dir, file.name);
                    if let Err(e) = self
                        .add_file_entry_streamed(zip, &file.id, &file_path, caller_id)
                        .await
                    {
                        info!("Could not add file {} to ZIP: {}", file.name, e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Generic batch operation for any type of async function
    pub async fn generic_batch_operation<T, F, Fut>(
        &self,
        items: Vec<T>,
        operation: F,
    ) -> Result<BatchResult<T>, BatchOperationError>
    where
        T: Clone + Send + 'static + std::fmt::Debug,
        F: Fn(T) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Result<T, DomainError>> + Send + 'static,
    {
        info!(
            "Starting generic batch operation with {} items",
            items.len()
        );
        let start_time = std::time::Instant::now();

        // Create result structure
        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: items.len(),
                ..Default::default()
            },
        };

        // buffer_unordered materialises only max_concurrent futures at a time
        let mut operation_stream = stream::iter(items.into_iter().map(|item| {
            let op = operation.clone();

            async move {
                let op_result = op(item.clone()).await;
                (item, op_result)
            }
        }))
        .buffer_unordered(self.config.concurrency.max_concurrent_files);

        // Process results as they complete
        while let Some((item, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(result_item) => {
                    result.successful.push(result_item);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    result.failed.push((format!("{:?}", item), e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        // Complete statistics
        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Generic batch operation completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Create multiple folders in parallel
    pub async fn create_folders(
        &self,
        folders: Vec<(String, Option<String>)>, // (name, parent_id)
        user_id: Uuid,
    ) -> Result<BatchResult<FolderDto>, BatchOperationError> {
        info!("Starting batch creation of {} folders", folders.len());
        let start_time = std::time::Instant::now();

        // Create result structure
        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: folders.len(),
                ..Default::default()
            },
        };

        let mut operation_stream = stream::iter(folders.into_iter().map(|(name, parent_id)| {
            let folder_service = self.folder_service.clone();

            async move {
                // If a parent is specified, verify the caller owns it
                if let Some(ref pid) = parent_id
                    && let Err(e) = folder_service.get_folder_owned(pid, user_id).await
                {
                    let id = format!("{}:{}", name, pid);
                    return (id, Err(e));
                }
                let dto = crate::application::dtos::folder_dto::CreateFolderDto {
                    name: name.clone(),
                    parent_id: parent_id.clone(),
                };
                let create_result = folder_service.create_folder(dto).await;
                let id = format!("{}:{}", name, parent_id.unwrap_or_default());
                (id, create_result)
            }
        }))
        .buffer_unordered(self.config.concurrency.max_concurrent_files);

        // Process results as they complete
        while let Some((id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(folder) => {
                    result.successful.push(folder);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    result.failed.push((id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        // Complete statistics
        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch folder creation completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }

    /// Get metadata of multiple folders in parallel
    pub async fn get_multiple_folders(
        &self,
        folder_ids: Vec<String>,
        user_id: Uuid,
    ) -> Result<BatchResult<FolderDto>, BatchOperationError> {
        info!("Starting batch load of {} folders", folder_ids.len());
        let start_time = std::time::Instant::now();

        // Create result structure
        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: folder_ids.len(),
                ..Default::default()
            },
        };

        let mut operation_stream = stream::iter(folder_ids.into_iter().map(|folder_id| {
            let folder_service = self.folder_service.clone();

            async move {
                let get_result = folder_service.get_folder_owned(&folder_id, user_id).await;
                (folder_id, get_result)
            }
        }))
        .buffer_unordered(self.config.concurrency.max_concurrent_files);

        // Process results as they complete
        while let Some((folder_id, operation_result)) = operation_stream.next().await {
            match operation_result {
                Ok(folder) => {
                    result.successful.push(folder);
                    result.stats.successful += 1;
                }
                Err(e) => {
                    result.failed.push((folder_id, e.to_string()));
                    result.stats.failed += 1;
                }
            }
        }

        // Complete statistics
        result.stats.execution_time_ms = start_time.elapsed().as_millis();
        result.stats.max_concurrency = self
            .config
            .concurrency
            .max_concurrent_files
            .min(result.stats.total);

        info!(
            "Batch folder load completed: {}/{} successful in {}ms",
            result.stats.successful, result.stats.total, result.stats.execution_time_ms
        );

        Ok(result)
    }
}

#[cfg(integration_tests)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
    #[allow(unused_imports)]
    use crate::infrastructure::repositories::pg::file_blob_write_repository::FileBlobWriteRepository;
    #[allow(unused_imports)]
    use crate::infrastructure::repositories::pg::folder_db_repository::FolderDbRepository;
    #[allow(unused_imports)]
    use std::sync::Arc;

    #[tokio::test]
    async fn test_generic_batch_operation() {
        // Create the batch service with stub repositories (lazy pool — no SQL is executed
        // in this test; generic_batch_operation never touches file/folder services).
        let folder_repo = Arc::new(FolderDbRepository::new_stub());
        let file_read_repo = Arc::new(FileBlobReadRepository::new_stub());
        let file_write_repo = Arc::new(FileBlobWriteRepository::new_stub());
        let batch_service = BatchOperationService::new(
            Arc::new(FileRetrievalService::new(file_read_repo)),
            Arc::new(FileManagementService::new(file_write_repo)),
            Arc::new(FolderService::new(folder_repo, None)),
            AppConfig::default(),
        );

        // Define a generic test operation (no more semaphore parameter)
        let operation = |item: i32| async move {
            if item % 2 == 0 {
                // Simulate success for even numbers
                Ok(item * 2)
            } else {
                // Simulate error for odd numbers
                Err(DomainError::validation_error("Odd number not allowed"))
            }
        };

        // Execute the batch operation
        let items = vec![1, 2, 3, 4, 5];

        let result = batch_service
            .generic_batch_operation(items, operation)
            .await
            .unwrap();

        // Verify the results
        assert_eq!(result.stats.total, 5);
        assert_eq!(result.stats.successful, 2);
        assert_eq!(result.stats.failed, 3);

        // Even numbers should be in successes, doubled
        assert!(result.successful.contains(&4)); // 2*2
        assert!(result.successful.contains(&8)); // 4*2

        // Odd numbers should be in failures
        assert_eq!(result.failed.len(), 3);
    }
}
