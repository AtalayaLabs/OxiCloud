<p align="center">
  <img src="images/oxicloud-logo.png" alt="OxiCloud logo" width="360" />
</p>

<p align="center">
  A fast self-hosted cloud for people who want files, calendars, contacts, and office editing without dragging a heavy stack behind them.
</p>

<p align="center">
  <a href="https://AtalayaLabs.github.io/OxiCloud/">Documentation</a>
  ·
  <a href="#quick-start">Quick Start</a>
  ·
  <a href="https://github.com/AtalayaLabs/OxiCloud/stargazers">Star OxiCloud</a>
  ·
  <a href="https://github.com/AtalayaLabs/OxiCloud/issues/new?template=feature_request.md">Request a Feature</a>
  ·
  <a href="#supported-clients">Supported Clients</a>
  ·
  <a href="#project-status">Project Status</a>
</p>

<p align="center">
  <a href="https://github.com/AtalayaLabs/OxiCloud/releases"><img src="https://img.shields.io/github/release/AtalayaLabs/OxiCloud.svg?style=for-the-badge" alt="Latest release" /></a>
  <a href="https://github.com/AtalayaLabs/OxiCloud/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/AtalayaLabs/OxiCloud/ci.yml?branch=main&style=for-the-badge&label=CI" alt="CI" /></a>
  <a href="https://github.com/AtalayaLabs/OxiCloud/stargazers"><img src="https://img.shields.io/github/stars/AtalayaLabs/OxiCloud?style=for-the-badge&logo=github" alt="GitHub stars" /></a>
  <a href="https://hub.docker.com/r/AtalayaLabs/oxicloud"><img src="https://img.shields.io/docker/image-size/AtalayaLabs/oxicloud?style=for-the-badge&logo=docker" alt="Docker image size" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-1.93%2B-orange?style=for-the-badge&logo=rust" alt="Rust 1.93+" /></a>
  <a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/badge/License-MIT-blue.svg?style=for-the-badge" alt="MIT license" /></a>
</p>

<p align="center">
  If OxiCloud saves you setup time, RAM, or complexity, <a href="https://github.com/AtalayaLabs/OxiCloud/stargazers">give it a star</a>.
  If something is missing, <a href="https://github.com/AtalayaLabs/OxiCloud/issues/new?template=feature_request.md">ask for a feature</a> or <a href="https://github.com/AtalayaLabs/OxiCloud/issues/new?template=documentation_request.md">request a docs improvement</a>.
</p>

![OxiCloud dashboard](<images/Captura de pantalla 2026-05-28 a las 22.35.08.png>)

## Why People Try OxiCloud

OxiCloud is aimed at self-hosters, home labs, and small teams who want the useful parts of a cloud suite without the operational drag of a traditional PHP stack.

What pulls people in:

- Standard protocols first: WebDAV, CalDAV, and CardDAV are built in
- Useful product surface already there: files, previews, sharing, trash, search, favorites, and recent items
- Modern auth and admin basics: OIDC/SSO, quotas, roles, and shared links
- Better interoperability: native desktop and mobile clients work without custom sync tooling for basic access
- Lower deployment friction: Docker Compose, environment-based configuration, Helm chart, and Nix module

> OxiCloud is not trying to mirror the full plugin ecosystem of Nextcloud. It is designed for a smaller stack, fast startup, and standards-based interoperability.

## Quick Start

### Docker Compose

Requires Docker and Docker Compose.

```bash
git clone https://github.com/AtalayaLabs/OxiCloud.git
cd OxiCloud
cp example.env .env

# If users will access OxiCloud through a domain or reverse proxy,
# set OXICLOUD_BASE_URL in .env before the first login.
docker compose up -d
```

Open `http://localhost:8086`.

### Prebuilt binary

Binary releases (Linux musl amd64/arm64, macOS Intel/Apple Silicon)
are attached to every tagged release on GitHub — the whole SPA + all
operator subcommands + migrations bake into a single self-contained
executable. See [`docs/install/binary.md`](docs/install/binary.md) for
the download / verify / systemd walkthrough.

`cargo binstall oxicloud` works too once a release is out.

### Run from source

Requires Rust 1.93+ and PostgreSQL.

```bash
git clone https://github.com/AtalayaLabs/OxiCloud.git
cd OxiCloud
cp example.env .env

# If PostgreSQL runs on your host instead of Docker, update both
# OXICLOUD_DB_CONNECTION_STRING and DATABASE_URL to use localhost:5432.
cargo run
```

Deployment details: [deployment guide](docs/config/deployment.md) · [example.env](example.env)

## What You Get

| Area | Included |
| --- | --- |
| Files | Multi-file upload, folders, inline previews, thumbnails, chunked uploads, deduplication, trash |
| Sync and clients | WebDAV, CalDAV, CardDAV, native OS clients, Thunderbird, DAVx5 |
| Security | JWT auth, Argon2id, OIDC/SSO, shared links, quotas, admin/user roles |
| Integrations | REST API and WOPI for Collabora or OnlyOffice |
| Operations | Docker image, Docker Compose, env-driven config, PostgreSQL backend |
| Project tooling | Architecture docs, Helm chart, Nix module, CI |

## Supported Clients

OxiCloud uses standard DAV protocols, so it works with native clients instead of requiring a custom sync stack for basic access.

| Use case | URL |
| --- | --- |
| Files via WebDAV | `https://your-host/webdav/` |
| Calendars via CalDAV | `https://your-host/caldav/` |
| Contacts via CardDAV | `https://your-host/carddav/` |

Common clients that work well:

- macOS Finder
- Windows Explorer
- GNOME Files and KDE Dolphin
- Thunderbird
- Apple Calendar and Contacts
- DAVx5 on Android

Client setup guides: [DAV client setup](docs/guide/dav-client-setup.md) · [WebDAV guide](docs/guide/webdav.md) · [CalDAV & CardDAV guide](docs/guide/caldav-carddav.md)

## Project Status

OxiCloud is actively developed and already covers the core self-hosted cloud workflow.

| Capability | Status | Notes |
| --- | --- | --- |
| File storage and web UI | Ready | Uploads, previews, sharing, trash, and search |
| WebDAV | Ready | Standard file access for desktop and mobile clients |
| CalDAV and CardDAV | Ready | Working with Thunderbird, Apple clients, and others |
| OIDC / SSO | Ready | Documentation and config examples included |
| WOPI office editing | Ready | Works with Collabora or OnlyOffice |
| DAVx5 Android support | Partial | File sync works well; calendar and contact behavior is still being refined |
| Desktop sync client | Planned | Not yet available |
| Mobile apps | Planned | Not yet available |
| End-to-end encryption | Planned | Roadmap item |

Roadmap: [TODO-LIST.md](TODO-LIST.md)

## Help Shape OxiCloud

If you want OxiCloud to get better faster, use the repo like a product feedback loop, not just a code dump.

- Give it a star if you want more people to discover the project: [Star OxiCloud](https://github.com/AtalayaLabs/OxiCloud/stargazers)
- Propose missing functionality: [feature request](https://github.com/AtalayaLabs/OxiCloud/issues/new?template=feature_request.md)
- Point out confusing onboarding or weak docs: [documentation request](https://github.com/AtalayaLabs/OxiCloud/issues/new?template=documentation_request.md)
- Report breakage or regressions: [bug report](https://github.com/AtalayaLabs/OxiCloud/issues/new?template=bug_report.md)
- Build it with us: [CONTRIBUTING.md](CONTRIBUTING.md)

The best feature ideas usually come from real deployment pain. If you hit friction, open an issue and describe the workflow you want.

## Architecture and Deployment

OxiCloud follows a clean, hexagonal architecture so protocol handlers, business logic, and infrastructure stay separated.

- Backend: Rust + Axum
- Database: PostgreSQL
- Configuration: environment variables
- Default deployment: Docker Compose
- Additional packaging: [Helm chart](charts/oxicloud) and [Nix module](tools/nix/module.nix)

Architecture docs: [internal architecture](docs/architecture/index.md) · [caching architecture](docs/architecture/caching.md) · [database transactions](docs/architecture/database-transactions.md) · [storage safety](docs/architecture/file-system-safety.md)

## Configuration and Integrations

Start with [example.env](example.env). The most important settings are:

- `OXICLOUD_BASE_URL` for reverse proxies, domains, and external access
- `OXICLOUD_DB_CONNECTION_STRING` for PostgreSQL
- `OXICLOUD_OIDC_ENABLED` and related settings for SSO
- `OXICLOUD_WOPI_ENABLED` and discovery URL for office editing
- `MIMALLOC_PURGE_DELAY=0` for lower idle RSS in constrained environments

Integration docs: [OIDC setup](docs/config/oidc.md) · [OIDC architecture](docs/config/oidc.md) · [OIDC config examples](docs/config/oidc-config-examples.md) · [WOPI integration](docs/config/wopi.md)

## Documentation

- Docs site: [AtalayaLabs.github.io/OxiCloud](https://AtalayaLabs.github.io/OxiCloud/)
- Deployment: [docs/config/deployment.md](docs/config/deployment.md)
- Batch operations: [docs/guide/batch-operations.md](docs/guide/batch-operations.md)
- Search: [docs/guide/search.md](docs/guide/search.md)
- Thumbnails and transcoding: [docs/guide/thumbnails-and-transcoding.md](docs/guide/thumbnails-and-transcoding.md)
- Deduplication: [docs/guide/deduplication.md](docs/guide/deduplication.md)

## Development

```bash
cargo fmt --all --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
```

## Contributing

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for community expectations.

If you are not ready to code yet, starring the project and opening a precise feature request is still a meaningful contribution.

<div align="center">

## Contributors

OxiCloud is a community-driven project, and we appreciate all contributions. Check out the [Contributors](https://github.com/AtalayaLabs/OxiCloud/graphs/contributors) page to see the amazing people who have helped make OxiCloud better.

<a href="https://github.com/AtalayaLabs/OxiCloud/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=AtalayaLabs/OxiCloud" alt="Contributors" />
</a>

## Star History

<a href="https://star-history.dera.page/#AtalayaLabs/OxiCloud&Date">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://star-history.dera.page/svg?repos=AtalayaLabs/OxiCloud&type=Date&theme=dark" />
   <source media="(prefers-color-scheme: light)" srcset="https://star-history.dera.page/svg?repos=AtalayaLabs/OxiCloud&type=Date" />
   <img alt="Star History Chart" src="https://star-history.dera.page/svg?repos=AtalayaLabs/OxiCloud&type=Date" />
 </picture>
</a>

## License

[MIT](https://opensource.org/licenses/MIT). See [LICENSE](LICENSE).

**OxiCloud** is a trademark of the OxiCloud project. All other trademarks are the property of their respective owners.

</div>
