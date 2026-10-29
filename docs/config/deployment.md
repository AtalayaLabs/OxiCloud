# Deployment & Docker

## Docker Image

OxiCloud uses a multi-stage Alpine build producing a **~40 MB** image:

1. **Base** — shared build dependencies (`musl-dev`, `pkgconfig`, `openssl-dev`, `libpq-dev`)
2. **Cacher** — pre-builds the dependency layer for fast rebuilds
3. **Builder** — compiles OxiCloud (`rust:1.94.0-alpine3.23`)
4. **Runtime** — minimal Alpine (`alpine:3.23.3`) with `libgcc`, `ca-certificates`, `libpq`, `tzdata`, `su-exec`

The final image runs as non-root user `oxicloud` (UID/GID 1001 by default — overridable via `PUID`/`PGID`, see [Matching the storage volume's UID/GID](#matching-the-storage-volumes-uidgid) below). Exposed port: **8086**.

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

### Matching the storage volume's UID/GID

By default the container runs the application as `oxicloud:oxicloud` (UID/GID **1001:1001**). The entrypoint runs as root on first boot, chowns `/app/storage` + `/app/static` to that user, then drops privileges via `su-exec`.

On NAS deployments the host-side storage dataset is often already owned by a specific non-1001 UID that can't easily be changed (TrueNAS Scale apps run as `568:568`, Synology Container Manager assigns `1026:100`, Unraid uses whatever the share's `nobody:users` mapping says). Mounting such a dataset into `/app/storage` leaves the server unable to write to it — the application exits silently after service initialization and the container restarts in a loop.

Two ways to fix:

**Option 1 — Set `PUID` / `PGID` env vars (recommended)**. The entrypoint rewrites the oxicloud user's UID/GID to match before the chown. Idempotent across reboots.

```yaml
  oxicloud:
    image: diocrafts/oxicloud:latest
    environment:
      PUID: 568          # TrueNAS Scale app UID
      PGID: 568          # TrueNAS Scale app GID
      # … your OXICLOUD_* vars
    volumes:
      - /mnt/pool/oxicloud-data:/app/storage
```

**Option 2 — Use the Docker `user:` directive**. The container starts as the specified UID (not root), the entrypoint sees that and skips both the UID rewrite and the chown — you take responsibility for ensuring the host-side volume is already correctly owned.

```yaml
  oxicloud:
    image: diocrafts/oxicloud:latest
    user: 568:568
    volumes:
      - /mnt/pool/oxicloud-data:/app/storage   # must be chowned 568:568 on the host first
```

`PUID`/`PGID` is the more forgiving path and the one to use unless you have a specific reason to run the container unprivileged from PID 1. The two mechanisms are compatible: if both are set, the Docker `user:` directive wins (the entrypoint runs as that user, skips the UID rewrite).

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

## Outbound TLS and Trust Store

Every outbound TLS connection OxiCloud makes — to PostgreSQL, SMTP, OIDC providers, webhook endpoints, Azure Blob Storage, S3-compatible storage (AWS, MinIO, Backblaze B2, Cloudflare R2, Wasabi…), WOPI editor discovery (Collabora Online / OnlyOffice), and plugin-manifest HTTP fetches — validates certificates against the **operating system's native trust store**.

OxiCloud does not ship a bundled Mozilla root list. There is no OxiCloud-side CA configuration knob. The trust store is whatever the host OS says it is.

### What this gives you

- **Internal / corporate CAs work out of the box.** Add your CA to the OS trust store once, and OxiCloud trusts it for *every* outbound TLS surface — PG, SMTP, OIDC, webhooks, Azure, S3 — without any OxiCloud configuration change.
- **CA distrust propagates automatically.** When a root CA gets compromised and the OS vendor ships a `ca-certificates` package update marking it distrusted, OxiCloud picks that up on the next process restart. No OxiCloud release required.
- **One trust story across the whole stack.** The CAs trusted by your PG client, your `curl` test, and OxiCloud are the same set — fewer "it works in one place but not the other" mysteries.

### Adding an internal CA — by deployment shape

The CA file must be **PEM-encoded** (base64 between `-----BEGIN CERTIFICATE-----` and `-----END CERTIFICATE-----`). If your CA is in DER (binary), convert it first:

```bash
openssl x509 -inform der -in root.der -out root.crt
```

Pick the recipe that matches how you run OxiCloud.

#### Linux host (bare-metal binary or systemd service)

```bash
# Debian / Ubuntu / Alpine — PEM goes under /usr/local/share/ca-certificates/
# with a .crt extension
sudo cp my-internal-root.crt /usr/local/share/ca-certificates/
sudo update-ca-certificates

# RHEL / Fedora / CentOS / Rocky — different directory, different refresh
sudo cp my-internal-root.pem /etc/pki/ca-trust/source/anchors/
sudo update-ca-trust

# Restart OxiCloud so the trust store is re-read on process start
sudo systemctl restart oxicloud
```

#### Docker (recommended — rebuild the image)

```dockerfile
FROM atalayalabs/oxicloud:latest
COPY my-internal-root.crt /usr/local/share/ca-certificates/
RUN update-ca-certificates
```

```bash
docker build -t oxicloud-internal .
docker run -d --name oxicloud oxicloud-internal
```

Clean separation: the CA is baked into your downstream image, no runtime mounts to re-create after each `docker rm`.

#### Docker (quick — bind-mount the full bundle)

Build a bundle on the host once, mount it over the one inside the container:

```bash
# Append your CA to a copy of the system bundle
cat /etc/ssl/certs/ca-certificates.crt my-internal-root.crt \
  > /etc/oxicloud/ca-bundle.crt

# Mount it over the container's bundle
docker run -d --name oxicloud \
  -v /etc/oxicloud/ca-bundle.crt:/etc/ssl/certs/ca-certificates.crt:ro \
  atalayalabs/oxicloud:latest
```

No custom image build. Trade-off: regenerate the bundle whenever the host's `ca-certificates` package updates — otherwise you lose Mozilla distrust propagation.

#### Docker Compose

```yaml
services:
  oxicloud:
    image: atalayalabs/oxicloud:latest
    volumes:
      - /etc/oxicloud/ca-bundle.crt:/etc/ssl/certs/ca-certificates.crt:ro
    # …rest of the service config
```

#### Kubernetes (Helm chart)

Mount your CA bundle as a `Secret` or `ConfigMap` into the pod's trust path. The Helm chart (`charts/oxicloud`) exposes `extraVolumes` / `extraVolumeMounts` for this:

```yaml
extraVolumes:
  - name: internal-ca
    secret:
      secretName: oxicloud-internal-ca   # kubectl create secret generic …
                                          #   --from-file=ca-bundle.crt=/path/…
extraVolumeMounts:
  - name: internal-ca
    mountPath: /etc/ssl/certs/ca-certificates.crt
    subPath: ca-bundle.crt
    readOnly: true
```

### Minimal / distroless deployments

OxiCloud's binary expects a populated OS trust store. If you deploy on a minimal base image that does not include CA certificates (scratch, distroless, some Alpine variants), outbound TLS fails with `certificate verify failed, no issuer certificate`. The fix is a one-line package install:

- **Alpine** / Alpine-based Docker image: `apk add --no-cache ca-certificates`
- **Debian slim / distroless-cc**: already includes `ca-certificates`
- **scratch**: copy `/etc/ssl/certs/ca-certificates.crt` from a builder stage, or switch to `distroless-cc`

The official OxiCloud Docker image already installs `ca-certificates` — no action needed if you use it as a base.

### Verifying your CA is trusted

The simplest smoke test is PG with full certificate verification. Point `OXICLOUD_DB_CONNECTION_STRING` at a PG whose cert chains to your internal CA with `sslmode=verify-full`:

```
OXICLOUD_DB_CONNECTION_STRING=postgres://user:pass@internal-pg.corp.lan:5432/oxicloud?sslmode=verify-full
```

Then start OxiCloud:

- **Clean startup** → trust store sees your CA. Done.
- **`certificate verify failed, unable to get local issuer certificate`** → the CA isn't where OxiCloud is looking. Re-check: PEM format, correct trust-store directory for the distribution, refresh command ran successfully, container restarted after the cert was installed.
- **`certificate verify failed, Hostname mismatch`** → trust store is fine; your cert's `subjectAltName` doesn't match the hostname in the connection string. OxiCloud trust is working — fix the cert.

The same recipe works for every other outbound surface. Replace the PG URL with any of these to verify the matching surface against your internal CA:

| Surface | Env var pointing at an internal-TLS endpoint |
|---|---|
| **S3-compatible storage** | `OXICLOUD_S3_ENDPOINT_URL` (and `OXICLOUD_STORAGE_BACKEND=s3`) |
| **Azure Blob storage** | `OXICLOUD_AZURE_ENDPOINT_URL` |
| **OIDC** | `OXICLOUD_OIDC_ISSUER_URL` |
| **SMTP** | `OXICLOUD_SMTP_HOST` + `OXICLOUD_SMTP_PORT` |
| **Webhooks** | `OXICLOUD_WEBHOOK_URL` |
| **WOPI (Collabora / OnlyOffice)** | `OXICLOUD_WOPI_DISCOVERY_URL` |

If one surface works and another fails against the same internal CA, the gap is not OxiCloud's trust story — all eight surfaces share the OS trust store at runtime. Look at the cert chain for the failing endpoint specifically (`openssl s_client -connect host:port -showcerts`).

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
