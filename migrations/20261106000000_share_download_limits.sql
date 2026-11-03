-- ════════════════════════════════════════════════════════════════════════════
-- Public-link download limits
-- ════════════════════════════════════════════════════════════════════════════
-- `max_downloads` is a per-link cap on content downloads (full-file GET and
-- folder ZIP); NULL means unlimited. `download_count` is the consumed tally,
-- incremented atomically in the same UPDATE that enforces the cap — the check
-- and the bump cannot drift apart under concurrent visitors.
--
-- Deliberately separate from `access_count`: that column counts share
-- LANDINGS (the /api/s/{token} login), which every page load and every
-- re-open performs. Reusing it here would spend the allowance before the
-- visitor downloaded anything.
ALTER TABLE storage.shares
    ADD COLUMN IF NOT EXISTS max_downloads BIGINT,
    ADD COLUMN IF NOT EXISTS download_count BIGINT NOT NULL DEFAULT 0;

COMMENT ON COLUMN storage.shares.max_downloads IS
    'NULL = unlimited. Otherwise the number of content downloads this link may serve.';
COMMENT ON COLUMN storage.shares.download_count IS
    'Content downloads served through this link (full-file GET + folder ZIP). Range requests and 304s are not counted.';
