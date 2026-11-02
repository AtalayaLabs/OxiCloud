# Plan — Per-chunk compression (v3 OXCPT header)

**Status:** DRAFT + design captured 2026-11-02. Not implemented. V1 stays
as the canonical shape for not-compressed data; v3 is a new on-disk
format that writers emit only when a compressibility heuristic
accepts the blob. One decision locked up front: **content-address is
BLAKE3 of plaintext**, same as today — the hash of a blob does not
change when its compression algorithm changes, when it is rewritten
compressed, or when it is rewritten decompressed. Dedup across the
whole corpus is unaffected.

E2E is **out of scope**: v2 is already reserved for client-encrypted
payloads whose body is opaque to the server. A Vault drive never
goes through server compression; the client decides whether to
compress before encrypting.

## 1. Why

Text, source, logs, SQL dumps, JSON, uncompressed office exports and
XML all compress 3–10× with zstd default. Already-compressed media
(JPEG, MP4, Office `.docx`-as-ZIP, archives) compresses ~1×, which is
why the decision is per-blob with a heuristic, not blanket. For the
10k-user target, savings are additive against existing CDC dedup;
nothing in this plan changes chunk boundaries, chunk counts, or
`chunk_manifests` keying.

Three concrete wins:

- **Storage cost on compressible content.** 3–10× on anything
  textual. Hits S3/Azure drives directly on the monthly bill.
- **Range reads stay cheap.** The decompression unit is the chunk,
  not the file. A WebDAV range GET on a 2 GiB file decompresses only
  the chunks the range touches.
- **Clean compose with server-side encryption.** v1 encrypted-at-rest
  is unchanged. v3's body is `compress → encrypt`, in that order
  (ciphertext does not compress; the reverse ordering is a defect
  pattern, not an option).

## 2. Decision — keep V1 alongside V3

V3 is reserved for blobs where **compressed = 1**. A not-compressed
blob stays as V1 (plaintext or encrypted). Writer picks V3 only when
the heuristic accepted compression.

Cost of the alternative (unified-V3-for-everything):

- +6 B per blob on not-compressed data
- 10M blobs × 6 B = 60 MB on a 10k-user instance
- On a 4 KiB chunk: 0.15% overhead; on 64 KiB: 0.009%

The 6 B is negligible, so unification would have been defensible on
simplicity grounds. The choice to keep V1 is deliberate: V1 is
settled, in production, and its on-disk shape is documented and
tooled against (`xxd`, admin panel fp chain, rotate audit lines).
Making V3 strictly a superset of V1 would force every rotate pass to
rewrite every V1 blob into V3 for format uniformity, which is a lot
of backend churn for no plaintext savings. Heterogeneous keeps
rotation cheap: V1 blobs stay V1 unless the head switches encryption
state or they prove compressible.

## 3. On-disk format

### V3 header (21 bytes)

```text
byte  0..4   "OXCPT"            magic                                 (5)
byte  5..6   0x00 0x03          format version = 3                    (2)
byte  7..14  <key_fp>           key fingerprint; 0x00..00 iff enc=0   (8)
byte 15      <flags>            bitfield                              (1)
byte 16      <comp_algo>        compression algorithm; 0 iff comp=0   (1)
byte 17..20  <plaintext_len>    u32 BE — BLAKE3-hashed plaintext size (4)
```

**flags** (bit 0 = LSB; MUST be 0 for reserved bits on write,
ignored on read):

| bit | meaning |
|---|---|
| 0 | compressed — body was passed through `comp_algo` before any encryption |
| 1 | encrypted — body is AES-GCM, same scheme as encrypted-v1 |
| 2..7 | reserved — MUST be 0 on write |

**comp_algo** is a byte (not a bit range inside `flags`) so we can
grow the algorithm set without touching the flag layout:

| value | algorithm |
|---|---|
| 0x00 | none — only valid when `flags.compressed = 0` |
| 0x01 | zstd — default; level is NOT encoded (any level decodes without it) |
| 0x02..0xFF | reserved |

### Body shape

Always compress-then-encrypt. Reader order mirrors: decrypt-then-decompress.

| flags | body |
|---|---|
| `comp=0 enc=0` | `<raw plaintext>` — legal but writers prefer V1 plaintext instead |
| `comp=1 enc=0` | `<comp_algo(plaintext)>` |
| `comp=0 enc=1` | `<12 B nonce><ciphertext(plaintext)><16 B tag>` — legal but writers prefer V1 encrypted instead |
| `comp=1 enc=1` | `<12 B nonce><ciphertext(comp_algo(plaintext))><16 B tag>` |

The two "legal but prefer V1" shapes exist so the reader doesn't
need a special-case branch, not because writers emit them. See § 4.

### Fingerprint rendering

Unchanged. V3's `key_fp` is the same `SHA-256(key_material)[..8]`
colon-hex rendering as V1 — admin panel, boot log, `oxicloud storage
fingerprint` CLI, rotate audit lines. Same string means same key
across V1 and V3.

## 4. Writer decision (new blob)

Executed per blob (chunk), before any backend write.

```text
if drive_kind == Vault
    → caller handed us ciphertext → V2 (unchanged)

elif head_encrypted and compressible(plaintext)
    → V3 { comp=1, enc=1, comp_algo=zstd }

elif head_encrypted
    → V1 encrypted (unchanged)

elif compressible(plaintext)
    → V3 { comp=1, enc=0, comp_algo=zstd }

else
    → V1 plaintext (unchanged)
```

### Compressibility heuristic

Compress the first 4–16 KiB of the plaintext at zstd level 1, keep
the compressed result iff `compressed_len / sample_len < 0.9`. If
accepted, continue the compression over the full chunk at the
configured level. Decision is per-blob, not re-evaluated per read.

Why first-N bytes, not full compression upfront: for a 1 MiB chunk,
zstd level 1 processes at ~1 GB/s single-core; the sample takes
microseconds. Running the full compression and discarding on a bad
ratio wastes a few hundred µs per non-compressible chunk; the sample
costs ~16 µs regardless.

A per-drive policy knob — call it
`OXICLOUD_STORAGE_<name>_COMPRESSION` with values `off | auto | on`
— lets operators force compression off on drives with high-churn
tiny-chunk workloads or incompressible content, and force it on
unconditionally for cold archives. Default: `auto` (heuristic).
Level is a second knob (`_COMPRESSION_LEVEL`, default `3`).

## 5. Reader — `read_dispatch` additions

Growing from "legacy / v1-plaintext / v1-encrypted / rescue" to
"legacy / v1-plaintext / v1-encrypted / v3 / rescue". V3 is parsed
once, then its flags drive the body decode:

```text
V3 (parse header):
    verify flags.reserved == 0
    verify (flags.compressed == 1) iff (comp_algo != 0)
    verify (flags.encrypted == 1) iff (key_fp != zeros)

    body = raw bytes after byte 20

    if flags.encrypted:
        body = AES-GCM-decrypt(key_for(key_fp), nonce, ct, tag)
    if flags.compressed:
        body = decompress(comp_algo, body, expected_len = plaintext_len)

    assert body.len() == plaintext_len
    return body
```

The `plaintext_len` field is used for two things:

1. **Buffer-size hint** for streaming decompression (zstd carries
   this internally but having it in the header avoids a parse pass).
2. **Cross-check** against `chunk_manifests.chunk_sizes[i]` during
   `backend_consistency` sweeps — a tamper or truncation that
   produces a valid-looking frame still mismatches.

The BLAKE3 rescue branch is unchanged. It only fires when neither V1
nor V3 nor V2 header is present (legacy pre-OXCPT bytes).

## 6. `backend_rotate` — decision table extension

Classify grows a `V3 { comp_algo, flags }` variant. The head-format
comparison needs to encode "head wants compression?" (yes if the
drive policy is `on`, or `auto` + head key is such-and-such) plus
head's compression algorithm. New rows atop the existing table in
`backend-storage.md § 3`:

| Current on-disk | Head | Action |
|---|---|---|
| `V1 { enc }` | wants `V3 { comp=1, enc }` and plaintext is `compressible` | rewrite — compress in place |
| `V1 { enc }` | wants `V3 { comp=1, enc }` and plaintext is NOT compressible | skip |
| `V3 { comp=1, enc=A }` | `V3 { comp=1, enc=B }` | rewrite — re-encrypt body, keep compressed |
| `V3 { comp=1, comp_algo=A }` | `V3 { comp=1, comp_algo=B }` | rewrite — recompress with new algo |
| `V3 { comp=1 }` | `V1` (policy flipped to compression off) | rewrite — decompress in place |

`BlobFormat::classify` and `EncryptedBlobBackend::head_format` are
the two sites that change; the rest of the rotate flow is unchanged
(same `put_blob_from_bytes_replace`, same `extra_stats`, same
smart-skip semantics).

## 7. Schema

One new column:

```sql
ALTER TABLE storage.blobs ADD COLUMN stored_bytes BIGINT;
```

`storage.blobs.size` keeps meaning **logical** size (plaintext
bytes, what the file layer sees — unchanged). `stored_bytes` is the
on-backend size including header, nonce, tag, and compressed body.
`NULL` means "not yet measured", filled in by the next read or by
`backend_consistency` under `?repair=true`. Needed for:

- Backend quota accuracy on S3/Azure (we bill ourselves against the
  wire bytes, not the plaintext bytes).
- `oxicloud_storage_compression_ratio` Prometheus metric
  (`SUM(size) / NULLIF(SUM(stored_bytes), 0)`).
- A compression admin panel line, same look as the key-fp chain.

`chunk_manifests.chunk_sizes[]` stays plaintext — unchanged. The
manifest is the file-layer view; the backend's compressed sizes are
below its horizon.

## 8. Dedup invariants

Content-address is BLAKE3 of plaintext. The compressed/encrypted
bytes are a storage-internal detail below the hash. Consequences:

- Two uploads of the same plaintext produce the same blob hash
  regardless of compression state. Second upload hits the dedup
  path, no new backend write.
- Changing the compression level does NOT invalidate dedup — the
  key doesn't change. The on-disk shape may change if `backend_rotate`
  decides to recompress.
- A file ingested on a `compression=off` drive and later migrated
  to a `compression=auto` drive dedupes correctly against existing
  blobs — the hash is drive-independent.

The one gotcha: `storage.chunk_manifests.chunk_sizes[i]` is plaintext
size and must match the plaintext size the reader produces for every
chunk. A compression bug that silently truncates would otherwise show
up as a size mismatch at the manifest level, which is strictly better
than silent corruption at the file layer. The reader's `assert
body.len() == plaintext_len` from § 5 catches it at the chunk
boundary before it reaches the file.

## 9. Observability

New fields and metrics:

- `backend_rotate.extra_stats`: per-row `compressed` /
  `recompressed` / `decompressed` counters alongside `rewritten` /
  `skipped` / `failed`.
- Audit event `encryption.compressed_rewrite` (and `_recompressed`,
  `_decompressed`) matching the shape of the existing
  `legacy_plaintext_rescued` / `rewritten` lines.
- Prometheus `oxicloud_storage_compressed_blob_count`,
  `oxicloud_storage_compressed_stored_bytes` and the derived
  `_compression_ratio` gauge, split by drive.
- Admin panel: a "Compression" badge per drive showing policy
  (off/auto/on), algorithm (zstd), and the drive's live compression
  ratio. Zero if no compressed blob yet.

## 10. Delivery order

1. `BlobFormat::V3` variant + classify + `head_format` plumb-through.
   No writer changes yet; reads of V3 should Just Work before any
   V3 blob exists.
2. `storage.blobs.stored_bytes` migration + backfill-on-read.
3. Writer gate — compressibility heuristic + per-drive policy env
   vars + `V3` emission path.
4. `backend_rotate` decision-table rows + per-blob counters.
5. Admin panel line + Prometheus metrics.
6. `backend-storage.md` § 2 and § 3 edits — document V3, extend the
   rotation decision table.

Each step is independently testable:

- Step 1: unit tests on `classify`, read a hand-crafted V3 blob,
  no backend writes yet.
- Step 2: migration test + a `backend_consistency` run under
  `?repair=true` on a legacy instance.
- Step 3: Hurl pin on upload-then-download of a compressible fixture;
  measure `stored_bytes < size` after the upload.
- Step 4: rotate on a mixed-version instance, assert `extra_stats`
  counts match a known seed.
- Step 5: cosmetic.
- Step 6: cosmetic.

## 11. Open questions

- **Thumbnails and derived blobs.** JPEG/WebP thumbs are already
  compressed; running zstd over them yields ~0% and costs the
  heuristic's ~16 µs per read. The heuristic will correctly reject
  them, so no code change needed, but the Prometheus ratio should
  split "thumbnails" out so the ratio for user content stays
  readable. **Lean: punt to a follow-up.**
- **Dictionary training.** zstd-dict trained on the user's own corpus
  can push ratios on short JSON/XML/HTML an extra 2–3×. Needs a
  training job + a dict registry + a `comp_algo = 0x02 zstd-dict`
  variant. Not v1. **Lean: separate plan, after shipped v1 data
  gives us a ratio measurement baseline.**
- **The `plaintext_len` u32.** 4 bytes cap chunk size at ~4 GiB,
  which is far above the ~1 MiB CDC cap. OK forever. Mentioning
  only because a future "big-chunk" mode would need u64 here.
- **Default compression level.** Zstd 3 is "default" and fast; 1
  compresses less but at >1 GB/s single-core. 19+ is premium and
  slow. Default 3; expose level via per-drive env; measure after
  rollout. **Lean: 3 is the right default for `auto`.**
- **Pass-through compressed chunks to the HTTP client.** Asked
  2026-11-02. **Lean: no, not in this plan.** Standard
  `Content-Encoding: gzip/br/zstd` via `tower-http::CompressionLayer`
  handles browser and WebDAV clients transparently; the re-compression
  from plaintext costs <1% of a GB/s read at zstd 1 and is below any
  threshold worth optimizing for a 10k-user target. Pass-through
  only wins on `comp=1 enc=0` blobs (no help on encrypted drives) and
  needs a manifest-aware wire protocol for range reads and client-side
  integrity verification — that's a sync-client feature (manifest-aware
  by design), not a compression feature. Reopen when the OxiCloud
  desktop sync client plan lands; until then, serve browsers via
  standard `Content-Encoding`.

## 12. What this does NOT buy

- Not faster reads. Decompression costs CPU; range reads save I/O
  only if the compression ratio × network latency outweighs the
  decompression cost — true on S3/Azure, break-even on local disk.
- Not E2E. v2 (opaque body, client-held key) is a separate plan.
- Not dedup beyond what CDC already does. Compression is below the
  hash; dedup is above it.
- Not legacy-blob removal. Legacy whole-file blobs stay slated for
  NEXT-MAJOR removal
  (see `project_legacy_whole_file_blob_removal` memory). This plan
  does not touch them — compression lives in the chunk path only.

## Related

- `docs/architecture/backend-storage.md` § 2 (OXCPT v1), § 3 (rotation
  decision table) — the two sections this plan extends.
- `docs/plan/derived-blobs.md` — the content-keyed sidecar tier.
  Thumbnails and audio-content-metadata are derived blobs, not
  user-content blobs; compression applies to both tiers via the same
  v3 shape but the heuristic will reject most derived content.
- `docs/plan/consistency-check.md` — `backend_consistency` is where
  the `stored_bytes` backfill-on-sweep lives.
