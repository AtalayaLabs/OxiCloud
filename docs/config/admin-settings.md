# Admin Settings

OxiCloud exposes an admin API for runtime configuration, dashboard stats, and user administration. All routes live under `/api/admin` and require an authenticated admin JWT.

## Settings Endpoints

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/api/admin/settings/oidc` | Read current OIDC settings |
| `PUT` | `/api/admin/settings/oidc` | Save OIDC settings |
| `POST` | `/api/admin/settings/oidc/test` | Test provider connectivity |
| `GET` | `/api/admin/settings/general` | Read general server settings |

The OIDC runtime UI complements the base configuration described in [OIDC / SSO](/config/oidc) and the provider samples in [OIDC Config Examples](/config/oidc-config-examples).

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

`blobs_consistency` and `backend_consistency` (recoverable jobs on the Jobs tab) accept `?storage=<name>` to probe any declared entry — not just the live one. Use this to verify a migration target before cutover, or to audit an old backend after cutover but before decommissioning:

```
POST /api/admin/jobs/blobs_consistency/trigger?storage=<name>
```

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