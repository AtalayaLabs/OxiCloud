-- =============================================================
-- storage.caller_accessible_drives(caller_id uuid, policy_flag text)
-- =============================================================
-- Returns every drive the caller can see via a direct `user` grant
-- or a group-mediated grant (direct + transitive, expanded through
-- `storage.caller_group_ids`), optionally restricted to drives whose
-- `effective_policies` flip a named boolean flag on.
--
-- §5 of `docs/plan/photos-resources-migration.md`. Replaces the
-- hand-rolled `WITH accessible AS (…)` CTEs used by every drive-
-- enumeration listing. The function has two load-bearing properties
-- the inline shape kept forgetting:
--
--   1. ONE row per drive, no matter how many `role_grants` rows the
--      caller reaches it through. The earlier `JOIN role_grants`
--      shape fanned a drive out to one row per matching grant — a
--      caller with BOTH a direct `user` grant AND a `group` grant on
--      the same drive duplicated every downstream probe (that's the
--      regression `tests/api/photos_multigrant_dedup.hurl` pins).
--      `WHERE EXISTS (…)` short-circuits at the first match and
--      keeps the one-row-per-drive shape every caller assumes.
--
--   2. ONE place to touch when the grant model changes. Group
--      nesting semantics, grant expiry rules, a future ReBAC
--      successor to `storage.role_grants`: everything that queries
--      "which drives does this caller see" updates HERE, not in
--      every listing repo in parallel.
--
-- STABLE SQL function — the planner inlines it into the caller's
-- query plan, so a `SELECT drive_id FROM caller_accessible_drives($1,
-- 'include_in_photo_index')` resolves to the same shape (and plan)
-- the inline CTE used to produce.
--
-- `policy_flag` applies the drive-policy filter whose main consumer
-- today is the photo-index axis (`'include_in_photo_index'`); pass
-- NULL to list every accessible drive without a policy restriction.
-- =============================================================

CREATE OR REPLACE FUNCTION storage.caller_accessible_drives(
    p_caller_id   uuid,
    p_policy_flag text DEFAULT NULL
) RETURNS TABLE (drive_id uuid)
LANGUAGE sql STABLE AS $$
    SELECT d.id
      FROM storage.drives_effective d
     WHERE EXISTS (
             SELECT 1 FROM storage.role_grants g
              WHERE g.resource_type = 'drive'
                AND g.resource_id   = d.id
                AND ( (g.subject_type = 'user'  AND g.subject_id = p_caller_id)
                   OR (g.subject_type = 'group' AND g.subject_id IN
                           (SELECT storage.caller_group_ids(p_caller_id))) )
                AND (g.expires_at IS NULL OR g.expires_at > NOW())
           )
       AND (
            p_policy_flag IS NULL
         OR (d.effective_policies->>p_policy_flag)::boolean = true
       );
$$;
