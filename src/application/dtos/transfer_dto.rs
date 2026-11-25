use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub use crate::application::dtos::favorites_dto::ItemKind;

/// Maximum number of top-level IDs accepted by a single transfer preflight or execute request.
pub const MAX_TRANSFER_ITEMS: usize = 1_000;

fn default_true() -> bool {
    true
}

/// Kind of internal transfer operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferOperation {
    Move,
    Copy,
}

/// Request body of `POST /api/folders/{id}/transfer-preflight`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TransferPreflightRequest {
    pub operation: TransferOperation,
    #[serde(default)]
    pub file_ids: Vec<String>,
    #[serde(default)]
    pub folder_ids: Vec<String>,
    /// When `true` (default), recursive folder↔folder collisions expand
    /// nested file and kind conflicts across both subtrees.
    #[serde(default = "default_true")]
    pub expand_folder_conflicts: bool,
}

/// File↔file collision found during transfer preflight (top-level or inside a merged folder).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransferFileConflictDto {
    pub source_id: String,
    pub source_name: String,
    pub relative_path: String,
    pub source_size: u64,
    pub source_modified_at: DateTime<Utc>,
    pub source_hash: String,
    pub target_id: String,
    pub target_folder_id: String,
    pub target_size: u64,
    pub target_modified_at: DateTime<Utc>,
    pub target_hash: String,
    pub identical: bool,
    pub can_update_target: bool,
    pub can_delete_source: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_folder_conflict_id: Option<String>,
}

/// Folder↔folder collision found during transfer preflight.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransferFolderConflictDto {
    pub source_id: String,
    pub source_name: String,
    pub relative_path: String,
    pub target_id: String,
    pub source_file_count: u64,
    pub target_file_count: u64,
    pub nested_conflict_count: u64,
    pub nested_identical_count: u64,
    pub source_has_shares: bool,
    pub can_delete_source: bool,
}

/// File↔folder or folder↔file kind mismatch preventing merge or replacement.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransferKindConflictDto {
    pub source_id: String,
    pub source_name: String,
    pub source_kind: ItemKind,
    pub relative_path: String,
    pub existing_id: String,
    pub existing_kind: ItemKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_folder_conflict_id: Option<String>,
}

/// Response body of `POST /api/folders/{id}/transfer-preflight`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransferPreflightResponse {
    /// `true` when all sources and destination are native PostgreSQL items;
    /// `false` for external mounts (`ext:` / mount root) where the client falls
    /// back to sequential move/copy endpoints.
    pub supported: bool,
    /// Folder IDs that would move/copy into themselves or one of their descendants.
    pub cycle_folder_ids: Vec<String>,
    /// File IDs already directly inside the destination folder.
    pub same_parent_file_ids: Vec<String>,
    /// Folder IDs already directly inside the destination folder.
    pub same_parent_folder_ids: Vec<String>,
    pub file_conflicts: Vec<TransferFileConflictDto>,
    pub folder_conflicts: Vec<TransferFolderConflictDto>,
    pub kind_conflicts: Vec<TransferKindConflictDto>,
}

/// Per-item resolution selected by the user for a conflicting transfer item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferDecisionAction {
    Skip,
    Replace,
    KeepBoth,
    Merge,
}

/// Single user decision keyed by `(kind, source_id)`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TransferItemDecision {
    pub source_id: String,
    pub kind: ItemKind,
    pub action: TransferDecisionAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_hash: Option<String>,
}

/// Request body of `POST /api/folders/{id}/transfer`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TransferExecuteRequest {
    pub operation: TransferOperation,
    #[serde(default)]
    pub file_ids: Vec<String>,
    #[serde(default)]
    pub folder_ids: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<TransferItemDecision>,
}

/// Outcome status of one processed item during `POST /api/folders/{id}/transfer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferOutcomeStatus {
    Moved,
    Copied,
    Replaced,
    Renamed,
    Merged,
    Skipped,
    Failed,
}

/// Per-item execution report.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransferItemOutcome {
    pub source_id: String,
    pub kind: ItemKind,
    pub status: TransferOutcomeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
}

/// Response body of `POST /api/folders/{id}/transfer`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ToSchema)]
pub struct TransferExecuteResponse {
    pub outcomes: Vec<TransferItemOutcome>,
    pub moved_count: u64,
    pub copied_count: u64,
    pub replaced_count: u64,
    pub renamed_count: u64,
    pub merged_count: u64,
    pub skipped_count: u64,
    pub failed_count: u64,
}
