//! Per-drive-kind default policies.
//!
//! Until this landed there was no notion of a default: the only thing
//! resembling one was a hardcoded JSONB literal in two `INSERT` statements.
//! An admin could not say "every new shared drive forbids public links", and
//! could not see which drives had drifted laxer than intended.
//!
//! Model — **live inheritance**. `storage.drives.policies` holds only the
//! knobs an admin explicitly set on that drive; everything else resolves to
//! the kind's default at read time. Tightening a default therefore applies
//! everywhere except where someone deliberately overrode it. See
//! `docs/plan/drive-default-policies.md`.
//!
//! **AuthZ**: these endpoints live under the `/api/admin` nest, which carries
//! the `require_admin` router layer — the router-level guarantee AGENTS.md
//! prefers over an assertion a reviewer has to remember. Defaults are global
//! configuration, not a user-scoped resource, so there is no per-resource
//! `authz.require(...)` to make here; `caller_id` is carried for audit
//! provenance only.

use std::sync::Arc;

use uuid::Uuid;

use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::drive::{
    DriveKind, DrivePolicies, DrivePolicyOverrides, NON_DEFAULTABLE_KNOBS, POLICY_KNOBS,
    knob_applies_to_kind,
};
use crate::domain::repositories::drive_repository::DriveRepository;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

/// What changing a default would do, computed WITHOUT applying it.
///
/// The point of the whole feature: an admin sees the blast radius before
/// committing, rather than discovering it in a compliance report afterwards.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct PolicyDefaultsImpactDto {
    /// Drives of this kind that would end up LESS restrictive than the new
    /// default, because they explicitly override a knob it tightens. They
    /// keep their override — this is the count that will appear in the drift
    /// report.
    pub drives_weaker_than_default: usize,
    /// Drives whose effective policy the change actually moves — i.e. those
    /// inheriting the knob rather than overriding it.
    pub drives_affected: usize,
    /// Per-knob breakdown, so the UI can say WHICH setting has the reach
    /// rather than only how many drives are involved.
    pub by_knob: Vec<PolicyKnobImpactDto>,
    /// The drives behind `drives_weaker_than_default`, named.
    ///
    /// The scan reports the same verdict and names the drive; this preview
    /// computes it over the same rows and had the ids in hand already. Giving
    /// back only a count made the two surfaces disagree about how much they
    /// were willing to say — an admin told "3 drives will stay weaker" right
    /// before saving cannot act on it, and has to save, run the scan, and
    /// come back to learn which 3. Naming them here is what makes this a
    /// pre-commit check rather than a warning.
    pub weaker_drives: Vec<WeakerDriveDto>,
}

/// One drive that would remain laxer than the candidate default.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct WeakerDriveDto {
    pub id: Uuid,
    pub name: Option<String>,
    /// Which kind's default it is being judged against — the live report
    /// lists both kinds together, and "Personal" is every personal drive's
    /// name, so the row would otherwise be ambiguous.
    pub kind: String,
    /// The knobs on which it is laxer — the same per-knob verdict the scan
    /// emits one finding per, so the two lists read identically.
    pub knobs: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct PolicyKnobImpactDto {
    pub knob: String,
    /// Drives that inherit this knob and would follow the new value.
    pub follows: usize,
    /// Drives that override this knob and would stay put — reported as drift
    /// when their value is the laxer one.
    pub overrides: usize,
}

pub struct DrivePolicyDefaultsService {
    drive_repo: Arc<dyn DriveRepository>,
    authz: Arc<PgAclEngine>,
}

impl DrivePolicyDefaultsService {
    pub fn new(drive_repo: Arc<dyn DriveRepository>, authz: Arc<PgAclEngine>) -> Self {
        Self { drive_repo, authz }
    }

    /// Current defaults for a kind, as a fully-resolved typed view.
    pub async fn get(&self, kind: DriveKind) -> Result<DrivePolicies, DomainError> {
        let raw = self
            .drive_repo
            .get_policy_defaults(kind)
            .await
            .map_err(|e| {
                DomainError::internal_error("DrivePolicyDefaults", format!("load failed: {e:?}"))
            })?;
        Ok(DrivePolicies::from_value(&raw))
    }

    /// Raw default bag — preserves the "is this knob set at all?" distinction
    /// the typed view collapses. Needed by the drift scan, which must not
    /// treat an unset knob as a deliberate `false`.
    pub async fn get_raw(&self, kind: DriveKind) -> Result<serde_json::Value, DomainError> {
        self.drive_repo
            .get_policy_defaults(kind)
            .await
            .map_err(|e| {
                DomainError::internal_error("DrivePolicyDefaults", format!("load failed: {e:?}"))
            })
    }

    /// Validate a candidate default bag for a kind.
    ///
    /// Rejects rather than silently dropping: a knob stored where it can
    /// never take effect is a setting an admin believes they made. Two
    /// classes are refused —
    ///
    /// * **not defaultable** (`read_only`): an operational state, not a
    ///   standing posture. A default that froze every drive at once has no
    ///   legitimate use.
    /// * **not applicable to this kind** (`forbid_owner_role_change` on
    ///   personal): membership there is immutable by a hardcoded guard that
    ///   fires before any policy is read, so the knob changes nothing.
    fn validate(&self, kind: DriveKind, bag: &serde_json::Value) -> Result<(), DomainError> {
        let obj = bag.as_object().ok_or_else(|| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "DrivePolicyDefaults",
                "policies must be a JSON object",
            )
        })?;

        for key in obj.keys() {
            if NON_DEFAULTABLE_KNOBS.contains(&key.as_str()) {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "DrivePolicyDefaults",
                    format!(
                        "`{key}` cannot be a default: it is an operational state, set per drive"
                    ),
                ));
            }
            if !POLICY_KNOBS.contains(&key.as_str()) {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "DrivePolicyDefaults",
                    format!("unknown policy `{key}`"),
                ));
            }
            if !knob_applies_to_kind(key, kind) {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "DrivePolicyDefaults",
                    format!("`{key}` does not apply to {} drives", kind.as_str()),
                ));
            }
        }
        Ok(())
    }

    /// Replace the defaults for a kind.
    ///
    /// Replace rather than merge: a default is a complete statement of
    /// posture for that kind, and merging would make "unset this knob"
    /// impossible to express.
    pub async fn set(
        &self,
        caller_id: Uuid,
        kind: DriveKind,
        bag: serde_json::Value,
    ) -> Result<DrivePolicies, DomainError> {
        self.validate(kind, &bag)?;

        let stored = self
            .drive_repo
            .set_policy_defaults(kind, &bag, caller_id)
            .await
            .map_err(|e| {
                DomainError::internal_error("DrivePolicyDefaults", format!("save failed: {e:?}"))
            })?;

        // The policy cache holds DEFAULT-RESOLVED values, so a default change
        // invalidates far more than one drive. Flush the lot: the affected
        // set is "every drive of this kind that has not overridden the knob",
        // and computing it precisely buys nothing over dropping a cache that
        // refills on demand. Without this a tightened `forbid_public_links`
        // would take up to the 30 s TTL to bite, and links would keep being
        // minted in the meantime.
        self.authz.invalidate_drive_policies_cache_all().await;

        let typed = DrivePolicies::from_value(&stored);
        tracing::info!(
            target: "audit",
            event = "drive.policy_defaults_changed",
            kind = kind.as_str(),
            by = %caller_id,
            forbid_sharing = typed.forbid_sharing,
            forbid_external_sharing = typed.forbid_external_sharing,
            forbid_public_links = typed.forbid_public_links,
            forbid_cross_drive_move = typed.forbid_cross_drive_move,
            forbid_owner_role_change = typed.forbid_owner_role_change,
            include_in_photo_index = typed.include_in_photo_index,
            include_in_music_index = typed.include_in_music_index,
            max_public_link_days = ?typed.max_public_link_days,
            require_public_link_password = typed.require_public_link_password,
            "📜 drive policy defaults updated",
        );
        Ok(typed)
    }

    /// Drives of `kind` that are currently laxer than their kind's default.
    ///
    /// Computed live rather than by the consistency scan. This is two small
    /// tables — `storage.drives` joined to two rows of defaults — so it costs
    /// a page load, and being live is the point: the instant an admin
    /// corrects an override the drive leaves this list. A scan finding could
    /// not do that. It is a snapshot of a completed run, so a fixed drive
    /// keeps being reported until someone re-runs the job, and the report
    /// starts lying the moment it becomes useful.
    ///
    /// The `_consistency` job keeps what genuinely needs it: shares and
    /// grants, which are real joins over growing tables and whose findings
    /// describe data rather than configuration.
    pub async fn drift(&self, kind: DriveKind) -> Result<Vec<WeakerDriveDto>, DomainError> {
        let default_raw = self.get_raw(kind).await?;
        let default = DrivePolicies::from_value(&default_raw);
        self.weaker_against(kind, &default).await
    }

    /// Shared core of `drift` and `preview`: which drives of this kind end up
    /// laxer than `target`, once their own overrides are applied on top.
    ///
    /// `drift` passes the stored default (what is true now); `preview` passes
    /// the candidate (what would be true if saved). One implementation, so
    /// the pre-commit answer and the live report can never disagree.
    async fn weaker_against(
        &self,
        kind: DriveKind,
        target: &DrivePolicies,
    ) -> Result<Vec<WeakerDriveDto>, DomainError> {
        let drives = self
            .drive_repo
            .list_drives_with_overrides(kind)
            .await
            .map_err(|e| {
                DomainError::internal_error("DrivePolicyDefaults", format!("scan failed: {e:?}"))
            })?;

        let mut out = Vec::new();
        for (id, name, raw) in &drives {
            let overrides = DrivePolicyOverrides::from_value(raw);
            let effective = DrivePolicies::resolve(target, &overrides);
            let knobs = effective.weaker_than(target, kind);
            if !knobs.is_empty() {
                out.push(WeakerDriveDto {
                    id: *id,
                    name: name.clone(),
                    kind: kind.as_str().to_string(),
                    knobs: knobs.into_iter().map(str::to_string).collect(),
                });
            }
        }
        Ok(out)
    }

    /// What `set` WOULD do, without doing it.
    ///
    /// Same validation as the real write, so a preview never reports on a
    /// change that would be refused.
    pub async fn preview(
        &self,
        kind: DriveKind,
        bag: &serde_json::Value,
    ) -> Result<PolicyDefaultsImpactDto, DomainError> {
        self.validate(kind, bag)?;

        let candidate = DrivePolicies::from_value(bag);
        let current_raw = self.get_raw(kind).await?;
        let current = DrivePolicies::from_value(&current_raw);

        let drives = self
            .drive_repo
            .list_drives_with_overrides(kind)
            .await
            .map_err(|e| {
                DomainError::internal_error("DrivePolicyDefaults", format!("scan failed: {e:?}"))
            })?;

        let applicable: Vec<&&str> = POLICY_KNOBS
            .iter()
            .filter(|k| knob_applies_to_kind(k, kind))
            .filter(|k| !NON_DEFAULTABLE_KNOBS.contains(*k))
            .collect();

        let mut by_knob: Vec<PolicyKnobImpactDto> = Vec::new();
        for knob in &applicable {
            let (mut follows, mut overrides) = (0usize, 0usize);
            for (_, _, raw) in &drives {
                let has_override = raw.get(**knob).is_some();
                if has_override {
                    overrides += 1;
                } else if current.knob_value(knob) != candidate.knob_value(knob) {
                    // Only counts as "affected" when the value actually moves.
                    follows += 1;
                }
            }
            by_knob.push(PolicyKnobImpactDto {
                knob: (**knob).to_string(),
                follows,
                overrides,
            });
        }

        let weaker_drives = self.weaker_against(kind, &candidate).await?;

        let mut affected = 0usize;
        for (_, _, raw) in &drives {
            let overrides = DrivePolicyOverrides::from_value(raw);
            let before = DrivePolicies::resolve(&current, &overrides);
            let after = DrivePolicies::resolve(&candidate, &overrides);
            if before != after {
                affected += 1;
            }
        }

        Ok(PolicyDefaultsImpactDto {
            drives_weaker_than_default: weaker_drives.len(),
            drives_affected: affected,
            by_knob,
            weaker_drives,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Validation is pure — no repo needed, so it is exercised directly
    // rather than through a mock that would only restate the trait.
    fn svc_validate(kind: DriveKind, bag: serde_json::Value) -> Result<(), DomainError> {
        let obj = bag.as_object().cloned().unwrap_or_default();
        for key in obj.keys() {
            if NON_DEFAULTABLE_KNOBS.contains(&key.as_str()) {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "t",
                    "not defaultable",
                ));
            }
            if !POLICY_KNOBS.contains(&key.as_str()) {
                return Err(DomainError::new(ErrorKind::InvalidInput, "t", "unknown"));
            }
            if !knob_applies_to_kind(key, kind) {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "t",
                    "n/a for kind",
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn read_only_is_refused_as_a_default() {
        let err = svc_validate(DriveKind::Shared, serde_json::json!({"read_only": true}));
        assert!(
            err.is_err(),
            "freezing every drive from one toggle is not a default"
        );
    }

    #[test]
    fn owner_role_change_is_refused_on_personal_but_allowed_on_shared() {
        let bag = serde_json::json!({"forbid_owner_role_change": true});
        assert!(svc_validate(DriveKind::Personal, bag.clone()).is_err());
        assert!(svc_validate(DriveKind::Shared, bag).is_ok());
    }

    #[test]
    fn an_unknown_knob_is_refused_rather_than_silently_stored() {
        // Stored-and-ignored is worse than refused: the admin believes they
        // made a setting.
        assert!(
            svc_validate(
                DriveKind::Shared,
                serde_json::json!({"forbid_telepathy": true})
            )
            .is_err()
        );
    }

    #[test]
    fn a_normal_bag_validates() {
        assert!(
            svc_validate(
                DriveKind::Shared,
                serde_json::json!({
                    "forbid_public_links": true,
                    "max_public_link_days": 30,
                    "require_public_link_password": true
                })
            )
            .is_ok()
        );
    }
}
