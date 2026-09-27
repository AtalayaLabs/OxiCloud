//! Drive — the top-level container that owns a tree of folders/files.
//!
//! Drives replaced the per-user `My Folder - <username>` wrapper at D0.
//! Every folder and file row carries a `drive_id` (added by D0's
//! migration); a drive is the natural unit of quota, sharing, and
//! lifecycle. Membership is expressed through `storage.role_grants` rows
//! with `resource_type='drive'` — there is no separate `drive_members`
//! table.
//!
//! ## Kinds
//!
//! Two kinds today; the discriminant is the `kind` column with a CHECK
//! constraint.
//!
//! - **`personal`** — single-user, single-owner. The owner is captured
//!   by `default_for_user` (for the default Personal drive) or by an
//!   Owner role_grant on a secondary personal drive. Personal drives
//!   refuse `add_member`, `remove_member`, and `delete_drive` (when
//!   it's the user's only or default drive). A user can have multiple
//!   personal drives — one is marked default (`default_for_user =
//!   <uid>`), the others are secondaries (`default_for_user = NULL`,
//!   one Owner row in role_grants pinning them to the same user).
//!
//! - **`shared`** — multi-member, group-aware, full role roster
//!   (viewer / commenter / contributor / editor / owner). Members
//!   come from role_grants; group subjects expand transitively via
//!   the existing `subject_groups` machinery. Last-owner protection
//!   applies on member removal and drive deletion. Quota is set by
//!   the drive owner (or admin); `used_bytes` tracks consumption.
//!
//! Future kinds (e.g. `system` for built-in scratch space) drop in by
//! extending the CHECK + the `DriveKind` enum.
//!
//! ## Policies
//!
//! `policies` is a JSONB bag carrying feature flags / capability toggles
//! that drive owners can flip without a schema change. Known keys live in
//! `docs/plan/drive.md` §8 and §15 (e.g. `forbid_public_links`,
//! `include_in_photo_index`, `forbid_music_index`). Unknown keys are
//! preserved by the application — the schema is intentionally permissive
//! so future capability flags can land without a migration.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::services::authorization::Subject;

/// Drive kind discriminant. Mirrors the `storage.drives.kind` CHECK
/// constraint values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriveKind {
    /// Single-owner storage compartment. Cannot have members added or
    /// removed via the membership API; the owner is fixed for the drive's
    /// lifetime.
    Personal,
    /// Multi-member drive supporting the full role roster. Membership is
    /// open to admin/owner-driven changes through the membership API.
    Shared,
}

impl DriveKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DriveKind::Personal => "personal",
            DriveKind::Shared => "shared",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "personal" => Some(DriveKind::Personal),
            "shared" => Some(DriveKind::Shared),
            _ => None,
        }
    }
}

/// Domain entity for a row in `storage.drives`.
///
/// Drives are pure metadata under the D0 design (docs/plan/drive.md §3):
/// no `name` column — the display name lives on the root folder pointed
/// at by `root_folder_id`. Code that needs the name pairs this struct
/// with a JOIN through `storage.folders`; see the repository's
/// `DriveWithRootName` view-model.
///
/// Field-level constraints are enforced at the SQL layer (CHECK on
/// `kind`, partial UNIQUE on `default_for_user`). The struct mirrors
/// the column set 1:1; behaviour beyond field access lives in
/// `DriveRepository` and `DriveService` (post-D0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// Stable identifier. Generated server-side at creation.
    pub id: Uuid,
    /// Discriminant — see [`DriveKind`].
    pub kind: DriveKind,
    /// Set iff this is the user's default personal drive (UNIQUE in SQL
    /// via a partial index `WHERE default_for_user IS NOT NULL`). NULL
    /// on shared drives and on secondary personal drives.
    pub default_for_user: Option<Uuid>,
    /// The drive's mount-point folder. The column is NULLable in SQL
    /// only because the atomic creation CTE writes it mid-statement
    /// (a column-level `NOT NULL` would refuse the initial drive INSERT
    /// — see docs/plan/drive.md §3). After any successful creation path,
    /// this is populated; code reading `Drive` may treat it as `Uuid`,
    /// not `Option<Uuid>`. A NULL at read time is a data-invariant bug.
    pub root_folder_id: Uuid,
    /// Soft cap on this drive's storage usage, in bytes. `None` means
    /// "no quota" (rare; reserved for admin overrides). The default
    /// initial quota for a fresh personal drive is taken from the
    /// owner's `auth.users.storage_quota_bytes` at creation time.
    /// **Mutation is OxiCloud-admin only** (docs/plan/drive.md §7) —
    /// not in the drive `owner` role bundle.
    pub quota_bytes: Option<i64>,
    /// Running total of bytes consumed. Maintained incrementally by
    /// upload/delete paths in D4; on D0 still reflects the pre-Drive
    /// per-user counters via the backfill.
    pub used_bytes: i64,
    /// Capability flags / feature toggles. Extensible JSONB — see
    /// `docs/plan/drive.md` §8 and §15 for the known keys.
    pub policies: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Drive {
    /// `true` for the user's default personal drive (the only drive for
    /// which `default_for_user` is set to that user's id).
    pub fn is_default_for(&self, user_id: Uuid) -> bool {
        self.default_for_user == Some(user_id)
    }

    /// Typed view of `policies` for enforcement code. Lenient deserialise:
    /// unknown keys are preserved on disk (the column stays the canonical
    /// JSONB bag) but ignored here, missing keys default to `false`.
    /// See `docs/plan/drive.md` §8.
    pub fn typed_policies(&self) -> DrivePolicies {
        DrivePolicies::from_value(&self.policies)
    }

    /// `true` if this drive is a personal drive of any kind (default or
    /// secondary). Encapsulates the kind check at the call site.
    pub fn is_personal(&self) -> bool {
        matches!(self.kind, DriveKind::Personal)
    }
}

/// Typed mirror of the `policies` JSONB. Five known keys; the JSONB column
/// remains the source of truth and may carry unknown keys verbatim — this
/// struct is a read view for enforcement and a write view for the policy
/// PATCH endpoint. Every field defaults to `false` (everything allowed)
/// so a freshly-created drive doesn't need a populated policy bag.
///
/// See `docs/plan/drive.md` §8 for the enforcement matrix
/// (which callsite each key is checked at).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(default)]
pub struct DrivePolicies {
    /// Disables per-resource grants on resources in this drive. Drive-level
    /// membership (Owner/Editor/Viewer) still works.
    ///
    /// The BROADER rule: it covers public links too, so it is enforced at
    /// `grant_handler::create_grant` (via [`DrivePolicies::refuse_sharing`])
    /// **and** on the public-link path (via
    /// [`DrivePolicies::refuse_public_links`]). Enforcing only the first left
    /// a drive that forbade sharing outright still minting anonymous links,
    /// while the admin editor greyed `forbid_public_links` out as "already
    /// enforced by" this one.
    pub forbid_sharing: bool,
    /// Blocks grants whose subject has `users.is_external = true`. Enforced
    /// at `magic_link_invite_service::resolve_or_create_recipient` and
    /// `grant_handler::create_grant`.
    pub forbid_external_sharing: bool,
    /// Blocks anonymous-link (token-share) creation on resources in this
    /// drive. Enforced at `share_service::create_shared_link`.
    pub forbid_public_links: bool,
    /// Blocks MOVE when `src.drive_id != dst.drive_id`. Enforced at the
    /// move endpoints. Lands paired with D6's cross-drive move work.
    pub forbid_cross_drive_move: bool,
    /// Locks the Owner-role membership set: no owner can be added,
    /// removed, or demoted by another owner — only OxiCloud admin can
    /// change the Owner roster. Editor / Viewer mutations by remaining
    /// owners are unaffected. Personal drives are already
    /// single-owner-immutable via `refuse_if_personal`, so this policy
    /// only adds value on shared drives. Enforced at
    /// `DriveManagementService::set_member_role` (refuses Owner role
    /// writes) and `::remove_member` (refuses Owner removals) when the
    /// caller is non-admin.
    pub forbid_owner_role_change: bool,
    /// Opts this drive into the `/api/photos` timeline (§15). Non-default
    /// drives are omitted by default so a random shared folder full of
    /// screenshots doesn't bleed into the personal timeline; owners flip
    /// this on when the drive genuinely is a photo library (e.g. "Family
    /// Photos"). Default personal drives get `true` on creation via the
    /// `PersonalDriveLifecycleHook` + a one-shot backfill for existing
    /// rows, so the SQL predicate is a single positive rule with no
    /// per-kind carve-out. Read at `file_blob_read_repository::
    /// list_media_files` + `list_geo_clusters`. See §15 for the query
    /// shape and rationale.
    pub include_in_photo_index: bool,
    /// Same shape as `include_in_photo_index`, applied to the Music
    /// library surface (playlists today; a `/api/music/tracks` library
    /// view later). Symmetric opt-in — Music was originally cross-drive
    /// via a `forbid_music_index` opt-out, but that mixed-form naming
    /// created "one include-in, one forbid" confusion and the
    /// "shared audio is always intentional" claim didn't hold under
    /// scrutiny (voicemail MP3s in a work drive shouldn't bleed into
    /// the personal library). See §15.
    pub include_in_music_index: bool,
    /// **Full freeze / legal-hold.** When `true`, every mutation on
    /// resources in this drive is refused — user-initiated and
    /// background alike. Compliance-grade guarantee:
    ///
    /// - User-initiated: enforced at `PgAclEngine::check_inner`, which
    ///   short-circuits `Create` / `Update` / `Delete` / `Share`
    ///   permissions on any resource in a read-only drive. Read still
    ///   passes. Manage-on-Drive still passes so admins can un-freeze.
    /// - Background jobs: the periodic trash-retention purge and
    ///   orphan-upload sweep filter out read-only drives at SELECT
    ///   time (SQL-side `JOIN storage.drives … WHERE (policies->>
    ///   'read_only')::boolean IS NOT TRUE`). Retention clock keeps
    ///   ticking; on unfreeze, the next sweep tick catches up.
    ///
    /// Applies to both personal and shared drives — a user winding
    /// down their account, freezing a secondary personal archive, or
    /// putting a shared drive on legal hold all use the same knob.
    /// Mutation is admin-only via `PATCH /api/drives/{id}/policies`
    /// (per §8 — same carve-out as every other policy).
    pub read_only: bool,
    /// Cap, in days, on how long an anonymous link in this drive may live.
    /// `None` = no cap, which is the LAXEST value — a link that never
    /// expires is permitted. Enforced at `share_service::create_shared_link`.
    ///
    /// The first non-boolean knob. Note the expiry it constrains lives on
    /// `storage.role_grants.expires_at` for the token grant, NOT on
    /// `storage.shares` — that column was dropped in
    /// `20260601000000_rebac_expiry_and_perms_cleanup.sql`.
    pub max_public_link_days: Option<u32>,
    /// Requires every anonymous link in this drive to carry a password.
    /// Enforced at `share_service::create_shared_link`; the corresponding
    /// state is `storage.shares.password_hash IS NOT NULL`.
    pub require_public_link_password: bool,
}

/// Every policy knob, as a stable machine name.
///
/// Single source of truth for "what knobs exist" on the Rust side — the
/// comparison, the defaults validation and the drift scan all iterate this
/// rather than each repeating the list. Mirrors `policyDefs` in
/// `frontend/src/lib/utils/drivePolicies.ts`.
pub const POLICY_KNOBS: &[&str] = &[
    "forbid_sharing",
    "forbid_external_sharing",
    "forbid_public_links",
    "forbid_cross_drive_move",
    "forbid_owner_role_change",
    "include_in_photo_index",
    "include_in_music_index",
    "read_only",
    "max_public_link_days",
    "require_public_link_password",
];

/// Knobs that may NOT appear in a per-kind default bag.
///
/// `read_only` is an operational state — freeze THIS drive, for a reason,
/// usually for a duration — not a standing posture. A default that froze
/// every drive at once has no legitimate use, and its drift finding
/// ("writable while the default says frozen") would be pure noise. See
/// `docs/plan/drive-default-policies.md`.
pub const NON_DEFAULTABLE_KNOBS: &[&str] = &["read_only"];

/// Returns `false` when the knob is meaningless for the given drive kind.
///
/// `forbid_owner_role_change` is moot on a personal drive: membership is
/// immutable there by a hardcoded guard (`refuse_if_personal`) that fires
/// before any policy is read, so the flag changes nothing either way.
/// Comparing it would produce a drift finding no admin can act on — and a
/// compliance list with false positives is one nobody reads.
pub fn knob_applies_to_kind(knob: &str, kind: DriveKind) -> bool {
    !(knob == "forbid_owner_role_change" && matches!(kind, DriveKind::Personal))
}

/// Per-drive policy OVERRIDES — the shape actually stored in
/// `storage.drives.policies` once defaults exist.
///
/// The distinction from [`DrivePolicies`] is the whole point: here `None`
/// means **inherit the kind's default**, where in `DrivePolicies` a `false`
/// means "this is the effective value". Before defaults existed the two
/// were the same thing, and an absent key simply meant `false`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct DrivePolicyOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forbid_sharing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forbid_external_sharing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forbid_public_links: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forbid_cross_drive_move: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forbid_owner_role_change: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_in_photo_index: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_in_music_index: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_public_link_days: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_public_link_password: Option<bool>,
}

impl DrivePolicyOverrides {
    /// Lenient parse, same contract as [`DrivePolicies::from_value`]: a
    /// malformed bag reads as "no overrides" rather than refusing the read.
    pub fn from_value(value: &serde_json::Value) -> Self {
        use serde::Deserialize as _;
        Self::deserialize(value).unwrap_or_default()
    }
}

/// How a knob's values order by strictness.
///
/// Needed because the direction is **not uniform**: for the `forbid_*`
/// family `true` is stricter, but `include_in_*` are opt-INs to a global
/// index, so `false` is stricter there. Getting this backwards silently
/// inverts the drift report, which is why each knob states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// `true` is the stricter value (`forbid_*`, `read_only`, …).
    TrueIsStricter,
    /// `false` is the stricter value (`include_in_*` — opt-in to exposure).
    FalseIsStricter,
    /// Smaller is stricter; `None` (no cap) is the laxest value of all.
    SmallerIsStricter,
}

pub fn knob_strictness(knob: &str) -> Strictness {
    match knob {
        "include_in_photo_index" | "include_in_music_index" => Strictness::FalseIsStricter,
        "max_public_link_days" => Strictness::SmallerIsStricter,
        _ => Strictness::TrueIsStricter,
    }
}

impl DrivePolicies {
    /// Parse from the raw JSONB. Lenient — unknown keys are dropped from
    /// the typed view but remain in the source `serde_json::Value`. A
    /// malformed bag (e.g. wrong type) falls back to the all-false default
    /// rather than refusing the read; enforcement code never panics on
    /// existing data.
    pub fn from_value(value: &serde_json::Value) -> Self {
        // Deserialize straight from the borrowed `Value` (`T::deserialize(&Value)`,
        // via serde_json's `Deserializer for &Value`) instead of
        // `serde_json::from_value(value.clone())` — the old form cloned the ENTIRE
        // policies DOM before walking it, on every drive-policy read (move/copy,
        // shared-link creation, grant). Byte-identical (same derived `Deserialize`
        // impl); the lenient `unwrap_or_default` fallback is unchanged.
        // (benches/ROUND23.md §J2)
        use serde::Deserialize as _;
        Self::deserialize(value).unwrap_or_default()
    }

    /// Effective policy = the kind's default, with the drive's explicit
    /// overrides laid on top.
    ///
    /// This is what every enforcement site consumes; none of them changed
    /// when defaults landed, because they still receive a fully-resolved
    /// `DrivePolicies`. Mirrors the SQL side exactly — `storage.drives_effective`
    /// computes `default || overrides`, and `||` is right-biased in Postgres
    /// for the same reason the overrides win here.
    pub fn resolve(default: &DrivePolicies, overrides: &DrivePolicyOverrides) -> Self {
        Self {
            forbid_sharing: overrides.forbid_sharing.unwrap_or(default.forbid_sharing),
            forbid_external_sharing: overrides
                .forbid_external_sharing
                .unwrap_or(default.forbid_external_sharing),
            forbid_public_links: overrides
                .forbid_public_links
                .unwrap_or(default.forbid_public_links),
            forbid_cross_drive_move: overrides
                .forbid_cross_drive_move
                .unwrap_or(default.forbid_cross_drive_move),
            forbid_owner_role_change: overrides
                .forbid_owner_role_change
                .unwrap_or(default.forbid_owner_role_change),
            include_in_photo_index: overrides
                .include_in_photo_index
                .unwrap_or(default.include_in_photo_index),
            include_in_music_index: overrides
                .include_in_music_index
                .unwrap_or(default.include_in_music_index),
            read_only: overrides.read_only.unwrap_or(default.read_only),
            // `None` here genuinely means "inherit", so `.or()` rather than
            // `.unwrap_or()`: the default's own `None` (no cap) must survive.
            max_public_link_days: overrides
                .max_public_link_days
                .or(default.max_public_link_days),
            require_public_link_password: overrides
                .require_public_link_password
                .unwrap_or(default.require_public_link_password),
        }
    }

    /// Read one knob as a JSON value, for the generic per-knob paths
    /// (comparison, drift detail). Keeps the knob list in one place instead
    /// of every caller matching on field names.
    pub fn knob_value(&self, knob: &str) -> serde_json::Value {
        use serde_json::Value;
        match knob {
            "forbid_sharing" => Value::Bool(self.forbid_sharing),
            "forbid_external_sharing" => Value::Bool(self.forbid_external_sharing),
            "forbid_public_links" => Value::Bool(self.forbid_public_links),
            "forbid_cross_drive_move" => Value::Bool(self.forbid_cross_drive_move),
            "forbid_owner_role_change" => Value::Bool(self.forbid_owner_role_change),
            "include_in_photo_index" => Value::Bool(self.include_in_photo_index),
            "include_in_music_index" => Value::Bool(self.include_in_music_index),
            "read_only" => Value::Bool(self.read_only),
            "require_public_link_password" => Value::Bool(self.require_public_link_password),
            "max_public_link_days" => match self.max_public_link_days {
                Some(d) => Value::from(d),
                None => Value::Null,
            },
            _ => Value::Null,
        }
    }

    /// Is `self` at least as strict as `other` on this one knob?
    ///
    /// Per-knob rather than a blanket comparison because the direction
    /// varies — see [`Strictness`]. The `SmallerIsStricter` arm is the one
    /// most easily written backwards: `None` means "no cap", which is LAXER
    /// than any cap, so a `None` self is at least as strict as `other` only
    /// when `other` is also `None`.
    pub fn at_least_as_strict_on(&self, other: &Self, knob: &str) -> bool {
        match knob_strictness(knob) {
            Strictness::TrueIsStricter => {
                let (a, b) = (self.knob_bool(knob), other.knob_bool(knob));
                a || !b
            }
            Strictness::FalseIsStricter => {
                let (a, b) = (self.knob_bool(knob), other.knob_bool(knob));
                !a || b
            }
            Strictness::SmallerIsStricter => {
                match (self.max_public_link_days, other.max_public_link_days) {
                    (_, None) => true,        // nothing is laxer than no cap
                    (None, Some(_)) => false, // we have no cap, they do → we are laxer
                    (Some(a), Some(b)) => a <= b,
                }
            }
        }
    }

    fn knob_bool(&self, knob: &str) -> bool {
        matches!(self.knob_value(knob), serde_json::Value::Bool(true))
    }

    /// The knobs on which `self` is LESS restrictive than `default`.
    ///
    /// Empty means compliant. Deliberately a list rather than a verdict:
    /// policies are a lattice, not a ladder — a drive can be stricter on one
    /// knob and weaker on another, so "is this drive compliant?" has no
    /// single answer worth rendering. Knobs that do not apply to the kind
    /// are skipped entirely (see [`knob_applies_to_kind`]).
    pub fn weaker_than(&self, default: &Self, kind: DriveKind) -> Vec<&'static str> {
        POLICY_KNOBS
            .iter()
            .filter(|k| knob_applies_to_kind(k, kind))
            .filter(|k| !NON_DEFAULTABLE_KNOBS.contains(*k))
            .filter(|k| !self.at_least_as_strict_on(default, k))
            .copied()
            .collect()
    }

    /// D5 `forbid_public_links` gate, used by every entry point that
    /// mints an anonymous token-share on a resource in this drive
    /// (`share_service::create_shared_link` today; future protocol
    /// surfaces — e.g. NextCloud OCS share — must call this too). The
    /// gate owns the decision + audit + canonical error so the
    /// rejection shape stays in lockstep across surfaces. See
    /// `docs/plan/drive.md` §8.
    ///
    /// Returns `Ok(())` when the policy is off; emits a
    /// `share.rejected` audit line and returns
    /// `OperationNotSupported` when on.
    pub fn refuse_public_links(&self, ctx: PublicLinkGateContext) -> Result<(), DomainError> {
        // `forbid_sharing` is the BROADER rule and covers public links too.
        // Three separate places already said so — the knob's own help text
        // ("covers public links and external sharing as well"), the
        // compliance scan, which reports existing links as violations under
        // it, and the admin editor, which greys `forbid_public_links` out as
        // "already enforced by Forbid per-resource sharing". Checking only
        // the narrow knob here made all three of those claims false: a link
        // could still be minted on a drive that forbade sharing outright,
        // while the UI told the admin it could not.
        //
        // Report the NARROWER knob when both are on, since that is the one an
        // admin would relax to permit this link — same precedence the scan
        // uses, so a refusal and the finding for an existing link name the
        // same cause.
        let (reason, message) = if self.forbid_public_links {
            (
                "forbid_public_links",
                "This drive does not allow public links.",
            )
        } else if self.forbid_sharing {
            (
                "forbid_sharing",
                "This drive does not allow sharing individual files or folders, \
                 which includes public links.",
            )
        } else {
            return Ok(());
        };
        tracing::info!(
            target: "audit",
            event = "share.rejected",
            reason = reason,
            caller_id = %ctx.caller_id,
            item_type = ctx.item_type,
            item_id = %ctx.item_id,
            "👮🏻‍♂️ public-link creation refused",
        );
        Err(DomainError::operation_not_supported("Share", message))
    }

    /// D5 `forbid_sharing` gate: refuses **per-resource** grants on
    /// resources in this drive when the policy is on. Drive-level
    /// membership stays unaffected — otherwise a drive that disables
    /// sharing would also become uneditable except by the original
    /// owner. The semantic the plan §8 commits to is "no fine-grained
    /// sharing of individual files; access happens through drive
    /// membership only."
    ///
    /// Enforced at `grant_handler::create_grant` for File / Folder
    /// resources. The Drive-resource branch of `/api/grants` and the
    /// `/api/drives/{id}/members` routes deliberately don't call this
    /// gate.
    ///
    /// Returns `Ok(())` when the policy is off; emits a
    /// `grant.rejected` audit line and returns `OperationNotSupported`
    /// when on.
    pub fn refuse_sharing(&self, ctx: SharingGateContext) -> Result<(), DomainError> {
        if !self.forbid_sharing {
            return Ok(());
        }
        tracing::info!(
            target: "audit",
            event = "grant.rejected",
            reason = "forbid_sharing",
            caller_id = %ctx.caller_id,
            resource_type = ctx.resource_type,
            resource_id = %ctx.resource_id,
            "👮🏻‍♂️ per-resource grant refused: forbid_sharing",
        );
        Err(DomainError::operation_not_supported(
            "Grant",
            "This drive does not allow per-resource sharing.",
        ))
    }

    /// D5 `forbid_owner_role_change` gate: refuses Owner-role mutations
    /// (adding a new Owner, demoting an existing Owner, or removing
    /// one) when the caller isn't OxiCloud admin and the policy is on.
    /// Membership of non-Owner roles is unaffected.
    ///
    /// Enforced at `DriveManagementService::set_member_role` (refuses
    /// Owner role writes) and `::remove_member` (refuses removing an
    /// Owner subject). Skipped when `caller_is_admin = true` — the
    /// policy exists to constrain owners, not the tenant operator.
    /// Personal drives never reach this gate because
    /// `refuse_if_personal` rejects every member mutation upstream.
    ///
    /// Returns `Ok(())` when the policy is off or the caller is admin;
    /// emits a `drive_membership.rejected` audit line and returns
    /// `OperationNotSupported` otherwise.
    pub fn refuse_owner_role_change(
        &self,
        ctx: OwnerRoleChangeGateContext,
    ) -> Result<(), DomainError> {
        if !self.forbid_owner_role_change {
            return Ok(());
        }
        if ctx.caller_is_admin {
            return Ok(());
        }
        tracing::info!(
            target: "audit",
            event = "drive_membership.rejected",
            reason = "forbid_owner_role_change",
            operation = ctx.operation,
            caller_id = %ctx.caller_id,
            drive_id = %ctx.drive_id,
            subject_type = ctx.subject_type,
            subject_id = %ctx.subject_id,
            "👮🏻‍♂️ owner-role mutation refused: forbid_owner_role_change",
        );
        Err(DomainError::operation_not_supported(
            "Drive",
            "This drive's Owner membership is locked — only OxiCloud admin can change owners.",
        ))
    }

    /// D5 `forbid_cross_drive_move` gate: refuses MOVE when
    /// `src.drive_id != dst.drive_id`. The policy lives on the SOURCE
    /// drive — its owner decides whether content can leave. Targets'
    /// owners already gate inbound moves via the `Create` permission
    /// on the destination folder, so a symmetric check would be
    /// redundant.
    ///
    /// Enforced at `file_management_service::move_file_with_perms`
    /// and `folder_service::move_folder_with_perms`. The handler
    /// doesn't see this gate — it lives in the service layer per
    /// the AuthZ architecture rule in CLAUDE.md.
    ///
    /// Returns `Ok(())` when the policy is off; emits a
    /// `move.rejected` audit line and returns `OperationNotSupported`
    /// when on.
    pub fn refuse_cross_drive_move(
        &self,
        ctx: CrossDriveMoveGateContext,
    ) -> Result<(), DomainError> {
        if !self.forbid_cross_drive_move {
            return Ok(());
        }
        tracing::info!(
            target: "audit",
            event = "move.rejected",
            reason = "forbid_cross_drive_move",
            caller_id = %ctx.caller_id,
            resource_type = ctx.resource_type,
            resource_id = %ctx.resource_id,
            src_drive_id = %ctx.src_drive_id,
            dst_drive_id = %ctx.dst_drive_id,
            "👮🏻‍♂️ cross-drive move refused: forbid_cross_drive_move",
        );
        Err(DomainError::operation_not_supported(
            "Move",
            "This drive does not allow moving content out to another drive.",
        ))
    }

    /// D5 `forbid_external_sharing` gate, shared by every entry point
    /// that creates a grant on a resource in this drive
    /// (`grant_handler::create_grant`,
    /// `DriveManagementService::set_member_role`). Each caller
    /// resolves `is_external` from whichever source naturally fits
    /// (the just-created `User` entity in the email path, a
    /// `get_user_flags` probe in the user-by-id path); the gate
    /// itself owns the decision + audit + canonical error so the
    /// shape stays in lockstep across surfaces. See `docs/plan/drive.md` §8.
    ///
    /// Returns `Ok(())` when the subject is allowed (policy off, subject
    /// is not a User, or the User is not external). Returns
    /// `OperationNotSupported` after emitting a `grant.rejected` audit
    /// line otherwise.
    pub fn refuse_external_sharing(
        &self,
        subject: Subject,
        is_external: bool,
        ctx: ExternalSharingGateContext,
    ) -> Result<(), DomainError> {
        if !self.forbid_external_sharing {
            return Ok(());
        }
        let Subject::User(uid) = subject else {
            return Ok(());
        };
        if !is_external {
            return Ok(());
        }
        tracing::info!(
            target: "audit",
            event = "grant.rejected",
            reason = "forbid_external_sharing",
            stage = ctx.stage,
            caller_id = %ctx.caller_id,
            subject_id = %uid,
            drive_id = ?ctx.drive_id,
            resource_type = ?ctx.resource_type,
            resource_id = ?ctx.resource_id,
            "👮🏻‍♂️ grant refused: forbid_external_sharing",
        );
        Err(DomainError::operation_not_supported(
            "Grant",
            "This drive does not allow external sharing.",
        ))
    }
}

/// Audit / identity context for [`DrivePolicies::refuse_owner_role_change`].
///
/// Carries the subject (the user/group whose Owner status is being
/// added, removed, or demoted) and the calling operation tag
/// (`"set_member_role"` or `"remove_member"`) so the audit log
/// pinpoints exactly which mutation the policy refused.
#[derive(Debug, Clone, Copy)]
pub struct OwnerRoleChangeGateContext {
    pub caller_id: Uuid,
    pub caller_is_admin: bool,
    pub drive_id: Uuid,
    pub operation: &'static str,
    pub subject_type: &'static str,
    pub subject_id: Uuid,
}

/// Audit / identity context for [`DrivePolicies::refuse_cross_drive_move`].
///
/// Carries the source and destination drive ids so the audit log
/// captures exactly which boundary the refused move would cross —
/// useful when investigating whether someone is probing the gate or
/// genuinely trying to organize content.
#[derive(Debug, Clone, Copy)]
pub struct CrossDriveMoveGateContext {
    pub caller_id: Uuid,
    /// `"file"` or `"folder"`.
    pub resource_type: &'static str,
    pub resource_id: Uuid,
    pub src_drive_id: Uuid,
    pub dst_drive_id: Uuid,
}

/// Audit / identity context for [`DrivePolicies::refuse_sharing`].
///
/// Only File / Folder resources reach this gate — the per-resource
/// grant surface. Drive-resource grants go through
/// `set_member_role` and aren't subject to `forbid_sharing`.
#[derive(Debug, Clone, Copy)]
pub struct SharingGateContext {
    pub caller_id: Uuid,
    /// `"file"` or `"folder"`.
    pub resource_type: &'static str,
    pub resource_id: Uuid,
}

/// Audit / identity context for [`DrivePolicies::refuse_public_links`].
///
/// Single callsite today (`share_service::create_shared_link`), but the
/// struct is the explicit contract so future surfaces (NextCloud OCS
/// share, WebDAV public-link sigil, …) land with the same shape.
#[derive(Debug, Clone, Copy)]
pub struct PublicLinkGateContext {
    pub caller_id: Uuid,
    /// `"file"` or `"folder"` — the share target's resource kind.
    pub item_type: &'static str,
    pub item_id: Uuid,
}

/// Audit / identity context for [`DrivePolicies::refuse_external_sharing`].
///
/// Two callsites with different identifiers naturally fill this in:
/// - `grant_handler` (File/Folder branch): `drive_id = None`,
///   `resource_type` + `resource_id` set
/// - `DriveManagementService::set_member_role`: `drive_id` set,
///   `resource_type` + `resource_id = None`
///
/// All three appear in the audit log so a single grep on
/// `grant.rejected reason=forbid_external_sharing` surfaces every
/// refusal regardless of entry point.
#[derive(Debug, Clone, Copy)]
pub struct ExternalSharingGateContext {
    pub caller_id: Uuid,
    /// Distinguishes the call site for log aggregators. Known values
    /// today: `"late_user"` (grant_handler), `"drive_member"`
    /// (set_member_role). New entry points pick a fresh string.
    pub stage: &'static str,
    pub drive_id: Option<Uuid>,
    pub resource_type: Option<&'static str>,
    pub resource_id: Option<Uuid>,
}

#[cfg(test)]
mod public_link_gate_tests {
    use super::*;

    fn ctx() -> PublicLinkGateContext {
        PublicLinkGateContext {
            caller_id: Uuid::nil(),
            item_type: "folder",
            item_id: Uuid::nil(),
        }
    }

    #[test]
    fn permitted_when_neither_knob_is_set() {
        assert!(DrivePolicies::default().refuse_public_links(ctx()).is_ok());
    }

    #[test]
    fn refused_by_the_narrow_knob() {
        let p = DrivePolicies {
            forbid_public_links: true,
            ..Default::default()
        };
        assert!(p.refuse_public_links(ctx()).is_err());
    }

    /// The regression this gate was missing: `forbid_sharing` is the broader
    /// rule and covers links, which the knob's help text, the compliance scan
    /// and the admin editor's "already enforced by" hint all asserted — while
    /// the gate itself let the link through.
    #[test]
    fn refused_by_forbid_sharing_alone() {
        let p = DrivePolicies {
            forbid_sharing: true,
            forbid_public_links: false,
            ..Default::default()
        };
        let err = p
            .refuse_public_links(ctx())
            .expect_err("forbid_sharing must cover public links");
        // The message has to say WHY, since the narrow knob is off and an
        // admin reading "does not allow public links" would go looking at the
        // wrong setting.
        assert!(
            format!("{err}").contains("individual files or folders"),
            "message should name the broader rule, got: {err}"
        );
    }

    #[test]
    fn narrow_knob_wins_the_reason_when_both_are_set() {
        let p = DrivePolicies {
            forbid_sharing: true,
            forbid_public_links: true,
            ..Default::default()
        };
        let err = p.refuse_public_links(ctx()).expect_err("must refuse");
        // Same precedence the scan uses, so a refusal and the finding for an
        // already-existing link name the same cause.
        assert!(
            format!("{err}").contains("does not allow public links"),
            "narrower knob should be reported, got: {err}"
        );
    }
}

#[cfg(test)]
mod policy_default_tests {
    use super::*;

    fn strict_default() -> DrivePolicies {
        DrivePolicies {
            forbid_public_links: true,
            include_in_photo_index: false,
            max_public_link_days: Some(30),
            require_public_link_password: true,
            ..Default::default()
        }
    }

    #[test]
    fn resolve_takes_the_default_when_nothing_is_overridden() {
        let got = DrivePolicies::resolve(&strict_default(), &DrivePolicyOverrides::default());
        assert_eq!(got, strict_default());
    }

    #[test]
    fn resolve_lets_an_override_win_over_the_default() {
        let overrides = DrivePolicyOverrides {
            forbid_public_links: Some(false),
            ..Default::default()
        };
        let got = DrivePolicies::resolve(&strict_default(), &overrides);
        assert!(!got.forbid_public_links, "explicit override must win");
        // Everything NOT overridden still follows the default — the whole
        // point of live inheritance.
        assert!(got.require_public_link_password);
        assert_eq!(got.max_public_link_days, Some(30));
    }

    #[test]
    fn an_absent_cap_override_inherits_rather_than_clearing_the_cap() {
        // `None` on a scalar override means "inherit", NOT "no cap". Using
        // `unwrap_or` instead of `or` would silently drop every inherited
        // cap and the drive would read as uncapped.
        let got = DrivePolicies::resolve(&strict_default(), &DrivePolicyOverrides::default());
        assert_eq!(got.max_public_link_days, Some(30));
    }

    #[test]
    fn forbid_flags_treat_true_as_stricter() {
        let strict = DrivePolicies {
            forbid_public_links: true,
            ..Default::default()
        };
        let lax = DrivePolicies {
            forbid_public_links: false,
            ..Default::default()
        };
        assert!(strict.at_least_as_strict_on(&lax, "forbid_public_links"));
        assert!(!lax.at_least_as_strict_on(&strict, "forbid_public_links"));
    }

    #[test]
    fn index_opt_ins_are_inverted_true_is_laxer() {
        // `include_in_*` are opt-INs to a global index, so `true` is MORE
        // exposure. Sharing the `forbid_*` direction would invert the
        // report for exactly these two knobs.
        let exposed = DrivePolicies {
            include_in_photo_index: true,
            ..Default::default()
        };
        let private = DrivePolicies {
            include_in_photo_index: false,
            ..Default::default()
        };
        assert!(private.at_least_as_strict_on(&exposed, "include_in_photo_index"));
        assert!(!exposed.at_least_as_strict_on(&private, "include_in_photo_index"));
    }

    #[test]
    fn no_cap_is_the_laxest_value_of_all() {
        // The comparison most likely to be written backwards.
        let uncapped = DrivePolicies {
            max_public_link_days: None,
            ..Default::default()
        };
        let capped = DrivePolicies {
            max_public_link_days: Some(30),
            ..Default::default()
        };
        let tighter = DrivePolicies {
            max_public_link_days: Some(7),
            ..Default::default()
        };

        assert!(!uncapped.at_least_as_strict_on(&capped, "max_public_link_days"));
        assert!(capped.at_least_as_strict_on(&uncapped, "max_public_link_days"));
        assert!(tighter.at_least_as_strict_on(&capped, "max_public_link_days"));
        assert!(!capped.at_least_as_strict_on(&tighter, "max_public_link_days"));
        // Equal caps are "at least as strict" — not a violation.
        assert!(capped.at_least_as_strict_on(&capped, "max_public_link_days"));
    }

    #[test]
    fn weaker_than_reports_every_violated_knob_not_a_verdict() {
        // Stricter on one knob, weaker on two others: policies are a
        // lattice, so there is no single compliant/non-compliant answer.
        let drive = DrivePolicies {
            forbid_public_links: false,   // weaker
            include_in_photo_index: true, // weaker (opt-in to exposure)
            forbid_sharing: true,         // STRICTER than the default
            max_public_link_days: Some(30),
            require_public_link_password: true,
            ..Default::default()
        };
        let weak = drive.weaker_than(&strict_default(), DriveKind::Shared);
        assert!(weak.contains(&"forbid_public_links"));
        assert!(weak.contains(&"include_in_photo_index"));
        assert!(
            !weak.contains(&"forbid_sharing"),
            "being stricter is not a violation"
        );
        assert_eq!(weak.len(), 2);
    }

    #[test]
    fn a_compliant_drive_reports_nothing() {
        assert!(
            strict_default()
                .weaker_than(&strict_default(), DriveKind::Shared)
                .is_empty()
        );
    }

    #[test]
    fn owner_role_change_is_never_compared_on_a_personal_drive() {
        // Membership is immutable on personal drives via `refuse_if_personal`,
        // which fires before any policy is read — so `false` here changes
        // nothing and must not be reported as drift.
        let default = DrivePolicies {
            forbid_owner_role_change: true,
            ..Default::default()
        };
        let drive = DrivePolicies {
            forbid_owner_role_change: false,
            ..Default::default()
        };

        assert!(drive.weaker_than(&default, DriveKind::Personal).is_empty());
        // …but it IS a real violation on a shared drive.
        assert_eq!(
            drive.weaker_than(&default, DriveKind::Shared),
            vec!["forbid_owner_role_change"]
        );
    }

    #[test]
    fn read_only_is_never_reported_as_drift() {
        // Not defaultable, so a writable drive is never "weaker" than a
        // frozen default — that finding would be noise, not a problem.
        let default = DrivePolicies {
            read_only: true,
            ..Default::default()
        };
        let drive = DrivePolicies {
            read_only: false,
            ..Default::default()
        };
        assert!(drive.weaker_than(&default, DriveKind::Shared).is_empty());
    }
}
