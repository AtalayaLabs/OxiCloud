-- Drop `is_public` from audio.playlists.
--
-- Unlike the calendar flag dropped alongside this, the playlist one was
-- live: the UI had a globe toggle, `GET /api/playlists?include_public=true`
-- unioned every flagged playlist into the caller's listing, and the flag
-- short-circuited the Read gate on both `get_playlist` and
-- `list_playlist_tracks`.
--
-- It is removed because "readable by every account on the instance" was
-- never what the name suggested — it is not a public *link*, and an
-- anonymous visitor could never use it — and because it was a second
-- authorization surface sitting beside the grant table, which is where
-- the recent track-metadata IDOR got its blast radius. Sharing a playlist
-- with "everyone" belongs in `storage.role_grants` as a grant to an
-- everyone-principal, not as a boolean that bypasses the grant check.
--
-- Affected playlists become reachable only through their grants: the
-- owner keeps theirs, existing per-user shares keep working, and nobody
-- else sees them. No playlist or track is deleted.
--
-- The count is reported so the operator can tell affected users rather
-- than discovering it as a support ticket.
DO $$
DECLARE
    public_rows BIGINT;
BEGIN
    SELECT count(*) INTO public_rows
    FROM audio.playlists
    WHERE is_public;

    IF public_rows > 0 THEN
        RAISE WARNING
            'dropping audio.playlists.is_public: % playlist(s) were flagged public and are now reachable only through their grants (owner + existing shares). No tracks were removed.',
            public_rows;
    END IF;
END
$$;

DROP INDEX IF EXISTS audio.idx_playlists_is_public;

ALTER TABLE audio.playlists DROP COLUMN IF EXISTS is_public;
