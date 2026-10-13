//! DTOs for the `/api/drives` endpoint surface.
//!
//! D0 surfaces only the read-only list. Mutating endpoints
//! (`POST /api/drives` for shared-drive creation, `PATCH` for rename /
//! policy edits, membership APIs) land in D2/D3.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::dtos::grant_dto::RoleDto;
use crate::domain::entities::drive::DriveKind;
use crate::domain::repositories::drive_repository::DriveWithRootName;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DriveKindDto {
    Personal,
    Shared,
}

impl From<DriveKind> for DriveKindDto {
    fn from(k: DriveKind) -> Self {
        match k {
            DriveKind::Personal => DriveKindDto::Personal,
            DriveKind::Shared => DriveKindDto::Shared,
        }
    }
}

/// One row in `GET /api/drives` — a drive the caller can read.
///
/// `default_for_user` is `Some(<caller_id>)` for the caller's default
/// Personal drive and `None` otherwise. The picker UI uses this to put
/// the default at the top of the list and mark it as "your home".
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DriveDto {
    pub id: Uuid,
    /// Display name. Sourced from `storage.folders.name` of the row
    /// pointed at by `root_folder_id` (drives have no `name` column —
    /// see docs/plan/drive.md §3). The wire shape is unchanged from
    /// the client's perspective.
    pub name: String,
    pub kind: DriveKindDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_for_user: Option<Uuid>,
    /// The drive's mount-point folder. Folder API calls
    /// (`POST /api/folders { parent_id: <root_folder_id> }`,
    /// `PATCH /api/folders/<root_folder_id>` to rename) use this id —
    /// no polymorphic "create at drive root" surface needed.
    pub root_folder_id: Uuid,
    /// Storage cap in bytes. `None` means "no quota" (admin override /
    /// future system drives). Mutation is OxiCloud-admin only — drive
    /// owners cannot self-grant capacity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_bytes: Option<i64>,
    /// Running total of bytes consumed. Maintained incrementally in D4;
    /// on D0 this reflects the backfilled baseline.
    pub used_bytes: i64,
    /// Capability-flag bag — clients render UI affordances based on
    /// known keys (`forbid_public_links`, `include_in_photo_index`,
    /// `forbid_music_index`, …). Unknown keys preserved verbatim.
    ///
    /// `#[schema(value_type = Object)]` tightens the OpenAPI emission
    /// from "any JSON value" to `type: object, additionalProperties:
    /// true` — forward-compat for unknown keys stays, and TS SDK
    /// consumers get `{ [key: string]: unknown }` instead of a bare
    /// `unknown` they'd have to cast through every access.
    #[schema(value_type = Object)]
    pub policies: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,

    /// Highest role the **calling** user holds on this drive (direct OR
    /// group-mediated). Populated by `GET /api/drives` and the drive
    /// grants in `/api/grants/incoming/resources`. Omitted (`None`) in
    /// contexts where the caller isn't naturally a member — e.g. the
    /// outgoing-grants listing where the caller is the granter and may
    /// not have a current role on the drive themselves.
    ///
    /// **UI gating**:
    /// - `Some(Owner)` → mutation controls (add/edit/remove members, change policies, delete drive)
    /// - `Some(Editor/Contributor/Commenter)` → mutate drive contents, no membership UI
    /// - `Some(Viewer)` → read-only
    /// - `None` → show neither member list nor mutation controls
    ///
    /// See `project_caller_role_on_file_folder_dto` memory for the
    /// pattern extension to FileDto / FolderDto (deferred, perf-sensitive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_role: Option<RoleDto>,
}

impl From<DriveWithRootName> for DriveDto {
    fn from(d: DriveWithRootName) -> Self {
        Self {
            id: d.drive.id,
            name: d.root_folder_name,
            kind: d.drive.kind.into(),
            default_for_user: d.drive.default_for_user,
            root_folder_id: d.drive.root_folder_id,
            quota_bytes: d.drive.quota_bytes,
            used_bytes: d.drive.used_bytes,
            policies: d.drive.policies,
            created_at: d.drive.created_at,
            updated_at: d.drive.updated_at,
            caller_role: d.caller_role.map(RoleDto::from),
        }
    }
}

#[cfg(test)]
mod wire_shape_tests {
    //! Byte-level snapshot of the DriveDto JSON wire shape — the
    //! ISO-8601 timestamp pivot companion for `GET /api/drives` and the
    //! admin drive-management surface.

    use super::*;
    use crate::application::dtos::timestamp_helpers::epoch_to_datetime;

    #[test]
    fn timestamps_serialize_as_rfc3339_utc_strings() {
        let dto = DriveDto {
            id: Uuid::nil(),
            name: "Personal".to_string(),
            kind: DriveKindDto::Personal,
            default_for_user: None,
            root_folder_id: Uuid::nil(),
            quota_bytes: None,
            used_bytes: 0,
            policies: serde_json::json!({}),
            created_at: epoch_to_datetime(1_700_000_000),
            updated_at: epoch_to_datetime(1_700_086_400),
            caller_role: None,
        };
        let json = serde_json::to_value(dto).unwrap();
        assert_eq!(json["created_at"], "2023-11-14T22:13:20Z");
        assert_eq!(json["updated_at"], "2023-11-15T22:13:20Z");
    }
}
