# What is OxiCloud?

OxiCloud is a self-hosted cloud platform written in Rust. It provides file storage, calendar sync (CalDAV), contacts sync (CardDAV), and office document editing (WOPI) — all from a single binary.

NextCloud was too slow on a home server. So OxiCloud was built to run on minimal hardware and stay out of the way.

## OxiCloud vs NextCloud

| Metric | OxiCloud | NextCloud |
|--------|----------|-----------|
| **Language** | Rust (compiled, zero-cost abstractions) | PHP (interpreted) |
| **Docker image** | ~40 MB (Alpine, static binary) | ~1 GB+ (Apache + PHP + modules) |
| **Idle RAM** | ~30–50 MB | ~250–512 MB |
| **Cold start** | < 1 s | 5–15 s |
| **CPU at idle** | ~0 % | 1–5 % (cron, background jobs) |
| **Min. hardware** | 1 vCPU / 512 MB RAM | 2 vCPU / 2 GB RAM |
| **File dedup** | BLAKE3 content-addressable | None |
| **Dependencies** | Single binary + PostgreSQL | PHP, Apache/Nginx, Redis, Cron, … |
| **WebDAV** | Built-in (RFC 4918) | Built-in |
| **CalDAV / CardDAV** | Built-in | Via apps |
| **WOPI** | Built-in | Via apps |
| **OIDC / SSO** | Built-in | Via apps |

> NextCloud is a mature, feature-rich ecosystem. OxiCloud targets users who prioritise raw performance, simplicity, and low resource usage over plugin breadth.

## Key Features

### Storage & Files
- [Drives](/guide/drives) — Personal + Shared spaces with per-drive quota, members, and policies
- [Backend Storage](/guide/backend-storage) — Local disk, S3, Azure; multiple backends side-by-side; live migration between them
- Drag-and-drop upload, multi-file, grid & list views
- Chunked uploads (TUS-like, parallel, resumable, MD5 integrity)
- BLAKE3 content-addressable file deduplication with ref-counting
- Adaptive compression (zstd / gzip per MIME type)
- Trash bin with soft-delete and auto-purge
- Favourites, recent files, full-text search
- Inline preview for images, PDF, text, audio & video
- On-the-fly thumbnails & transcoding (WebP / AVIF)

### Protocols
- **WebDAV** — RFC 4918, streaming PROPFIND, locking
- **CalDAV** — calendar sync (Thunderbird, GNOME Calendar, iOS, DAVx⁵)
- **CardDAV** — contacts sync with vCard support
- **WOPI** — Collabora Online / OnlyOffice
- **REST API** — complete JSON API

### Security
- JWT + Argon2id password hashing
- OIDC / SSO (Keycloak, Authentik, Authelia, Google, Azure AD)
- Role-based access, per-folder permissions, storage quotas
- Shared links with optional password protection

### Infrastructure
- Single binary, ~40 MB Docker image
- Dual DB pool (user queries never starved by background tasks)
- Write-behind caching (moka) for sub-millisecond reads
- LTO-optimised release builds
- 222+ automated tests

## Feature Status

| Feature | Status |
|---------|--------|
| File storage & upload | ✅ Working |
| WebDAV | ✅ Working |
| CalDAV | ✅ Working |
| CardDAV | ✅ Working |
| WOPI / Office editing | ✅ Working |
| OIDC / SSO | ✅ Working |
| Trash / recycle bin | ✅ Working |
| Full-text search | ✅ Working |
| Shared links | ✅ Working |
| Music library & playlists | ✅ Working |
| Photo gallery | ✅ Working |
| Desktop sync client | ❌ Planned |
| Android / iOS app | ❌ Planned |
| E2E encryption | ❌ Planned |

## Next Steps

- [Quick Start →](/guide/installation)
- [Deployment & Docker →](/config/deployment)
- [Architecture →](/architecture/)
