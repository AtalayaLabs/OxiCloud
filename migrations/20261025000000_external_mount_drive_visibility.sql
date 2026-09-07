-- External mounts inherit access from the drive containing their mount-root
-- folder. Existing mounts remain in their current drive; for the historical
-- default that is the configuring administrator's personal drive.
UPDATE storage.external_mounts
SET visibility = 'drive'
WHERE visibility = 'owner';

ALTER TABLE storage.external_mounts
    ALTER COLUMN visibility SET DEFAULT 'drive';

COMMENT ON COLUMN storage.external_mounts.visibility IS
    'Compatibility marker. External mount visibility is inherited from the mount-root folder drive and its role grants.';
