-- ════════════════════════════════════════════════════════════════════════════
-- Immediate ancestor-ETag bumps for structural folder changes (F4)
-- ════════════════════════════════════════════════════════════════════════════
-- `20260627000000_async_tree_etag_queue` made every ancestor bump
-- asynchronous: triggers only enqueue, `TreeEtagFlushService` applies the
-- bump up to one flush interval (default 500 ms) after commit. For file
-- writes that is the right trade — uploads take no folder-row locks and
-- cannot deadlock each other. For folder create / rename / move / trash /
-- restore / delete it opens a window a sync client can fall into
-- (docs/plan/nc-sync-client-findings.md, F4): a PROPFIND inside the
-- window sees the new listing under the OLD collection ETag. Observed
-- with the NC desktop client: a server-side folder move arrived as
-- "source gone" without "destination present", so the client deleted
-- its copy and re-downloaded the whole folder one poll later.
--
-- Structural folder changes now bump the ancestor chain IN THE WRITER'S
-- OWN STATEMENT, so the new listing and the new collection ETags commit
-- atomically. The state lives entirely in the database, so it holds with
-- any number of app instances (HA) — no in-process "pending" set.
--
-- Why this does not bring back the 20260627000000 deadlocks:
--   * The immediate bump locks victims `FOR NO KEY UPDATE SKIP LOCKED`.
--     NO KEY UPDATE is compatible with the FOR KEY SHARE an upload's FK
--     check takes on its parent folder (the conflict that caused the
--     deadlocks was FOR UPDATE), and SKIP LOCKED means the bump never
--     WAITS on anyone — no wait edge, no cycle.
--   * It runs from the statement-level AFTER trigger, i.e. as the last
--     step of the writing statement. Every structural folder write is a
--     single-statement transaction (folder_db_repository.rs), so the
--     locks it takes are released at commit without the transaction
--     waiting on anything in between.
--   * Any victim that was skipped (locked by the flusher or a concurrent
--     writer) or vanished falls back to the queue: the targets are
--     enqueued exactly as before and the flusher completes the bump. The
--     queue stays the delivery guarantee; the immediate bump only closes
--     the window when it can do so without waiting.
--
-- Scope: only the TOP-LEVEL rows of a statement bump immediately — rows
-- whose parent is not itself changed by the same statement. For a move
-- or rename that is the one row; for a subtree trash / restore it is the
-- subtree root, whose parent chain is what a client walks. Targets inside
-- a changed subtree stay on the queue, so a large trash does not rewrite
-- its whole subtree inside the user's request. File-side triggers are
-- unchanged (still queue-only).
--
-- Semantics kept from 20260627000000 / 20260802100003: depth guard,
-- UPDATE value filter, old + new chains on moves, strict-ancestor
-- self-exclusion, drive_id isolation, and the monotonic
-- `GREATEST(NOW(), tree_modified_at + 1 s)` bump (two bumps within one
-- wall-clock second must still yield two distinct second-granular ETags).

-- ── Shared helper ────────────────────────────────────────────────────────
-- Parallel arrays, one entry per bump target (an inclusive target: the
-- target folder and all its ancestors are bumped). `p_immediate` marks the
-- targets to bump now; the rest, plus all of them if any immediate victim
-- could not be locked, go to `storage.tree_etag_dirty`.
CREATE OR REPLACE FUNCTION storage.tree_etag_bump_or_enqueue(
    p_lpath     ltree[],
    p_folder_id uuid[],
    p_drive_id  uuid[],
    p_immediate boolean[]
)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    v_wanted bigint := 0;
    v_bumped bigint := 0;
BEGIN
    IF coalesce(cardinality(p_lpath), 0) = 0 THEN
        RETURN;
    END IF;

    IF true = ANY (p_immediate) THEN
        WITH targets AS (
            SELECT t.lpath, t.drive_id
              FROM unnest(p_lpath, p_drive_id, p_immediate)
                   AS t(lpath, drive_id, immediate)
             WHERE t.immediate
        ),
        wanted AS (
            -- Same victim walk as the flusher (tree_etag_flush_service.rs).
            SELECT f.id
              FROM storage.folders f
             WHERE EXISTS (
                 SELECT 1 FROM targets t
                  WHERE f.lpath @> t.lpath
                    AND (t.drive_id IS NULL OR f.drive_id = t.drive_id)
             )
        ),
        locked AS (
            SELECT f.id
              FROM storage.folders f
             WHERE f.id IN (SELECT id FROM wanted)
             ORDER BY f.id
               FOR NO KEY UPDATE OF f SKIP LOCKED
        ),
        bumped AS (
            UPDATE storage.folders f
               SET tree_modified_at =
                       GREATEST(NOW(), f.tree_modified_at + interval '1 second')
              FROM locked l
             WHERE f.id = l.id
            RETURNING f.id
        )
        SELECT (SELECT count(*) FROM wanted), (SELECT count(*) FROM bumped)
          INTO v_wanted, v_bumped;
    END IF;

    INSERT INTO storage.tree_etag_dirty (lpath, folder_id, drive_id)
    SELECT t.lpath, t.folder_id, t.drive_id
      FROM unnest(p_lpath, p_folder_id, p_drive_id, p_immediate)
           AS t(lpath, folder_id, drive_id, immediate)
     WHERE NOT t.immediate
        OR v_bumped < v_wanted;
END;
$$;


-- ── Folder-side INSERT / DELETE trigger ──────────────────────────────────
-- Targets as in 20260802100003 (the PARENT's lpath — strict ancestors
-- only); a row is top-level when its parent is not in the same statement.
-- A DELETE cascade removes descendants in nested statements (depth > 1,
-- skipped), so a single-folder delete is always top-level.
CREATE OR REPLACE FUNCTION storage.bump_tree_from_folders_stmt()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    v_lpath     ltree[];
    v_folder_id uuid[];
    v_drive_id  uuid[];
    v_immediate boolean[];
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    SELECT array_agg(t.lpath), array_agg(t.parent_id),
           array_agg(t.drive_id), array_agg(t.immediate)
      INTO v_lpath, v_folder_id, v_drive_id, v_immediate
      FROM (SELECT DISTINCT
                   subpath(r.lpath, 0, nlevel(r.lpath) - 1) AS lpath,
                   r.parent_id,
                   r.drive_id,
                   NOT EXISTS (SELECT 1 FROM changed_rows p
                                WHERE p.id = r.parent_id) AS immediate
              FROM changed_rows r
             WHERE r.lpath IS NOT NULL
               AND nlevel(r.lpath) > 1) t;

    PERFORM storage.tree_etag_bump_or_enqueue(
        v_lpath, v_folder_id, v_drive_id, v_immediate);

    RETURN NULL;
END;
$$;


-- ── Folder-side UPDATE trigger ───────────────────────────────────────────
-- Old and new parent chains, same value filter as before. Top-level =
-- the row's parent (on that side) is not itself among the changed rows.
CREATE OR REPLACE FUNCTION storage.bump_tree_from_folders_stmt_upd()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE
    v_lpath     ltree[];
    v_folder_id uuid[];
    v_drive_id  uuid[];
    v_immediate boolean[];
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    WITH changed AS (
        SELECT n.id,
               o.lpath     AS old_lpath,
               o.parent_id AS old_parent_id,
               o.drive_id  AS old_drive_id,
               n.lpath     AS new_lpath,
               n.parent_id AS new_parent_id,
               n.drive_id  AS new_drive_id
          FROM old_rows o
          JOIN new_rows n USING (id)
         WHERE (o.name, o.parent_id, o.is_trashed, o.updated_at)
               IS DISTINCT FROM
               (n.name, n.parent_id, n.is_trashed, n.updated_at)
    ),
    sides AS (
        SELECT old_lpath AS lpath, old_parent_id AS parent_id,
               old_drive_id AS drive_id
          FROM changed WHERE old_lpath IS NOT NULL
        UNION
        SELECT new_lpath, new_parent_id, new_drive_id
          FROM changed WHERE new_lpath IS NOT NULL
    )
    SELECT array_agg(t.lpath), array_agg(t.parent_id),
           array_agg(t.drive_id), array_agg(t.immediate)
      INTO v_lpath, v_folder_id, v_drive_id, v_immediate
      FROM (SELECT DISTINCT
                   subpath(s.lpath, 0, nlevel(s.lpath) - 1) AS lpath,
                   s.parent_id,
                   s.drive_id,
                   NOT EXISTS (SELECT 1 FROM changed c
                                WHERE c.id = s.parent_id) AS immediate
              FROM sides s
             WHERE nlevel(s.lpath) > 1) t;

    PERFORM storage.tree_etag_bump_or_enqueue(
        v_lpath, v_folder_id, v_drive_id, v_immediate);

    RETURN NULL;
END;
$$;
