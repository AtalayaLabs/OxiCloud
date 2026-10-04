# Admin Settings

OxiCloud exposes an admin API for runtime configuration, dashboard stats, and user administration. All routes live under `/api/admin` and require an authenticated admin JWT.

## Settings Endpoints

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/admin/settings/oidc` | Read current OIDC settings |
| `PUT` | `/api/admin/settings/oidc` | Save OIDC settings |
| `POST` | `/api/admin/settings/oidc/test` | Test provider connectivity |
| `GET` | `/api/admin/settings/general` | Read general server settings |
| `GET` | `/api/admin/notify/info` | Read the job-alerting policy: severity floor, wired channels, mail recipients |

The OIDC runtime UI complements the base configuration described in [OIDC / SSO](/config/oidc) and the provider samples in [OIDC Config Examples](/config/oidc-config-examples).

### Job alerting

`GET /api/admin/notify/info` answers the question the other diagnostics
cannot: *will I actually be told?* It reports `min_severity`, the channels
that were **built** rather than merely configured, and the parsed mail
recipients. The floor is the part worth reading — the shipped default
admits only `data_loss`, so a healthy relay and a configured webhook can
coexist with complete silence about drift, which looks exactly like a
broken channel.

The transports keep their own tests, and they test transports:
`POST /api/admin/smtp/test` sends to an address you type in, and
`POST /api/admin/webhook/test` posts a synthetic alert to the configured
endpoint. Neither exercises the chain in between — the transition diff and
the severity threshold — because both inject an alert past it.

For that, trigger the **`notify_selftest`** job (admin jobs panel, or
`POST /api/admin/jobs/notify_selftest/trigger`). It records one synthetic
finding, so a real run drives the whole path: finding row → diff against
the previous completed run → threshold → every configured channel.
Read-only, on demand only, never scheduled. Parameters:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `severity` | `data_loss` | `data_loss`, `inconsistent` or `anomaly`. The default is the worst one so the test clears the default floor; anything milder would deliver nothing on a correctly configured instance, which reads as the failure being tested for. |
| `kind` | `selftest_finding` | The finding kind. Alerts fire per kind and only on a change, so re-running with the same kind correctly sends nothing the second time. |
| `findings` | `1` | How many to record. `0` records none, so the next run reports the previous kind as **cleared** — the way to exercise the resolution alert. |
| `stall` | `0` | Stop with a retryable error for this many attempts before completing, as a backend outage would. Each subsequent trigger resumes the same run: with `stall=2` the first attempt alerts, the second is silent (same reason, already reported) and the third completes and sends the all-clear. |

An unrecognised `severity` fails the run rather than recording a finding
that would sit below every threshold.

### Reclaiming orphaned storage now

Nothing unlinks backend objects directly. Both discovery paths **enqueue**
into `storage.pending_actions`, and one job drains it — so "delete this
orphan" is always two steps, and the jobs cover different orphan classes:

| job | what it reclaims |
|---|---|
| `backend_consistency?repair=true` | Backend objects with **no DB row**. Invisible to the DB-driven GC, which is why they can only be queued. |
| `dedup_gc?force=true` | The complementary class: rows at `ref_count = 0` still inside their **orphan grace window**. `force` skips the wait and queues their bytes now. |
| `backend_reclaim` | The only thing that unlinks. Drains the queue, re-verifying each object under a row lock first — so content that became referenced again is never deleted. |

To reclaim everything immediately, run all three in that order: enqueue
both sources, then drain once. For backend objects alone, the first and
last suffice — `dedup_gc` cannot see an object with no row.

`backend_reclaim` is scheduled every 300 s, so **doing nothing also
works**; the sequence above only removes the wait.

Re-running `backend_consistency` before the drain catches up is safe and
will not re-report the same orphans: it reads the queue and counts them
as `already_queued` in the run's outcome rather than raising a finding.
The exception is deliberate — an entry the drain has **parked** (attempts
exhausted) still surfaces as `orphan_blob_reclaim_parked`, because
nothing retries it without a human. Check that entry's `last_error`,
fix the cause, then un-park it.

### Why a run that stopped is worth an alert of its own

A run that gives up records two columns on `jobs.recoverable_runs`:
`error_reason`, a stable key (`backend_unavailable`, `backend_timeout`,
`job_failed`, `server_restart`), and `error_message`, the full error
chain. Alerting fires on the presence of the former.

The distinction that makes this work without special cases: **an
operator pause records neither.** Both an operator pause and an
environment failure land as `Paused` in `status`, so "stopped with a
reason" is the only reliable discriminator — and it means a human
clicking Pause never pages anyone.

Worth knowing about these:

- **Graded `anomaly`**, so the default `data_loss` floor does not
  deliver them. The grade is deliberate: the severity scale measures how
  bad the *data* is, and a stopped run makes no claim about data.
  Labelling "I could not check" as `data_loss` would corrupt the signal
  that has to stay trustworthy.
- **Once per run, not once per attempt.** A paused run gets auto-resumed;
  without de-duplication a backend down for a day would mail on every
  resume, and a channel that repeats itself is one that gets muted.
- **The all-clear is automatic.** When the run eventually completes,
  whoever heard about the stall hears that it ended.

`error_reason` is exposed on the run DTO and rendered in the jobs panel
beside the message. Switch on it, never on `error_message`, which is
prose and free to be reworded.

## Dashboard Endpoint

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/admin/dashboard` | Read server statistics and feature state |

Typical dashboard fields include:

- server version
- whether auth and OIDC are enabled
- whether quotas are enabled
- total, active, and admin user counts
- quota usage totals and percentage

## User Management Endpoints

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/admin/users` | List users |
| `GET` | `/api/admin/users/{id}` | Get one user |
| `DELETE` | `/api/admin/users/{id}` | Delete a user |
| `PUT` | `/api/admin/users/{id}/role` | Change role |
| `PUT` | `/api/admin/users/{id}/active` | Activate or deactivate a user |
| `PUT` | `/api/admin/users/{id}/quota` | Update a storage quota |

### Built-in safety guards

- Admins cannot delete their own account
- Admins cannot change their own role
- Admins cannot deactivate themselves

## OIDC Settings Priority

When the same setting exists in multiple places, OxiCloud resolves it in this order:

1. Environment variables such as `OXICLOUD_OIDC_*`
2. Values stored in the admin settings table
3. Built-in defaults

If a value is overridden by environment variables, the admin API can expose that in the response so operators know why a saved value is not taking effect.

## Test Connection Example

```json
{
  "issuer_url": "https://keycloak.example.com/realms/main"
}
```

Successful responses include discovered endpoints such as the authorization endpoint, token endpoint, and userinfo endpoint.

## Storage & Migration

The admin storage tab operates on the **named storage entries** declared in `.env` (see [Storage Entries](/config/env#storage-entries-multi-entry-recommended)). The set of entries is immutable per-deploy — adding or removing one requires a server restart. Runtime behaviour is driven by a single DB row that names which entry is currently active.

### Endpoints

| Method | Path | Description |
| --- | --- | --- |
| `GET`  | `/api/admin/settings/storage`             | List entries + active pointer + read-only flag + basic stats |
| `POST` | `/api/admin/settings/storage/test`        | Reachability + round-trip test against the currently-effective backend |
| `POST` | `/api/admin/storage/migration/start`      | Trigger a cross-entry migration. Body: `{"target_name": "<entry>"}` |
| `POST` | `/api/admin/storage/migration/pause`      | Cooperative cancel — handler yields at the next batch boundary |
| `POST` | `/api/admin/storage/migration/resume`     | Resume a paused run (target read from `params.target_name`, no body needed) |
| `GET`  | `/api/admin/storage/migration`            | Poll the current run's progress |

Runs are recoverable — status, cursor, and per-blob failure findings all live in `jobs.recoverable_runs` / `jobs.run_findings`. The same run history is browsable via `GET /api/admin/jobs/backend_migration/runs`.

### Cutover flow (moving the active pointer)

1. Declare the target entry in `.env` and restart so `OXICLOUD_STORAGE_ENTRIES` picks it up.
2. Admin storage tab → pick the target from the dropdown → **Start migration**. The server engages global read-only mode (writes refused across the whole app; reads keep working), then copies blobs from source → target.
3. On `Completed`, the server writes `admin_settings.storage.active_backend_name = <target>`. Read-only stays ON — writes on the OLD backend would strand data now that the pointer says the new one is active.
4. **Operator restarts the server.** Boot picks the new active entry, and the boot-clear rule drops the read-only flag (`no in-flight run + booted-entry matches DB pointer`). Server writable again, on the new backend.

### Repair flag — pointer / entry drift

If an entry is renamed or removed from `.env` while the DB pointer still names the old one, boot aborts with a clear error pointing at:

```
oxicloud storage select <name>
```

This one-shot repair command re-runs the same env-parse the server does at boot, verifies `<name>` is declared in `OXICLOUD_STORAGE_ENTRIES`, updates `admin_settings.storage.active_backend_name` in the DB, and exits. Operator then restarts normally. See [Environment Variables — Storage Entries](/config/env#storage-entries-multi-entry-recommended) for the model, and [`oxicloud --help`](https://github.com/oxicloud/oxicloud/blob/main/src/main.rs) for the full flag list.

### Auditing entries other than the active one

`backend_consistency` (a recoverable job on the Jobs tab) accepts `?storage=<name>` to audit any declared entry — not just the live one. Use this to verify a migration target before cutover, or to audit an old backend after cutover but before decommissioning:

```
POST /api/admin/jobs/backend_consistency/trigger?storage=<name>
```

Add `?deep=true` to also read every blob back and re-hash it, which catches silent bit-rot. That is a full read of the entry and can take hours.

`blobs_consistency` does *not* accept `?storage=<name>`: it only reads the database, so there is no entry for it to scope.

Unknown names 400 at the HTTP layer.

## Data Storage

Runtime settings are stored in `auth.admin_settings`.

```sql
CREATE TABLE IF NOT EXISTS auth.admin_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    category   TEXT NOT NULL,
    is_secret  BOOLEAN DEFAULT FALSE,
    updated_by VARCHAR(36),
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);
```

## Related Pages

- [OIDC / SSO](/config/oidc)
- [OIDC Config Examples](/config/oidc-config-examples)
- [Environment Variables](/config/env)