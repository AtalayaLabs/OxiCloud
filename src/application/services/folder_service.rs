use crate::application::dtos::cursor::PageCursor;
use crate::application::dtos::drive_dto::DriveKindDto;
use crate::application::dtos::folder_dto::{
    AccessSourceDriveDto, AccessSourceDto, AccessSourceKind, AccessSourceSubjectDto,
    AccessSourceSubjectKind, CreateFolderDto, FolderAncestorDto, FolderAncestorsDto, FolderDto,
    FolderResourceCursor, FolderResourceRow, ListResourcesOptions, MoveFolderDto, RenameFolderDto,
};
use crate::application::dtos::grant_dto::RoleDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::external_mount_ports::MountEntry;
use crate::application::ports::file_lifecycle::FileLifecycleHook;
use crate::application::ports::folder_ports::FolderUseCase;
use crate::application::services::external_mount_router::{MountRouter, ResolvedId};
use crate::application::services::file_lifecycle_service::FileLifecycleService;
use crate::application::services::mount_dto::{
    audit_mount_write, mount_entry_folder_dto, mount_folder_dto, mount_parent_id,
};
use crate::application::services::mount_registry::MountConfig;
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::services::authorization::{Permission, Resource, ResourceKind, Role, Subject};
use crate::domain::services::external_mount_id::NodeId;
use crate::domain::services::path_service::{StoragePath, validate_storage_name};
use crate::infrastructure::repositories::pg::folder_db_repository::FolderDbRepository;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use std::sync::Arc;
use uuid::Uuid;

/// Implementation of the use case for folder operations
pub struct FolderService {
    folder_storage: Arc<FolderDbRepository>,
    authz: Arc<PgAclEngine>,
    /// External-mount classifier. Lets folder operations branch a mount-root or
    /// `ext:` id onto the provider instead of the PostgreSQL repositories.
    mount_router: Arc<MountRouter>,
    /// File lifecycle dispatcher. Carried so `delete_folder_with_perms`
    /// can fire `on_file_deleted` for every file the PG cascade is about
    /// to reap. Always present — the dispatcher itself is a no-op when
    /// no hooks are registered, so callers don't need an Option branch.
    file_lifecycle: Arc<FileLifecycleService>,
    /// Drive repository — used by D5's `forbid_cross_drive_move` gate
    /// on `move_folder_with_perms`. Optional so stubs / test factories
    /// can build the service without wiring the full drive repo; in
    /// that case the cross-drive move check is skipped (the policy is
    /// silently off). Production DI wires it via `with_drive_repo`.
    drive_repo: Option<Arc<dyn crate::domain::repositories::drive_repository::DriveRepository>>,
    /// Storage-usage service — used to pre-check the destination
    /// drive's `used_bytes + subtree_bytes ≤ quota_bytes` invariant
    /// on cross-drive MOVE. Silently skipped when unwired (stubs).
    storage_usage:
        Option<Arc<crate::application::services::storage_usage_service::StorageUsageService>>,
}

impl FolderService {
    /// Creates a new folder service
    pub fn new(
        folder_storage: Arc<FolderDbRepository>,
        authz: Arc<PgAclEngine>,
        file_lifecycle: Arc<FileLifecycleService>,
        mount_router: Arc<MountRouter>,
    ) -> Self {
        Self {
            folder_storage,
            authz,
            mount_router,
            file_lifecycle,
            drive_repo: None,
            storage_usage: None,
        }
    }

    /// Borrow the external-mount classifier (handlers branch on this before
    /// treating an id as a native UUID).
    pub fn mount_router(&self) -> &MountRouter {
        &self.mount_router
    }

    /// Authorize a mutation inside a mount. All operations within a mount gate
    /// on the mount-root folder grant (the `cfg.mount_id` resource).
    async fn require_mount_perm(
        &self,
        cfg: &MountConfig,
        perm: Permission,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        self.authz
            .require(
                Subject::User(caller_id),
                perm,
                Resource::Folder(cfg.mount_id),
            )
            .await
    }

    /// If `id` addresses a mount directory (root or `ext:` child), return the
    /// mount config and the node id of that directory. `None` for native ids.
    fn mount_node_for(&self, id: &str) -> Option<(Arc<MountConfig>, NodeId)> {
        match self.mount_router.classify(id) {
            ResolvedId::Regular => None,
            ResolvedId::MountRoot { cfg } => Some((cfg, NodeId::default())),
            ResolvedId::MountChild { cfg, node_id } => Some((cfg, node_id)),
        }
    }

    /// Resolve a move destination within the SAME mount as `cfg`, returning the
    /// destination parent's node id. Errors (`UnsupportedOperation`) if the
    /// destination is absent, native, or in a different mount.
    fn mount_dest_node(
        &self,
        cfg: &MountConfig,
        parent_id: Option<&str>,
    ) -> Result<NodeId, DomainError> {
        let Some(parent_id) = parent_id else {
            return Err(cross_boundary_move_err());
        };
        match self.mount_router.classify(parent_id) {
            ResolvedId::MountRoot { cfg: dest } if dest.mount_id == cfg.mount_id => {
                Ok(NodeId::default())
            }
            ResolvedId::MountChild { cfg: dest, node_id } if dest.mount_id == cfg.mount_id => {
                Ok(node_id)
            }
            _ => Err(cross_boundary_move_err()),
        }
    }

    /// Wires the drive repository, enabling D5
    /// `forbid_cross_drive_move` enforcement on
    /// `move_folder_with_perms`. Without it, the gate is silently
    /// skipped.
    pub fn with_drive_repo(
        mut self,
        drive_repo: Arc<dyn crate::domain::repositories::drive_repository::DriveRepository>,
    ) -> Self {
        self.drive_repo = Some(drive_repo);
        self
    }

    /// Wires the storage-usage service so `move_folder_with_perms`
    /// can pre-check the destination drive's quota on cross-drive
    /// folder moves.
    pub fn with_storage_usage(
        mut self,
        storage_usage: Arc<
            crate::application::services::storage_usage_service::StorageUsageService,
        >,
    ) -> Self {
        self.storage_usage = Some(storage_usage);
        self
    }

    /// Batch counterpart of `get_folder`: resolve many folder ids in ONE
    /// query instead of one per id. Like `get_folder` it performs no
    /// per-folder authorization — both current callers (ACL grant listing,
    /// NextCloud favorites REPORT) resolve ids already vetted by the
    /// authorization engine or the favorites table. Missing or trashed ids
    /// are absent from the result; callers re-associate by `id`.
    pub async fn get_folders_by_ids(&self, ids: &[String]) -> Result<Vec<FolderDto>, DomainError> {
        let folders = self.folder_storage.get_folders_by_ids(ids).await?;
        Ok(folders.into_iter().map(FolderDto::from).collect())
    }

    /// Helper: parse a folder id string into a `Resource::Folder`. Returns
    /// `DomainError::not_found` on parse error (anti-enumeration — the same
    /// error as "folder does not exist").
    fn folder_resource(id: &str) -> Result<Resource, DomainError> {
        Uuid::parse_str(id)
            .map(Resource::Folder)
            .map_err(|_| DomainError::not_found("Folder", id))
    }

    /// Creates a stub implementation for testing and middleware
    pub fn new_stub() -> impl FolderUseCase {
        struct FolderServiceStub;

        impl FolderUseCase for FolderServiceStub {
            async fn require_permission(
                &self,
                _caller_id: Uuid,
                _permission: Permission,
                _folder_id: &str,
            ) -> Result<(), DomainError> {
                Ok(())
            }
            async fn create_folder_with_perms(
                &self,
                _dto: CreateFolderDto,
                _user_id: Uuid,
            ) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn get_folder(&self, _id: &str) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn get_folder_with_perms(
                &self,
                _id: &str,
                _caller_id: Uuid,
            ) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn get_folder_by_path(
                &self,
                _path: &str,
                _drive_id: Uuid,
            ) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn list_folders(
                &self,
                _parent_id: Option<&str>,
            ) -> Result<Vec<FolderDto>, DomainError> {
                Ok(vec![])
            }

            async fn list_folders_with_perms(
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

            async fn list_folders_paginated_with_perms(
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

            async fn rename_folder_with_perms(
                &self,
                _id: &str,
                _dto: RenameFolderDto,
                _caller_id: Uuid,
            ) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn move_folder_with_perms(
                &self,
                _id: &str,
                _dto: MoveFolderDto,
                _caller_id: Uuid,
            ) -> Result<FolderDto, DomainError> {
                Ok(FolderDto::empty())
            }

            async fn delete_folder_with_perms(
                &self,
                _id: &str,
                _caller_id: Uuid,
            ) -> Result<(), DomainError> {
                Ok(())
            }
        }

        FolderServiceStub
    }
}

impl FolderUseCase for FolderService {
    /// Verifies the caller has the given permition on a resource
    /// `folder_id`. `None` is the caller's root namespace and always allowed.
    ///
    /// Returns `Ok(())` when permitted, `DomainError::not_found(...)` when not
    /// (anti-enumeration — same error as "folder doesn't exist").
    ///
    /// Used by handlers that need a fail-fast pre-check BEFORE spooling
    /// large request bodies (file upload, chunked upload). The authoritative
    /// check happens again inside the upload/management services before any
    /// DB write — this is a UX/resource optimization, not a security boundary.
    async fn require_permission(
        &self,
        caller_id: Uuid,
        permission: Permission,
        folder_id: &str,
    ) -> Result<(), DomainError> {
        let resource = Self::folder_resource(folder_id)?;
        self.authz
            .require(Subject::User(caller_id), permission, resource)
            .await
    }

    /// Creates a new folder
    async fn create_folder_with_perms(
        &self,
        dto: CreateFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        if let Err(reason) = validate_storage_name(&dto.name) {
            return Err(DomainError::validation_error(format!(
                "Invalid folder name '{}': {reason}",
                dto.name
            )));
        }

        let Some(parent_id) = dto.parent_id.as_deref() else {
            return Err(DomainError::validation_error(
                "Root folder creation is reserved for registration",
            ));
        };

        // External mount: create the directory on the provider, not in PG.
        match self.mount_router.classify(parent_id) {
            ResolvedId::Regular => {}
            ResolvedId::MountRoot { cfg } => {
                self.require_mount_perm(&cfg, Permission::Create, caller_id)
                    .await?;
                let stat = cfg
                    .provider
                    .create_dir(&NodeId::default(), &dto.name)
                    .await?;
                audit_mount_write("mkdir", &cfg, caller_id, stat.node_id.as_str());
                return Ok(mount_folder_dto(&cfg, parent_id, &stat));
            }
            ResolvedId::MountChild { cfg, node_id } => {
                self.require_mount_perm(&cfg, Permission::Create, caller_id)
                    .await?;
                let stat = cfg.provider.create_dir(&node_id, &dto.name).await?;
                audit_mount_write("mkdir", &cfg, caller_id, stat.node_id.as_str());
                return Ok(mount_folder_dto(&cfg, parent_id, &stat));
            }
        }

        let parent_resource = Self::folder_resource(parent_id)?;
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Create,
                parent_resource,
            )
            .await?;

        let folder = self
            .folder_storage
            .create_folder(dto.name, dto.parent_id, caller_id)
            .await?;
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

    /// Gets a folder by its ID, enforcing that `caller_id` has `Read` access
    /// (via ownership or a grant — including cascading from ancestor folders).
    async fn get_folder_with_perms(
        &self,
        id: &str,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Self::folder_resource(id)?,
            )
            .await?;
        self.get_folder(id).await
    }

    /// Gets a folder by its path, scoped to a drive.
    async fn get_folder_by_path(
        &self,
        path: &str,
        drive_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        // External mount: a path that descends past a mount root (non-empty
        // remainder) resolves on the provider. The mount root itself is a real
        // folder row, so the empty-remainder case falls through to the DB.
        if let Some((cfg, remainder)) = self.mount_router.find_path(drive_id, path)
            && !remainder.is_empty()
        {
            let node = cfg.provider.resolve_path(&remainder);
            let stat = cfg.provider.stat(&node).await?;
            if !stat.is_dir {
                return Err(DomainError::not_found("Folder", path));
            }
            let parent = mount_parent_id(&cfg, stat.node_id.as_str());
            return Ok(mount_folder_dto(&cfg, &parent, &stat));
        }

        let storage_path = StoragePath::from_string(path);

        let folder = self
            .folder_storage
            .get_folder_by_path(&storage_path, drive_id)
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
                tracing::warn!("errror while fetching folders {}", e);
                DomainError::internal_error(
                    "FolderStorage",
                    format!("Failed to list folders in parent: {:?}: {}", parent_id, e),
                )
            })?;

        // Convert to DTOs
        Ok(folders.into_iter().map(FolderDto::from).collect())
    }

    /// Lists folders scoped to a specific owner.
    ///
    /// **Note (post PR 3):** the self-heal block that auto-created a
    /// home folder when listing returned empty has been removed.
    /// `PersonalDriveLifecycleHook` (registered on `UserLifecycleService`)
    /// now provisions the folder on `on_user_created` / `on_user_login`,
    /// idempotently, so the listing path no longer needs to self-heal.
    async fn list_folders_with_perms(
        &self,
        parent_id: Option<&str>,
        caller_id: Uuid,
    ) -> Result<Vec<FolderDto>, DomainError> {
        if let Some(parent_id_unwrapped) = parent_id {
            // check authorisation
            self.authz
                .require(
                    Subject::User(caller_id),
                    Permission::Read,
                    Self::folder_resource(parent_id_unwrapped)?,
                )
                .await?;
            return self.list_folders(parent_id).await;
        }
        // No parent → list the caller's readable root folders. The
        // predicate scopes by drive-membership grants (post-PR-B),
        // closing the pre-D7 gap where the legacy `user_id` filter
        // surfaced admin-created folders that admin had no role on.
        let folders = self
            .folder_storage
            .list_root_folders_for_caller(caller_id)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!("Failed to list root folders for caller '{caller_id}': {e}"),
                )
            })?;
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

    /// Keyset-paged sub-folder listing (name order), caller-scoped.
    ///
    /// AuthZ mirrors `list_folders_paginated_with_perms`: one
    /// `authz.require(Read)` on the parent per batch; root scope goes
    /// through the caller's drive-membership listing.
    async fn list_folders_batch_with_perms(
        &self,
        parent_id: Option<&str>,
        caller_id: Uuid,
        after_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FolderDto>, DomainError> {
        match parent_id {
            Some(pid) => {
                self.authz
                    .require(
                        Subject::User(caller_id),
                        Permission::Read,
                        Self::folder_resource(pid)?,
                    )
                    .await?;
                let folders = self
                    .folder_storage
                    .list_folders_batch(parent_id, after_name, limit)
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "FolderStorage",
                            format!("Failed to batch-list folders in parent {pid}: {e}"),
                        )
                    })?;
                Ok(folders.into_iter().map(FolderDto::from).collect())
            }
            None => {
                // Root scope: one row per readable drive — a handful.
                let mut all = self
                    .folder_storage
                    .list_root_folders_for_caller(caller_id)
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "FolderStorage",
                            format!("Failed to batch-list root folders for '{caller_id}': {e}"),
                        )
                    })?;
                all.sort_by(|a, b| a.name().cmp(b.name()));
                Ok(all
                    .into_iter()
                    .filter(|f| after_name.is_none_or(|a| f.name() > a))
                    .take(limit)
                    .map(FolderDto::from)
                    .collect())
            }
        }
    }

    /// Lists folders with pagination, scoped to a specific owner.
    async fn list_folders_paginated_with_perms(
        &self,
        parent_id: Option<&str>,
        owner_id: Uuid,
        pagination: &crate::application::dtos::pagination::PaginationRequestDto,
    ) -> Result<crate::application::dtos::pagination::PaginatedResponseDto<FolderDto>, DomainError>
    {
        let pagination = pagination.validate_and_adjust();

        // External mount: list subdirectories from the provider (used by the
        // WebDAV/NextCloud PROPFIND Depth:1 folder loop).
        if let Some(pid) = parent_id
            && let Some((cfg, node)) = self.mount_node_for(pid)
        {
            self.require_mount_perm(&cfg, Permission::Read, owner_id)
                .await?;
            let entries = cfg.provider.list_dir(&node).await?;
            let mut dirs: Vec<FolderDto> = entries
                .iter()
                .filter(|e| e.is_dir)
                .map(|e| mount_entry_folder_dto(&cfg, pid, e))
                .collect();
            let total = dirs.len();
            let (offset, limit) = (pagination.offset(), pagination.limit());
            let page: Vec<FolderDto> = dirs.drain(..).skip(offset).take(limit).collect();
            return Ok(
                crate::application::dtos::pagination::PaginatedResponseDto::new(
                    page,
                    pagination.page,
                    pagination.page_size,
                    total,
                ),
            );
        }

        if let Some(parent_id_unwrapped) = parent_id {
            self.authz
                .require(
                    Subject::User(owner_id),
                    Permission::Read,
                    Self::folder_resource(parent_id_unwrapped)?,
                )
                .await?;
            return self.list_folders_paginated(parent_id, &pagination).await;
        } else {
            let (folders, total_items) = self
                .folder_storage
                .list_root_folders_for_caller_paginated(
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
                            "Failed to list root folders for caller '{}' with pagination: {}",
                            owner_id, e
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
    }

    /// Renames a folder after verifying the caller has `Update` permission.
    async fn rename_folder_with_perms(
        &self,
        id: &str,
        dto: RenameFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        if let Err(reason) = validate_storage_name(&dto.name) {
            return Err(DomainError::validation_error(format!(
                "Invalid folder name '{}': {reason}",
                dto.name
            )));
        }

        // External mount: rename on the provider. The mount root cannot be
        // renamed through here (it's a real folder row managed elsewhere).
        match self.mount_router.classify(id) {
            ResolvedId::Regular => {}
            ResolvedId::MountRoot { .. } => {
                return Err(DomainError::operation_not_supported(
                    "Folder",
                    "a mount root cannot be renamed through this endpoint",
                ));
            }
            ResolvedId::MountChild { cfg, node_id } => {
                self.require_mount_perm(&cfg, Permission::Update, caller_id)
                    .await?;
                let stat = cfg.provider.rename(&node_id, &dto.name).await?;
                let parent = mount_parent_id(&cfg, stat.node_id.as_str());
                audit_mount_write("rename", &cfg, caller_id, stat.node_id.as_str());
                return Ok(mount_folder_dto(&cfg, &parent, &stat));
            }
        }

        // Drive roots double as the drive's display name (per drive.md §3,
        // `drives.name` is sourced from `storage.folders.name` of the row
        // pointed at by `root_folder_id`). Per drive.md §6 the rename is
        // Owner-only — but with `Permission::Update` that's leaky because
        // every Editor of the drive has Update on every folder in the
        // drive, including the root. So we promote the requirement to
        // `Manage` for root folders. A root is identified by
        // `parent_id IS NULL`; that's the same property the drive seeder
        // and the drive-of-resource resolver rely on, so no schema-level
        // assumption shifts here.
        let folder = self.folder_storage.get_folder(id).await.map_err(|e| {
            DomainError::internal_error(
                "FolderStorage",
                format!("Failed to look up folder before rename: {id}: {e}"),
            )
        })?;
        let required_perm = if folder.parent_id().is_none() {
            Permission::Manage
        } else {
            Permission::Update
        };

        self.authz
            .require(
                Subject::User(caller_id),
                required_perm,
                Self::folder_resource(id)?,
            )
            .await?;

        let renamed = self
            .folder_storage
            .rename_folder(id, dto.name, caller_id)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!("Failed to rename folder with ID: {}: {}", id, e),
                )
            })?;

        // Root folders double as the drive's display name (see the
        // `required_perm` branch above and `drive_pg_repository.rs`
        // `readable_cache` + `default_drive_cache` docs).
        // `drives.name` is sourced from `folders.name` of the root
        // folder, so a rename affects BOTH caches — every user's
        // readable-drive list AND the per-user default-drive lookup.
        // Both are 30 s TTL; without the invalidation, `GET /api/drives`
        // returns the stale name for up to that window after a root
        // rename. Surfaced by `tests/api/drives_membership.hurl`
        // Step 23. Regression from commit `12dc648c` ("perf: round 4 —
        // drive-selector cache") which added the caches without
        // wiring the root-rename invalidation.
        if folder.parent_id().is_none()
            && let Some(drive_repo) = &self.drive_repo
        {
            drive_repo.invalidate_readable_all();
            drive_repo.invalidate_default_drive_all();
        }

        Ok(FolderDto::from(renamed))
    }

    /// Moves a folder to a new parent. Requires `Update` on the source and
    /// `Create` on the destination parent (if any).
    async fn move_folder_with_perms(
        &self,
        id: &str,
        dto: MoveFolderDto,
        caller_id: Uuid,
    ) -> Result<FolderDto, DomainError> {
        // External mount: moves must stay within a single mount. The provider
        // relocates; cross-backend moves (mount ↔ native, or between mounts) are
        // forbidden in v1.
        match self.mount_router.classify(id) {
            ResolvedId::Regular => {
                // Native source: forbid moving INTO a mount.
                if let Some(parent_id) = &dto.parent_id
                    && self.mount_router.is_mount_id(parent_id)
                {
                    return Err(cross_boundary_move_err());
                }
            }
            ResolvedId::MountRoot { .. } => {
                return Err(DomainError::operation_not_supported(
                    "Folder",
                    "a mount root cannot be moved",
                ));
            }
            ResolvedId::MountChild { cfg, node_id } => {
                let dest = self.mount_dest_node(&cfg, dto.parent_id.as_deref())?;
                self.require_mount_perm(&cfg, Permission::Update, caller_id)
                    .await?;
                self.require_mount_perm(&cfg, Permission::Create, caller_id)
                    .await?;
                let stat = cfg.provider.move_within(&node_id, &dest).await?;
                audit_mount_write("move", &cfg, caller_id, stat.node_id.as_str());
                let parent = mount_parent_id(&cfg, stat.node_id.as_str());
                return Ok(mount_folder_dto(&cfg, &parent, &stat));
            }
        }

        let source_resource = Self::folder_resource(id)?;
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Update,
                source_resource,
            )
            .await?;

        if let Some(parent_id) = &dto.parent_id {
            // Cannot move a folder into itself (cycle guard).
            if parent_id == id {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "Folder",
                    "Cannot move a folder into itself",
                ));
            }
            let parent_resource = Self::folder_resource(parent_id)?;
            self.authz
                .require(
                    Subject::User(caller_id),
                    Permission::Create,
                    parent_resource,
                )
                .await?;
            // TODO: full descendant-cycle check (moving a folder into one of its own descendants)
        }

        // D5 `forbid_cross_drive_move` + D6 `resource.moved_between_drives`
        // audit share the same src/dst lookup. Gate before the move,
        // audit after a successful move when the two drives differ.
        // Skipped for parent_id=None (root namespace, same-drive
        // semantics) and when drive_repo isn't wired (stubs/tests) —
        // same shape as `move_file_with_perms`.
        let mut cross_drive: Option<(Uuid, Uuid)> = None;
        if let Some(drive_repo) = &self.drive_repo
            && let Some(parent_id) = &dto.parent_id
        {
            let src_folder_uuid =
                Uuid::parse_str(id).map_err(|_| DomainError::not_found("Folder", id))?;
            let dst_folder_uuid = Uuid::parse_str(parent_id)
                .map_err(|_| DomainError::not_found("Folder", parent_id.as_str()))?;
            // Independent point reads — overlapped so the pre-move drive
            // resolution pays one round-trip, not two (ROUND10, same shape
            // as `move_file_with_perms`).
            let (src_res, dst_res) = tokio::join!(
                drive_repo.get_drive_id_and_policies_for_folder(src_folder_uuid),
                drive_repo.drive_id_for_folder(dst_folder_uuid),
            );
            let (src_drive_id, src_policies) = src_res.map_err(|e| {
                DomainError::internal_error("Drive", format!("source drive lookup: {e:?}"))
            })?;
            let dst_drive_id = dst_res.map_err(|e| {
                DomainError::internal_error("Drive", format!("destination drive lookup: {e:?}"))
            })?;
            if src_drive_id != dst_drive_id {
                src_policies.refuse_cross_drive_move(
                    crate::domain::entities::drive::CrossDriveMoveGateContext {
                        caller_id,
                        resource_type: "folder",
                        resource_id: src_folder_uuid,
                        src_drive_id,
                        dst_drive_id,
                    },
                )?;
                // Destination drive quota: sum the moved subtree's
                // non-trashed files and refuse if the destination
                // couldn't hold them. Same 507 shape as the file
                // path + upload path — DomainError::QuotaExceeded
                // maps at the AppError boundary.
                if let Some(storage_usage) = &self.storage_usage {
                    let subtree_bytes = storage_usage.folder_subtree_bytes(src_folder_uuid).await?;
                    if let Ok(subtree_u64) = u64::try_from(subtree_bytes) {
                        storage_usage
                            .check_drive_quota(dst_drive_id, subtree_u64)
                            .await?;
                    }
                }
                cross_drive = Some((src_drive_id, dst_drive_id));
            }
        }

        let parent_ref = dto.parent_id.as_deref();
        let folder = self
            .folder_storage
            .move_folder(id, parent_ref, caller_id)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FolderStorage",
                    format!("Failed to move folder with ID: {}: {}", id, e),
                )
            })?;

        // Cross-drive move flushes the authz engine's `owner_cache`
        // — every descendant's cached `Resource → drive_id` mapping
        // just got stale via the cascade trigger, and we don't (yet)
        // walk the subtree to invalidate individually. Small perf
        // cost (single JOIN per resource touched over the next
        // minute) versus a stale-authz bug where destination-drive
        // Owner cascades don't apply to moved content.
        if cross_drive.is_some() {
            self.authz.invalidate_owner_cache_all().await;
        }

        // D6 audit: only emit when the move crossed a drive boundary.
        // The cascade trigger has already propagated drive_id to the
        // subtree at this point (see migration
        // `20260807000000_cascade_drive_id_on_folder_move.sql`).
        if let Some((src_drive_id, dst_drive_id)) = cross_drive {
            tracing::info!(
                target: "audit",
                event = "resource.moved_between_drives",
                resource_type = "folder",
                resource_id = %folder.id(),
                src_drive_id = %src_drive_id,
                dst_drive_id = %dst_drive_id,
                by = %caller_id,
                "📦 folder moved between drives",
            );
        }

        Ok(FolderDto::from(folder))
    }

    /// Deletes a folder after verifying the caller has `Delete` permission.
    /// The DB trigger `trg_cleanup_grants_folder` cleans up `access_grants`
    /// rows targeting the deleted folder automatically.
    ///
    /// Enumerates the subtree's file ids BEFORE the bulk DELETE so
    /// `on_file_deleted` fires per file the PG cascade is about to reap —
    /// without this, file-id-keyed lifecycle data (e.g. `ext-{file_id}.jpg`
    /// video thumbnails, moka cache entries) leaks past the cascade.
    /// Same shape `clear_trash_in` uses (`trash_service.rs:804-846`).
    async fn delete_folder_with_perms(&self, id: &str, caller_id: Uuid) -> Result<(), DomainError> {
        // External mount: delete on the provider (permanent — mounts have no
        // trash). The mount root is a real folder row and is not deletable here.
        match self.mount_router.classify(id) {
            ResolvedId::Regular => {}
            ResolvedId::MountRoot { .. } => {
                return Err(DomainError::operation_not_supported(
                    "Folder",
                    "a mount root cannot be deleted through this endpoint",
                ));
            }
            ResolvedId::MountChild { cfg, node_id } => {
                self.require_mount_perm(&cfg, Permission::Delete, caller_id)
                    .await?;
                cfg.provider.delete(&node_id).await?;
                audit_mount_write("delete", &cfg, caller_id, node_id.as_str());
                return Ok(());
            }
        }

        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Delete,
                Self::folder_resource(id)?,
            )
            .await?;

        // Snapshot the file ids BEFORE the bulk DELETE — the rows are gone
        // afterward. Failure to enumerate is non-fatal (logged in the repo
        // method); the delete proceeds and only file-id-keyed cleanup is
        // skipped (blob-keyed thumbnails still get reaped by GC).
        let cascaded_file_ids = self
            .folder_storage
            .list_file_ids_in_subtree(id)
            .await
            .unwrap_or_default();

        self.folder_storage.delete_folder(id).await.map_err(|e| {
            DomainError::internal_error(
                "FolderStorage",
                format!("Failed to delete folder with ID: {}: {}", id, e),
            )
        })?;

        for file_id in &cascaded_file_ids {
            self.file_lifecycle.on_file_deleted(file_id);
        }

        Ok(())
    }
}

/// The error returned when a move would cross a storage backend boundary
/// (mount ↔ native, or between two different mounts). Forbidden in v1.
fn cross_boundary_move_err() -> DomainError {
    DomainError::operation_not_supported(
        "Folder",
        "moving between external mounts and regular storage is not supported",
    )
}

// ── FolderService — cursor-paginated resource listing ────────────────────────

impl FolderService {
    /// Ancestor chain for the shared breadcrumb component. Returns the
    /// list of folders from the caller-visible root (drive root or
    /// share boundary) down to the leaf, plus an `access_source`
    /// describing HOW the caller reached that topmost ancestor.
    ///
    /// AuthZ: requires `Read` on the leaf. Anti-enum via `NotFound` on
    /// denial (the `require` helper turns denials into 404 to match
    /// listing endpoints — same pattern used by `get_folder_with_perms`).
    ///
    /// Boundary detection: the recursive SQL walks all the way to the
    /// drive root and reports two Read predicates per ancestor
    /// (`has_folder_grant`, `has_drive_grant`). We drop ancestors that
    /// have NEITHER — that's a folder the caller can't Read, which by
    /// definition means everything above it is also invisible to them.
    /// The last surviving ancestor is the "root of this caller's view."
    ///
    /// Access-source kind: `Drive` when the topmost accessible ancestor's
    /// Read came (even in part) from drive-membership; `DirectShare`
    /// otherwise. `Token` is reserved for future public-link callers.
    /// Subject enrichment (grantor / group name) is deferred — MVP
    /// returns `subject: None` and the FE renders a generic tooltip.
    pub async fn get_ancestors_with_perms(
        &self,
        leaf_id: &str,
        caller_id: Uuid,
    ) -> Result<FolderAncestorsDto, DomainError> {
        // Gate: caller must have Read on the leaf. Denial → 404 (anti-enum).
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Self::folder_resource(leaf_id)?,
            )
            .await?;

        let leaf_uuid =
            Uuid::parse_str(leaf_id).map_err(|_| DomainError::not_found("Folder", leaf_id))?;

        let mut rows = self
            .folder_storage
            .fetch_ancestor_walk(caller_id, leaf_uuid)
            .await?;
        if rows.is_empty() {
            return Err(DomainError::not_found("Folder", leaf_id));
        }
        // Repo returns root-first (ORDER BY depth DESC). Walk from index 0
        // (topmost) and drop entries with NO Read grant — that's the
        // share/drive boundary, everything above is invisible.
        let boundary = rows
            .iter()
            .position(|r| r.has_folder_grant || r.has_drive_grant)
            .unwrap_or(rows.len());
        rows.drain(..boundary);
        if rows.is_empty() {
            // Shouldn't happen: `authz.require(Read, leaf)` above passed,
            // so at least the leaf must have some Read source. Defensive
            // 404 rather than emit an empty chain.
            return Err(DomainError::not_found("Folder", leaf_id));
        }

        // The topmost surviving row is the root of the caller's view.
        // Its grant profile drives `AccessSource`.
        let top = &rows[0];

        // Subject enrichment: identify the specific grant that gave the
        // caller access to `top`, then resolve its subject's display
        // name in the same query. Drives the tooltip on the breadcrumb
        // root chip ("Shared with you by Alice" / "Shared with your
        // team via Design"). No `expires_at` filter — the ancestor
        // walk's guard already proved the caller is authorized to see
        // this ancestor, so the follow-up name lookup is display-only
        // (see `feedback_trust_grant_janitor_no_expires_at_read`).
        let (grant_resource_type, grant_resource_id) = if top.has_drive_grant {
            ("drive", top.drive_id)
        } else {
            ("folder", top.id)
        };
        let grant_by = self
            .folder_storage
            .fetch_grant_by(caller_id, grant_resource_type, grant_resource_id)
            .await?;
        let subject = grant_by
            .as_ref()
            .map(
                |(subject_type_str, subject_id, name, _role)| AccessSourceSubjectDto {
                    kind: match subject_type_str.as_str() {
                        "group" => AccessSourceSubjectKind::Group,
                        _ => AccessSourceSubjectKind::User,
                    },
                    id: *subject_id,
                    name: name.clone(),
                },
            );
        // Caller's role via the boundary grant. `Role::parse` returns
        // None only if the SQL stored a role we don't understand — the
        // ENUM constraint makes that a schema drift, not a runtime case
        // to chase. Silent None keeps the endpoint working with an older
        // deployment if a future role is added ahead of the code.
        let caller_role = grant_by
            .as_ref()
            .and_then(|(_, _, _, role_str)| Role::parse(role_str))
            .map(RoleDto::from);

        let access_source = if top.has_drive_grant {
            // Drive-membership Read — even if a direct folder grant also
            // exists, the drive channel is the more useful "how did I
            // get here" signal (it names the drive the caller sees in
            // their picker). Fetch the drive header for id/name/kind.
            // `.map` (not `match`) — the drive-vanished-mid-query fallback
            // is a straight `None`, no side effects; clippy's manual_map
            // lint prefers this shape.
            let drive = self
                .folder_storage
                .fetch_drive_header(top.drive_id)
                .await?
                .map(|(id, name, kind_str)| AccessSourceDriveDto {
                    id,
                    name,
                    kind: match kind_str.as_str() {
                        "personal" => DriveKindDto::Personal,
                        _ => DriveKindDto::Shared,
                    },
                });
            AccessSourceDto {
                kind: AccessSourceKind::Drive,
                drive,
                subject,
                caller_role,
            }
        } else {
            // Direct folder-level grant (share). Subject carries who
            // shared it (user or group), enabling "shared with you by X"
            // in the FE tooltip.
            AccessSourceDto {
                kind: AccessSourceKind::DirectShare,
                drive: None,
                subject,
                caller_role,
            }
        };

        let ancestors = rows
            .into_iter()
            .map(|r| FolderAncestorDto {
                id: r.id,
                name: r.name,
                parent_id: r.parent_id,
                drive_id: r.drive_id,
            })
            .collect();

        Ok(FolderAncestorsDto {
            ancestors,
            access_source,
        })
    }

    /// Cursor-paginated listing of sub-folders **and** files inside `parent_id`.
    ///
    /// Enforces `Permission::Read` on the parent folder before querying.
    /// `order_by` controls both the SQL `ORDER BY` and the cursor encoding.
    /// `kinds` filters the result to only the specified resource types.
    pub async fn list_resources_paged_with_perms(
        &self,
        parent_id: &str,
        caller_id: Uuid,
        opts: ListResourcesOptions<'_>,
    ) -> Result<(Vec<FolderResourceRow>, Option<String>), DomainError> {
        // 1. AuthZ — same check as list_folders_with_perms
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Self::folder_resource(parent_id)?,
            )
            .await?;

        let pid =
            Uuid::parse_str(parent_id).map_err(|_| DomainError::not_found("Folder", parent_id))?;

        let ListResourcesOptions {
            limit,
            cursor,
            order_by,
            kinds,
            reverse,
        } = opts;

        // 2. Fetch limit+1 rows so we can detect has_next
        let mut rows = self
            .folder_storage
            .list_resources_paged(
                pid,
                caller_id,
                limit + 1,
                cursor.as_ref(),
                order_by,
                kinds,
                reverse,
            )
            .await?;

        // 3. Detect has_next, build encoded next cursor
        let next_cursor = if rows.len() > limit {
            let last = &rows[limit - 1];
            let c = build_folder_resource_cursor(last, order_by, reverse);
            rows.truncate(limit);
            Some(c.encode())
        } else {
            None
        };

        Ok((rows, next_cursor))
    }

    /// List one directory inside an external mount (the mount root when
    /// `node_id` is empty, or a nested virtual folder otherwise).
    ///
    /// Authorization collapses onto the mount-root folder: a caller who may
    /// `Read` the mount root may browse everything inside it. The provider
    /// reads the live backend; entries are sorted in memory and paginated with
    /// a name-keyset cursor (directories are bounded, see provider cap).
    ///
    /// Returns the page of raw [`MountEntry`]s plus an encoded next cursor; the
    /// handler maps each entry to a `FolderResourceItemDto` with a synthetic
    /// `ext:` id.
    pub async fn list_mount_dir_with_perms(
        &self,
        cfg: &MountConfig,
        node_id: &NodeId,
        caller_id: Uuid,
        opts: ListResourcesOptions<'_>,
    ) -> Result<(Vec<MountEntry>, Option<String>), DomainError> {
        // AuthZ — everything in the mount is gated by the mount-root folder.
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Resource::Folder(cfg.mount_id),
            )
            .await?;

        let entries = cfg.provider.list_dir(node_id).await?;
        let cursor_name = opts.cursor.as_ref().and_then(|c| c.sort_str.as_deref());
        Ok(paginate_mount_entries(
            entries,
            opts.kinds,
            opts.order_by,
            opts.reverse,
            opts.limit,
            cursor_name,
        ))
    }
}

/// Filter, sort, and page a directory's worth of mount entries, returning the
/// page plus an encoded next cursor. Pure (no I/O / authz) so it can be tested
/// exhaustively.
///
/// The cursor is a **name keyset**: names are unique within a directory, so the
/// last emitted name is a stable resume key under any sort dimension. Resume is
/// best-effort — if the cursor's entry was deleted out-of-band the page restarts
/// from the top (documented; avoids an infinite loop).
fn paginate_mount_entries(
    mut entries: Vec<MountEntry>,
    kinds: Option<&[ResourceKind]>,
    order_by: &str,
    reverse: bool,
    limit: usize,
    cursor_name: Option<&str>,
) -> (Vec<MountEntry>, Option<String>) {
    if let Some(kinds) = kinds {
        let want_files = kinds.contains(&ResourceKind::File);
        let want_folders = kinds.contains(&ResourceKind::Folder);
        entries.retain(|e| if e.is_dir { want_folders } else { want_files });
    }

    sort_mount_entries(&mut entries, order_by, reverse);

    let start = match cursor_name {
        Some(name) => entries
            .iter()
            .position(|e| name.eq_ignore_ascii_case(&e.name))
            .map(|i| i + 1)
            .unwrap_or(0),
        None => 0,
    };

    let has_more = entries.len() > start + limit;
    let page: Vec<MountEntry> = entries.into_iter().skip(start).take(limit).collect();

    let next_cursor = if has_more {
        page.last().map(|last| {
            FolderResourceCursor {
                order_by: order_by.to_owned(),
                resource_id: Uuid::nil(),
                sort_str: Some(last.name.clone()),
                sort_int: None,
                sort_ts: None,
                reverse,
            }
            .encode()
        })
    } else {
        None
    };

    (page, next_cursor)
}

/// Sort mount entries in place. Folders sort before files for the `name`/`type`
/// dimensions; otherwise by the requested key with name as the tie-breaker.
/// `reverse` flips the final order.
fn sort_mount_entries(entries: &mut [MountEntry], order_by: &str, reverse: bool) {
    use std::cmp::Ordering;
    let name_key = |e: &MountEntry| e.name.to_lowercase();
    entries.sort_by(|a, b| {
        let primary = match order_by {
            "modified_at" => a.modified_at.cmp(&b.modified_at),
            "created_at" => a.created_at.cmp(&b.created_at),
            "size" => a.size.cmp(&b.size),
            // "name" / "type" / anything else: folders first, then by name.
            _ => b.is_dir.cmp(&a.is_dir),
        };
        let ord = primary.then_with(|| name_key(a).cmp(&name_key(b)));
        if ord == Ordering::Equal {
            Ordering::Equal
        } else if reverse {
            ord.reverse()
        } else {
            ord
        }
    });
}

/// Build the next-page cursor from the last row of the current page.
/// `reverse` is stored in the cursor so subsequent pages use the same order.
fn build_folder_resource_cursor(
    row: &FolderResourceRow,
    order_by: &str,
    reverse: bool,
) -> FolderResourceCursor {
    match order_by {
        "type" => FolderResourceCursor {
            order_by: "type".to_owned(),
            resource_id: row.id,
            sort_str: Some(row.sort_str.clone()),
            sort_int: Some(row.type_order),
            sort_ts: None,
            reverse,
        },
        "modified_at" => FolderResourceCursor {
            order_by: "modified_at".to_owned(),
            resource_id: row.id,
            sort_str: None,
            sort_int: None,
            sort_ts: Some(row.modified_at),
            reverse,
        },
        "created_at" => FolderResourceCursor {
            order_by: "created_at".to_owned(),
            resource_id: row.id,
            sort_str: None,
            sort_int: None,
            sort_ts: Some(row.created_at),
            reverse,
        },
        "size" => FolderResourceCursor {
            order_by: "size".to_owned(),
            resource_id: row.id,
            sort_str: None,
            sort_int: Some(row.size),
            sort_ts: None,
            reverse,
        },
        _ => FolderResourceCursor {
            // "name" (default): sort_int = folder_first (0 or 1)
            order_by: "name".to_owned(),
            resource_id: row.id,
            sort_str: Some(row.sort_str.clone()),
            sort_int: Some(i64::from(row.folder_first)),
            sort_ts: None,
            reverse,
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PersonalDriveLifecycleHook
//
// Owns home-folder provisioning policy. Replaces:
//   - the 4 eager `create_personal_folder` calls in AuthApplicationService
//     (register / setup_create_admin / admin_create_user / OIDC JIT)
//   - the self-heal at `list_folders_with_perms` when no root folders exist
//
// Lives in this file (not under a centralised `lifecycle/` directory)
// because the folder service owns home-folder policy — see the
// "owner-located convention" note in
// `docs/architecture/user-lifecycle.md`.
// ─────────────────────────────────────────────────────────────────────────────

use async_trait::async_trait;

use crate::application::ports::user_lifecycle::{DeletionMode, LogoutReason, UserLifecycleHook};
use crate::domain::entities::user::User;

/// Lifecycle hook: provisions a user's default Personal drive at first
/// login (replaces the legacy `My Folder - <username>` wrapper as of D0).
///
/// Two writes happen on first provisioning:
///   1. A row in `storage.drives` with `kind='personal'`,
///      `default_for_user=<uid>`, and the user's quota carried over from
///      `auth.users.storage_quota_bytes`.
///   2. An Owner role grant in `storage.role_grants` so the user can
///      read/write/manage their own drive (the engine's owner short-
///      circuit applies to folders/files but not drives — see
///      `pg_acl_engine::check_inner` D0-6 rewrite).
///
/// Both writes are idempotent: `find_default_for_user` short-circuits
/// when the drive already exists; `set_role` is an UPSERT that no-ops
/// when the Owner row is already present.
pub struct PersonalDriveLifecycleHook {
    drive_repo: Arc<dyn crate::domain::repositories::drive_repository::DriveRepository>,
    // The `AuthorizationEngine` trait isn't `dyn`-compatible (native
    // async-fn-in-trait methods are not object-safe), so we hold the
    // concrete engine. This matches the convention already used by
    // `AppState.authorization`. Only the idempotent-rerun path uses it
    // now; the create path goes through the repo's atomic CTE which
    // writes the role_grant inline.
    authorization: Arc<crate::infrastructure::services::pg_acl_engine::PgAclEngine>,
}

impl PersonalDriveLifecycleHook {
    pub fn new(
        drive_repo: Arc<dyn crate::domain::repositories::drive_repository::DriveRepository>,
        authorization: Arc<crate::infrastructure::services::pg_acl_engine::PgAclEngine>,
    ) -> Self {
        Self {
            drive_repo,
            authorization,
        }
    }

    /// Idempotent provisioning shared by `on_user_created` and
    /// `on_user_login`. External users are skipped per tip #2 in the
    /// trait docstring — they have no resources of their own, only
    /// grants on other users' resources.
    async fn provision_if_needed(&self, user: &User) -> Result<(), DomainError> {
        use crate::domain::repositories::drive_repository::DriveRepositoryError;
        use crate::domain::services::authorization::{Resource, Role, Subject};

        if user.is_external() {
            return Ok(());
        }

        // Idempotent shortcut: if the user already has a default drive,
        // the atomic CTE already ran on a prior turn. The CTE writes
        // the Owner role_grant inline, so there's nothing to repair —
        // but we still re-emit the grant via `set_role` (UPSERT-safe)
        // to cover the historical case where a pre-CTE provisioning
        // path partially completed (drive created, grant missing).
        match self.drive_repo.find_default_for_user(user.id()).await {
            Ok(drive_with_name) => {
                self.authorization
                    .set_role(
                        user.id(),
                        Subject::User(user.id()),
                        Role::Owner,
                        Resource::Drive(drive_with_name.drive.id),
                        None,
                    )
                    .await
                    .map(|_grant| ())?;
                return Ok(());
            }
            Err(DriveRepositoryError::NotFound(_)) => { /* fall through to create */ }
            Err(e) => {
                return Err(DomainError::internal_error(
                    "PersonalDriveHook",
                    format!("find_default lookup: {e}"),
                ));
            }
        }

        // One atomic CTE — drive row + root folder ("Personal",
        // parent_id=NULL, drive_id pinned) + drives.root_folder_id
        // wire-up + Owner role_grant. Single SQL statement, atomic
        // against server crash mid-sequence (docs/plan/drive.md §3).
        //
        // `quota_bytes = None` (NULL in the DB) is the invariant for
        // every personal drive per plan §7: the cap for a user's
        // personal storage lives on `auth.users.storage_quota_bytes`
        // (the user envelope), not on the drive row. Passing
        // `Some(user.storage_quota_bytes())` here previously baked
        // the user quota into `drives.quota_bytes` and — combined
        // with the "0 = unlimited" convention on the user check but
        // "0 = literal zero" convention on the drive check — turned
        // "unlimited user" into "0-byte drive" (see #595). The
        // migration `20260916000000_null_personal_drive_quota.sql`
        // heals existing rows and adds a CHECK constraint pinning
        // this invariant at the schema layer.
        let drive_with_name = self
            .drive_repo
            .create_personal_drive_atomic(user.id(), None)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "PersonalDriveHook",
                    format!("create_personal_drive_atomic: {e}"),
                )
            })?;

        tracing::info!(
            target: "user_lifecycle",
            hook = "personal_drive",
            user_id = %user.id(),
            drive_id = %drive_with_name.drive.id,
            root_folder_id = %drive_with_name.drive.root_folder_id,
            "Default personal drive + root folder + owner grant provisioned (atomic CTE)"
        );
        Ok(())
    }
}

#[async_trait]
impl UserLifecycleHook for PersonalDriveLifecycleHook {
    fn name(&self) -> &'static str {
        "personal_drive"
    }

    async fn on_user_created(&self, user: &User) -> Result<(), DomainError> {
        self.provision_if_needed(user).await
    }

    /// Login is the safety net — if `on_user_created` failed at any
    /// earlier point (or the user was created in a flow that pre-dated
    /// this hook), provisioning happens here on next login.
    async fn on_user_login(&self, user: &User) -> Result<(), DomainError> {
        self.provision_if_needed(user).await
    }

    /// External → internal upgrade. `on_user_created` fired at signup
    /// with `is_external=true` and short-circuited in
    /// `provision_if_needed`. The user is now internal — same helper
    /// runs, but this time the `is_external` guard passes through and
    /// the atomic CTE creates their default drive + root folder +
    /// owner grant. Idempotent by construction: a rerun after a partial
    /// failure hits the `find_default_for_user` short-circuit.
    async fn on_upgraded_to_internal(&self, user: &User) -> Result<(), DomainError> {
        self.provision_if_needed(user).await
    }

    async fn on_user_logout(&self, _user: &User, _reason: LogoutReason) -> Result<(), DomainError> {
        // Drives don't react to logout. Explicit no-op per the
        // "no defaults" convention.
        Ok(())
    }

    async fn on_user_deleted(
        &self,
        user: &User,
        mode: DeletionMode,
        _tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DomainError> {
        // `storage.drives.default_for_user` has ON DELETE CASCADE
        // referencing `auth.users(id)`, and `storage.folders.drive_id`
        // / `storage.files.drive_id` both have ON DELETE CASCADE on
        // `storage.drives(id)` (M3). So a user delete cascades:
        // user → drive → folders → files in one transaction.
        //
        // The hook emits a per-mode tracing event so audit can tell
        // AdminDelete (currently recoverable only via DB-level rollback
        // before commit) from GdprPurge (no sweeper exists yet — the
        // variant is reserved for a future PR that adds retention).
        tracing::info!(
            target: "user_lifecycle",
            hook = "personal_drive",
            user_id = %user.id(),
            mode = ?mode,
            "Personal drive (and tree) will be removed via FK CASCADE on user delete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod mount_listing_tests {
    use super::{paginate_mount_entries, sort_mount_entries};
    use crate::application::dtos::cursor::PageCursor;
    use crate::application::dtos::folder_dto::FolderResourceCursor;
    use crate::application::ports::external_mount_ports::MountEntry;
    use crate::domain::services::authorization::ResourceKind;
    use crate::domain::services::external_mount_id::NodeId;

    fn entry(name: &str, is_dir: bool, size: u64, modified: u64) -> MountEntry {
        MountEntry {
            name: name.to_string(),
            node_id: NodeId(name.to_string()),
            is_dir,
            size,
            modified_at: modified,
            created_at: modified,
        }
    }

    fn names(entries: &[MountEntry]) -> Vec<String> {
        entries.iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn sorts_folders_first_then_name_case_insensitive() {
        let mut e = vec![
            entry("Banana.txt", false, 1, 1),
            entry("apple", true, 0, 1),
            entry("Cherry", true, 0, 1),
            entry("almond.txt", false, 1, 1),
        ];
        sort_mount_entries(&mut e, "name", false);
        assert_eq!(names(&e), ["apple", "Cherry", "almond.txt", "Banana.txt"]);
    }

    #[test]
    fn reverse_flips_order() {
        let mut e = vec![
            entry("a", false, 1, 1),
            entry("b", false, 1, 1),
            entry("d", true, 0, 1),
        ];
        sort_mount_entries(&mut e, "name", true);
        // folders-first then name, reversed.
        assert_eq!(names(&e), ["b", "a", "d"]);
    }

    #[test]
    fn sorts_by_size_modified_created() {
        let mut by_size = vec![
            entry("big", false, 100, 1),
            entry("small", false, 1, 1),
            entry("mid", false, 50, 1),
        ];
        sort_mount_entries(&mut by_size, "size", false);
        assert_eq!(names(&by_size), ["small", "mid", "big"]);

        let mut by_mtime = vec![
            entry("new", false, 1, 300),
            entry("old", false, 1, 100),
            entry("mid", false, 1, 200),
        ];
        sort_mount_entries(&mut by_mtime, "modified_at", false);
        assert_eq!(names(&by_mtime), ["old", "mid", "new"]);

        let mut by_ctime = vec![entry("z", false, 1, 9), entry("a", false, 1, 5)];
        sort_mount_entries(&mut by_ctime, "created_at", false);
        assert_eq!(names(&by_ctime), ["a", "z"]);
    }

    #[test]
    fn filters_by_kind() {
        let make = || vec![entry("dir", true, 0, 1), entry("file.txt", false, 1, 1)];
        let (files_only, _) =
            paginate_mount_entries(make(), Some(&[ResourceKind::File]), "name", false, 50, None);
        assert_eq!(names(&files_only), ["file.txt"]);

        let (folders_only, _) = paginate_mount_entries(
            make(),
            Some(&[ResourceKind::Folder]),
            "name",
            false,
            50,
            None,
        );
        assert_eq!(names(&folders_only), ["dir"]);

        let (both, _) = paginate_mount_entries(
            make(),
            Some(&[ResourceKind::File, ResourceKind::Folder]),
            "name",
            false,
            50,
            None,
        );
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn paginates_with_name_keyset_cursor() {
        let all = || {
            vec![
                entry("a", false, 1, 1),
                entry("b", false, 1, 1),
                entry("c", false, 1, 1),
                entry("d", false, 1, 1),
                entry("e", false, 1, 1),
            ]
        };

        // Page 1: limit 2 → [a, b], cursor present.
        let (p1, c1) = paginate_mount_entries(all(), None, "name", false, 2, None);
        assert_eq!(names(&p1), ["a", "b"]);
        let c1 = c1.expect("cursor after first page");
        let decoded = FolderResourceCursor::decode(&c1).expect("decodes");
        assert_eq!(decoded.sort_str.as_deref(), Some("b"));
        assert!(!decoded.reverse);

        // Page 2: resume after "b" → [c, d], cursor present.
        let (p2, c2) = paginate_mount_entries(all(), None, "name", false, 2, Some("b"));
        assert_eq!(names(&p2), ["c", "d"]);
        assert!(c2.is_some());

        // Page 3: resume after "d" → [e], no further cursor.
        let (p3, c3) = paginate_mount_entries(all(), None, "name", false, 2, Some("d"));
        assert_eq!(names(&p3), ["e"]);
        assert!(c3.is_none());
    }

    #[test]
    fn no_cursor_when_page_is_last() {
        let e = vec![entry("a", false, 1, 1), entry("b", false, 1, 1)];
        let (page, cursor) = paginate_mount_entries(e, None, "name", false, 50, None);
        assert_eq!(page.len(), 2);
        assert!(cursor.is_none());
    }

    #[test]
    fn deleted_cursor_entry_restarts_best_effort() {
        // Cursor names "zzz" which is not present → start from the top.
        let e = vec![entry("a", false, 1, 1), entry("b", false, 1, 1)];
        let (page, _) = paginate_mount_entries(e, None, "name", false, 50, Some("zzz"));
        assert_eq!(names(&page), ["a", "b"]);
    }

    #[test]
    fn empty_directory_yields_empty_page() {
        let (page, cursor) = paginate_mount_entries(vec![], None, "name", false, 50, None);
        assert!(page.is_empty());
        assert!(cursor.is_none());
    }

    #[test]
    fn limit_larger_than_len_returns_all_without_cursor() {
        let e = vec![entry("a", false, 1, 1), entry("b", false, 1, 1)];
        let (page, cursor) = paginate_mount_entries(e, None, "name", false, 100, None);
        assert_eq!(page.len(), 2);
        assert!(cursor.is_none());
    }

    #[test]
    fn kind_filter_excluding_all_yields_empty() {
        let e = vec![entry("only_dir", true, 0, 1)];
        let (page, cursor) =
            paginate_mount_entries(e, Some(&[ResourceKind::File]), "name", false, 50, None);
        assert!(page.is_empty());
        assert!(cursor.is_none());
    }

    #[test]
    fn cursor_preserves_reverse_flag() {
        let e = vec![
            entry("a", false, 1, 1),
            entry("b", false, 1, 1),
            entry("c", false, 1, 1),
        ];
        let (_p, c) = paginate_mount_entries(e, None, "name", true, 1, None);
        let decoded = FolderResourceCursor::decode(&c.unwrap()).unwrap();
        assert!(decoded.reverse);
        assert_eq!(decoded.order_by, "name");
    }
}

#[cfg(all(test, integration_tests))]
mod mount_authz_integration {
    use super::*;
    use crate::application::dtos::folder_dto::ListResourcesOptions;
    use crate::application::services::external_mount_router::{MountRouter, ResolvedId};
    use crate::application::services::file_retrieval_service::FileRetrievalService;
    use crate::application::services::mount_registry::MountRegistry;
    use crate::domain::services::external_mount_id::{NodeId, encode_child_id};
    use crate::infrastructure::repositories::pg::{
        ExternalMountPgRepository, FileBlobReadRepository, SubjectGroupPgRepository,
    };
    use crate::infrastructure::services::mount_provider_factory::DefaultMountProviderFactory;
    use crate::mount_it_support::{fresh_db, insert_mount, make_user, provision_folder};
    use std::sync::Arc;

    fn opts<'a>() -> ListResourcesOptions<'a> {
        ListResourcesOptions {
            limit: 50,
            cursor: None,
            order_by: "name",
            kinds: None,
            reverse: false,
        }
    }

    /// Build a real PgAclEngine over the live pool. The folder-ancestry cascade
    /// uses the engine's own pool; the file repo is a stub (not exercised by
    /// folder checks).
    fn acl(pool: &Arc<sqlx::PgPool>) -> Arc<PgAclEngine> {
        Arc::new(PgAclEngine::new(
            pool.clone(),
            Arc::new(FolderDbRepository::new(pool.clone())),
            Arc::new(FileBlobReadRepository::new_stub()),
            Arc::new(SubjectGroupPgRepository::new(pool.clone())),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ))
    }

    /// Provision a mount over `host`, build a wired FolderService, and return
    /// `(folder_service, mount_root_uuid_string, owner_id)`.
    async fn wire_mount(
        pool: &Arc<sqlx::PgPool>,
        host: &std::path::Path,
    ) -> (FolderService, String, Uuid) {
        let p = provision_folder(pool, "owner", "Media").await;
        insert_mount(pool, &p, host.to_str().unwrap()).await;
        let registry = Arc::new(MountRegistry::empty());
        registry
            .reload(
                &ExternalMountPgRepository::new(pool.clone()),
                &DefaultMountProviderFactory::new(),
            )
            .await;
        let router = Arc::new(MountRouter::new(registry));
        let fs = FolderService::new(
            Arc::new(FolderDbRepository::new(pool.clone())),
            acl(pool),
            Arc::new(
                crate::application::services::file_lifecycle_service::FileLifecycleService::new(),
            ),
            router,
        );
        (fs, p.mount_folder_id.to_string(), p.owner_id)
    }

    /// P2 write path: owner can mkdir/rename/delete inside a mount (reflected on
    /// the host fs); a stranger is denied; the mount root cannot be renamed.
    #[tokio::test]
    async fn owner_mkdir_rename_delete_on_mount() {
        use crate::application::dtos::folder_dto::{CreateFolderDto, RenameFolderDto};
        let (_c, pool) = fresh_db().await;
        let host = tempfile::tempdir().unwrap();
        let (fs, mount_id, owner) = wire_mount(&pool, host.path()).await;

        // mkdir under the mount root.
        let created = fs
            .create_folder_with_perms(
                CreateFolderDto {
                    name: "docs".into(),
                    parent_id: Some(mount_id.clone()),
                },
                owner,
            )
            .await
            .expect("owner may mkdir");
        assert!(host.path().join("docs").is_dir());
        assert!(created.id.starts_with("ext:"));
        assert_eq!(created.parent_id.as_deref(), Some(mount_id.as_str()));

        // Stranger may NOT mkdir.
        let stranger = make_user(&pool, "stranger").await;
        let denied = fs
            .create_folder_with_perms(
                CreateFolderDto {
                    name: "evil".into(),
                    parent_id: Some(mount_id.clone()),
                },
                stranger,
            )
            .await;
        assert!(denied.is_err());
        assert!(!host.path().join("evil").exists());

        // rename the created dir.
        let renamed = fs
            .rename_folder_with_perms(
                &created.id,
                RenameFolderDto {
                    name: "papers".into(),
                },
                owner,
            )
            .await
            .expect("owner may rename");
        assert!(host.path().join("papers").is_dir());
        assert!(!host.path().join("docs").exists());

        // The mount root itself cannot be renamed through this path.
        assert!(
            fs.rename_folder_with_perms(
                &mount_id,
                RenameFolderDto {
                    name: "nope".into()
                },
                owner
            )
            .await
            .is_err()
        );

        // delete (permanent — mounts have no trash).
        fs.delete_folder_with_perms(&renamed.id, owner)
            .await
            .expect("owner may delete");
        assert!(!host.path().join("papers").exists());
    }

    /// P2: file rename/delete and streaming upload on a mount, with authz.
    #[tokio::test]
    async fn file_rename_delete_and_upload_on_mount() {
        use crate::application::ports::external_mount_ports::MountByteStream;
        use crate::application::ports::file_ports::FileManagementUseCase;
        use crate::application::services::external_upload_service::ExternalUploadService;
        use crate::application::services::file_management_service::FileManagementService;
        use crate::infrastructure::repositories::pg::FileBlobWriteRepository;
        use bytes::Bytes;
        use futures::stream;

        let (_c, pool) = fresh_db().await;
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("a.txt"), b"hello").unwrap();

        let p = provision_folder(&pool, "owner", "Media").await;
        insert_mount(&pool, &p, host.path().to_str().unwrap()).await;
        let registry = Arc::new(MountRegistry::empty());
        registry
            .reload(
                &ExternalMountPgRepository::new(pool.clone()),
                &DefaultMountProviderFactory::new(),
            )
            .await;
        let router = Arc::new(MountRouter::new(registry.clone()));
        let cfg = registry.get(&p.mount_folder_id).expect("registered");

        let mgmt = FileManagementService::with_trash(
            Arc::new(FileBlobWriteRepository::new_stub()),
            None,
            None,
            None,
            None,
            acl(&pool),
        )
        .with_mount_router(router.clone());

        let file_id = encode_child_id(p.mount_folder_id, "a.txt");

        // Owner renames the mount file.
        let renamed = mgmt
            .rename_file_with_perms(&file_id, p.owner_id, "b.txt")
            .await
            .expect("owner may rename");
        assert!(host.path().join("b.txt").exists());
        assert!(!host.path().join("a.txt").exists());
        assert_eq!(renamed.content_hash, "");

        // Stranger may not delete.
        let stranger = make_user(&pool, "stranger").await;
        assert!(
            mgmt.delete_file_with_perms(&renamed.id, stranger)
                .await
                .is_err()
        );
        assert!(host.path().join("b.txt").exists());

        // Owner deletes (permanent — no trash).
        mgmt.delete_file_with_perms(&renamed.id, p.owner_id)
            .await
            .expect("owner may delete");
        assert!(!host.path().join("b.txt").exists());

        // Streaming upload straight to the provider.
        let upload = ExternalUploadService::new(acl(&pool));
        let body: MountByteStream<'static> =
            Box::pin(stream::once(async { Ok(Bytes::from_static(b"uploaded")) }));
        let dto = upload
            .write_file(&cfg, &NodeId::default(), "new.txt", body, p.owner_id)
            .await
            .expect("owner may upload");
        assert_eq!(dto.size, 8);
        assert_eq!(
            std::fs::read(host.path().join("new.txt")).unwrap(),
            b"uploaded"
        );

        // Stranger upload denied.
        let body2: MountByteStream<'static> =
            Box::pin(stream::once(async { Ok(Bytes::from_static(b"x")) }));
        assert!(
            upload
                .write_file(&cfg, &NodeId::default(), "evil.txt", body2, stranger)
                .await
                .is_err()
        );
        assert!(!host.path().join("evil.txt").exists());
    }

    /// P3: the WebDAV/NextCloud-facing path + listing methods resolve mount
    /// paths and enumerate provider children (PROPFIND Depth:1), and content
    /// streams from the provider.
    #[tokio::test]
    async fn webdav_path_resolution_and_listing() {
        use crate::application::dtos::pagination::PaginationRequestDto;
        use crate::application::ports::file_ports::FileRetrievalUseCase;
        use crate::application::services::file_retrieval_service::FileRetrievalService;
        use futures::TryStreamExt;

        let (_c, pool) = fresh_db().await;
        let host = tempfile::tempdir().unwrap();
        std::fs::create_dir(host.path().join("sub")).unwrap();
        std::fs::write(host.path().join("a.txt"), b"top").unwrap();
        std::fs::write(host.path().join("sub/b.txt"), b"nested!").unwrap();

        let p = provision_folder(&pool, "owner", "Media").await;
        insert_mount(&pool, &p, host.path().to_str().unwrap()).await;
        let registry = Arc::new(MountRegistry::empty());
        registry
            .reload(
                &ExternalMountPgRepository::new(pool.clone()),
                &DefaultMountProviderFactory::new(),
            )
            .await;
        let router = Arc::new(MountRouter::new(registry));
        let folder_service = FolderService::new(
            Arc::new(FolderDbRepository::new(pool.clone())),
            acl(&pool),
            Arc::new(
                crate::application::services::file_lifecycle_service::FileLifecycleService::new(),
            ),
            router.clone(),
        );
        let retrieval = FileRetrievalService::new_with_authz_for_test(
            Arc::new(FileBlobReadRepository::new_stub()),
            acl(&pool),
        )
        .with_mount_router(router.clone());

        // The mount root's materialized path; descend into it.
        let root = folder_service
            .get_folder(&p.mount_folder_id.to_string())
            .await
            .unwrap();

        // get_folder_by_path resolves a mount subdirectory → synthetic ext: id.
        let sub = folder_service
            .get_folder_by_path(&format!("{}/sub", root.path), p.drive_id)
            .await
            .expect("resolve sub dir by path");
        assert!(sub.id.starts_with("ext:"));
        assert_eq!(sub.name, "sub");

        // get_file_by_path resolves a mount file.
        let file = retrieval
            .get_file_by_path(&format!("{}/a.txt", root.path), p.drive_id)
            .await
            .expect("resolve file by path");
        assert!(file.id.starts_with("ext:"));
        assert_eq!(file.size, 3);

        // PROPFIND Depth:1 folder loop: list subdirectories of the mount root.
        let dirs = folder_service
            .list_folders_paginated_with_perms(
                Some(&p.mount_folder_id.to_string()),
                p.owner_id,
                &PaginationRequestDto::default(),
            )
            .await
            .expect("list mount subdirs");
        assert_eq!(
            dirs.items
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            ["sub"]
        );

        // PROPFIND Depth:1 file loop: list files of the mount root.
        let files = retrieval
            .list_files_batch_with_perms(
                Some(&p.mount_folder_id.to_string()),
                p.owner_id,
                None,
                100,
            )
            .await
            .expect("list mount files");
        assert_eq!(
            files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["a.txt"]
        );

        // Content streams from the provider (WebDAV GET) — resolve the nested
        // file by path, then stream it by its ext: id.
        let nested = retrieval
            .get_file_by_path(&format!("{}/sub/b.txt", root.path), p.drive_id)
            .await
            .expect("nested file");
        let content: Vec<u8> = Box::into_pin(retrieval.get_file_stream(&nested.id).await.unwrap())
            .map_ok(|b| b.to_vec())
            .try_concat()
            .await
            .unwrap();
        assert_eq!(content, b"nested!");
    }

    /// P2: a move that would cross the mount boundary is forbidden.
    #[tokio::test]
    async fn cross_boundary_move_forbidden() {
        use crate::application::dtos::folder_dto::{CreateFolderDto, MoveFolderDto};
        let (_c, pool) = fresh_db().await;
        let host = tempfile::tempdir().unwrap();
        std::fs::create_dir(host.path().join("inside")).unwrap();
        let (fs, mount_id, owner) = wire_mount(&pool, host.path()).await;

        let child_id = encode_child_id(Uuid::parse_str(&mount_id).unwrap(), "inside");

        // Moving a mount child to the user's native root (parent_id = None) is
        // a cross-backend move → UnsupportedOperation.
        let err = fs
            .move_folder_with_perms(&child_id, MoveFolderDto { parent_id: None }, owner)
            .await
            .expect_err("cross-boundary move must be forbidden");
        assert_eq!(
            err.kind,
            crate::domain::errors::ErrorKind::UnsupportedOperation
        );

        // A native folder cannot be moved INTO the mount either.
        let native = fs
            .create_folder_with_perms(
                CreateFolderDto {
                    name: "n".into(),
                    parent_id: Some(mount_id.clone()),
                },
                owner,
            )
            .await;
        // (n is created inside the mount; that's a normal mkdir, allowed.)
        assert!(native.is_ok());
    }

    /// Full read path: owner can list a mount's live contents; a stranger with
    /// no grant is denied. Exercises the REAL authorization cascade
    /// (`authz.require(Resource::Folder(mount_id))`) over ltree ancestry.
    #[tokio::test]
    async fn owner_lists_mount_contents_stranger_denied() {
        let (_c, pool) = fresh_db().await;

        // Real host directory the mount points at.
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(host.path().join("sub")).unwrap();

        let p = provision_folder(&pool, "owner", "Media").await;
        insert_mount(&pool, &p, host.path().to_str().unwrap()).await;

        // Build the registry from the DB (also exercises reload + provider build).
        let registry = Arc::new(MountRegistry::empty());
        registry
            .reload(
                &ExternalMountPgRepository::new(pool.clone()),
                &DefaultMountProviderFactory::new(),
            )
            .await;
        let router = Arc::new(MountRouter::new(registry.clone()));
        let folder_service = FolderService::new(
            Arc::new(FolderDbRepository::new(pool.clone())),
            acl(&pool),
            Arc::new(
                crate::application::services::file_lifecycle_service::FileLifecycleService::new(),
            ),
            router.clone(),
        );

        let cfg = registry.get(&p.mount_folder_id).expect("mount registered");

        // The mount root UUID classifies as a MountRoot.
        assert!(matches!(
            router.classify(&p.mount_folder_id.to_string()),
            ResolvedId::MountRoot { .. }
        ));

        // Owner lists the live directory contents.
        let (entries, _cursor) = folder_service
            .list_mount_dir_with_perms(&cfg, &NodeId::default(), p.owner_id, opts())
            .await
            .expect("owner may list");
        let mut names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["a.txt", "sub"]);

        // A stranger with no grant on the mount-root folder is denied
        // (NotFound — anti-enumeration).
        let stranger = make_user(&pool, "stranger").await;
        let err = folder_service
            .list_mount_dir_with_perms(&cfg, &NodeId::default(), stranger, opts())
            .await
            .expect_err("stranger must be denied");
        assert_eq!(err.kind, crate::domain::errors::ErrorKind::NotFound);
    }

    /// Download path authz: owner can stat/open a mount file; stranger denied.
    #[tokio::test]
    async fn owner_reads_mount_file_stranger_denied() {
        let (_c, pool) = fresh_db().await;

        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("doc.txt"), b"payload").unwrap();

        let p = provision_folder(&pool, "owner", "Media").await;
        insert_mount(&pool, &p, host.path().to_str().unwrap()).await;

        let registry = Arc::new(MountRegistry::empty());
        registry
            .reload(
                &ExternalMountPgRepository::new(pool.clone()),
                &DefaultMountProviderFactory::new(),
            )
            .await;
        let cfg = registry.get(&p.mount_folder_id).expect("registered");

        let retrieval = FileRetrievalService::new_with_authz_for_test(
            Arc::new(FileBlobReadRepository::new_stub()),
            acl(&pool),
        );

        let node = NodeId::from("doc.txt");

        // Owner: stat succeeds with the real size.
        let stat = retrieval
            .stat_mount_file_with_perms(&cfg, &node, p.owner_id)
            .await
            .expect("owner may stat");
        assert_eq!(stat.size, 7);
        assert!(!stat.is_dir);

        // Owner: open succeeds (smoke — stream is consumed elsewhere).
        assert!(
            retrieval
                .open_mount_file_with_perms(&cfg, &node, p.owner_id, None)
                .await
                .is_ok()
        );

        // The synthetic id for this file round-trips through the router.
        let ext_id = encode_child_id(p.mount_folder_id, "doc.txt");
        assert!(matches!(
            MountRouter::new(registry.clone()).classify(&ext_id),
            ResolvedId::MountChild { .. }
        ));

        // Stranger: denied.
        let stranger = make_user(&pool, "stranger").await;
        let err = retrieval
            .stat_mount_file_with_perms(&cfg, &node, stranger)
            .await
            .expect_err("stranger denied");
        assert_eq!(err.kind, crate::domain::errors::ErrorKind::NotFound);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Integration test — verifies the folder-cascade hook fix lands `on_file_deleted`
// for every file the PG cascade reaps when a folder is permanently deleted.
//
// Background: `delete_folder_with_perms` issues a bulk SQL DELETE that the PG
// `ON DELETE CASCADE` fans out to descendant folders + files. Without
// service-layer enumeration, file-id-keyed lifecycle data (thumbnails keyed
// on `ext-{file_id}.jpg`, moka cache entries, future per-file metadata)
// silently leaks. See [[bug-folder-cascade-hooks-missing]] in agent memory.
//
// How to run:
//   bash tests/common/spawn-db.sh
//   RUSTFLAGS='--cfg integration_tests' cargo test \
//       -p oxicloud --lib folder_service::cascade_hook_integration_tests
// ────────────────────────────────────────────────────────────────────────────
#[cfg(integration_tests)]
#[allow(dead_code)]
mod cascade_hook_integration_tests {
    use super::*;
    use crate::application::ports::blob_storage_ports::BlobStorageBackend;
    use crate::application::ports::file_lifecycle::FileLifecycleHook;
    use crate::infrastructure::repositories::pg::SubjectGroupPgRepository;
    use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
    use crate::infrastructure::services::dedup_service::DedupService;
    use crate::infrastructure::services::local_blob_backend::LocalBlobBackend;
    use crate::integration_test_support::{ensure_clean_test_db, test_db_url};
    use sqlx::Row;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Records every `on_file_deleted` call so the test can assert the
    /// exact set of file ids the cascade fired hooks for. Other lifecycle
    /// methods are no-ops — this fix only touches the deletion path.
    #[derive(Default)]
    struct RecordingHook {
        deleted: Mutex<Vec<String>>,
    }

    impl FileLifecycleHook for RecordingHook {
        fn on_file_created(
            &self,
            _file_id: &str,
            _blob_hash: &str,
            _content_type: &str,
            _is_new_blob: bool,
        ) {
        }
        fn on_file_copied(
            &self,
            _file_id: &str,
            _blob_hash: &str,
            _content_type: &str,
            _source_file_id: &str,
        ) {
        }
        fn on_file_updated(&self, _file_id: &str, _blob_hash: &str, _content_type: &str) {}
        fn on_file_deleted(&self, file_id: &str) {
            self.deleted.lock().unwrap().push(file_id.to_string());
        }
    }

    async fn test_pool() -> Arc<sqlx::PgPool> {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&test_db_url())
            .await
            .expect("connect to test DB — run tests/common/spawn-db.sh first");
        ensure_clean_test_db(&pool).await;
        Arc::new(pool)
    }

    /// Returns `(user_id, drive_id, drive_root_folder_id)` — same default
    /// Personal drive every internal user gets post-D0 (provisioned by
    /// `PersonalDriveLifecycleHook`).
    async fn seed_user(pool: &sqlx::PgPool) -> (Uuid, Uuid, Uuid) {
        sqlx::query(
            "SELECT u.id AS user_id, d.id AS drive_id, d.root_folder_id
               FROM auth.users u
               JOIN storage.drives d ON d.default_for_user = u.id
              LIMIT 1",
        )
        .fetch_one(pool)
        .await
        .map(|r| {
            (
                r.get::<Uuid, _>("user_id"),
                r.get::<Uuid, _>("drive_id"),
                r.get::<Uuid, _>("root_folder_id"),
            )
        })
        .expect("auth.users + storage.drives must be seeded (init-test-schema.sh)")
    }

    /// Build a real `PgAclEngine` against the test pool so
    /// `delete_folder_with_perms` can actually evaluate Owner — the user
    /// from `seed_user` owns the default drive, so `Permission::Delete`
    /// on its descendants resolves through the Owner short-circuit.
    async fn build_authz(
        pool: Arc<sqlx::PgPool>,
        dir: &TempDir,
        folder_repo: Arc<FolderDbRepository>,
    ) -> Arc<PgAclEngine> {
        let backend = Arc::new(LocalBlobBackend::new(&dir.path().join("blobs")));
        backend.initialize().await.expect("init backend");
        let dedup = Arc::new(DedupService::new(backend, pool.clone(), pool.clone()));
        let file_repo = Arc::new(FileBlobReadRepository::new(
            pool.clone(),
            dedup,
            folder_repo.clone(),
        ));
        let group_repo = Arc::new(SubjectGroupPgRepository::new(pool.clone()));
        Arc::new(PgAclEngine::new(
            pool,
            folder_repo,
            file_repo,
            group_repo,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ))
    }

    /// Seed a file row under `folder_id`. `blob_hash` is just a string —
    /// `storage.files.blob_hash` is VARCHAR(64) without a FK, so no blob
    /// row is required. The cascade decrement trigger no-ops when the
    /// hash is unknown.
    async fn seed_file_under(
        pool: &sqlx::PgPool,
        user_id: Uuid,
        drive_id: Uuid,
        folder_id: Uuid,
        label: &str,
    ) -> Uuid {
        let blob_hash = blake3::hash(format!("cascade-{label}-{}", Uuid::new_v4()).as_bytes())
            .to_hex()
            .to_string();
        // Post-D7: `user_id` omitted — the column is nullable and
        // provenance flows through `created_by` / `updated_by`.
        sqlx::query_scalar(
            "INSERT INTO storage.files
                 (name, drive_id, folder_id, blob_hash, size, created_by, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $6)
             RETURNING id",
        )
        .bind(format!(
            "rust-test-cascade-{label}-{}",
            &Uuid::new_v4().to_string()[..8]
        ))
        .bind(drive_id)
        .bind(folder_id)
        .bind(&blob_hash)
        .bind(42i64)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("seed file row")
    }

    #[tokio::test]
    async fn delete_folder_with_perms_fires_hook_for_cascaded_files() {
        let pool = test_pool().await;
        let dir = TempDir::new().unwrap();
        let (user_id, drive_id, drive_root) = seed_user(&pool).await;

        let folder_repo = Arc::new(FolderDbRepository::new(pool.clone()));
        let authz = build_authz(pool.clone(), &dir, folder_repo.clone()).await;
        let recorder: Arc<RecordingHook> = Arc::new(RecordingHook::default());
        let fls = Arc::new(
            crate::application::services::file_lifecycle_service::FileLifecycleService::new()
                .with_hook(recorder.clone() as Arc<dyn FileLifecycleHook>),
        );
        let service = FolderService::new(
            folder_repo.clone(),
            authz,
            fls,
            Arc::new(MountRouter::new(Arc::new(
                crate::application::services::mount_registry::MountRegistry::empty(),
            ))),
        );

        // Build parent/child via the production create path — it stamps
        // provenance and computes paths the same way as live uploads.
        let parent = folder_repo
            .create_folder(
                format!(
                    "rust-test-cascade-parent-{}",
                    &Uuid::new_v4().to_string()[..8]
                ),
                Some(drive_root.to_string()),
                user_id,
            )
            .await
            .expect("create parent");
        let child = folder_repo
            .create_folder(
                format!(
                    "rust-test-cascade-child-{}",
                    &Uuid::new_v4().to_string()[..8]
                ),
                Some(parent.id().to_string()),
                user_id,
            )
            .await
            .expect("create child");
        let child_uuid = Uuid::parse_str(child.id()).expect("child uuid");

        // Two files: one directly under the parent, one nested under
        // child. The cascade should reap both; the hook must fire for both.
        let parent_uuid = Uuid::parse_str(parent.id()).expect("parent uuid");
        let direct_file = seed_file_under(&pool, user_id, drive_id, parent_uuid, "direct").await;
        let nested_file = seed_file_under(&pool, user_id, drive_id, child_uuid, "nested").await;

        // Act — the production code path under test.
        service
            .delete_folder_with_perms(parent.id(), user_id)
            .await
            .expect("delete_folder_with_perms");

        // Assert — every cascaded file id appears in the hook record.
        let captured = recorder.deleted.lock().unwrap().clone();
        assert!(
            captured.contains(&direct_file.to_string()),
            "expected on_file_deleted for direct-child file {direct_file}, got {captured:?}"
        );
        assert!(
            captured.contains(&nested_file.to_string()),
            "expected on_file_deleted for nested file {nested_file}, got {captured:?}"
        );
    }
}
