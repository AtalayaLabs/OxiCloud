#!/usr/bin/env bash
# =============================================================
# Unit tests for `scripts/compute-docker-tags.sh`.
#
# Exercises every trigger channel the docker-publish workflow
# supports, plus the invalid-input path. Runs standalone in
# under a second — cheap regression check to lock the tag-set
# contract before pushing changes to `.github/workflows/
# docker-publish.yml`.
#
# Run:
#   bash scripts/test-docker-publish-tags.sh
# =============================================================

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$SCRIPT_DIR/compute-docker-tags.sh"

if [ ! -f "$SCRIPT" ]; then
    echo "compute-docker-tags.sh not found at $SCRIPT" >&2
    exit 2
fi

REGISTRY_IMAGE=diocrafts/oxicloud
GHCR_REGISTRY_IMAGE=ghcr.io/atalayalabs/oxicloud

pass=0
fail=0

# Runs the script with the given env, compares stdout against expected.
# `env "$@" bash ...` passes the env vars only for this invocation so
# leftover state from a prior case can't leak across.
expect() {
    local name="$1" expected="$2"
    shift 2
    local actual rc
    actual=$(env -i \
        REGISTRY_IMAGE="$REGISTRY_IMAGE" \
        GHCR_REGISTRY_IMAGE="$GHCR_REGISTRY_IMAGE" \
        PATH="/usr/bin:/bin" \
        "$@" \
        bash "$SCRIPT" 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "FAIL: $name — script exited $rc:"
        echo "$actual" | sed 's/^/    /'
        fail=$((fail + 1))
        return
    fi
    if [ "$actual" = "$expected" ]; then
        echo "PASS: $name"
        pass=$((pass + 1))
    else
        echo "FAIL: $name"
        echo "  expected:"
        echo "$expected" | sed 's/^/    /'
        echo "  actual:"
        echo "$actual" | sed 's/^/    /'
        fail=$((fail + 1))
    fi
}

expect_fail() {
    local name="$1"
    shift
    if env -i \
        REGISTRY_IMAGE="$REGISTRY_IMAGE" \
        GHCR_REGISTRY_IMAGE="$GHCR_REGISTRY_IMAGE" \
        PATH="/usr/bin:/bin" \
        "$@" \
        bash "$SCRIPT" >/dev/null 2>&1
    then
        echo "FAIL: $name (script should have exited non-zero)"
        fail=$((fail + 1))
    else
        echo "PASS: $name"
        pass=$((pass + 1))
    fi
}

# ── Happy paths ─────────────────────────────────────────────────

expect "push to main → :main only, no :latest" \
"version=main
channel=main
tags:
  diocrafts/oxicloud:main
  ghcr.io/atalayalabs/oxicloud:main" \
    EVENT_NAME=push GITHUB_REF=refs/heads/main

expect "push of version tag → :<v> + :latest" \
"version=0.8.7
channel=release
tags:
  diocrafts/oxicloud:0.8.7
  diocrafts/oxicloud:latest
  ghcr.io/atalayalabs/oxicloud:0.8.7
  ghcr.io/atalayalabs/oxicloud:latest" \
    EVENT_NAME=push GITHUB_REF=refs/tags/v0.8.7

expect "release event → :<v> + :latest" \
"version=0.9.0
channel=release
tags:
  diocrafts/oxicloud:0.9.0
  diocrafts/oxicloud:latest
  ghcr.io/atalayalabs/oxicloud:0.9.0
  ghcr.io/atalayalabs/oxicloud:latest" \
    EVENT_NAME=release RELEASE_TAG=v0.9.0

expect "workflow_dispatch with 'v' prefix" \
"version=1.0.0
channel=release
tags:
  diocrafts/oxicloud:1.0.0
  diocrafts/oxicloud:latest
  ghcr.io/atalayalabs/oxicloud:1.0.0
  ghcr.io/atalayalabs/oxicloud:latest" \
    EVENT_NAME=workflow_dispatch DISPATCH_VERSION=v1.0.0

expect "workflow_dispatch without 'v' prefix (permissive)" \
"version=1.0.0
channel=release
tags:
  diocrafts/oxicloud:1.0.0
  diocrafts/oxicloud:latest
  ghcr.io/atalayalabs/oxicloud:1.0.0
  ghcr.io/atalayalabs/oxicloud:latest" \
    EVENT_NAME=workflow_dispatch DISPATCH_VERSION=1.0.0

expect "release with pre-release version" \
"version=0.9.0-rc1
channel=release
tags:
  diocrafts/oxicloud:0.9.0-rc1
  diocrafts/oxicloud:latest
  ghcr.io/atalayalabs/oxicloud:0.9.0-rc1
  ghcr.io/atalayalabs/oxicloud:latest" \
    EVENT_NAME=release RELEASE_TAG=v0.9.0-rc1

# ── SKIP_DOCKERHUB path (forks without DOCKERHUB_TOKEN) ─────────

expect "push to main + SKIP_DOCKERHUB → GHCR only" \
"version=main
channel=main
tags:
  ghcr.io/atalayalabs/oxicloud:main" \
    EVENT_NAME=push GITHUB_REF=refs/heads/main SKIP_DOCKERHUB=true

expect "release + SKIP_DOCKERHUB → GHCR :<v> + :latest only" \
"version=0.9.0
channel=release
tags:
  ghcr.io/atalayalabs/oxicloud:0.9.0
  ghcr.io/atalayalabs/oxicloud:latest" \
    EVENT_NAME=release RELEASE_TAG=v0.9.0 SKIP_DOCKERHUB=true

expect "dispatch + SKIP_DOCKERHUB → GHCR :<v> + :latest only" \
"version=1.0.0
channel=release
tags:
  ghcr.io/atalayalabs/oxicloud:1.0.0
  ghcr.io/atalayalabs/oxicloud:latest" \
    EVENT_NAME=workflow_dispatch DISPATCH_VERSION=v1.0.0 SKIP_DOCKERHUB=true

expect "explicit SKIP_DOCKERHUB=false behaves like default (both registries)" \
"version=main
channel=main
tags:
  diocrafts/oxicloud:main
  ghcr.io/atalayalabs/oxicloud:main" \
    EVENT_NAME=push GITHUB_REF=refs/heads/main SKIP_DOCKERHUB=false

# ── Case-safety — GHCR / DH reject mixed-case names ─────────────
#
# Regression pin: `${{ github.repository_owner }}` inserts a
# GitHub username verbatim, which is often mixed-case
# (e.g. EdouardVanbelle). The registries reject that with
# "repository name must be lowercase". The script normalises
# both inputs; these cases assert it.

# Local override so we can pass a mixed-case owner without touching
# the harness's defaults on other cases.
_orig_ghcr="$GHCR_REGISTRY_IMAGE"
_orig_dh="$REGISTRY_IMAGE"

GHCR_REGISTRY_IMAGE=ghcr.io/EdouardVanbelle/OxiCloud \
REGISTRY_IMAGE=ghcr.io/EdouardVanbelle/OxiCloud \
expect "mixed-case owner and image lowercased in tags" \
"version=main
channel=main
tags:
  ghcr.io/edouardvanbelle/oxicloud:main" \
    EVENT_NAME=push GITHUB_REF=refs/heads/main \
    REGISTRY_IMAGE=DioCrafts/OxiCloud \
    GHCR_REGISTRY_IMAGE=ghcr.io/EdouardVanbelle/OxiCloud \
    SKIP_DOCKERHUB=true

expect "release with mixed-case DH namespace lowercased" \
"version=0.8.7
channel=release
tags:
  diocrafts/oxicloud:0.8.7
  diocrafts/oxicloud:latest
  ghcr.io/edouardvanbelle/oxicloud:0.8.7
  ghcr.io/edouardvanbelle/oxicloud:latest" \
    EVENT_NAME=release RELEASE_TAG=v0.8.7 \
    REGISTRY_IMAGE=DioCrafts/OxiCloud \
    GHCR_REGISTRY_IMAGE=ghcr.io/EdouardVanbelle/OxiCloud

REGISTRY_IMAGE="$_orig_dh"
GHCR_REGISTRY_IMAGE="$_orig_ghcr"
unset _orig_dh _orig_ghcr

# ── Error paths ─────────────────────────────────────────────────

expect_fail "unknown event rejected" \
    EVENT_NAME=cron GITHUB_REF=refs/heads/main

expect_fail "push without GITHUB_REF rejected" \
    EVENT_NAME=push

expect_fail "release without RELEASE_TAG rejected" \
    EVENT_NAME=release

expect_fail "dispatch without DISPATCH_VERSION rejected" \
    EVENT_NAME=workflow_dispatch

# ── Report ──────────────────────────────────────────────────────

echo ""
echo "─────────────────────────"
echo "Passed: $pass  Failed: $fail"
[ "$fail" -eq 0 ]
