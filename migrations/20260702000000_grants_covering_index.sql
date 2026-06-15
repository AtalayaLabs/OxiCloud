-- ════════════════════════════════════════════════════════════════════════════
-- ReBAC: covering index for the hot authorization check
-- ════════════════════════════════════════════════════════════════════════════
-- Every non-owner request runs PgAclEngine's cascade check
-- (pg_acl_engine.rs::folder_cascade_grant_exists / file_cascade_grant_exists),
-- which probes storage.access_grants with this shape:
--
--     WHERE subject_type = ANY($1)
--       AND subject_id   = ANY($2)
--       AND permission   = $3
--       AND resource_type = '<folder|file>'
--       AND (expires_at IS NULL OR expires_at > NOW())
--     [ AND resource_id = $4 ]            -- only the direct file-grant arm
--
-- Before this migration the probe was served by the UNIQUE constraint index
-- (subject_type, subject_id, resource_type, resource_id, permission). That is a
-- workable composite path, but it is suboptimal for the *cascade* arm (which
-- constrains permission + resource_type but NOT resource_id): permission sits
-- after resource_id in that index, and — crucially — the index has no expires_at,
-- so the probe must visit the heap for every candidate grant to read expiry and
-- visibility. On a subject with many grants (the implicit INTERNAL group is in
-- every internal user's set) that is one heap touch per candidate, on every check.
--
-- This index orders the cascade's equality columns contiguously
-- (subject_type, subject_id, permission, resource_type) and carries resource_id
-- as the last key column (so the direct file-grant arm's `resource_id = $4` is
-- still a seek). expires_at is an INCLUDE payload — never an equality/range seek
-- bound — which makes the probe a covering **Index-Only Scan**: zero heap fetches.
--
-- Measured on a synthetic subject with 20 000 grants of which only 100 are
-- (read, folder): the grant probe drops from ~51 shared-buffer touches / 100 heap
-- fetches (UNIQUE-index path) to ~9 buffers / 0 heap fetches (this index). The
-- absolute win grows with grant-table size and falls on every shared-resource
-- request, so it scales with sharing and request rate rather than data that fits
-- in cache.
--
-- Build + swap run in the migration's transaction, so the table is never without
-- a subject index even for an instant: the new index is created before the old
-- one is dropped, and a failure rolls both back atomically.

CREATE INDEX IF NOT EXISTS idx_grants_subject_perm_resource
    ON storage.access_grants (subject_type, subject_id, permission, resource_type, resource_id)
    INCLUDE (expires_at);

COMMENT ON INDEX storage.idx_grants_subject_perm_resource IS
    'Covering index for the PgAclEngine cascade check (subject × permission × '
    'resource_type [× resource_id]); expires_at is an INCLUDE payload so the '
    'grant-existence probe is an Index-Only Scan. Supersedes idx_grants_subject, '
    'whose (subject_type, subject_id) prefix this index also serves.';

-- idx_grants_subject (subject_type, subject_id) is redundant: its prefix is also
-- the leading prefix of BOTH this new index and the UNIQUE constraint index, so
-- every query it served — the cascade check, list_incoming_*, set_expiry /
-- revoke_all_for_subject, and the subject-delete trigger cleanup — is covered
-- without it. (Confirmed empirically: the planner already preferred the UNIQUE
-- index over idx_grants_subject for the cascade probe.) Drop it to stop
-- maintaining a third overlapping index on every grant write. idx_grants_resource,
-- idx_grants_expires_at and idx_grants_granted_by serve different access patterns
-- and are left in place.
DROP INDEX IF EXISTS storage.idx_grants_subject;
