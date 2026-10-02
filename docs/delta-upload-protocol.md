# Delta-Upload Protocol

Upload only what changed. The server's dedup store already splits every
file into content-defined chunks (FastCDC, 64 KB – 1 MiB, avg 256 KB,
BLAKE3-addressed) and shares unchanged chunks between file versions —
but a classic upload still transfers every byte just for the server to
discard the known ones. This protocol moves the "which chunks are new?"
question to the client, so unchanged bytes never cross the wire.

Editing a few bytes of a 500 MB file re-uploads ~1 MiB instead of
500 MB.

## Who can use it

Any authenticated API client. The OxiCloud web frontend uses it
automatically for files ≥ 8 MiB (`frontend/src/lib/api/endpoints/deltaUpload.ts` +
`frontend/static/workers/deltaWorker.js`, chunking with the vendored WASM build of the
server's own FastCDC+BLAKE3 crates, falling back to a plain byte upload
on any failure). Generic WebDAV/NextCloud clients cannot (their
protocols have no delta concept) — they keep uploading full bytes, and
the server keeps deduplicating those on write.

Chunk boundaries are the **client's choice**: matching the server's
FastCDC parameters (64 KB / 256 KB / 1 MiB, as the bundled WASM module
does) maximizes cross-version sharing — including against versions that
entered through plain byte uploads — but any split with chunks of
1 byte … 1 MiB is valid; correctness is guaranteed by server-side
verification, not by the chunking scheme.

## The three steps

### 1. `POST /api/files/delta/negotiate`

```json
{ "chunks": [ { "h": "<blake3-hex>", "s": 262144 }, … ] }
```

Response — the distinct chunk hashes the caller must upload, in
first-occurrence order:

```json
{ "missing": [ "<blake3-hex>", … ] }
```

The answer is **user-scoped**: a chunk counts as available only when one
of the *caller's own* (non-trashed) files already references it. The
endpoint is purely advisory — the commit re-checks entitlement
atomically, so a stale or spoofed answer can never leak content.

### 2. `PUT /api/files/delta/chunks`

Body: `application/octet-stream`, a sequence of frames

```
[u32 length, big-endian][length bytes]  …repeated…
```

- one frame per chunk, each 1 byte … 1 MiB (the CDC maximum),
- whole request capped by `OXICLOUD_CHUNK_MAX_BYTES` (default 100 MB) —
  split larger deltas across requests.

The server **recomputes BLAKE3 of every frame itself** (a declared hash
is never trusted for content addressing) and registers the chunks as
unreferenced orphans (`ref_count = 0`). Response:

```json
{ "received": [ { "h": "<server-computed>", "s": 262144 }, … ] }
```

Compare against your own hashes to catch corruption before committing.
Abandoned uploads need no cleanup call: the periodic GC sweeps
zero-reference chunks.

The browser groups frames into requests of at most 8 MiB by default,
including each frame's four-byte length prefix. The per-tab
`window.oxi.UPLOAD_BATCH_BYTES` override accepts a finite positive byte
budget. A frame larger than that budget is sent alone: content-addressed
chunks cannot be split into smaller frames without changing their hashes.
Set the budget below the proxy's request limit, and allow at least the
maximum chunk size plus four bytes at the proxy.

The same batching applies when a commit returns `409 still_missing`.
Recovery validates the requested hashes against the file's manifest,
uploads each distinct missing chunk once in bounded requests, and only
then retries the commit. The browser permits two recovery rounds; a
failed recovery PUT stops the round and uses the existing upload fallback.

### 3. `POST /api/files/delta/commit`

```json
{
  "file_hash": "<blake3-hex of the whole file>",
  "chunks": [ { "h": "…", "s": 262144 }, … ],   // full sequence, in order
  "name": "video.mp4", "folder_id": "<uuid>"     // create mode
  // — or —
  "file_id": "<uuid>"                            // update (replace content)
}
```

Server-side, in order:

1. **AuthZ** — `Create` on the folder (create mode) or `Update` on the
   file (update mode); quota on the logical size.
2. **Pin** — one atomic `UPDATE … RETURNING` takes a reference on every
   distinct chunk the caller is *entitled* to: chunks reachable through
   the caller's own files, or unreferenced orphans (the just-uploaded
   state). Anything else → `409 { "still_missing": […] }`: upload
   exactly those and retry the same commit.
3. **Verify** — the pinned sequence is re-read and the whole-file BLAKE3
   recomputed. A mismatch releases the pins and returns 400 (and an
   audit event): the declared `file_hash` is never trusted, because a
   forged manifest would poison future whole-file dedup hits for *other
   users* uploading the genuine content.
4. **Attach** — the manifest is inserted with the same accounting as the
   streaming byte path (a concurrent identical commit resolves via
   `ON CONFLICT`: the loser's references are released and it becomes a
   dedup hit).
5. **Row** — the file is created (`201`, body = FileDto) or its content
   swapped (`200`).

If the caller already owns the exact `file_hash`, the commit
short-circuits to a pure reference bump — chunks aren't even looked at
(same as `POST /api/files/by-hash`).

## Delta download (sync clients)

The inverse direction, for a client app that already holds an older
version locally and wants the server's current one:

1. `GET /api/files/{id}/manifest` → `{ file_hash, total_size, chunks }`
   — the file's chunk recipe. **Owner-scoped** like the rest of the
   delta surface (shared files use the regular download endpoints).
   Served with `ETag: "<file_hash>"`; a manifest is immutable for a
   given hash, so `If-None-Match` revalidation answers 304 — polling
   sync clients pay one header round-trip per unchanged file.
2. Diff the manifest against the local chunk inventory (chunk the local
   copy with the same WASM module the upload direction ships).
3. `POST /api/files/delta/download` with `{ "hashes": […] }` → the
   requested chunks as `[u32 BE length][bytes]` frames in request order
   (the same wire format as the upload direction). Entitlement is the
   same possession rule as negotiate/commit: chunks must be reachable
   through the caller's own files; anything else → 404
   `{ "not_available": […] }` — deliberately indistinguishable from
   "never existed". Batches are bounded by `OXICLOUD_CHUNK_MAX_BYTES`;
   split large deltas across requests.
4. Reassemble locally per the manifest order and verify the whole-file
   BLAKE3 against `file_hash`.

Editing 3 bytes of a 24 MB file on one device costs a second device one
manifest GET plus ~1 chunk (~256 KB) instead of 24 MB.

## Security model

- **No content oracle.** Possession is proven per chunk: without bytes
  you can only claim what your own files already reference. Probing
  someone else's chunk hashes yields `still_missing`, indistinguishable
  from the hash never existing.
- **No manifest poisoning.** `file_hash` and every chunk hash are
  recomputed server-side before becoming addressable.
- **Bounded resources.** Per-frame cap 1 MiB, per-request cap
  `OXICLOUD_CHUNK_MAX_BYTES`, whole-file cap `OXICLOUD_MAX_UPLOAD_SIZE`,
  per-caller rate limit (240 delta requests/min), quota enforced at
  commit. Orphan chunks are GC-swept.
- **Audit.** Rejections emit `delta_upload.rejected` with stable
  `reason` keys: `rate_limited`, `chunk_verification_failed`,
  `file_hash_mismatch` — and `delta_download.rejected` with
  `manifest_not_owner` / `chunks_not_owned`. AuthZ denials surface as
  the engine's standard `authz.denied`.

## Error summary

| Status | Meaning | Client action |
|---|---|---|
| 400 | malformed framing/hashes/sizes, or `file_hash` mismatch | fix and retry from step 1 |
| 404 | folder/file not found or not accessible | — |
| 409 | `{"still_missing": […]}` | PUT those chunks, retry the commit |
| 429 | rate limited | back off |
| 507 | quota exceeded | — |

## Cost notes

- `negotiate` is one indexed query (GIN over manifest chunk arrays).
- `commit` performs one sequential server-side read of the full logical
  file for verification — cheap on local backends, a full object read on
  S3/Azure. Still strictly cheaper than receiving the bytes, and the
  client's bandwidth saving is unaffected.
