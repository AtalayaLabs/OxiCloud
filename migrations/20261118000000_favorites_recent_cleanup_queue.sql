-- =============================================================
-- Issue #913 — Favorites / Recent cleanup on grant loss
-- =============================================================
-- A favorite or recent-item row survives independently of the
-- grant that let the owner read its target. When a grant is
-- revoked (via API) or purged after expiry grace, the row
-- continues to JOIN back to `storage.files` on the listing path
-- and leaks live metadata (name, size, content_hash, mime, …)
-- for a resource the caller can no longer read — e.g. an
-- owner-side rename or rewrite is observable through the
-- stale favorite.
--
-- Design (see conversation on branch
-- `fix/recent-or-favorite-removal-on-grant-loss`):
--
-- * This migration adds a tiny `auth.cleanup_queue` keyed on
--   `user_id` (natural dedup) plus a trigger that enqueues
--   every user whose grant path has been affected by a
--   `role_grants` DELETE. The queue is drained by a scheduled
--   job (`FavoritesRecentCleanupJob`) which, per queued user,
--   iterates the user's favorites + recent rows, re-checks
--   each target through `PgAclEngine::check`, and deletes
--   rows the user can no longer read via SOME grant path.
--
-- * Trigger-based so EVERY delete path is covered without
--   relying on handler discipline:
--     - `DELETE /api/grants/{id}` (service layer calls
--       `authz.revoke` → role_grants row DELETE).
--     - `GrantCleanupService::purge` (post-grace expiry
--       purge) → `authz.purge_expired_grants` → role_grants
--       row DELETE.
--     - Direct-SQL operator revoke (no handler in the loop).
--     - Any future code path that hits
--       `DELETE FROM storage.role_grants ...`.
--   A missing audit-log line is survivable; a missing
--   cleanup is a security leak that reopens the issue.
--
-- * Group subjects fan out at enqueue time, in plpgsql,
--   through a recursive CTE over `auth.subject_group_members`.
--   The drain job sees per-user rows only — no group-shape
--   concerns downstream. Group membership removal itself is
--   NOT covered by this trigger; that signal is orthogonal
--   (the role_grants row is untouched when Bob leaves a
--   group that still holds a grant) and lives on the
--   `project_collab_authz_eviction_pending.md` roadmap under
--   `group_membership_lost` — the sweep's drain job can be
--   invoked with the same semantics once that event is wired.
--
-- * Expiry on its own does NOT enqueue — the grant row is
--   still present in `storage.role_grants` during grace, the
--   AuthZ engine already filters it at read time on every
--   permission check, and keeping the favorite visible for the
--   grace window matches the explicit UX decision. Only the
--   physical DELETE (after grace) triggers enqueue.
--
-- * Token subjects (anonymous share-link) have no per-user
--   favorites/recent state to clean up — the trigger's
--   `subject_type = 'user' OR 'group'` guard skips them.
-- =============================================================


-- ---------------------------------------------------------------
-- 1. Queue table — one row per user with pending cleanup work.
-- ---------------------------------------------------------------
--
-- PK on `user_id` is deliberate and load-bearing: it turns the
-- trigger's `INSERT ... ON CONFLICT (user_id) DO NOTHING` into
-- natural dedup. A mass purge of 100 expired grants that span
-- 50 users leaves 50 rows in the queue, not 100 — the drain
-- job sweeps each user once per cycle regardless of how many
-- grants touched them.
--
-- `ON DELETE CASCADE` on `user_id` means deleting the user
-- clears their queue row for free; nothing stale remains after
-- a user-delete sweep.
--
-- `enqueued_at` is load-bearing: the drain captures it alongside
-- the user_id and matches on the pair when it later deletes the
-- row, so a revoke that fires between SELECT and DELETE bumps
-- the timestamp (via `ON CONFLICT DO UPDATE` in the trigger
-- below) and the stale DELETE no-ops, leaving the row for the
-- next tick to re-sweep.
CREATE TABLE IF NOT EXISTS auth.cleanup_queue (
    user_id     UUID        PRIMARY KEY REFERENCES auth.users(id) ON DELETE CASCADE,
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT now()
);


-- ---------------------------------------------------------------
-- 2. Trigger function — fans a role_grants DELETE into one
--    queued row per affected user.
-- ---------------------------------------------------------------
--
-- `SECURITY DEFINER` is NOT used. The trigger executes in the
-- same session as the DELETE, so it sees the same schema it
-- would in that session; the writes land in `auth` which the
-- app role already holds INSERT on.
CREATE OR REPLACE FUNCTION auth.enqueue_cleanup_after_grant_removed()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    -- `ON CONFLICT DO UPDATE SET enqueued_at = EXCLUDED.enqueued_at`
    -- (instead of DO NOTHING) is the drain-race guard: a revoke
    -- that fires between the drain's SELECT and its DELETE bumps
    -- `enqueued_at`, so the drain's stale (user_id, enqueued_at)
    -- DELETE no-ops and the user is re-swept on the next tick.
    -- `DO NOTHING` would leave the drain racing with the enqueue
    -- and silently losing the second revoke's cleanup signal.
    --
    -- `WHERE EXISTS (SELECT 1 FROM auth.users …)` is the delete-
    -- user cascade guard. The admin user-delete flow deletes the
    -- user row first (which CASCADEs to `auth.user_favorites` /
    -- `auth.user_recent_files` / `auth.cleanup_queue` itself,
    -- closing the leak the normal way) and THEN deletes the
    -- user's `role_grants` rows explicitly (role_grants has no FK
    -- back to auth.users because subject_id is polymorphic). That
    -- second DELETE fires this trigger with a `subject_id` that
    -- no longer exists in auth.users within the same transaction,
    -- and the INSERT below would 500 on the FK — a correctness
    -- bug Hurl tests surface via the `admin/users` delete path.
    -- Skipping the INSERT when the user is already gone is safe:
    -- their favorites/recent rows cascaded away with the user,
    -- so there is nothing left to sweep.
    IF OLD.subject_type = 'user' THEN
        INSERT INTO auth.cleanup_queue (user_id)
        SELECT OLD.subject_id
        WHERE EXISTS (SELECT 1 FROM auth.users WHERE id = OLD.subject_id)
        ON CONFLICT (user_id) DO UPDATE
            SET enqueued_at = EXCLUDED.enqueued_at;
    ELSIF OLD.subject_type = 'group' THEN
        -- Recursive expansion over nested groups. The
        -- `subject_group_members` row holds EXACTLY one of
        -- `member_user_id` / `member_group_id` (xor
        -- CHECK in the table) — the CTE walks `member_group_id`
        -- edges and the final SELECT projects `member_user_id`
        -- leaves. ON CONFLICT folds a user who belongs to several
        -- nested groups all under the revoked parent into one
        -- queue row; the DO UPDATE bumps the timestamp so the
        -- same drain-race guard applies.
        --
        -- The join against `auth.users` filters out already-deleted
        -- members for the same reason the user-subject branch
        -- guards above. A revoked group grant whose members
        -- include a just-deleted user would otherwise 500 on the
        -- FK.
        INSERT INTO auth.cleanup_queue (user_id)
        WITH RECURSIVE reachable_groups(group_id) AS (
            SELECT OLD.subject_id
            UNION
            SELECT sgm.member_group_id
            FROM auth.subject_group_members sgm
            JOIN reachable_groups rg ON sgm.group_id = rg.group_id
            WHERE sgm.member_group_id IS NOT NULL
        )
        SELECT DISTINCT sgm.member_user_id
        FROM auth.subject_group_members sgm
        JOIN reachable_groups rg ON sgm.group_id = rg.group_id
        JOIN auth.users u ON u.id = sgm.member_user_id
        WHERE sgm.member_user_id IS NOT NULL
        ON CONFLICT (user_id) DO UPDATE
            SET enqueued_at = EXCLUDED.enqueued_at;
    END IF;
    -- 'token' subjects: nothing to enqueue (no per-user state).
    RETURN OLD;
END;
$$;


-- ---------------------------------------------------------------
-- 3. Trigger — AFTER DELETE so the role_grants row is already
--    gone when downstream re-checks happen.
-- ---------------------------------------------------------------
--
-- Statement-level is tempting (one call per batch DELETE) but
-- plpgsql's statement-level trigger has no OLD row; it would
-- need a transition table `REFERENCING OLD TABLE AS ...`. Row-
-- level is cheaper to reason about and the per-row INSERT is
-- already ON CONFLICT NO-OP for duplicates, so a batch of 100
-- DELETEs affecting 10 users produces 10 queue rows.
DROP TRIGGER IF EXISTS role_grants_enqueue_cleanup
    ON storage.role_grants;

CREATE TRIGGER role_grants_enqueue_cleanup
    AFTER DELETE ON storage.role_grants
    FOR EACH ROW
    EXECUTE FUNCTION auth.enqueue_cleanup_after_grant_removed();


COMMENT ON TABLE  auth.cleanup_queue IS
    'Per-user queue for favorites/recent-item cleanup after grant loss. '
    'Fed by a trigger on storage.role_grants DELETE, drained by the '
    'FavoritesRecentCleanupJob. PK on user_id gives natural dedup.';

COMMENT ON FUNCTION auth.enqueue_cleanup_after_grant_removed() IS
    'Fans a role_grants DELETE into per-user queue rows. User subjects '
    'enqueue themselves; group subjects fan out through transitive '
    'subject_group_members. Token subjects skipped (no per-user state).';
