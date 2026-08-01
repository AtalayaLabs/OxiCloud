# Backend Storage

OxiCloud writes uploaded files to a **backend storage** — the physical place where the bytes actually live. Three kinds are supported:

- **Local disk** — a folder on the server.
- **S3-compatible** — AWS S3, OVH, MinIO, Backblaze B2, Cloudflare R2, DigitalOcean Spaces, Wasabi.
- **Azure Blob Storage** — Microsoft Azure.

You can declare more than one backend at a time (for example, keep the current disk backend while adding an S3 target), pick which one is currently in use from the admin panel, and migrate all your data between them without downtime.

## Declaring backends

Backends are declared in your **`.env`** file (or the equivalent environment variables in a Docker Compose / Kubernetes deployment). Each backend gets a short **name** you choose — like `local_main`, `s3_prod`, `s3_archive` — and its own set of settings under that name.

Example: one local backend today and an S3 backend ready for a future migration.

```
OXICLOUD_STORAGE_ENTRIES=local_main,s3_prod

OXICLOUD_STORAGE_local_main_BACKEND=local
OXICLOUD_STORAGE_local_main_ROOT_DIR=/srv/oxicloud

OXICLOUD_STORAGE_s3_prod_BACKEND=s3
OXICLOUD_STORAGE_s3_prod_S3_BUCKET=my-oxicloud-bucket
OXICLOUD_STORAGE_s3_prod_S3_REGION=gra
OXICLOUD_STORAGE_s3_prod_S3_ENDPOINT_URL=https://s3.gra.io.cloud.ovh.net
OXICLOUD_STORAGE_s3_prod_S3_ACCESS_KEY=…
OXICLOUD_STORAGE_s3_prod_S3_SECRET_KEY=…
```

A few things to know:

- The first entry in `OXICLOUD_STORAGE_ENTRIES` is used on first boot if you haven't picked one from the admin panel yet.
- Names must be short (letters, digits, `_`, `-`), unique, and stable — once you pick a name, keep it.
- After editing `.env`, **restart the server** so it picks up the new declaration. The declaration is what unlocks the entry in the admin panel — from then on you can switch to it (and move data to it) without another restart.
- The full list of settings each backend type accepts lives in the [Environment Variables reference](/config/env#storage-entries-multi-entry-recommended).

### Encryption at rest

Any backend can be encrypted at rest by adding an encryption key to it:

```
OXICLOUD_STORAGE_s3_prod_ENCRYPTION_KEY=…   # base64 of 32 random bytes
```

A key can be generated from **Settings → Storage → Generate key** in the admin panel. Set the same key on a new backend during migration and OxiCloud re-encrypts the data as it copies.

::: warning
If you lose the encryption key, the data encrypted with it is unrecoverable. Store the key somewhere as safe as you'd store a database backup.
:::

## Checking a backend

Once a backend is declared, it appears on the admin **Storage** tab as a card:

- **Location** — the folder path or `endpoint / bucket`.
- **Encryption** — a lock icon when a key is set for this entry.
- **Status** — either **active** (the one currently in use) or **available** (declared but not in use yet).

Each card has three buttons:

- **Test** — connects to the backend and does a small write / read / delete round-trip. On success it reports the round-trip time. On failure it tells you exactly what went wrong (bad credentials, wrong region, missing permission, unreachable host). Nothing persists on the backend after the test.
- **Blob consistency** — runs a full integrity audit against that backend. It verifies every file OxiCloud knows about is present on this backend and reports any missing pieces on the Jobs tab.
- **Migrate & activate** — visible on non-active backends. Moves all data to this backend and makes it the new active one. Details below.

## Migrating between backends

Migration copies every file from the currently-active backend to a chosen target backend, then switches the app over to the target. The typical flow:

1. Add the new backend to your `.env` **without removing the current one**.
2. Restart the server so the new backend becomes visible in the admin panel.
3. On the **Storage** tab, click **Test** on the new backend to confirm it's reachable and writable.
4. Click **Migrate & activate** on the new backend and confirm the prompt.

During the migration:

- The server enters **read-only mode**. Users can still browse and download files; uploads, renames, deletes, and shares are refused until the migration finishes. A banner at the top of the Storage tab reminds you.
- Progress is shown on the migration status line under the entries list — how many blobs have been copied, the estimated time remaining.
- If the migration fails partway (network drops, quota exceeded), it pauses rather than losing progress. Clicking **Migrate & activate** again resumes from where it stopped.

When the migration reaches 100%:

- The new backend automatically takes over as the active one.
- Read-only mode is lifted. Users can write again — everything now goes to the new backend.
- No restart is required.

The old backend is left untouched. Nothing is deleted from it. Once you're confident the new backend is holding up, you can decommission the old one at your own pace (empty the old S3 bucket, unmount the old disk, etc.).

### Migrations that are refused

The admin panel refuses to start a migration in two cases:

- **Target equals source** — pointing a migration at the currently-active backend does nothing useful.
- **Target and source share the same physical storage** — for example, two backends that name the same S3 bucket but with different credentials or encryption keys. This would corrupt the data mid-migration. If you're trying to rotate an encryption key, migrate to a **different** bucket first, then rotate.

### Verifying a migration

After a migration completes, the "Blob consistency" button on the new backend runs a full audit. It walks every file OxiCloud knows about and confirms it's really present on the backend, byte-for-byte. Results appear on the Jobs tab. A clean run confirms the migration was complete.

## Repairing a stuck config

If you rename or remove a backend from `.env` while it was still the active one, the server may refuse to boot with an error like:

```
active_backend_name = `s3_prod`, but no entry with that name is declared in
OXICLOUD_STORAGE_ENTRIES. Available: [local_main]. […]
oxicloud --select-storage <one-of-the-available-names>
```

Run the command it suggests to pick a still-declared backend and the server will boot again on the next start:

```
oxicloud --select-storage local_main
```

This just updates which backend OxiCloud considers active — it doesn't move any data.

## Common gotchas

- **S3 region must match the endpoint.** Every S3-compatible provider signs requests against a specific region string. `us-east-1` is right for real AWS S3 but wrong for OVH (`gra`, `sbg`, etc.), Backblaze B2, Wasabi, and others. Check your provider's docs for the exact region name.
- **`FORCE_PATH_STYLE=true` for non-AWS.** Path-style URLs (`endpoint/bucket/…`) are safer with providers whose bucket-name DNS setup isn't standard, or with bucket names that contain dots.
- **Test before migrating.** The **Test** button on each backend does a proper write/read/delete round-trip — a green result means the credentials and permissions are correct for the operations a migration actually needs. Don't skip it.
- **Keep the old backend around** until at least one full `blob consistency` audit passes on the new one. Cheap insurance.
