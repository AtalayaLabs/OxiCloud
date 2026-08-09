#!/usr/bin/env bash
# Boot a clean test DB then start OxiCloud (passed as arguments).
# Used by playwright.config.ts as the webServer command so that both
# `npm test` and `npx playwright test` always start from an empty database.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OXICLOUD_STORAGE_PATH="$REPO_ROOT/tests/e2e/storage"

# Mirror everything (these markers + the server's own stdout/stderr) to a log
# file as well as the console. Playwright captures the webServer's stdout, but
# in CI that capture isn't always surfaced in the job log — the file is, via an
# `if: always()` "print server startup log" step in ci.yml. The final
# `exec "$@"` below inherits these redirected fds, so the server's output is
# tee'd too, while the process still replaces this shell (Playwright tracks the
# PID for teardown).
SERVER_LOG="$REPO_ROOT/tests/e2e/server-startup.log"
exec > >(tee "$SERVER_LOG") 2>&1

mark() { echo "[start-server $(date -u +%H:%M:%S)] $*"; }

mark "repo_root=$REPO_ROOT"
mark "server binary args: $*"
if [[ -n "${1:-}" && "$1" != "cargo" ]]; then
  ls -la "$1" 2>&1 || mark "WARNING: server binary '$1' not found"
fi
mark "DATABASE_URL=${DATABASE_URL:-<unset>} OXICLOUD_SERVER_PORT=${OXICLOUD_SERVER_PORT:-<unset>}"

# ensure storage is empty before starting
mark "wiping $OXICLOUD_STORAGE_PATH to ensure clean startup"
rm -rf "$OXICLOUD_STORAGE_PATH"
mkdir -p "$OXICLOUD_STORAGE_PATH"

# Spawn database
mark "spawning test database…"
bash "$REPO_ROOT/tests/common/spawn-db.sh"
mark "database ready; starting server…"

# Point `--config` at an INTENTIONALLY EMPTY file. Two reasons:
#
#   1. Blocks the fallback `dotenvy::dotenv()` probe of `$CWD/.env`
#      in main.rs — otherwise a developer's local `.env`
#      (typical: `OXICLOUD_METRICS_LISTEN=127.0.0.1:9090`) leaks
#      into the test server via CWD auto-load and clashes with
#      anything the dev is already running.
#
#   2. An empty file has nothing to override — so every var
#      Playwright loaded into `webServer.env` from server.env AND
#      every per-suite override on top (SERVER_PORT, STORAGE_PATH,
#      RUST_LOG, OPAQUE_MODE, DPOP_MODE, …) reaches the server
#      intact. We can't use `--config server.env` here because
#      `dotenvy::from_filename_override` would clobber Playwright's
#      per-suite overrides for the keys it shares with the file.
#
# See tests/common/empty.env for the header explaining this.
CONFIG_PATH="$REPO_ROOT/tests/common/empty.env"

# `cargo run` needs `--` to separate cargo's own flags from the
# binary's — a direct-binary invocation takes them raw. First arg
# tells us which we're in.
if [[ "${1:-}" == "cargo" ]]; then
  exec "$@" -- --config "$CONFIG_PATH"
else
  exec "$@" --config "$CONFIG_PATH"
fi
