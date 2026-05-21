use std::sync::Arc;

use crate::domain::entities::folder::Folder;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// DTO for folder creation requests
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateFolderDto {
    /// Name of the folder to create
    pub name: String,

    /// Parent folder ID (None for root level)
    pub parent_id: Option<String>,
}

/// DTO for folder rename requests
#[derive(Debug, Deserialize, ToSchema)]
pub struct RenameFolderDto {
    /// New name for the folder
    pub name: String,
}

/// DTO for folder move requests
#[derive(Debug, Deserialize, ToSchema)]
pub struct MoveFolderDto {
    /// New parent folder ID (None for root level)
    pub parent_id: Option<String>,
}

/// Outcome of `FolderUseCase::ensure_path`. Distinguishes whether the
/// leaf folder of the requested path was newly created or already existed,
/// which is exactly the information WebDAV `MKCOL` needs to choose between
/// `201 Created` and `405 Method Not Allowed` (RFC 4918 §9.3.1).
#[derive(Debug, Clone)]
pub enum EnsurePathOutcome {
    /// The leaf already existed when the request arrived.
    Existed(FolderDto),
    /// The leaf was created by this call.
    Created(FolderDto),
}

/// DTO for folder responses
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FolderDto {
    /// Folder ID
    pub id: String,

    /// Folder name
    pub name: String,

    /// Path to the folder (relative)
    pub path: String,

    /// Parent folder ID
    pub parent_id: Option<String>,

    /// Owner user ID (scopes visibility per user)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,

    /// Creation timestamp
    pub created_at: u64,

    /// Last modification timestamp
    pub modified_at: u64,

    /// Whether this is a root folder
    pub is_root: bool,

    // ── Pre-computed display fields (Arc<str>: always identical values) ──
    /// FontAwesome icon CSS class (always "fas fa-folder")
    #[schema(value_type = String)]
    pub icon_class: Arc<str>,

    /// Extra CSS class for icon styling (always "folder-icon")
    #[schema(value_type = String)]
    pub icon_special_class: Arc<str>,

    /// Human-readable category (always "Folder")
    #[schema(value_type = String)]
    pub category: Arc<str>,
}

impl From<Folder> for FolderDto {
    fn from(folder: Folder) -> Self {
        let is_root = folder.parent_id().is_none();

        Self {
            id: folder.id().to_string(),
            name: folder.name().to_string(),
            path: folder.path_string().to_string(),
            parent_id: folder.parent_id().map(String::from),
            owner_id: folder.owner_id().map(|u| u.to_string()),
            created_at: folder.created_at(),
            modified_at: folder.modified_at(),
            is_root,
            icon_class: Arc::from("fas fa-folder"),
            icon_special_class: Arc::from("folder-icon"),
            category: Arc::from("Folder"),
        }
    }
}

// To convert from FolderDto to Folder for batch handlers
impl From<FolderDto> for Folder {
    fn from(dto: FolderDto) -> Self {
        // Display fields (icon_class, icon_special_class, category)
        // are not part of the domain entity and are ignored.
        Folder::from_dto(
            dto.id,
            dto.name,
            dto.path,
            dto.parent_id,
            dto.created_at,
            dto.modified_at,
        )
    }
}

impl FolderDto {
    /// Creates an empty folder DTO for stub implementations
    pub fn empty() -> Self {
        Self {
            id: "stub-id".to_string(),
            name: "stub-folder".to_string(),
            path: "/stub/path".to_string(),
            parent_id: None,
            owner_id: None,
            created_at: 0,
            modified_at: 0,
            is_root: true,
            icon_class: Arc::from("fas fa-folder"),
            icon_special_class: Arc::from("folder-icon"),
            category: Arc::from("Folder"),
        }
    }
}

impl Default for FolderDto {
    fn default() -> Self {
        Self::empty()
    }
}
