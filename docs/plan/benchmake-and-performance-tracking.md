# OxiCloud Load Test Scaffolding (K6) — Scenario A

## Context

OxiCloud needs a repeatable way to **detect performance regressions** as features land. The current `tests/load/` directory is empty except for a README asking K6-vs-drill. Scenario A focuses on a single user with many subfolders and cascading systems (folder ops, ReBAC permission groups, nested subject groups) — measuring p50/p95/p99 to identify inflections when new features are added. Scenario B (many concurrent users) is deferred.

**Decisions already locked in via conversation:**
- K6 (Go-based, JS scripting in goja) over Rust testers — client overhead is dwarfed by server latency for these scenarios; regression deltas only require client *consistency*, not raw speed.
- **Two scenario tiers**: a **fast smoke** for PRs (~1 min, no regression gate — just verifies the harness still runs) and **long scenarios** for nightly + manual (regression-gated).
- **Hybrid seeding**: bulk fixtures via direct SQL (`cargo run --bin load-seed`); the resources each scenario actively touches go through REST so the measured path is realistic.
- **Result storage**: `baseline/baseline.json` committed in repo (p50/p95/p99 + tolerance per metric); per-run raw JSON kept locally / CI artifact, gitignored. Baseline updates are deliberate PRs.
- **No PR gating on long suite** — nightly on main + manual `workflow_dispatch` only.

## Approach

### 1. Directory layout

```
tests/load/
  README.md
  test.env                         # base_url=http://localhost:8088, admin creds
  run.sh                           # full suite: spawn-db → seed → server → k6 → compare → cleanup
  smoke.sh                         # smoke only: same shape, runs scenarios/smoke.js, no gate
  compare.mjs                      # diffs k6 summary.json vs baseline/baseline.json
  lib/
    auth.js                        # login() → bearer token (POST /api/auth/login)
    http.js                        # base URL + auth header helpers
    metrics.js                     # custom k6 Trends, naming convention <scenario>.<op>
  scenarios/
    smoke.js                       # 1 VU, 1 iter — login, create folder, upload, list, delete
    folder_cascade.js              # list/move/copy/trash on depth-8 fanout-5 tree
    share_cascade_rebac.js         # grant a folder, fetch N descendants as grantee
    subject_group_nested.js        # nested-group chain (depth 3), grant via group, fetch
  baseline/
    baseline.json                  # committed; { "<scenario>.<op>": {p50,p95,p99,tolerance_pct} }
  results/
    .gitkeep                       # raw runs, gitignored
```

Mirrors the `tests/api/` shell pattern (run.sh, test.env, separate server port). Uses port **8088** to avoid colliding with api-test on 8087.

### 2. Rust bulk seeder — `src/bin/load-seed.rs`

New binary registered in `Cargo.toml` alongside `generate-openapi`
(the historical `migrate-nfc-filenames` bin has since been folded into
`oxicloud migrate nfc-filenames` — see `docs/plan/bundled-binary.md` § 1b).

**CLI:**
```
cargo run --bin load-seed -- \
  --depth 8 --fanout 5 --files-per-leaf 10 \
  --extra-users 20 --group-depth 3 --group-fanout 5
```

**Inserts via sqlx (one transaction per phase):**
- 1 admin + N extra users into `auth.users` (Argon2id with light params m=16384,t=1,p=1; same params as `src/infrastructure/services/password_hasher.rs` test path).
- One shared 0-byte blob row in `storage.blobs` with `ref_count = total_files`.
- Folder tree in `storage.folders`, **inserted level-by-level** so the `trg_folders_path` BEFORE-INSERT trigger can compute `path`/`lpath` from the parent chain.
- Files in `storage.files`, all referencing the shared blob hash for dedup.
- Subject groups in `auth.subject_groups` + membership rows in `auth.subject_group_members` (XOR `member_user_id`/`member_group_id`), forming a nested chain `G_root → G_mid → G_leaf → users`.
- ReBAC grants in `storage.access_grants` for a known set of test resources (used by `share_cascade_rebac.js`).

Reuses existing connection config: reads `DATABASE_URL` from env, same shape as `tests/common/server.env`.

**Critical gotchas surfaced during exploration:**
- Tree-ETag triggers (`folders_bump_tree_etag_*`) are STATEMENT-level (per commit `4200209d`), so bulk inserts fire them once per statement — safe.
- ReBAC cascade is computed in the application layer (`pg_acl_engine`), not at DB level. Seeder just inserts grant rows; the cascade is the *server* behavior we want to measure.
- `auth.subject_groups.name` is CITEXT, max 64, RFC-5321 local-part shape — generated names use `g_NNN` pattern to stay valid.

### 3. K6 scenarios

All scenarios load `baseline.json` at startup and set `thresholds` dynamically from it (`http_req_duration{op:x}: p(95)<baseline.x.p95 * (1 + tolerance_pct/100)`). This makes K6 itself fail the run on regression, *and* `compare.mjs` produces the human-readable diff.

**Endpoint contracts (confirmed by exploration):**
- Login: `POST /api/auth/login` body `{username, password}` → `{access_token, ...}`.
- Folder ops: `POST /api/folders`, `GET /api/folders/{id}/resources` (cursor-paginated; replaced the removed `/contents` + `/contents/paginated`), `PUT /api/folders/{id}/move`, `POST /api/batch/folders/copy`, `DELETE /api/folders/{id}`. Handlers in `src/interfaces/api/handlers/folder_handler.rs` and `batch_handler.rs`.
- Grants: `POST /api/grants` (subject `{type, id|email}`, resource `{type, id}`, `role` or `permissions[]`), `GET /api/grants?resource_type=folder&resource_id=…`. Handler `src/interfaces/api/handlers/grant_handler.rs`.
- Groups: `POST /api/groups` (admin), `POST /api/groups/{id}/members` body `{user_id}` or `{group_id}`. Handler `src/interfaces/api/handlers/subject_group_handler.rs`. Nesting limit is 8 — `--group-depth 3` is well inside.
- Files: multipart `POST /api/files/upload` with fields `folder_id` + `file`.

**Per-scenario metric naming** (`metrics.js` enforces): `<scenario>.<op>` → e.g. `folder_cascade.list_depth8`, `share_cascade_rebac.fetch_as_grantee`, `subject_group_nested.grant_via_chain3`.

**Cold/warm split**: first iteration tagged `cold` (its metrics flow to `<scenario>.<op>_cold`), rest are warm. Single first iter avoids polluting warm metrics with one-shot connection setup.

### 4. Runner scripts

**`run.sh`** (mirrors `tests/api/run.sh` shape):
1. `spawn-db.sh` (reused from `tests/common/`)
2. `init-test-schema.sh` (reused)
3. `cargo run --bin load-seed -- <args>` against the test DB
4. Start `target/<profile>/oxicloud` on port 8088 with `OXICLOUD_STORAGE_PATH=tests/load/storage`
5. `wait_for_http http://localhost:8088/ready`
6. `k6 run --summary-export=results/$(date +%s).json scenarios/folder_cascade.js scenarios/share_cascade_rebac.js scenarios/subject_group_nested.js`
7. `node compare.mjs results/<latest>.json baseline/baseline.json` → exits non-zero on regression
8. `trap cleanup EXIT` tears down server + DB

**`smoke.sh`**: same shape but only runs `scenarios/smoke.js`, **skips the seeder** (smoke creates its own minimal data), **skips `compare.mjs`**. Goal is harness liveness, not regression.

### 5. `compare.mjs`

Plain Node script (no deps, just `fs` + `process`). Reads two JSONs, walks every metric in baseline, computes:
```
delta_pct = (current - baseline) / baseline * 100
```
Prints a human table:
```
folder_cascade.list_depth8.p95: 142ms → 198ms (+39%)  REGRESSION (tolerance 10%)
share_cascade_rebac.grant.p95:  60ms → 58ms  (-3%)    ok
```
Exit 1 if any metric exceeds tolerance. Exit 0 otherwise. Missing metrics in current = exit 1 (suite drift); new metrics in current = warn only.

### 6. Baseline format

```json
{
  "folder_cascade.list_depth8": { "p50": 12.0, "p95": 45.0, "p99": 80.0, "tolerance_pct": 10 },
  "share_cascade_rebac.fetch_as_grantee": { "p50": 18.0, "p95": 55.0, "p99": 100.0, "tolerance_pct": 10 },
  ...
}
```

Values are placeholders — first run on a stable machine seeds real numbers. Updated only via the `load-baseline` recipe (which rewrites `baseline.json` from the latest run) and committed deliberately as `chore(load): accept new baseline for <reason>`.

### 7. Justfile recipes (added to existing `justfile`)

```
load:               # full suite — nightly + manual
    bash tests/load/run.sh

load-smoke:         # fast harness check
    bash tests/load/smoke.sh

load-baseline:      # rerun, overwrite baseline from latest result
    bash tests/load/run.sh
    cp tests/load/results/$(ls -t tests/load/results/*.json | head -1) tests/load/baseline/baseline.json

load-seed:          # standalone seeder for local poking
    cargo run --bin load-seed -- --depth 8 --fanout 5 --files-per-leaf 10
```

Matches existing recipe naming (`test-*`, `front-*`, `api-test`).

### 8. CI workflows (stubs)

- **`.github/workflows/load-nightly.yml`** — cron `0 3 * * *` UTC + `workflow_dispatch`; runs `just load` on `ubuntu-latest` initially with a `# TODO: replace with self-hosted runner — shared GH runners produce noisy results for regression detection` comment; uploads `tests/load/results/*.json` as artifact; on `compare.mjs` non-zero, opens an issue using `peter-evans/create-issue-from-file` with the diff report.
- **`.github/workflows/load-smoke.yml`** — on every PR to `main`; runs `just load-smoke`; ~1 min budget; no regression gate, just "harness builds & runs."

### 9. Critical files to create / modify

**Create:**
- `tests/load/README.md`
- `tests/load/test.env`
- `tests/load/run.sh`, `smoke.sh`
- `tests/load/compare.mjs`
- `tests/load/lib/auth.js`, `http.js`, `metrics.js`
- `tests/load/scenarios/smoke.js`, `folder_cascade.js`, `share_cascade_rebac.js`, `subject_group_nested.js`
- `tests/load/baseline/baseline.json` (placeholder values)
- `tests/load/results/.gitkeep`
- `src/bin/load-seed.rs`
- `.github/workflows/load-nightly.yml`, `load-smoke.yml`

**Modify:**
- `Cargo.toml` — add `[[bin]] name = "load-seed" path = "src/bin/load-seed.rs"` after the `generate-openapi` entry (the `migrate-nfc-filenames` bin referenced in earlier drafts has been folded into `oxicloud migrate nfc-filenames`)
- `justfile` — append four `load*` recipes
- `.gitignore` — add `tests/load/results/*.json` and `tests/load/storage/`

### 10. Reused existing utilities

- `tests/common/spawn-db.sh`, `stop-db.sh`, `init-test-schema.sh`, `server.env` — reused as-is by `run.sh` / `smoke.sh`.
- Argon2id parameters from `src/infrastructure/services/password_hasher.rs` test path — mirrored in `load-seed.rs` so test users can log in via the real auth flow.
- DB schema migrations (`migrations/`) — applied via the existing `init-test-schema.sh`; no schema changes needed.

## Verification

1. **Build**: `cargo build --bin load-seed` succeeds; `cargo clippy --all-features --all-targets -- -D warnings` stays green.
2. **Seeder works standalone**: `just db && just load-seed` populates `auth.users`, `storage.folders` (depth-8 tree visible via `SELECT count(*) FROM storage.folders WHERE user_id = …`), `auth.subject_groups`, `storage.access_grants`.
3. **Smoke runs locally**: `just load-smoke` — exits 0 in under ~90 s, leaves no orphan postgres/oxicloud processes.
4. **Full suite runs locally**: `just load` — k6 prints per-scenario p50/p95/p99; `compare.mjs` prints the diff table; since `baseline.json` is empty placeholders, expect a "REGRESSION" stub-report that the user reviews before running `just load-baseline` to accept the first real baseline.
5. **Baseline workflow**: after first `just load`, `just load-baseline` rewrites `baseline.json`; subsequent `just load` exits 0 (within tolerance).
6. **CI smoke**: trigger `load-smoke.yml` on a draft PR; confirm it completes under the time budget and uploads no artifacts on success.
7. **CI nightly**: manually trigger `load-nightly.yml` via `workflow_dispatch`; confirm artifact upload and (since no real baseline yet) confirm issue creation works.

## Out of scope

- Real baseline values (first run on user's hardware seeds them).
- Self-hosted runner setup (workflow stub uses `ubuntu-latest` with a TODO comment).
- Scenario B (many concurrent users) — deferred per user request.
- Goose / Rust-tester alternative — deferred until scenario B demonstrates K6 saturation issues.
- Additional scenarios (CalDAV / CardDAV / WebDAV / search depth) — the scaffolding makes them additive: drop a new file in `scenarios/`, add its metric names to `baseline.json`.
