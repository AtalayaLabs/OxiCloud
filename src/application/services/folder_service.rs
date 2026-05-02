use crate::application::dtos::folder_dto::{
    CreateFolderDto, EnsurePathOutcome, FolderDto, MoveFolderDto, RenameFolderDto,
};
use crate::application::ports::inbound::FolderUseCase;
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::services::path_service::StoragePath;
use crate::infrastructure::repositories::pg::folder_db_repository::FolderDbRepository;
use crate::infrastructure::services::path_resolver_service::{
    PathResolverService, ResolvedResource,
};
use std::sync::Arc;
use uuid::Uuid;

/// Implementation of the use case for folder operations
pub struct FolderService {
    folder_storage: Arc<FolderDbRepository>,
    path_resolver: Option<Arc<PathResolverService>>,
}

impl FolderService {
    /// Creates a new folder service.
    ///
    /// `path_resolver` is required for `ensure_path` (owner-scoped path
    /// resolution); other use cases work without it. Tests and reduced
    /// stubs may pass `None`.
    pub fn new(
        folder_storage: Arc<FolderDbRepository>,
        path_resolver: Option<Arc<PathResolverService>>,
    ) -> Self {
        Self {
            folder_storage,
            path_resolver,
        }
    }

    /// Creates a stub implementation for testing and middleware
    pub fn new_stub() -> impl FolderUseCase {
        struct FolderServiceStub;

        impl FolderUseCase for FolderServiceStub {
            async fn create_folder(&self, _dto: CreateFolderDto) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn ensure_path(
                &self,
                _path: &str,
                _owner: Uuid,
            ) -> Result<EnsurePathOutcome, DomainError> {
                Ok(EnsurePathOutcome::Created(FolderDto::empty()))
            }

            async fn get_folder(&self, _id: &str) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn get_folder_owned(
                &self,
                _id: &str,
                _caller_id: Uuid,
            ) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn get_folder_by_path(&self, _path: &str) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn list_folders(
                &self,
                _parent_id: Option<&str>,
            ) -> Result<Vec<FolderDto>, DomainError> {
                Ok(vec![])
            }

            async fn list_folders_for_owner(
                &self,
                _parent_id: Option<&str>,
                _owner_id: Uuid,
            ) -> Result<Vec<FolderDto>, DomainError> {
                Ok(vec![])
            }

            async fn list_folders_paginated(
                &self,
                _parent_id: Option<&str>,
                _pagination: &crate::application::dtos::pagination::PaginationRequestDto,
            ) -> Result<
                crate::application::dtos::pagination::PaginatedResponseDto<FolderDto>,
                DomainError,
            > {
                Ok(
                    crate::application::dtos::pagination::PaginatedResponseDto::new(
                        vec![],
                        0,
                        10,
                        0,
                    ),
                )
            }

            async fn list_folders_for_owner_paginated(
                &self,
                _parent_id: Option<&str>,
                _owner_id: Uuid,
                _pagination: &crate::application::dtos::pagination::PaginationRequestDto,
            ) -> Result<
                crate::application::dtos::pagination::PaginatedResponseDto<FolderDto>,
                DomainError,
            > {
                Ok(
                    crate::application::dtos::pagination::PaginatedResponseDto::new(
                        vec![],
                        0,
                        10,
                        0,
                    ),
                )
            }

            async fn rename_folder(
                &self,
                _id: &str,
                _dto: RenameFolderDto,
                _caller_id: Uuid,
            ) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn move_folder(
                &self,
                _id: &str,
                _dto: MoveFolderDto,
                _caller_id: Uuid,
            ) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn delete_folder(&self, _id: &str, _caller_id: Uuid) -> Result<(), DomainError> {
                Ok(())
            }

            async fn create_home_folder(
                &self,
                _user_id: Uuid,
                _name: String,
            ) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }
        }

        FolderServiceStub
    }
}

impl FolderUseCase for FolderService {
    /// Creates a new folder
    async fn create_folder(&self, dto: CreateFolderDto) -> Result<FolderDto, DomainError> {
        // Input validation
        if dto.name.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "Folder",
                "Folder name cannot be empty",
            ));
        }

        // If a parent_id is provided, verify it exists
        if let Some(parent_id) = &dto.parent_id {
            let parent_exists = self.folder_storage.get_folder(parent_id).await.is_ok();
            if !parent_exists {
                return Err(DomainError::not_found("Folder", parent_id));
            }
        }

        let folder = self
            .folder_storage
            .create_folder(dto.name, dto.parent_id)
            .await?;

        Ok(FolderDto::from(folder))
    }

    async fn ensure_path(&self, path: &str, owner: Uuid) -> Result<EnsurePathOutcome, DomainError> {
        let resolver = self.path_resolver.as_ref().ok_or_else(|| {
            DomainError::internal_error(
                "FolderService",
                "ensure_path called without a PathResolverService — this is a DI configuration bug",
            )
        })?;

        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "Folder",
                "ensure_path requires a non-empty path",
            ));
        }
        let last_index = segments.len() - 1;

        let mut parent_id: Option<String> = None;
        let mut accumulated = String::new();

        for (i, segment) in segments.iter().enumerate() {
            if !accumulated.is_empty() {
                accumulated.push('/');
            }
            accumulated.push_str(segment);
            let is_leaf = i == last_index;

            match resolver.resolve_path_for_user(&accumulated, owner).await {
                Ok(ResolvedResource::Folder(existing)) => {
                    if is_leaf {
                        return Ok(EnsurePathOutcome::Existed(existing));
                    }
                    parent_id = Some(existing.id);
                }
                Ok(ResolvedResource::File(_)) => {
                    // A file occupies the path. RFC 4918 §9.3.1: MKCOL on
                    // an existing resource of any kind → 405. Surface this
                    // as `AlreadyExists`; the handler maps to 405 via
                    // `From<DomainError>` (`AlreadyExists` → `Conflict`),
                    // overridden to 405 by the MKCOL adapter when the
                    // outcome variant indicates pre-existence. Here we
                    // distinguish by erroring with `AlreadyExists` rather
                    // than returning an `Existed` outcome, because we don't
                    // own a `FolderDto` for a file — and because
                    // intermediate file collisions are unrecoverable.
                    return Err(DomainError::already_exists(
                        "Folder",
                        format!("a file already exists at path '{}'", accumulated),
                    ));
                }
                Err(e) if e.kind == ErrorKind::NotFound => {
                    let create_dto = CreateFolderDto {
                        name: (*segment).to_string(),
                        parent_id: parent_id.clone(),
                    };
                    match self.create_folder(create_dto).await {
                        Ok(created) => {
                            if is_leaf {
                                return Ok(EnsurePathOutcome::Created(created));
                            }
                            parent_id = Some(created.id);
                        }
                        Err(e) if e.kind == ErrorKind::AlreadyExists => {
                            // Race: another request created the same folder
                            // between our resolve and create calls. Re-read.
                            match resolver.resolve_path_for_user(&accumulated, owner).await? {
                                ResolvedResource::Folder(existing) => {
                                    if is_leaf {
                                        return Ok(EnsurePathOutcome::Existed(existing));
                                    }
                                    parent_id = Some(existing.id);
                                }
                                ResolvedResource::File(_) => {
                                    return Err(DomainError::already_exists(
                                        "Folder",
                                        format!("a file already exists at path '{}'", accumulated),
                                    ));
                                }
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }

        // Loop body always returns when `is_leaf`; reaching here means
        // `segments` was empty, but we guarded that above.
        unreachable!("ensure_path loop must return on the leaf segment")
    }

    /// Creates a root-level home folder for a user during registration.
    async fn create_home_folder(
        &self,
        user_id: Uuid,
        name: String,
    ) -> Result<FolderDto, DomainError> {
        let folder = self
            .folder_storage
            .create_home_folder(user_id, name)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!("Failed to create home folder: {}", e),
                )
            })?;

        Ok(FolderDto::from(folder))
    }

    async fn list_subtree_folders(&self, folder_id: &str) -> Result<Vec<FolderDto>, DomainError> {
        let folders = self.folder_storage.list_subtree_folders(folder_id).await?;
        Ok(folders.into_iter().map(FolderDto::from).collect())
    }

    /// Gets a folder by its ID
    async fn get_folder(&self, id: &str) -> Result<FolderDto, DomainError> {
        let folder = self.folder_storage.get_folder(id).await.map_err(|e| {
            DomainError::internal_error(
                "FolderStorage",
                format!("Failed to get folder with ID: {}: {}", id, e),
            )
        })?;

        Ok(FolderDto::from(folder))
    }

    /// Gets a folder by its ID, enforcing that `caller_id` is the owner.
    async fn get_folder_owned(&self, id: &str, caller_id: Uuid) -> Result<FolderDto, DomainError> {
        let folder_dto = self.get_folder(id).await?;
        if folder_dto.owner_id.as_deref() != Some(&caller_id.to_string()) {
            tracing::warn!(
                "get_folder_owned: user '{}' attempted to access folder '{}' owned by '{:?}'",
                caller_id,
                id,
                folder_dto.owner_id
            );
            return Err(DomainError::not_found("Folder", id));
        }
        Ok(folder_dto)
    }

    /// Gets a folder by its path
    async fn get_folder_by_path(&self, path: &str) -> Result<FolderDto, DomainError> {
        // Convert the string path to StoragePath
        let storage_path = StoragePath::from_string(path);

        let folder = self
            .folder_storage
            .get_folder_by_path(&storage_path)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!("Failed to get folder at path: {}: {}", path, e),
                )
            })?;

        Ok(FolderDto::from(folder))
    }

    /// Lists folders within a parent folder
    async fn list_folders(&self, parent_id: Option<&str>) -> Result<Vec<FolderDto>, DomainError> {
        let folders = self
            .folder_storage
            .list_folders(parent_id)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!("Failed to list folders in parent: {:?}: {}", parent_id, e),
                )
            })?;

        // Convert to DTOs
        Ok(folders.into_iter().map(FolderDto::from).collect())
    }

    /// Lists folders scoped to a specific owner.
    /// Self-healing: if listing root folders and none exist, creates a home folder.
    async fn list_folders_for_owner(
        &self,
        parent_id: Option<&str>,
        owner_id: Uuid,
    ) -> Result<Vec<FolderDto>, DomainError> {
        let owner_id_short = {
            let s = owner_id.to_string();
            s[..8.min(s.len())].to_string()
        };
        let folders = self
            .folder_storage
            .list_folders_by_owner(parent_id, owner_id)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!(
                        "Failed to list folders for owner '{}' in parent {:?}: {}",
                        owner_id, parent_id, e
                    ),
                )
            })?;

        // Self-healing: if listing root folders and none exist, create a home folder
        // This ensures the frontend always gets a valid userHomeFolderId
        if parent_id.is_none() && folders.is_empty() {
            tracing::info!(
                "No root folders found for user {}, creating home folder automatically",
                owner_id
            );
            let folder_name = format!("My Folder - {}", owner_id_short);
            match self
                .folder_storage
                .create_home_folder(owner_id, folder_name.clone())
                .await
            {
                Ok(home_folder) => {
                    tracing::info!(
                        "Created home folder '{}' for user {}",
                        folder_name,
                        owner_id
                    );
                    return Ok(vec![FolderDto::from(home_folder)]);
                }
                Err(e) => {
                    tracing::warn!("Failed to create home folder for user {}: {}", owner_id, e);
                    // Return empty list rather than failing - user might not have storage quota, etc.
                }
            }
        }

        Ok(folders.into_iter().map(FolderDto::from).collect())
    }

    /// Lists folders with pagination
    async fn list_folders_paginated(
        &self,
        parent_id: Option<&str>,
        pagination: &crate::application::dtos::pagination::PaginationRequestDto,
    ) -> Result<crate::application::dtos::pagination::PaginatedResponseDto<FolderDto>, DomainError>
    {
        let pagination = pagination.validate_and_adjust();

        let (folders, total_items) = self
            .folder_storage
            .list_folders_paginated(parent_id, pagination.offset(), pagination.limit(), true)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!(
                        "Failed to list folders with pagination in parent: {:?}: {}",
                        parent_id, e
                    ),
                )
            })?;

        let total = total_items.unwrap_or(folders.len());

        let response = crate::application::dtos::pagination::PaginatedResponseDto::new(
            folders.into_iter().map(FolderDto::from).collect(),
            pagination.page,
            pagination.page_size,
            total,
        );

        Ok(response)
    }

    /// Lists folders with pagination, scoped to a specific owner.
    async fn list_folders_for_owner_paginated(
        &self,
        parent_id: Option<&str>,
        owner_id: Uuid,
        pagination: &crate::application::dtos::pagination::PaginationRequestDto,
    ) -> Result<crate::application::dtos::pagination::PaginatedResponseDto<FolderDto>, DomainError>
    {
        let pagination = pagination.validate_and_adjust();

        let (folders, total_items) = self
            .folder_storage
            .list_folders_by_owner_paginated(
                parent_id,
                owner_id,
                pagination.offset(),
                pagination.limit(),
                true,
            )
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!(
                        "Failed to list folders for owner '{}' with pagination in parent {:?}: {}",
                        owner_id, parent_id, e
                    ),
                )
            })?;

        let total = total_items.unwrap_or(folders.len());

        let response = crate::application::dtos::pagination::PaginatedResponseDto::new(
            folders.into_iter().map(FolderDto::from).collect(),
            pagination.page,
            pagination.page_size,
            total,
        );

        Ok(response)
    }

    /// Renames a folder after verifying ownership.
    async fn rename_folder(
        &self,
        id: &str,
        dto: RenameFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        // Input validation
        if dto.name.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "Folder",
                "New folder name cannot be empty",
            ));
        }

        // Verify the folder exists and belongs to the caller
        let existing_folder = self.folder_storage.get_folder(id).await.map_err(|e| {
            DomainError::internal_error(
                "FolderStorage",
                format!("Failed to get folder with ID: {} for renaming: {}", id, e),
            )
        })?;

        if existing_folder.owner_id() != Some(caller_id) {
            tracing::warn!(
                "rename_folder: user '{}' attempted to rename folder '{}' owned by '{:?}'",
                caller_id,
                id,
                existing_folder.owner_id()
            );
            return Err(DomainError::not_found("Folder", id));
        }

        // Rename folder — UPDATE RETURNING gives us the updated row directly
        let folder = self
            .folder_storage
            .rename_folder(id, dto.name)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!("Failed to rename folder with ID: {}: {}", id, e),
                )
            })?;

        Ok(FolderDto::from(folder))
    }

    /// Moves a folder to a new parent after verifying ownership.
    async fn move_folder(
        &self,
        id: &str,
        dto: MoveFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        // Verify the source folder exists and belongs to the caller
        let source_folder = self.folder_storage.get_folder(id).await.map_err(|e| {
            DomainError::internal_error(
                "FolderStorage",
                format!("Failed to get folder with ID: {} for moving: {}", id, e),
            )
        })?;

        if source_folder.owner_id() != Some(caller_id) {
            tracing::warn!(
                "move_folder: user '{}' attempted to move folder '{}' owned by '{:?}'",
                caller_id,
                id,
                source_folder.owner_id()
            );
            return Err(DomainError::not_found("Folder", id));
        }

        // If a parent_id is specified, verify it exists
        if let Some(parent_id) = &dto.parent_id {
            // Verify we are not trying to move the folder into itself or one of its descendants
            if parent_id == id {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "Folder",
                    "Cannot move a folder into itself",
                ));
            }

            // Verify the destination exists
            let parent_exists = self.folder_storage.get_folder(parent_id).await.is_ok();
            if !parent_exists {
                return Err(DomainError::not_found("Folder", parent_id));
            }

            // TODO: Ideally we should verify the entire hierarchy to prevent cycles
        }

        // Move folder — UPDATE RETURNING gives us the updated row directly
        let parent_ref = dto.parent_id.as_deref();
        let folder = self
            .folder_storage
            .move_folder(id, parent_ref)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!("Failed to move folder with ID: {}: {}", id, e),
                )
            })?;

        Ok(FolderDto::from(folder))
    }

    /// Deletes a folder after verifying ownership.
    async fn delete_folder(&self, id: &str, caller_id: Uuid) -> Result<(), DomainError> {
        // Verify the folder exists and belongs to the caller
        let folder = self.folder_storage.get_folder(id).await.map_err(|e| {
            DomainError::internal_error(
                "FolderStorage",
                format!("Failed to get folder with ID: {} for deletion: {}", id, e),
            )
        })?;

        if folder.owner_id() != Some(caller_id) {
            tracing::warn!(
                "delete_folder: user '{}' attempted to delete folder '{}' owned by '{:?}'",
                caller_id,
                id,
                folder.owner_id()
            );
            return Err(DomainError::not_found("Folder", id));
        }

        // Delete the folder
        self.folder_storage.delete_folder(id).await.map_err(|e| {
            DomainError::internal_error(
                "FolderStorage",
                format!("Failed to delete folder with ID: {}: {}", id, e),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_without_resolver() -> FolderService {
        FolderService::new(Arc::new(FolderDbRepository::new_stub()), None)
    }

    #[tokio::test]
    async fn ensure_path_rejects_empty_path() {
        let svc = service_without_resolver();
        // No resolver is fine here — the empty-path guard comes first only
        // *after* the resolver check, so we instead use a path with at
        // least one segment but expect the no-resolver error to surface.
        let err = svc
            .ensure_path("foo", Uuid::new_v4())
            .await
            .expect_err("must fail without a path resolver");
        assert_eq!(err.kind, ErrorKind::InternalError);
        assert!(err.message.contains("PathResolverService"));
    }

    #[tokio::test]
    async fn ensure_path_without_resolver_is_internal_error() {
        // This guards against accidentally constructing FolderService
        // with `path_resolver: None` in a production code path, which
        // would silently disable MKCOL.
        let svc = service_without_resolver();
        let err = svc
            .ensure_path("a/b/c", Uuid::new_v4())
            .await
            .expect_err("must fail without a path resolver");
        assert_eq!(err.kind, ErrorKind::InternalError);
    }
}
