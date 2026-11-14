-- Links: durable queue of grant and group-membership changes, drained by the link job.
CREATE TABLE IF NOT EXISTS storage.link_events (
    id            BIGSERIAL PRIMARY KEY,
    subject_type  TEXT NOT NULL CHECK (subject_type IN ('user', 'group')),
    subject_id    UUID NOT NULL,
    resource_type TEXT NOT NULL CHECK (resource_type IN ('folder', 'drive', 'group')),
    resource_id   UUID NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    cursor        UUID
);

CREATE OR REPLACE FUNCTION storage.enqueue_link_event_grant()
RETURNS TRIGGER AS $$
DECLARE
    r storage.role_grants%ROWTYPE;
BEGIN
    r := COALESCE(NEW, OLD);
    IF r.subject_type IN ('user', 'group') AND r.resource_type IN ('folder', 'drive') THEN
        INSERT INTO storage.link_events (subject_type, subject_id, resource_type, resource_id)
        VALUES (r.subject_type, r.subject_id, r.resource_type, r.resource_id);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_link_event_grant ON storage.role_grants;
CREATE TRIGGER trg_link_event_grant
    AFTER INSERT OR DELETE OR UPDATE OF expires_at ON storage.role_grants
    FOR EACH ROW
    EXECUTE FUNCTION storage.enqueue_link_event_grant();

CREATE OR REPLACE FUNCTION storage.enqueue_link_event_membership()
RETURNS TRIGGER AS $$
DECLARE
    r auth.subject_group_members%ROWTYPE;
BEGIN
    r := COALESCE(NEW, OLD);
    INSERT INTO storage.link_events (subject_type, subject_id, resource_type, resource_id)
    VALUES (
        CASE WHEN r.member_user_id IS NOT NULL THEN 'user' ELSE 'group' END,
        COALESCE(r.member_user_id, r.member_group_id),
        'group',
        r.group_id
    );
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_link_event_membership ON auth.subject_group_members;
CREATE TRIGGER trg_link_event_membership
    AFTER INSERT OR DELETE ON auth.subject_group_members
    FOR EACH ROW
    EXECUTE FUNCTION storage.enqueue_link_event_membership();

-- I1, I3, I4 on the link row itself; I1 requires the recipient's default personal drive.
CREATE OR REPLACE FUNCTION storage.check_share_mount_row()
RETURNS TRIGGER AS $$
DECLARE
    v_default      UUID;
    v_target       storage.folders%ROWTYPE;
BEGIN
    IF NEW.mount_target_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT default_for_user INTO v_default FROM storage.drives WHERE id = NEW.drive_id;
    IF v_default IS NULL OR v_default <> NEW.created_by THEN
        RAISE EXCEPTION 'link must live in the recipient''s default personal drive'
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
