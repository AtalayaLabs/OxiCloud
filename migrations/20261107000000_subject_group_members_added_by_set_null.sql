ALTER TABLE auth.subject_group_members
    ALTER COLUMN added_by DROP NOT NULL,
    DROP CONSTRAINT subject_group_members_added_by_fkey,
    ADD CONSTRAINT subject_group_members_added_by_fkey
        FOREIGN KEY (added_by) REFERENCES auth.users(id) ON DELETE SET NULL;

COMMENT ON COLUMN auth.subject_group_members.added_by IS
    'User who added the membership; NULL when that user has been deleted.';
