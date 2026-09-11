-- notif.notifications — durable per-user notification records.
--
-- Backs the bell UI and the retention job. The message bus is best-effort
-- (a subscriber offline at publish time misses the push); this table is
-- the truth. Every `NotificationService::create` writes a row AND
-- publishes a `NotificationReceived` event on `user:{user_id}:notifications`.
-- A missed bus event recovers on the next `GET /api/notifications`.
--
-- See `docs/plan/message-bus.md § Slice E` for the wire contract and
-- retention policy.

CREATE SCHEMA IF NOT EXISTS notif;

CREATE TABLE IF NOT EXISTS notif.notifications (
    id           UUID  PRIMARY KEY DEFAULT gen_random_uuid(),

    -- Recipient. Every row is scoped to exactly one user; a share fanned
    -- to N members is N rows. Fanout truncation for very-large groups
    -- happens in the ingester (see plan § Notification fanout truncated),
    -- not here.
    user_id      UUID  NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,

    -- Notification kind — a stable slug the FE routes on for icon/label/
    -- action-button choice. New kinds are additive; never repurpose an
    -- existing one. Initial kinds:
    --   share_granted, new_login_from_new_device,
    --   job_completed_for_you, storage_quota_threshold
    kind         TEXT  NOT NULL,

    -- Per-kind opaque JSON with the fields the FE needs to render the
    -- row without a follow-up API call (subject name, resource id,
    -- action link…). Shape is a per-kind contract owned by the ingester;
    -- the DB stays schema-free here so a new field doesn't require a
    -- migration.
    payload      JSONB NOT NULL DEFAULT '{}'::jsonb,

    -- Wall-clock creation stamp. Sort key for the bell. Server-clock,
    -- not caller-clock — this is a DB-generated fact.
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- NULL = unread; non-NULL = when the user explicitly marked it
    -- read. Retention job deletes rows where read_at IS NOT NULL AND
    -- read_at < now() - retention_days.
    read_at      TIMESTAMPTZ
);

-- Bell fetch — GET /api/notifications lists a user's rows newest-first,
-- typically capped at ~50, sometimes filtered on unread. This one index
-- covers both the list query and the mark-all-read filter, and the
-- INCLUDE clause keeps common bell renders (id, kind, created_at,
-- read_at) index-only.
CREATE INDEX IF NOT EXISTS notifications_user_created_read
    ON notif.notifications (user_id, created_at DESC)
    INCLUDE (read_at, kind);

-- Retention job DELETE — scans read-and-old rows only. Partial keeps
-- the index tiny in the typical steady state where most rows are
-- unread.
CREATE INDEX IF NOT EXISTS notifications_read_at
    ON notif.notifications (read_at)
    WHERE read_at IS NOT NULL;
