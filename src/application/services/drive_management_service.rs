//! D2 — drive membership management service.
//!
//! Translates `POST/PATCH/DELETE /api/drives/{id}/members` into role-grant
//! writes on `resource_type='drive'`, layering D2's business rules on top:
//!
//! - **Personal-drive guard** (§2): drives with `kind='personal'` are
//!   single-user single-owner by invariant; any member mutation is refused
//!   at the service edge with `403`. Listing a personal drive's members is
//!   still allowed (returns exactly the owner row) so the UI can render the
//!   same shape across drive kinds without per-kind branching.
//!
//! - **Last-owner protection** (shared drives): removing or demoting the
//!   final `Role::Owner` would leave the drive unmanageable. Refused at the
//!   service edge — the caller has to promote someone else to Owner first,
//!   or delete the drive.
//!
//! - **Authorization**: caller must hold `Permission::Manage` on the drive
//!   to mutate; `Permission::Read` to list. `AuthorizationEngine::require`
//!   emits the canonical `authz.denied` audit line on rejection.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::common::errors::DomainError;
use crate::domain::entities::drive::DriveKind;
use crate::domain::repositories::drive_repository::{DriveRepository, DriveRepositoryError};
use crate::domain::repositories::subject_group_repository::SubjectGroupRepository;
use crate::domain::services::authorization::{Grant, Permission, Resource, Role, Subject};
use crate::infrastructure::repositories::pg::DrivePgRepository;
use crate::infrastructure::repositories::pg::SubjectGroupPgRepository;
use crate::infrastructure::repositories::pg::UserPgRepository;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

pub struct DriveManagementService {
    drive_repo: Arc<DrivePgRepository>,
    authz: Arc<PgAclEngine>,
    /// Needed to validate that a Group owner subject is non-empty at
    /// create-drive time — refusing creation with an empty group avoids
    /// constructing an orphan-owned drive (the "drive must always have
    /// ≥1 effective Owner-user" invariant from day one).
    group_repo: Arc<SubjectGroupPgRepository>,
    /// D5: `set_member_role` reads `users.is_external` to enforce
    /// `forbid_external_sharing` on the drive — closes the gap that the
    /// `POST /api/drives/{id}/members` route would otherwise open
    /// (the grant_handler check only catches `POST /api/grants`).
    user_repo: Arc<UserPgRepository>,
}

impl DriveManagementService {
    pub fn new(
        drive_repo: Arc<DrivePgRepository>,
        authz: Arc<PgAclEngine>,
        group_repo: Arc<SubjectGroupPgRepository>,
        user_repo: Arc<UserPgRepository>,
    ) -> Self {
        Self {
            drive_repo,
            authz,
            group_repo,
            user_repo,
        }
    }

    /// `POST /api/drives` — create a shared drive owned by a group.
    ///
    /// **AuthZ (D3a)**: OxiCloud-admin only. The plan (`drive.md §6`)
    /// reads "admin or group owner triggers" — D3a starts with the
    /// admin-only path; group-owner triggering can extend the gate
    /// later without changing the wire shape or the service method
    /// signature. `caller_is_admin` is resolved by the HTTP handler
    /// from `CurrentUser.role` and passed in; the service trusts it
    /// (defense-in-depth check stays at the route layer).
    ///
    /// Audit log: `drive.created` with the drive id, the owner group,
    /// and the granted_by (the admin caller).
    pub async fn create_shared_drive(
        &self,
        caller_id: Uuid,
        caller_is_admin: bool,
        name: &str,
        owner_subject: Subject,
        quota_bytes: Option<i64>,
    ) -> Result<crate::domain::repositories::drive_repository::DriveWithRootName, DomainError> {
        if !caller_is_admin {
            tracing::info!(
                target: "audit",
                event = "drive_create.rejected",
                reason = "not_admin",
                caller_id = %caller_id,
                owner_type = owner_subject.type_str(),
                owner_id = %owner_subject.id(),
                "👮🏻‍♂️ refused shared-drive create: caller is not an OxiCloud admin",
            );
            return Err(DomainError::access_denied(
                "Drive",
                "Only OxiCloud administrators can create shared drives.",
            ));
        }

        // Token subjects are share-link identities, not entities that can
        // own things. Refuse at the service edge.
        if matches!(owner_subject, Subject::Token(_)) {
            tracing::info!(
                target: "audit",
                event = "drive_create.rejected",
                reason = "invalid_owner_kind",
                caller_id = %caller_id,
                owner_type = "token",
                "👮🏻‍♂️ refused shared-drive create: owner cannot be a Token subject",
            );
            return Err(DomainError::validation_error(
                "Drive owner must be a user or a group, not a token.",
            ));
        }

        // Group owners must be non-empty — otherwise the drive is created
        // with no transitive Owner-user from day one. Per Ed's invariant:
        // "a drive must always remain with at least one Owner-user". User
        // owners trivially satisfy this.
        if let Subject::Group(gid) = owner_subject {
            let n = self.group_repo.count_members(gid).await.map_err(|e| {
                DomainError::internal_error("Drive", format!("group lookup failed: {e:?}"))
            })?;
            if n < 1 {
                tracing::info!(
                    target: "audit",
                    event = "drive_create.rejected",
                    reason = "owner_group_empty",
                    caller_id = %caller_id,
                    owner_group_id = %gid,
                    "👮🏻‍♂️ refused shared-drive create: owner group has no members",
                );
                return Err(DomainError::validation_error(
                    "Owner group has no members — the drive would have no effective Owner.",
                ));
            }
        }

        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(DomainError::validation_error("Drive name is required."));
        }

        let drive = self
            .drive_repo
            .create_shared_drive_atomic(trimmed, owner_subject, quota_bytes, caller_id)
            .await
            .map_err(|e| DomainError::internal_error("Drive", format!("create failed: {e:?}")))?;

        tracing::info!(
            target: "audit",
            event = "drive.created",
            kind = "shared",
            drive_id = %drive.drive.id,
            owner_type = owner_subject.type_str(),
            owner_id = %owner_subject.id(),
            granted_by = %caller_id,
            "🆕 shared drive created '{}' owned by {} {}",
            trimmed, owner_subject.type_str(), owner_subject.id(),
        );

        Ok(drive)
    }

    /// `GET /api/drives/{id}/members` — every role grant on the drive.
    pub async fn list_members(
        &self,
        caller_id: Uuid,
        drive_id: Uuid,
    ) -> Result<Vec<Grant>, DomainError> {
        let resource = Resource::Drive(drive_id);
        self.authz
            .require(Subject::User(caller_id), Permission::Read, resource)
            .await?;
        self.authz.list_grants_on_resource(resource).await
    }

    /// `POST /api/drives/{id}/members` (create) or
    /// `PATCH /api/drives/{id}/members/{subject_id}` (role change).
    ///
    /// `set_role` is idempotent — `(subject, resource)` is unique — so the
    /// two HTTP shapes share one service method. Returns the resulting grant.
    ///
    /// `caller_is_admin = true` skips the per-drive `Manage` check
    /// (used by `/api/admin/drives/{id}/members` so an admin who
    /// created the drive for someone else can still edit owners).
    /// Personal-drive guard and last-owner protection still apply —
    /// admin bypass is about *access*, not invariants. The audit log
    /// flags admin-driven changes via `via_admin = true` so a reader
    /// can tell what fired the mutation. The caller (HTTP handler) is
    /// authoritative for `caller_is_admin`; the route gate is the
    /// source of truth and the service trusts the flag.
    pub async fn set_member_role(
        &self,
        caller_id: Uuid,
        caller_is_admin: bool,
        drive_id: Uuid,
        subject: Subject,
        role: Role,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Grant, DomainError> {
        let resource = Resource::Drive(drive_id);
        if !caller_is_admin {
            self.authz
                .require(Subject::User(caller_id), Permission::Manage, resource)
                .await?;
        }

        self.refuse_if_personal(drive_id, "set_member_role").await?;

        // D5: `forbid_external_sharing` on a shared drive — refuses
        // grant writes whose User subject is `is_external = true`.
        // Closes the `POST /api/drives/{id}/members` gap that
        // grant_handler's same-shaped check (covering `POST /api/grants`
        // only) doesn't reach. Group/Token subjects can't be external
        // by construction, so the lookup runs only for User subjects.
        // See `docs/plan/drive.md` §8.
        self.refuse_if_forbid_external_sharing(drive_id, subject, caller_id)
            .await?;

        // D5: `forbid_owner_role_change` — locks the Owner roster
        // against non-admin callers. Fires when this write would add a
        // new Owner (role == Owner) OR demote a current Owner
        // (subject is currently Owner and role != Owner).
        self.refuse_if_forbid_owner_role_change(
            drive_id,
            subject,
            Some(role),
            caller_id,
            caller_is_admin,
            "set_member_role",
        )
        .await?;

        // Demotion of the last owner = last-owner protection trips. A fresh
        // owner-role write or any non-owner subject is fine; only the case
        // "this subject is currently the only owner AND the new role is not
        // owner" is refused. Applies to admin-bypass too: orphaning a
        // shared drive is the same category error regardless of who fires
        // the request.
        if !matches!(role, Role::Owner) {
            self.refuse_if_last_owner_change(drive_id, subject, caller_id)
                .await?;
        }

        let grant = self
            .authz
            .set_role(caller_id, subject, role, resource, expires_at)
            .await?;

        // Drop the entire drive-role cache for this drive so the new
        // grant is visible on the very next `check` — without this, a
        // caller that gets Owner via `POST /api/drives/{id}/members`
        // then immediately acts on drive content (WebDAV cross-drive
        // MOVE, admin-driven cleanup, drive management) hits the
        // stale "no role for this subject on this drive" entry
        // seeded at some earlier `check`. TTL rescues eventually,
        // but the storage_cleanup_check.sh drain pattern hits this
        // race within a single test-second and fails on `authz.denied`
        // for admin's cascade to files inside.
        self.authz
            .invalidate_drive_role_cache_for_drive(drive_id)
            .await;
        // Same freshness contract for the repo's readable-drives cache:
        // the subject's drive list changed with this grant.
        match subject {
            Subject::User(uid) => self.drive_repo.invalidate_readable_for_user(uid).await,
            _ => self.drive_repo.invalidate_readable_all(),
        }

        // D6 §11: canonical `drive.member_added` audit event covers
        // every successful membership write (add + role-refresh, since
        // the underlying `set_role` is UPSERT — distinguishing the two
        // would require an additional read and bring no extra ops
        // value). `via_admin` carries the bypass signal that used to
        // live in a separate `drive_membership.set_via_admin` event;
        // log aggregators now have one canonical name per operation.
        tracing::info!(
            target: "audit",
            event = "drive.member_added",
            drive_id = %drive_id,
            subject_type = subject.type_str(),
            subject_id = %subject.id(),
            role = role.as_str(),
            via_admin = caller_is_admin,
            by = %caller_id,
            expires_at = ?expires_at,
            "🤝 drive member added",
        );
        Ok(grant)
    }

    /// `DELETE /api/drives/{id}/members/{subject_id}`. Idempotent — removing
    /// a subject with no current grant succeeds (matches `clear_role`).
    ///
    /// `caller_is_admin` mirrors `set_member_role`: skips the per-drive
    /// `Manage` check but keeps personal-drive guard + last-owner
    /// protection. Audit emits `drive_membership.removed_via_admin`
    /// when the bypass fires.
    pub async fn remove_member(
        &self,
        caller_id: Uuid,
        caller_is_admin: bool,
        drive_id: Uuid,
        subject: Subject,
    ) -> Result<(), DomainError> {
        let resource = Resource::Drive(drive_id);
        if !caller_is_admin {
            self.authz
                .require(Subject::User(caller_id), Permission::Manage, resource)
                .await?;
        }

        self.refuse_if_personal(drive_id, "remove_member").await?;

        // D5: `forbid_owner_role_change` — locks the Owner roster
        // against non-admin callers. Fires when this would remove a
        // current Owner.
        self.refuse_if_forbid_owner_role_change(
            drive_id,
            subject,
            None, // None = removal, not a role write
            caller_id,
            caller_is_admin,
            "remove_member",
        )
        .await?;

        self.refuse_if_last_owner_change(drive_id, subject, caller_id)
            .await?;

        self.authz.clear_role(subject, resource).await?;

        // Mirror of `set_member_role`'s cache invalidation: after
        // clearing a role we MUST drop the `drive_role_cache` entries
        // targeting this drive, otherwise the just-removed subject's
        // former role stays visible until TTL expires. Same anti-drift
        // reason as the sibling add path above.
        self.authz
            .invalidate_drive_role_cache_for_drive(drive_id)
            .await;
        // And the repo's readable-drives cache: the drive must vanish
        // from the removed subject's list immediately.
        match subject {
            Subject::User(uid) => self.drive_repo.invalidate_readable_for_user(uid).await,
            _ => self.drive_repo.invalidate_readable_all(),
        }

        // D6 §11: canonical `drive.member_removed` audit event covers
        // every successful removal (owner-driven or admin bypass).
        // `via_admin` replaces the separate
        // `drive_membership.removed_via_admin` event — single name,
        // one boolean field for the bypass signal.
        tracing::info!(
            target: "audit",
            event = "drive.member_removed",
            drive_id = %drive_id,
            subject_type = subject.type_str(),
            subject_id = %subject.id(),
            via_admin = caller_is_admin,
            by = %caller_id,
            "👋 drive member removed",
        );
        Ok(())
    }

    /// `DELETE /api/drives/{id}` and `DELETE /api/admin/drives/{id}`.
    ///
    /// Policy (drive.md §6 + memos):
    /// - Caller must hold `Permission::Manage` on the drive — typically
    ///   the Owner. `caller_is_admin = true` bypasses this check; the
    ///   route gate is the access control then. Audit emits
    ///   `drive.deleted_via_admin` when the bypass fires.
    /// - The user's default Personal drive (`drives.default_for_user
    ///   IS NOT NULL`) is refused with `405` — deleting your home is a
    ///   category error. Secondary personal drives + shared drives
    ///   follow the same content-empty rule below.
    /// - The drive must be empty (no live folders other than the root,
    ///   no live files). Trashed rows are excluded — owners can
    ///   delete a drive whose trash bin still holds rows; the trash GC
    ///   cleans them up after the retention window. Non-empty drives
    ///   return `409 Conflict` so the UI can prompt the owner to
    ///   move/trash content first.
    ///
    /// On success the drive row, its root folder, and every
    /// `role_grants` row scoped to the drive are removed in one
    /// transaction.
    pub async fn delete_drive(
        &self,
        caller_id: Uuid,
        caller_is_admin: bool,
        drive_id: Uuid,
    ) -> Result<(), DomainError> {
        let resource = Resource::Drive(drive_id);
        if !caller_is_admin {
            self.authz
                .require(Subject::User(caller_id), Permission::Manage, resource)
                .await?;
        }

        let drive = self.drive_repo.get_by_id(drive_id).await.map_err(|e| {
            DomainError::internal_error("Drive", format!("Failed to fetch drive: {e:?}"))
        })?;

        if drive.drive.default_for_user.is_some() {
            tracing::info!(
                target: "audit",
                event = "drive_delete.rejected",
                reason = "default_personal_drive",
                drive_id = %drive_id,
                by = %caller_id,
                "👮🏻‍♂️ refused delete on default personal drive {drive_id}",
            );
            return Err(DomainError::operation_not_supported(
                "Drive",
                "The default Personal drive cannot be deleted.",
            ));
        }

        let empty = self.drive_repo.is_empty(drive_id).await.map_err(|e| {
            DomainError::internal_error("Drive", format!("Failed to check emptiness: {e:?}"))
        })?;
        if !empty {
            tracing::info!(
                target: "audit",
                event = "drive_delete.rejected",
                reason = "drive_not_empty",
                drive_id = %drive_id,
                by = %caller_id,
                "👮🏻‍♂️ refused delete on non-empty drive {drive_id}",
            );
            return Err(DomainError::new(
                crate::common::errors::ErrorKind::Conflict,
                "Drive",
                "Drive is not empty — move or trash its contents before deleting.",
            ));
        }

        self.drive_repo
            .delete_atomic(drive_id)
            .await
            .map_err(|e| DomainError::internal_error("Drive", format!("delete failed: {e:?}")))?;

        // Drop every cached drive-role entry for this drive so the next
        // /api/drives listing for any subject doesn't show a row pointing
        // at a deleted drive_id. Single-key cache invalidations are safe
        // even when no entry matches.
        self.authz
            .invalidate_drive_role_cache_for_drive(drive_id)
            .await;

        tracing::info!(
            target: "audit",
            event = if caller_is_admin {
                "drive.deleted_via_admin"
            } else {
                "drive.deleted"
            },
            drive_id = %drive_id,
            by = %caller_id,
            "🗑 drive deleted",
        );
        Ok(())
    }

    /// `PATCH /api/drives/{id}/policies`. OxiCloud-admin only.
    ///
    /// The drive's `policies` JSONB bag is a compliance surface — same
    /// category as `drives.quota_bytes` and `users.storage_quota_bytes`
    /// (§7). Owner mutation would make the policies self-policing
    /// (an owner could disable `forbid_external_sharing`, share, and
    /// re-enable), so mutation is restricted to the tenant operator.
    /// The handler is the gate (refuses non-admin callers with 404 for
    /// anti-enumeration); this method trusts that gate and writes
    /// unconditionally.
    ///
    /// JSONB-level merge preserves unknown keys; only the partial
    /// supplied is overwritten. Returns the post-merge typed view.
    /// Audit emits `drive.policy_changed` with the post-merge bag for
    /// steady-state observability.
    ///
    /// Ed's call, 2026-07-17: intentional deviation from the AGENTS.md
    /// "AuthZ in service layer" rule for this specific endpoint —
    /// the handler-layer admin check stays, this method stays trusting.
    /// See memory `feedback_drive_policies_admin_at_handler`.
    pub async fn update_policies(
        &self,
        caller_id: Uuid,
        drive_id: Uuid,
        partial: serde_json::Value,
    ) -> Result<crate::domain::entities::drive::DrivePolicies, DomainError> {
        let merged = self
            .drive_repo
            .update_policies(drive_id, &partial)
            .await
            .map_err(|e| match e {
                DriveRepositoryError::NotFound(_) => {
                    DomainError::not_found("Drive", drive_id.to_string())
                }
                other => DomainError::internal_error(
                    "Drive",
                    format!("update_policies failed: {other:?}"),
                ),
            })?;

        // Flush the cached typed policy view so the very next mutating
        // authz check on any resource in this drive sees the fresh
        // `read_only` value (and every other policy field). Without this,
        // a policy change would take up to `DRIVE_POLICIES_CACHE_TTL` (30 s)
        // to take effect on the hot path — unacceptable for the read_only
        // freeze, which admins expect to be effective immediately.
        self.authz
            .invalidate_drive_policies_cache_for_drive(drive_id)
            .await;

        // Logs the EFFECTIVE policy after the change, not just the keys
        // this call touched — an audit reader wants "what is in force on
        // this drive now", and a value inherited from the kind's default
        // governs it exactly as much as one set here.
        tracing::info!(
            target: "audit",
            event = "drive.policy_changed",
            drive_id = %drive_id,
            by = %caller_id,
            forbid_sharing = merged.forbid_sharing,
            forbid_external_sharing = merged.forbid_external_sharing,
            forbid_public_links = merged.forbid_public_links,
            forbid_cross_drive_move = merged.forbid_cross_drive_move,
            forbid_owner_role_change = merged.forbid_owner_role_change,
            include_in_photo_index = merged.include_in_photo_index,
            include_in_music_index = merged.include_in_music_index,
            read_only = merged.read_only,
            max_public_link_days = ?merged.max_public_link_days,
            require_public_link_password = merged.require_public_link_password,
            "📜 drive policies updated",
        );
        Ok(merged)
    }

    /// `PATCH /api/drives/{id}/quota`. OxiCloud-admin only.
    ///
    /// `quota_bytes = None` (or ≤ 0 from the wire, normalised to None
    /// here) means unlimited — matches the DB convention where a NULL
    /// `drives.quota_bytes` row is treated as no cap by
    /// `storage_usage_service`.
    ///
    /// **Refuses personal drives** with `InvalidInput`. Personal
    /// drives carry `NULL` on the row by design (memory
    /// `project_user_envelope_quota_model`) — the effective cap comes
    /// from the owner user's `storage_quota_bytes`, editable via
    /// `PUT /api/admin/users/{id}/quota`. Allowing a per-personal-drive
    /// quota here would fork the model into two competing enforcement
    /// paths; keep the envelope model intact.
    ///
    /// **Soft-quota semantic on reduction.** A newly-lowered quota
    /// can land BELOW the drive's current `used_bytes` — this method
    /// accepts that without failing. `storage_usage_service` gates
    /// new writes on `used + delta ≤ quota`, so a shared drive
    /// already over its freshly-reduced cap can only shrink (delete)
    /// until it comes back under; no existing content is retroactively
    /// touched. Ed's call: intentional design, matches how filesystems
    /// treat quota shrink (Linux xfs quota tools do the same).
    ///
    /// Follows the same handler-gates-admin deviation from AGENTS.md
    /// as `update_policies` — see memory
    /// `feedback_drive_policies_admin_at_handler`. The handler
    /// refuses non-admin callers with 404 anti-enumeration; this
    /// method trusts that gate and writes unconditionally on
    /// shared-kind drives.
    ///
    /// Emits `drive.quota_changed` for steady-state observability.
    /// Returns the persisted post-mutation quota so the handler can
    /// echo it in the API response.
    pub async fn update_quota(
        &self,
        caller_id: Uuid,
        drive_id: Uuid,
        quota_bytes: Option<i64>,
    ) -> Result<Option<i64>, DomainError> {
        // Normalise sentinel values: `0` and negative numbers on the
        // wire all mean "unlimited" — same convention the storage
        // service uses on the query side (see `check_drive_quota`).
        // Doing this once here (rather than in every caller) keeps the
        // audit line + DB row consistent.
        let quota_bytes = quota_bytes.filter(|&q| q > 0);

        let drive = self
            .drive_repo
            .get_by_id(drive_id)
            .await
            .map_err(|e| match e {
                DriveRepositoryError::NotFound(_) => {
                    DomainError::not_found("Drive", drive_id.to_string())
                }
                other => DomainError::internal_error(
                    "Drive",
                    format!("Failed to fetch drive: {other:?}"),
                ),
            })?;

        // Personal drives are refused with `InvalidInput` — a 400 that
        // the handler doesn't need to translate specially. Audit line
        // captures the attempt so an operator can see if someone is
        // trying to circumvent the envelope model.
        if drive.drive.kind == crate::domain::entities::drive::DriveKind::Personal {
            tracing::info!(
                target: "audit",
                event = "drive.quota_change_rejected",
                reason = "personal_drive_uses_user_envelope",
                drive_id = %drive_id,
                by = %caller_id,
                "👮🏻‍♂️ refused quota edit on personal drive {drive_id} — use PUT /api/admin/users/{{id}}/quota",
            );
            return Err(DomainError::validation_error(
                "Personal drive quota is not editable here — set the owner user's storage envelope via PUT /api/admin/users/{id}/quota instead.",
            ));
        }

        let persisted = self
            .drive_repo
            .update_quota(drive_id, quota_bytes)
            .await
            .map_err(|e| match e {
                DriveRepositoryError::NotFound(_) => {
                    DomainError::not_found("Drive", drive_id.to_string())
                }
                other => {
                    DomainError::internal_error("Drive", format!("update_quota failed: {other:?}"))
                }
            })?;

        // Under-usage note in the audit line: an admin should be able
        // to spot from `grep audit drive.quota_changed` whether the
        // new cap put the drive into the "over quota, delete-only"
        // state, so the numbers (used, new quota) are both present.
        tracing::info!(
            target: "audit",
            event = "drive.quota_changed",
            drive_id = %drive_id,
            by = %caller_id,
            new_quota_bytes = ?persisted,
            used_bytes = drive.drive.used_bytes,
            over_quota = persisted.map(|q| drive.drive.used_bytes > q).unwrap_or(false),
            "💾 drive quota updated",
        );

        Ok(persisted)
    }

    /// D5 `forbid_external_sharing` for `set_member_role`. Fetches the
    /// data this surface has but grant_handler doesn't (drive policies +
    /// user flags), then defers the decision + audit + canonical error
    /// to `DrivePolicies::refuse_external_sharing` — the same gate
    /// `grant_handler::create_grant` runs for File/Folder resources. One
    /// rejection shape across both entry points.
    ///
    /// Group / Token subjects can't be external by construction, so the
    /// user lookup is skipped (the gate handles those branches too, but
    /// returning early avoids a wasted SELECT on the drive row).
    async fn refuse_if_forbid_external_sharing(
        &self,
        drive_id: Uuid,
        subject: Subject,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        let Subject::User(uid) = subject else {
            return Ok(());
        };
        let drive = self.drive_repo.get_by_id(drive_id).await.map_err(|e| {
            DomainError::internal_error("Drive", format!("Failed to fetch drive: {e:?}"))
        })?;
        let policies = drive.drive.typed_policies();
        if !policies.forbid_external_sharing {
            return Ok(());
        }
        let flags = self
            .user_repo
            .get_user_flags(uid)
            .await
            .map_err(|e| DomainError::internal_error("User", format!("flags lookup: {e:?}")))?;
        policies.refuse_external_sharing(
            subject,
            flags.is_external,
            crate::domain::entities::drive::ExternalSharingGateContext {
                caller_id,
                stage: "drive_member",
                drive_id: Some(drive_id),
                resource_type: None,
                resource_id: None,
            },
        )
    }

    // ── Business rules ──────────────────────────────────────────────────────

    /// Personal drives are single-user single-owner; any member mutation is
    /// a category error (§2). Returns `Forbidden` with an audit line.
    async fn refuse_if_personal(&self, drive_id: Uuid, op: &str) -> Result<(), DomainError> {
        let drive = self.drive_repo.get_by_id(drive_id).await.map_err(|e| {
            DomainError::internal_error("Drive", format!("Failed to fetch drive: {e:?}"))
        })?;
        if matches!(drive.drive.kind, DriveKind::Personal) {
            tracing::info!(
                target: "audit",
                event = "drive_membership.rejected",
                reason = "personal_drive_immutable",
                operation = %op,
                drive_id = %drive_id,
                "👮🏻‍♂️ refused {op} on personal drive {drive_id}",
            );
            return Err(DomainError::operation_not_supported(
                "Drive",
                "Personal drives have a fixed single-owner membership and cannot be modified.",
            ));
        }
        Ok(())
    }

    /// D5 `forbid_owner_role_change`. Fetches drive policies (one PK
    /// probe), bails out early when the policy is off or the caller is
    /// admin, then determines whether the requested op actually
    /// mutates the Owner roster:
    ///
    /// - `new_role = Some(Role::Owner)` — Owner add or refresh. Owner
    ///   roster mutation.
    /// - `new_role = Some(Role::X)` and subject is currently Owner —
    ///   demotion. Owner roster mutation.
    /// - `new_role = None` (remove) and subject is currently Owner —
    ///   removal. Owner roster mutation.
    ///
    /// In any of those cases, defers to
    /// `DrivePolicies::refuse_owner_role_change` for the audit + error.
    async fn refuse_if_forbid_owner_role_change(
        &self,
        drive_id: Uuid,
        subject: Subject,
        new_role: Option<Role>,
        caller_id: Uuid,
        caller_is_admin: bool,
        operation: &'static str,
    ) -> Result<(), DomainError> {
        // Fast bypass for the tenant operator.
        if caller_is_admin {
            return Ok(());
        }
        let drive = self.drive_repo.get_by_id(drive_id).await.map_err(|e| {
            DomainError::internal_error("Drive", format!("Failed to fetch drive: {e:?}"))
        })?;
        let policies = drive.drive.typed_policies();
        if !policies.forbid_owner_role_change {
            return Ok(());
        }

        // Determine whether this op touches the Owner roster. An Owner
        // add (role == Owner) always does; a non-Owner write or a
        // removal only does when the subject currently holds Owner —
        // fetched lazily on the second case to skip the round-trip
        // when we already know the answer.
        let touches_owner = if matches!(new_role, Some(Role::Owner)) {
            true
        } else {
            let grants = self
                .authz
                .list_grants_on_resource(Resource::Drive(drive_id))
                .await?;
            grants
                .iter()
                .any(|g| g.subject == subject && matches!(g.role, Role::Owner))
        };
        if !touches_owner {
            return Ok(());
        }

        policies.refuse_owner_role_change(
            crate::domain::entities::drive::OwnerRoleChangeGateContext {
                caller_id,
                caller_is_admin,
                drive_id,
                operation,
                subject_type: subject.type_str(),
                subject_id: subject.id(),
            },
        )
    }

    /// Refuse the change if `subject` is currently the sole `Owner` on the
    /// drive and the operation would remove or demote them. A shared drive
    /// must always have at least one Owner — otherwise it becomes orphaned
    /// (no one can ever grant permissions again).
    async fn refuse_if_last_owner_change(
        &self,
        drive_id: Uuid,
        subject: Subject,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        let resource = Resource::Drive(drive_id);
        let grants = self.authz.list_grants_on_resource(resource).await?;

        // `subject` must currently BE an owner — otherwise no demotion risk.
        let subject_is_owner = grants
            .iter()
            .any(|g| g.subject == subject && matches!(g.role, Role::Owner));
        if !subject_is_owner {
            return Ok(());
        }

        let owner_count = grants
            .iter()
            .filter(|g| matches!(g.role, Role::Owner))
            .count();
        if owner_count <= 1 {
            tracing::info!(
                target: "audit",
                event = "drive_membership.rejected",
                reason = "last_owner",
                drive_id = %drive_id,
                caller_id = %caller_id,
                subject_type = subject.type_str(),
                subject_id = %subject.id(),
                "👮🏻‍♂️ refused last-owner removal on drive {drive_id}",
            );
            return Err(DomainError::validation_error(
                "A shared drive must keep at least one Owner — promote another \
                 member to Owner first, or delete the drive.",
            ));
        }
        Ok(())
    }
}
