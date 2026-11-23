# Round 10 — auth alloc purge, parent-resolution herd batching, query-shape pack, NC conditional revalidation

Benchmark-gated, same rule as ROUND2-9: every change ships with a
BEFORE/AFTER benchmark and equivalence/safety gates; an AFTER that doesn't
beat its BEFORE gets rolled back or redesigned. Two items this round went
through exactly that loop: the stack integer formatters first benchmarked
SLOWER than `to_string()` and were rewritten (§13) before adoption, and the
first parent-batching design (a channel task) measured 66 µs of pure hop
overhead per sequential miss and was replaced by the leader-inline protocol
(§10) before adoption.

Measured on 4 cores / 15 GiB, local PostgreSQL 16 (fsync off), release
profile; frontend on Node 26 / vitest 4 (jsdom). Reproduce any row with the
command in its section.

## Summary

| # | change | key metric | before → after |
|--:|---|---|---|
| 1 | Authenticated-request identity build (`Arc<str>` claims + inline `SmolStr` role) | allocs / ns per request | 4 → 1 allocs · 77 → 59 ns |
| 2 | Basic-auth cache hit (`Arc<str>` cached identity) | allocs / ns per DAV request | 3 → 0 allocs · 62 → 41 ns |
| 3 | Share download metadata double-fetch → `_preloaded` | queries / ms per download | 2 → 1 · 0.700 → 0.321 ms (**2.18x**) |
| 4 | Contact-group summary → `COUNT(*)` | ms, 500-member group | 5.76 → 0.39 (**14.9x**) |
| 5 | `save_faces` per-face INSERT loop → UNNEST batch | ms, 30-face image | 5.90 → 1.52 (**3.9x**) |
| 6 | Playlist reorder per-track UPDATE loop → UNNEST | ms, 500-track reorder | 167.0 → 2.6 (**63.7x**), now atomic |
| 7 | Search page files∥folders `tokio::join!` | ms per search | 4.16 → 2.87 (**1.45x**) |
| 8 | Move pre-check drive lookups `join!` | ms per move | 0.664 → 0.311 (**2.14x**) |
| 9 | Trash listing partial `(drive_id, trashed_at) WHERE is_trashed` | ms per page (30-drive box) | 0.615 → 0.496 (**1.2x**) |
| 10 | Parent-resolution herd batching (leader-inline) | parent queries, 100-thumb cold herd | **100 → 2** · herd wall 65.5 → 35.4 ms (**1.9x**) |
| 11 | Folder-cascade single-flight (`try_get_with`) | ltree queries, same-folder cold herd | K → 1 (rode along with §10's gates) |
| 12 | NC preview + avatar honour `If-None-Match` | bytes/req on revalidation (e2e) | preview 5 004 → 0 · avatar 196 992 → 0 |
| 13 | `common::fmt` integer LUT rewrite | ns/op vs `to_string()` | i64: 33.7 → **16.1** (std: 22.5) |
| 14 | NC PROPFIND int props + trashbin dates → stack fmt | allocs per 500-row page / 2000-item bin | 1 501 → 1 · 4 002 → 2 (wall 1.11x / 1.13x) |
| 15 | WebDAV scope probe, base_url snapshot, cookie OnceLock, JWT keys, cipher Arc, request-id | see §15 | e.g. base_url listing 71.5 µs → 1 ns |
| 16 | CalDAV update/delete gate narrow read | ms (11 KB `ical_data` row) | 0.323 → 0.308 (1.05x + 11 KB less wire) |
| 17 | Legacy favorites/recents rows: binary UUID decode | ms per 500-row page | 2.80 → 2.58 (**1.09x**) |
| 18 | SPA: search stale-guard + AbortController | completed round-trips, 10-query burst | 10 → 1; stale-clobber eliminated |
| 19 | SPA: `getFolder` in-flight dedup | requests per cold deep-link | 2 → 1 |
| 20 | SPA: `gridColumns` matchMedia hoist | MQL constructions / 10k calls | 10 000 → 0 · 13.4 → 2.6 ms (**5.2x**) |

Plus: `count_files` (a dead port method whose impl ran the full paginated
search) deleted outright; NC chunk PUT retry-probe folded into the open
(`create_new`, one stat less per chunk); tantivy per-query analyzer
double-clone dropped.

## [1][2] Auth hot path — the ROUND6/9 deferred "cheapest known win"

Every authenticated request built `CurrentUser` by deep-cloning
`username`/`email` out of the cached `Arc<TokenClaims>` and `to_string`ing
the live role — the exact 2-allocs-per-request item deferred since ROUND6,
plus two more nobody had counted (`flags.role.to_string()` in
`decide_live_role`, and the Basic-auth cache handing out 3 owned Strings
per moka hit on every DAV request).

Now: `TokenClaims.username/email` are `Arc<str>` (serde `rc`, same one
allocation at decode time), `CurrentUser.username/email` are `Arc<str>`
(refcount bumps), `CurrentUser.role` is an inline `SmolStr` fed by
`LiveRole::Active(SmolStr)` + the new `UserRole::as_str()` (`&'static`,
zero alloc), and `CachedBasicAuthResult` carries the same types so a
Basic hit is bumps + a 24-byte memcpy. JSON wire shape is unchanged
(byte-identity gated); OpenAPI keeps `String` via `value_type`.

> **Superseded since Round 10, in the same direction.** The numbers above
> stand as measured, but both role carriers are gone:
> `CurrentUser.role: SmolStr` + `is_external: bool` became one `Copy`
> `Caller` enum (no string, so no 23-byte buffer and no reparse), and
> `CachedBasicAuthResult` lost its role field entirely — both callers
> dropped it as stale, since that cache has a 300 s TTL against the live
> flags cache's 30 s. So the AFTER side of sections 1 and 2 is now smaller
> than when it was measured, and `UserRole::as_str()` survives only for
> rendering (logs, the wire), not for comparison.

```
cargo run --release --features bench --example bench_round10_micro
# [1] identity build  BEFORE 77.2 ns / 4.00 allocs → AFTER 59.0 / 1.00
#     gate: fields + serialized JSON byte-identical
# [2] basic-auth hit  BEFORE 62.2 ns / 3.00 allocs → AFTER 40.9 / 0.00
```

The JWT service also stopped rebuilding `EncodingKey`/`DecodingKey`/
`Validation` per call (now fields; the verify-miss path drops 4 allocs,
§15), and `generate_access_token`'s `format!("{}", role)` became
`as_str().to_string()`.

## [3] Share download — the handler already had the DTO

`serve_share_file` fetched the file DTO for ETag/Range handling, then
called `get_file_optimized`, which re-ran the same metadata query. The
authenticated download path already used `get_file_optimized_preloaded`;
the public-share path now does too (the DTO is moved, not cloned — the
one later use of `size` is captured first).

```
cargo run --release --features bench --example bench_round10_queries
# [1] BEFORE 2 queries 0.700 ms/download → AFTER 1 query 0.321 ms (2.18x)
```

## [4] Contact-group summary — 500 vCards hydrated to compute `len()`

`get_group` called `get_contacts_in_group` — full rows (vCard TEXT that
can carry base64 photos + 3 JSONB arrays parsed per contact) — and kept
only the count. New `count_contacts_in_group` port method backed by
`SELECT COUNT(*)` on `group_memberships`.

```
# [3] 500 members: BEFORE hydrate-all 5.762 ms → AFTER COUNT(*) 0.387 (14.9x)
```

## [5][6] Write-path N+1 loops → one UNNEST statement

- `save_faces`: one INSERT per face inside a transaction → a single
  multi-row `INSERT … SELECT FROM unnest(...)` (the `bbox` float4[] rides
  as 4 parallel component arrays, reassembled server-side). 30-face image:
  5.90 → 1.52 ms (**3.9x**); gate re-reads a stored row field-by-field.
- `reorder_items`: one autocommit UPDATE per track (non-atomic — a
  mid-loop failure left a half-applied order) → one
  `UPDATE … FROM unnest($1) WITH ORDINALITY`. 500-track reorder:
  167.0 → 2.6 ms (**63.7x**); gate compares every final position.

## [7][8] Independent awaits overlapped (`join!`, decide-by-bench)

- `SearchService::search` awaited the content-index lookup, the file page
  and the folder query serially in both branches; `suggest_with_perms` had
  the correct shape since ROUND4. All three are independent; the two SQL
  arms measured 4.16 → 2.87 ms (**1.45x**) with identical results.
  (Content-index enabled widens the win — the Tantivy arm is the long pole
  and now overlaps both queries.)
- File/folder move pre-check ran the source-drive-policies and
  destination-drive point reads serially before comparing:
  0.664 → 0.311 ms (**2.14x**). Adopted per the ROUND6 protocol (these are
  two independent point reads whose server-side execution parallelizes —
  the shape that wins even on a local socket).
- The NC PROPFIND folder-HEADER trio (favorites + oc:fileid + dead props
  for the folder's own entry, on the TTFB critical path of every folder
  PROPFIND) got the same `join!` ROUND9 gave the per-page child triples.

## [9] Trash listing — the dropped-index gap

Migration 20260904 removed `user_id` and with it the only trash-listing
index; what remained forced either a live-rows scan of the drive
(`idx_files_drive_id`) or an all-tenants trash scan
(`idx_files_trash_expiry`). New partial pair
`(drive_id, trashed_at) WHERE is_trashed` (migration 20260920000000)
bounds the read to the caller's drives' trashed rows, pre-ordered for the
`trashed_at`/`deletion_date` keysets. On a 30-drive box (3 000 live + 25
trashed each): 0.615 → 0.496 ms (**1.2x**); the gap widens with drive size
since the BEFORE plan scans live rows. Identical row sets gated; the
retention sweeper keeps its global expiry index.

## [10][11] Cold-album herd — parent batching + cascade single-flight

ROUND9 §10 left the cold first view paying one parent PK read per photo
and noted batching "needs a wider engine API". It doesn't: the browser
fires its thumbnail requests near-simultaneously, so the batching can live
INSIDE `file_parent_folder_cached`:

- **Leader-inline protocol** (`parent_batch` slot): an idle miss marks
  itself leader (one mutex op) and runs its point query exactly as before
  — the sequential path is unchanged (a channel-task design measured
  ~66 µs/miss of hop overhead and was REJECTED). Misses arriving while
  the leader is in flight park a oneshot; the leader serves them all with
  ONE `id = ANY($1)` charity batch after its own read; a second wave is
  handed to a detached drainer so the leader's response is never delayed
  by more than one batch. A cancelled leader's guard wakes every parked
  waiter to re-elect; waiters that exhaust retries fall back to the
  inline point read. Requested-but-absent ids memoise as `None`,
  matching the point read's semantics.
- **`cascade_grant_cached` → `try_get_with`** (the ROUND3 auth-herd
  pattern): K concurrent files of one album all recurse into the SAME
  folder decision; get→compute→insert let each run the ltree query.
  Single-flight collapses that to one loader; moka never caches loader
  errors, preserving error semantics.

```
cargo run --release --features bench --example bench_thumbnail_cascade_cache
# thumbs=100 (folder-grant recipient, no drive membership)
# ROUND8 cold (union/file)      65.59 ms   655.90 µs/thumb
# AFTER cold sequential         65.53 ms   655.33 µs/thumb  (parity — no
#   sequential regression from the protocol; this box's high per-query
#   latency compresses the R9 decomposition margin visible on faster I/O)
# AFTER warm (revalidation)      0.14 ms     1.42 µs/thumb  (unchanged)
# AFTER herd (concurrent cold)  35.35 ms   353.47 µs/thumb  (~1.9x vs
#   sequential cold — and the real shape of a grid's first view)
#   parent queries for the herd: 2   (was 100)
# gates: all original ROUND8/9 safety gates (outsider denied, clear_role
#   revoke denies immediately, direct-grant sibling isolation) plus NEW:
#   herd answers == point-read answers per file, parent queries < K/4
```

## [12] NC preview + avatar — ETag existed, nobody compared it

- `/index.php/core/preview` set an immutable ETag but never read
  `If-None-Match` — every gallery revalidation re-ran NC-id resolve, file
  fetch, authz, blob-hash query, thumbnail cache read and full body. The
  handler now answers 304 right after the authz check (never before it).
- `/index.php/avatar/{user}/{size}` had no ETag at all, and re-decoded the
  stored data URI on every request (for WebP avatars: a full image decode
  + PNG encode per request). Now: content-hash ETag (over the stored URI,
  computed before any decode), 304 on match, and the WebP→PNG transcode
  memoised in a 32-entry moka keyed by content hash.

End-to-end (real server + curl loops, the PHOTOS-ETAG methodology; 60
requests per arm):

```
# preview  200: 5 004 bytes/req  1.35 ms   →  304: 0 bytes  1.25 ms
# avatar   200: 196 992 bytes/req 2.30 ms  →  304: 0 bytes  1.97 ms
# gates: fresh GET 200 with ETag; matching If-None-Match → 304 empty;
#        stale If-None-Match → full 200. All six pass.
```

Per NC client per cache-lapse this removes ~197 KB (avatar) + ~5 KB/photo
(previews) of transfer plus the per-request DB/disk work behind them.

## [13] `common::fmt` — the bench caught our own helpers losing

The round's first micro run showed the PROPFIND int-field port SLOWER on
wall despite 1 500 fewer allocs. An isolated interleaved probe confirmed:
the byte-at-a-time div-by-10 loop in `u64_str` (33.7 ns) lost to
`u64::to_string()` (22.5 ns) — std renders via a 2-digit lookup table.
Rewrote `u64_str`/`i64_str` (and the date helpers' `push2`) on the same
`DEC_LUT` technique, dropping `i64_str`'s temp-buffer copy:

```
# interleaved probe, 20M ops/arm
# to_string 22.5 ns   i64_str BEFORE 33.7 ns → AFTER 16.1 ns
# to_rfc2822 42.7 ns  rfc2822_utc 33.6 ns
```

This speeds every existing ROUND4-9 call site (`d:getcontentlength`,
`oc:size`, digest lengths, dates) as well as the new ones.

## [14] NC PROPFIND / trashbin emit stragglers

With §13 in place, the remaining `to_string()`/`to_rfc2822()` fields moved
to the stack helpers: `oc:fileid`, `nc:creation_time`, `nc:upload_time`,
quota bytes (files + folders writers), and the trashbin's per-item
modified/deletion-time/fileid (which still ran the chrono interpreter).

```
# [3] 500-row page  BEFORE 95.5 µs / 1501 allocs → AFTER 86.3 / 1   (1.11x)
# [4] 2000-item bin BEFORE 318.5 µs / 4002 allocs → AFTER 280.7 / 2 (1.13x)
# gates: XML byte-identical in both harnesses
```

## [15] Micro-pack (each gated in `bench_round10_micro`)

- **WebDAV scope probe**: `format!("{prefix}/")` per request → borrow-only
  `strip_prefix` pair. 38 → 3.5 ns, 1 → 0 allocs, identical routing.
- **`ShareService.base_url`**: `env::var("OXICLOUD_BASE_URL")` + rebuild
  PER DTO ROW → constructor snapshot. 500-row listing: 71.5 µs → 1 ns.
- **`cookie_secure`**: 4 env-var resolutions + duplicate SECURITY log
  lines per login → `OnceLock` (process-invariant by definition).
- **JWT verify miss**: fresh `Validation` + `DecodingKey` per decode →
  service fields. 4 748 → 4 527 ns, 17 → 13 allocs (HMAC dominates).
- **`EncryptedBlobBackend`**: per-op clone of the expanded AES-256 round
  keys → `Arc` bump. 30.8 → 13.3 ns per hand-off.
- **Request-id header**: `Uuid::to_string` + `HeaderValue::from_str` →
  stack-encode. 85 → 46 ns, 2 → 1 allocs, identical bytes.
- **NC chunk PUT**: the retry-detection `stat` per chunk folded into the
  open — `stream_body_to_path` now opens `create_new` first and reports
  `created_fresh` (AlreadyExists → truncate-open), the ROUND9 §5a pattern
  applied to the NC surface.

## [16] CalDAV update/delete gate — narrow `calendar_id` read

The service fetched the FULL event row (with `ical_data` — 11 KB in the
benched shape, unbounded with attendees/VALARMs) only to read
`.calendar_id` for the authz gate. New `find_calendar_id_by_event_id`
scalar. 0.323 → 0.308 ms on a local socket (1.05x) — adopted for the
direction: the win is the row width off the wire, which grows with event
size and network distance. (This is NOT the deferred authz-reorder — the
gate still runs before the mutation, same order.)

## [17] Legacy favorites/recents rows — the ROUND6 §10 port, with a catch

The two legacy listing methods still shipped `::TEXT` casts. Porting them
to binary decode surfaced that `auth.user_favorites.id` is a SERIAL
`integer`, not a UUID — the bench's identity gate caught the wrong decode
before it could ship (decode as `i32`, render app-side). 500-row page:
2.80 → 2.58 ms (**1.09x**), rendered tuples identical.

## [18][19][20] SPA pack (vitest gates)

```
cd frontend && npx vitest run src/routes/search/staleGuard.bench.test.ts \
  src/lib/api/endpoints/folderDedup.bench.test.ts src/lib/utils/grid.bench.test.ts
```

- **Search stale-response guard** (the flag open since ROUND7): rapid-fire
  query/sort/filter changes had no seq token and no abort — a slow older
  response could clobber a newer one, and superseded recursive searches
  ran to completion server-side. `run()` now carries a sequence token +
  `AbortController` (threaded through `searchFiles`/`searchSuggest`);
  AppShell's suggest box got the same guard. 10-query burst: completed
  round-trips 10 → 1; final result provably fresh (BEFORE ends on the
  STALE query).
- **`getFolder` in-flight dedup** (the `resolveUser` pattern): cold
  deep-links fired the same folder-metadata GET twice (breadcrumbs +
  drive-id resolver). Concurrent duplicates now share one request;
  sequential calls still refetch (freshness unchanged, gated).
- **`gridColumns`**: a fresh `matchMedia` (style read) per call inside the
  grid windowing derives → one module-level MQL fed by its `change`
  listener (the photos-timeline fix applied to the shared util). 10k
  calls: 10 000 → 0 MQL constructions, 13.4 → 2.6 ms; output identity
  gated across the breakpoint, crossings propagate via the listener.

## Rejected / reworked this round (the discipline working)

- **Channel-task parent batcher**: correct, but the mpsc+oneshot
  round-trip measured 65.9 µs per sequential miss — a pure regression for
  the non-concurrent case. Replaced with leader-inline (§10).
- **First stack-formatter port**: slower than `to_string()` on wall
  (§13); adopted only after the LUT rewrite made it faster on BOTH axes.
- **`uf.id` as binary UUID**: wrong type entirely (SERIAL int) — the
  equivalence gate caught it; shipped as `i32` decode instead.

## Deferred / flagged (not shipped this round)

- **CalDAV authz-before-fetch reorder** — still awaiting maintainer
  sign-off per the authz-change convention (ROUND9 flag stands).
- **Grouped file/grid views are unvirtualized** (files group-by and
  ResourceList grid sections mount every row; the flat/list paths are
  windowed) — a UI-behaviour change big enough to want its own pass.
- **`search_files_paginated`'s `COUNT(*) OVER()` + OFFSET** — keyset would
  change the API's total-count contract; needs a product decision on
  whether search totals can become approximate/capped.
- **`ResourceList.selectedEntries`** recomputes an O(N) filter per
  selection toggle once the toolbar is visible; hosts often shadow it
  with their own copy. Needs a small API rework (getter or id-index).
- **Chunk-upload `progress.bin`** full rewrite per chunk (REST surface) —
  debouncing trades crash-resume granularity; flagged for discussion.
- **`CachedBlobBackend::local_blob_path`** sync `stat` on the reactor
  (remote-backend deployments' media hooks) — needs an async variant of
  the port method; low urgency.
