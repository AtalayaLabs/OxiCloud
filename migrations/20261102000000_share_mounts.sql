-- ════════════════════════════════════════════════════════════════════════════
-- Share mount points (docs/plan/share-mounts.md § The model)
-- ════════════════════════════════════════════════════════════════════════════
-- A folder row with mount_target_id set is a mount: it lives in the
-- recipient's personal drive and answers for the target folder, which stays
-- in its own drive. Invariants I1–I5 are enforced here, not in Rust.

ALTER TABLE storage.folders
    ADD COLUMN IF NOT EXISTS mount_target_id UUID
        REFERENCES storage.folders(id) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS idx_folders_mount_target
    ON storage.folders(mount_target_id)
    WHERE mount_target_id IS NOT NULL;

-- I5: one mount per (recipient drive, target), trashed rows included.
CREATE UNIQUE INDEX IF NOT EXISTS idx_folders_one_mount_per_target
    ON storage.folders(drive_id, mount_target_id)
    WHERE mount_target_id IS NOT NULL;

-- I1, I3, I4 on the mount row itself.
CREATE OR REPLACE FUNCTION storage.check_share_mount_row()
RETURNS TRIGGER AS $$
DECLARE
    v_drive_kind   TEXT;
    v_target       storage.folders%ROWTYPE;
BEGIN
    IF NEW.mount_target_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT kind INTO v_drive_kind FROM storage.drives WHERE id = NEW.drive_id;
    IF v_drive_kind IS DISTINCT FROM 'personal' THEN
        RAISE EXCEPTION 'share mount must live in a personal drive'
            USING ERRCODE = 'check_violation';
    END IF;
    SELECT * INTO v_target FROM storage.folders WHERE id = NEW.mount_target_id;
    IF v_target.id IS NULL THEN
        RAISE EXCEPTION 'share mount target does not exist'
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF v_target.mount_target_id IS NOT NULL THEN
        RAISE EXCEPTION 'share mount target must not itself be a mount'
            USING ERRCODE = 'check_violation';
    END IF;
    IF v_target.drive_id = NEW.drive_id THEN
        RAISE EXCEPTION 'share mount target must be in another drive'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_share_mount_row ON storage.folders;
CREATE TRIGGER trg_share_mount_row
    BEFORE INSERT OR UPDATE OF mount_target_id, drive_id, parent_id ON storage.folders
    FOR EACH ROW
    EXECUTE FUNCTION storage.check_share_mount_row();

-- I2: a mount has no children. TG_ARGV[0] names the parent column.
CREATE OR REPLACE FUNCTION storage.forbid_children_of_share_mount()
RETURNS TRIGGER AS $$
DECLARE
    v_parent UUID;
BEGIN
    v_parent := (to_jsonb(NEW) ->> TG_ARGV[0])::uuid;
    IF v_parent IS NOT NULL AND EXISTS (
        SELECT 1 FROM storage.folders p
         WHERE p.id = v_parent AND p.mount_target_id IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'share mount cannot have children'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_no_children_under_mount_folders ON storage.folders;
CREATE TRIGGER trg_no_children_under_mount_folders
    BEFORE INSERT OR UPDATE OF parent_id ON storage.folders
    FOR EACH ROW
    EXECUTE FUNCTION storage.forbid_children_of_share_mount('parent_id');

DROP TRIGGER IF EXISTS trg_no_children_under_mount_files ON storage.files;
CREATE TRIGGER trg_no_children_under_mount_files
    BEFORE INSERT OR UPDATE OF folder_id ON storage.files
    FOR EACH ROW
    EXECUTE FUNCTION storage.forbid_children_of_share_mount('folder_id');

-- Recipient declined a target: reconcile must not recreate the mount.
CREATE TABLE IF NOT EXISTS storage.share_mount_declines (
    recipient_id     UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    target_folder_id UUID NOT NULL REFERENCES storage.folders(id) ON DELETE CASCADE,
    declined_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (recipient_id, target_folder_id)
);

-- Driver for the periodic reconcile job (P2). NULL = never reconciled.
ALTER TABLE auth.users
    ADD COLUMN IF NOT EXISTS mounts_reconciled_at TIMESTAMPTZ;

COMMENT ON COLUMN storage.folders.mount_target_id IS
    'Non-NULL marks a share mount: the row answers for the target folder (docs/plan/share-mounts.md).';
