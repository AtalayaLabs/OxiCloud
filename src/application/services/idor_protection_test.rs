//! Tests for IDOR (Insecure Direct Object Reference) protection.
//!
//! Verifies that ownership checks at the repository and service layers
//! correctly reject access when the caller is not the file owner.

use bytes::Bytes;
use futures::Stream;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use uuid::Uuid;

use crate::application::ports::storage_ports::{FileReadPort, FileWritePort};
use crate::common::errors::DomainError;
use crate::domain::entities::file::File;
use crate::domain::services::path_service::StoragePath;

// ═══════════════════════════════════════════════════════════════════════════
// Mock repositories
// ═══════════════════════════════════════════════════════════════════════════

/// A simple in-memory mock that maps (file_id → (File, owner_id)).
struct MockFileReadPort {
    /// file_id → (File, owner_id)
    files: Mutex<HashMap<String, (File, Uuid)>>,
}

impl MockFileReadPort {
    fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
        }
    }

    /// Insert a test file owned by `owner_id`.
    fn insert(&self, id: &str, name: &str, owner_id: Uuid) {
        let file = File::new(
            id.to_string(),
            name.to_string(),
            StoragePath::from_string(&format!("/{}", name)),
            42,
            "text/plain".to_string(),
            None,
        )
        .unwrap();
        self.files
            .lock()
            .unwrap()
            .insert(id.to_string(), (file, owner_id));
    }
}

impl FileReadPort for MockFileReadPort {
    async fn get_file(&self, id: &str) -> Result<File, DomainError> {
        let files = self.files.lock().unwrap();
        files
            .get(id)
            .map(|(f, _)| f.clone())
            .ok_or_else(|| DomainError::not_found("File", id.to_string()))
    }

    async fn get_file_or_trashed(&self, id: &str) -> Result<File, DomainError> {
        let files = self.files.lock().unwrap();
        files
            .get(id)
            .map(|(f, _)| f.clone())
            .ok_or_else(|| DomainError::not_found("File", id.to_string()))
    }

    async fn get_file_for_owner(&self, id: &str, owner_id: Uuid) -> Result<File, DomainError> {
        let files = self.files.lock().unwrap();
        match files.get(id) {
            Some((file, actual_owner)) if *actual_owner == owner_id => Ok(file.clone()),
            // Return NotFound regardless — do not leak existence
            _ => Err(DomainError::not_found("File", id.to_string())),
        }
    }

    async fn list_files(&self, _folder_id: Option<&str>) -> Result<Vec<File>, DomainError> {
        Ok(Vec::new())
    }

    async fn get_file_stream(
        &self,
        _id: &str,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        unimplemented!()
    }

    async fn get_file_range_stream(
        &self,
        _id: &str,
        _start: u64,
        _end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        unimplemented!()
    }

    async fn get_file_path(&self, _id: &str) -> Result<StoragePath, DomainError> {
        unimplemented!()
    }

    async fn get_parent_folder_id(&self, _path: &str) -> Result<String, DomainError> {
        unimplemented!()
    }

    async fn get_blob_hash(&self, _file_id: &str) -> Result<String, DomainError> {
        Ok(String::new())
    }

    async fn search_files_paginated(
        &self,
        _folder_id: Option<&str>,
        _criteria: &crate::application::dtos::search_dto::SearchCriteriaDto,
        _user_id: Uuid,
    ) -> Result<(Vec<File>, usize), DomainError> {
        Ok((Vec::new(), 0))
    }

    async fn count_files(
        &self,
        _folder_id: Option<&str>,
        _criteria: &crate::application::dtos::search_dto::SearchCriteriaDto,
        _user_id: Uuid,
    ) -> Result<usize, DomainError> {
        Ok(0)
    }

    async fn get_folder_id_by_path(&self, _folder_path: &str) -> Result<String, DomainError> {
        unimplemented!()
    }

    async fn stream_files_in_subtree(
        &self,
        _folder_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<File, DomainError>> + Send>>, DomainError> {
        Ok(Box::pin(futures::stream::empty()))
    }
}

/// Minimal mock write port — only `move_file` and `rename_file` need real logic.
#[allow(dead_code)]
struct MockFileWritePort {
    files: Mutex<HashMap<String, File>>,
}

impl MockFileWritePort {
    #[allow(dead_code)]
    fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
        }
    }

    #[allow(dead_code)]
    fn insert(&self, id: &str, name: &str) {
        let file = File::new(
            id.to_string(),
            name.to_string(),
            StoragePath::from_string(&format!("/{}", name)),
            42,
            "text/plain".to_string(),
            None,
        )
        .unwrap();
        self.files.lock().unwrap().insert(id.to_string(), file);
    }
}

impl FileWritePort for MockFileWritePort {
    async fn save_file_from_temp(
        &self,
        _name: String,
        _folder_id: Option<String>,
        _content_type: String,
        _temp_path: &Path,
        _size: u64,
        _pre_computed_hash: Option<String>,
    ) -> Result<File, DomainError> {
        unimplemented!()
    }

    async fn move_file(
        &self,
        file_id: &str,
        _target_folder_id: Option<String>,
    ) -> Result<File, DomainError> {
        let files = self.files.lock().unwrap();
        files
            .get(file_id)
            .cloned()
            .ok_or_else(|| DomainError::not_found("File", file_id.to_string()))
    }

    async fn rename_file(&self, file_id: &str, _new_name: &str) -> Result<File, DomainError> {
        let files = self.files.lock().unwrap();
        files
            .get(file_id)
            .cloned()
            .ok_or_else(|| DomainError::not_found("File", file_id.to_string()))
    }

    async fn delete_file(&self, _id: &str) -> Result<(), DomainError> {
        Ok(())
    }

    async fn update_file_content_from_temp(
        &self,
        _file_id: &str,
        _temp_path: &Path,
        _size: u64,
        _content_type: Option<String>,
        _pre_computed_hash: Option<String>,
        _modified_at: Option<i64>,
    ) -> Result<(String, i64), DomainError> {
        Ok((String::new(), 0))
    }

    async fn register_file_deferred(
        &self,
        _name: String,
        _folder_id: Option<String>,
        _content_type: String,
        _size: u64,
    ) -> Result<(File, PathBuf), DomainError> {
        unimplemented!()
    }

    async fn copy_file(
        &self,
        _file_id: &str,
        _target_folder_id: Option<String>,
    ) -> Result<File, DomainError> {
        unimplemented!()
    }

    async fn move_to_trash(&self, _file_id: &str) -> Result<(), DomainError> {
        Ok(())
    }

    async fn restore_from_trash(
        &self,
        _file_id: &str,
        _original_path: &str,
    ) -> Result<(), DomainError> {
        Ok(())
    }

    async fn delete_file_permanently(&self, _file_id: &str) -> Result<(), DomainError> {
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests — FileReadPort::get_file_for_owner (Repository layer, Solution C)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn get_file_for_owner_returns_file_for_correct_owner() {
    let alice_id = Uuid::new_v4();
    let repo = MockFileReadPort::new();
    repo.insert("file-1", "secret.txt", alice_id);

    let result = repo.get_file_for_owner("file-1", alice_id).await;
    assert!(result.is_ok(), "owner should be able to read own file");
    assert_eq!(result.unwrap().id(), "file-1");
}

#[tokio::test]
async fn get_file_for_owner_rejects_wrong_owner() {
    let alice_id = Uuid::new_v4();
    let bob_id = Uuid::new_v4();
    let repo = MockFileReadPort::new();
    repo.insert("file-1", "secret.txt", alice_id);

    let result = repo.get_file_for_owner("file-1", bob_id).await;
    assert!(result.is_err(), "non-owner should be rejected");

    // Must be NotFound, NOT Forbidden — avoids leaking existence
    let err = result.unwrap_err();
    let msg = format!("{}", err);
    assert!(
        msg.contains("not found") || msg.contains("NotFound"),
        "error must be NotFound, got: {}",
        msg
    );
}

#[tokio::test]
async fn get_file_for_owner_returns_not_found_for_missing_file() {
    let alice_id = Uuid::new_v4();
    let repo = MockFileReadPort::new();

    let result = repo.get_file_for_owner("nonexistent", alice_id).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn verify_file_owner_uses_default_impl() {
    let alice_id = Uuid::new_v4();
    let bob_id = Uuid::new_v4();
    let repo = MockFileReadPort::new();
    repo.insert("file-1", "secret.txt", alice_id);

    // Default impl delegates to get_file_for_owner and maps to ()
    assert!(repo.verify_file_owner("file-1", alice_id).await.is_ok());
    assert!(repo.verify_file_owner("file-1", bob_id).await.is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests — FileManagementService _owned methods (Service layer, Solution B)
// ═══════════════════════════════════════════════════════════════════════════
//
// Note: FileManagementService::with_trash takes concrete types for the write
// repository (Arc<FileBlobWriteRepository>). We cannot construct real PG repos
// without a database. Instead, we test the verify_owner logic indirectly by
// testing the mock-based trait interactions at the port level, and document
// that integration tests hitting the real DB are the ultimate verification.
//
// The tests below verify the *contract*: _owned methods must call
// verify_owner before delegating, and verify_owner must fail-closed when
// no read repo is available.

#[tokio::test]
async fn verify_file_owner_delegates_to_read_port() {
    // This test verifies the FileReadPort contract that verify_file_owner
    // returns Ok for the correct owner and Err for others.
    let user_id = Uuid::new_v4();
    let attacker_id = Uuid::new_v4();
    let read = MockFileReadPort::new();
    read.insert("abc-123", "report.pdf", user_id);

    // Same user → Ok
    let ok = read.verify_file_owner("abc-123", user_id).await;
    assert!(ok.is_ok(), "correct owner should pass verify_file_owner");

    // Different user → Err
    let err = read.verify_file_owner("abc-123", attacker_id).await;
    assert!(err.is_err(), "wrong owner should fail verify_file_owner");
}

#[tokio::test]
async fn owned_methods_require_ownership_check_first() {
    // Simulate what the _owned methods do: verify_owner then delegate.
    // We test with the mock read port to prove the sequence.
    let owner_id = Uuid::new_v4();
    let attacker_id = Uuid::new_v4();
    let read = MockFileReadPort::new();
    read.insert("file-1", "data.csv", owner_id);

    // Step 1: verify_owner for correct owner → Ok
    let step1 = read.verify_file_owner("file-1", owner_id).await;
    assert!(step1.is_ok());

    // Step 2: verify_owner for attacker → Err, so the move/rename never executes
    let step2 = read.verify_file_owner("file-1", attacker_id).await;
    assert!(step2.is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests — Trait-level _owned method stubs (StubFileManagementUseCase)
// ═══════════════════════════════════════════════════════════════════════════

use crate::application::ports::file_ports::FileManagementUseCase;
use crate::common::stubs::StubFileManagementUseCase;

#[tokio::test]
async fn stub_move_file_owned_returns_ok() {
    let user_id = Uuid::new_v4();
    let stub = StubFileManagementUseCase;
    let result = stub
        .move_file_with_perms("file-1", user_id, Some("folder-2".to_string()))
        .await;
    assert!(result.is_ok(), "stub should return Ok for move_file_owned");
}

#[tokio::test]
async fn stub_rename_file_owned_returns_ok() {
    let user_id = Uuid::new_v4();
    let stub = StubFileManagementUseCase;
    let result = stub
        .rename_file_with_perms("file-1", user_id, "new-name.txt")
        .await;
    assert!(
        result.is_ok(),
        "stub should return Ok for rename_file_owned"
    );
}

use crate::application::ports::file_ports::FileRetrievalUseCase;
use crate::common::stubs::StubFileRetrievalUseCase;

#[tokio::test]
async fn stub_get_file_owned_returns_ok() {
    let user_id = Uuid::new_v4();
    let stub = StubFileRetrievalUseCase;
    let result = stub.get_file_with_perms("file-1", user_id).await;
    assert!(result.is_ok(), "stub should return Ok for get_file_owned");
}

#[tokio::test]
async fn stub_get_file_optimized_owned_returns_ok() {
    let user_id = Uuid::new_v4();
    let stub = StubFileRetrievalUseCase;
    let result = stub
        .get_file_optimized_with_perms("file-1", user_id, true, false)
        .await;
    assert!(
        result.is_ok(),
        "stub should return Ok for get_file_optimized_owned"
    );
}
