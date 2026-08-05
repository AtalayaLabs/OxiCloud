# syntax=docker/dockerfile:1.7
# Selects which builder stage assembles the runtime image. Defaults reproduce
# the CI/release path exactly (the `builder` stage; binaries under
# target/release). The e2e image build overrides these to
# BUILDER=builder-cache / BIN_DIR=/app/bin to use the BuildKit cache-mount
# builder. Declared in the global scope because FROM (unlike COPY --from) can
# expand a build arg in a stage reference.
ARG BUILDER=builder
ARG BIN_DIR=/app/target/release

# ─── Stage 1: Shared build base (avoids duplicate apk install) ────────────────
FROM rust:1.96-alpine3.24 AS base
# sqlx's postgres driver speaks the wire protocol in pure Rust (no pq-sys in
# Cargo.lock) and TLS goes through rustls, so libpq headers are never needed at
# build time. perl/make/gcc/musl-dev remain for the C builds of aws-lc-sys.
RUN apk --no-cache upgrade && \
    apk add --no-cache musl-dev pkgconfig gcc perl make

# ─── Stage 1b: Build the SvelteKit frontend (Vite) ───────────────────────────
# Produces the SPA in /static-dist. `npm ci` is cached unless the lockfile
# changes; the Rust build no longer bundles assets (see build.rs).
FROM node:26.3.1-alpine3.24 AS frontend
WORKDIR /frontend
COPY frontend/package.json frontend/package-lock.json ./
# Cache mount for npm's package store: when the lockfile changes (busting the
# layer) npm ci still reuses already-downloaded tarballs instead of refetching
# them. Persists in the local BuildKit cache; ignored harmlessly when absent.
RUN --mount=type=cache,target=/root/.npm npm ci
COPY frontend/ ./
# VITE_E2E=1 keeps the test-only `data-testid` attributes in the build (set by
# the e2e image build); unset for release images, which strip them entirely.
ARG VITE_E2E
ENV VITE_E2E=${VITE_E2E}
RUN npm run build

# ─── Stage 2: Cache dependencies ─────────────────────────────────────────────
FROM base AS cacher
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# build.rs runs during the dependency build; it only injects git metadata.
COPY build.rs ./
# Create a minimal project to download and cache dependencies
RUN mkdir -p src/bin && \
    echo 'fn main() { println!("Dummy build for caching dependencies"); }' > src/main.rs && \
    echo 'fn main() {}' > src/bin/generate-openapi.rs && \
    echo 'fn main() {}' > src/bin/migrate-nfc-filenames.rs && \
    echo 'fn main() {}' > src/bin/oxicloud-cli.rs && \
    echo 'fn main() {}' > src/bin/opaque-hurl-helper.rs && \
    cargo build --release --bin oxicloud --bin generate-openapi --bin migrate-nfc-filenames --bin oxicloud-cli && \
    rm -rf src static-dist target/release/deps/oxicloud* target/release/build/oxicloud-*

# ─── Stage 3: Build the application ──────────────────────────────────────────
FROM base AS builder
WORKDIR /app
# Copy cached dependencies (only target dir and cargo registry)
COPY --from=cacher /app/target target
COPY --from=cacher /usr/local/cargo/registry /usr/local/cargo/registry
# Copy source, build script, and migrations
COPY Cargo.toml Cargo.lock build.rs ./
COPY src src
COPY migrations migrations
# askama templates — read at *compile time* by the derive macro, so
# they must be present in the build stage even though they're embedded
# into the final binary and never read from disk at runtime.
COPY templates templates
# Build with all optimizations (DATABASE_URL only needed at compile-time for sqlx)
ARG DATABASE_URL="postgres://postgres:postgres@localhost/oxicloud"
# Git metadata pipe-through. build.rs reads these env vars to stamp
# GIT_HASH / GIT_BRANCH into the binary (consumed by `oxicloud
# --version`). Without this pipe-through Docker builds always fall
# back to "unknown" — there's no .git/ in the build context, and the
# workflow's GitHub Actions env (GITHUB_SHA / GITHUB_REF_NAME /
# GITHUB_HEAD_REF) isn't visible to RUN steps unless threaded in
# explicitly as build-args. CI passes these via build-args in the
# docker-build and docker-publish workflows; local `docker build` can
# pass `--build-arg GITHUB_SHA=$(git rev-parse HEAD) --build-arg
# GITHUB_REF_NAME=$(git rev-parse --abbrev-ref HEAD)` to get the same
# stamping behaviour.
ARG GITHUB_SHA=""
ARG GITHUB_REF_NAME=""
ARG GITHUB_HEAD_REF=""
# Explicit --bin list: defence-in-depth so the prod image never ships
# test-only bins (e.g. load-seed) even if `required-features` gating
# changes upstream.
RUN DATABASE_URL="${DATABASE_URL}" \
    GITHUB_SHA="${GITHUB_SHA}" \
    GITHUB_REF_NAME="${GITHUB_REF_NAME}" \
    GITHUB_HEAD_REF="${GITHUB_HEAD_REF}" \
    cargo build --release --bin oxicloud --bin generate-openapi --bin migrate-nfc-filenames --bin oxicloud-cli
# The SPA is built by the Vite frontend stage; bring it in for the runtime copy
# below (build.rs has no asset pipeline — it only injects git metadata).
COPY --from=frontend /static-dist ./static-dist

# ─── Stage 3b: Cache-mount builder (local e2e fast incremental rebuilds) ──────
# Built ONLY when BUILDER=builder-cache is passed (the Testcontainers e2e build,
# which calls .withBuildkit()). BuildKit cache mounts persist the cargo registry
# and target/ in the local BuildKit cache across runs, so a one-line src change
# recompiles just the changed crate instead of the whole dependency graph. CI
# never sets this arg, so this stage is absent from CI's build graph and CI
# behaviour/caching is unchanged.
#
# NOTE: target/ is a cache mount, so it is NOT part of the image layer once the
# RUN finishes — the two shipped binaries MUST be cp'd out within the same RUN.
# TARGETARCH scopes the target/ mount per-arch so it is never shared across
# architectures (object files are arch-specific).
FROM base AS builder-cache
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src src
COPY migrations migrations
COPY templates templates
COPY --from=frontend /static-dist ./static-dist
ARG DATABASE_URL="postgres://postgres:postgres@localhost/oxicloud"
ARG TARGETARCH
# Git metadata pipe-through (see builder stage above for why this
# matters and what callers must pass).
ARG GITHUB_SHA=""
ARG GITHUB_REF_NAME=""
ARG GITHUB_HEAD_REF=""
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=shared \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=shared \
    --mount=type=cache,id=oxicloud-target-${TARGETARCH},target=/app/target,sharing=locked \
    DATABASE_URL="${DATABASE_URL}" \
    GITHUB_SHA="${GITHUB_SHA}" \
    GITHUB_REF_NAME="${GITHUB_REF_NAME}" \
    GITHUB_HEAD_REF="${GITHUB_HEAD_REF}" \
    cargo build --release && \
    mkdir -p /app/bin && \
    cp target/release/oxicloud /app/bin/oxicloud && \
    cp target/release/migrate-nfc-filenames /app/bin/migrate-nfc-filenames && \
    cp target/release/oxicloud-cli /app/bin/oxicloud-cli

# ─── Stage 3c: Select the builder & normalise the binary path ─────────────────
# FROM expands the global ${BUILDER} arg to alias the chosen builder stage
# (`builder` for CI/release, `builder-cache` for the e2e image). It then copies
# the two shipped binaries from the builder-specific ${BIN_DIR} into a single
# stable path (/app/release) so the runtime stage's COPYs are independent of
# which builder ran. `static-dist` already lives at /app/static-dist in both
# builders, so it needs no normalisation.
FROM ${BUILDER} AS app
ARG BIN_DIR
RUN mkdir -p /app/release && \
    cp "${BIN_DIR}/oxicloud" "${BIN_DIR}/migrate-nfc-filenames" "${BIN_DIR}/oxicloud-cli" /app/release/

# ─── Stage 4: Minimal runtime image ──────────────────────────────────────────
FROM alpine:3.24.0

# OCI image metadata
LABEL org.opencontainers.image.title="OxiCloud" \
      org.opencontainers.image.description="Ultra-fast, secure & lightweight self-hosted cloud storage built in Rust" \
      org.opencontainers.image.url="https://github.com/DioCrafts/OxiCloud" \
      org.opencontainers.image.source="https://github.com/DioCrafts/OxiCloud" \
      org.opencontainers.image.vendor="DioCrafts" \
      org.opencontainers.image.licenses="MIT"

# Install only necessary runtime dependencies and update packages
# su-exec is needed by the entrypoint to drop privileges after fixing volume permissions.
# ffmpeg powers server-side video thumbnail extraction (one frame → WebP pipeline);
# without it videos simply have no thumbnail (OXICLOUD_ENABLE_VIDEO_THUMBNAILS).
# No libpq: the pure-Rust sqlx postgres driver never links it.
RUN apk --no-cache upgrade && \
    apk add --no-cache libgcc ca-certificates tzdata su-exec ffmpeg && \
    addgroup -g 1001 -S oxicloud && \
    adduser -u 1001 -S oxicloud -G oxicloud

# Copy the compiled binary and entrypoint (--chmod avoids extra RUN chmod layers)
COPY --from=app --chmod=755 /app/release/oxicloud /usr/local/bin/
# Ship the NFC filename migration binary alongside the server so
# operators can run it inside the container without a separate Rust
# toolchain — `docker exec <container> migrate-nfc-filenames --dry-run`
# to preview, drop `--dry-run` to execute. One-shot tool, safe to
# ship; it only mutates `storage.files` rows whose name ≠ NFC(name).
COPY --from=app --chmod=755 /app/release/migrate-nfc-filenames /usr/local/bin/
# Ship the OPAQUE server-setup generator alongside the server so operators
# can generate their `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP` value inside the
# container without a separate Rust toolchain:
#   docker run --rm <image> oxicloud-cli opaque setup   # prints the base64 value
# One-shot, side-effect-free — safe to include; the runtime doesn't
# invoke it, admins do (see docs/config/authentication.md §OPAQUE).
COPY --from=app --chmod=755 /app/release/oxicloud-cli /usr/local/bin/
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN sed -i 's/\r//' /usr/local/bin/entrypoint.sh && \
    chmod 755 /usr/local/bin/entrypoint.sh

# Copy the built SPA (produced by the Vite frontend stage)
COPY --from=app --chown=oxicloud:oxicloud /app/static-dist /app/static
# Create storage directory with proper permissions
RUN mkdir -p /app/storage && chown -R oxicloud:oxicloud /app/storage

# Allocator tuning — make RSS track the live working set.
# mimalloc retains freed pages by default, so process RSS clamps at the peak
# even after the in-memory caches (file content, thumbnails, transcode) expire
# by TTL. Purging immediately returns those pages to the kernel at no throughput
# cost. Measured on this musl/aarch64 image: a 400 MB alloc→free spike returns
# 0 MB by default vs ~400 MB with this set (idle RSS back to a few MB).
ENV MIMALLOC_PURGE_DELAY=0

# Set working directory
WORKDIR /app

# Expose application port
EXPOSE 8086

# Liveness probe — verifies the HTTP server is up (no DB check, fast).
# Docker / Compose / Swarm will mark the container unhealthy after 3 failures.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://localhost:8086/health || exit 1

# Entrypoint fixes volume permissions then drops to oxicloud user.
# The container starts as root so it can chown mounted volumes,
# then su-exec drops privileges before running the application.
ENTRYPOINT ["entrypoint.sh"]
CMD ["oxicloud"]
