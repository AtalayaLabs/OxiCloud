-- ════════════════════════════════════════════════════════════════════════════
-- Drive policy defaults — per-kind, live inheritance
-- ════════════════════════════════════════════════════════════════════════════
--
-- Design: docs/plan/drive-default-policies.md
--
-- Until now there was no notion of a default drive policy. The only thing
-- resembling one was a hardcoded JSONB literal in two INSERT statements
-- (`drive_pg_repository.rs` — personal drives got the two index flags on,
-- shared drives got an empty bag). An admin could not say "every new shared
-- drive forbids public links", and there was no way to see which drives had
-- drifted laxer than intended.
--
-- Model: **live inheritance, overrides only.**
--   * `storage.drive_policy_defaults` holds one row per drive kind.
--   * `storage.drives.policies` holds ONLY the knobs an admin explicitly set
--     on that drive. A key's absence now means "inherit", where before it
--     meant "false".
--   * Effective policy = default(kind) || drive overrides. The `||` operator
--     is right-biased in Postgres, so the drive's own keys win.
--
-- The two kinds are independent settings, not one default with an exception:
-- a personal default and a shared default for the same knob are unrelated.
--
-- Key invariants:
--   * `kind` mirrors the CHECK on `storage.drives.kind`. Extending the drive
--     kinds (the reserved `Vault`, see AGENTS.md) means a DROP + ADD CHECK
--     pair here as well as there, plus a row insert — deliberately explicit
--     so a new kind cannot silently inherit another kind's posture.
--   * `read_only` is NEVER stored here. It is an operational state (freeze
--     this drive, for this reason, for this long), not a standing posture,
--     and a default that froze every drive at once would be a footgun with
--     no legitimate use. Enforced in the service layer, not by constraint,
--     because the bag is intentionally permissive about unknown keys.
--
-- ─────────────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS storage.drive_policy_defaults (
    kind        TEXT PRIMARY KEY
        CHECK (kind IN ('personal', 'shared')),

    -- Same permissive JSONB bag as `storage.drives.policies`: unknown keys
    -- are preserved verbatim so a future knob lands without a migration.
    policies    JSONB NOT NULL DEFAULT '{}'::jsonb,

    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Who last edited this default. No FK: an admin account can be deleted
    -- without invalidating the audit trail of what they set, matching the
    -- §14 provenance convention used elsewhere (NOT NULL columns with no FK).
    updated_by  UUID
);

COMMENT ON TABLE storage.drive_policy_defaults IS
    'Per-drive-kind default policy bag. Effective policy = this || drives.policies.';
COMMENT ON COLUMN storage.drive_policy_defaults.policies IS
    'JSONB capability-flag defaults; see docs/plan/drive-default-policies.md.';

-- ─────────────────────────────────────────────────────────────────────────────
-- Seed with EXACTLY today's hardcoded literals, so upgrading changes no
-- drive's effective behaviour. `drive_pg_repository.rs` created personal
-- drives with the two index flags on and shared drives with an empty bag;
-- those literals move here and the INSERTs stop carrying them.
-- ─────────────────────────────────────────────────────────────────────────────

INSERT INTO storage.drive_policy_defaults (kind, policies) VALUES
    ('personal', '{"include_in_photo_index": true, "include_in_music_index": true}'::jsonb),
    ('shared',   '{}'::jsonb)
ON CONFLICT (kind) DO NOTHING;

-- ─────────────────────────────────────────────────────────────────────────────
-- Prune redundant keys from existing drives.
--
-- Existing bags were written by partial-merge PATCHes, so most carry keys
-- whose value already equals the seeded default. Under the new semantics
-- those would read as DELIBERATE overrides, and the very first drift report
-- would list every drive in the system — noise that would make the report
-- useless on day one.
--
-- Deleting a key whose value equals the default is behaviour-identical:
-- resolution yields the same answer either way. What changes is only whether
-- the drive is recorded as having *decided* that value.
--
-- Note `-` on jsonb removes a key by name; the subquery compares the drive's
-- value against the default for its own kind.
-- ─────────────────────────────────────────────────────────────────────────────

DO $$
DECLARE
    k TEXT;
    drive_kind TEXT;
BEGIN
    FOREACH drive_kind IN ARRAY ARRAY['personal', 'shared'] LOOP
        FOR k IN
            SELECT jsonb_object_keys(policies)
              FROM storage.drive_policy_defaults
             WHERE kind = drive_kind
        LOOP
            UPDATE storage.drives d
               SET policies = d.policies - k
              FROM storage.drive_policy_defaults p
             WHERE p.kind = d.kind
               AND d.kind = drive_kind
               AND d.policies ? k
               AND d.policies -> k = p.policies -> k;
        END LOOP;
    END LOOP;
END $$;

-- Keys explicitly set to the type default (`false`) that the kind's default
-- bag does not mention at all are ALSO redundant — absent resolves to the
-- same `false`. Strip them so the drift report only ever shows real
-- decisions. `read_only` is exempt: it is never inherited, so an explicit
-- `false` there is the only representation it has.
DO $$
DECLARE
    k TEXT;
BEGIN
    FOREACH k IN ARRAY ARRAY[
        'forbid_sharing', 'forbid_external_sharing', 'forbid_public_links',
        'forbid_cross_drive_move', 'forbid_owner_role_change',
        'include_in_photo_index', 'include_in_music_index'
    ] LOOP
        UPDATE storage.drives d
           SET policies = d.policies - k
          FROM storage.drive_policy_defaults p
         WHERE p.kind = d.kind
           AND d.policies -> k = 'false'::jsonb
           AND NOT (p.policies ? k);
    END LOOP;
END $$;

-- ─────────────────────────────────────────────────────────────────────────────
-- Effective-policy view.
--
-- Four enforcement sites read the bag in raw SQL and must see the RESOLVED
-- value, not the override bag:
--   * file_blob_read_repository — include_in_photo_index (timeline, places)
--   * trash_db_repository       — read_only (purge, file + folder branches)
--
-- The join is dozens of drive rows against two default rows; negligible even
-- on the photos timeline, which is the hottest of the four.
--
-- `SELECT d.*` deliberately: callers that selected columns off
-- `storage.drives` keep working when pointed here, and only the policy
-- expression changes name.
-- ─────────────────────────────────────────────────────────────────────────────

CREATE OR REPLACE VIEW storage.drives_effective AS
SELECT
    d.*,
    COALESCE(p.policies, '{}'::jsonb) || d.policies AS effective_policies
  FROM storage.drives d
  LEFT JOIN storage.drive_policy_defaults p ON p.kind = d.kind;

COMMENT ON VIEW storage.drives_effective IS
    'storage.drives with effective_policies = kind default || per-drive overrides.';
