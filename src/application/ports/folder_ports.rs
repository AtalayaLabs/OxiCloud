/// Primary port for folder operations
use uuid::Uuid;

use crate::application::dtos::folder_dto::{
    CreateFolderDto, FolderDto, MoveFolderDto, RenameFolderDto,
};

use crate::common::errors::DomainError;
use crate::domain::services::authorization::{Permission, Subject};

pub trait FolderUseCase: Send + Sync + 'static {
    /// Takes a **set** of [`Subject`]s rather than a bare `caller_id: Uuid` —
    /// one HTTP caller can hold several credentials at once, and a
    /// public-share visitor authorises as `Subject::Token(share_id)`, which
    /// the engine already understands. Returns the credential that granted,
    /// so a handler making a further single-subject call for the same folder
    /// passes back the one that worked. See the note on
    /// `FileManagementUseCase::require_permission`.
    async fn require_permission(
        &self,
        callers: &[Subject],
        permission: Permission,
        folder_id: &str,
    ) -> Result<Subject, DomainError>;

    /// Creates a new folder
    async fn create_folder_with_perms(
        &self,
        dto: CreateFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError>;

    /// Gets a folder by its ID
    async fn get_folder(&self, id: &str) -> Result<FolderDto, DomainError>;

    /// Gets a folder by its ID, enforcing that `caller_id` is the owner.
    ///
    /// Returns `NotFound` if the folder does not exist **or** belongs to
    /// another user.  All user-facing handlers should use this method.
    /// Takes a [`Subject`], not a `Uuid`: this is on the anonymous allowlist,
    /// so the caller may be a share token. Pass the subject that actually
    /// granted (see [`Self::require_permission`]) rather than re-deriving one.
    async fn get_folder_with_perms(
        &self,
        id: &str,
        caller: Subject,
    ) -> Result<FolderDto, DomainError>;

    /// Gets a folder by its path within the caller's tree.
    ///
    /// Scoped by `drive_id` because `storage.folders.path` is unique
    /// only within a single drive after D0 — multiple drives (whether
    /// owned by the same user or different users) share names like
    /// `"Personal"` for their root folder (docs/plan/drive.md §10).
    /// Pre-D0 the wrapper name embedded the username; post-D0 the
    /// caller derives a `drive_id` from its protocol context (NC
    /// chroot, native default-drive lookup, WOPI default-drive).
    async fn get_folder_by_path(
        &self,
        path: &str,
        drive_id: Uuid,
    ) -> Result<FolderDto, DomainError>;

    /// Lists folders within a parent folder
    async fn list_folders(&self, parent_id: Option<&str>) -> Result<Vec<FolderDto>, DomainError>;

    /// Lists folders scoped to a specific owner (for user-facing endpoints).
    /// At root level, only returns folders belonging to this user.
    async fn list_folders_with_perms(
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
    async fn list_folders_paginated_with_perms(
        &self,
        parent_id: Option<&str>,
        owner_id: Uuid,
        pagination: &crate::application::dtos::pagination::PaginationRequestDto,
    ) -> Result<crate::application::dtos::pagination::PaginatedResponseDto<FolderDto>, DomainError>;

    /// Keyset-paged sub-folder listing in name order, scoped to a caller —
    /// `name > after_name LIMIT limit`, `has_next = len() == limit`.
    ///
    /// Used by streaming WebDAV/NC PROPFIND: O(page) per page off the
    /// `idx_folders_unique_name` index instead of the quadratic
    /// `COUNT(*) OVER() … LIMIT/OFFSET` walk (benches/FOLDER-KEYSET.md).
    ///
    /// The default implementation falls back to `list_folders_with_perms`
    /// + in-memory slice so stubs and mocks compile without changes.
    async fn list_folders_batch_with_perms(
        &self,
        parent_id: Option<&str>,
        caller_id: Uuid,
        after_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FolderDto>, DomainError> {
        let mut all = self.list_folders_with_perms(parent_id, caller_id).await?;
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all
            .into_iter()
            .filter(|f| after_name.is_none_or(|a| f.name.as_str() > a))
            .take(limit)
            .collect())
    }

    /// Renames a folder (ownership verified against caller_id)
    async fn rename_folder_with_perms(
        &self,
        id: &str,
        dto: RenameFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError>;

    /// Moves a folder to another parent (ownership verified against caller_id)
    async fn move_folder_with_perms(
        &self,
        id: &str,
        dto: MoveFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError>;

    /// Deletes a folder (ownership verified against caller_id)
    async fn delete_folder_with_perms(&self, id: &str, caller_id: Uuid) -> Result<(), DomainError>;

    /// Lists every folder in a subtree rooted at `folder_id` (inclusive),
    /// ordered by path.  Uses ltree `<@` — single GiST-indexed query.
    ///
    /// Default: returns an empty vec (stubs / mocks).
    async fn list_subtree_folders(&self, folder_id: &str) -> Result<Vec<FolderDto>, DomainError> {
        let _ = folder_id;
        Ok(Vec::new())
    }
}
