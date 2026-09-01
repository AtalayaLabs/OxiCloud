//! Stub/Dummy implementations for dependency injection.
//!
//! These no-op implementations are used exclusively by `AppState::default()`
//! to provide a minimal, valid state for the auth middleware and route
//! construction before the real services are wired in `main.rs`.
//!
//! **None of these stubs should ever handle real user requests.**

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;
use uuid::Uuid;

use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::{
    CreateFolderDto, FolderDto, MoveFolderDto, RenameFolderDto,
};
use crate::application::dtos::pagination::{PaginatedResponseDto, PaginationRequestDto};
use crate::application::dtos::search_dto::{
    SearchCriteriaDto, SearchResultsDto, SearchSuggestionsDto,
};
use crate::application::ports::file_ports::{
    FileManagementUseCase, FileRetrievalUseCase, FileUploadUseCase, OptimizedFileContent,
    StoredBlob,
};
use crate::application::ports::folder_ports::FolderUseCase;

use crate::application::ports::inbound::SearchUseCase;
use crate::application::ports::storage_ports::{FileReadPort, FileWritePort};
use crate::application::ports::zip_ports::ZipPort;
use crate::common::errors::DomainError;
use crate::domain::entities::file::File;
use crate::domain::entities::folder::Folder;
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::services::authorization::Permission;
use crate::domain::services::i18n_service::{I18nResult, I18nService, Locale};
use crate::domain::services::path_service::StoragePath;

// ---------------------------------------------------------------------------
// ZipPort
// ---------------------------------------------------------------------------

/// Placeholder ZipPort that always errors. Replaced after application services
/// are fully initialised.
pub struct StubZipPort;

impl ZipPort for StubZipPort {
    async fn create_folder_zip(
        &self,
        _folder_id: &str,
        _folder_name: &str,
    ) -> Result<tempfile::NamedTempFile, DomainError> {
        Err(DomainError::internal_error(
            "ZipService",
            "ZipService not initialized",
        ))
    }
}

// ---------------------------------------------------------------------------
// FileReadPort
// ---------------------------------------------------------------------------

pub struct StubFileReadPort;

impl FileReadPort for StubFileReadPort {
    async fn get_file(&self, _id: &str) -> Result<File, DomainError> {
        Ok(File::default())
    }

    async fn get_file_or_trashed(&self, _id: &str) -> Result<File, DomainError> {
        Ok(File::default())
    }

    async fn list_files(&self, _folder_id: Option<&str>) -> Result<Vec<File>, DomainError> {
        Ok(Vec::new())
    }

    async fn get_file_stream(
        &self,
        _id: &str,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        let empty_stream = futures::stream::empty::<Result<Bytes, std::io::Error>>();
        Ok(Box::new(empty_stream))
    }

    async fn get_file_range_stream(
        &self,
        _id: &str,
        _start: u64,
        _end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        let empty_stream = futures::stream::empty::<Result<Bytes, std::io::Error>>();
        Ok(Box::new(empty_stream))
    }

    async fn get_file_path(&self, _id: &str) -> Result<StoragePath, DomainError> {
        Ok(StoragePath::from_string("/"))
    }

    async fn get_parent_folder_id(
        &self,
        _path: &str,
        _drive_id: Uuid,
    ) -> Result<String, DomainError> {
        Ok("root".to_string())
    }

    async fn get_folder_id_by_path(
        &self,
        _folder_path: &str,
        _drive_id: Uuid,
    ) -> Result<String, DomainError> {
        Ok("stub-folder-id".to_string())
    }

    async fn get_blob_hash(&self, _file_id: &str) -> Result<String, DomainError> {
        Ok(String::new())
    }

    async fn search_files_paginated(
        &self,
        _folder_id: Option<&str>,
        _criteria: &SearchCriteriaDto,
        _user_id: Uuid,
    ) -> Result<(Vec<File>, Vec<(bool, bool)>, usize), DomainError> {
        Ok((Vec::new(), Vec::new(), 0))
    }

    async fn stream_files_in_subtree(
        &self,
        _folder_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<File, DomainError>> + Send>>, DomainError> {
        Ok(Box::pin(futures::stream::empty()))
    }
}

// ---------------------------------------------------------------------------
// FileWritePort
// ---------------------------------------------------------------------------

pub struct StubFileWritePort;

impl FileWritePort for StubFileWritePort {
    async fn save_file_with_blob(
        &self,
        _name: String,
        _folder_id: Option<String>,
        _content_type: String,
        _blob_hash: &str,
        _size: u64,
        _caller_id: Uuid,
    ) -> Result<File, DomainError> {
        Ok(File::default())
    }

    async fn move_file(
        &self,
        _file_id: &str,
        _target_folder_id: Option<String>,
        _caller_id: Uuid,
    ) -> Result<File, DomainError> {
        Ok(File::default())
    }

    async fn copy_file(
        &self,
        _file_id: &str,
        _target_folder_id: Option<String>,
        _new_name: Option<&str>,
        _caller_id: Uuid,
    ) -> Result<File, DomainError> {
        Ok(File::default())
    }

    async fn rename_file(
        &self,
        _file_id: &str,
        _new_name: &str,
        _caller_id: Uuid,
    ) -> Result<File, DomainError> {
        Ok(File::default())
    }

    async fn delete_file(&self, _id: &str) -> Result<(), DomainError> {
        Ok(())
    }

    async fn update_file_content_with_blob(
        &self,
        _file_id: &str,
        _blob_hash: &str,
        _size: u64,
        _modified_at: Option<i64>,
        _caller_id: Uuid,
        _expected_hash: Option<&str>,
    ) -> Result<(String, i64), DomainError> {
        Ok((String::new(), 0))
    }

    async fn register_file_deferred(
        &self,
        _name: String,
        _folder_id: Option<String>,
        _content_type: String,
        _size: u64,
        _caller_id: Uuid,
    ) -> Result<(File, PathBuf), DomainError> {
        Ok((File::default(), PathBuf::from("/tmp/dummy")))
    }

    async fn move_to_trash(&self, _file_id: &str, _caller_id: Uuid) -> Result<(), DomainError> {
        Ok(())
    }

    async fn restore_from_trash(
        &self,
        _file_id: &str,
        _original_path: &str,
        _caller_id: Uuid,
    ) -> Result<(), DomainError> {
        Ok(())
    }

    async fn delete_file_permanently(&self, _file_id: &str) -> Result<(), DomainError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FolderStoragePort
// ---------------------------------------------------------------------------

pub struct StubFolderStoragePort;

impl FolderRepository for StubFolderStoragePort {
    async fn create_folder(
        &self,
        _name: String,
        _parent_id: Option<String>,
        _caller_id: Uuid,
    ) -> Result<Folder, DomainError> {
        Ok(Folder::default())
    }

    async fn get_folder(&self, _id: &str) -> Result<Folder, DomainError> {
        Ok(Folder::default())
    }

    async fn get_folder_by_path(
        &self,
        _storage_path: &StoragePath,
        _drive_id: Uuid,
    ) -> Result<Folder, DomainError> {
        Ok(Folder::default())
    }

    async fn list_folders(&self, _parent_id: Option<&str>) -> Result<Vec<Folder>, DomainError> {
        Ok(Vec::new())
    }

    async fn list_root_folders_for_caller(
        &self,
        _caller_id: Uuid,
    ) -> Result<Vec<Folder>, DomainError> {
        Ok(Vec::new())
    }

    async fn list_folders_paginated(
        &self,
        _parent_id: Option<&str>,
        _offset: usize,
        _limit: usize,
        _include_total: bool,
    ) -> Result<(Vec<Folder>, Option<usize>), DomainError> {
        Ok((Vec::new(), Some(0)))
    }

    async fn list_root_folders_for_caller_paginated(
        &self,
        _caller_id: Uuid,
        _offset: usize,
        _limit: usize,
        _include_total: bool,
    ) -> Result<(Vec<Folder>, Option<usize>), DomainError> {
        Ok((Vec::new(), Some(0)))
    }

    async fn rename_folder(
        &self,
        _id: &str,
        _new_name: String,
        _caller_id: Uuid,
    ) -> Result<Folder, DomainError> {
        Ok(Folder::default())
    }

    async fn move_folder(
        &self,
        _id: &str,
        _new_parent_id: Option<&str>,
        _caller_id: Uuid,
    ) -> Result<Folder, DomainError> {
        Ok(Folder::default())
    }

    async fn delete_folder(&self, _id: &str) -> Result<(), DomainError> {
        Ok(())
    }

    async fn folder_exists(
        &self,
        _storage_path: &StoragePath,
        _drive_id: Uuid,
    ) -> Result<bool, DomainError> {
        Ok(false)
    }

    async fn get_folder_path(&self, _id: &str) -> Result<StoragePath, DomainError> {
        Ok(StoragePath::from_string("/"))
    }

    async fn move_to_trash(&self, _folder_id: &str, _caller_id: Uuid) -> Result<(), DomainError> {
        Ok(())
    }

    async fn restore_from_trash(
        &self,
        _folder_id: &str,
        _original_path: &str,
        _caller_id: Uuid,
    ) -> Result<(), DomainError> {
        Ok(())
    }

    async fn delete_folder_permanently(&self, _folder_id: &str) -> Result<(), DomainError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// I18nService
// ---------------------------------------------------------------------------

pub struct StubI18nService;

impl I18nService for StubI18nService {
    async fn translate(&self, _key: &str, _locale: Locale) -> I18nResult<String> {
        Ok(String::new())
    }

    async fn translate_args(
        &self,
        _key: &str,
        _locale: Locale,
        _args: &[(&str, &str)],
    ) -> I18nResult<String> {
        Ok(String::new())
    }

    async fn load_translations(&self, _locale: Locale) -> I18nResult<()> {
        Ok(())
    }

    async fn available_locales(&self) -> Vec<Locale> {
        vec![Locale::default()]
    }

    async fn is_supported(&self, _locale: Locale) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// FolderUseCase
// ---------------------------------------------------------------------------

pub struct StubFolderUseCase;

impl FolderUseCase for StubFolderUseCase {
    async fn require_permission(
        &self,
        _caller_id: Uuid,
        _permission: Permission,
        _file_id: &str,
    ) -> Result<(), DomainError> {
        Ok(())
    }

    async fn create_folder_with_perms(
        &self,
        _dto: CreateFolderDto,
        _user_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        Ok(FolderDto::default())
    }

    async fn get_folder(&self, _id: &str) -> Result<FolderDto, DomainError> {
        Ok(FolderDto::default())
    }

    async fn get_folder_with_perms(
        &self,
        _id: &str,
        _caller_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        Ok(FolderDto::default())
    }

    async fn get_folder_by_path(
        &self,
        _path: &str,
        _drive_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        Ok(FolderDto::default())
    }

    async fn list_folders(&self, _parent_id: Option<&str>) -> Result<Vec<FolderDto>, DomainError> {
        Ok(Vec::new())
    }

    async fn list_folders_with_perms(
        &self,
        _parent_id: Option<&str>,
        _owner_id: Uuid,
    ) -> Result<Vec<FolderDto>, DomainError> {
        Ok(Vec::new())
    }

    async fn list_folders_paginated(
        &self,
        _parent_id: Option<&str>,
        _pagination: &PaginationRequestDto,
    ) -> Result<PaginatedResponseDto<FolderDto>, DomainError> {
        Ok(PaginatedResponseDto::new(Vec::new(), 0, 10, 0))
    }

    async fn list_folders_paginated_with_perms(
        &self,
        _parent_id: Option<&str>,
        _owner_id: Uuid,
        _pagination: &PaginationRequestDto,
    ) -> Result<PaginatedResponseDto<FolderDto>, DomainError> {
        Ok(PaginatedResponseDto::new(Vec::new(), 0, 10, 0))
    }

    async fn rename_folder_with_perms(
        &self,
        _id: &str,
        _dto: RenameFolderDto,
        _caller_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        Ok(FolderDto::default())
    }

    async fn move_folder_with_perms(
        &self,
        _id: &str,
        _dto: MoveFolderDto,
        _caller_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        Ok(FolderDto::default())
    }

    async fn delete_folder_with_perms(
        &self,
        _id: &str,
        _caller_id: Uuid,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FileUploadUseCase
// ---------------------------------------------------------------------------

pub struct StubFileUploadUseCase;

impl FileUploadUseCase for StubFileUploadUseCase {
    async fn upload_file_streaming(
        &self,
        _name: String,
        _folder_id: Option<String>,
        _content_type: String,
        _blob: StoredBlob,
        _caller_id: Uuid,
    ) -> Result<FileDto, DomainError> {
        Ok(FileDto::default())
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_file_streaming_with_perms(
        &self,
        _path: &str,
        _drive_id: Uuid,
        _blob: StoredBlob,
        _content_type: &str,
        _modified_at: Option<i64>,
        _caller_id: Uuid,
        _expected_hash: Option<&str>,
    ) -> Result<FileDto, DomainError> {
        Ok(FileDto::default())
    }

    async fn upload_file_streaming_with_perms(
        &self,
        _name: String,
        _folder_id: Option<String>,
        _content_type: String,
        _blob: StoredBlob,
        _caller_id: Uuid,
    ) -> Result<FileDto, DomainError> {
        Ok(FileDto::default())
    }
}

// ---------------------------------------------------------------------------
// FileRetrievalUseCase
// ---------------------------------------------------------------------------

pub struct StubFileRetrievalUseCase;

impl FileRetrievalUseCase for StubFileRetrievalUseCase {
    async fn get_file(&self, _id: &str) -> Result<FileDto, DomainError> {
        Ok(FileDto::default())
    }

    async fn get_file_or_trashed_with_perms(
        &self,
        _id: &str,
        _owner_id: Uuid,
    ) -> Result<FileDto, DomainError> {
        Ok(FileDto::default())
    }

    async fn list_files(&self, _folder_id: Option<&str>) -> Result<Vec<FileDto>, DomainError> {
        Ok(Vec::new())
    }

    async fn list_files_with_perms(
        &self,
        _folder_id: Option<&str>,
        _owner_id: Uuid,
    ) -> Result<Vec<FileDto>, DomainError> {
        Ok(Vec::new())
    }

    async fn get_file_stream(
        &self,
        _id: &str,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        let empty_stream = futures::stream::empty::<Result<Bytes, std::io::Error>>();
        Ok(Box::new(empty_stream))
    }

    async fn get_file_stream_with_perms(
        &self,
        _id: &str,
        _caller_id: Uuid,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        let empty_stream = futures::stream::empty::<Result<Bytes, std::io::Error>>();
        Ok(Box::new(empty_stream))
    }

    async fn get_file_optimized(
        &self,
        _id: &str,
        _accept_webp: bool,
        _prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        Ok((
            FileDto::default(),
            OptimizedFileContent::Bytes {
                data: Bytes::new(),
                mime_type: Arc::from(""),
                was_transcoded: false,
            },
        ))
    }

    async fn get_file_range_stream(
        &self,
        _id: &str,
        _start: u64,
        _end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        let empty_stream = futures::stream::empty::<Result<Bytes, std::io::Error>>();
        Ok(Box::new(empty_stream))
    }

    async fn get_file_by_path(&self, _path: &str, _drive_id: Uuid) -> Result<FileDto, DomainError> {
        Err(DomainError::not_found("File", "stub"))
    }

    async fn stream_files_in_subtree(
        &self,
        _folder_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<FileDto, DomainError>> + Send>>, DomainError> {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn get_file_with_perms(
        &self,
        _id: &str,
        _caller_id: Uuid,
    ) -> Result<FileDto, DomainError> {
        Ok(FileDto::default())
    }

    async fn get_file_optimized_with_perms(
        &self,
        _id: &str,
        _caller_id: Uuid,
        _accept_webp: bool,
        _prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        Ok((
            FileDto::default(),
            OptimizedFileContent::Bytes {
                data: Bytes::new(),
                mime_type: Arc::from(""),
                was_transcoded: false,
            },
        ))
    }

    async fn get_file_range_stream_with_perms(
        &self,
        _id: &str,
        _caller_id: Uuid,
        _start: u64,
        _end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        let empty_stream = futures::stream::empty::<Result<Bytes, std::io::Error>>();
        Ok(Box::new(empty_stream))
    }
}

// ---------------------------------------------------------------------------
// FileManagementUseCase
// ---------------------------------------------------------------------------

pub struct StubFileManagementUseCase;

impl FileManagementUseCase for StubFileManagementUseCase {
    async fn require_permission(
        &self,
        _caller_id: Uuid,
        _permission: Permission,
        _file_id: &str,
    ) -> Result<(), DomainError> {
        Ok(())
    }

    async fn copy_file_with_perms(
        &self,
        _file_id: &str,
        _caller_id: Uuid,
        _folder_id: Option<String>,
        _new_name: Option<String>,
    ) -> Result<FileDto, DomainError> {
        Ok(FileDto::default())
    }

    async fn delete_file_with_perms(&self, _id: &str, _caller_id: Uuid) -> Result<(), DomainError> {
        Ok(())
    }

    async fn delete_and_cleanup_with_perms(
        &self,
        _id: &str,
        _user_id: Uuid,
    ) -> Result<bool, DomainError> {
        Ok(false)
    }

    async fn move_file_with_perms(
        &self,
        _file_id: &str,
        _caller_id: Uuid,
        _folder_id: Option<String>,
    ) -> Result<FileDto, DomainError> {
        Ok(FileDto::default())
    }

    async fn rename_file_with_perms(
        &self,
        _file_id: &str,
        _caller_id: Uuid,
        _new_name: &str,
    ) -> Result<FileDto, DomainError> {
        Ok(FileDto::default())
    }

    async fn copy_folder_tree_with_perms(
        &self,
        _source_folder_id: &str,
        _caller_id: Uuid,
        _target_parent_id: Option<String>,
        _dest_name: Option<String>,
    ) -> Result<crate::application::ports::storage_ports::CopyFolderTreeResult, DomainError> {
        Err(DomainError::internal_error(
            "StubFileManagement",
            "copy_folder_tree_owned not implemented",
        ))
    }
}

// ---------------------------------------------------------------------------
// SearchUseCase
// ---------------------------------------------------------------------------

pub struct StubSearchUseCase;

impl SearchUseCase for StubSearchUseCase {
    async fn search(
        &self,
        _criteria: SearchCriteriaDto,
        _user_id: Uuid,
    ) -> Result<Arc<SearchResultsDto>, DomainError> {
        Ok(Arc::new(SearchResultsDto::empty()))
    }

    async fn suggest(
        &self,
        _query: &str,
        _folder_id: Option<&str>,
        _limit: usize,
        _caller_id: Uuid,
    ) -> Result<SearchSuggestionsDto, DomainError> {
        Ok(SearchSuggestionsDto {
            suggestions: Vec::new(),
            query_time_ms: 0,
        })
    }

    async fn clear_search_cache(&self) -> Result<(), DomainError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DedupPort
// ---------------------------------------------------------------------------

use crate::application::ports::dedup_ports::{BlobMetadataDto, DedupPort, DedupStatsDto};

pub struct StubDedupPort;

impl DedupPort for StubDedupPort {
    async fn blob_exists(&self, _hash: &str) -> bool {
        false
    }

    async fn find_derived_blob(
        &self,
        _source_hash: &str,
        _kind: &str,
        _variant: &str,
    ) -> Option<crate::application::ports::dedup_ports::DerivedBlobRef> {
        None
    }

    async fn get_blob_metadata(&self, _hash: &str) -> Option<BlobMetadataDto> {
        None
    }

    async fn read_blob_stream(
        &self,
        _hash: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>
    {
        Err(DomainError::internal_error(
            "DedupService",
            "DedupService not initialized",
        ))
    }

    async fn read_blob_range_stream(
        &self,
        _hash: &str,
        _start: u64,
        _end: Option<u64>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>
    {
        Err(DomainError::internal_error(
            "DedupService",
            "DedupService not initialized",
        ))
    }

    async fn blob_size(&self, _hash: &str) -> Result<u64, DomainError> {
        Err(DomainError::internal_error(
            "DedupService",
            "DedupService not initialized",
        ))
    }

    async fn add_reference(&self, _hash: &str) -> Result<(), DomainError> {
        Ok(())
    }

    async fn remove_reference(&self, _hash: &str) -> Result<bool, DomainError> {
        Ok(false)
    }

    async fn hash_file(&self, _path: &Path) -> Result<String, DomainError> {
        Ok(String::new())
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        PathBuf::from(format!("stub_blob_{}.blob", hash))
    }

    async fn get_stats(&self) -> DedupStatsDto {
        DedupStatsDto::default()
    }

    async fn flush(&self) -> Result<(), DomainError> {
        Ok(())
    }

    async fn verify_integrity(&self) -> Result<Vec<String>, DomainError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// MetadataCachePort
// ---------------------------------------------------------------------------

use crate::application::ports::cache_ports::{
    CachedMetadataDto, ContentCachePort, MetadataCachePort,
};

pub struct StubMetadataCachePort;

impl MetadataCachePort for StubMetadataCachePort {
    async fn get_metadata(&self, _path: &Path) -> Option<CachedMetadataDto> {
        None
    }

    async fn is_file(&self, _path: &Path) -> Option<bool> {
        None
    }

    async fn refresh_metadata(&self, path: &Path) -> Result<CachedMetadataDto, DomainError> {
        Ok(CachedMetadataDto {
            path: path.to_path_buf(),
            exists: false,
            is_file: false,
            size: None,
            mime_type: None,
            created_at: None,
            modified_at: None,
        })
    }

    async fn invalidate(&self, _path: &Path) {}

    async fn invalidate_directory(&self, _dir_path: &Path) {}
}

// ---------------------------------------------------------------------------
// ContentCachePort
// ---------------------------------------------------------------------------

pub struct StubContentCachePort;

impl ContentCachePort for StubContentCachePort {
    fn should_cache(&self, _size: usize) -> bool {
        false
    }

    async fn get(&self, _file_id: &str) -> Option<(Bytes, Arc<str>, Arc<str>)> {
        None
    }

    async fn put(
        &self,
        _file_id: String,
        _content: Bytes,
        _etag: Arc<str>,
        _content_type: Arc<str>,
    ) {
    }

    async fn invalidate(&self, _file_id: &str) {}

    async fn clear(&self) {}
}
