# Round 29 — read-path cache-serve allocs, NC REPORT href buffer, auth per-request allocs, DB over-fetch

Seven behaviour-preserving cuts, each behind a counting-allocator BEFORE/AFTER gate
that `exit(1)`s (`GATE FAIL … rollback`) unless AFTER allocates strictly fewer than
BEFORE. Sections span the four hot paths a deep re-audit surfaced that the prior 28
rounds had not reached: the content-cache serve fast path (video scrubbing), the
NextCloud REPORT emit loops, the NextCloud Basic-Auth request path, and two
Postgres over-fetch sites.

Reproduce:

```bash
RUSTFLAGS="-C target-cpu=x86-64-v3" \
  cargo run --release --features bench --example bench_round29_micro
```

| §  | site | allocs/op BEFORE→AFTER | wall |
|----|------|-----------------------:|-----:|
| A  | NC REPORT href reused buffer            | 3500 → 2003 /500-row page | 1.59× |
| B  | cache-serve borrow-probe (video scrub)  | 6 → 0 /cache hit          | 5.85× |
| C  | `read_full` single-frame zero-copy      | 1 → 0                     | 194× |
| D  | login-lockout single-alloc key          | 3 → 1                     | 2.10× |
| E  | NC composite-username parse borrow       | 1 → 0                     | 2.86× |
| F  | contact-group `vcard` over-fetch        | 200 → 0 /200-row page     | decode-shape |
| G  | admin-count `COUNT(*)` vs hydrate        | 25 → 0 /poll              | decode-shape |

---

## [B] Content-cache serve fast path: eager owned args built before the borrow-probe (HIGHEST — the hottest read path)

`file_retrieval_service::optimized_inner` (Tier 1) and `get_file_range_preloaded`
built the owned `get_or_load` arguments — `format!("\"{}\"", hash)` (the quoted
etag), `hash.to_string()` (the cache key), `id.to_string()` — **before** the cache
was probed. But `FileContentCache::get_or_load`'s first line is a lock-free
`self.get(&cache_key)` that returns on a hit and never touches `etag` / `ct` / the
load closure. So every cache **hit** — the steady state of a repeat download and of
a *range-seek storm* (video scrubbing hits `get_file_range_preloaded` on every
seek) — allocated ~3–6 Strings and immediately dropped them. The returned etag/ct
are discarded by both callers (`let (bytes, ..)`), and the response etag is built
independently from `file_dto.etag`, so the eager etag was dead on the miss path too.

AFTER probes `cache.get(&hash)` (a borrow, zero owned allocs) first and slices on a
hit; only a miss builds the owned args and calls the new `load_and_cache`.
`get_or_load` is split into `get` + `load_and_cache` (it now composes them), so the
miss path is **not** re-probed — the hit/miss stat counters stay byte-identical to a
single `get_or_load` call. Also folds in the removal of the unconditional
`content_hash.clone()` + `name.clone()` that ran for every request including the
≥10 MB streaming tier that used neither.

| arm    | ns/op | allocs/op |
|--------|------:|----------:|
| BEFORE | 241.1 |      6.00 |
| AFTER  |  41.2 |      0.00 |

**6 → 0 allocs per cache hit, 5.85× wall.** On a 200-seek video scrub this removes
~1200 throwaway allocations. Equivalence: same cached `Bytes` returned; the split
preserves the exact single-`get` stat accounting.

## [A] NextCloud REPORT emit loops: per-row href String → one reused buffer + once-encoded user

The two REPORT handlers (`report_handler`: favorites `filter-files` + `search`)
each emit a file loop and a folder loop that built `<d:href>` per row with
`nc_href(url_user, subpath)` — a fresh `String` per file row — and
`format!("{}/", nc_href(...))` — **two** Strings per folder row — while re-encoding
the constant `url_user` on every row. The hotter PROPFIND child loop was already
hoisted to a reused buffer + once-encoded prefix (ROUND19/27); the REPORT loops were
the last per-row href allocation on the NC emit surface (the ROUND20/27/28 deferred
item). AFTER adds `nc_href_into` / `nc_collection_href_into` (the 0-alloc,
write-into-a-buffer form; `nc_href`/`nc_collection_href` now delegate to them, no
duplication) and computes into one `href_buf` reused across both loops with the
`encoded_user` computed once per page.

| arm    |     ns/op | allocs/op |
|--------|----------:|----------:|
| BEFORE | 115 849.2 |   3500.00 |
| AFTER  |  72 835.0 |   2003.00 |

**1497 fewer allocs on a 500-row page, 1.59× wall.** The 2003 residual is the
per-segment `urlencoding::encode` (4 path segments/row) that AFTER keeps to stay
byte-identical; the win is the removed per-row href `String`, the folder `format!`,
and the per-row user encode. Equivalence: AFTER href bytes match BEFORE
(file + folder) across a matrix of paths.

## [C] `read_full`: single-frame blob no longer double-copied

`read_full` reassembled the blob stream with `BytesMut::with_capacity(cap)` +
`extend_from_slice` per frame. The local backend yields owned contiguous `Bytes`
frames, and a sub-`CACHE_THRESHOLD` blob arrives as exactly **one** frame — yet the
old code copied that whole payload a second time into a fresh buffer (a full-payload
memcpy + a `BytesMut` alloc) for every small cacheable download and every
uncacheable small read. AFTER returns the sole frame directly; only a multi-frame
read pays the pre-sized concat (byte-identical).

| arm    | ns/op | allocs/op |
|--------|------:|----------:|
| BEFORE | 3325.9 |     1.00 |
| AFTER  |   17.2 |     0.00 |

**1 → 0 allocs and one 200 KB memcpy removed (194× wall on the isolated copy).**
Equivalence: identical `Bytes` out; multi-frame path unchanged.

## [D] NextCloud login-lockout key: `to_lowercase()` + `format!` → one ASCII buffer

`LoginLockoutService::key` built the composite `(account, IP)` cache key with
`format!("{}|{}", username.to_lowercase(), client_ip)` — two heap allocations — on
**every** NC request (the check on the way in; a hit on the happy path is a lockout
miss). App passwords authenticate with an already-lowercase ASCII username in ~all
traffic, so AFTER renders the lowercased key into one pre-sized buffer for the ASCII
case and keeps `str::to_lowercase` only on the rare non-ASCII branch (exact Unicode,
e.g. final-sigma, semantics).

| arm    | ns/op | allocs/op |
|--------|------:|----------:|
| BEFORE |  95.7 |      3.00 |
| AFTER  |  45.7 |      1.00 |

**3 → 1 alloc, 2.10× wall.** Byte-identical key verified across {ASCII lower,
mixed-case, composite `~` marker, IPv4, IPv6, non-ASCII, `unknown`}. The lockout
decision (same key bytes, threshold, TTL) is unchanged; failed verifications still
bypass the cache and pay full Argon2.

## [E] NextCloud composite-username parse: owned clone → borrow

The `{username}~{drive_marker}` split allocated the prefix per request —
`raw_username.clone()` on the common no-marker path (a full duplicate),
`u.to_string()` + `m.to_string()` on the marker path — even though `username` is
only ever passed by reference and `raw_username` outlives every use before it moves
into `NcSession`. AFTER borrows `&str` slices out of the already-owned
`raw_username`.

| arm    | ns/op | allocs/op |
|--------|------:|----------:|
| BEFORE |  22.5 |      1.00 |
| AFTER  |   7.9 |      0.00 |

**1 → 0 allocs on the common DAV path, 2.86× wall.** Stacks with §D on the same
per-request surface. Byte-identical inputs reach every downstream call.

## [F] Contact-group listing: stop fetching the multi-KB `vcard` only to drop it

`contact_group_pg_repository::get_contacts_in_group` SELECTed `c.vcard` — the full
serialized vCard TEXT with an embedded base64 `PHOTO`, the largest column — and
decoded it into a `String` per contact, but its sole live caller
(`list_contacts_in_group`) maps every row to `ContactDto`, which has **no vcard
field**, so it was fetched, shipped, decoded, and dropped. This is the ROUND25 §Q2
`row_to_contact_lite` treatment applied to the **live** group method this time (Q2
shipped it to `get_contacts_by_group`, which has zero call sites). AFTER omits the
column and passes `String::new()`.

| arm    |    ns/op | allocs/op |
|--------|---------:|----------:|
| BEFORE | 70 121.1 |    200.00 |
| AFTER  |      1.4 |      0.00 |

The micro isolates the discarded-`String` decode (200 rows × 8 KiB): **200 → 0
per-row allocs**. The new SQL was run against a live schema (all columns resolve,
join valid, empty and populated results correct); `ContactDto` output is
byte-identical. Same unit economics ROUND25 §Q2 *measured* (6.4× wall on 1000 ×
8 KiB vCards). Bandwidth win scales with the embedded-photo size.

## [G] admin-user count: hydrate every full row → scalar `COUNT(*)`

`count_admin_users` (the system-status / initialization endpoint, polled at
bootstrap / login-page render) called `list_users_by_role("admin").len()`, fetching
every admin's full 21-column row — including the up-to-512 KiB avatar `image` data
URI and the `ui_preferences` JSONB (decoded into a discarded `serde_json::Value`
DOM) — only to take the length. AFTER adds `count_users_by_role` →
`SELECT COUNT(*) … WHERE role::text = $1` through the existing domain-trait / port
delegation pattern.

| arm    |    ns/op | allocs/op |
|--------|---------:|----------:|
| BEFORE | 12 634.9 |     25.00 |
| AFTER  |      0.7 |      0.00 |

The micro isolates the hydrate-N-rows-then-`len` cost (3 admins × a 64 KiB avatar +
JSONB DOM): **25 → 0 allocs**. Validated on a live DB with 3 seeded admins carrying
200 KiB avatars: the `COUNT(*)` returns the correct `3` while the wire payload drops
from **600 000 bytes** (the three avatars) + JSONB to **8 bytes**, and the app
hydrates zero `User` structs. Win scales with admin count, avatar size, and
PG-connection distance.

---

## Not shipped — carried forward

Concrete, still-valuable items surfaced by the same re-audit, deferred here because
they need a fixture this round can't drive, a structural change wider than an
allocation cut, or a live-DB validation harness:

- **Delta `store_loose_chunks` check-then-write (highest-value dedup item).** The
  delta upload path writes every received chunk to the backend unconditionally, then
  registers with `ON CONFLICT DO NOTHING` — unlike the main `settle_batch` ingest,
  which runs one `WHERE hash = ANY($1)` existence probe per batch and writes only
  absent chunks (the discipline the S3 backend's dropped-HEAD comment already
  assumes). Bringing the delta path to parity eliminates redundant disk writes /
  object-store PUTs for content the server already has (multi-tenant overlap,
  abandoned-upload orphan re-sends). Deferred: near-zero on a single-tenant local
  server (the highest win is on S3/Azure), it restructures the ingest, and its gate
  is a backend-write-count harness (not the allocator), so it wants its own pass. The
  frontend already negotiates a Dropbox-style batched have/need exchange, so the
  client does **not** re-upload content the server has — this is purely the
  server-side write.
- **`ingest_chunks_from_stream` end-of-stream reshape move.** The final chunk
  registration clones every newly-written 64-byte hash to reshape for `sync_blobs` +
  the UNNEST bind (`~4000 String allocs on a 1 GB upload`); the sibling sites were
  converted to `into_iter().unzip()` moves in ROUND23/25 but this one wasn't.
  Deferred: `st.written` must be restored on the two fallible error paths before
  `guard.rollback()` (which itself `mem::take`s it), so the move needs a
  `rollback_with_written` variant — error-path surgery on the ingest correctness path
  for a once-per-upload (not per-frame) alloc cut.
- **NC `parse_basic_auth` credential borrow.** The shared helper returns
  `(String, String)` via two `to_string()`s; the native Basic path already hands
  `credentials.split_once(':')` `&str` borrows to `verify_basic_auth`. Bringing NC to
  parity removes 2 allocs/request but touches a unit-tested shared helper and wants a
  `decode_basic_credentials` extraction to avoid a third copy of the base64 logic.
- **DB `create_folder` 2 round-trips → 1 `INSERT … SELECT … RETURNING`** (the drive_id
  is a pure function of the parent; `move_folder` already folds this). Needs the
  `RowNotFound → not_found` branch and a live-DB gate on the dup-name / missing-parent
  outcomes.
- **DB `list_users` / `search_users` lite SELECT** (drop `password_hash` +
  `ui_preferences`, neither in `UserDto`) and **contacts `(address_book_id, full_name,
  first_name, last_name)` composite index** for the paginated `ORDER BY`.
- **File-metadata short-TTL cache** so a range-seek storm stops re-`SELECT`ing the
  whole file row after the first seek (ROUND7 removed the per-seek authz; the metadata
  read remains). A genuinely new cache + write-invalidation wiring — its own validated
  pass.
- **S3 read zero-copy forward** and the encrypted `PLAINTEXT_EMIT_SIZE` bump — the
  ROUND25–28 carried-forward items needing MinIO / real-backend fixtures.

## Environment / methodology

- Counting global allocator (`benches/bench_round29_micro.rs`), no Postgres for the
  gate. Each section is BEFORE (replica of the shipped-before shape) vs AFTER (replica
  of the shipped-after shape, which the source now matches) with a value-equivalence
  assertion and a `GATE FAIL … rollback` `exit(1)` if AFTER doesn't allocate fewer
  than BEFORE. §F and §G additionally validated against a live PostgreSQL 16 with the
  full migration set applied and a seeded fixture (query validity, result equivalence,
  wire-byte delta).
- Built with `RUSTFLAGS="-C target-cpu=x86-64-v3"` (the checked-in
  `.cargo/config.toml` pins `target-cpu=native`, which `SIGILL`s on this host).
- Verified beyond the bench: `cargo fmt --all --check` clean,
  `cargo clippy --all-features --all-targets -- -D warnings` clean,
  `cargo test --lib` green.
