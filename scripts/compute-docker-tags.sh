#!/usr/bin/env bash
# =============================================================
# Compute Docker channel + version + tag set for the
# `docker-publish` workflow. Extracted from the workflow so the
# logic can be unit-tested via `scripts/test-docker-publish-tags.sh`
# without needing to dispatch the workflow itself.
#
# The workflow's meta step invokes this via `bash scripts/
# compute-docker-tags.sh` with the GITHUB_* env vars set; the
# same call form works from a local shell for smoke checks
# ("what would we publish if I tag v0.8.8 tomorrow?").
#
# Inputs (env vars — missing required inputs exit non-zero):
#   EVENT_NAME           workflow_dispatch | release | push
#   GITHUB_REF           refs/heads/main | refs/tags/vX.Y.Z | ...
#                        (required for the `push` event)
#   DISPATCH_VERSION     workflow_dispatch only, e.g. v0.5.3 or 0.5.3
#   RELEASE_TAG          release event only,      e.g. v0.8.7
#   REGISTRY_IMAGE       Docker Hub image   (e.g. diocrafts/oxicloud)
#   GHCR_REGISTRY_IMAGE  GHCR image         (e.g. ghcr.io/atalayalabs/oxicloud)
#   SKIP_DOCKERHUB       optional. When "true", omits Docker Hub tags
#                        from the output — used by forks whose
#                        DOCKERHUB_TOKEN secret isn't configured. The
#                        workflow only pushes to GHCR (which needs no
#                        external secret; auth via GITHUB_TOKEN).
#
# Outputs:
#   Always writes `version=<v>`, `channel=<c>`, and a `tags:` block
#   to stdout — visible in workflow logs and captured by the test
#   harness for diff-based assertions.
#
#   When `GITHUB_OUTPUT` is set (inside a GHA `run:` step), also
#   emits the same values via GHA's `>> $GITHUB_OUTPUT` convention
#   so subsequent steps can reference `${{ steps.meta.outputs.tags }}`.
#
# Channel semantics (mirrors the workflow's tag policy):
#   release   — tag push / release event / manual dispatch:
#               publish `:<version>` AND move `:latest`.
#   main      — push to `main` branch: publish `:main` (mutable
#               tip) only. Never touches `:latest`, never emits a
#               per-commit `:main-<sha>` (would balloon the
#               registry across every merge).
# =============================================================

set -euo pipefail

: "${EVENT_NAME:?EVENT_NAME required}"
: "${REGISTRY_IMAGE:?REGISTRY_IMAGE required}"
: "${GHCR_REGISTRY_IMAGE:?GHCR_REGISTRY_IMAGE required}"

# Both GHCR and Docker Hub reject mixed-case namespace / image names
# ("repository name must be lowercase"). `${{ github.repository_owner
# }}` in the workflow inserts the GitHub username verbatim, and GitHub
# expression syntax has no `lower()` function. So we normalise here
# — the workflow keeps its declarative `env:` block, the script owns
# the case-safety contract, and tests cover it (see
# `test-docker-publish-tags.sh` for mixed-case cases).
REGISTRY_IMAGE=$(echo "$REGISTRY_IMAGE" | tr '[:upper:]' '[:lower:]')
GHCR_REGISTRY_IMAGE=$(echo "$GHCR_REGISTRY_IMAGE" | tr '[:upper:]' '[:lower:]')

# Docker Hub is optional — a fork without DOCKERHUB_TOKEN configured
# still publishes to GHCR (its own namespace, auth via GITHUB_TOKEN)
# but skips Docker Hub cleanly. The workflow sets this to "true" when
# `secrets.DOCKERHUB_TOKEN` is empty; the tag set below omits DH
# entries in that case.
SKIP_DOCKERHUB="${SKIP_DOCKERHUB:-false}"

case "$EVENT_NAME" in
    workflow_dispatch)
        : "${DISPATCH_VERSION:?DISPATCH_VERSION required for workflow_dispatch}"
        VERSION="${DISPATCH_VERSION#v}"
        CHANNEL="release"
        ;;
    release)
        : "${RELEASE_TAG:?RELEASE_TAG required for release event}"
        VERSION="${RELEASE_TAG#v}"
        CHANNEL="release"
        ;;
    push)
        : "${GITHUB_REF:?GITHUB_REF required for push event}"
        if [ "$GITHUB_REF" = "refs/heads/main" ]; then
            VERSION="main"
            CHANNEL="main"
        else
            # Tag push — strip refs/tags/ prefix and leading `v`.
            VERSION="${GITHUB_REF#refs/tags/}"
            VERSION="${VERSION#v}"
            CHANNEL="release"
        fi
        ;;
    *)
        echo "compute-docker-tags: unsupported EVENT_NAME: $EVENT_NAME" >&2
        exit 1
        ;;
esac

# Assemble the multi-line tag set. Format matches what the
# `docker/build-push-action` `tags:` input consumes — one tag
# per line, whitespace ignored between lines. Docker Hub entries
# omitted entirely when SKIP_DOCKERHUB=true — the build-push-action
# just doesn't see the tags, so no auth is attempted for them.
if [ "$CHANNEL" = "main" ]; then
    if [ "$SKIP_DOCKERHUB" = "true" ]; then
        TAGS="${GHCR_REGISTRY_IMAGE}:main"
    else
        TAGS="${REGISTRY_IMAGE}:main
${GHCR_REGISTRY_IMAGE}:main"
    fi
else
    if [ "$SKIP_DOCKERHUB" = "true" ]; then
        TAGS="${GHCR_REGISTRY_IMAGE}:${VERSION}
${GHCR_REGISTRY_IMAGE}:latest"
    else
        TAGS="${REGISTRY_IMAGE}:${VERSION}
${REGISTRY_IMAGE}:latest
${GHCR_REGISTRY_IMAGE}:${VERSION}
${GHCR_REGISTRY_IMAGE}:latest"
    fi
fi

# Emit to $GITHUB_OUTPUT when running under GHA — subsequent
# workflow steps read via `${{ steps.meta.outputs.tags }}`.
if [ -n "${GITHUB_OUTPUT:-}" ]; then
    {
        echo "version=$VERSION"
        echo "channel=$CHANNEL"
        echo "tags<<EOF"
        echo "$TAGS"
        echo "EOF"
    } >> "$GITHUB_OUTPUT"
fi

# Also emit VERSION + CHANNEL + SKIP_DOCKERHUB to $GITHUB_ENV —
# later steps (`Verify published image`, DockerHub-gated conditionals)
# read these directly. Keeps the step-scoped env aligned with the
# steps.meta.outputs.* set for consumers that prefer one or the other.
#
# Also OVERRIDE the workflow-level REGISTRY_IMAGE / GHCR_REGISTRY_IMAGE
# env vars with the lowercased forms. Without this, the verify step
# would read the workflow-declared mixed-case value from
# `${{ github.repository_owner }}` (e.g. `ghcr.io/EdouardVanbelle/
# oxicloud`) and `docker pull` would reject it — despite the tags
# themselves being lowercased in the actual push. Step-level env
# additions take precedence over workflow-level for subsequent steps.
if [ -n "${GITHUB_ENV:-}" ]; then
    {
        echo "VERSION=$VERSION"
        echo "CHANNEL=$CHANNEL"
        echo "SKIP_DOCKERHUB=$SKIP_DOCKERHUB"
        echo "REGISTRY_IMAGE=$REGISTRY_IMAGE"
        echo "GHCR_REGISTRY_IMAGE=$GHCR_REGISTRY_IMAGE"
    } >> "$GITHUB_ENV"
fi

# Always echo to stdout — visible in workflow logs (useful for
# dry-run verification, when the push step is skipped) and
# consumed by the test harness for equality checks.
echo "version=$VERSION"
echo "channel=$CHANNEL"
echo "tags:"
echo "$TAGS" | sed 's/^/  /'
