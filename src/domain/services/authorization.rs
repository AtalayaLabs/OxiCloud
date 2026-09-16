//! Domain types for the ReBAC authorization model.
//!
//! These types are storage-agnostic — they describe the relationship between
//! a subject (who), a resource (what), and a permission (action). The
//! `AuthorizationEngine` port consumes them and the `PgAclEngine` implementation
//! maps them to / from `storage.role_grants` rows.

use crate::application::dtos::cursor::PageCursor;
use std::fmt;
use uuid::Uuid;

// ════════════════════════════════════════════════════════════════════════════
// Subject — who has the permission
// ════════════════════════════════════════════════════════════════════════════

/// A principal that can be granted permissions.
///
/// All variants carry a `Uuid` that uniquely identifies the subject within
/// its type's namespace.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Subject {
    /// A registered OxiCloud user (`auth.users.id`).
    User(Uuid),
    /// A user group (reserved for future use; no group CRUD in v1).
    Group(Uuid),
    /// An anonymous share token (`storage.shares.id`).
    Token(Uuid),
}

impl Subject {
    /// SQL discriminator string matching the `subject_type` CHECK constraint.
    pub fn type_str(&self) -> &'static str {
        match self {
            Subject::User(_) => "user",
            Subject::Group(_) => "group",
            Subject::Token(_) => "token",
        }
    }

    /// The raw UUID regardless of variant.
    pub fn id(&self) -> Uuid {
        match self {
            Subject::User(id) | Subject::Group(id) | Subject::Token(id) => *id,
        }
    }

    /// The `auth.users.id` behind this subject, if there is one.
    ///
    /// `None` for `Token` (a public-share link) and `Group`, neither of
    /// which has a user row. Use this — never [`Self::id`] — wherever a
    /// value is about to be written to a column with an FK to
    /// `auth.users`, or recorded as "this person did something":
    /// `Subject::id()` would hand back a share id that satisfies neither.
    pub fn user_id(&self) -> Option<Uuid> {
        match self {
            Subject::User(id) => Some(*id),
            Subject::Group(_) | Subject::Token(_) => None,
        }
    }

    /// The `storage.shares.id` behind this subject, if it is a share token.
    ///
    /// Counterpart to [`Self::user_id`]. Binding this as a nullable SQL
    /// parameter lets one query serve both principal kinds: for a user it
    /// is `NULL`, so any `subject_id = $n` comparison against it is `NULL`
    /// — never true — and the existing plan is unchanged.
    pub fn token_id(&self) -> Option<Uuid> {
        match self {
            Subject::Token(id) => Some(*id),
            Subject::User(_) | Subject::Group(_) => None,
        }
    }

    /// Reconstruct from a SQL row's `(subject_type, subject_id)` pair.
    ///
    /// `"external"` is no longer accepted: PR-2 of the external-users
    /// work folded the federated-identity case into `Subject::User(uuid)`
    /// with `auth.users.is_external = TRUE`. The DB CHECK constraint
    /// on `storage.role_grants.subject_type` was narrowed to match.
    pub fn from_parts(subject_type: &str, id: Uuid) -> Option<Self> {
        match subject_type {
            "user" => Some(Subject::User(id)),
            "group" => Some(Subject::Group(id)),
            "token" => Some(Subject::Token(id)),
            _ => None,
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.type_str(), self.id())
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Resource — what the permission is on
// ════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Resource {
    Folder(Uuid),
    File(Uuid),
    /// A drive — root scope for a tree of folders/files plus its own
    /// membership and policy bag. Added in D0; membership lives in
    /// `storage.role_grants` (no separate `drive_members` table).
    Drive(Uuid),
    /// A CalDAV calendar. Membership + sharing lives in
    /// `storage.role_grants` with `resource_type='calendar'` —
    /// replaces the pre-Round-3 dedicated `caldav.calendar_shares`
    /// table and the `check_calendar_access` bespoke helper. No
    /// cascade parent (calendars are top-level per user); the engine
    /// resolves directly against `role_grants` on the resource.
    Calendar(Uuid),
    /// A CardDAV address book. Same shape as `Calendar` —
    /// `storage.role_grants` with `resource_type='address_book'`
    /// replaces `carddav.address_book_shares` and the
    /// `check_address_book_access` bespoke helper.
    AddressBook(Uuid),
    /// A music playlist. Same shape as `Calendar`/`AddressBook` —
    /// `storage.role_grants` with `resource_type='playlist'` replaces
    /// the pre-Round-3 dedicated `music.playlist_shares` table and the
    /// bespoke `user_has_access` / `user_can_write` helpers on
    /// `MusicStorageAdapter`.
    Playlist(Uuid),
}

impl Resource {
    pub fn type_str(&self) -> &'static str {
        match self {
            Resource::Folder(_) => "folder",
            Resource::File(_) => "file",
            Resource::Drive(_) => "drive",
            Resource::Calendar(_) => "calendar",
            Resource::AddressBook(_) => "address_book",
            Resource::Playlist(_) => "playlist",
        }
    }

    pub fn id(&self) -> Uuid {
        match self {
            Resource::Folder(id)
            | Resource::File(id)
            | Resource::Drive(id)
            | Resource::Calendar(id)
            | Resource::AddressBook(id)
            | Resource::Playlist(id) => *id,
        }
    }

    pub fn from_parts(resource_type: &str, id: Uuid) -> Option<Self> {
        match resource_type {
            "folder" => Some(Resource::Folder(id)),
            "file" => Some(Resource::File(id)),
            "drive" => Some(Resource::Drive(id)),
            "calendar" => Some(Resource::Calendar(id)),
            "address_book" => Some(Resource::AddressBook(id)),
            "playlist" => Some(Resource::Playlist(id)),
            _ => None,
        }
    }

    /// Parse `(item_type, item_id)` from an API-facing pair of strings
    /// (favorites, recent, batch endpoints all take this shape).
    /// Combines UUID parse + type mapping so callers stay one-line and
    /// error shapes are identical across surfaces. Returns
    /// `DomainError::new(InvalidInput, …)` on malformed input; callers
    /// that need the anti-enum 404 shape do that separately by feeding
    /// the parsed `Resource` into `authz.require(...)`.
    pub fn parse(
        item_type: &str,
        item_id: &str,
    ) -> Result<Self, crate::common::errors::DomainError> {
        use crate::common::errors::{DomainError, ErrorKind};
        let uuid = Uuid::parse_str(item_id).map_err(|_| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Resource",
                format!("Invalid item UUID '{item_id}'"),
            )
        })?;
        Self::from_parts(item_type, uuid).ok_or_else(|| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "Resource",
                format!("Unsupported item type '{item_type}'"),
            )
        })
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.type_str(), self.id())
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Permission — what action is allowed
// ════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Permission {
    /// View resource content / list folder contents.
    Read,
    /// Create child resources inside (only meaningful on folders).
    Create,
    /// Grant permissions to other subjects.
    Share,
    /// Add comments (reserved — comments feature not implemented yet).
    Comment,
    /// Delete the resource.
    Delete,
    /// Modify the resource (rename, move, edit content).
    Update,
    /// Configure the resource's settings, add/remove members, change role
    /// assignments. Used by:
    /// - Drive owners managing drive membership and policies.
    /// - Group owners managing the group itself (Group-as-Resource, future).
    ///
    /// Folder and file resources do not currently surface a `Manage` check;
    /// the permission lives in the enum because the role bundle (`Owner`)
    /// includes it, and the resource types that DO check it (`Drive`,
    /// `Group`) are added in subsequent PRs (see `docs/plan/drive.md`).
    Manage,
}

impl Permission {
    /// Every permission, in a stable order. Used by `Role::expand()` and SQL
    /// `permission = ANY(...)` lookups.
    pub const ALL: [Permission; 7] = [
        Permission::Read,
        Permission::Create,
        Permission::Share,
        Permission::Comment,
        Permission::Delete,
        Permission::Update,
        Permission::Manage,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Create => "create",
            Permission::Share => "share",
            Permission::Comment => "comment",
            Permission::Delete => "delete",
            Permission::Update => "update",
            Permission::Manage => "manage",
        }
    }

    /// Parse a permission from its SQL discriminator string. Returns None
    /// for unknown values.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Permission::Read),
            "create" => Some(Permission::Create),
            "share" => Some(Permission::Share),
            "comment" => Some(Permission::Comment),
            "delete" => Some(Permission::Delete),
            "update" => Some(Permission::Update),
            "manage" => Some(Permission::Manage),
            _ => None,
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Grant — a row in storage.role_grants
// ════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
pub struct Grant {
    pub id: Uuid,
    pub subject: Subject,
    pub resource: Resource,
    /// Role-keyed since D-Prep cleanup: one `Grant` represents the role
    /// row in `storage.role_grants` rather than a single permission. The
    /// engine and HTTP surface no longer carry per-permission rows;
    /// callers that need permissions use `role.expand()`.
    pub role: Role,
    pub granted_by: Uuid,
    pub granted_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ════════════════════════════════════════════════════════════════════════════
// Role — a named bundle of permissions
// ════════════════════════════════════════════════════════════════════════════
//
// Roles are the load-bearing model for ReBAC grants since D-Prep. Each
// `storage.role_grants` row stores one role; the engine expands the bundle
// at read time via `Role::expand()`. Adding a role is two edits:
//   1. a variant here + match arm in `expand()` / `as_str()` / `parse()`
//   2. an `ALTER TYPE storage.grant_role ADD VALUE 'name'` migration
//
// `RoleDto` (DTO layer) carries the wire-format derives + the legacy
// `"admin"` alias for backwards compat.

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Role {
    Viewer,
    Commenter,
    Contributor,
    Editor,
    Owner,
}

impl Role {
    /// Expand the role into its permission bundle. Single source of truth —
    /// any code that needs "does this role include Permission X?" routes
    /// through here (or its inverse, `roles_implying`).
    pub fn expand(self) -> &'static [Permission] {
        match self {
            Role::Viewer => &[Permission::Read],
            Role::Commenter => &[Permission::Read, Permission::Comment],
            Role::Contributor => &[Permission::Read, Permission::Create],
            Role::Editor => &[
                Permission::Read,
                Permission::Comment,
                Permission::Create,
                Permission::Update,
            ],
            Role::Owner => &[
                Permission::Read,
                Permission::Comment,
                Permission::Create,
                Permission::Update,
                Permission::Share,
                Permission::Delete,
                Permission::Manage,
            ],
        }
    }

    /// Lowercase discriminator — matches the SQL `role` ENUM values in
    /// `storage.role_grants` (after the `::text` cast).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Commenter => "commenter",
            Role::Contributor => "contributor",
            Role::Editor => "editor",
            Role::Owner => "owner",
        }
    }

    /// Parse a role from its SQL discriminator. Returns `None` for unknown
    /// values. The `"admin"` legacy alias is handled by `RoleDto` at the
    /// wire boundary — the database only ever stores the canonical names.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Role::Viewer),
            "commenter" => Some(Role::Commenter),
            "contributor" => Some(Role::Contributor),
            "editor" => Some(Role::Editor),
            "owner" => Some(Role::Owner),
            _ => None,
        }
    }

    /// Every role, in declaration order. Mirrors the `storage.grant_role`
    /// ENUM order in PG, which is weakest-to-strongest as written here for
    /// historical reasons (`storage.grant_role` declares strongest first).
    pub const ALL: [Role; 5] = [
        Role::Viewer,
        Role::Commenter,
        Role::Contributor,
        Role::Editor,
        Role::Owner,
    ];
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Inverse of [`Role::expand`]: returns every role whose bundle contains
/// the given permission. Used by the engine to build the SQL
/// `WHERE role IN (...)` filter on hot-path queries like "what drives can
/// this caller read?".
pub fn roles_implying(permission: Permission) -> &'static [Role] {
    use Permission::*;
    match permission {
        Read => &[
            Role::Viewer,
            Role::Commenter,
            Role::Contributor,
            Role::Editor,
            Role::Owner,
        ],
        Comment => &[Role::Commenter, Role::Editor, Role::Owner],
        Create => &[Role::Contributor, Role::Editor, Role::Owner],
        Update => &[Role::Editor, Role::Owner],
        Delete => &[Role::Owner],
        Share => &[Role::Owner],
        Manage => &[Role::Owner],
    }
}

impl Grant {
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| exp < chrono::Utc::now())
    }
}

// ════════════════════════════════════════════════════════════════════════════
// ResourceKind — type-only discriminator (no id), used for filtering queries
// ════════════════════════════════════════════════════════════════════════════

/// Resource type without an id — used to filter paginated grant queries by
/// type. Mirrors the `resource_type` column values in `storage.role_grants`.
/// Add new variants here when new resource types are supported.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceKind {
    File,
    Folder,
    Drive,
    // Future: Calendar, AddressBook, Playlist, …
}

impl ResourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResourceKind::File => "file",
            ResourceKind::Folder => "folder",
            ResourceKind::Drive => "drive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "file" => Some(ResourceKind::File),
            "folder" => Some(ResourceKind::Folder),
            "drive" => Some(ResourceKind::Drive),
            _ => None,
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// IncomingGrantSummary — aggregated across multiple permission rows
// ════════════════════════════════════════════════════════════════════════════

/// Multiple `access_grants` rows for the same `(subject, resource)` collapsed
/// into one record. Used by `list_incoming_resources_paged` to avoid sending
/// duplicate resource items to the caller.
#[derive(Debug, Clone)]
pub struct IncomingGrantSummary {
    pub resource_type: ResourceKind,
    pub resource_id: Uuid,
    /// All permissions held on this resource (aggregated).
    pub permissions: Vec<Permission>,
    /// Earliest `granted_at` across all permission rows.
    pub granted_at: chrono::DateTime<chrono::Utc>,
    /// Granter of the earliest grant.
    pub granted_by: Uuid,
}

// ════════════════════════════════════════════════════════════════════════════
// OutgoingGrantEntry / OutgoingResourceSummary — per-subject grant within a
// resource that the current user shared with others
// ════════════════════════════════════════════════════════════════════════════

/// One (subject, permissions) pair within an outgoing resource summary.
/// The `subject_display` field is resolved by the SQL layer: username for
/// `user` subjects, share item_name for `token` subjects.
#[derive(Debug, Clone)]
pub struct OutgoingGrantEntry {
    pub grant_id: Uuid,
    pub subject_type: String,
    pub subject_id: Uuid,
    /// Human-readable label: username (users) or share name (tokens).
    pub subject_display: String,
    /// All permissions held by this subject on the resource (aggregated).
    pub permissions: Vec<Permission>,
    pub granted_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// True when the token subject has a password set (`storage.shares.password_hash IS NOT NULL`).
    /// Always `false` for `user` subjects.
    pub has_password: bool,
    /// True when the user subject is a magic-link-only external user
    /// (`auth.users.is_external = TRUE`). Always `false` for token and
    /// group subjects, and for internal-user subjects. Surfaced on the
    /// My Shares DTO so the frontend's per-row menu can label the
    /// notify item "Resend invitation email" (external) vs "Notify by
    /// email" (internal).
    pub is_external: bool,
}

/// All subjects that the current user has shared a single resource with,
/// together with the resource type, id, and when it was first shared.
#[derive(Debug, Clone)]
pub struct OutgoingResourceSummary {
    pub resource_type: ResourceKind,
    pub resource_id: Uuid,
    /// Earliest `granted_at` across all grants on this resource.
    pub first_shared_at: chrono::DateTime<chrono::Utc>,
    /// One entry per (subject, permissions) pair.
    pub grants: Vec<OutgoingGrantEntry>,
}

// ════════════════════════════════════════════════════════════════════════════
// GrantCursor — opaque pagination cursor for list_incoming_resources_paged
// ════════════════════════════════════════════════════════════════════════════

/// Encodes the position of the last seen item in a cursor-paginated grant
/// listing. The encoding is opaque to API callers — only the backend
/// decodes it.
///
/// The `sort_by` field must match the active sort dimension — if the caller
/// switches sort order the handler discards any cursor whose `sort_by` does
/// not match, restarting from the first page.
///
/// Sort-key fields populated per `sort_by` value:
/// - `"granted_at"` (default) — uses `granted_at` + `resource_id`
/// - `"name"`        — uses `resource_name` (lowercased) + `resource_id`
/// - `"type"`        — uses `type_order` + `resource_name` (lowercased) + `resource_id`
/// - `"granted_by"`  — uses `resource_name` (owner display name, lowercased) + `granted_at` + `resource_id`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GrantCursor {
    /// Sort dimension that was active when this cursor was produced.
    #[serde(default = "GrantCursor::default_sort")]
    pub sort_by: String,
    pub granted_at: chrono::DateTime<chrono::Utc>,
    pub resource_id: Uuid,
    /// Lowercased sort string — resource name for `"name"`/`"type"`,
    /// owner display name for `"granted_by"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_name: Option<String>,
    /// Generic integer sort key:
    /// - `"type"`  — category_order (0 = Folder, 100 = Image, …)
    /// - `"size"`  — file size in bytes (-1 = Folder sentinel)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_int: Option<i64>,
    /// Whether the result set was reversed when this cursor was produced.
    /// Must be passed unchanged on subsequent page requests.
    #[serde(default)]
    pub reverse: bool,
}

impl GrantCursor {
    fn default_sort() -> String {
        "granted_at".to_owned()
    }
}

/// Delegate encode/decode to the shared [`PageCursor`] trait.
impl PageCursor for GrantCursor {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_roundtrip() {
        let id = Uuid::new_v4();
        let cases = [Subject::User(id), Subject::Group(id), Subject::Token(id)];
        for s in cases {
            let back = Subject::from_parts(s.type_str(), s.id()).unwrap();
            assert_eq!(s, back);
        }
        assert!(Subject::from_parts("unknown", id).is_none());
        // `external` is no longer a valid subject_type — folded into `user`.
        assert!(Subject::from_parts("external", id).is_none());
    }

    #[test]
    fn resource_roundtrip() {
        let id = Uuid::new_v4();
        for r in [
            Resource::Folder(id),
            Resource::File(id),
            Resource::Calendar(id),
            Resource::AddressBook(id),
            Resource::Playlist(id),
        ] {
            let back = Resource::from_parts(r.type_str(), r.id()).unwrap();
            assert_eq!(r, back);
        }
    }

    #[test]
    fn permission_roundtrip() {
        for p in Permission::ALL {
            assert_eq!(Permission::parse(p.as_str()), Some(p));
        }
        assert!(Permission::parse("administrate").is_none());
    }
}
