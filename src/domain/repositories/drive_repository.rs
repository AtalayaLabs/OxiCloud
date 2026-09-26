//! Repository for [`Drive`] entities backed by `storage.drives`.
//!
//! Drives have no separate membership table — owner/editor/viewer
//! membership lives in `storage.role_grants` with
//! `resource_type='drive'`. That means **listing the drives a user can
//! reach goes through the role-grant query, not through this
//! repository**. This repo handles:
//!
//!   * Creating a drive (used by the user-creation lifecycle hook and
//!     by D3's shared-drive flow).
//!   * Looking up a single drive by id (used by the engine's owner_of /
//!     check paths, by `/api/drives/{id}`, and by the drive picker).
//!   * Finding the caller's default drive (used by the Photos / Music
//!     endpoints and by D1's redirect-from-`/` logic).
//!
//! Membership-flavoured queries (e.g. "list every drive user X can
//! read") live in `DriveListingService` (post-D0) which reads
//! `role_grants` and resolves the matching drive rows here.

use thiserror::Error;
use uuid::Uuid;

use crate::domain::entities::drive::{Drive, DriveKind};

#[derive(Debug, Error)]
pub enum DriveRepositoryError {
    #[error("Drive not found: {0}")]
    NotFound(String),
    /// A user already has a default drive set — partial unique index on
    /// `default_for_user` rejects a second one. Surfaces the constraint
    /// explicitly so the lifecycle hook can no-op idempotently.
    #[error("User already has a default drive: {0}")]
    DefaultDriveAlreadyExists(String),
    #[error("Invalid drive kind: {0}")]
    InvalidKind(String),
    #[error("Storage error: {0}")]
    StorageError(String),
}

/// A drive paired with the display name from its root folder.
///
/// `storage.drives` has no `name` column under the D0 design
/// (docs/plan/drive.md §3) — the display name lives on
/// `storage.folders.name` of the row pointed at by `drive.root_folder_id`.
/// Read paths join the two tables and hand callers this view-model so the
/// API surface can continue to expose a single "drive with name" shape
/// without a follow-up query per drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveWithRootName {
    pub drive: Drive,
    /// The drive's display name. Sourced from `storage.folders.name`
    /// of the root folder via JOIN at read time.
    pub root_folder_name: String,
    /// Highest role the calling user holds on this drive (direct OR
    /// group-mediated). Populated by `list_readable_by` (which already
    /// JOINs `role_grants` for accessibility, so the role is in scope at
    /// query time). `None` for repo methods called without a caller
    /// context (`get_by_id`, `get_by_ids`, `find_default_for_user`,
    /// `create_personal_drive_atomic`) — the DTO layer omits the field
    /// via `#[serde(skip_serializing_if = "Option::is_none")]`.
    ///
    /// See [[project-caller-role-on-file-folder-dto]] for the pattern
    /// extension to FileDto/FolderDto with a perf warning.
    pub caller_role: Option<crate::domain::services::authorization::Role>,
}

#[async_trait::async_trait]
pub trait DriveRepository: Send + Sync + 'static {
    /// Atomically create a personal drive together with its root folder
    /// and the owner role_grant — all four DB writes in a single SQL
    /// statement (docs/plan/drive.md §3 "Atomic creation"). The
    /// statement runs as its own implicit transaction in autocommit mode
    /// so a server crash mid-statement leaves no half-row state.
    ///
    /// The root folder is created with name `"Personal"` (the canonical
    /// default) and `parent_id IS NULL`. The drive's `root_folder_id`
    /// is wired to point at it before the statement commits.
    ///
    /// Returns `DefaultDriveAlreadyExists` when the owner already has a
    /// default drive — relies on the partial UNIQUE index on
    /// `default_for_user`.
    async fn create_personal_drive_atomic(
        &self,
        owner_id: Uuid,
        quota_bytes: Option<i64>,
    ) -> Result<DriveWithRootName, DriveRepositoryError>;

    /// Atomically create a **shared** drive together with its root folder
    /// and the initial Owner-role grant. Mirrors
    /// `create_personal_drive_atomic` but with three differences:
    ///   - `kind='shared'`, `default_for_user=NULL`
    ///   - root folder name is caller-supplied (validated upstream)
    ///   - Owner role_grant subject is caller-supplied — either a
    ///     single `User` (becomes the sole drive Owner) or a `Group`
    ///     (the group's transitive user members all gain the Owner
    ///     role via subject expansion). Token subjects are refused at
    ///     the service edge.
    ///
    /// `granted_by` is recorded on the role_grant row + on the root
    /// folder's `created_by` / `updated_by` columns for audit
    /// traceability — the OxiCloud admin who provisioned the drive.
    ///
    /// **AuthZ contract**: this method performs no authorization. The
    /// service layer MUST verify the caller has the OxiCloud `admin`
    /// system role (D3a). If `owner_subject` is `Group`, the service
    /// MUST also have verified the group has ≥1 user member —
    /// otherwise the drive is created with no effective Owner-user
    /// and would breach the "drive must always have ≥1 effective
    /// Owner" invariant from day one.
    async fn create_shared_drive_atomic(
        &self,
        name: &str,
        owner_subject: crate::domain::services::authorization::Subject,
        quota_bytes: Option<i64>,
        granted_by: Uuid,
    ) -> Result<DriveWithRootName, DriveRepositoryError>;

    /// Fetch a drive by id together with its display name. `NotFound`
    /// when no row matches.
    async fn get_by_id(&self, id: Uuid) -> Result<DriveWithRootName, DriveRepositoryError>;

    /// Batch fetch — returns one row per existing id. Missing ids are
    /// silently dropped (matches the `get_files_by_ids` / `get_folders_by_ids`
    /// shape used by `list_shared_with_me`). Caller-side `HashMap<Uuid, _>`
    /// lookup gives `Option<Drive>` semantics for stale grants whose drive
    /// was deleted between listing and resolution.
    async fn get_by_ids(
        &self,
        ids: &[Uuid],
    ) -> Result<Vec<DriveWithRootName>, DriveRepositoryError>;

    /// Return the caller's default personal drive paired with its
    /// display name, or `NotFound` if they don't have one (e.g.
    /// external users; users created before the lifecycle hook fired).
    /// Drives the Photos timeline scope, the `/api/recent/*` scope, and
    /// D1's redirect-from-`/`.
    async fn find_default_for_user(
        &self,
        user_id: Uuid,
    ) -> Result<DriveWithRootName, DriveRepositoryError>;

    /// Canonical "what is this user's home root folder id?" lookup.
    ///
    /// Returns `Some(uuid)` for any internal user with a default personal
    /// drive (the lifecycle hook provisions one at registration), and
    /// `None` for users who have no default drive (external users; users
    /// created before the hook existed). The id identifies the user's
    /// home **by drive ownership** (`default_for_user == user_id`),
    /// never by folder name — users can rename their home, so any code
    /// that wants to ask "is this folder the user's home?" must compare
    /// folder ids, not names.
    ///
    /// Storage errors (DB unreachable, etc.) bubble up as `Err`; the
    /// "user simply has no home" case is `Ok(None)`, not an error.
    async fn home_root_folder_id_for(
        &self,
        user_id: Uuid,
    ) -> Result<Option<Uuid>, DriveRepositoryError> {
        match self.find_default_for_user(user_id).await {
            Ok(d) => Ok(Some(d.drive.root_folder_id)),
            Err(DriveRepositoryError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// List drives the caller can read, resolved via `role_grants` for
    /// `resource_type='drive'`. Group memberships (direct + transitive)
    /// are expanded inline by the `storage.caller_group_ids(caller)`
    /// SQL function — callers pass only the caller's uuid, no
    /// expansion ceremony.
    ///
    /// Returns rows in a stable order: default drive first (if any),
    /// then by display name. The `/api/drives` handler relies on that
    /// order for the picker UI without a follow-up sort.
    /// Returned as `Arc<Vec<…>>`: warm hits are a refcount bump straight
    /// off the per-user cache instead of a deep clone of every row's
    /// Strings — this runs per DAV request with an explicit drive
    /// selector.
    async fn list_readable_by(
        &self,
        caller_id: Uuid,
    ) -> Result<std::sync::Arc<Vec<DriveWithRootName>>, DriveRepositoryError>;

    /// `true` when the drive holds no live (non-trashed) folders other
    /// than its own root and no live files at all. Used by
    /// `DriveManagementService::delete_drive` to enforce the
    /// "empty-before-delete" rule — owners must clear / trash the
    /// content first so a single click can't wipe a populated drive.
    async fn is_empty(&self, drive_id: Uuid) -> Result<bool, DriveRepositoryError>;

    /// Drop the cached readable-drive list for one user. Called by
    /// service-layer code paths that mutate state affecting a specific
    /// caller's drive listing (grant writes, membership changes) but
    /// don't reach through the drive-repo itself. Default no-op — the
    /// no-cache stubs need no plumbing.
    async fn invalidate_readable_for_user(&self, _user_id: Uuid) {}

    /// Drop every cached readable-drive list. Called when the affected
    /// user set is unknown at this layer — group-subject grants, drive
    /// deletion, policy edits, root-folder renames (drive.name is
    /// sourced from the root folder, so a rename affects the listing
    /// for every user with a grant on the drive). Default no-op.
    fn invalidate_readable_all(&self) {}

    /// Drop every entry in the "default drive per user" cache. Called
    /// from paths that mutate a drive's display name or its root
    /// folder id at the concrete cache level (root-folder rename is
    /// the only one today). Same class of bug as
    /// `invalidate_readable_all` — the cache holds a `DriveWithRootName`
    /// with `root_folder_name` baked in, so a rename would otherwise
    /// stay stale for the cache TTL. Default no-op.
    fn invalidate_default_drive_all(&self) {}

    /// Hard-delete a drive: its `role_grants` rows, its root folder,
    /// and the drive row itself, in one transaction. Caller is
    /// responsible for ensuring `is_empty` first; this method does
    /// **not** re-check. Returns `NotFound` if the drive id is gone.
    async fn delete_atomic(&self, drive_id: Uuid) -> Result<(), DriveRepositoryError>;

    /// List every drive on the system, regardless of caller membership.
    ///
    /// Used by the admin panel's `GET /api/admin/drives`. Distinct from
    /// `list_readable_by` (which filters by `role_grants`) because an
    /// admin who creates a shared drive for someone else has no grant
    /// on it — but still needs to see, audit, and manage it. The HTTP
    /// gate (admin-only middleware) is what makes the unrestricted
    /// listing safe; no role-based filtering happens here.
    ///
    /// Returns rows ordered by display name. `caller_role` is left
    /// unset on the returned `DriveWithRootName` — the admin is not
    /// necessarily a member, so the per-drive role would be misleading
    /// here.
    async fn list_all(&self) -> Result<Vec<DriveWithRootName>, DriveRepositoryError>;

    /// Resolve a file's owning drive policies in one round-trip. Used by
    /// D5 enforcement points (`forbid_public_links`, `forbid_sharing`, …)
    /// to gate per-resource actions without a separate file-lookup +
    /// drive-lookup pair.
    ///
    /// Returns `NotFound` when the file id is gone or its `drive_id`
    /// doesn't resolve to a drive row (a state the no-orphan triggers
    /// prevent in production, but the caller should still propagate the
    /// 404 cleanly).
    async fn get_policies_for_file(
        &self,
        file_id: Uuid,
    ) -> Result<crate::domain::entities::drive::DrivePolicies, DriveRepositoryError>;

    /// Resolve a folder's owning drive policies in one round-trip. Same
    /// shape as [`Self::get_policies_for_file`].
    async fn get_policies_for_folder(
        &self,
        folder_id: Uuid,
    ) -> Result<crate::domain::entities::drive::DrivePolicies, DriveRepositoryError>;

    /// Resolve a file's owning drive id + its drive's policies in one
    /// round-trip. Used by D5 `forbid_cross_drive_move` enforcement —
    /// the move-file service needs both pieces (drive id to compare
    /// against the destination, policies to gate). Returns `NotFound`
    /// when the file row or its drive_id doesn't resolve.
    async fn get_drive_id_and_policies_for_file(
        &self,
        file_id: Uuid,
    ) -> Result<(Uuid, crate::domain::entities::drive::DrivePolicies), DriveRepositoryError>;

    /// Same as [`Self::get_drive_id_and_policies_for_file`] for folders.
    async fn get_drive_id_and_policies_for_folder(
        &self,
        folder_id: Uuid,
    ) -> Result<(Uuid, crate::domain::entities::drive::DrivePolicies), DriveRepositoryError>;

    /// Read the default policy bag for a drive kind.
    ///
    /// Returns the raw JSONB rather than a typed `DrivePolicies` because the
    /// caller needs to distinguish "this knob is set in the default" from
    /// "this knob happens to be false" — the typed view collapses the two.
    /// Returns an empty object when no row exists, which resolves to
    /// today's all-permissive behaviour.
    async fn get_policy_defaults(
        &self,
        kind: crate::domain::entities::drive::DriveKind,
    ) -> Result<serde_json::Value, DriveRepositoryError>;

    /// Replace (not merge) the default bag for a kind, returning the stored
    /// value.
    ///
    /// Replace, deliberately: the per-drive PATCH merges so an admin can
    /// nudge one knob without restating the rest, but a DEFAULT is a
    /// complete statement of posture for that kind. Merging would make
    /// "unset this knob" impossible to express — the caller could never
    /// take a key back out.
    async fn set_policy_defaults(
        &self,
        kind: crate::domain::entities::drive::DriveKind,
        policies: &serde_json::Value,
        updated_by: Uuid,
    ) -> Result<serde_json::Value, DriveRepositoryError>;

    /// Every drive of the given kind, with its OVERRIDE bag, for the drift
    /// scan and the impact preview.
    ///
    /// Returns `(drive_id, name_or_none, overrides)`. Deliberately the
    /// overrides rather than the resolved policy — the comparison needs to
    /// know what was explicitly decided, not what it resolves to.
    async fn list_drives_with_overrides(
        &self,
        kind: crate::domain::entities::drive::DriveKind,
    ) -> Result<Vec<(Uuid, Option<String>, serde_json::Value)>, DriveRepositoryError>;

    /// Resolve just the drive id of a folder — fast PK probe used by
    /// the cross-drive-move gate to identify the move destination
    /// (where we don't need policies, just the discriminator). Returns
    /// `NotFound` when the folder row doesn't exist.
    async fn drive_id_for_folder(&self, folder_id: Uuid) -> Result<Uuid, DriveRepositoryError>;

    /// Merge the given partial policy bag into the drive's existing
    /// `policies` JSONB. JSONB-level merge preserves unknown keys already
    /// present on disk (the column stays the canonical bag — see
    /// `DrivePolicies::from_value`). `caller_id` is recorded for the audit
    /// log emitted at the service layer.
    ///
    /// **Returns the EFFECTIVE policy, not the drive's own overrides** —
    /// the kind's default with this drive's bag laid on top, matching what
    /// every read path returns. The admin modal writes this response into
    /// its cached row, so returning raw overrides would flip that row from
    /// effective to raw on the first save and inherited knobs would start
    /// displaying as unset.
    ///
    /// A key whose value is JSON `null` is REMOVED rather than stored —
    /// that is how a drive stops overriding a knob and follows its kind's
    /// default again. A stored null would survive the merge and clear the
    /// inherited value instead, and would disagree with
    /// `DrivePolicies::resolve`, which parses into an `Option` and
    /// inherits.
    ///
    /// Caller is responsible for the `Manage` permission check; this
    /// method does not re-verify.
    ///
    /// `partial` is a raw JSON object carrying **only** the keys the
    /// caller wants to change — the repo passes it verbatim to the
    /// `policies || $partial` JSONB merge. Using the typed
    /// `DrivePolicies` here would serialise every field (including
    /// unset ones as `false`) and clobber other flags on the row;
    /// keeping the merge on the raw `Value` preserves the
    /// partial-update semantic the handler documents.
    async fn update_policies(
        &self,
        drive_id: Uuid,
        partial: &serde_json::Value,
    ) -> Result<crate::domain::entities::drive::DrivePolicies, DriveRepositoryError>;

    /// Set the drive-level storage quota on a **shared** drive.
    ///
    /// `quota_bytes = None` means unlimited (matches the wire and DB
    /// convention — `drives.quota_bytes` is nullable; a NULL row → the
    /// storage-usage service treats it as no cap).
    ///
    /// **Personal drives are refused at the service layer** — their
    /// effective cap comes from the owner user's
    /// `users.storage_quota_bytes` envelope (see the memory
    /// `project_user_envelope_quota_model`). This method does not
    /// re-check the kind; the service does, and only calls the repo
    /// with a validated shared-drive id.
    ///
    /// A newly-lowered quota can be **under** the drive's current
    /// `used_bytes` — that's a deliberate soft-quota semantic. The
    /// `storage_usage_service` gates NEW writes on
    /// `used + delta <= quota`, so a shared drive already over its
    /// freshly-reduced cap can only shrink (delete) until it comes back
    /// under the limit; no existing content is retroactively touched.
    ///
    /// Cache invalidation mirrors `update_policies` — the user-keyed
    /// readable-drive-list caches carry the quota alongside the row so
    /// they'd serve stale numbers otherwise; the default-drive cache
    /// carries the DriveWithRootName which also includes the quota.
    ///
    /// Returns the persisted post-mutation value so the caller can
    /// echo it back in the audit log and API response.
    async fn update_quota(
        &self,
        drive_id: Uuid,
        quota_bytes: Option<i64>,
    ) -> Result<Option<i64>, DriveRepositoryError>;
}

/// Convenience: convert the canonical kind discriminator from its SQL
/// form into the typed enum. Mirrored on the entity for symmetry.
impl DriveKind {
    pub fn from_sql(s: &str) -> Result<Self, DriveRepositoryError> {
        DriveKind::parse(s).ok_or_else(|| DriveRepositoryError::InvalidKind(s.to_owned()))
    }
}

/// Locate the user's home root folder within a generic list of items,
/// identifying it by **drive ownership** (never by folder name — users
/// can rename their home).
///
/// `id_fn` extracts a candidate `Uuid` from each item. The callsite
/// commonly works with `FolderDto` (whose `id` is a `String`); the
/// closure is `|f| Uuid::parse_str(&f.id).ok()`. Items whose ids can't
/// be parsed are simply skipped — `position` ignores them.
///
/// Defined as a free function (not a trait method) so the
/// `DriveRepository` trait stays `dyn`-compatible. Generic over both
/// the repo (`R`) and the item shape (`T`); accepts both concrete repo
/// types and `&dyn DriveRepository`.
///
/// Returns `None` when:
///   * The user has no default drive (external users, pre-hook accounts).
///   * The user's home root folder id isn't present in `items`.
///   * The repo lookup errored (storage error is swallowed to None —
///     callers wanting fail-loud semantics should call
///     `home_root_folder_id_for` directly).
pub async fn position_of_user_home_root_folder<R, T>(
    drive_repo: &R,
    user_id: Uuid,
    items: &[T],
    id_fn: impl Fn(&T) -> Option<Uuid>,
) -> Option<usize>
where
    R: DriveRepository + ?Sized,
{
    let home_id = drive_repo
        .home_root_folder_id_for(user_id)
        .await
        .ok()
        .flatten()?;
    items.iter().position(|item| id_fn(item) == Some(home_id))
}
