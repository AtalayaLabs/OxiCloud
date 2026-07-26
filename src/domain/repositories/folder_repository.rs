//! Domain persistence port for the Folder entity.
//!
//! Defines the contract that any folder storage implementation
//! must fulfill. This trait lives in the domain because Folder is a core entity
//! of the system and its persistence contracts belong to the domain layer,
//! following the principles of Clean/Hexagonal Architecture.
//!
//! Concrete implementations (filesystem, PostgreSQL, S3, etc.) live in
//! the infrastructure layer.

use crate::common::errors::DomainError;
use crate::domain::entities::folder::Folder;
use crate::domain::services::path_service::StoragePath;
use uuid::Uuid;

// NOTE on `caller_role` for the two listing methods below:
// We deliberately do NOT compute or return the caller's role per row.
// The frontend already fetches `/api/drives` (which surfaces
// `caller_role` per drive) and cross-references by `folder.drive_id` —
// see `MoveDialog.svelte` and the config/drive page. Adding
// `caller_role` to `FolderDto` would either (a) mean redundant
// server-side work for a client-side concern the client already
// handles, or (b) drag folder-level grant cascades into the query
// which is real cost for a rare edge case. Punted; see
// `project_caller_role_on_file_folder_dto` memory.

/// Domain port for folder persistence.
///
/// Defines the CRUD and management operations required for
/// the Folder entity in the storage system.
pub trait FolderRepository: Send + Sync + 'static {
    /// Creates a new folder.
    ///
    /// `caller_id` is stamped into `created_by` and `updated_by`
    /// (D0 §14 provenance — authorship belongs to whoever issued the
    /// create, not to the parent folder's owner). Pre-D2 they're
    /// silently equivalent (only the owner can write); D2 ships
    /// shared drives where this distinction matters.
    async fn create_folder(
        &self,
        name: String,
        parent_id: Option<String>,
        caller_id: Uuid,
    ) -> Result<Folder, DomainError>;

    /// Gets a folder by its ID
    async fn get_folder(&self, id: &str) -> Result<Folder, DomainError>;

    /// Gets a folder by its storage path within a drive's tree.
    ///
    /// Post-D0, `storage.folders.path` is unique only within a single
    /// drive — root-folder names like `"Personal"` repeat across drives.
    /// The `drive_id` filter scopes the lookup to a specific drive
    /// (caller derives it from its protocol context: NC chroot, native
    /// default-drive lookup, WOPI default-drive lookup).
    async fn get_folder_by_path(
        &self,
        storage_path: &StoragePath,
        drive_id: Uuid,
    ) -> Result<Folder, DomainError>;

    /// Lists folders within a parent folder
    async fn list_folders(&self, parent_id: Option<&str>) -> Result<Vec<Folder>, DomainError>;

    /// Lists root-level folders the caller can read — scoped through
    /// drive-membership grants (`role_grants` on `resource_type='drive'`)
    /// rather than the legacy `folders.user_id` column. Group memberships
    /// are expanded inline by `storage.caller_group_ids($caller)` in the
    /// SQL. Closes [[bug-root-folder-listing-legacy-user-id]] — root
    /// folders admin created for other users but has no role on no
    /// longer surface in the admin's `GET /api/folders`.
    ///
    /// Non-root queries (parent_id != None) go through `list_folders`
    /// with the parent already permission-checked at the service layer,
    /// so this method carries no `parent_id` parameter.
    async fn list_root_folders_for_caller(
        &self,
        caller_id: Uuid,
    ) -> Result<Vec<Folder>, DomainError>;

    /// Lists folders with pagination
    async fn list_folders_paginated(
        &self,
        parent_id: Option<&str>,
        offset: usize,
        limit: usize,
        include_total: bool,
    ) -> Result<(Vec<Folder>, Option<usize>), DomainError>;

    /// Paginated companion to `list_root_folders_for_caller` — same
    /// drive-scoped predicate, adds LIMIT/OFFSET + optional
    /// window-function COUNT.
    async fn list_root_folders_for_caller_paginated(
        &self,
        caller_id: Uuid,
        offset: usize,
        limit: usize,
        include_total: bool,
    ) -> Result<(Vec<Folder>, Option<usize>), DomainError>;

    /// Keyset-paged listing of `parent_id`'s direct sub-folders in name
    /// order — `name > $after_name ORDER BY name LIMIT $limit`, one bounded
    /// index-range read per page off the partial unique index
    /// `idx_folders_unique_name`. Streaming PROPFIND drains sub-folders
    /// with this instead of `COUNT(*) OVER() … LIMIT/OFFSET`, which
    /// window-aggregated and rescanned all N sub-folders on every page
    /// (4.5x on a 5k-dir parent, benches/FOLDER-KEYSET.md). `has_next`
    /// falls out of `rows.len() == limit` — no total needed.
    ///
    /// The default implementation falls back to `list_folders` + in-memory
    /// slice so stubs and mocks compile without changes.
    async fn list_folders_batch(
        &self,
        parent_id: Option<&str>,
        after_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Folder>, DomainError> {
        let mut all = self.list_folders(parent_id).await?;
        all.sort_by(|a, b| a.name().cmp(b.name()));
        Ok(all
            .into_iter()
            .filter(|f| after_name.is_none_or(|a| f.name() > a))
            .take(limit)
            .collect())
    }

    /// Renames a folder. `caller_id` is stamped into `updated_by`
    /// alongside the `updated_at = NOW()` bump (§14 provenance).
    async fn rename_folder(
        &self,
        id: &str,
        new_name: String,
        caller_id: Uuid,
    ) -> Result<Folder, DomainError>;

    /// Moves a folder to another parent. `caller_id` is stamped into
    /// `updated_by` alongside the `updated_at = NOW()` bump
    /// (§14 provenance).
    async fn move_folder(
        &self,
        id: &str,
        new_parent_id: Option<&str>,
        caller_id: Uuid,
    ) -> Result<Folder, DomainError>;

    /// Deletes a folder
    async fn delete_folder(&self, id: &str) -> Result<(), DomainError>;

    /// Checks if a folder exists at the given path within a drive.
    ///
    /// Post-D0 `storage.folders.path` is unique only within a single
    /// drive — the `drive_id` filter scopes the existence check.
    async fn folder_exists(
        &self,
        storage_path: &StoragePath,
        drive_id: Uuid,
    ) -> Result<bool, DomainError>;

    /// Gets the path of a folder
    async fn get_folder_path(&self, id: &str) -> Result<StoragePath, DomainError>;

    // ── Trash operations ──

    /// Moves a folder to the trash. `caller_id` is stamped into
    /// `updated_by` for the root row and every cascade-trashed
    /// descendant (§14 provenance).
    async fn move_to_trash(&self, folder_id: &str, caller_id: Uuid) -> Result<(), DomainError>;

    /// Restores a folder from the trash to its original location.
    /// `caller_id` is stamped into `updated_by` for the root row and
    /// every cascade-restored descendant (§14 provenance).
    async fn restore_from_trash(
        &self,
        folder_id: &str,
        original_path: &str,
        caller_id: Uuid,
    ) -> Result<(), DomainError>;

    /// Permanently deletes a folder (used by the trash)
    async fn delete_folder_permanently(&self, folder_id: &str) -> Result<(), DomainError>;

    /// File ids in the subtree rooted at `folder_id` (inclusive).
    ///
    /// Single GiST scan on `storage.folders.lpath`. Service-layer paths
    /// that delete a folder via bulk SQL (the PG cascade reaps descendant
    /// files transparently) call this BEFORE the delete so they can fire
    /// `on_file_deleted` per-file. Without it, file-id-keyed lifecycle
    /// data (e.g. `ext-{file_id}.jpg` video thumbnails) leaks past the
    /// cascade. See [[bug-folder-cascade-hooks-missing]].
    ///
    /// Default: returns an empty vec (stubs / mocks).
    async fn list_file_ids_in_subtree(&self, folder_id: &str) -> Result<Vec<String>, DomainError> {
        let _ = folder_id;
        Ok(Vec::new())
    }

    /// Lists every folder in a subtree rooted at `folder_id` (inclusive).
    ///
    /// Uses ltree `<@` for a single GiST-indexed scan.  The result is
    /// ordered by `path` so callers can iterate in directory order.
    ///
    /// Default: falls back to `list_folders` (one level only).
    async fn list_subtree_folders(&self, folder_id: &str) -> Result<Vec<Folder>, DomainError> {
        let _ = folder_id;
        Ok(Vec::new())
    }

    /// Lists all descendant folders in a subtree (ltree-based), scoped
    /// to drives the caller can read.
    ///
    /// Returns all folders whose lpath is a descendant of the given
    /// folder's lpath. Used for recursive search — O(1) SQL via GiST
    /// index instead of O(N) recursive traversal. Drive-membership
    /// filtering (including group cascade via `caller_group_ids`) is
    /// applied inline in the SQL.
    ///
    /// The default implementation returns an empty vec (stubs / mocks).
    async fn list_descendant_folders(
        &self,
        folder_id: &str,
        name_contains: Option<&str>,
        caller_id: Uuid,
    ) -> Result<Vec<Folder>, DomainError> {
        let _ = (folder_id, name_contains, caller_id);
        Ok(Vec::new())
    }

    /// Search folders with SQL-level filtering by name and scope,
    /// restricted to drives the caller can read.
    ///
    /// - **Non-recursive** (`recursive = false`): searches direct children of
    ///   `parent_id` (or root folders when `None`).
    /// - **Recursive with `parent_id`**: delegates to `list_descendant_folders`
    ///   (ltree GiST-indexed scan).
    /// - **Recursive without `parent_id`**: searches ALL folders in drives
    ///   the caller can read, with optional name filter in SQL.
    ///
    /// The default implementation falls back to `list_folders` + in-memory
    /// filter so that stubs and mocks compile without changes.
    ///
    /// Returns `(folders, caller_flags)` where `caller_flags` is a
    /// parallel `Vec<(is_favorite, is_shared)>` aligned 1:1 with
    /// `folders` by index. The default impl fills with `(false, false)`
    /// so stubs stay trivial; the concrete PG impl computes them via
    /// per-row EXISTS on `auth.user_favorites` and `storage.role_grants`
    /// (matching the file-side `search_files_paginated` pattern).
    async fn search_folders(
        &self,
        parent_id: Option<&str>,
        name_contains: Option<&str>,
        caller_id: Uuid,
        recursive: bool,
    ) -> Result<(Vec<Folder>, Vec<(bool, bool)>), DomainError> {
        // Recursive with folder_id → use optimised ltree scan
        let folders = if recursive && let Some(fid) = parent_id {
            self.list_descendant_folders(fid, name_contains, caller_id)
                .await?
        } else {
            // Fallback: load + filter in memory (stubs / mocks)
            let all = self.list_folders(parent_id).await?;
            match name_contains {
                Some(q) if !q.is_empty() => {
                    let q = q.to_lowercase();
                    all.into_iter()
                        .filter(|f| f.name().to_lowercase().contains(&q))
                        .collect()
                }
                _ => all,
            }
        };
        let flags = vec![(false, false); folders.len()];
        Ok((folders, flags))
    }

    /// Return up to `limit` folders whose name contains `query` (case-insensitive).
    ///
    /// Results are ordered by relevance (exact > starts-with > contains) for
    /// autocomplete suggestions.
    ///
    /// `caller_id` scopes results to folders whose owning drive the caller
    /// can Read (direct or group-mediated `role_grants`). Without it the
    /// endpoint leaked names + paths across every tenant on the instance —
    /// closed as AuthZ audit finding #1 (2026-07-12).
    ///
    /// The default implementation falls back to `list_folders` + in-memory
    /// filter so that stubs and mocks compile without changes. Stub-mode
    /// callers already operate against a single tenant's data, so ignoring
    /// `caller_id` here is safe; the PG impl enforces the real scope.
    async fn suggest_folders_by_name(
        &self,
        parent_id: Option<&str>,
        query: &str,
        limit: usize,
        _caller_id: uuid::Uuid,
    ) -> Result<Vec<Folder>, DomainError> {
        let all = self.list_folders(parent_id).await?;
        let q = query.to_lowercase();
        let mut matched: Vec<Folder> = all
            .into_iter()
            .filter(|f| f.name().to_lowercase().contains(&q))
            .collect();
        matched.truncate(limit);
        Ok(matched)
    }

    /// `true` if `candidate_folder_id` is `root_folder_id` itself or any
    /// (transitive) descendant. Default impl fails closed so stubs deny
    /// access by default.
    async fn is_folder_in_subtree(
        &self,
        candidate_folder_id: &str,
        root_folder_id: &str,
    ) -> Result<bool, DomainError> {
        let _ = (candidate_folder_id, root_folder_id);
        Ok(false)
    }

    /// `true` if `file_id`'s parent folder lies within the subtree rooted
    /// at `root_folder_id`.
    async fn is_file_in_subtree(
        &self,
        file_id: &str,
        root_folder_id: &str,
    ) -> Result<bool, DomainError> {
        let _ = (file_id, root_folder_id);
        Ok(false)
    }
}
