# Backend Storage

Reference for implementors of storage backends, encryption layers,
consistency checks, and migration/rotation jobs.

The user-facing "how to configure S3" guide lives in
[`docs/config/env.md`](../config/env.md); the operational plan
history is in [`docs/plan/storage-multi-entry.md`](../plan/storage-multi-entry.md)
and [`docs/plan/storage-key-rotation.md`](../plan/storage-key-rotation.md).
This page is the "what the code actually does and why" reference.

---

## Supported backends

Three concrete implementations of `BlobStorageBackend` ship in the
tree today. All go through the same `EncryptedBlobBackend` wrapper
(see §6) so encryption, header format, BLAKE3 rescue, smart-skip
probe, and lifecycle behaviour are uniform.

| Backend | `StorageBackendType` | Env key prefix | Config surface | Impl |
|---|---|---|---|---|
| **Local filesystem** | `Local` | `OXICLOUD_STORAGE_<name>_ROOT_DIR` | Root directory on the host FS. Shard tree `<root>/.blobs/<xx>/`. Atomic replace via tempfile + `rename(2)`. | [`local_blob_backend.rs`](../../src/infrastructure/services/local_blob_backend.rs) |
| **S3-compatible** | `S3` | `OXICLOUD_STORAGE_<name>_BUCKET` + `_REGION` + `_ACCESS_KEY` + `_SECRET_KEY` + optional `_ENDPOINT_URL` + `_FORCE_PATH_STYLE` | AWS S3, Cloudflare R2, Backblaze B2, MinIO, DigitalOcean Spaces, Wasabi — anything speaking the S3 API. | [`s3_blob_backend.rs`](../../src/infrastructure/services/s3_blob_backend.rs) |
| **Azure Blob Storage** | `Azure` | `OXICLOUD_STORAGE_<name>_ACCOUNT_NAME` + `_ACCOUNT_KEY` (or `_SAS_TOKEN`) + `_CONTAINER` + optional `_ENDPOINT_URL` | Azure Blob Storage; `_ENDPOINT_URL` targets Azurite (local emulator) or private endpoints. | [`azure_blob_backend.rs`](../../src/infrastructure/services/azure_blob_backend.rs) |

Every entry is declared in `OXICLOUD_STORAGE_ENTRIES` (comma-separated
list of names). The active entry is stored in `admin_settings` and
switched via `oxicloud --select-storage <name>` on the command line
or automatically at the end of a successful `backend_migration`.
Non-active entries stay reachable through the multi-entry API (test,
audit, migrate-into).

Adding a new backend = new struct implementing `BlobStorageBackend`
+ a new arm on `StorageBackendType` + a new branch in
`entry_backend::build_base_backend`. The wrapper stack, key rotation,
consistency check, and migration all pick it up for free (see §6).

---

## 1. The `file → blob → chunk` model

Three separate concerns, three separate storage layers:

```text
                     PostgreSQL                                  Backend
                                                              (Local/S3/Azure)
 ┌─────────────────────┐     ┌──────────────────────┐         ┌──────────────┐
 │ storage.files       │     │ storage.blobs        │         │              │
 │ - id                │────▶│ - hash (BLAKE3)      │────────▶│ .blobs/xx/   │
 │ - name              │     │ - size, ref_count    │         │   <hash>.blob│
 │ - folder_id         │     │ - content_type       │         │              │
 │ - blob_hash         │     │                      │         │              │
 └─────────────────────┘     └──────────────────────┘         └──────────────┘
                                     ▲
                                     │ (for chunked files only)
                             ┌───────┴──────────────┐
                             │ storage.chunk_manifests
                             │ - file_hash          │
                             │ - chunk_hashes[]     │
                             │ - total_size         │
                             └──────────────────────┘
```

**File** (`storage.files`) — DB row. Has a name, a folder, a drive, an
owner, a size, a MIME type. Points at exactly one **content descriptor**:
either a whole-file blob or a chunk manifest, both keyed by BLAKE3 hash.
Files are what users see; nothing about them lives on the backend.

**Blob** (`storage.blobs`) — DB row + **physical bytes on a backend**.
Content-addressable: the row's primary key is `hash = BLAKE3(plaintext_bytes)`.
`ref_count` is the number of live references (files or manifests) pointing
at this blob; when it reaches zero, `dedup_gc` removes both the row and
the physical bytes (after a grace window). Every backend lays blobs out
under a two-char shard directory: `.blobs/<first-two-hex>/<full-hash>.blob`
— Local's filesystem tree, S3's object keys, Azure's blob names. See
`LocalBlobBackend::object_key` and its S3/Azure counterparts.

**Chunk** (`storage.chunk_manifests`) — content-defined-chunking (CDC)
subdivision of a file. When an upload exceeds the whole-file threshold,
the ingest pipeline splits it into ≤1 MiB chunks and stores each as
its own blob. The manifest records the chunk sequence + total size;
downloads stream through the manifest, fetching each chunk blob in
order. **Chunk blobs are indistinguishable from whole-file blobs** at
the backend layer — they're just blobs. `storage.chunk_manifests` is
pure PG state with no backend bytes.

**Why this matters for backend implementors:** you only ever deal with
blobs. You never see file paths, folder trees, chunks, manifests, or
users. Your API is `(hash) → put/get/exists/delete bytes`. Everything
else is orchestrated above.

---

## 2. The v1 blob header (`OXCPT`)

Every blob written since the key-rotation implementation landed
starts with a 15-byte header:

```text
byte 0..4    "OXCPT"        magic marker (5 bytes)
byte 5..6    0x00 0x01      format version (2 bytes, big-endian u16)
byte 7..14   <key_fp>       key fingerprint (8 bytes)
```

Then either:
- **plaintext-v1**: `key_fp` is all zeros; the header is followed by
  raw plaintext bytes.
- **encrypted-v1**: `key_fp` is `SHA-256(key_material)[..8]`; the
  header is followed by a 12-byte AES-GCM nonce, then the ciphertext,
  then the 16-byte GCM tag. Total overhead: **43 bytes**.

Rendered visually via `xxd -l 15 <blob>`:

```text
4f58 4350 54 00 01 00 00 00 00 00 00 00 00     OXCPT..........       ← plaintext-v1
4f58 4350 54 00 01 15 f3 8f 80 2c ae 2c 50     OXCPT......,.,P       ← encrypted-v1 with key_fp = 15:f3:8f:80:2c:ae:2c:50
```

Fingerprints are rendered the same colon-hex form (`15:f3:…:50`)
everywhere they appear: boot log, admin panel pair chain, `xxd`
inspection, `oxicloud --fingerprint <base64>` CLI, and the rotate /
migration audit lines. That means an admin can cross-reference by
eye — same string means same key.

### Why the header exists

Before the key-rotation implementation, blobs had no header.
Reading a legacy blob meant "try to decrypt with the
currently-configured key; if it works, it's encrypted; if not,
it's plaintext." This has three problems:

1. **Ambiguity on key change.** If the operator changed the key,
   every existing blob became unreadable — nothing on disk said
   which key was used.
2. **No way to smart-skip.** A rotation or migration couldn't tell
   whether a target blob was already in the desired state without
   reading and re-hashing every byte.
3. **No forward compatibility.** Any future format change (E2E,
   compression, alternate cipher) would need magic-byte detection
   layered on top.

The header solves all three. Magic bytes disambiguate legacy from
v1. Version bytes let us evolve the format. `key_fp` lets us
identify *which* key was used without trying every candidate.

### Future: end-to-end encryption

The current `EncryptedV1` variant is **server-side encryption at
rest** — the server holds the key. E2E encryption (client holds the
key, server sees only ciphertext) is designed to slot in as a new
version:

```text
byte 5..6    0x00 0x02      format version = 2 (E2E)
byte 7..14   <key_fp>       hint identifying the client key
byte 15..    <opaque body>  client-encrypted payload, opaque to server
```

The read/write pipeline stays the same at the backend layer — the
server just passes bytes through. `read_dispatch` grows a match arm
for version 2 that skips server decryption entirely. This is why the
version bytes exist as a distinct field: the file format is
extensible without a magic-byte rewrite.

### BLAKE3 rescue for legacy plaintext

Deployments that predate the key-rotation implementation have blobs
on disk with no `OXCPT` header — the pipeline calls these `Legacy`
format. Reads try to
decrypt with each pair-list key; if all fail, a last-resort branch
computes `BLAKE3(raw_bytes)` and returns the bytes as plaintext iff
the digest matches the expected hash. Zero-false-positive by
construction (content-addressable proof). Emits
`encryption.legacy_plaintext_rescued` audit lines so operators can
spot which blobs still need re-writing. See
`encrypted_blob_backend.rs::read_dispatch` last branch.

The rescue is transparent — downloads, thumbnails, consistency
checks, and rotation all benefit. The first time `backend_rotate`
sweeps a legacy-plaintext blob, it classifies it as `Legacy`,
rewrites it through the wrapper, and the resulting blob has a proper
v1 header. After one rotate pass, rescue never fires again.

---

## 3. Key rotation

### Pair-list config

Encryption is configured per storage entry via a comma-separated
**pair list**:

```bash
OXICLOUD_STORAGE_<name>_ENCRYPTION_KEY='aes_gcm:<b64_key1>,aes_gcm:<b64_key2>,none:'
```

The **head** is the leftmost entry — writes use this key. Every entry
in the list is available for reads (fallback loop). `none:` in the
list declares "raw plaintext is a legitimate on-disk shape for this
backend" — the leftmost `none:` becomes the head if placed first,
otherwise it enables the plaintext-fallback branch of `read_dispatch`.

### Rotate job (`backend_rotate`)

Recoverable job that iterates every blob on the current backend and
rewrites any whose header doesn't match the current head format.
Decision table via `BlobFormat::classify` compared against
`EncryptedBlobBackend::head_format`:

| Current on-disk | Head | Action |
|---|---|---|
| `EncryptedV1 { key_fp: A }` | `EncryptedV1 { key_fp: A }` | skip |
| `EncryptedV1 { key_fp: A }` | `EncryptedV1 { key_fp: B }` | rewrite (key change) |
| `PlaintextV1` | `EncryptedV1 { key_fp: X }` | rewrite (encrypt in place) |
| `EncryptedV1` | `PlaintextV1` | rewrite (decrypt in place) |
| `Legacy` | anything v1 | rewrite (upgrade header) |

Reports per-blob outcomes (`rewritten`, `skipped`, `failed`) and the
final head format/fp in the run's `extra_stats`. Each rewrite goes
through `put_blob_from_bytes_replace` — see §6.

### Head-key vs fallback keys

- **Head** — used for **writes only**. Rotating just means "declare
  a new head and run `backend_rotate` to catch up existing bytes."
- **Fallback keys** — read-only. Kept in the pair list until every
  blob on disk has been rewritten under the head, then safe to
  remove from `.env`.

The admin panel shows the whole pair chain per entry with the head
badged; after a successful rotation with `failed=0`, non-head keys
can be safely dropped.

---

## 4. Blob consistency (`blobs_consistency`)

Read-only recoverable job that walks `storage.blobs` and reports
divergence between the DB registry and the physical backend.

### Shallow mode (default)

Per row:

- `blob_exists(hash)` on the active backend → if false, record
  `blob_missing_from_backend` (severity `data_loss`)
- Compare `ref_count` against the actual reference count computed
  from `SUM` over `storage.files.blob_hash` + `chunk_manifests.chunk_hashes[]`
  → if mismatch, record `refcount_mismatch` (severity `inconsistent`)

Cost: one existence probe + one aggregate SQL per row. Fast on
S3/Azure (single HEAD).

### Deep mode (`?deep=true`)

Adds a full read of every blob:

- Stream the blob through `EncryptedBlobBackend::get_blob_stream`
  (strips header, decrypts if needed, applies BLAKE3 rescue for
  legacy plaintext)
- Recompute `BLAKE3(plaintext)` and compare against the row's `hash`
- If bytes match: no finding
- If bytes differ: record `blob_corrupted` (silent bit-rot)
- If the read pipeline errors (missing key, unreadable header,
  network failure): record `blob_unreadable`

Both `blob_corrupted` and `blob_unreadable` carry the list of files
that reference the offending hash (`affected_files`) so the operator
can decide whether to re-upload or drop.

The `deep` flag is persisted in the run's `params` on Fresh open so
it survives a mid-run restart — resume continues in deep mode
without the operator re-specifying it.

---

## 5. Backend migration (`backend_migration`)

Recoverable job that copies every blob from the current active
backend to a target entry, then hot-swaps the active pointer on
completion.

### Cursor + resume

Iterates blobs in ascending hash order. Checkpoints the last-visited
hash to `jobs.recoverable_runs.cursor` after each batch. On restart,
`WHERE hash > cursor` picks up from where the previous session
stopped. `MigrationProgress` counter is seeded from
`stats.scanned_count` on resume so the maintenance banner shows
continued progress, not a fresh `0`.

### Smart-skip via `head_check`

Before each copy, `target.head_check(hash)` returns one of:

- `Match` → target blob already carries the current head's
  format+`key_fp`; skip. Debug-log
  `backend_migration.blob_skipped_head_match`.
- `Mismatch(current_format)` → target blob exists with a different
  header (legacy shape, old key, plaintext vs encrypted). Overwrite
  via `put_blob_from_bytes_replace`. Info-log
  `backend_migration.blob_overwritten` — this is the **legacy
  skip-check residual repair** log line: exactly the blobs that
  historically escaped re-encryption because the pre-rotation
  migration path had an `if target exists { skip }` short-circuit
  (see §5 below).
- `Absent` → fresh write. Info-log `backend_migration.blob_written`.

The check is one 15-byte range-read via `get_blob_range_stream(hash, 0, Some(15))`
on the inner backend — cheap on Local (`pread`), cheap on S3/Azure
(single GET with `Range: bytes=0-14`).

### Legacy skip-check residual repair (the historic bug)

Before the key-rotation implementation, the migration path hit
`if target.blob_exists(hash) { continue }` before every copy — it
silently skipped blobs already present on the target, even if the
target's current head key was different from the key used at the
historical write time. Result: mixed-key target backends, and blobs
that suddenly failed to read when the old key was later removed
from the pair list.

The rotation work removed the app-layer skip. Backend-agnostic
write-side fixes followed: every backend's
`put_blob_from_bytes_replace` was overridden to bypass the internal
HEAD-probe skip (`S3BlobBackend`, `AzureBlobBackend`) or use
`O_CREAT|O_TRUNC` via tempfile+rename (`LocalBlobBackend`). See §6
for the full contract.

### Failure gate

`finish_completed` refuses to flip the active-backend pointer if
`failed > 0`. Emits `storage_migration.aborted` audit, clears
readonly (source stays active — writes safe there), and returns
`RunOutcome::Failed`. Operator inspects findings, then either
retries (walk short-circuits on head-format matches → cheap
re-attempt), fixes the source, or explicitly accepts the partial
via `oxicloud --select-storage <target>`.

---

## 6. Implementor contract — the `BlobStorageBackend` trait

### Always wrapped in `EncryptedBlobBackend`

Every entry is built via `entry_backend::build_entry_backend_typed`,
which unconditionally wraps the raw backend in `EncryptedBlobBackend`
— regardless of whether the entry has an encryption key. A `none:`
head gets an `EncryptedBlobBackend` with `head_cipher = None` that
writes plaintext-v1 blobs. This means:

- Every read goes through `read_dispatch` → magic-byte inspection →
  v1 or legacy branch → decrypt (if needed) → BLAKE3 rescue (if
  legacy plaintext).
- Every write goes through the wrapper's write path → prepend the
  15-byte header → encrypt with head cipher (or leave plaintext) →
  hand to inner backend.
- The **inner backend never sees plaintext application content** —
  only the header-wrapped or encrypted body.

Your job as a backend implementor: implement `BlobStorageBackend`
for opaque byte payloads. Never inspect or modify the bytes.

### Required overrides

The trait provides defaults but they lie about correctness for the
rotate/migrate use case. **Every production backend MUST override
`put_blob_from_bytes_replace`.**

| Method | Semantics | Default | Must override? |
|---|---|---|---|
| `put_blob` | Content-addressed upload. May skip if hash exists (dedup, idempotency). | — | yes |
| `put_blob_from_bytes` | Same, in-memory bytes. May skip if exists. | — | yes |
| `put_blob_from_bytes_unsynced` | Unconditional PUT. Durability not required on return; caller batches `sync_blobs`. | delegates to `put_blob_from_bytes` (WRONG for skip-backends) | recommended (dedup fast-path) |
| **`put_blob_from_bytes_replace`** | **Unconditional overwrite. Durable on return.** Used by rotate + migration. | delegates to `put_blob_from_bytes` (WRONG for every current backend) | **yes** |
| `get_blob_stream` | Full-blob stream. | — | yes |
| `get_blob_range_stream` | Range stream — used by the 15-byte `head_check` probe. | — | yes |
| `blob_exists` | Cheap presence check. | — | yes |
| `delete_blob` | Physical deletion. | — | yes |
| `sync_blobs` | Fsync barrier for `_unsynced` writes. | no-op | Local only |
| `initialize` | Called at boot. Verify creds, create shard dirs, reap tempfiles. | no-op | yes |

The `put_blob*` skip-if-exists semantics is correct for **uploads**
— dedup hits should short-circuit. It's wrong for **rewrites** —
rotate needs to overwrite the header, migration needs to overwrite
with new-key ciphertext. `put_blob_from_bytes_replace` is the
escape hatch. On S3/Azure it delegates to `put_blob_from_bytes_unsynced`
(unconditional PUT, durable on return). On Local it does
write-to-tempfile + `rename(2)` + fsync — atomic overwrite on POSIX.

### Reference implementations

- [`LocalBlobBackend`](../../src/infrastructure/services/local_blob_backend.rs)
  — filesystem tree under a configurable root. Shard directory
  `.blobs/<xx>/`. Tempfiles named `<hash>.replace.<pid>.<counter>.tmp`,
  reaped at boot by `initialize`.
- [`S3BlobBackend`](../../src/infrastructure/services/s3_blob_backend.rs)
  — AWS SDK v2. Object key `<xx>/<hash>.blob` under a configurable
  bucket. `put_blob_from_bytes_replace` delegates to
  `put_blob_from_bytes_unsynced` (skips the HEAD probe).
- [`AzureBlobBackend`](../../src/infrastructure/services/azure_blob_backend.rs)
  — Azure SDK. Same shape as S3.

Read them side-by-side before implementing a new backend — the three
follow the same skeleton so the diff is where your backend's
semantics genuinely differ.

---

## Related docs

- [File and blob lifecycle →](./file-and-blob-lifecycle.md) — the
  hook system that observes file/blob CRUD events.
- [Background jobs →](./jobs.md) — how to plug in a new job tenant.
- [Storage quotas →](./storage-quotas.md) — usage accounting layer
  (independent of the backend).
- [Storage multi-entry plan →](../plan/storage-multi-entry.md) —
  design history: why multiple entries, why hot-swap migration.
- [Storage key rotation plan →](../plan/storage-key-rotation.md) —
  design history and slice-by-slice implementation notes.
