#!/usr/bin/env bash
# Boot a clean test DB then start OxiCloud serving the *SvelteKit* SPA
# (static-dist) for the coverage e2e suite. Mirrors start-server.sh but uses a
# separate storage dir so it can coexist with the legacy suite's state.
#
# The caller (playwright.coverage.config.ts) sets OXICLOUD_STATIC_PATH to
# ./static-dist so the debug `cargo run` build serves the instrumented Vite
# output instead of the legacy ./static vanilla frontend.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# Honour the caller's OXICLOUD_STORAGE_PATH so a second webServer entry in
# playwright.coverage.config.ts (shared-worker-bus-ON backend) can point at
# its own directory without stomping on the per-tab backend's state. Relative
# paths are resolved against the repo root to match the server's own `cwd`.
if [[ -n "${OXICLOUD_STORAGE_PATH:-}" ]]; then
  if [[ "$OXICLOUD_STORAGE_PATH" = /* ]]; then
    SPA_STORAGE_PATH="$OXICLOUD_STORAGE_PATH"
  else
    SPA_STORAGE_PATH="$REPO_ROOT/$OXICLOUD_STORAGE_PATH"
  fi
else
  SPA_STORAGE_PATH="$REPO_ROOT/tests/e2e/storage-spa"
fi

# Mirror markers + the server's stdout/stderr to a log file as well as the
# console; CI surfaces it via the "Print server startup log" step (Playwright's
# own webServer capture isn't always shown there). The final `exec "$@"`
# inherits these fds, so the server's output is tee'd while it still replaces
# this shell (Playwright tracks the PID for teardown).
# Each backend logs to its own file so two Playwright webServer entries
# (per-tab backend on 8088, shared-worker backend on 8089) don't interleave.
# Falls back to the historical un-suffixed path when no port is set.
LOG_SUFFIX="${OXICLOUD_SERVER_PORT:+-${OXICLOUD_SERVER_PORT}}"
SERVER_LOG="$REPO_ROOT/tests/e2e/server-startup${LOG_SUFFIX}.log"
exec > >(tee "$SERVER_LOG") 2>&1

mark() { echo "[start-server-spa $(date -u +%H:%M:%S)] $*"; }
mark "repo_root=$REPO_ROOT  server args: $*"
if [[ -n "${1:-}" && "$1" != "cargo" ]]; then
  ls -la "$1" 2>&1 || mark "WARNING: server binary '$1' not found"
fi
mark "DATABASE_URL=${DATABASE_URL:-<unset>} PORT=${OXICLOUD_SERVER_PORT:-<unset>} STATIC=${OXICLOUD_STATIC_PATH:-<unset>}"

# ensure storage is empty before starting
mark "wiping $SPA_STORAGE_PATH to ensure clean startup"
rm -rf "$SPA_STORAGE_PATH"
mkdir -p "$SPA_STORAGE_PATH"

# Spawn database (idempotent — reuses the running test postgres if present).
mark "spawning test database…"
bash "$REPO_ROOT/tests/common/spawn-db.sh"
mark "database ready; starting server…"

# Point `--config` at an INTENTIONALLY EMPTY file: blocks the
# CWD `.env` fallback in main.rs (so developer envs don't leak
# in) while overriding nothing (so every Playwright-set env var,
# including per-suite DPOP/OPAQUE/plugin overrides, reaches the
# server intact). See tests/common/empty.env and
# tests/e2e/start-server.sh for the full rationale.
CONFIG_PATH="$REPO_ROOT/tests/common/empty.env"
if [[ "${1:-}" == "cargo" ]]; then
  exec "$@" -- --config "$CONFIG_PATH"
else
  exec "$@" --config "$CONFIG_PATH"
fi
