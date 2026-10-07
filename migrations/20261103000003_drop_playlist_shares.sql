-- Drop audio.playlist_shares — the follow-up the ReBAC backfill promised.
--
-- `20260910000001_backfill_playlist_role_grants.sql` said, in its own
-- header:
--
--   The legacy `audio.playlist_shares` table stays in place through
--   this PR for rollback safety. It gets dropped in a follow-up
--   migration one release later, once the new engine path bakes.
--
-- That follow-up was never written. Its calendar and address-book
-- siblings were finished — `caldav.calendar_shares` and
-- `carddav.address_book_shares` are already gone, and their repository
-- methods were deleted rather than left behind — so the playlist table
-- is the last one standing.
--
-- It has been unreachable since the Phase 3 service rewrite: sharing a
-- playlist goes through `authz.set_role` into `storage.role_grants`,
-- and `MusicService` never calls the storage port's share methods. The
-- SQL stayed compiled in behind the port, which is why this was easy to
-- miss. Nothing could have written a row, and on the reference instance
-- the sequence still reports `is_called = f` — not one insert, ever.
--
-- Step 1 re-runs the original translation before dropping anything.
-- On a converged instance it is a no-op. On one that somehow still
-- holds rows, it guarantees every share has a `role_grants` equivalent
-- *before* the table disappears, so the drop cannot lose an
-- authorization relationship. Same mapping and same conflict key as
-- the original backfill, so re-running is safe.
INSERT INTO storage.role_grants
    (subject_type, subject_id, resource_type, resource_id, role, granted_by)
SELECT
    'user',
    s.user_id,
    'playlist',
    s.playlist_id,
    (CASE WHEN s.can_write THEN 'editor' ELSE 'viewer' END)::storage.grant_role,
    p.owner_id
  FROM audio.playlist_shares s
  JOIN audio.playlists       p ON p.id = s.playlist_id
 WHERE s.user_id <> p.owner_id
ON CONFLICT (subject_type, subject_id, resource_type, resource_id)
    DO NOTHING;

-- Step 2: report what step 1 had to rescue. Expected to be silent; a
-- warning here means an instance was still carrying legacy shares that
-- the 2026-09-10 backfill did not cover, which is worth knowing even
-- though the rows were just translated rather than lost.
DO $$
DECLARE
    legacy_rows BIGINT;
BEGIN
    SELECT count(*) INTO legacy_rows FROM audio.playlist_shares;

    IF legacy_rows > 0 THEN
        RAISE WARNING
            'audio.playlist_shares held % row(s) at drop time. Each was re-translated into storage.role_grants first, so no access was lost — but the 2026-09-10 backfill should already have covered them.',
            legacy_rows;
    END IF;
END
$$;

DROP TABLE IF EXISTS audio.playlist_shares;
