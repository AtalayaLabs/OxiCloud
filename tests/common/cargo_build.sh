#!/usr/bin/env bash
# Shared `cargo build` wrapper for the API harness.
#
# Replaces the `cargo build … 2>&1 | tail -n 20` idiom that every build
# site used to spell out. `tail` cannot print anything until the command
# it is reading from EXITS, so a multi-minute build produced a silent
# terminal for its whole duration — and the only thing still writing was
# the already-running server's log, which read as if the harness had
# hung on it.
#
# That is not hypothetical: a run that took 5m03s swallowed cargo's own
# "Blocking waiting for file lock on build directory" — the one line
# that explained the wait — until after the wait was over.
#
# So: stream to a file at a FIXED, announced path (tailable while the
# build runs), stay quiet on success, and print the tail on failure.
#
# Sourced by `tests/api/run.sh` and `tests/api/rt_bus_check.sh`. Both
# run under `set -euo pipefail`; this returns non-zero rather than
# calling `die`, because each script defines its own.
#
# ── Why `--features plugins` is injected below ────────────────────────
#
# Every build of this binary — the harnesses here, `just build`,
# `just run`, `just dev`, and `tests/e2e/playwright.coverage.config.ts`
# — writes into the SAME `target/<profile>` tree, and cargo's
# fingerprint includes the resolved feature set. Two sets cannot
# coexist there: each switch invalidates the other's artifacts and
# costs a full recompile of the crate.
#
# The harnesses used to build bare (`{}`) while everything a developer
# runs by hand builds `--features plugins`, so an `api-test` →
# `just dev` → Playwright loop paid a full rebuild at every hop. The
# Playwright config made it worse by design: it refuses to reuse
# `target/debug/oxicloud` unless `hasPluginsFeature()` passes, so it
# rebuilt after EVERY api-test run.
#
# `plugins` is the set to converge on because it is also what CI's
# `build` job ships (`cargo build --release --features plugins`) as the
# artifact the `api-test` job consumes — so the harnesses now exercise
# the binary shape CI actually tests, which is worth more than the
# minutes saved.
#
# THIS IS A WHOLE-FAMILY INVARIANT, not a local choice. The harnesses
# that don't source this file spell the same flag out at their own
# build site and point here: `tests/webdav/run.sh`,
# `tests/webdav/run-litmus.sh`, `tests/webdav-drive-root/run.sh`,
# `tests/oidc/run.sh`, `tests/oidc/run-sso-only.sh`,
# `tests/oidc/run-manual-sso-only.sh`, `tests/nextcloudcmd/run.sh`,
# `tests/caldav/run-pycaldav.sh`. Change the set and change all of
# them, or `just test-api` starts thrashing between its own harnesses.
# No env override on purpose: one that only half the family honoured
# would reintroduce exactly the mismatch this removes.
#
# Deliberately NOT aligned: `tests/load/run.sh` (`load_seed_bin`) and
# `tests/bundled-binary/run.sh` (`bundled-assets`) build a different
# binary on purpose and run rarely.
#
# Helper bins keep their own `--features test_utils`, which cargo
# unions with this one. That set (`{plugins, test_utils}`) is a third
# fingerprint, so building a helper does still invalidate the server —
# but each helper is guarded by `[[ ! -x "$BIN" ]]`, so it happens
# once per tree rather than once per run. Moving the helpers into
# their own workspace crates is the real fix and is tracked separately.

# Where the build output goes. Under `target/` so it is already
# gitignored and lands beside what it describes.
#
# One path for every build site: they are sequential, never concurrent,
# and a single well-known file is what makes `tail -f` a usable
# instruction. Truncated per build, so it always describes the current
# one.
cargo_build_log_path() {
  printf '%s/target/api-test-build.log' "$REPO_ROOT"
}

# cargo_build_logged <what> [cargo args...]
#
# `<what>` names the thing being built, for the log line only.
cargo_build_logged() {
  local what="$1"
  shift
  local log
  log="$(cargo_build_log_path)"
  # `target/` may not exist on a cold tree — the build would create it,
  # but the redirect below happens first.
  mkdir -p "$(dirname "$log")"
  printf '\033[1;36m[build]\033[0m %s — output: %s\n' "$what" "$log"
  if ! (cd "$REPO_ROOT" && cargo build --features plugins "$@" >"$log" 2>&1); then
    printf '\033[1;31m[build FAIL]\033[0m %s — last 20 lines of %s:\n' "$what" "$log" >&2
    tail -n 20 "$log" >&2
    return 1
  fi
}
