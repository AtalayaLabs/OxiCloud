//! DTOs for the ReBAC `/api/grants` REST endpoints.
//!
//! The wire shapes are intentionally separate from the domain types
//! (`Subject`, `Resource`, `Permission`, `Grant`) so that domain stays
//! storage-agnostic and DTOs can evolve with the HTTP contract.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::application::dtos::cursor::{CursorListResponse, CursorQuery, PageCursor};
use crate::application::dtos::drive_dto::DriveDto;
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::domain::services::authorization::{Grant, Permission, Resource, Role, Subject};

// ════════════════════════════════════════════════════════════════════════════
// Subject / Resource / Permission DTOs
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SubjectTypeDto {
    User,
    Group,
    Token,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SubjectDto {
    #[serde(rename = "type")]
    pub kind: SubjectTypeDto,
    pub id: Uuid,
}

impl From<SubjectDto> for Subject {
    fn from(dto: SubjectDto) -> Self {
        match dto.kind {
            SubjectTypeDto::User => Subject::User(dto.id),
            SubjectTypeDto::Group => Subject::Group(dto.id),
            SubjectTypeDto::Token => Subject::Token(dto.id),
        }
    }
}

impl From<Subject> for SubjectDto {
    fn from(s: Subject) -> Self {
        let (kind, id) = match s {
            Subject::User(id) => (SubjectTypeDto::User, id),
            Subject::Group(id) => (SubjectTypeDto::Group, id),
            Subject::Token(id) => (SubjectTypeDto::Token, id),
        };
        SubjectDto { kind, id }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceTypeDto {
    Folder,
    File,
    Drive,
    Calendar,
    AddressBook,
    Playlist,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResourceDto {
    #[serde(rename = "type")]
    pub kind: ResourceTypeDto,
    pub id: Uuid,
}

impl From<ResourceDto> for Resource {
    fn from(dto: ResourceDto) -> Self {
        match dto.kind {
            ResourceTypeDto::Folder => Resource::Folder(dto.id),
            ResourceTypeDto::File => Resource::File(dto.id),
            ResourceTypeDto::Drive => Resource::Drive(dto.id),
            ResourceTypeDto::Calendar => Resource::Calendar(dto.id),
            ResourceTypeDto::AddressBook => Resource::AddressBook(dto.id),
            ResourceTypeDto::Playlist => Resource::Playlist(dto.id),
        }
    }
}

impl From<Resource> for ResourceDto {
    fn from(r: Resource) -> Self {
        let (kind, id) = match r {
            Resource::Folder(id) => (ResourceTypeDto::Folder, id),
            Resource::File(id) => (ResourceTypeDto::File, id),
            Resource::Drive(id) => (ResourceTypeDto::Drive, id),
            Resource::Calendar(id) => (ResourceTypeDto::Calendar, id),
            Resource::AddressBook(id) => (ResourceTypeDto::AddressBook, id),
            Resource::Playlist(id) => (ResourceTypeDto::Playlist, id),
        };
        ResourceDto { kind, id }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDto {
    Read,
    Create,
    Share,
    Comment,
    Delete,
    Update,
    Manage,
}

impl From<PermissionDto> for Permission {
    fn from(p: PermissionDto) -> Self {
        match p {
            PermissionDto::Read => Permission::Read,
            PermissionDto::Create => Permission::Create,
            PermissionDto::Share => Permission::Share,
            PermissionDto::Comment => Permission::Comment,
            PermissionDto::Delete => Permission::Delete,
            PermissionDto::Update => Permission::Update,
            PermissionDto::Manage => Permission::Manage,
        }
    }
}

impl From<Permission> for PermissionDto {
    fn from(p: Permission) -> Self {
        match p {
            Permission::Read => PermissionDto::Read,
            Permission::Create => PermissionDto::Create,
            Permission::Share => PermissionDto::Share,
            Permission::Comment => PermissionDto::Comment,
            Permission::Delete => PermissionDto::Delete,
            Permission::Update => PermissionDto::Update,
            Permission::Manage => PermissionDto::Manage,
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Roles — the load-bearing model for ReBAC grants
// ════════════════════════════════════════════════════════════════════════════
//
// One row per role assignment in `storage.role_grants.role` (a
// `storage.grant_role` ENUM). The engine expands the bundle at query time
// via `Role::expand()` on the domain enum. Adding a role is two edits:
// the variant + match arm on `Role`, and an `ALTER TYPE
// storage.grant_role ADD VALUE 'name'` migration.

/// Wire-format wrapper around the domain `Role` enum. Carries the
/// serde/utoipa derives. Maps 1:1 to/from `Role` via `From`.
///
/// The historical `"admin"` alias for `Owner` (used during the D-Prep
/// dual-write window for cached clients) has been retired in the cleanup
/// PR — the OxiCloud UI emits `"owner"` exclusively. Stragglers receive
/// a 422 on POST/PUT, which surfaces the upgrade cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RoleDto {
    Viewer,
    Commenter,
    Contributor,
    Editor,
    Owner,
}

impl From<RoleDto> for Role {
    fn from(r: RoleDto) -> Self {
        match r {
            RoleDto::Viewer => Role::Viewer,
            RoleDto::Commenter => Role::Commenter,
            RoleDto::Contributor => Role::Contributor,
            RoleDto::Editor => Role::Editor,
            RoleDto::Owner => Role::Owner,
        }
    }
}

impl From<Role> for RoleDto {
    fn from(r: Role) -> Self {
        match r {
            Role::Viewer => RoleDto::Viewer,
            Role::Commenter => RoleDto::Commenter,
            Role::Contributor => RoleDto::Contributor,
            Role::Editor => RoleDto::Editor,
            Role::Owner => RoleDto::Owner,
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Request DTOs
// ════════════════════════════════════════════════════════════════════════════

/// Subject shape accepted by `POST /api/grants`. Internally-tagged enum
/// so the existing `{type:"user", id:"..."}` payload keeps working
/// alongside the new `{type:"email", email:"..."}` variant that feeds
/// the invite-by-email flow. The response-side [`SubjectDto`] stays
/// unchanged — externals resolve to `Subject::User(uuid)` with
/// `is_external = TRUE` on the user row, never a distinct subject type.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubjectInputDto {
    User {
        id: Uuid,
    },
    Group {
        id: Uuid,
    },
    Token {
        id: Uuid,
    },
    /// Invite-by-email. Lazily provisions an external user with the
    /// normalised address as both username and email when no match
    /// exists; otherwise reuses the existing user. Triggers a magic-link
    /// invitation email when the resolved user has no other login
    /// credential.
    Email {
        email: String,
    },
    OpenCloudMesh {
        share_with: String,
    },
}

/// `POST /api/grants` — create or refresh a role assignment.
///
/// Strictly role-keyed since the cleanup PR: callers send exactly one
/// role; the engine writes a single row in `storage.role_grants`. The
/// historical per-permission shape (`permissions: [...]`) was dropped —
/// the OxiCloud UI is the only known caller and it already sends `role`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateGrantDto {
    pub subject: SubjectInputDto,
    pub resource: ResourceDto,
    pub role: RoleDto,
    /// Optional expiry for the grant. RFC 3339 / ISO 8601.
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// `PUT /api/grants/role` — reconcile a subject's role on a resource.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateRoleDto {
    pub subject: SubjectDto,
    pub resource: ResourceDto,
    pub role: RoleDto,
    /// Optional expiry applied to every grant written or updated by this call.
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ════════════════════════════════════════════════════════════════════════════
// Response DTOs
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GrantDto {
    pub id: Uuid,
    pub subject: SubjectDto,
    pub resource: ResourceDto,
    /// Role-keyed since D-Prep cleanup — one row in `storage.role_grants`
    /// is one `GrantDto`. The bundle of underlying permissions is implied
    /// by the role and recomputed client-side from the same lookup table.
    pub role: RoleDto,
    pub granted_by: Uuid,
    pub granted_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<Grant> for GrantDto {
    fn from(g: Grant) -> Self {
        Self {
            id: g.id,
            subject: g.subject.into(),
            resource: g.resource.into(),
            role: g.role.into(),
            granted_by: g.granted_by,
            granted_at: g.granted_at,
            expires_at: g.expires_at,
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Notification DTOs (PR N1) — surfaced in the create-grant and /notify
// responses so the frontend can show actionable toasts ("Notified Carol",
// "Carol already notified recently", "Notified 8 of 10 group members").
// ════════════════════════════════════════════════════════════════════════════

/// One per resolved recipient. `kind` discriminates; sibling fields are
/// only meaningful for the matching variant. Tagged JSON shape:
///
/// ```json
/// { "kind": "sent",           "detail": "magic_link" }
/// { "kind": "sent",           "detail": "plain_notification" }
/// { "kind": "coalesced",      "last_sent_at": "2026-06-04T12:00:00Z" }
/// { "kind": "rate_limited",   "retry_after_secs": 1800 }
/// { "kind": "not_applicable", "reason": "recipient_opted_out" }
/// ```
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotifyOutcomeDto {
    /// An email actually went out for this recipient. `detail` is
    /// `"magic_link"` (external invitation with a fresh token) or
    /// `"plain_notification"` (internal "you got a new grant" mail).
    Sent { detail: String },
    /// Skipped silently because this (granter, recipient) pair was
    /// notified less than the coalesce-window ago. The grant is still
    /// recorded; the recipient sees it next time they log in.
    Coalesced {
        last_sent_at: chrono::DateTime<chrono::Utc>,
    },
    /// Per-recipient hard cap (5/h) reached. The caller may retry after
    /// `retry_after_secs`.
    RateLimited { retry_after_secs: u32 },
    /// No mail was dispatched for this recipient. `reason` is one of:
    /// - `"recipient_opted_out"`  — user toggled `notify_on_share = false`
    /// - `"operator_disabled"`    — `OXICLOUD_NOTIFY_INTERNAL_USERS_ON_SHARE=false`
    /// - `"no_email"`             — user row has no email on file
    /// - `"oidc_only_no_email"`   — OIDC-only user with no email claim
    /// - `"subject_is_token"`     — anonymous link share (the surface
    ///   that creates the grant or the `/notify` endpoint maps this to 409)
    NotApplicable { reason: String },
}

/// The aggregated result of dispatching share notifications for ONE grant
/// action (one `create_grant` request OR one `/notify` call). Carries
/// per-recipient outcomes so the frontend can render a single
/// summary-style toast:
///
/// - `total_recipients = 1`, `outcomes[0] = Sent` → "Notified Carol"
/// - `total_recipients = 1`, `outcomes[0] = Coalesced` → "Carol already
///   notified recently"
/// - `total_recipients = N`, all `Sent` → "Notified all N group members"
/// - `total_recipients = N`, mix → "Notified 8 of 10 — 2 opted out"
///
/// `total_recipients` equals `outcomes.len()` after resolution. For
/// token-subject grants it is `0` (no human recipient — no toast).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotifyOutcomeSetDto {
    pub total_recipients: usize,
    pub outcomes: Vec<NotifyOutcomeDto>,
}

impl NotifyOutcomeSetDto {
    /// Construct an empty set (token subjects, no recipients to notify).
    pub fn empty() -> Self {
        Self {
            total_recipients: 0,
            outcomes: Vec::new(),
        }
    }

    /// Construct from a list of outcomes, deriving `total_recipients`
    /// from the list length. Use this from `RecipientNotificationService`
    /// after the per-member loop completes.
    pub fn from_outcomes(outcomes: Vec<NotifyOutcomeDto>) -> Self {
        Self {
            total_recipients: outcomes.len(),
            outcomes,
        }
    }
}

/// Response body for `POST /api/grants`. Wraps the array of created
/// grants (one per `permission` in the request) together with the
/// aggregated notification result. Replaces the previous bare
/// `Vec<GrantDto>` shape; the frontend share modal is updated in
/// lockstep.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreateGrantResponseDto {
    pub grants: Vec<GrantDto>,
    pub notification: NotifyOutcomeSetDto,
}

// ════════════════════════════════════════════════════════════════════════════
// Shared-with-me DTOs  (GET /api/grants/incoming/resources)
// ════════════════════════════════════════════════════════════════════════════

/// Query parameters for `GET /api/grants/incoming/resources`.
///
/// `limit`, `cursor`, and `sort_by` follow the standard [`CursorQuery`]
/// contract.  They are declared directly here rather than via
/// `#[serde(flatten)]` because `serde_urlencoded` (Axum's query extractor)
/// does not support flattening.
#[derive(Debug, Deserialize, IntoParams)]
pub struct SharedWithMeQuery {
    /// Maximum number of items to return (1–200, default 50).
    #[serde(default = "CursorQuery::default_limit")]
    pub limit: u32,
    /// Opaque cursor from a previous response. Omit to start from the
    /// most-recently-granted item.
    pub cursor: Option<String>,
    /// Sort dimension. Supported values: `"granted_at"` (default),
    /// `"granted_by"` (for swimlane grouping).
    pub sort_by: Option<String>,
    /// Comma-separated resource types to include, e.g. `file,folder`.
    /// Omit to return all known types.
    pub resource_types: Option<String>,
    /// Reverse the sort order. Default `false` (normal order).
    /// Must be the same on all pages of the same result set — the cursor
    /// carries this flag so the server can validate consistency.
    #[serde(default)]
    pub reverse: bool,
}

impl SharedWithMeQuery {
    /// Returns `limit` clamped to `[1, 200]`.
    pub fn limit_clamped(&self) -> usize {
        self.limit.clamp(1, 200) as usize
    }

    /// Decode the optional cursor string.  Invalid cursor → start from top.
    pub fn decode_cursor<C: PageCursor>(&self) -> Option<C> {
        self.cursor.as_deref().and_then(C::decode)
    }
}

/// The resource payload for one item in the shared-with-me list.
///
/// The variant is discriminated by `resource_type` on the parent
/// [`SharedWithMeItemDto`].  Serialised as the inner object (no wrapper key)
/// via `#[serde(untagged)]`, so consumers see the file/folder fields directly
/// under the `resource` key.
#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum ResourceContentDto {
    File(FileDto),
    Folder(FolderDto),
    Drive(DriveDto),
}

/// One item in the shared-with-me list.
///
/// `resource_type` indicates whether `resource` contains a file or a folder.
/// Using a single `resource` field (instead of nullable `file`/`folder` pairs)
/// makes adding new resource types backward-compatible — only `resource_type`
/// gains a new variant; the wrapper shape stays the same.
#[derive(Debug, Serialize, ToSchema)]
pub struct SharedWithMeItemDto {
    pub resource_type: ResourceTypeDto,
    /// All permissions the caller holds on this resource (aggregated).
    pub permissions: Vec<PermissionDto>,
    /// Earliest grant date for this resource.
    pub granted_at: chrono::DateTime<chrono::Utc>,
    /// UUID of the user who created the (earliest) grant.
    pub granted_by: Uuid,
    /// Full resource details. Shape is determined by `resource_type`.
    pub resource: ResourceContentDto,
}

/// Derive the closest-matching role label from a set of permissions.
///
/// **Legacy helper for the dual-write window.** Once D-Prep ships and the
/// engine reads `role_grants.role` directly, this function becomes unused
/// and is dropped in the cleanup PR. Kept here so callers that still hit
/// `access_grants` and reconstruct a role for display can stay working
/// during the transition.
///
/// Emits the new five-role roster on output (`"viewer"` / `"commenter"` /
/// `"contributor"` / `"editor"` / `"owner"`). Note this is **lossy** for
/// permission sets that don't match a bundle exactly — but D-Prep's
/// pre-flight refuses to migrate any such cluster, so post-migration data
/// only contains bundle-shaped sets.
pub fn role_from_permissions(perms: &[Permission]) -> &'static str {
    let has_read = perms.contains(&Permission::Read);
    let has_comment = perms.contains(&Permission::Comment);
    let has_create = perms.contains(&Permission::Create);
    let has_update = perms.contains(&Permission::Update);
    let has_delete = perms.contains(&Permission::Delete);
    let has_share = perms.contains(&Permission::Share);

    if has_delete && has_share {
        "owner"
    } else if has_create && has_update {
        "editor"
    } else if has_read && has_create && !has_update {
        "contributor"
    } else if has_read && has_comment && !has_create && !has_update {
        "commenter"
    } else {
        "viewer"
    }
}

/// Response for `GET /api/grants/incoming/resources`.
pub type SharedWithMeDto = CursorListResponse<SharedWithMeItemDto>;

// ════════════════════════════════════════════════════════════════════════════
// My-Shares DTOs  (GET /api/grants/outgoing/resources)
// ════════════════════════════════════════════════════════════════════════════

/// One (subject, permissions) entry within an outgoing resource item.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OutgoingResourceGrantDto {
    pub grant_id: Uuid,
    /// `"user"` | `"token"`
    pub subject_type: String,
    pub subject_id: Uuid,
    /// Human-readable label (username for users, share name for tokens).
    pub subject_display: String,
    /// Role label: `"viewer"` | `"commenter"` | `"contributor"` | `"editor"`
    /// | `"owner"`. Emitted by `role_from_permissions()` during the dual-write
    /// window; once D-Prep cleanup lands this is read directly from
    /// `storage.role_grants.role`. The legacy `"admin"` spelling is no longer
    /// emitted — clients that cached it must accept `"owner"` too (the API
    /// `Role::parse` still accepts `"admin"` on input for one release).
    pub role: String,
    pub granted_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether the token has a password set. Always `false` for user subjects.
    pub has_password: bool,
    /// True when the subject is a magic-link-only external user
    /// (PR N2). Always `false` for token and group subjects, and for
    /// internal users. Used by the My Shares per-row menu to choose
    /// between "Resend invitation email" (external) and "Notify by
    /// email" (internal).
    #[serde(default)]
    pub is_external: bool,
}

/// One item in the my-shares list.
#[derive(Debug, Serialize, ToSchema)]
pub struct OutgoingResourceItemDto {
    pub resource_type: ResourceTypeDto,
    /// Earliest grant date across all subjects on this resource.
    pub first_shared_at: chrono::DateTime<chrono::Utc>,
    /// Full resource details. Shape is determined by `resource_type`.
    pub resource: ResourceContentDto,
    /// One entry per (subject, permissions) pair.
    pub grants: Vec<OutgoingResourceGrantDto>,
}

/// Response for `GET /api/grants/outgoing/resources`.
pub type MySharesDto = CursorListResponse<OutgoingResourceItemDto>;
