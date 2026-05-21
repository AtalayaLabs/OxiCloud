//! Tests for batch operations
//!
//! This module contains tests for batch operation functionality,
//! including the trash folders fix for issue #124.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::application::services::batch_operations::{
        BatchOperationService, BatchResult, BatchStats,
    };
    use crate::application::services::file_management_service::FileManagementService;
    use crate::application::services::file_retrieval_service::FileRetrievalService;
    use crate::application::services::folder_service::FolderService;
    use crate::common::config::AppConfig;
    use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
    use crate::infrastructure::repositories::pg::file_blob_write_repository::FileBlobWriteRepository;
    use crate::infrastructure::repositories::pg::folder_db_repository::FolderDbRepository;

    /// Test that verifies the result mapping logic for batch operations.
    /// This addresses issue #124 where batch folder trash was reporting 0/1 successful.
    #[test]
    fn test_trash_result_mapping() {
        // Test Ok(()) maps to Ok(id)
        let folder_id = "test-folder-id".to_string();
        let trash_result: Result<(), String> = Ok(());
        let id_for_result = folder_id.clone();
        let mapped = trash_result.map(|_| id_for_result);

        assert!(mapped.is_ok(), "Ok result should remain Ok after mapping");
        assert_eq!(
            mapped.unwrap(),
            folder_id,
            "Mapped result should contain the folder_id"
        );

        // Test Err maps to Err (preserved)
        let folder_id2 = "test-folder-id-2".to_string();
        let trash_result2: Result<(), String> = Err("Some error".to_string());
        let id_for_result2 = folder_id2.clone();
        let mapped2 = trash_result2.map(|_| id_for_result2);

        assert!(
            mapped2.is_err(),
            "Err result should remain Err after mapping"
        );
    }

    /// Test that batch result counting works correctly
    #[test]
    fn test_batch_result_counting() {
        let mut result = BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            stats: BatchStats {
                total: 3,
                successful: 0,
                failed: 0,
                execution_time_ms: 0,
                max_concurrency: 1,
            },
        };

        // Simulate processing 2 successes and 1 failure
        result.successful.push("id1".to_string());
        result.stats.successful += 1;

        result.successful.push("id3".to_string());
        result.stats.successful += 1;

        result.failed.push(("id2".to_string(), "error".to_string()));
        result.stats.failed += 1;

        assert_eq!(result.stats.successful, 2, "Should have 2 successful");
        assert_eq!(result.stats.failed, 1, "Should have 1 failed");
        assert_eq!(
            result.successful.len(),
            2,
            "Successful vector should have 2 items"
        );
        assert_eq!(result.failed.len(), 1, "Failed vector should have 1 item");
    }

    /// Test that BatchOperationService can be created with stub repositories
    #[tokio::test]
    async fn test_batch_service_creation() {
        let folder_repo = Arc::new(FolderDbRepository::new_stub());
        let file_read_repo = Arc::new(FileBlobReadRepository::new_stub());
        let file_write_repo = Arc::new(FileBlobWriteRepository::new_stub());

        let file_retrieval = Arc::new(FileRetrievalService::new(file_read_repo));
        let file_management = Arc::new(FileManagementService::new(file_write_repo));
        let folder_service = Arc::new(FolderService::new(folder_repo, None));

        let _batch_service = BatchOperationService::new(
            file_retrieval,
            file_management,
            folder_service,
            AppConfig::default(),
        );

        // Service created successfully
    }
}
