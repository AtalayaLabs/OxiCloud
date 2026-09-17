# TOML configuration file

OxiCloud is configured through `OXICLOUD_*` environment variables ([env.md](./env.md)). A TOML file is a second way to supply the same settings, for operators who would rather keep one annotated, structured, version-controlled file than a flat list of exports.

```bash
oxicloud --config /etc/oxicloud/oxicloud.toml
OXICLOUD_CONFIG=/etc/oxicloud/oxicloud.toml oxicloud
```

Both forms accept either a TOML file (recognised by the `.toml` extension) or a `.env` file, as before. `OXICLOUD_CONFIG` is also honoured by the `opaque`, `migrate` and `storage` subcommands, which cannot parse the flag.

A copy-ready starting point ships as [`oxicloud.example.toml`](./oxicloud.example.toml).

## The mapping

**A key is its environment variable with the `OXICLOUD_` prefix dropped, lower-cased.** There is no second vocabulary to learn and env.md documents both spellings at once:

| TOML | Environment variable |
|---|---|
| `[server]` → `server_port = 8086` | `OXICLOUD_SERVER_PORT` |
| `[auth]` → `jwt_secret = "…"` | `OXICLOUD_JWT_SECRET` |
| `[database]` → `db_max_connections = 20` | `OXICLOUD_DB_MAX_CONNECTIONS` |

The six top-level tables — `server`, `database`, `auth`, `storage`, `features`, `integrations` — group the settings; each variable belongs to exactly one, and filing it under a different one is an error that names the right table.

Nested tables are a grouping aid: their segments join back into the same variable, so the two spellings below are identical and you can pick whichever reads better.

```toml
[auth.rate_limit]
login_max = 10          # OXICLOUD_RATE_LIMIT_LOGIN_MAX

[auth]
rate_limit_login_max = 10
```

## Values

| TOML | Becomes |
|---|---|
| `"text"` | the string |
| `8086`, `1.5` | the number, as written |
| `true` / `false` | `true` / `false` |
| `["password", "oidc"]` | `password,oidc` — the comma-separated form every list-valued setting already parses |

## Multi-entry storage

Storage entries carry an operator-chosen label, which the table name supplies verbatim (case included):

```toml
[storage.entries.local_main]
backend = "local"
root_dir = "/var/lib/oxicloud/blobs"
```

sets `OXICLOUD_STORAGE_local_main_BACKEND` and `OXICLOUD_STORAGE_local_main_ROOT_DIR`. The keys allowed inside an entry are `backend`, `root_dir`, `encryption_key`, `s3_*` and `azure_*`.

## Unknown keys stop the boot

The difference that matters in practice: a key that does not name a real setting fails the start, with the variable it would have been.

```
failed to load config /etc/oxicloud/oxicloud.toml: `auth.jwt_secrets` is not a setting (it would be `OXICLOUD_JWT_SECRETS`)
```

An environment variable with the same typo is simply never read — the server boots, quietly using the default, and the mistake surfaces much later as behaviour nobody can explain.

## Precedence

```
compiled defaults  <  config file  <  environment
```

The file is read before anything reads a setting, and it fills in only what the environment does not already carry. A variable set for the process therefore wins — which is what `-e` on a container, a systemd `Environment=` line or a one-off `OXICLOUD_…=… oxicloud` is for. The boot line says how many settings yielded that way:

```
loaded 34 settings from /etc/oxicloud/oxicloud.toml (2 overridden by the environment)
```

Without `--config` / `OXICLOUD_CONFIG` the dev-convenience probe for `./.env` still runs, with the same precedence.

## What it does not cover

Settings that are not environment variables are not in the file either — there is exactly one configuration surface, and this is a second way to write to it. Tuning knobs that only exist as `AppConfig` defaults stay compile-time constants.
