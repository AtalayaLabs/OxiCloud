# Deployment & Docker

## Docker Image

OxiCloud uses a multi-stage Alpine build producing a **~40 MB** image:

1. **Base** — shared build dependencies (`musl-dev`, `pkgconfig`, `openssl-dev`, `libpq-dev`)
2. **Cacher** — pre-builds the dependency layer for fast rebuilds
3. **Builder** — compiles OxiCloud (`rust:1.94.0-alpine3.23`)
4. **Runtime** — minimal Alpine (`alpine:3.23.3`) with `libgcc`, `ca-certificates`, `libpq`, `tzdata`, `su-exec`

The final image runs as non-root user `oxicloud` (UID/GID 1001). Exposed port: **8086**.

## Docker Compose

```yaml
services:
  postgres:
    image: postgres:18.2-alpine3.23
    restart: always
    environment:
      POSTGRES_DB: oxicloud
      POSTGRES_USER: postgres
      POSTGRES_PASSWORD: postgres
    ports:
      - "5432:5432"
    networks:
      - oxicloud
    volumes:
      - pg_data:/var/lib/postgresql/
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres"]
      interval: 5s
      timeout: 5s
      retries: 5

  oxicloud:
    image: diocrafts/oxicloud:latest
    restart: always
    build:
      context: .
      dockerfile: Dockerfile
    ports:
      - "8086:8086"
    networks:
      - oxicloud
    env_file:
      - .env
    volumes:
      - storage_data:/app/storage
    depends_on:
      postgres:
        condition: service_healthy

networks:
  oxicloud:
    driver: bridge

volumes:
  pg_data:
  storage_data:
```

This example mirrors the repository's current `docker-compose.yml`. If you deploy from a registry-only setup, you can keep the `image:` line and remove the `build:` stanza.

## Kubernetes (Helm)

### Prerequisites
- Kubernetes cluster
- Default StorageClass
- Ingress Controller
- Helm 3+

### Install

```bash
helm upgrade --install oxicloud charts/oxicloud \
  -f charts/oxicloud/values.yaml
```

### Verify

```bash
kubectl get pods -n oxicloud
kubectl logs statefulset/oxicloud -n oxicloud
```

### WOPI Verification

If Collabora/OnlyOffice is enabled:

```bash
kubectl logs statefulset/oxicloud -n oxicloud | grep "WOPI discovery loaded"
```

## Reverse Proxy & Subpath Deployments

OxiCloud runs happily behind any reverse proxy (nginx, Apache, Caddy, Traefik).
Two rules apply to every proxy setup:

1. Set `OXICLOUD_BASE_URL` to the public URL (e.g. `https://cloud.example.com`)
   so share links, OIDC callbacks and WOPI URLs are generated correctly, and
   set `OXICLOUD_COOKIE_SECURE=true` when the proxy terminates TLS.
2. Forward `X-Forwarded-Proto` and `X-Forwarded-Host` — the DPoP middleware
   reconstructs the browser-visible URL from them; without them every bound
   request fails with `dpop.verify_failed reason=wrong_htu`.

### Serving under a subpath

To serve OxiCloud under a URL prefix instead of a (sub)domain root — e.g.
`https://example.com/oxicloud` — set `OXICLOUD_BASE_PATH=/oxicloud` and include
the prefix in `OXICLOUD_BASE_URL` (`https://example.com/oxicloud`). That is the
whole configuration: the prefix is a runtime setting, so the same binary and the
same frontend build serve any prefix, and changing it is a restart.

The frontend carries no prefix. Its asset URLs are relative and the shell's
`<base href>` anchors them; the server fills that tag in from
`OXICLOUD_BASE_PATH` when it serves `index.html`. A shell built by a frontend
too old to carry the tag fails the boot with a rebuild hint.

The proxy must forward the prefix **unstripped** — the server expects to see
`/oxicloud/...` on the wire:

::: code-group

```nginx [nginx]
location /oxicloud {
    # No trailing slash on either side: the prefix is passed through as-is.
    proxy_pass http://127.0.0.1:8086;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    # WebSocket upgrade for /oxicloud/api/rt/ws (message bus)
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    client_max_body_size 0;
}
```

```apache [Apache]
# mod_proxy + mod_proxy_http + mod_proxy_wstunnel
ProxyPreserveHost On
RequestHeader set X-Forwarded-Proto "https"

# WebSocket upgrade (message bus) — upgrade=websocket handles it in-place
ProxyPass        /oxicloud http://127.0.0.1:8086/oxicloud upgrade=websocket
ProxyPassReverse /oxicloud http://127.0.0.1:8086/oxicloud
```

:::

Known limitation: RFC 6764 requires the CalDAV/CardDAV **autodiscovery**
endpoints `/.well-known/caldav` and `/.well-known/carddav` at the domain
root, which a subpath deployment cannot own. DAV clients still work when
given the full URL (`https://example.com/oxicloud/caldav/...`); to keep
autodiscovery, have the proxy redirect the two well-known paths to the
prefixed ones.

## Feature Dependency Matrix

| Feature | Requires DB | Requires Auth | Feature Flag |
|---|---|---|---|
| File storage | Yes | No | Always on |
| Authentication | Yes | — | `OXICLOUD_ENABLE_AUTH` |
| OIDC / SSO | Yes | Yes | `OXICLOUD_OIDC_ENABLED` |
| File sharing | Yes | Yes | `OXICLOUD_ENABLE_FILE_SHARING` |
| Trash | Yes | No | `OXICLOUD_ENABLE_TRASH` |
| Search | Yes | No | `OXICLOUD_ENABLE_SEARCH` |
| Favorites | Yes | Yes | Always on |
| Storage quotas | Yes | Yes | Per-user via admin panel (no master switch) |
| WebDAV | Yes | Optional | Always on |
| CalDAV / CardDAV | Yes | Yes | Always on |
| Deduplication | No | No | Always on |
| Thumbnails | No | No | Always on |
| Chunked uploads | No | No | Always on |
