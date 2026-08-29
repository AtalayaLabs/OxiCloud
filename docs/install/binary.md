# Installing OxiCloud from a Binary Release

OxiCloud ships prebuilt binaries for common Linux and macOS platforms
attached to every tagged release on GitHub. This page covers downloading,
verifying, and running one.

If you'd rather run OxiCloud as a container, see the Docker image at
`ghcr.io/atalayalabs/oxicloud`. If you're a Rust developer who just
wants the binary without hand-fetching a tarball, `cargo binstall
oxicloud` picks the right archive for your host automatically.

## Which tarball do I want?

Every release attaches three tarballs plus a `SHA256SUMS` manifest.
Pick by your host's architecture and OS:

| Host | Tarball |
|---|---|
| Linux x86-64 (Intel / AMD servers, most VPS, WSL) | `oxicloud-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux ARM64 (Raspberry Pi 4/5, Ampere, Graviton, ARM servers) | `oxicloud-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple Silicon (M-series) | `oxicloud-<version>-aarch64-apple-darwin.tar.gz` |

The Linux tarballs link against musl, so they run on ANY glibc version
— Alpine, Debian, Ubuntu, Fedora, Arch, Rocky, and every version in
between. You never need to worry about `GLIBC_x.yy not found`.

**Intel macOS, Windows, and 32-bit ARM are not currently shipped as
prebuilt tarballs.** Intel Mac users have three fallbacks:

1. `cargo install oxicloud --locked --features bundled-assets` from
   source (needs the Rust toolchain).
2. Docker: `docker pull --platform linux/amd64 ghcr.io/atalayalabs/oxicloud`.
3. Run one of the two Linux musl tarballs inside a Linux VM
   (Multipass, Lima, UTM, etc.).

## Hardware notes

| Model | Notes |
|---|---|
| Pi 5 (4 GB / 8 GB) | Good experience |
| Pi 4 (4 GB / 8 GB) | Solid |
| Pi 4 (2 GB) | Works with face indexing disabled; expect swap under load |
| Pi 3 (any variant) | Marginal — only for a very light single-user personal cloud |
| Pi 2 / Pi Zero / Pi 1 | Not supported (1 GB RAM is below the practical floor) |
| Any ARM64 server | Good — the aarch64 tarball is what you want |
| Any x86-64 server from 2010 or newer | Good — Nehalem / Bulldozer + newer, per the release CPU baseline |

## Verifying the download

Every release ships a `SHA256SUMS` manifest listing every tarball with
its hash. Verify your download before extracting:

```
sha256sum -c SHA256SUMS
```

Only files present in the current directory are checked, so this
succeeds when just the tarball you downloaded matches its entry.

## Extracting

The archive lands as a per-version-per-triple directory next to it:

```
tar xzf oxicloud-<version>-<triple>.tar.gz
cd oxicloud-<version>-<triple>/
ls
# oxicloud  example.env  LICENSE  README-install.md
```

The four files:

- `oxicloud` — the single self-contained binary. The server, all
  operator subcommands (`oxicloud opaque setup`, `oxicloud migrate
  nfc-filenames`, `oxicloud storage select`), and the SvelteKit web
  frontend are all baked in.
- `example.env` — every OxiCloud environment variable documented with
  defaults. Copy to `.env` and edit as needed.
- `LICENSE` — the project license.
- `README-install.md` — a shorter version of this page for offline
  reference.

## Prerequisites

Only one moving part is required: a PostgreSQL 13+ instance with the
`pg_trgm` and `ltree` extensions available. Anything else you might
need is either baked into the binary or optional.

### Required

- **PostgreSQL 13+** with `pg_trgm` and `ltree` extensions. Any distro
  package works (Debian/Ubuntu's `postgresql`, Alpine's `postgresql`,
  Homebrew's `postgresql@17`, etc.). Cloud databases like Neon,
  Supabase, and RDS also work provided the two extensions are enabled.

### System libraries (usually pre-installed)

- **`ca-certificates`** — for outbound HTTPS (OIDC discovery, S3, magic
  links). Pre-installed on essentially every distribution.
- **`tzdata`** — timezone database. Pre-installed on nearly every
  distribution; alpine minimal images sometimes need it added.

### Optional

- **`ffmpeg`** — only needed if you want the server to extract a
  thumbnail frame from uploaded videos. When ffmpeg is missing the
  server logs a warning at boot and videos get a placeholder icon —
  everything else keeps working. If your client uploads video
  previews itself (some desktop and mobile clients do), or if you
  simply don't want thumbnails, set
  `OXICLOUD_ENABLE_VIDEO_THUMBNAILS=false` in your `.env` to silence
  the warning.

Distro install commands for the optional prerequisite:

| Distro | Command |
|---|---|
| Alpine | `apk add ffmpeg` |
| Debian / Ubuntu | `apt install ffmpeg` |
| Fedora / RHEL | `dnf install ffmpeg` (RPM Fusion for the full codec set) |
| Arch | `pacman -S ffmpeg` |
| macOS | `brew install ffmpeg` |
| Any Linux (portable) | grab a static build from https://github.com/BtbN/FFmpeg-Builds/releases and point `OXICLOUD_FFMPEG_PATH` at it |

## First run

The absolute minimum to boot the server is `DATABASE_URL`:

```
DATABASE_URL="postgres://oxicloud:secret@localhost:5432/oxicloud" \
    ./oxicloud
```

The binary applies its embedded database migrations on startup, then
listens on `127.0.0.1:8086` by default. Open your browser at
`http://localhost:8086/` and follow the setup flow to create the first
admin account.

For anything more than a smoke test, copy `example.env` to `.env`,
edit it, and run `./oxicloud --config .env` — that pins the config
source and makes stray shell environment variables not silently leak
in.

## Running as a systemd service (Linux)

Move the binary to a system location and create a systemd unit. The
example below runs as a dedicated `oxicloud` user, loads config from
`/etc/oxicloud/oxicloud.env`, and stores data under `/var/lib/oxicloud`.

```
sudo useradd --system --home /var/lib/oxicloud --create-home --shell /usr/sbin/nologin oxicloud
sudo install -m 0755 oxicloud /usr/local/bin/oxicloud
sudo mkdir -p /etc/oxicloud
sudo cp example.env /etc/oxicloud/oxicloud.env
sudo chown -R oxicloud:oxicloud /etc/oxicloud
sudo chmod 0640 /etc/oxicloud/oxicloud.env
```

Create `/etc/systemd/system/oxicloud.service`:

```
[Unit]
Description=OxiCloud self-hosted cloud storage
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=simple
User=oxicloud
Group=oxicloud
WorkingDirectory=/var/lib/oxicloud
ExecStart=/usr/local/bin/oxicloud --config /etc/oxicloud/oxicloud.env
Restart=on-failure
RestartSec=5

# Sandbox — plenty of room to tighten further per your policy
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/oxicloud
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

Enable and start:

```
sudo systemctl daemon-reload
sudo systemctl enable --now oxicloud
sudo systemctl status oxicloud
journalctl -u oxicloud -f
```

Terminate the reverse-proxy (nginx, Caddy, HAProxy, Traefik) in front
of it for TLS and public exposure — OxiCloud itself binds plaintext
HTTP on `127.0.0.1` by default.

## Upgrading

Replace the binary and restart the service:

```
# Download and verify the new tarball
sha256sum -c SHA256SUMS
tar xzf oxicloud-<new-version>-<triple>.tar.gz
cd oxicloud-<new-version>-<triple>/

sudo systemctl stop oxicloud
sudo install -m 0755 oxicloud /usr/local/bin/oxicloud
sudo systemctl start oxicloud
```

Database migrations apply automatically on startup. Rollbacks are not
supported by sqlx's migration model; if you need to roll back, stop
the server, roll back your Postgres data directory to a snapshot, and
install the previous binary.

## Installing via `cargo binstall`

If you already have the Rust toolchain and just want the binary
without hand-picking a tarball:

```
cargo binstall oxicloud
```

`cargo-binstall` reads the URL template baked into the release
metadata, downloads the tarball for your host triple, verifies its
signature (when present), and installs `oxicloud` into
`~/.cargo/bin`. This resolves to the same tarball you'd download by
hand.

## Where to go from here

- Environment reference — see [`docs/config/env.md`](../config/env.md)
  for every `OXICLOUD_*` variable and its default.
- Authentication setup (OPAQUE, OIDC, magic links) — see
  [`docs/config/authentication.md`](../config/authentication.md).
- Storage backends (local disk, S3, Azure Blob, encryption) — see
  [`docs/config/storage.md`](../config/storage.md) if present, or the
  entries under `OXICLOUD_STORAGE_*` in the environment reference.
- File a bug or a feature request — GitHub issues at
  https://github.com/AtalayaLabs/OxiCloud.
