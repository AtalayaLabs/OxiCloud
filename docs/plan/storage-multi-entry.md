# Plan — Multi-entry storage config + name-selected active backend

## Context

Today OxiCloud has a single storage backend, configured via a flat set of env
vars (`OXICLOUD_STORAGE_BACKEND`, `OXICLOUD_S3_*`, `OXICLOUD_STORAGE_ENCRYPTION_KEY`,
…). The admin panel has a second, parallel storage-config surface backed by
`admin_settings.storage.*` rows, used by the migration tool to pick a target.
Priority is `env > DB > defaults`, so the DB config effectively acts as a
staging area for "what the migration should copy INTO" but never wins at boot.

Two chronic problems fall out:

1. **Split-brain config.** Admin edits DB via the panel; app boot ignores DB.
   Migration completes; live backend hasn't moved. Admin has to remember to
   copy env vars into `.env` and restart. Two sources of truth for the same
   setting. Cutover is a manual multi-step flow; users routinely get it wrong.

2. **Migration data-loss window on concurrent writes.** The copy walks
   `storage.blobs` in hash order. A blob whose hash is lex-lower than the
   current cursor, written to source AFTER migration passed it, is never
   copied to target. `passed=true, findings=0` completion does NOT guarantee
   target has every blob. Silent.

3. **Migration target selection is fragile.** DTO passes the whole S3 config
   at trigger time; secrets sit plaintext in `admin_settings`. Any future
   pluggable-storage story compounds this (Azure, GCS, WebDAV-as-source, …).

This plan replaces the split-brain model with a single-source-of-truth
architecture:

- `.env` declares **N named storage entries** (immutable per-deploy).
- `admin_settings.storage.active_backend_name` holds ONE row — which named
  entry the app currently runs on. That's the whole runtime config.
- Migration is the atomic transition from one active entry to another. Server
  is put in read-only mode for the copy window; on completion, the active
  pointer flips; a restart cuts over.

Named entries also solve two adjacent problems Ed flagged during design:
- **Per-entry encryption keys** enable `local (raw) → s3 (encrypted K1)`
  moves AND `s3 (K1) → s3-new-bucket (K2)` key-rotation moves.
- **`?storage=<name>` on `blobs_consistency`/`backend_consistency`** lets
  operators audit any registered entry (target verification, pre-decommission
  check, etc.), replacing the sample-based `verify_migration` endpoint with a
  full-walk audit.

## Design decisions

### Named entries in `.env` — explicit allowlist

```
OXICLOUD_STORAGE_ENTRIES=local_main,s3_prod

OXICLOUD_STORAGE_local_main_BACKEND=local
OXICLOUD_STORAGE_local_main_ROOT_DIR=/data

OXICLOUD_STORAGE_s3_prod_BACKEND=s3
OXICLOUD_STORAGE_s3_prod_S3_BUCKET=my-bucket
OXICLOUD_STORAGE_s3_prod_S3_ENDPOINT_URL=https://s3.example.com
OXICLOUD_STORAGE_s3_prod_S3_REGION=us-east-1
OXICLOUD_STORAGE_s3_prod_S3_ACCESS_KEY=...
OXICLOUD_STORAGE_s3_prod_S3_SECRET_KEY=...
OXICLOUD_STORAGE_s3_prod_S3_FORCE_PATH_STYLE=true
OXICLOUD_STORAGE_s3_prod_ENCRYPTION_KEY=<base64-32-bytes>
```

- **Explicit `_ENTRIES` allowlist** — order-independent, admin-authored names.
  Env-pattern-matching was considered and rejected: too fragile
  (someone typos `OXICLOUD_STORAGE_s3_prud_BUCKET`, gets a silently-registered
  ghost entry). Explicit list forces a declaration.
- **Names** are admin-chosen strings matching `[a-z0-9_-]{1,32}`, unique
  within the list. Parsed at boot; unparseable → fail-fast with the offending
  name in the error.
- **Order does not matter** — DB says which is active. Reordering entries in
  `.env` never changes runtime behaviour.

### Legacy flat-var interaction — three states, one is fail-fast

Multi-entry lives alongside the pre-existing single-backend flat vars
(`OXICLOUD_STORAGE_BACKEND`, `OXICLOUD_S3_*`, `OXICLOUD_AZURE_*`,
`OXICLOUD_STORAGE_ENCRYPTION_KEY`, `_ENABLED`). The parser resolves the
interaction as follows:

| `_ENTRIES` | Legacy storage-backend vars present | Behaviour |
|---|---|---|
| Unset / empty | Absent | `storage_entries = []`. Boot uses framework defaults (Local at `storage/`). |
| Unset / empty | Present | **Synthesize** a single entry named `default` from the legacy vars. Preserves upgrade path — existing deployments keep working without touching `.env`. |
| Set (e.g. `foo,bar`) | Absent | Parse each named entry from `_STORAGE_<NAME>_*` vars. Fail-fast if any declared entry is missing its required per-name fields. |
| Set (e.g. `foo,bar`) | **Present** | **FAIL FAST**. Boot aborts with an error listing every legacy var found. Admin must remove them or migrate them into per-entry `_STORAGE_<NAME>_*` form. |

**Why fail-fast on the "both set" case:** without it, admin state is
ambiguous — someone edits `OXICLOUD_S3_BUCKET` expecting it to matter,
but it's silently ignored because `_ENTRIES` won. The subtle-bug cost
is much higher than the one-time cleanup cost. Refusing to boot forces
the conversion once, with a clear message naming the exact vars to
remove.

**Set of "legacy storage-backend vars"** counted for the conflict check:
`OXICLOUD_STORAGE_BACKEND`, all seven `OXICLOUD_S3_*`, all five
`OXICLOUD_AZURE_*`, `OXICLOUD_STORAGE_ENCRYPTION_ENABLED`, and
`OXICLOUD_STORAGE_ENCRYPTION_KEY`. `OXICLOUD_STORAGE_PATH` is NOT in
this set — it drives multiple non-backend things (chunk dir default,
etc.) and remains a valid ambient path; per-entry `_ROOT_DIR` falls
back to it for Local entries when unset.

### One DB row: `active_backend_name`

Single setting in `admin_settings`:

```
storage.active_backend_name = "local_main"
```

- **Boot logic**:
  1. Parse `AppConfig.storage_entries` from env.
  2. Read `active_backend_name` from `admin_settings`.
  3. If unset (fresh install): use the FIRST name in `_ENTRIES`. Log which
     one, don't fail.
  4. Look up the entry by name. Build `blob_backend` from it.
  5. If the name doesn't exist in current env (deploy drift — someone removed
     an entry): **fail-fast at boot** with a clear error naming the missing
     entry AND listing the available ones. Admin fixes env or overrides the
     setting via a fallback CLI (see §Fallback).
- Everything the admin panel currently writes about storage
  (`s3.bucket`, `s3.access_key`, …) is **removed** from DB. `save_storage_settings`
  is deleted along with those rows.

### Encryption is per-entry

`OXICLOUD_STORAGE_<NAME>_ENCRYPTION_KEY` (base64 of exactly 32 bytes) on an
entry → that entry's backend is wrapped in `EncryptedBlobBackend` at build
time. Absent → raw backend.

- **Presence-implies-enabled.** No separate `_ENCRYPTION_ENABLED` toggle —
  one env var per entry is enough.
- **Fail-fast on invalid key**: bad base64, wrong decoded length → boot
  aborts with the entry name in the error message. A bad key is a real
  deployment error, must be caught at boot, never silently disabled.
- **Legacy `OXICLOUD_STORAGE_ENCRYPTION_KEY`** remains honoured for the
  synthesized `default` entry when `_ENTRIES` is empty (upgrade path).

Cross-entry encryption combinations work out of the box because
`EncryptedBlobBackend` is a decorator and `copy_blob` in the migration
handler always spools plaintext to a tmp file:

| Source | Target | Migration behaviour |
|---|---|---|
| Raw local | Encrypted S3 (K1) | Read plaintext → write encrypts with K1 |
| Encrypted S3 (K1) | Raw local | Read decrypts with K1 → write plaintext |
| Encrypted S3 (K1) | Encrypted S3 (K2) on different bucket | Read decrypts K1 → write encrypts K2 (rotation via new bucket) |
| Encrypted S3 (K1) | Encrypted S3 (K2) on SAME bucket | **REFUSED** — see below |

**In-place key rotation is refused.** Same physical bucket + different
encryption key would have the migration overwrite `<hash>.blob` with K2
ciphertext while the LIVE backend is still K1-configured → readers get
K1-decrypt-of-K2-bytes → 500. Silent data-loss window. The
`is_source_target_identical` guard (see §Migration flow) catches this because
`storage_identity` deliberately excludes the encryption key. The refusal
message spells the case out and recommends the two-step workaround (rotate
via a temp bucket).

**Proper in-place rotation (deferred future slice)**

The safe implementation is to namespace object keys by encryption
generation — `<hash>.k2.blob` or `k2/<hash>.blob` (backend-specific
convention). Then:

- Both key generations coexist in the same bucket during rotation.
- Reads continue against K1 (LIVE backend) — old keys untouched.
- Target writes go to K2 keys.
- Cutover restarts with K2 as live; K1 objects can be reaped async by
  a follow-up sweep.
- The `is_source_target_identical` guard's `storage_identity` string
  changes to INCLUDE the encryption generation — same-bucket
  different-generation then correctly registers as a legitimate
  migration, no longer refused.

Schema changes required:
- `EncryptedBlobBackend` gains a `generation: u32` field.
- Object keys become `<hash>.k<gen>.blob` (or backend-specific
  equivalent — S3 key naming, Azure blob naming).
- Read path tries current-gen first, falls back to prior-gen for the
  rotation window (bounded by cutover-completion + async-reap
  duration).

Not built now — real key-rotation demand is rare enough that the
two-step workaround via temp bucket is acceptable. File this as
a follow-up slice AFTER multi-entry lands; the named-entry
infrastructure is a prerequisite (generation-versioned keys only
make sense when there's an entry model to hold "which generation
is active" as configuration).

### Read-only mode reuses the existing AuthZ short-circuit

`DrivePolicies.read_only` already gates writes at
`PgAclEngine::check_inner` — short-circuits `Create|Update|Delete|Share`
Permission checks on any resource in a read-only drive. We extend that clause
with a global check:

```rust
// Inside PgAclEngine::check_inner, before per-drive read_only check:
if permission.is_write() && self.migration_readonly.load(Ordering::Relaxed) {
    return AclDecision::Denied("server in migration read-only mode");
}
```

- The flag is a `AtomicBool` on `AppState`, backed by
  `admin_settings.storage.migration_readonly` so it **survives restart**
  (server crashes mid-migration → boots read-only → admin retriggers → still
  safe).
- **Admin operations bypass** as they already do — admin can still exit
  read-only, cancel migration, restart the server.
- **Reads are unaffected**. Users can still browse and download during
  migration.
- **Boot-time clearing**: if boot detects `migration_readonly=true` AND no
  in-flight `backend_migration` row (no `Running`/`Paused`) AND
  `active_backend_name` matches the entry the app booted onto → assume
  successful cutover completed on prior boot, clear the flag. Otherwise leave
  it set; admin knows they still need to finish something.

### Migration flow — atomic + restart-proof

```
1. Admin picks target entry from a dropdown → clicks "Migrate to s3_prod"

2. Backend:
   - Verify target_name exists in AppConfig.storage_entries
   - Verify target_name != active_backend_name (no-op guard; existing
     `is_source_target_identical` refactored to compare NAMES not identity
     strings — but the identity check still runs as a second-line defence
     against the encryption-in-place case)
   - Write admin_settings.storage.migration_readonly = true
   - Trigger `backend_migration` recoverable job with
     params = { source_name: "local_main", target_name: "s3_prod" }

3. Migration runs — target resolved fresh each batch by NAME lookup, so
   the run's params carries only the name. No secrets in params. If the
   process restarts mid-run, the resume path re-resolves the entry from
   env by the persisted name. Env is the source of truth for credentials.

4. On Completed:
   - Write admin_settings.storage.active_backend_name = "s3_prod"
   - Log "Cutover complete — restart the server to switch to the new backend"
   - LEAVE read-only mode on. The app is still running with local_main as
     the live backend; if we lifted read-only now, writes would go to
     local_main even though the DB pointer says s3_prod. Forces the operator
     restart, which resolves the ambiguity.

5. Admin restarts the server:
   - Boot reads active_backend_name = "s3_prod" → live backend is now S3
   - Boot's read-only-clear rule fires (no in-flight migration + active
     matches booted entry) → migration_readonly cleared
   - Server writable, on the new backend. Cutover complete.
```

**Handling in-progress restart (server dies while migration is running)**:
- Boot sweep flips `Running` → `Paused` on the migration row (existing Part
  2 machinery).
- `active_backend_name` unchanged. Boots on old backend.
- `migration_readonly` stays true (in-flight migration → boot-time clear
  rule DOESN'T fire).
- Admin retriggers migration → run_or_resume reads params.target_name →
  resumes from cursor with the same target.

**No secret ever reaches the DB.** Params holds only the entry names.
Credentials stay in env; migration resolves them at each batch by name.

### Concurrent-write safety — read-only for the copy window

The full-quiesce trade-off: users can browse/download during migration but
cannot upload, rename, delete, or share. For a multi-hour migration this is
noticeable; the alternative (dual-write decorator) is genuinely a week of
work (runtime backend swapping, failure-mode reconciliation, `MigrationBlobBackend`
rebuild) and only justified if migration is a routine op. Read-only is the
honest v1 answer — pick a low-traffic window, run the migration, restart.

Read-only is engaged at trigger time and cleared at boot after cutover.
Nothing more elaborate. Dual-write is filed as a future upgrade if operator
demand appears.

### `?storage=<name>` for consistency audits

Once entries are named, `?storage=<name>` becomes the generic "probe any
registered entry" knob on the two tenants that touch a backend:

- `blobs_consistency?storage=<name>` — DB → backend probe.
- `backend_consistency?storage=<name>` — backend → DB probe.

Unspecified → falls through to the active backend (today's behaviour,
preserved).

**Use cases this unlocks**:
- Pre-cutover verification: `blobs_consistency?storage=s3_prod` after
  migration completes — full walk (not a sample) proving target has every
  blob before the .env flip + restart. **Retires `verify_migration`** —
  sample-check becomes redundant when full audit is one click away.
- Pre-decommission verification: `?storage=local_main` after cutover — check
  the old backend still has every blob DB expects before `rm -rf` the local
  `.blobs/`.
- Ad-hoc audit of any registered entry, backup-restore verification, etc.

**Plumbing**:
- `JobRunArgs` gains `storage: Option<String>`.
- `TriggerJobQuery` on the admin trigger endpoint parses `?storage=<name>`.
- `BlobsConsistencyCheck` and `BackendConsistencyCheck` constructors take
  `Arc<StorageSettingsService>` (or a smaller `EntryResolver` port); at run
  start they use `args.storage` to pick the backend, falling back to the
  injected active-backend Arc.
- **Unknown entry**: fail-fast at HTTP layer (`400` before the run ever
  starts) with `known: [local_main, s3_prod]` in the response. Cheaper than
  burning a run row.
- **Combined with `?deep=true`**: legit — "full byte-level integrity audit
  of a named entry, not the live one". Audit log records both.
- Params on the run row records `probed_storage: <name>` (or "active") for
  post-hoc diagnosis.

### Fallback for the "boot fails on missing entry" case

If admin renames an entry in `.env` (or removes one that DB still points
at), boot fails fast with a clear error. Operator has two ways out:

1. Fix `.env` — add the missing entry back OR update `_ENTRIES` to include
   an alternative that DOES exist, plus flip `active_backend_name` before
   restart.
2. **CLI repair flag on the `oxicloud` binary itself**:
   ```
   oxicloud --select-storage <name>
   ```
   Behaviour: parse `.env`, verify `<name>` exists in `_ENTRIES` (fail-fast
   with the available names listed if not), connect to DB, UPDATE
   `admin_settings.storage.active_backend_name`, print confirmation, exit 0.
   Does NOT continue to boot the server — one-shot repair; admin starts
   the server normally afterwards.

The bare-flag on the shipped binary is chosen over a separate `just`
recipe or auxiliary bin because:
- **Docker-friendly**: `docker exec oxicloud oxicloud --select-storage foo`
  — no need to install extra tooling in the container.
- **Systemd-friendly**: can be run as a `ExecStartPre=` one-shot before the
  main service unit.
- **No dep on `just`** being installed (dev-machine tool, not typical prod).
- **Same binary, same env-parse code path**: the repair uses THE SAME
  `.env` parser the server does, so "verified present" means the server
  will succeed on next boot too. No parser drift possible.

The boot-time error message points at this flag explicitly, with the
exact command line filled in.

### Interaction with existing surfaces

- **`/admin/storage` tab** rewritten:
  - Read-only listing of entries (name, backend type, encryption on/off, is
    active).
  - "Migrate to X" dropdown (choose target entry).
  - Migration progress + verify/cancel (existing).
  - Read-only banner when `migration_readonly` is on.
  - The Save form, S3-field editors, .env cutover hint — all deleted (no
    settings edit here anymore).
- **`test_storage_connection`** loses the S3-fields DTO; becomes a
  round-trip probe against a named entry: `POST .../test?storage=<name>`.
- **`verify_migration`** deleted; the "Verify integrity" button on the
  storage tab is rewired to trigger `blobs_consistency?storage=<name>`
  against the migration target (or dropped in favour of the standard
  admin/jobs Run button — TBD in slice 6).

## Slice breakdown

Single PR is too big; splitting into a coherent sequence. Slices 1-2 are
foundational; the rest layer on top independently within reason.

| # | Slice | Depends on | Rough size |
|---|---|---|---|
| 1 | Config parser: `OXICLOUD_STORAGE_ENTRIES` + per-entry vars + `_ENCRYPTION_KEY` → `Vec<NamedStorageEntry>` on `AppConfig`. Legacy synthesis for empty `_ENTRIES` (default entry from legacy flat vars). | — | 1 day |
| 2 | Boot: read `active_backend_name` from `admin_settings`; look up entry; build `blob_backend` via a shared `build_entry_backend(&NamedStorageEntry)` factory (wraps encryption decorator when key present). Fail-fast on missing entry with actionable message. | 1 | ~half day |
| 3 | Migration handler rewrite: params carry `target_name` only. Handler resolves target by name from `storage_settings.build_entry_backend(name)`. `is_source_target_identical` guard refactored to compare NAMES first; keeps physical-identity check as second-line refusal (with encryption-differs-specific message). Retire the migration DTO S3-field body. | 1, 2 | ~half day |
| 4 | Global `migration_readonly` flag on `AppState`, backed by `admin_settings.storage.migration_readonly`. One clause added to `PgAclEngine::check_inner`. Boot-time clear rule (no in-flight + active matches booted → clear). | 2 | ~half day |
| 5 | Cutover state machine: on migration `Completed`, write `active_backend_name = target_name`, keep read-only on. Boot on new backend after operator restart. | 4 | ~half day |
| 6 | Admin storage tab rewrite: list entries, show active, migrate dropdown, read-only banner. Delete Save form + S3 field editors + .env cutover hint. | 1, 3, 4 | 1 day |
| 7 | `?storage=<name>` on `blobs_consistency` + `backend_consistency`. `JobRunArgs.storage` plumbing, `TriggerJobQuery.storage`, entry-resolver at run start, params records probed name. Retire `verify_migration` + its DTO + its route + its handler. | 1, 3 | 1 day |
| 8 | `oxicloud --select-storage <name>` bare-flag repair command on the main binary. Parses `.env`, verifies entry exists, UPDATEs DB, exits. Boot-time missing-entry error message points at it. See §Fallback. | 2 | ~quarter day |

**Total: ~5-6 days end to end.** Slices 6 and 7 can proceed in parallel with
each other once 1-5 land. Slice 8 is an ops nicety, could ship whenever.

## Verification

Per slice, plus these end-to-end scenarios in Hurl:

1. **Fresh install, no `_ENTRIES`**: boot uses synthesized `default` entry from
   legacy vars, `active_backend_name` unset. `GET /admin/settings/storage`
   returns one entry, active. No cutover UI.
2. **Two entries, no active set**: boot picks first in `_ENTRIES`, logs it,
   proceeds. Admin panel shows both entries with the picked one marked active.
3. **Change active without migration** (rarely useful but must be safe):
   `POST /admin/settings/storage/active-name` (or however the pointer is
   exposed) updates the row; next restart boots on the new entry. Existing
   blobs on the new entry NOT verified — operator's problem, but
   `blobs_consistency` catches the drift on next run.
4. **Migration happy path**: two entries, entry A active, migrate to B.
   Read-only comes on. Copy runs. `active_backend_name` flips to B on
   completion. Read-only stays on until restart. After restart, live is B,
   read-only cleared.
5. **Restart mid-migration**: kill server after checkpoint N. Boot: row Paused,
   `active` still A, `migration_readonly` still true. Admin retriggers →
   resumes from checkpoint N against target B (resolved by name from params).
   Complete → pointer flips → restart → live is B.
6. **In-place encryption rotation refused**: two entries, same S3 bucket,
   different encryption keys. Trigger migration → refuses with the specific
   error message pointing at the encryption case and the two-step workaround.
7. **`?storage=<name>` on blobs_consistency**: run against `s3_prod` before
   cutover. Full walk, `probed_storage` in run row. Then cutover, then rerun
   against `local_main` — verifies old backend still has everything.
8. **Unknown storage name**: `POST /admin/jobs/blobs_consistency/trigger?storage=nope`
   → 400 with known-names list. No run row created.
9. **Missing entry at boot**: `active_backend_name = "gone"` but `_ENTRIES`
   doesn't include it → boot aborts with the specific message pointing at
   `oxicloud --select-storage <name>` (with the available names filled in).
   Re-run the binary with `--select-storage local_main` → verifies + updates
   DB + exits 0. Restart the server → boots cleanly on `local_main`.
10. **Encryption key invalid**: `OXICLOUD_STORAGE_<N>_ENCRYPTION_KEY=badbase64`
    → boot aborts with entry name + reason (not valid base64 / wrong length).
11. **Legacy vars alongside `_ENTRIES`**: set `_ENTRIES=foo` AND leave a
    stale `OXICLOUD_S3_BUCKET=...` in `.env`. Boot aborts with the full
    list of legacy vars detected, tells the admin to remove them (or move
    them into `OXICLOUD_STORAGE_<NAME>_S3_BUCKET` form). Removing the
    legacy var → next boot succeeds.
12. **Legacy synthesis path**: `_ENTRIES` unset, `OXICLOUD_STORAGE_BACKEND=s3`
    + `OXICLOUD_S3_BUCKET=...` set. Boot synthesizes one entry named
    `default` from those vars. `storage_entries.len() == 1`, name is
    `default`, backend is S3, config carries the flat-var values.

## Out of scope

- **Dual-write decorator / zero-downtime migration**: read-only is v1;
  dual-write is filed under "if operator demand appears". Would rebuild
  `MigrationBlobBackend` (deleted in the recoverable-migration PR — retained
  in git for the same reason).
- **In-place encryption key rotation** (same-bucket, different key): refused
  by the identity guard; workaround is two-step via temp bucket. Proper fix
  (per-generation object naming) is documented inline in the Encryption
  section above; deferred until real demand appears.
- **Runtime backend hot-swap without restart**: not attempted. Requires
  `Arc<RwLock<Arc<dyn BlobStorageBackend>>>` indirection at every call site
  plus per-request coordination; huge blast radius. Read-only + restart is
  the honest answer.
- **Per-user or per-drive storage backends**: everything in this plan is
  server-scoped. If per-drive storage becomes a real need, the named-entry
  registry is the right substrate but `blob_backend` on AppState becomes a
  `EntryResolver` and every call site changes. Not now.
- **DB storage config UI**: retired. Admin panel is a controller (list +
  test + migrate + audit), never a persister. If persisted per-entry knobs
  become a need (retention days per entry, quota per entry, …), those live
  in DB rows keyed by entry name — a small extension, not a return to the
  old model.
- **Secret encryption in DB**: `admin_settings.storage.*` secret rows are
  deleted along with `save_storage_settings`. If the migration params
  approach ever grows to store secrets (e.g., a future "supply the target
  creds inline for one-off migrations"), those get encrypted at rest with
  a KMS-provided key. Not needed for this plan — params holds only names.

## Related memory notes

- `feedback_no_abbreviated_env_vars` — full-word env var names
  (`OXICLOUD_STORAGE_local_main_S3_ENDPOINT_URL`, not
  `OXICLOUD_STORAGE_local_main_S3_EP`).
- `feedback_config_file_overrides_shell` — `dotenvy` behaviour on explicit
  config path. Matters when `_ENTRIES` values are loaded from `--config` vs
  shell.
- `project_admin_middleware_layer` — admin ops bypass read-only via existing
  middleware; the AuthZ short-circuit added in slice 4 doesn't need any
  new admin carve-out.
- `bug_drive_rename_editor_can_do_it` — reminder that
  `PgAclEngine::check_inner` is where write-permission short-circuits live;
  the new global read-only clause lands next to the per-drive one.
- `docs/plan/job-registry.md` Part 2 — recoverable-run engine that
  `backend_migration` runs on; `params` field, resume semantics, boot
  sweep.
