# Round 25 — encrypted-read in-place decrypt (RAM), delta-commit hash move, dead folder-Query, public-playlist N+1 fold, contact vcard over-fetch

This round lands a cross-cutting perf pass surfaced by a fresh six-way audit of
the tree (dedup/upload, blob-I/O, DB query-shape, HTTP/DAV emitters, auth/global
config, frontend), cross-referenced against everything ROUND2–24 already shipped
so nothing here re-treads landed work. Five items ship, each behind a
BEFORE/AFTER benchmark that `std::process::exit(1)`s ("`GATE FAIL … rollback`")
unless AFTER strictly beats BEFORE — the round's roll-back rule encoded into the
benchmark, so an AFTER that doesn't win is never applied to the source.

Reproduce:

```bash
# M1–M3 — counting global allocator (count + BYTES), no Postgres
RUSTFLAGS="-C target-cpu=x86-64-v3" \
  cargo run --release --features bench --example bench_round25_micro

# Q1–Q2 — live dev Postgres (reads DATABASE_URL from .env)
RUSTFLAGS="-C target-cpu=x86-64-v3" \
  cargo run --release --features bench --example bench_round25_queries
```

The two headline items match the owner's top priorities: **M1 halves peak RAM on
every encrypted blob read**, and **Q1 collapses the public-playlist gallery from
101 DB round-trips to 1**.

---

## [M1] `EncryptedBlobBackend::decrypt_bytes` — full ciphertext copy → in-place detached decrypt (RAM)

`decrypt_bytes` claimed in its own doc comment to decrypt "**in place** … the
ciphertext buffer is reused for the plaintext instead of allocating a second
copy." It did not:

```rust
let mut ciphertext = encrypted.split_off(NONCE_SIZE); // allocates + memcpy's the whole tail
```

`Vec::split_off(12)` allocates a fresh `Vec` sized `len-12` and `ptr::copy`s the
entire ciphertext+tag into it — so **every decrypted CDC chunk (≤ 1 MiB), and
every legacy whole-file blob, paid one full-payload allocation + memcpy on read**.
ROUND11 §15 fixed the *encrypt* side (`encrypt_in_place_detached`) but the
decrypt side was never given the same treatment; the stale doc comment is the
tell that it was believed already done.

AFTER lifts the 12-byte nonce and 16-byte GCM tag to the stack, decrypts the
middle in place via `decrypt_in_place_detached` (the detached API already used by
the encrypt side), and returns a **zero-copy `Bytes::slice` past the nonce** — no
extra allocation, no full-payload copy. Plaintext bytes are identical.

| arm    | allocs/op | bytes/op | note |
|--------|----------:|---------:|------|
| BEFORE |      3.00 |  524 356 | input clone + `split_off` copy + `Bytes::from` |
| AFTER  |      2.00 |  262 196 | input clone + `Bytes::from` only |

**−262 160 bytes/op** at a 256 KiB payload — the copied ciphertext eliminated;
peak heap on a decrypt drops from ~2× to ~1× the payload. The win scales with
payload, so a legacy whole-file blob read no longer transiently doubles a
multi-hundred-MB allocation. Gate: **AFTER bytes/op strictly lower** (it is). The
equivalence arm asserts the decrypted plaintext is byte-identical to the
`split_off` path across the short-input edge, 64 KiB and 1 MiB.

## [M2] Delta commit — third per-occurrence hash clone → move-unzip (dedup allocations)

`delta_upload_service::commit_with_perms` owns `request: DeltaCommitRequest`, yet
materialized the per-occurrence chunk-hash list a **third** time at the manifest
bind (after the distinct set and the verification tuple):

```rust
let chunk_hashes: Vec<String> = request.chunks.iter().map(|c| c.h.clone()).collect();
let chunk_sizes:  Vec<u64>    = request.chunks.iter().map(|c| c.s).collect();
```

`request.chunks` is dead after this line (only `request.file_hash` is read
below), so AFTER moves the hashes out instead of cloning each 64-char hash:

```rust
let (chunk_hashes, chunk_sizes): (Vec<String>, Vec<u64>) =
    request.chunks.into_iter().map(|c| (c.h, c.s)).unzip();
```

| arm    | allocs/op (4000 chunks) | bytes/op |
|--------|------------------------:|---------:|
| BEFORE |                 8 003.00 |  768 000 |
| AFTER  |                 4 003.00 |  512 000 |

**−4000 allocs/op** (the N hash-String clones) on the flagship "upload only what
changed" path. Gate: AFTER allocs/op strictly lower. Equivalence: the produced
`(chunk_hashes, chunk_sizes)` are element-equal to the clone-collect arms.

## [M3] `folder_handler::download_folder_zip` — dead `Query<HashMap>` extractor removed (allocations)

Both the route wrapper and `download_folder_zip_impl` bound
`Query<HashMap<String,String>>` as `_params` and discarded it — the handler reads
only the path `id`. axum's `Query` extractor parses the whole query string into a
`HashMap` plus an owned `String` key and value per param, all dropped unread. AFTER
deletes the extractor; axum ignores any query string when none is present, so the
response is byte-identical.

| arm    |  ns/op | allocs/op | bytes/op |
|--------|-------:|----------:|---------:|
| BEFORE | 212.1  |      5.00 |      268 |
| AFTER  |   0.3  |      0.00 |        0 |

Pure dead-work elimination (**614× wall**, 5 → 0 allocs) whenever a client
appends any query string (cache-buster, tracking param). Gate: AFTER allocs/op
strictly lower.

## [Q1] Public-playlist listing — 1 + N `COUNT(*)` → one `LEFT JOIN … GROUP BY` (DB round-trips)

`MusicStorageAdapter::list_public_playlists` ran one listing SELECT then one
`SELECT COUNT(*) FROM audio.playlist_items WHERE playlist_id = $1` **per returned
playlist** — up to **101 serial round-trips** for a `limit=100` gallery page.
AFTER folds the count into the listing with a single
`LEFT JOIN audio.playlist_items … GROUP BY p.id`, exposed as a new inherent
`PlaylistPgRepository::list_public_playlists_with_counts` returning
`(Playlist, track_count)` — backed by the existing
`idx_playlist_items_playlist_id`. (The adapter holds the concrete repo type, so
no trait change was needed; the two sibling 1+N adapter methods have no live
caller and are left untouched.)

Live Postgres, 100 public playlists (varying track counts), p50 over 30 passes:

| arm    | p50 ms | round-trips |
|--------|-------:|------------:|
| BEFORE | 16.572 |         101 |
| AFTER  |  0.458 |           1 |

**36.2× wall, 101 → 1 round-trips.** Equivalence: the `(playlist → track_count)`
map is identical BEFORE vs AFTER (asserted; mismatch `exit(1)`s). Gate: AFTER p50
strictly lower. On a remote/managed Postgres, where each round-trip is a network
RTT rather than a local socket hop, the win is far larger than the localhost 36×.

## [Q2] Contact REST listings — stop over-fetching the multi-KB `vcard` TEXT (bandwidth)

`get_contacts_by_address_book_paginated`, `search_contacts` and
`get_contacts_by_group` all `SELECT … vcard …` — the full serialized vCard TEXT,
the largest column (can embed a base64 `PHOTO` of tens of KB). But every caller
maps `Contact → ContactDto`, which has **no vcard field**, so it is fetched,
shipped over the wire, decoded into a `String` and immediately dropped. AFTER
adds a `row_to_contact_lite` mapper (shared `row_to_contact_with_vcard` core, no
duplication) that supplies an empty vcard, and narrows those three SELECTs to
omit the column. The shared `get_contacts_by_address_book` (also used by the
whole-book vCard export) and the CardDAV sync/multiget paths keep the column.

Live Postgres, 1000 contacts each carrying an 8 KiB vCard, p50 over 20 passes:

| arm    | p50 ms | note |
|--------|-------:|------|
| BEFORE | 10.010 | SELECT incl. vcard, decoded + dropped |
| AFTER  |  1.570 | SELECT without vcard |

**6.4× wall** — and the win is bytes-on-the-wire + per-row `String` allocation,
both of which grow with vCard size (photos push these to tens of KB each).
Equivalence: the kept DTO fields `(id, full_name, photo_url)` are identical
across the change (asserted). Gate: AFTER p50 strictly lower.

---

## Not shipped — verified this round, deferred to a later pass

The six-way audit surfaced far more than shipped here; the following were
verified real against current source and carry a benchmark plan, but each needs a
multi-signature change, a remote-backend fixture, an operator-facing decision, or
its own validated pass. Grouped by area for the next rounds.

### Blob-I/O / disk (owner priority)
- **`CachedBlobBackend::initialize` never pre-creates the 256 shard dirs** (the
  line-122 comment says it does; it only makes `cache_dir`), so all three cache
  writes pay a per-chunk `create_dir_all(parent)` — a wasted `mkdirat(EEXIST)` +
  component stat + blocking-pool dispatch on cached-remote deployments. Fix mirrors
  `LocalBlobBackend::initialize`'s `HEX_PREFIXES` loop; gate on a `strace -c`
  `mkdirat` count + wall on tmpfs. (conf 0.9)
- **Eviction listener unlinks with a blocking `std::fs::remove_file` on the tokio
  worker** (`cached_blob_backend.rs:98`) — `moka::sync` runs the listener inline on
  the inserting worker; every write-through eviction blocks a reactor thread on
  `unlink(2)`. Hand off via `spawn_blocking`/a drain task; gate on p99 scheduling
  delay under eviction pressure. (conf 0.85)
- **S3 reads copy every served byte** through `into_async_read()+ReaderStream`
  (`s3_blob_backend.rs:261/298`) while Azure already forwards SDK `Bytes` frames
  zero-copy — needs a MinIO/stub fixture to gate. (conf 0.6)
- **`store_loose_chunks` writes loose chunks to the backend serially** while the
  main ingest overlaps 8 (`buffer_unordered`); on a remote backend the delta path
  serializes RTTs the main path hides. Needs a latency-stub backend. (conf 0.5)
- **`local_blob_path` does a synchronous `path.exists()` stat on the reactor** —
  wants an async port variant. (conf 0.6)
- **`PLAINTEXT_EMIT_SIZE` = 64 KiB vs the 256 KiB every other backend streams** —
  quarters the encrypted-read frame count; the "parity" comment justifying 64 KiB
  is factually wrong. Wants a streaming A/B (frame count + wall). (conf 0.5)

### DB query-shape
- **Drive-policy reads decode through a throwaway `serde_json::Value` DOM**
  (`drive_pg_repository.rs` 4 methods) — ROUND23 §J2 removed only the clone, not the
  DOM; fold to `sqlx::types::Json<DrivePolicies>` like §J1. Fires on move/copy and
  every share/grant create. (conf 0.75)
- **Contact create/update build a throwaway `Value` before binding JSONB** (the
  write-side twin of ROUND23 §J1) — bind `sqlx::types::Json(&dtos)` directly. (conf 0.6)

### Dedup / upload
- **`attach_manifest` reshapes chunk sizes into a throwaway `Vec<i64>` per upload**
  — carry sizes as `i64` end-to-end (validated in an earlier draft of
  `bench_round25_micro` §M4; deferred because it threads a type change through
  `ChunkIngestOutcome`, the delta/stream/legacy paths and the `total_size` sums —
  its own pass). (conf 0.5)
- **`store_from_stream` rebuilds the distinct-hash set the CDC loop already held**
  as `pinned ∪ written` (`dedup_service.rs:499`/`distinct_hashes`) — return the list
  the ingest already owns instead of an O(N) rescan + HashSet + N clones. ROUND14
  deferred. (conf 0.55)
- **Whole-file dedup-hit fast path is 3 serial manifest round-trips** (owner-check +
  metadata SELECT + ref-bump UPDATE) — fold metadata+bump into one
  `UPDATE … RETURNING` (3→2), or the whole thing into one atomic statement (3→1,
  also closes a TOCTOU). Authz-sensitive; needs a validated pass. (conf 0.55)
- **Ownership checks bind the caller UUID as text** (`to_string()` + `$2::uuid`)
  instead of a native `Uuid`, unlike the sibling claimable/pin queries. (conf 0.5)
- **Delta-download authorize is 2 round-trips** (entitlement then sizes) foldable
  into one entitlement-JOIN-blobs query. (conf 0.55)

### HTTP / DAV emitters (allocations)
- **`format_oc_id` allocates a fresh `String` per NC PROPFIND/REPORT/trashbin row**
  — thread a `format_oc_id_into(&mut String, …)` buffer like the href buffer already
  in those loops. Multi-signature; ROUND20 deferred. (conf 0.85)
- **`search_service::suggest_with_perms` builds a full `FileDto`/`FolderDto` per
  candidate** to read 5 fields, computing (and dropping) `etag` + `size_formatted`
  Strings on every keystroke. (conf 0.75)
- **CardDAV whole-book GET accumulates a throwaway per-contact vCard `String`** into
  an unsized buffer — wants a `write_vcard_into(&mut String, …)`. (conf 0.7)
- **NC REPORT/trashbin per-row href + NC avatar `HeaderMap` clone + `list_files_query`
  `Query<HashMap>`** — the remaining H1/href-buffer items ROUND22 left. (conf 0.55–0.65)

### Auth / global config
- **`foldhash` is already in the lockfile transitively** (via hashbrown), so the
  long-deferred fast-hasher lead is nearly free: `foldhash::quality::RandomState`
  (random-seeded, DoS-safe) for the attacker-controlled delta-upload hash sets, and
  `foldhash::fast` for the trusted-key NC PROPFIND `favorite_ids`/`nc_id` maps.
  Wall-gated (a hasher swap changes 0 allocations). (conf 0.7)
- **`tracing` has no `release_max_level` feature** — per-request `debug!`s in the
  auth middleware and the authz `require()` granted path compile into release and
  pay a runtime level check. `release_max_level_info` compiles them out (binary-size
  + hot-path win) but silently disables `RUST_LOG=debug` on release builds — an
  **operator-facing tradeoff** that wants a maintainer decision, so it is flagged, not
  shipped. (conf 0.65)
- **`profile.release` uses `lto = "thin"`** while `profile.bench` already trusts
  `lto = "fat"` — a last-slice hot-path + size win at the cost of link time. (conf 0.55)
- **`panic = "abort"` — VERIFIED UNSAFE, do not apply.** `text_extractor.rs:177`
  relies on `catch_unwind` to survive `pdf-extract` panics on malformed PDFs, and
  tokio's per-task panic isolation itself needs unwinding; under abort a single
  hostile PDF (or any handler `.unwrap()`) becomes a whole-process crash. Keep
  `panic = "unwind"`. (Recorded so a future pass doesn't re-open it.) (conf 0.9)

### Frontend
- **Client folder-listing cache (`getCachedFolder`/`cacheFolder` + ETag) is dead
  code** — never called; every folder navigation refetches the full body with
  `cache:'no-store'` and no `If-None-Match`. Wire the SWR cache in (bandwidth +
  instant paint on revisits). (conf 0.6)
- **Grouped listing views mount one `VirtualWindow` per section** — O(sections)
  scroll listeners + `getBoundingClientRect` reads per scroll frame; hoist to one
  shared tracker (the deferred "unify onto VirtualRows"). (conf 0.6)
- **`VirtualRows.offsets` prefix-sum, flat dotfile filter O(N²), `typeLabel`
  per-call 13-entry object** — the residual per-page frontend rebuilds. (conf 0.5–0.65)

---

## Environment / methodology

- **M1–M3:** counting global allocator tracking BOTH alloc **count** and **bytes**
  (`benches/bench_round25_micro.rs`), no Postgres. Each section is BEFORE
  (verbatim replica of the shipped-before shape) vs AFTER (replica of the
  shipped-after shape, which the source now matches), with a value-equivalence
  assertion and a `GATE FAIL … rollback` `exit(1)` if the AFTER arm fails to beat
  BEFORE on its gate metric (M1 gates on bytes/op — the RAM win; M2/M3 on allocs/op).
  Tunables: `M1_ITERS` (2000), `PAYLOAD` (262144), `CHUNKS` (4000), `BENCH_ITERS` (200000).
- **Q1–Q2:** live dev **PostgreSQL 16** (schema from `migrations/`), reads
  `DATABASE_URL` from `.env`. Each section seeds its own fixture (`bench25_*` /
  `bench25-*` markers, torn down around the run), asserts an equivalence gate
  (result set identical BEFORE vs AFTER — mismatch `exit(1)`s), and gates on p50
  wall strictly decreasing. Q1's `playlist_items.file_id` FK is bypassed during
  seeding with `session_replication_role = replica` (superuser) purely to isolate
  the query shape without a `storage.files` fixture. Tunables: `Q1_PLAYLISTS` (100),
  `Q1_PASSES` (30), `Q2_CONTACTS` (1000), `Q2_PASSES` (20), `Q2_VCARD_KB` (8).
- Built with `RUSTFLAGS="-C target-cpu=x86-64-v3"` (the checked-in
  `.cargo/config.toml` pins `target-cpu=native`, which `SIGILL`s on this session's
  host under AVX-512 — see ROUND23/24; local build-flag override only, the config
  is unchanged).
- Verified beyond the benches: `cargo fmt --all --check` clean,
  `cargo clippy --features bench -- -D warnings` clean, and the contact / playlist /
  encrypted-backend / delta-upload unit tests pass.
