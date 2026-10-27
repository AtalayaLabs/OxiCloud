# Plan — phase folders for the Hurl API suite

**Status:** design only, nothing implemented. Deliberately parked so
`blob-resniff.md` §B stays the active work — this is a test-infrastructure
refactor with a suite-wide blast radius and no urgency, now that the drift
guard (`aeddf416`) stops new scenarios going missing.

## Problem

`tests/api/run.sh` holds a hand-kept `HURL_FILES=( … )` array of 114 entries
that is the only thing making a scenario run. Two consequences:

1. **Files go missing silently.** Eight `.hurl` files existed on disk that
   nothing ever executed — including `lowercase_usernames`, which
   `docs/plan/username-lowercase.md` lists as a shipped deliverable, and
   `webdav_etag_cross_check`, which `docs/architecture/dates.md` cites as the
   authoritative pin for a rule it tells readers never to inline. They look
   like coverage and review as coverage. `aeddf416` now fails the run on a
   *new* one and grandfathers those eight in `UNWIRED_KNOWN`, which is a
   backstop, not a fix: the array is still the registry.
2. **Ordering intent lives far from the scenario.** The rationale for *where*
   a file sits is a comment in `run.sh` — "Second-to-last … BY DESIGN",
   "LAST, deliberately" — several hundred lines from the file it describes.

## Shape

Scenarios move into **numbered phase folders**; `run.sh` globs. The directory
listing becomes the order, so forgetting to register a file stops being
possible rather than being detected.

```
tests/api/
  010-bootstrap/   setup, auth_login, opaque_substrate, user_ui_preferences
  020-auth/        sessions, magic link, app passwords, registration
  030-nc-auth/     nc_status_capabilities … nc_avatar_preview
  040-resources/   files-folders, listings, etag contract, favorites, trash
  050-blobs/       copy, dedup, refcount, thumbnails, transcode, mime sniffing
  060-jobs/        admin_jobs, recoverable_jobs, jobs_notify
  070-dav/         caldav + carddav
  080-media/       playlists, audio_id3_extraction
  090-sharing/     public shares, permissions, grants, groups
  100-drives/      drives_foundation, membership, external_users
  110-admin/       admin user ops, role hierarchy, device auth, mounts
  120-quota/       quotas, drive policies, read-only, cross-drive moves
  130-dav-props/   dead/protected/quota properties, PATCH, PUT gaps
  140-clocks/      content-clock + capture-date scenarios
  150-webdav/      drive root, permissions, nested move, NFC, WOPI
  900-slow/        backend_migration_blackhole  (~31s by design)
  990-external/    backend_consistency_azure    (needs Azurite on 10000)
```

Boundaries are derived from the order the suite runs in today, not invented —
settle the exact cuts while moving, not now.

```bash
for dir in "$API_DIR"/[0-9]*/; do
  run_if_present "$dir/init.hurl"      # context setup
  for f in "$dir"*.hurl; do            # sorted; init/cleanup excluded
    selected "$(scenario_name "$f")" && run_hurl "$f"
  done
  run_if_present "$dir/cleanup.hurl"   # ALWAYS, see below
done
```

## Per-folder init / cleanup

Each folder may carry `init.hurl` and `cleanup.hurl` (or `.sh`), run around
its scenarios. Two things this buys that the current shape cannot:

- **Cleanup runs even when a scenario fails.** Today teardown lives at the
  bottom of each `.hurl`, so a mid-file failure skips it and leaves residue
  in a database every later scenario shares — one red test cascades into
  unrelated red tests. Folder cleanup runs under a trap, so the blast radius
  of a failure stops at its phase.
- **Shared setup stops being copy-pasted.** Most files open with the same
  login; several also mint an app password. That belongs in `init.hurl`.

Rules: `init`/`cleanup` are excluded from `--list` and from selector matching
(they are not scenarios); `cleanup` must be idempotent and must not fail the
run on its own; a selective run still executes its phase's init/cleanup, or
selection would test a different environment than a full run.

## Context vs phase — they are different axes

A phase is ordering *within one server environment*. A scenario needing a
**different server env** does not belong in `tests/api/` at all: the repo
already has the pattern — `tests/oidc/` and `tests/webdav-drive-root/` each
carry their own `run.sh`, `test.env` and `storage/`, chained from the
justfile.

`admin_advisories` is the one scenario on the wrong side of that line today.
It asserts on whether `OXICLOUD_JOBS_NOTIFY_EMAIL_TO` and
`OXICLOUD_WEBHOOK_URL` are set, which `server.env` deliberately leaves unset,
so it cannot share an environment with the rest — it needs its own context
directory, not a phase folder.

## Migration

| step | note |
|---|---|
| `git mv` 114 files into folders | preserve the current order exactly; the suite shares one DB and several scenarios depend on predecessors |
| move the ordering comments | out of `run.sh`, into the header of the file each describes — the point of the exercise |
| rewrite the runner | glob + per-folder init/cleanup; strip the folder prefix for selector names so `run.sh rechunk_legacy` keeps working |
| 27 doc cross-references | `docs/` and `src/` cite `tests/api/<name>.hurl` paths; all break |
| retire `UNWIRED_KNOWN` | the glob makes it unnecessary — unless a file is deliberately excluded, which should be a context dir instead |
| the 7 forgotten scenarios | they start running for the first time. Some may fail on state or ordering, and **that failure is the finding** — but triage them one at a time BEFORE the move, or the move lands red for reasons unrelated to itself |

Do the orphan triage first, as its own change. Moving 114 files and
discovering seven never-executed scenarios in the same commit makes both
undebuggable.

## Open

- Whether `init`/`cleanup` should be `.hurl`, `.sh`, or whichever is present.
  `.hurl` keeps it in one language; `.sh` can do things Hurl cannot (reset
  storage, poke the DB). Probably: both, `.sh` first.
- Whether a phase folder should be able to declare env overrides, or whether
  needing one is by definition a context dir. Leaning hard toward the latter —
  one env per runner keeps "what was this scenario run against" answerable.
