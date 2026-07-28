-- ============================================================================
-- Slice 7 of Part 2 — persistent finding storage.
--
-- Findings from consistency jobs (and eventually storage_migration failure
-- rows, reextract failures) currently flow into `tracing::warn!` targeted
-- at `oxicloud::consistency`. That works for live tailing but rotates
-- away — an operator opening the admin UI a day after a run has nothing
-- to drill into. This table makes findings first-class + queryable.
--
-- `run_findings` is INTENTIONALLY generic. The consistency-check plan and
-- the plan doc's tenant table both list `kind` + `severity` + `resource_id`
-- + `detail` as the union of what every current tenant needs. Adding a
-- tenant with a novel per-finding field means widening `detail`
-- (JSONB, per-tenant shape), not adding a column.
--
-- Cascade rule: findings live and die with their parent run. Deleting a
-- terminal run row (retention pruning, planned) also drops its findings.
-- ============================================================================

CREATE TABLE jobs.run_findings (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id       UUID        NOT NULL
                              REFERENCES jobs.recoverable_runs(id)
                              ON DELETE CASCADE,
    -- Machine-readable kind (e.g. 'stale_used_bytes', 'missing_blob').
    -- Stable across releases per the audit-log convention — see
    -- feedback memory `enum_over_string_literals_in_logs`. New failure
    -- mode = new value, never repurpose an existing one.
    kind         TEXT        NOT NULL,
    -- Severity spectrum:
    --   'data_loss'    — bytes / rows unreachable or gone.
    --   'inconsistent' — counters / materialised values wrong,
    --                    content intact.
    --   'anomaly'      — surprising state worth surfacing, no known impact.
    -- TEXT (not ENUM) so tenants can grow the vocabulary without a schema
    -- migration; the app-layer types.rs is where the canonical set lives.
    severity     TEXT        NOT NULL,
    -- Nullable — some findings pertain to the run as a whole
    -- (e.g. "backend enumeration truncated after 1M keys") rather than
    -- one specific resource.
    resource_id  UUID,
    -- Per-tenant per-finding structured detail (cached/actual/delta,
    -- blob_hash, expected/stored path, ...). Consumers key off `kind`
    -- to know the shape.
    detail       JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Time-ordered listing per run: `GET /api/admin/jobs/{name}/runs/{id}/findings`
-- pages by (run_id, created_at) — index-only scan.
CREATE INDEX ON jobs.run_findings (run_id, created_at);

-- Aggregation queries — "how many `missing_blob` findings across all
-- runs of `files_consistency`" or "how many `data_loss`-severity
-- findings today". Both need `(kind)` and `(severity)` predicates; a
-- composite works for either since the leading column is selective.
CREATE INDEX ON jobs.run_findings (kind, severity, created_at);

COMMENT ON TABLE jobs.run_findings IS
    'Structured per-finding records emitted by recoverable jobs. Replaces '
    'the transitional `tracing::warn!(event=consistency_finding)` calls '
    'in the consistency tenants — see docs/plan/job-registry.md Part 2.';
