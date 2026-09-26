//! Drive-policy compliance scan — discovery by default, per-drive repair on
//! request.
//!
//! Answers the question an admin cannot otherwise ask, and which arises the
//! moment a default is tightened: **which existing shares violate their
//! drive's policy?** Policy enforcement is creation-time only — tightening
//! `forbid_public_links` does nothing to links already minted. That gap is
//! the point of this job.
//!
//! "Shares" here means BOTH anonymous links and grants to users and groups.
//! `storage.shares` holds only the former, so a scan reading it alone would
//! report "clean" while every file shared with a colleague contravened a
//! freshly-enabled `forbid_sharing` — silent about the most common kind of
//! share there is.
//!
//! **What this job deliberately does NOT do: drift.** "Which drives are
//! laxer than their kind's default" used to be a third pass here, and it was
//! the wrong home for it. It reads two small tables and needs no joins, so
//! `DrivePolicyDefaultsService::drift` computes it live on page load —
//! which also makes it CORRECT: a scan finding is a snapshot of a completed
//! run, so a drive stayed on the report after the admin had fixed it, until
//! somebody re-ran the job. The live view empties as the overrides are
//! corrected. It is also not the kind of thing this family is for: a
//! `_consistency` job reports on data, and drift is configuration posture —
//! a decision to revisit, not an integrity problem.
//!
//! **Read-only by default; deletes only under `repair=true`.**
//! Retroactively revoking links people are using is not something a policy
//! save — or a background sweep — should do on its own, so the default run
//! only reports. `repair` is the separate explicit opt-in the consistency
//! family is built around.
//!
//! Repair additionally **requires `drive`**, and omitting it REFUSES rather
//! than defaulting to everything. The admin panel puts a repair checkbox
//! beside a Run button, so a default of "all drives" would make one misclick
//! revoke every non-compliant share on the deployment — and there is no undo,
//! the grants are gone.
//!
//! `drive=*` is how the whole estate is cleaned deliberately. The wildcard
//! has to be typed, which is the property that matters: it cannot be reached
//! by leaving a field blank. A single drive is named by id, or by a name that
//! matches exactly one.
//!
//! Under repair each share is deleted ONCE, before its findings are written,
//! so every finding states whether the access it describes still exists
//! (`detail.removed`). One link can breach three knobs at the same time;
//! deleting per finding would count three revocations for one link.
//!
//! The underlying queries are single statements over small tables, so the
//! job exists for SURFACING rather than performance: `jobs.run_findings` plus
//! the admin findings drawer is the only durable, drillable per-resource list
//! an admin has. Batching exists to keep the cancel poll responsive, not
//! because the data is large.
//!
//! See `docs/plan/drive-default-policies.md`.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::domain::entities::drive::{DriveKind, DrivePolicies, DrivePolicyOverrides};
use crate::infrastructure::scheduler::{
    JobParam, JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, RecoverableJobHandler,
    RunOutcome, RunStatus, record_or_log,
};

pub const DRIVE_POLICIES_CONSISTENCY_JOB_NAME: &str = "drive_policies_consistency";

/// Drives per batch. Drives are few (dozens per install), so this only sets
/// the cancel-poll cadence.
const BATCH_SIZE: i64 = 100;

/// The `drive` value meaning "every drive".
///
/// Repair refuses to run without `drive`, so this is the only way to clean
/// the whole estate. A wildcard has to be typed — which is the point: it
/// cannot be reached by leaving the field blank and clicking Run.
const ALL_DRIVES: &str = "*";

/// What a run was pointed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriveScope {
    /// Every drive — only from an explicit `drive=*`.
    All,
    One(Uuid),
}

pub struct DrivePoliciesConsistencyCheck {
    pool: Arc<PgPool>,
}

impl DrivePoliciesConsistencyCheck {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    pub async fn register_recoverable_job(
        self: Arc<Self>,
        registry: &JobRegistry,
        provider: &Arc<dyn JobStoreProvider>,
    ) -> Arc<Self> {
        registry
            .register_recoverable_job(self.clone(), provider.clone(), None)
            .await;
        self
    }

    /// Resolve the `drive` parameter to exactly one drive id.
    ///
    /// Accepts an id, or a name — but a name only when it identifies ONE
    /// drive. Names are not unique here: every personal drive is called
    /// "Personal", and two shared drives may share a name. Since the caller
    /// may be about to delete that drive's shares, an ambiguous name is
    /// refused with the candidate ids rather than resolved to whichever row
    /// sorted first.
    ///
    /// Returns the operator-facing message on failure — this runs before any
    /// work, so failing the run with an explanation is the whole handling.
    async fn resolve_drive(&self, raw: &str) -> Result<Uuid, String> {
        if let Ok(id) = Uuid::parse_str(raw) {
            let exists: Result<(i64,), sqlx::Error> =
                sqlx::query_as("SELECT COUNT(*) FROM storage.drives WHERE id = $1")
                    .bind(id)
                    .fetch_one(self.pool.as_ref())
                    .await;
            return match exists {
                Ok((1,)) => Ok(id),
                Ok(_) => Err(format!("drive `{raw}` does not exist")),
                Err(e) => Err(format!("resolving drive `{raw}`: {e}")),
            };
        }

        // The name lives on the root folder; `storage.drives` has none.
        let rows = sqlx::query(
            "SELECT d.id FROM storage.drives d \
               JOIN storage.folders fo ON fo.id = d.root_folder_id \
              WHERE fo.name = $1",
        )
        .bind(raw)
        .fetch_all(self.pool.as_ref())
        .await;

        let rows = match rows {
            Ok(r) => r,
            Err(e) => return Err(format!("resolving drive `{raw}`: {e}")),
        };
        let ids: Vec<Uuid> = rows.iter().filter_map(|r| r.try_get("id").ok()).collect();
        match ids.len() {
            1 => Ok(ids[0]),
            0 => Err(format!(
                "no drive is named `{raw}` — pass a drive id, or check the name"
            )),
            n => Err(format!(
                "`{raw}` names {n} drives, so it is ambiguous — pass one of these ids instead: {}",
                ids.iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Delete one anonymous link and the token grant that carries its access.
    ///
    /// Both rows, in one transaction: the `storage.shares` row is the link's
    /// identity and the `role_grants` row is what actually permits the read.
    /// Removing only the first would leave a grant with nothing pointing at
    /// it — access still live, and invisible to this scan on the next run
    /// because the scan joins through `shares`.
    async fn delete_link(&self, share_id: Uuid) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM storage.role_grants WHERE subject_type = 'token' AND subject_id = $1",
        )
        .bind(share_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM storage.shares WHERE id = $1")
            .bind(share_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    /// Delete one user or group grant.
    async fn delete_grant(&self, grant_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM storage.role_grants WHERE id = $1")
            .bind(grant_id)
            .execute(self.pool.as_ref())
            .await
            .map(|_| ())
    }

    /// The default bag for each kind, read once per run rather than per
    /// drive. Defaults change about as often as an admin opens a settings
    /// page, so a snapshot for the duration of a scan is fine — and a run
    /// that mixed two generations of default would produce findings that
    /// contradict each other.
    async fn load_defaults(&self) -> Result<(DrivePolicies, DrivePolicies), sqlx::Error> {
        let rows = sqlx::query("SELECT kind, policies FROM storage.drive_policy_defaults")
            .fetch_all(self.pool.as_ref())
            .await?;
        let (mut personal, mut shared) = (DrivePolicies::default(), DrivePolicies::default());
        for row in rows {
            let kind: String = row.try_get("kind")?;
            let bag: serde_json::Value = row.try_get("policies")?;
            match kind.as_str() {
                "personal" => personal = DrivePolicies::from_value(&bag),
                "shared" => shared = DrivePolicies::from_value(&bag),
                _ => {}
            }
        }
        Ok((personal, shared))
    }
}

#[async_trait]
impl RecoverableJobHandler for DrivePoliciesConsistencyCheck {
    fn name(&self) -> &str {
        DRIVE_POLICIES_CONSISTENCY_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Reports public links AND user/group grants that violate the policy \
         of the drive they live in. Read-only by default — tightening a \
         policy never revokes access that already exists, so this is how it \
         becomes visible. With `repair=true` and a `drive`, deletes what it \
         reports on that one drive."
    }

    /// Read-only unless `repair` is passed. See [`Self::repair_description`].
    fn mutates(&self) -> Mutates {
        Mutates::OnRepairOnly
    }

    fn repair_description(&self) -> Option<&'static str> {
        Some(
            "Deletes the reported public links and user/group grants, revoking \
             the access they carry. There is no undo. Requires `drive`: a \
             single drive, or `*` to mean every drive — omitting it refuses, \
             so a misclick cannot clean the whole estate.",
        )
    }

    fn parameters(&self) -> &'static [JobParam] {
        const PARAMS: &[JobParam] = &[
            JobParam::boolean(
                "repair",
                false,
                "Delete what the scan reports instead of only listing it. Revokes access.",
            ),
            // Required for repair, optional for a plain scan (where it simply
            // narrows the report).
            //
            // A drive id, a name that matches exactly one drive, or `*` for
            // every drive. Names are accepted for convenience but are NOT
            // unique — every personal drive is called "Personal" — so an
            // ambiguous name is refused with the candidates rather than
            // resolved to a guess. Deleting the wrong drive's shares is not
            // something to be clever about.
            JobParam::string(
                "drive",
                "One drive (id, or a name matching exactly one), or `*` for all. Required with repair=true.",
            ),
        ];
        PARAMS
    }

    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> =
            sqlx::query_as("SELECT COUNT(*) FROM storage.drives")
                .fetch_one(self.pool.as_ref())
                .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::consistency",
                    event = "drive_policies_consistency.count_total_failed",
                    error = %e,
                    "count_total failed — run will not surface a progress bar"
                );
                None
            }
        }
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        let repair = args.get_bool("repair");

        // Resolve `drive` before anything else: under repair it decides
        // whether the run may proceed at all.
        //
        // `*` is every drive, spelled out. It exists so that "clean the whole
        // estate" remains possible while staying impossible to reach by
        // accident — see the refusal below.
        let scope: Option<DriveScope> = match args.get_str("drive") {
            None => None,
            Some(ALL_DRIVES) => Some(DriveScope::All),
            Some(raw) => match self.resolve_drive(raw).await {
                Ok(id) => Some(DriveScope::One(id)),
                Err(message) => return RunOutcome::Failed { message },
            },
        };

        // Repair never defaults to the whole deployment. Omitting `drive`
        // refuses outright rather than falling back to "all", because the
        // admin panel puts a repair checkbox next to a Run button: one
        // misclick would otherwise revoke every non-compliant share on the
        // instance, and there is no undo — the grants are gone.
        //
        // `*` is the deliberate form. A wildcard has to be typed, which is
        // the property that matters; it cannot be arrived at by leaving a
        // field blank.
        if repair && scope.is_none() {
            return RunOutcome::Failed {
                message: format!(
                    "drive_policies_consistency: `repair=true` requires `drive` — repair \
                     deletes shares and there is no undo. Pass a drive id or name to clean \
                     one drive, or `{ALL_DRIVES}` to clean every drive."
                ),
            };
        }

        let drive_filter: Option<Uuid> = match scope {
            Some(DriveScope::One(id)) => Some(id),
            // Both "not specified" (a plain scan) and `*` read everything.
            Some(DriveScope::All) | None => None,
        };

        if repair {
            tracing::info!(
                target: "audit",
                event = "drive_policy.repair_started",
                // The blast radius, named: an operator reading this later has
                // to be able to tell a one-drive clean-up from an estate-wide
                // one without reconstructing the query string.
                scope = match scope {
                    Some(DriveScope::All) => "all_drives",
                    _ => "one_drive",
                },
                run_id = %store.run_id(),
                drive_id = ?drive_filter,
                "👮🏻‍♂️ drive policy repair: non-compliant links and grants in scope will be DELETED",
            );
        }

        let mut cursor: Option<Uuid> = match resume_cursor {
            None => None,
            Some(bytes) if bytes.is_empty() => None,
            Some(bytes) if bytes.len() == 16 => {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&bytes);
                Some(Uuid::from_bytes(arr))
            }
            Some(bytes) => {
                return RunOutcome::Failed {
                    message: format!("invalid cursor: expected 16 bytes, got {}", bytes.len()),
                };
            }
        };

        let (personal_default, shared_default) = match self.load_defaults().await {
            Ok(d) => d,
            Err(e) => {
                return RunOutcome::Failed {
                    message: format!("load defaults: {e}"),
                };
            }
        };

        let mut share_findings = 0u64;
        let mut grant_findings = 0u64;
        // Only non-zero under repair. Reported separately from the finding
        // counts because "found 12, removed 12" and "found 12, removed 0" are
        // different outcomes and an admin must be able to tell them apart.
        let mut links_removed = 0u64;
        let mut grants_removed = 0u64;

        loop {
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    return RunOutcome::Paused {
                        cursor: cursor.map(|u| u.as_bytes().to_vec()).unwrap_or_default(),
                    };
                }
                Ok(_) => {}
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("status poll: {e}"),
                    };
                }
            }

            // Drive rows plus their display name (which lives on the root
            // folder — `storage.drives` has no name column).
            let rows = match sqlx::query(
                "SELECT d.id, d.kind, d.policies, fo.name \
                   FROM storage.drives d \
                   LEFT JOIN storage.folders fo ON fo.id = d.root_folder_id \
                  WHERE ($1::uuid IS NULL OR d.id > $1) \
                    AND ($3::uuid IS NULL OR d.id = $3) \
                  ORDER BY d.id \
                  LIMIT $2",
            )
            .bind(cursor)
            .bind(BATCH_SIZE)
            .bind(drive_filter)
            .fetch_all(self.pool.as_ref())
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("drive batch: {e}"),
                    };
                }
            };

            if rows.is_empty() {
                break;
            }

            for row in &rows {
                let drive_id: Uuid = match row.try_get("id") {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let kind_str: String = row.try_get("kind").unwrap_or_default();
                let Some(kind) = DriveKind::parse(&kind_str) else {
                    continue;
                };
                let name: Option<String> = row.try_get("name").ok();
                let bag: serde_json::Value = row
                    .try_get("policies")
                    .unwrap_or_else(|_| serde_json::json!({}));

                let default = match kind {
                    DriveKind::Personal => &personal_default,
                    DriveKind::Shared => &shared_default,
                };
                let overrides = DrivePolicyOverrides::from_value(&bag);
                let effective = DrivePolicies::resolve(default, &overrides);

                // ── 1. Public links that violate this drive's policy ────
                //
                // `shares.item_id` is TEXT holding a UUID, so the cast is
                // mandatory; the regex guard keeps a malformed value from
                // raising and killing the whole batch. There is no FK from
                // shares to files/folders, so a dangling share resolves to
                // no drive and simply does not appear here.
                let shares = match sqlx::query(
                    "SELECT s.id, s.item_name, s.item_type, \
                            (s.password_hash IS NOT NULL) AS has_password, \
                            g.expires_at \
                       FROM storage.shares s \
                       LEFT JOIN storage.files   f  ON s.item_type = 'file' \
                             AND s.item_id ~ '^[0-9a-fA-F-]{36}$' AND f.id  = s.item_id::uuid \
                       LEFT JOIN storage.folders fo ON s.item_type = 'folder' \
                             AND s.item_id ~ '^[0-9a-fA-F-]{36}$' AND fo.id = s.item_id::uuid \
                       LEFT JOIN storage.role_grants g \
                              ON g.subject_type = 'token' AND g.subject_id = s.id \
                      WHERE COALESCE(f.drive_id, fo.drive_id) = $1",
                )
                .bind(drive_id)
                .fetch_all(self.pool.as_ref())
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        return RunOutcome::Failed {
                            message: format!("share scan for drive {drive_id}: {e}"),
                        };
                    }
                };

                for s in &shares {
                    let share_id: Uuid = match s.try_get("id") {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let token_name: Option<String> = s.try_get("item_name").ok();
                    let item_type: String = s.try_get("item_type").unwrap_or_default();
                    let has_password: bool = s.try_get("has_password").unwrap_or(false);
                    let expires_at: Option<chrono::DateTime<chrono::Utc>> =
                        s.try_get("expires_at").ok().flatten();

                    // Under repair the link is deleted ONCE, before its
                    // findings are recorded, so each finding can state
                    // truthfully whether the thing it describes still exists.
                    //
                    // Once per link, not once per finding: one link can breach
                    // three knobs at the same time (forbidden, passwordless,
                    // and beyond the cap), and deleting it three times would
                    // count three removals for one revocation.
                    // Each condition evaluated ONCE and reused below, rather
                    // than re-tested at its finding site. The cap comparison
                    // reads the clock, so testing it twice could land either
                    // side of the boundary and have repair delete a link the
                    // report never mentioned.
                    let forbidden = effective.forbid_public_links || effective.forbid_sharing;
                    let needs_password = effective.require_public_link_password && !has_password;
                    let over_cap = effective.max_public_link_days.map(|cap| {
                        // A null expiry is never-expires — the laxest value
                        // there is, and the finding most worth surfacing.
                        // Reported explicitly rather than skipped.
                        let over = match expires_at {
                            None => true,
                            Some(exp) => {
                                exp > chrono::Utc::now() + chrono::Duration::days(cap as i64)
                            }
                        };
                        (cap, over)
                    });

                    let violates =
                        forbidden || needs_password || matches!(over_cap, Some((_, true)));

                    let mut removed = false;
                    if repair && violates {
                        if let Err(e) = self.delete_link(share_id).await {
                            // A failed delete means the link is still live.
                            // Failing the run is the point: a repair that
                            // silently skipped one and reported success would
                            // leave the admin believing the drive was clean.
                            return RunOutcome::Failed {
                                message: format!("repair: deleting link {share_id}: {e}"),
                            };
                        }
                        links_removed += 1;
                        removed = true;
                        tracing::info!(
                            target: "audit",
                            event = "drive_policy.share_revoked",
                            reason = "violates_drive_policy",
                            run_id = %store.run_id(),
                            drive_id = %drive_id,
                            share_id = %share_id,
                            item_type = %item_type,
                            "👮🏻‍♂️ public link deleted by drive-policy repair",
                        );
                    }

                    // The link exists at all, on a drive that now forbids
                    // public links — or forbids per-resource sharing
                    // outright, which covers links too. Report the narrower
                    // knob when both apply, since that is the one an admin
                    // would relax to permit this link.
                    if forbidden {
                        let knob = if effective.forbid_public_links {
                            "forbid_public_links"
                        } else {
                            "forbid_sharing"
                        };
                        share_findings += 1;
                        record_or_log(
                            store,
                            DRIVE_POLICIES_CONSISTENCY_JOB_NAME,
                            "share_violates_drive_policy",
                            "anomaly",
                            Some(share_id),
                            serde_json::json!({
                                "drive_id":   drive_id,
                                "drive_name": name,
                                "knob":       knob,
                                "token_name": token_name,
                                "item_type":  item_type,
                                // Whether repair deleted it in this same run,
                                // so a finding never describes a link that is
                                // already gone as though it were still live.
                                "removed":    removed,
                            }),
                        )
                        .await;
                    }

                    if needs_password {
                        share_findings += 1;
                        record_or_log(
                            store,
                            DRIVE_POLICIES_CONSISTENCY_JOB_NAME,
                            "share_missing_required_password",
                            "anomaly",
                            Some(share_id),
                            serde_json::json!({
                                "drive_id":   drive_id,
                                "drive_name": name,
                                "token_name": token_name,
                                "item_type":  item_type,
                                "removed":    removed,
                            }),
                        )
                        .await;
                    }

                    if let Some((cap_days, over)) = over_cap
                        && over
                    {
                        share_findings += 1;
                        record_or_log(
                            store,
                            DRIVE_POLICIES_CONSISTENCY_JOB_NAME,
                            "share_outlives_policy_cap",
                            "anomaly",
                            Some(share_id),
                            serde_json::json!({
                                "drive_id":   drive_id,
                                "drive_name": name,
                                "cap_days":   cap_days,
                                "expires_at": expires_at,
                                "never_expires": expires_at.is_none(),
                                "token_name": token_name,
                                // Every other finding in this job carries
                                // `item_type`, and a report that names the
                                // resource kind on some rows but not others
                                // reads as a rendering bug — the same folder
                                // appeared once as "photos (folder)" and once
                                // as bare "photos".
                                "item_type":  item_type,
                                "removed":    removed,
                            }),
                        )
                        .await;
                    }
                }

                // ── 2. Grants to users and groups ──────────────────────
                //
                // `storage.shares` is EXCLUSIVELY the anonymous-link
                // table, so the pass above cannot see a file shared with
                // a colleague. Without this block, turning on
                // `forbid_sharing` would leave every existing per-resource
                // grant contravening it and the report would say "clean" —
                // silent about the most common kind of share there is.
                //
                // Drive-level grants are deliberately excluded:
                // `resource_type = 'drive'` is MEMBERSHIP, which
                // `forbid_sharing` does not govern (the grant handler
                // skips Drive resources for exactly this reason). Reporting
                // them would flag every member of every drive.
                //
                // Expired grants are filtered out — they already grant
                // nothing, so listing them is noise.
                if effective.forbid_sharing || effective.forbid_external_sharing {
                    let grants = match sqlx::query(
                        "SELECT g.id, g.subject_type, g.subject_id, g.resource_type, \
                                g.role::text AS role, \
                                COALESCE(u.is_external, false) AS subject_is_external, \
                                u.username \
                           FROM storage.role_grants g \
                           LEFT JOIN storage.files   f  ON g.resource_type = 'file' \
                                 AND f.id  = g.resource_id \
                           LEFT JOIN storage.folders fo ON g.resource_type = 'folder' \
                                 AND fo.id = g.resource_id \
                           LEFT JOIN auth.users u ON g.subject_type = 'user' \
                                 AND u.id = g.subject_id \
                          WHERE g.subject_type IN ('user', 'group') \
                            AND g.resource_type IN ('file', 'folder') \
                            AND COALESCE(f.drive_id, fo.drive_id) = $1 \
                            AND (g.expires_at IS NULL OR g.expires_at > NOW())",
                    )
                    .bind(drive_id)
                    .fetch_all(self.pool.as_ref())
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            return RunOutcome::Failed {
                                message: format!("grant scan for drive {drive_id}: {e}"),
                            };
                        }
                    };

                    for g in &grants {
                        let grant_id: Uuid = match g.try_get("id") {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let subject_type: String = g.try_get("subject_type").unwrap_or_default();
                        let subject_id: Option<Uuid> = g.try_get("subject_id").ok();
                        let resource_type: String = g.try_get("resource_type").unwrap_or_default();
                        let role: String = g.try_get("role").unwrap_or_default();
                        let is_external: bool = g.try_get("subject_is_external").unwrap_or(false);
                        let username: Option<String> = g.try_get("username").ok().flatten();

                        // `forbid_sharing` is the broader rule and covers
                        // the external case, so report the narrower knob
                        // only when sharing itself is still permitted —
                        // otherwise one grant would raise two findings
                        // describing the same problem.
                        let knob = if effective.forbid_sharing {
                            Some("forbid_sharing")
                        } else if is_external {
                            Some("forbid_external_sharing")
                        } else {
                            None
                        };

                        if let Some(knob) = knob {
                            // Deleted before the finding is written, so the
                            // finding can say whether the access it describes
                            // still exists. Same reasoning as for links.
                            let mut removed = false;
                            if repair {
                                if let Err(e) = self.delete_grant(grant_id).await {
                                    return RunOutcome::Failed {
                                        message: format!("repair: deleting grant {grant_id}: {e}"),
                                    };
                                }
                                grants_removed += 1;
                                removed = true;
                                tracing::info!(
                                    target: "audit",
                                    event = "drive_policy.grant_revoked",
                                    reason = knob,
                                    run_id = %store.run_id(),
                                    drive_id = %drive_id,
                                    grant_id = %grant_id,
                                    subject_type = %subject_type,
                                    subject_id = ?subject_id,
                                    is_external = is_external,
                                    "👮🏻‍♂️ grant deleted by drive-policy repair",
                                );
                            }

                            grant_findings += 1;
                            record_or_log(
                                store,
                                DRIVE_POLICIES_CONSISTENCY_JOB_NAME,
                                "grant_violates_drive_policy",
                                "anomaly",
                                Some(grant_id),
                                serde_json::json!({
                                    "drive_id":      drive_id,
                                    "drive_name":    name,
                                    "knob":          knob,
                                    "subject_type":  subject_type,
                                    "subject_id":    subject_id,
                                    "username":      username,
                                    "is_external":   is_external,
                                    "resource_type": resource_type,
                                    "role":          role,
                                    "removed":       removed,
                                }),
                            )
                            .await;
                        }
                    }
                }
            }

            let last_id: Uuid = match rows.last().and_then(|r| r.try_get("id").ok()) {
                Some(v) => v,
                None => break,
            };
            cursor = Some(last_id);
            let batch_len = rows.len() as u64;
            if let Err(e) = store
                .checkpoint(last_id.as_bytes().to_vec(), batch_len)
                .await
            {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }

            if (rows.len() as i64) < BATCH_SIZE {
                break;
            }
        }

        tracing::info!(
            target: "oxicloud::consistency",
            event = "drive_policies_consistency.completed",
            run_id = %store.run_id(),
            share_findings = share_findings,
            grant_findings = grant_findings,
            "drive_policies_consistency completed",
        );

        let mut extra_stats = serde_json::Map::new();
        extra_stats.insert(
            "shares_violating_policy".into(),
            serde_json::json!(share_findings),
        );
        // Counted separately from links: a public link and a colleague's
        // access are different problems with different remedies, and an
        // admin reading one number would not know which they were looking
        // at.
        extra_stats.insert(
            "grants_violating_policy".into(),
            serde_json::json!(grant_findings),
        );
        // Always present, even as zeroes on a plain scan: an admin reading a
        // past run needs to know whether it removed anything, and an absent
        // key is ambiguous between "removed none" and "this run could not".
        extra_stats.insert("links_removed".into(), serde_json::json!(links_removed));
        extra_stats.insert("grants_removed".into(), serde_json::json!(grants_removed));
        RunOutcome::Completed { extra_stats }
    }
}
