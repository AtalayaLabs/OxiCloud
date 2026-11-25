use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::folder_dto::MoveFolderDto;
use crate::application::dtos::timestamp_helpers::epoch_to_datetime;
use crate::application::dtos::transfer_dto::{
    ItemKind, MAX_TRANSFER_ITEMS, TransferDecisionAction, TransferExecuteRequest,
    TransferExecuteResponse, TransferFileConflictDto, TransferFolderConflictDto,
    TransferItemDecision, TransferItemOutcome, TransferKindConflictDto, TransferOperation,
    TransferOutcomeStatus, TransferPreflightRequest, TransferPreflightResponse,
};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::file_ports::FileManagementUseCase;
use crate::application::ports::folder_ports::FolderUseCase;
use crate::application::ports::storage_ports::FileReadPort;
use crate::application::services::external_mount_router::{MountRouter, ResolvedId};
use crate::application::services::file_management_service::FileManagementService;
use crate::application::services::folder_service::FolderService;
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::domain::services::name_conflict::ConflictPolicy;
use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
use crate::infrastructure::repositories::pg::folder_db_repository::{
    FolderDbRepository, TransferSubtreeFileRow, TransferSubtreeSnapshot,
};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

/// Application service orchestrating conflict-aware Move and Copy transfers
/// (preflight comparison + resolved execution including recursive folder merge).
pub struct TransferService {
    folder_storage: Arc<FolderDbRepository>,
    file_read: Arc<FileBlobReadRepository>,
    file_management: Arc<FileManagementService>,
    folder_service: Arc<FolderService>,
    authz: Arc<PgAclEngine>,
    mount_router: Arc<MountRouter>,
}

impl TransferService {
    pub fn new(
        folder_storage: Arc<FolderDbRepository>,
        file_read: Arc<FileBlobReadRepository>,
        file_management: Arc<FileManagementService>,
        folder_service: Arc<FolderService>,
        authz: Arc<PgAclEngine>,
        mount_router: Arc<MountRouter>,
    ) -> Self {
        Self {
            folder_storage,
            file_read,
            file_management,
            folder_service,
            authz,
            mount_router,
        }
    }

    /// Analyze potential name, kind, permission, same-parent, and cycle
    /// conflicts before executing a move or copy into `dest_folder_id`.
    pub async fn preflight_transfer_with_perms(
        &self,
        caller_id: Uuid,
        dest_folder_id: &str,
        request: TransferPreflightRequest,
    ) -> Result<TransferPreflightResponse, DomainError> {
        let (file_ids, folder_ids) =
            validate_and_dedup_ids(&request.file_ids, &request.folder_ids)?;

        let dest_resource = self.folder_authz_resource(dest_folder_id)?;
        self.authz
            .require(Subject::User(caller_id), Permission::Create, dest_resource)
            .await?;

        let all_native = matches!(
            self.mount_router.classify(dest_folder_id),
            ResolvedId::Regular
        ) && file_ids
            .iter()
            .all(|id| matches!(self.mount_router.classify(id), ResolvedId::Regular))
            && folder_ids
                .iter()
                .all(|id| matches!(self.mount_router.classify(id), ResolvedId::Regular));

        if !all_native {
            return Ok(TransferPreflightResponse {
                supported: false,
                cycle_folder_ids: Vec::new(),
                same_parent_file_ids: Vec::new(),
                same_parent_folder_ids: Vec::new(),
                file_conflicts: Vec::new(),
                folder_conflicts: Vec::new(),
                kind_conflicts: Vec::new(),
            });
        }

        let dest_uuid = Uuid::parse_str(dest_folder_id)
            .map_err(|_| DomainError::not_found("Folder", dest_folder_id))?;

        let source_perm = match request.operation {
            TransferOperation::Move => Permission::Update,
            TransferOperation::Copy => Permission::Read,
        };

        let mut src_files = Vec::with_capacity(file_ids.len());
        for fid in &file_ids {
            let uuid = Uuid::parse_str(fid).map_err(|_| DomainError::not_found("File", fid))?;
            self.authz
                .require(Subject::User(caller_id), source_perm, Resource::File(uuid))
                .await?;
            src_files.push(self.file_read.get_file(fid).await?);
        }

        let mut src_folders = Vec::with_capacity(folder_ids.len());
        for did in &folder_ids {
            let uuid = Uuid::parse_str(did).map_err(|_| DomainError::not_found("Folder", did))?;
            self.authz
                .require(
                    Subject::User(caller_id),
                    source_perm,
                    Resource::Folder(uuid),
                )
                .await?;
            src_folders.push(self.folder_storage.get_folder(did).await?);
        }

        let dest_children = self
            .folder_storage
            .list_direct_children_for_transfer(dest_uuid)
            .await?;

        let dst_folder_by_name: HashMap<&str, Uuid> = dest_children
            .folders
            .iter()
            .map(|(id, name)| (name.as_str(), *id))
            .collect();
        let dst_file_by_name: HashMap<&str, &TransferSubtreeFileRow> = dest_children
            .files
            .iter()
            .map(|f| (f.name.as_str(), f))
            .collect();

        let mut cycle_folder_ids = Vec::new();
        let mut same_parent_file_ids = Vec::new();
        let mut same_parent_folder_ids = Vec::new();
        let mut file_conflicts = Vec::new();
        let mut folder_conflicts = Vec::new();
        let mut kind_conflicts = Vec::new();

        for file in src_files {
            if file.folder_id() == Some(dest_folder_id) {
                same_parent_file_ids.push(file.id().to_string());
                continue;
            }
            if let Some(&blocking_folder_id) = dst_folder_by_name.get(file.name()) {
                kind_conflicts.push(TransferKindConflictDto {
                    source_id: file.id().to_string(),
                    source_name: file.name().to_string(),
                    source_kind: ItemKind::File,
                    relative_path: file.name().to_string(),
                    existing_id: blocking_folder_id.to_string(),
                    existing_kind: ItemKind::Folder,
                    parent_folder_conflict_id: None,
                });
                continue;
            }
            if let Some(&dst_file) = dst_file_by_name.get(file.name()) {
                let (can_update_target, can_delete_source) = self
                    .resolve_file_conflict_caps(
                        caller_id,
                        file.id(),
                        dst_file.id,
                        request.operation,
                    )
                    .await;
                let src_hash = file.content_hash().to_string();
                let dst_hash = dst_file.blob_hash.clone();
                file_conflicts.push(TransferFileConflictDto {
                    source_id: file.id().to_string(),
                    source_name: file.name().to_string(),
                    relative_path: file.name().to_string(),
                    source_size: file.size(),
                    source_modified_at: epoch_to_datetime(file.modified_at()),
                    source_hash: src_hash.clone(),
                    target_id: dst_file.id.to_string(),
                    target_folder_id: dst_file.folder_id.to_string(),
                    target_size: dst_file.size.max(0) as u64,
                    target_modified_at: epoch_to_datetime(dst_file.modified_at.max(0) as u64),
                    target_hash: dst_hash.clone(),
                    identical: src_hash == dst_hash,
                    can_update_target,
                    can_delete_source,
                    parent_folder_conflict_id: None,
                });
            }
        }

        for folder in src_folders {
            let src_uuid = Uuid::parse_str(folder.id())
                .map_err(|_| DomainError::not_found("Folder", folder.id()))?;

            if self
                .folder_storage
                .is_descendant_or_same(src_uuid, dest_uuid)
                .await?
            {
                cycle_folder_ids.push(folder.id().to_string());
                continue;
            }

            if folder.parent_id() == Some(dest_folder_id) {
                same_parent_folder_ids.push(folder.id().to_string());
                continue;
            }

            if let Some(&blocking_file) = dst_file_by_name.get(folder.name()) {
                kind_conflicts.push(TransferKindConflictDto {
                    source_id: folder.id().to_string(),
                    source_name: folder.name().to_string(),
                    source_kind: ItemKind::Folder,
                    relative_path: folder.name().to_string(),
                    existing_id: blocking_file.id.to_string(),
                    existing_kind: ItemKind::File,
                    parent_folder_conflict_id: None,
                });
                continue;
            }

            if let Some(&dst_folder_id) = dst_folder_by_name.get(folder.name()) {
                let (src_snap, dst_snap, source_has_shares, can_delete_source) = tokio::try_join!(
                    self.folder_storage.list_subtree_for_transfer(src_uuid),
                    self.folder_storage.list_subtree_for_transfer(dst_folder_id),
                    self.folder_storage.folder_has_shares_or_grants(src_uuid),
                    async {
                        Ok(self
                            .authz
                            .check(
                                Subject::User(caller_id),
                                Permission::Delete,
                                Resource::Folder(src_uuid),
                            )
                            .await
                            .unwrap_or(false))
                    }
                )?;

                let (nested_files, nested_kinds, nested_conflict_count, nested_identical_count) =
                    self.diff_subtrees_for_merge(
                        caller_id,
                        request.operation,
                        folder.id(),
                        folder.name(),
                        &src_snap,
                        &dst_snap,
                    )
                    .await;

                folder_conflicts.push(TransferFolderConflictDto {
                    source_id: folder.id().to_string(),
                    source_name: folder.name().to_string(),
                    relative_path: folder.name().to_string(),
                    target_id: dst_folder_id.to_string(),
                    source_file_count: src_snap.files.len() as u64,
                    target_file_count: dst_snap.files.len() as u64,
                    nested_conflict_count,
                    nested_identical_count,
                    source_has_shares,
                    can_delete_source,
                });

                if request.expand_folder_conflicts {
                    file_conflicts.extend(nested_files);
                    kind_conflicts.extend(nested_kinds);
                }
            }
        }

        Ok(TransferPreflightResponse {
            supported: true,
            cycle_folder_ids,
            same_parent_file_ids,
            same_parent_folder_ids,
            file_conflicts,
            folder_conflicts,
            kind_conflicts,
        })
    }

    /// Execute a resolved move or copy transfer into `dest_folder_id`.
    pub async fn execute_transfer_with_perms(
        &self,
        caller_id: Uuid,
        dest_folder_id: &str,
        request: TransferExecuteRequest,
    ) -> Result<TransferExecuteResponse, DomainError> {
        let (file_ids, folder_ids) =
            validate_and_dedup_ids(&request.file_ids, &request.folder_ids)?;

        if !matches!(
            self.mount_router.classify(dest_folder_id),
            ResolvedId::Regular
        ) || file_ids
            .iter()
            .any(|id| !matches!(self.mount_router.classify(id), ResolvedId::Regular))
            || folder_ids
                .iter()
                .any(|id| !matches!(self.mount_router.classify(id), ResolvedId::Regular))
        {
            return Err(DomainError::operation_not_supported(
                "Folder",
                "conflict-resolved transfers are not supported on external mounts",
            ));
        }

        let dest_resource = self.folder_authz_resource(dest_folder_id)?;
        self.authz
            .require(Subject::User(caller_id), Permission::Create, dest_resource)
            .await?;

        let dest_uuid = Uuid::parse_str(dest_folder_id)
            .map_err(|_| DomainError::not_found("Folder", dest_folder_id))?;

        let mut decisions: HashMap<(ItemKind, String), TransferItemDecision> =
            HashMap::with_capacity(request.decisions.len());
        for d in request.decisions {
            decisions.insert((d.kind, d.source_id.clone()), d);
        }

        let mut response = TransferExecuteResponse::default();
        let dest_children = self
            .folder_storage
            .list_direct_children_for_transfer(dest_uuid)
            .await?;
        let mut dst_folder_by_name: HashMap<String, Uuid> = dest_children
            .folders
            .into_iter()
            .map(|(id, name)| (name, id))
            .collect();
        let mut dst_file_by_name: HashMap<String, TransferSubtreeFileRow> = dest_children
            .files
            .into_iter()
            .map(|f| (f.name.clone(), f))
            .collect();

        // 1. Top-level files
        let source_perm = match request.operation {
            TransferOperation::Move => Permission::Update,
            TransferOperation::Copy => Permission::Read,
        };
        for fid in file_ids {
            let file_uuid = match Uuid::parse_str(&fid) {
                Ok(u) => u,
                Err(_) => {
                    record_error(
                        &mut response,
                        fid.clone(),
                        ItemKind::File,
                        DomainError::not_found("File", fid),
                    );
                    continue;
                }
            };
            if let Err(e) = self
                .authz
                .require(
                    Subject::User(caller_id),
                    source_perm,
                    Resource::File(file_uuid),
                )
                .await
            {
                record_error(&mut response, fid, ItemKind::File, e);
                continue;
            }
            let file = match self.file_read.get_file(&fid).await {
                Ok(f) => f,
                Err(e) => {
                    record_error(&mut response, fid, ItemKind::File, e);
                    continue;
                }
            };

            let decision = decisions.get(&(ItemKind::File, fid.clone()));

            // Same-parent rule: no-op on Move; direct Duplicate (`name (2).ext`) on Copy.
            if file.folder_id() == Some(dest_folder_id) {
                if matches!(
                    decision.map(|d| d.action),
                    Some(TransferDecisionAction::Skip)
                ) || request.operation == TransferOperation::Move
                {
                    record_outcome(
                        &mut response,
                        TransferItemOutcome {
                            source_id: fid,
                            kind: ItemKind::File,
                            status: TransferOutcomeStatus::Skipped,
                            target_id: None,
                            final_name: Some(file.name().to_string()),
                            error: None,
                            error_type: None,
                        },
                    );
                    continue;
                }
                let res = self
                    .file_management
                    .copy_file_with_perms_on_conflict(
                        &fid,
                        caller_id,
                        Some(dest_folder_id.to_string()),
                        None,
                        ConflictPolicy::Rename,
                    )
                    .await;
                record_file_result(&mut response, fid, res, TransferOutcomeStatus::Renamed);
                continue;
            }

            if dst_folder_by_name.contains_key(file.name()) {
                match decision.map(|d| d.action) {
                    Some(TransferDecisionAction::KeepBoth) => {
                        let res = self
                            .transfer_single_file(
                                caller_id,
                                &fid,
                                dest_folder_id,
                                request.operation,
                                ConflictPolicy::Rename,
                            )
                            .await;
                        record_file_result(&mut response, fid, res, TransferOutcomeStatus::Renamed);
                    }
                    _ => record_skipped(&mut response, fid, ItemKind::File, file.name()),
                }
                continue;
            }

            if let Some(existing) = dst_file_by_name.get(file.name()).cloned() {
                match decision.map(|d| d.action) {
                    Some(TransferDecisionAction::Replace) => {
                        let expected_hash = decision.and_then(|d| d.expected_hash.as_deref());
                        let is_move = request.operation == TransferOperation::Move;
                        let res = self
                            .file_management
                            .replace_file_from_source_with_perms(
                                &fid,
                                &existing.id.to_string(),
                                caller_id,
                                is_move,
                                expected_hash,
                            )
                            .await;
                        record_file_result(
                            &mut response,
                            fid,
                            res,
                            TransferOutcomeStatus::Replaced,
                        );
                    }
                    Some(TransferDecisionAction::KeepBoth) => {
                        let res = self
                            .transfer_single_file(
                                caller_id,
                                &fid,
                                dest_folder_id,
                                request.operation,
                                ConflictPolicy::Rename,
                            )
                            .await;
                        record_file_result(&mut response, fid, res, TransferOutcomeStatus::Renamed);
                    }
                    Some(TransferDecisionAction::Skip) | None => {
                        record_skipped(&mut response, fid, ItemKind::File, file.name());
                    }
                    Some(TransferDecisionAction::Merge) => {
                        record_error(
                            &mut response,
                            fid,
                            ItemKind::File,
                            DomainError::validation_error("merge action is only valid for folders"),
                        );
                    }
                }
                continue;
            }

            // If caller sent an explicit `expected_hash` replace even though target vanished, refuse with 412.
            if let Some(d) = decision
                && d.action == TransferDecisionAction::Replace
                && d.expected_hash.is_some()
            {
                record_error(
                    &mut response,
                    fid,
                    ItemKind::File,
                    DomainError::precondition_failed(
                        "File",
                        "target file was removed concurrently",
                    ),
                );
                continue;
            }
            if matches!(
                decision.map(|d| d.action),
                Some(TransferDecisionAction::Skip)
            ) {
                record_skipped(&mut response, fid, ItemKind::File, file.name());
                continue;
            }

            let ok_status = match request.operation {
                TransferOperation::Move => TransferOutcomeStatus::Moved,
                TransferOperation::Copy => TransferOutcomeStatus::Copied,
            };
            let res = self
                .transfer_single_file(
                    caller_id,
                    &fid,
                    dest_folder_id,
                    request.operation,
                    ConflictPolicy::Fail,
                )
                .await;
            if let Ok(ref dto) = res {
                // Keep snapshot accurate for subsequent items in same batch.
                if let Ok(uuid) = Uuid::parse_str(&dto.id) {
                    dst_file_by_name.insert(
                        dto.name.clone(),
                        TransferSubtreeFileRow {
                            id: uuid,
                            folder_id: dest_uuid,
                            name: dto.name.clone(),
                            size: dto.size as i64,
                            modified_at: dto.modified_at.timestamp(),
                            blob_hash: dto.content_hash.clone(),
                        },
                    );
                }
            }
            record_file_result(&mut response, fid, res, ok_status);
        }

        // 2. Top-level folders
        for did in folder_ids {
            let src_uuid = match Uuid::parse_str(&did) {
                Ok(u) => u,
                Err(_) => {
                    record_error(
                        &mut response,
                        did.clone(),
                        ItemKind::Folder,
                        DomainError::not_found("Folder", did),
                    );
                    continue;
                }
            };
            if let Err(e) = self
                .authz
                .require(
                    Subject::User(caller_id),
                    source_perm,
                    Resource::Folder(src_uuid),
                )
                .await
            {
                record_error(&mut response, did, ItemKind::Folder, e);
                continue;
            }
            let folder = match self.folder_storage.get_folder(&did).await {
                Ok(f) => f,
                Err(e) => {
                    record_error(&mut response, did, ItemKind::Folder, e);
                    continue;
                }
            };

            if self
                .folder_storage
                .is_descendant_or_same(src_uuid, dest_uuid)
                .await?
            {
                record_error(
                    &mut response,
                    did,
                    ItemKind::Folder,
                    DomainError::new(
                        ErrorKind::InvalidInput,
                        "Folder",
                        "Cannot move or copy a folder into itself or one of its subfolders",
                    ),
                );
                continue;
            }

            let decision = decisions.get(&(ItemKind::Folder, did.clone()));

            // Same-parent rule: no-op on Move; direct Duplicate (`Docs (2)`) on Copy.
            if folder.parent_id() == Some(dest_folder_id) {
                if matches!(
                    decision.map(|d| d.action),
                    Some(TransferDecisionAction::Skip)
                ) || request.operation == TransferOperation::Move
                {
                    record_skipped(&mut response, did, ItemKind::Folder, folder.name());
                    continue;
                }
                let res = self
                    .file_management
                    .copy_folder_tree_with_perms_on_conflict(
                        &did,
                        caller_id,
                        Some(dest_folder_id.to_string()),
                        None,
                        ConflictPolicy::Rename,
                    )
                    .await;
                match res {
                    Ok(copied) => {
                        let final_name = self
                            .folder_storage
                            .get_folder(&copied.new_root_folder_id)
                            .await
                            .ok()
                            .map(|f| f.name().to_string());
                        record_outcome(
                            &mut response,
                            TransferItemOutcome {
                                source_id: did,
                                kind: ItemKind::Folder,
                                status: TransferOutcomeStatus::Renamed,
                                target_id: Some(copied.new_root_folder_id),
                                final_name,
                                error: None,
                                error_type: None,
                            },
                        );
                    }
                    Err(e) => record_error(&mut response, did, ItemKind::Folder, e),
                }
                continue;
            }

            if dst_file_by_name.contains_key(folder.name()) {
                match decision.map(|d| d.action) {
                    Some(TransferDecisionAction::KeepBoth) => {
                        let res = self
                            .transfer_single_folder(
                                caller_id,
                                &did,
                                dest_folder_id,
                                request.operation,
                                ConflictPolicy::Rename,
                            )
                            .await;
                        match res {
                            Ok((target_id, final_name)) => record_outcome(
                                &mut response,
                                TransferItemOutcome {
                                    source_id: did,
                                    kind: ItemKind::Folder,
                                    status: TransferOutcomeStatus::Renamed,
                                    target_id: Some(target_id),
                                    final_name: Some(final_name),
                                    error: None,
                                    error_type: None,
                                },
                            ),
                            Err(e) => record_error(&mut response, did, ItemKind::Folder, e),
                        }
                    }
                    _ => record_skipped(&mut response, did, ItemKind::Folder, folder.name()),
                }
                continue;
            }

            if let Some(&existing_dst_folder) = dst_folder_by_name.get(folder.name()) {
                match decision.map(|d| d.action) {
                    Some(TransferDecisionAction::Merge) => {
                        let source_perm = match request.operation {
                            TransferOperation::Move => Permission::Update,
                            TransferOperation::Copy => Permission::Read,
                        };
                        if let Err(e) = self
                            .authz
                            .require(
                                Subject::User(caller_id),
                                source_perm,
                                Resource::Folder(src_uuid),
                            )
                            .await
                        {
                            record_error(&mut response, did, ItemKind::Folder, e);
                            continue;
                        }
                        if let Err(e) = self
                            .authz
                            .require(
                                Subject::User(caller_id),
                                Permission::Create,
                                Resource::Folder(existing_dst_folder),
                            )
                            .await
                        {
                            record_error(&mut response, did, ItemKind::Folder, e);
                            continue;
                        }

                        match self
                            .merge_folder_into(
                                caller_id,
                                src_uuid,
                                existing_dst_folder,
                                request.operation,
                                &decisions,
                                &mut response,
                            )
                            .await
                        {
                            Ok(()) => record_outcome(
                                &mut response,
                                TransferItemOutcome {
                                    source_id: did,
                                    kind: ItemKind::Folder,
                                    status: TransferOutcomeStatus::Merged,
                                    target_id: Some(existing_dst_folder.to_string()),
                                    final_name: Some(folder.name().to_string()),
                                    error: None,
                                    error_type: None,
                                },
                            ),
                            Err(e) => record_error(&mut response, did, ItemKind::Folder, e),
                        }
                    }
                    Some(TransferDecisionAction::KeepBoth) => {
                        let res = self
                            .transfer_single_folder(
                                caller_id,
                                &did,
                                dest_folder_id,
                                request.operation,
                                ConflictPolicy::Rename,
                            )
                            .await;
                        match res {
                            Ok((target_id, final_name)) => record_outcome(
                                &mut response,
                                TransferItemOutcome {
                                    source_id: did,
                                    kind: ItemKind::Folder,
                                    status: TransferOutcomeStatus::Renamed,
                                    target_id: Some(target_id),
                                    final_name: Some(final_name),
                                    error: None,
                                    error_type: None,
                                },
                            ),
                            Err(e) => record_error(&mut response, did, ItemKind::Folder, e),
                        }
                    }
                    _ => record_skipped(&mut response, did, ItemKind::Folder, folder.name()),
                }
                continue;
            }

            if matches!(
                decision.map(|d| d.action),
                Some(TransferDecisionAction::Skip)
            ) {
                record_skipped(&mut response, did, ItemKind::Folder, folder.name());
                continue;
            }

            let ok_status = match request.operation {
                TransferOperation::Move => TransferOutcomeStatus::Moved,
                TransferOperation::Copy => TransferOutcomeStatus::Copied,
            };
            match self
                .transfer_single_folder(
                    caller_id,
                    &did,
                    dest_folder_id,
                    request.operation,
                    ConflictPolicy::Fail,
                )
                .await
            {
                Ok((target_id, final_name)) => {
                    if let Ok(uuid) = Uuid::parse_str(&target_id) {
                        dst_folder_by_name.insert(final_name.clone(), uuid);
                    }
                    record_outcome(
                        &mut response,
                        TransferItemOutcome {
                            source_id: did,
                            kind: ItemKind::Folder,
                            status: ok_status,
                            target_id: Some(target_id),
                            final_name: Some(final_name),
                            error: None,
                            error_type: None,
                        },
                    );
                }
                Err(e) => record_error(&mut response, did, ItemKind::Folder, e),
            }
        }

        if response.failed_count > 0 {
            tracing::info!(
                target: "audit",
                event = "transfer.partial_or_rejected",
                reason = "item_failures",
                caller_id = %caller_id,
                dest_folder_id = %dest_folder_id,
                failed_count = response.failed_count,
                "👮🏻‍♂️ transfer completed with item failures",
            );
        }

        Ok(response)
    }

    fn merge_folder_into<'a>(
        &'a self,
        caller_id: Uuid,
        src_folder_id: Uuid,
        dst_folder_id: Uuid,
        operation: TransferOperation,
        decisions: &'a HashMap<(ItemKind, String), TransferItemDecision>,
        response: &'a mut TransferExecuteResponse,
    ) -> Pin<Box<dyn Future<Output = Result<(), DomainError>> + Send + 'a>> {
        Box::pin(async move {
            let src_children = self
                .folder_storage
                .list_direct_children_for_transfer(src_folder_id)
                .await?;
            let dst_children = self
                .folder_storage
                .list_direct_children_for_transfer(dst_folder_id)
                .await?;

            let dst_folders: HashMap<String, Uuid> = dst_children
                .folders
                .into_iter()
                .map(|(id, name)| (name, id))
                .collect();
            let dst_files: HashMap<String, TransferSubtreeFileRow> = dst_children
                .files
                .into_iter()
                .map(|f| (f.name.clone(), f))
                .collect();

            let dst_folder_str = dst_folder_id.to_string();

            // 1. Subfolders
            for (sub_src_id, sub_src_name) in src_children.folders {
                let sub_id_str = sub_src_id.to_string();
                let decision = decisions.get(&(ItemKind::Folder, sub_id_str.clone()));

                if let Some(&sub_dst_id) = dst_folders.get(&sub_src_name) {
                    if matches!(
                        decision.map(|d| d.action),
                        Some(TransferDecisionAction::Skip)
                    ) {
                        record_skipped(response, sub_id_str, ItemKind::Folder, &sub_src_name);
                        continue;
                    }
                    if matches!(
                        decision.map(|d| d.action),
                        Some(TransferDecisionAction::KeepBoth)
                    ) {
                        match self
                            .transfer_single_folder(
                                caller_id,
                                &sub_id_str,
                                &dst_folder_str,
                                operation,
                                ConflictPolicy::Rename,
                            )
                            .await
                        {
                            Ok((tid, final_name)) => record_outcome(
                                response,
                                TransferItemOutcome {
                                    source_id: sub_id_str,
                                    kind: ItemKind::Folder,
                                    status: TransferOutcomeStatus::Renamed,
                                    target_id: Some(tid),
                                    final_name: Some(final_name),
                                    error: None,
                                    error_type: None,
                                },
                            ),
                            Err(e) => record_error(response, sub_id_str, ItemKind::Folder, e),
                        }
                        continue;
                    }

                    // Default for matching folder inside a merge is recursive merge.
                    self.authz
                        .require(
                            Subject::User(caller_id),
                            Permission::Create,
                            Resource::Folder(sub_dst_id),
                        )
                        .await?;
                    self.merge_folder_into(
                        caller_id, sub_src_id, sub_dst_id, operation, decisions, response,
                    )
                    .await?;
                    continue;
                }

                if dst_files.contains_key(&sub_src_name) {
                    match decision.map(|d| d.action) {
                        Some(TransferDecisionAction::KeepBoth) => {
                            match self
                                .transfer_single_folder(
                                    caller_id,
                                    &sub_id_str,
                                    &dst_folder_str,
                                    operation,
                                    ConflictPolicy::Rename,
                                )
                                .await
                            {
                                Ok((tid, final_name)) => record_outcome(
                                    response,
                                    TransferItemOutcome {
                                        source_id: sub_id_str,
                                        kind: ItemKind::Folder,
                                        status: TransferOutcomeStatus::Renamed,
                                        target_id: Some(tid),
                                        final_name: Some(final_name),
                                        error: None,
                                        error_type: None,
                                    },
                                ),
                                Err(e) => record_error(response, sub_id_str, ItemKind::Folder, e),
                            }
                        }
                        _ => record_skipped(response, sub_id_str, ItemKind::Folder, &sub_src_name),
                    }
                    continue;
                }

                // Non-conflicting subfolder: transfer whole subtree in 1 call!
                let ok_status = match operation {
                    TransferOperation::Move => TransferOutcomeStatus::Moved,
                    TransferOperation::Copy => TransferOutcomeStatus::Copied,
                };
                match self
                    .transfer_single_folder(
                        caller_id,
                        &sub_id_str,
                        &dst_folder_str,
                        operation,
                        ConflictPolicy::Fail,
                    )
                    .await
                {
                    Ok((tid, final_name)) => record_outcome(
                        response,
                        TransferItemOutcome {
                            source_id: sub_id_str,
                            kind: ItemKind::Folder,
                            status: ok_status,
                            target_id: Some(tid),
                            final_name: Some(final_name),
                            error: None,
                            error_type: None,
                        },
                    ),
                    Err(e) => record_error(response, sub_id_str, ItemKind::Folder, e),
                }
            }

            // 2. Direct files
            for src_file in src_children.files {
                let file_id_str = src_file.id.to_string();
                let decision = decisions.get(&(ItemKind::File, file_id_str.clone()));

                if dst_folders.contains_key(&src_file.name) {
                    match decision.map(|d| d.action) {
                        Some(TransferDecisionAction::KeepBoth) => {
                            let res = self
                                .transfer_single_file(
                                    caller_id,
                                    &file_id_str,
                                    &dst_folder_str,
                                    operation,
                                    ConflictPolicy::Rename,
                                )
                                .await;
                            record_file_result(
                                response,
                                file_id_str,
                                res,
                                TransferOutcomeStatus::Renamed,
                            );
                        }
                        _ => record_skipped(response, file_id_str, ItemKind::File, &src_file.name),
                    }
                    continue;
                }

                if let Some(dst_file) = dst_files.get(&src_file.name) {
                    match decision.map(|d| d.action) {
                        Some(TransferDecisionAction::Replace) => {
                            let expected_hash = decision.and_then(|d| d.expected_hash.as_deref());
                            let is_move = operation == TransferOperation::Move;
                            let res = self
                                .file_management
                                .replace_file_from_source_with_perms(
                                    &file_id_str,
                                    &dst_file.id.to_string(),
                                    caller_id,
                                    is_move,
                                    expected_hash,
                                )
                                .await;
                            record_file_result(
                                response,
                                file_id_str,
                                res,
                                TransferOutcomeStatus::Replaced,
                            );
                        }
                        Some(TransferDecisionAction::KeepBoth) => {
                            let res = self
                                .transfer_single_file(
                                    caller_id,
                                    &file_id_str,
                                    &dst_folder_str,
                                    operation,
                                    ConflictPolicy::Rename,
                                )
                                .await;
                            record_file_result(
                                response,
                                file_id_str,
                                res,
                                TransferOutcomeStatus::Renamed,
                            );
                        }
                        Some(TransferDecisionAction::Skip) | None => {
                            record_skipped(response, file_id_str, ItemKind::File, &src_file.name);
                        }
                        Some(TransferDecisionAction::Merge) => {
                            record_error(
                                response,
                                file_id_str,
                                ItemKind::File,
                                DomainError::validation_error(
                                    "merge action is only valid for folders",
                                ),
                            );
                        }
                    }
                    continue;
                }

                let ok_status = match operation {
                    TransferOperation::Move => TransferOutcomeStatus::Moved,
                    TransferOperation::Copy => TransferOutcomeStatus::Copied,
                };
                let res = self
                    .transfer_single_file(
                        caller_id,
                        &file_id_str,
                        &dst_folder_str,
                        operation,
                        ConflictPolicy::Fail,
                    )
                    .await;
                record_file_result(response, file_id_str, res, ok_status);
            }

            // 3. On Move: delete emptied source folder (never drop unmoved skipped children).
            if operation == TransferOperation::Move
                && self.folder_storage.is_folder_empty(src_folder_id).await?
                && self
                    .authz
                    .check(
                        Subject::User(caller_id),
                        Permission::Delete,
                        Resource::Folder(src_folder_id),
                    )
                    .await
                    .unwrap_or(false)
            {
                let _ = self
                    .folder_service
                    .delete_folder_with_perms(&src_folder_id.to_string(), caller_id)
                    .await;
            }

            Ok(())
        })
    }

    async fn diff_subtrees_for_merge(
        &self,
        caller_id: Uuid,
        operation: TransferOperation,
        root_conflict_id: &str,
        root_name: &str,
        src_snap: &TransferSubtreeSnapshot,
        dst_snap: &TransferSubtreeSnapshot,
    ) -> (
        Vec<TransferFileConflictDto>,
        Vec<TransferKindConflictDto>,
        u64,
        u64,
    ) {
        let src_dir_rels = build_rel_dir_map(src_snap);
        let dst_dir_rels = build_rel_dir_map(dst_snap);

        let dst_folder_by_rel: HashMap<&str, Uuid> = dst_dir_rels
            .iter()
            .filter_map(|(id, rel)| {
                if rel.is_empty() {
                    None
                } else {
                    Some((rel.as_str(), *id))
                }
            })
            .collect();

        let mut dst_file_by_rel: HashMap<String, &TransferSubtreeFileRow> =
            HashMap::with_capacity(dst_snap.files.len());
        for f in &dst_snap.files {
            if let Some(parent_rel) = dst_dir_rels.get(&f.folder_id) {
                let rel = join_rel(parent_rel, &f.name);
                dst_file_by_rel.insert(rel, f);
            }
        }

        let mut nested_files = Vec::new();
        let mut nested_kinds = Vec::new();
        let mut nested_conflict_count: u64 = 0;
        let mut nested_identical_count: u64 = 0;

        for sf in &src_snap.folders {
            let Some(rel) = src_dir_rels.get(&sf.id) else {
                continue;
            };
            if rel.is_empty() {
                continue;
            }
            if let Some(&blocking_file) = dst_file_by_rel.get(rel) {
                nested_conflict_count += 1;
                nested_kinds.push(TransferKindConflictDto {
                    source_id: sf.id.to_string(),
                    source_name: sf.name.clone(),
                    source_kind: ItemKind::Folder,
                    relative_path: join_rel(root_name, rel),
                    existing_id: blocking_file.id.to_string(),
                    existing_kind: ItemKind::File,
                    parent_folder_conflict_id: Some(root_conflict_id.to_string()),
                });
            }
        }

        for sf in &src_snap.files {
            let Some(parent_rel) = src_dir_rels.get(&sf.folder_id) else {
                continue;
            };
            let rel = join_rel(parent_rel, &sf.name);
            let display_rel = join_rel(root_name, &rel);

            if let Some(&blocking_folder_id) = dst_folder_by_rel.get(rel.as_str()) {
                nested_conflict_count += 1;
                nested_kinds.push(TransferKindConflictDto {
                    source_id: sf.id.to_string(),
                    source_name: sf.name.clone(),
                    source_kind: ItemKind::File,
                    relative_path: display_rel,
                    existing_id: blocking_folder_id.to_string(),
                    existing_kind: ItemKind::Folder,
                    parent_folder_conflict_id: Some(root_conflict_id.to_string()),
                });
                continue;
            }

            if let Some(&df) = dst_file_by_rel.get(&rel) {
                let identical = sf.blob_hash == df.blob_hash;
                if identical {
                    nested_identical_count += 1;
                }
                nested_conflict_count += 1;
                let (can_update_target, can_delete_source) = self
                    .resolve_file_conflict_caps(caller_id, &sf.id.to_string(), df.id, operation)
                    .await;
                nested_files.push(TransferFileConflictDto {
                    source_id: sf.id.to_string(),
                    source_name: sf.name.clone(),
                    relative_path: display_rel,
                    source_size: sf.size.max(0) as u64,
                    source_modified_at: epoch_to_datetime(sf.modified_at.max(0) as u64),
                    source_hash: sf.blob_hash.clone(),
                    target_id: df.id.to_string(),
                    target_folder_id: df.folder_id.to_string(),
                    target_size: df.size.max(0) as u64,
                    target_modified_at: epoch_to_datetime(df.modified_at.max(0) as u64),
                    target_hash: df.blob_hash.clone(),
                    identical,
                    can_update_target,
                    can_delete_source,
                    parent_folder_conflict_id: Some(root_conflict_id.to_string()),
                });
            }
        }

        (
            nested_files,
            nested_kinds,
            nested_conflict_count,
            nested_identical_count,
        )
    }

    async fn resolve_file_conflict_caps(
        &self,
        caller_id: Uuid,
        source_file_id: &str,
        target_file_uuid: Uuid,
        operation: TransferOperation,
    ) -> (bool, bool) {
        let can_update_target = self
            .authz
            .check(
                Subject::User(caller_id),
                Permission::Update,
                Resource::File(target_file_uuid),
            )
            .await
            .unwrap_or(false);

        let can_delete_source = match operation {
            TransferOperation::Copy => true,
            TransferOperation::Move => match Uuid::parse_str(source_file_id) {
                Ok(src_uuid) => self
                    .authz
                    .check(
                        Subject::User(caller_id),
                        Permission::Delete,
                        Resource::File(src_uuid),
                    )
                    .await
                    .unwrap_or(false),
                Err(_) => false,
            },
        };

        (can_update_target, can_delete_source)
    }

    async fn transfer_single_file(
        &self,
        caller_id: Uuid,
        file_id: &str,
        dest_folder_id: &str,
        operation: TransferOperation,
        policy: ConflictPolicy,
    ) -> Result<crate::application::dtos::file_dto::FileDto, DomainError> {
        match operation {
            TransferOperation::Move => {
                self.file_management
                    .move_file_with_perms_on_conflict(
                        file_id,
                        caller_id,
                        Some(dest_folder_id.to_string()),
                        policy,
                    )
                    .await
            }
            TransferOperation::Copy => {
                self.file_management
                    .copy_file_with_perms_on_conflict(
                        file_id,
                        caller_id,
                        Some(dest_folder_id.to_string()),
                        None,
                        policy,
                    )
                    .await
            }
        }
    }

    async fn transfer_single_folder(
        &self,
        caller_id: Uuid,
        folder_id: &str,
        dest_folder_id: &str,
        operation: TransferOperation,
        policy: ConflictPolicy,
    ) -> Result<(String, String), DomainError> {
        match operation {
            TransferOperation::Move => {
                let moved = self
                    .folder_service
                    .move_folder_with_perms(
                        folder_id,
                        MoveFolderDto {
                            parent_id: Some(dest_folder_id.to_string()),
                            on_conflict: policy,
                        },
                        caller_id,
                    )
                    .await?;
                Ok((moved.id, moved.name))
            }
            TransferOperation::Copy => {
                let copied = self
                    .file_management
                    .copy_folder_tree_with_perms_on_conflict(
                        folder_id,
                        caller_id,
                        Some(dest_folder_id.to_string()),
                        None,
                        policy,
                    )
                    .await?;
                let folder = self
                    .folder_storage
                    .get_folder(&copied.new_root_folder_id)
                    .await?;
                Ok((copied.new_root_folder_id, folder.name().to_string()))
            }
        }
    }

    fn folder_authz_resource(&self, folder_id: &str) -> Result<Resource, DomainError> {
        match self.mount_router.folder_authz_resource(folder_id) {
            Some(r) => Ok(r),
            None => Err(DomainError::not_found("Folder", folder_id)),
        }
    }
}

fn validate_and_dedup_ids(
    file_ids: &[String],
    folder_ids: &[String],
) -> Result<(Vec<String>, Vec<String>), DomainError> {
    let mut seen_files = HashSet::with_capacity(file_ids.len());
    let mut files = Vec::with_capacity(file_ids.len());
    for id in file_ids {
        if !id.is_empty() && seen_files.insert(id.clone()) {
            files.push(id.clone());
        }
    }

    let mut seen_folders = HashSet::with_capacity(folder_ids.len());
    let mut folders = Vec::with_capacity(folder_ids.len());
    for id in folder_ids {
        if !id.is_empty() && seen_folders.insert(id.clone()) {
            folders.push(id.clone());
        }
    }

    if files.len() + folders.len() > MAX_TRANSFER_ITEMS {
        return Err(DomainError::validation_error(format!(
            "Transfer request exceeds maximum of {MAX_TRANSFER_ITEMS} items"
        )));
    }

    Ok((files, folders))
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn build_rel_dir_map(snap: &TransferSubtreeSnapshot) -> HashMap<Uuid, String> {
    let mut map = HashMap::with_capacity(snap.folders.len() + 1);
    map.insert(snap.root_id, String::new());
    // Folders are ordered by `nlevel(lpath) ASC`, so every parent is inserted before its children.
    for f in &snap.folders {
        if let Some(pid) = f.parent_id
            && let Some(parent_rel) = map.get(&pid)
        {
            map.insert(f.id, join_rel(parent_rel, &f.name));
        }
    }
    map
}

fn record_outcome(response: &mut TransferExecuteResponse, outcome: TransferItemOutcome) {
    match outcome.status {
        TransferOutcomeStatus::Moved => response.moved_count += 1,
        TransferOutcomeStatus::Copied => response.copied_count += 1,
        TransferOutcomeStatus::Replaced => response.replaced_count += 1,
        TransferOutcomeStatus::Renamed => response.renamed_count += 1,
        TransferOutcomeStatus::Merged => response.merged_count += 1,
        TransferOutcomeStatus::Skipped => response.skipped_count += 1,
        TransferOutcomeStatus::Failed => response.failed_count += 1,
    }
    response.outcomes.push(outcome);
}

fn record_skipped(
    response: &mut TransferExecuteResponse,
    source_id: String,
    kind: ItemKind,
    name: &str,
) {
    record_outcome(
        response,
        TransferItemOutcome {
            source_id,
            kind,
            status: TransferOutcomeStatus::Skipped,
            target_id: None,
            final_name: Some(name.to_string()),
            error: None,
            error_type: None,
        },
    );
}

fn record_error(
    response: &mut TransferExecuteResponse,
    source_id: String,
    kind: ItemKind,
    err: DomainError,
) {
    record_outcome(
        response,
        TransferItemOutcome {
            source_id,
            kind,
            status: TransferOutcomeStatus::Failed,
            target_id: None,
            final_name: None,
            error: Some(err.message.clone()),
            error_type: Some(format!("{:?}", err.kind)),
        },
    );
}

fn record_file_result(
    response: &mut TransferExecuteResponse,
    source_id: String,
    res: Result<crate::application::dtos::file_dto::FileDto, DomainError>,
    ok_status: TransferOutcomeStatus,
) {
    match res {
        Ok(dto) => record_outcome(
            response,
            TransferItemOutcome {
                source_id,
                kind: ItemKind::File,
                status: ok_status,
                target_id: Some(dto.id),
                final_name: Some(dto.name),
                error: None,
                error_type: None,
            },
        ),
        Err(err) => record_error(response, source_id, ItemKind::File, err),
    }
}

#[cfg(test)]
mod transfer_service_unit_tests {
    use super::*;

    #[test]
    fn validate_and_dedup_ids_enforces_cap_and_order() {
        let (f, d) = validate_and_dedup_ids(
            &["a".into(), "".into(), "a".into(), "b".into()],
            &["c".into(), "c".into()],
        )
        .expect("within limit");
        assert_eq!(f, vec!["a", "b"]);
        assert_eq!(d, vec!["c"]);

        let too_many: Vec<String> = (0..=MAX_TRANSFER_ITEMS).map(|i| i.to_string()).collect();
        let err = validate_and_dedup_ids(&too_many, &[]).expect_err("must reject over cap");
        assert_eq!(err.kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn build_rel_dir_map_computes_nested_paths() {
        let root = Uuid::new_v4();
        let sub1 = Uuid::new_v4();
        let sub2 = Uuid::new_v4();
        let snap = TransferSubtreeSnapshot {
            root_id: root,
            root_name: "Docs".into(),
            root_path: "Personal/Docs".into(),
            folders: vec![
                crate::infrastructure::repositories::pg::folder_db_repository::TransferSubtreeFolderRow {
                    id: sub1,
                    parent_id: Some(root),
                    name: "Sub".into(),
                    path: "Personal/Docs/Sub".into(),
                },
                crate::infrastructure::repositories::pg::folder_db_repository::TransferSubtreeFolderRow {
                    id: sub2,
                    parent_id: Some(sub1),
                    name: "Deep".into(),
                    path: "Personal/Docs/Sub/Deep".into(),
                },
            ],
            files: vec![],
        };
        let map = build_rel_dir_map(&snap);
        assert_eq!(map.get(&root).map(String::as_str), Some(""));
        assert_eq!(map.get(&sub1).map(String::as_str), Some("Sub"));
        assert_eq!(map.get(&sub2).map(String::as_str), Some("Sub/Deep"));
    }

    #[test]
    fn record_outcome_tracks_each_counter() {
        let mut resp = TransferExecuteResponse::default();
        record_skipped(&mut resp, "f1".into(), ItemKind::File, "a.txt");
        record_outcome(
            &mut resp,
            TransferItemOutcome {
                source_id: "f2".into(),
                kind: ItemKind::File,
                status: TransferOutcomeStatus::Replaced,
                target_id: Some("t2".into()),
                final_name: Some("b.txt".into()),
                error: None,
                error_type: None,
            },
        );
        record_error(
            &mut resp,
            "f3".into(),
            ItemKind::File,
            DomainError::precondition_failed("File", "stale hash"),
        );
        assert_eq!(resp.skipped_count, 1);
        assert_eq!(resp.replaced_count, 1);
        assert_eq!(resp.failed_count, 1);
        assert_eq!(resp.outcomes.len(), 3);
        assert_eq!(
            resp.outcomes[2].error_type.as_deref(),
            Some("PreconditionFailed")
        );
    }
}
