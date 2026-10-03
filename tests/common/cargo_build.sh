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
  if ! (cd "$REPO_ROOT" && cargo build "$@" >"$log" 2>&1); then
    printf '\033[1;31m[build FAIL]\033[0m %s — last 20 lines of %s:\n' "$what" "$log" >&2
    tail -n 20 "$log" >&2
    return 1
  fi
}
