# Caching Architecture

OxiCloud runs two independent cache layers with different jobs:

1. **In-memory metadata caches** — [moka](https://docs.rs/moka) instances that sit in front of PostgreSQL and other hot-path lookups. Sub-millisecond hits, bounded by entry count, TTL-evicted. Cover file metadata, directory listings, blob hashes, audio metadata, small thumbnails, on-the-fly image transcodes.
2. **On-disk blob cache** — an LRU cache of blob **bytes** on local SSD, only meaningful when the storage backend is remote (S3, Azure). Turns remote fetches into local reads for hot content; bounded by a disk-budget in bytes, LRU-evicted. Off by default.

The two layers are orthogonal — the moka caches shave query round-trips regardless of backend; the disk-blob cache shaves network round-trips when the backend is elsewhere.

## Layer 1 — In-memory metadata caches (moka)

| Cache | TTL | Max Entries | Purpose |
|---|---|---|---|
| File metadata | 60 s | 10 000 | Avoid re-querying PostgreSQL for file info |
| Directory listings | 120 s | 10 000 | Frequently accessed folder contents |
| Thumbnail cache | configurable | 1 000 | Generated WebP/AVIF thumbnails |
| Image transcode | configurable | 500 | On-the-fly image transcoding results |
| Blob hash | 30 s TTI | 5 000 | BLAKE3 hashes for dedup lookups |
| Audio metadata | — | 2 000 | ID3 tags and duration |

### How it works

1. **Read path:** check cache → if hit, return immediately (sub-ms); if miss, query PostgreSQL, populate cache, return
2. **Write path:** update PostgreSQL → invalidate relevant cache entries
3. **TTL expiry:** entries are evicted after their time-to-live, ensuring eventual consistency

### Why moka?

- **Lock-free** — no mutex contention under concurrent access
- **Bounded memory** — max entries prevent unbounded growth
- **TTL + TTI** — supports both time-to-live and time-to-idle eviction
- **Async-ready** — works natively with Tokio

## Layer 2 — On-disk blob cache

A local-SSD LRU cache of blob bytes, sitting between OxiCloud and remote storage backends (S3, Azure, or any other `BlobStorageBackend`). Every blob read probes the local cache first; misses fetch from the remote backend and populate the cache. Writes go to the remote backend AND the local cache simultaneously, so a just-uploaded blob is immediately hot for its own re-reads.

Structurally: the bytes live on disk, one `.blob` file per hash, sharded by hash prefix under a configurable directory (default `{root}/.blob-cache/<prefix>/<hash>.blob`). The in-process index is a `moka::sync::Cache` with a byte-weigher — same crate as Layer 1, but weighing by content size not entry count, and only tracking file existence, not payload.

### When it earns its keep

Turn on for any deployment where the backend is not on the same box:

- S3 (AWS, DigitalOcean Spaces, Cloudflare R2, MinIO on another host, …)
- Azure Blob Storage
- Any future network-attached backend

Local backends (`LocalFilesystem`) don't need it — they're already on the same box. Enabling it there just doubles disk usage for zero latency win.

**Thumbnails are the strongest reason to turn this on.** OxiCloud stores thumbnails as blobs alongside primary content (via `content_derived_blobs`, tracked in `storage.blobs` like any other blob) — the sidecar-on-disk layout is gone. On a remote backend this means every thumbnail render is a network fetch: a photos grid with 100 thumbnails is 100 S3 requests, per user, per visit. With Layer 2 on, that cost is paid once per thumbnail hash; every subsequent grid render is local-disk reads.

Concrete impact for the photos / file-listing hot paths:

- **Cold render** (all thumbnails uncached): one remote fetch per thumbnail, latency dominated by the backend's per-request round-trip (S3 typically 30-80 ms per object, more at distance).
- **Warm render** (thumbnails cached): local `open()` + read, sub-millisecond per file.
- **Hit rate in practice**: high — thumbnails are small (typically 5-30 KB per size variant), users re-visit the same folders repeatedly, and the LRU pattern strongly favours recency.

Rule of thumb: if your backend is remote AND you have any user-facing photo grid or file browser, Layer 2 is worth the disk budget. On S3 backends it's the difference between a snappy gallery and a spinner-per-tile browsing experience.

### Sizing guidance

The cache is LRU on a disk-budget basis. A working set larger than the cache size will still work but re-fetch cold blobs from the remote — no correctness cost, just latency. Rough sizing:

- **Home / personal cloud** — 5-10 GB is plenty; the working set for a household of active users is small.
- **Small team / SMB** — 50-100 GB for a hot photo library or shared document store.
- **Large deployment** — size against your top-decile access pattern; the cache doesn't need to cover the whole store.

The default budget is 50 GB (only applied if the cache is enabled). Adjust to what your local SSD can spare.

### Interaction with the moka layer

Independent. A file-metadata hit in Layer 1 tells you the row exists and has a `blob_hash` — but reading the actual bytes still goes through Layer 2 (or straight to the remote backend if disabled). A hit in Layer 2 short-circuits the network fetch; a miss populates it for the next read.

## Configuration

### In-memory metadata caches (Layer 1)

Cache parameters are currently hardcoded in `src/common/config.rs`. Key defaults:

```rust
file_cache_ttl_ms: 60_000,       // 1 minute
directory_cache_ttl_ms: 120_000,  // 2 minutes
max_cache_entries: 10_000,
```

### On-disk blob cache (Layer 2)

Environment-tunable — off by default; enable per deployment when the backend is remote:

| Env var | Default | Purpose |
|---|---|---|
| `OXICLOUD_STORAGE_CACHE_ENABLED` | `false` | Master switch. Set `true` to wrap the blob backend with the cache decorator. |
| `OXICLOUD_STORAGE_CACHE_MAX_SIZE` | `53687091200` (50 GB) | Disk-budget in bytes. LRU eviction fires when the cache exceeds this size. |
| `OXICLOUD_STORAGE_CACHE_PATH` | `{root}/.blob-cache` | Where the cache files live. Point at a fast SSD; can be a separate volume from the primary storage root. |

Restart the server after changing any of these — the cache is instantiated once at boot around the configured blob backend.
