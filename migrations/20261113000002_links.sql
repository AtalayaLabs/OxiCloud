-- Links: login reconcile is gone, so its marker column is too.
ALTER TABLE auth.users DROP COLUMN IF EXISTS mounts_reconciled_at;
