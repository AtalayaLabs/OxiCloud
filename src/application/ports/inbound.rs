use std::sync::Arc;

use uuid::Uuid;

use crate::application::dtos::folder_dto::{
    CreateFolderDto, EnsurePathOutcome, FolderDto, MoveFolderDto, RenameFolderDto,
};
use crate::application::dtos::search_dto::{
    SearchCriteriaDto, SearchResultsDto, SearchSuggestionsDto,
};
use crate::common::errors::DomainError;

/// Primary port for folder operations
pub trait FolderUseCase: Send + Sync + 'static {
    /// Creates a new folder
    async fn create_folder(&self, dto: CreateFolderDto) -> Result<FolderDto, DomainError>;

    /// Ensures every segment of `path` exists as a folder owned by `owner`,
    /// creating missing intermediates as needed. Path lookup is owner-scoped:
    /// folders belonging to other users are invisible.
    ///
    /// Returns whether the *leaf* segment was newly created or already existed,
    /// so that WebDAV `MKCOL` handlers can distinguish `201 Created` from
    /// `405 Method Not Allowed` (RFC 4918 §9.3.1).
    ///
    /// Returns `AlreadyExists` if the leaf path resolves to a non-folder
    /// resource (i.e. a file): MKCOL on an existing resource of any kind
    /// must be 405, not 201.
    async fn ensure_path(&self, path: &str, owner: Uuid) -> Result<EnsurePathOutcome, DomainError>;

    /// Gets a folder by its ID
    async fn get_folder(&self, id: &str) -> Result<FolderDto, DomainError>;

    /// Gets a folder by its ID, enforcing that `caller_id` is the owner.
    ///
    /// Returns `NotFound` if the folder does not exist **or** belongs to
    /// another user.  All user-facing handlers should use this method.
    async fn get_folder_owned(&self, id: &str, caller_id: Uuid) -> Result<FolderDto, DomainError>;

    /// Gets a folder by its path
    async fn get_folder_by_path(&self, path: &str) -> Result<FolderDto, DomainError>;

    /// Lists folders within a parent folder
    async fn list_folders(&self, parent_id: Option<&str>) -> Result<Vec<FolderDto>, DomainError>;

    /// Lists folders scoped to a specific owner (for user-facing endpoints).
    /// At root level, only returns folders belonging to this user.
    async fn list_folders_for_owner(
        &self,
        parent_id: Option<&str>,
        owner_id: Uuid,
    ) -> Result<Vec<FolderDto>, DomainError>;

    /// Lists folders with pagination
    async fn list_folders_paginated(
        &self,
        parent_id: Option<&str>,
        pagination: &crate::application::dtos::pagination::PaginationRequestDto,
    ) -> Result<crate::application::dtos::pagination::PaginatedResponseDto<FolderDto>, DomainError>;

    /// Lists folders with pagination, scoped to a specific owner.
    async fn list_folders_for_owner_paginated(
        &self,
        parent_id: Option<&str>,
        owner_id: Uuid,
        pagination: &crate::application::dtos::pagination::PaginationRequestDto,
    ) -> Result<crate::application::dtos::pagination::PaginatedResponseDto<FolderDto>, DomainError>;

    /// Renames a folder (ownership verified against caller_id)
    async fn rename_folder(
        &self,
        id: &str,
        dto: RenameFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError>;

    /// Moves a folder to another parent (ownership verified against caller_id)
    async fn move_folder(
        &self,
        id: &str,
        dto: MoveFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError>;

    /// Deletes a folder (ownership verified against caller_id)
    async fn delete_folder(&self, id: &str, caller_id: Uuid) -> Result<(), DomainError>;

    /// Creates a root-level home folder for a user during registration.
    async fn create_home_folder(
        &self,
        user_id: Uuid,
        name: String,
    ) -> Result<FolderDto, DomainError>;

    /// Lists every folder in a subtree rooted at `folder_id` (inclusive),
    /// ordered by path.  Uses ltree `<@` — single GiST-indexed query.
    ///
    /// Default: returns an empty vec (stubs / mocks).
    async fn list_subtree_folders(&self, folder_id: &str) -> Result<Vec<FolderDto>, DomainError> {
        let _ = folder_id;
        Ok(Vec::new())
    }
}

/**
 * Primary port for file and folder search.
 *
 * All search processing (filtering, scoring, sorting, categorization)
 * is handled server-side in Rust for maximum efficiency.
 */
pub trait SearchUseCase: Send + Sync + 'static {
    /// Performs a full search based on the specified criteria.
    ///
    /// Returns `Arc<SearchResultsDto>` so the cache and the caller share
    /// the same allocation — zero-copy on both insert and hit.
    /// `user_id` identifies the authenticated user so that SQL queries filter
    /// by owner and the result cache is isolated per tenant.
    async fn search(
        &self,
        criteria: SearchCriteriaDto,
        user_id: Uuid,
    ) -> Result<Arc<SearchResultsDto>, DomainError>;

    /// Returns quick suggestions for autocomplete (lightweight, fast).
    async fn suggest(
        &self,
        query: &str,
        folder_id: Option<&str>,
        limit: usize,
    ) -> Result<SearchSuggestionsDto, DomainError>;

    /// Clears the search results cache.
    async fn clear_search_cache(&self) -> Result<(), DomainError>;
}
