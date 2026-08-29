#!/usr/bin/env bash
# Bundled-binary integration test.
#
# Builds `oxicloud` with `--features bundled-assets`, then boots it
# with the on-disk `static-dist/` moved aside and OXICLOUD_STATIC_PATH
# pointed at a nonexistent directory — the ONLY code path this can
# take is the embedded corpus. Then curls the SPA shell, a locale
# file, and a deep-link route to prove the embed serves correctly.
#
# Why this test exists: the bundled-assets feature has three failure
# modes that don't surface in normal filesystem-served CI:
#
#   1. rust-embed configuration bugs (glob patterns, `include`/`exclude`
#      attrs). A wrong glob can silently produce a 0-file embed —
#      caught 2026-08-28.
#   2. Debug-vs-release behaviour drift. rust-embed's dynamic-read mode
#      in debug builds reads from disk at runtime, which masks embed
#      bugs. `debug-embed` feature bakes files in for BOTH profiles
#      (this test relies on it).
#   3. Axum `Path` extractor on fallback routes returning 500. The
#      embedded `serve_root` handler needs `Request` extraction, not
#      `Path` — caught 2026-08-28.
#
# All three are boot / first-request bugs a normal integration suite
# would miss. See docs/plan/bundled-binary.md § Verification.
#
# Usage (from repo root):
#   bash tests/bundled-binary/run.sh
#
# Prerequisites: docker, cargo, node+npm (for the frontend build), curl

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMMON="$REPO_ROOT/tests/common"
TEST_DIR="$REPO_ROOT/tests/bundled-binary"

# shellcheck source=test.env
source "$TEST_DIR/test.env"
SERVER_PORT="${base_url##*:}"

log()  { echo "[bundled-binary] $*"; }
die()  { echo "[bundled-binary] ERROR: $*" >&2; exit 1; }
pass() { echo "[bundled-binary] ✓  $*"; }
fail() { echo "[bundled-binary] ✗  $*" >&2; FAILS=$((FAILS + 1)); }

wait_for_http() {
  local url="$1" timeout="${2:-120}"
  local deadline=$(( $(date +%s) + timeout ))
  until curl -sf "$url" >/dev/null 2>&1; do
    [[ $(date +%s) -ge $deadline ]] && die "Timeout waiting for $url"
    sleep 1
  done
}

# ── Cleanup state ────────────────────────────────────────────────────
#
# The trap kills the running server (a stray port-8090 process would
# collide with the next run) and tears down the test postgres. We do
# NOT touch `static-dist/` — the embed path is forced via
# `OXICLOUD_STATIC_PATH` alone (see step 5), so there's no filesystem
# state to restore.
SERVER_PID=""
FAILS=0

cleanup() {
  local rc=$?
  if [[ -n "$SERVER_PID" ]]; then
    log "Stopping server (pid $SERVER_PID)..."
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  bash "$COMMON/stop-db.sh" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT

# ── 1. Ensure static-dist/ is present at build time ─────────────────
#
# rust-embed's derive macro scans this directory at compile time. If
# it's missing, build.rs's `bundled_assets_guard` panics. This step
# builds the SPA in-place when the dir is absent.
#
# `SKIP_FRONTEND_BUILD=1` opts out and fails fast with a hint — useful
# in CI pipelines where the SPA build is a separate cached step
# upstream of this test.
if [[ ! -f "$REPO_ROOT/static-dist/index.html" ]]; then
  if [[ "${SKIP_FRONTEND_BUILD:-0}" == "1" ]]; then
    die "static-dist/index.html missing (SKIP_FRONTEND_BUILD=1). \
Build the SPA first:  (cd frontend && npm ci && npm run build)"
  fi
  log "static-dist/ missing — building the SPA (set SKIP_FRONTEND_BUILD=1 to opt out)..."
  if [[ ! -d "$REPO_ROOT/frontend/node_modules" ]]; then
    log "  running 'npm ci' (first-time install)..."
    (cd "$REPO_ROOT/frontend" && npm ci) || die "npm ci failed"
  fi
  (cd "$REPO_ROOT/frontend" && npm run build) || die "npm run build failed"
  [[ -f "$REPO_ROOT/static-dist/index.html" ]] || die "SPA build finished but static-dist/index.html still missing"
fi

# ── 2. Build with --features bundled-assets ──────────────────────────
#
# Debug build — matches what a dev iterates on locally, faster than
# --release, and the `debug-embed` feature in rust-embed makes debug
# and release behave identically here (both compile-time embed).
log "Building oxicloud with --features bundled-assets..."
(cd "$REPO_ROOT" && cargo build --features bundled-assets --bin oxicloud 2>&1 | tail -n 5) \
  || die "cargo build --features bundled-assets failed"
OXICLOUD_BIN="$REPO_ROOT/target/debug/oxicloud"
[[ -x "$OXICLOUD_BIN" ]] || die "Binary missing after build: $OXICLOUD_BIN"

# ── 3. Start test Postgres ───────────────────────────────────────────
log "Starting test Postgres via $COMMON/spawn-db.sh..."
bash "$COMMON/spawn-db.sh"

# ── 4. Boot the server with the embed forced ─────────────────────────
#
# `OXICLOUD_STATIC_PATH=/tmp/oxicloud-bundled-nonexistent-$$` is a path
# that provably doesn't exist (unique to this run's PID). The resolver
# in `resolve_static_source` runs two filesystem probes derived from
# this env var, BOTH of which miss:
#   1. `<parent>/static-dist/` → `/tmp/static-dist/` (vanishingly
#      unlikely to exist)
#   2. `OXICLOUD_STATIC_PATH` itself — nonexistent by construction
# The resolver then falls through to `StaticSource::Embedded`. Repo-root
# `static-dist/` is never consulted at runtime — parenthood is derived
# from the env var, not CWD — so this test is stateless on the working
# directory.
STORAGE="$TEST_DIR/storage"
rm -rf "$STORAGE" && mkdir -p "$STORAGE"

LOG_FILE="$TEST_DIR/server.log"
: > "$LOG_FILE"

set -a
# shellcheck source=../common/server.env
source "$COMMON/server.env"
OXICLOUD_SERVER_PORT=$SERVER_PORT
OXICLOUD_STORAGE_PATH="$STORAGE"
OXICLOUD_STATIC_PATH="/tmp/oxicloud-bundled-nonexistent-$$"
# Disable the Prometheus /metrics listener — the shared server.env pins
# it to 127.0.0.1:9090 which collides when another test server (or a
# stray dev process) already holds that port, killing our boot before
# /ready is reachable. Metrics aren't part of what this test asserts.
OXICLOUD_METRICS_LISTEN=""
# Override the shared server.env's `RUST_LOG=warn,audit=info,...` which
# suppresses the info-level `static-assets:` lines this test asserts on
# (embed-source resolution + locale extraction). Match the app's own
# default (main.rs::run) so `http=warn` still tames the access log.
RUST_LOG="info,http=warn,http::web=error"
set +a

log "Starting server on port $SERVER_PORT with embed forced..."
"$OXICLOUD_BIN" > "$LOG_FILE" 2>&1 &
SERVER_PID=$!
wait_for_http "$base_url/ready" 120
log "Server ready — running assertions."

# ── 6. Assertions ────────────────────────────────────────────────────

# 6a. Boot log confirms the resolver picked the embed path (not
#     silently fell back to a stale filesystem dir).
if grep -q 'static-assets: no filesystem source found, serving embedded corpus' "$LOG_FILE"; then
  pass "boot log: resolver picked StaticSource::Embedded"
else
  fail "boot log missing 'serving embedded corpus' line — was a filesystem source unexpectedly found?"
fi

# 6b. Boot log confirms N>0 locales were staged. This is the guard
#     against the 2026-08-28 glob bug — a silent 0 would look like a
#     "success" to a non-strict test.
if grep -Eq 'staged [1-9][0-9]* embedded locale file\(s\)' "$LOG_FILE"; then
  staged=$(grep -oE 'staged [0-9]+ embedded locale' "$LOG_FILE" | tail -n1 | awk '{print $2}')
  pass "boot log: staged $staged embedded locale file(s)"
else
  fail "boot log: staged 0 locales (or line missing)  ← the 2026-08-28 regression class"
fi

# 6c. SPA shell reachable at /
code=$(curl -s -o /dev/null -w '%{http_code}' "$base_url/")
if [[ "$code" == "200" ]]; then
  pass "GET / → 200"
else
  fail "GET / → $code (expected 200)"
fi

# 6d. Shell body looks like the SvelteKit-built index.html. Any of
#     `<!doctype html>` or `data-color-scheme` or `<meta http-equiv=
#     "content-security-policy"` would confirm it's the real shell
#     and not an error page.
body=$(curl -s "$base_url/")
if echo "$body" | grep -qi '<!doctype html>' && echo "$body" | grep -q 'data-color-scheme'; then
  pass "SPA shell body has SvelteKit markers (<!doctype html> + data-color-scheme)"
else
  fail "SPA shell body doesn't look like the real index.html"
fi

# 6e. SPA fallback handler serves the shell for deep-link routes.
#     `serve_root` MUST NOT return 500 here (the 2026-08-28 axum
#     `Path` extractor bug on fallback routes).
for path in /login /files/some-deep-link; do
  code=$(curl -s -o /dev/null -w '%{http_code}' "$base_url$path")
  if [[ "$code" == "200" ]]; then
    pass "GET $path → 200 (SPA fallback)"
  else
    fail "GET $path → $code (SPA fallback broken?)"
  fi
done

# 6f. Favicon served from the embed (specific bytes, not the shell).
code=$(curl -s -o /dev/null -w '%{http_code}' "$base_url/favicon.ico")
ct=$(curl -sI "$base_url/favicon.ico" | grep -i '^content-type:' | tr -d '\r' | awk '{print $2}')
if [[ "$code" == "200" ]] && [[ "$ct" != text/html* ]]; then
  pass "GET /favicon.ico → 200 with non-HTML content-type ($ct)"
else
  fail "GET /favicon.ico → code=$code content-type=$ct (should be 200 image/*)"
fi

# 6g. Locale JSON reachable AND is valid JSON with expected shape.
#
# `curl -w` captures status + content-type in the same call as the body
# so a failure surfaces WHAT the server actually returned (an HTML SPA
# fallback? a 404? a redirect?) instead of hiding it behind an empty
# string. Avoids the `head -c 1 | grep` pipeline that emits a spurious
# "broken pipe" under `set -euo pipefail`.
locale_status=$(curl -s -o /tmp/oxicloud-bundled-locale-$$ -w '%{http_code}|%{content_type}' "$base_url/locales/en.json")
locale_body=$(cat /tmp/oxicloud-bundled-locale-$$ 2>/dev/null || echo '')
rm -f /tmp/oxicloud-bundled-locale-$$
locale_code="${locale_status%%|*}"
locale_ct="${locale_status##*|}"
locale_first_char="${locale_body:0:1}"
if [[ "$locale_code" == "200" ]] && [[ "$locale_first_char" == "{" ]]; then
  pass "GET /locales/en.json → 200 JSON body (content-type=$locale_ct)"
else
  fail "GET /locales/en.json → code=$locale_code content-type=$locale_ct first-char='$locale_first_char' body-len=${#locale_body}"
  # Diagnostic dump — first 200 bytes so we can see what the server
  # actually served (SPA fallback? empty? something else?).
  echo "[bundled-binary]   body head: $(printf '%.200s' "$locale_body")" >&2
fi

# 6h. Immutable-asset cache header is applied by the `_app/immutable`
#     nested router. Pick any hashed asset from the embed inventory
#     — the boot log doesn't list them, so grep the shell HTML for one
#     of its `modulepreload` refs.
imm_asset=$(echo "$body" | grep -oE '/_app/immutable/[^"]+\.js' | head -n1)
if [[ -n "$imm_asset" ]]; then
  cc=$(curl -sI "$base_url$imm_asset" | grep -i '^cache-control:' | tr -d '\r')
  if echo "$cc" | grep -q 'immutable'; then
    pass "immutable-asset cache header applied: $cc"
  else
    fail "immutable-asset $imm_asset cache header wrong: $cc"
  fi
else
  fail "couldn't find a /_app/immutable/*.js reference in the shell HTML to test"
fi

# 6i. Shell HTML carries a Content-Security-Policy with sha256 script
#     hashes. The SvelteKit build inlines a <meta http-equiv= ...> in
#     index.html; that alone counts (belt). If the axum response-level
#     CSP also carries hashes, even better (suspenders) — but the
#     current middleware ships a hardcoded string, so we only check
#     the meta tag which comes from the embedded bytes.
if echo "$body" | grep -q "'sha256-"; then
  pass "SPA shell HTML carries CSP with sha256 script hashes (from Vite build)"
else
  fail "SPA shell HTML has no sha256 CSP hashes — build produced a shell without them?"
fi

# ── Report ───────────────────────────────────────────────────────────

echo ""
if [[ $FAILS -gt 0 ]]; then
  echo "─── SERVER LOG (last 60 lines) ───────────────────────────"
  tail -n 60 "$LOG_FILE"
  echo "──────────────────────────────────────────────────────────"
  die "$FAILS assertion(s) failed"
fi
log "bundled-binary integration tests passed ✅"
