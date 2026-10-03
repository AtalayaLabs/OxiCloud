# Plan — Cache optimisation across the file-download hot path

**Status:** draft 2026-10-03, from a live sandbox investigation. Three items
below are **DONE** (same session); the rest are design-captured, awaiting
prioritisation. Driven by an observed 1.7-second cold-read on a 232 KB S3
blob and a general "every image is slow" symptom in a sandbox with a 240 MB
`.blob-cache` and S3 backend.

Each item carries its own state marker at its heading, same convention as
`storage-consistency.md`.

| item | state |
|---|---|
| §1 Timing instrumentation at tier boundaries | **DONE** — three `tracing::info!` channels, 1 LoC each at the loader / S3 call / handler-drop |
| §2 `X-Oxicloud-Cache` response header | **DONE** — phase 1 (`HIT`/`MISS`); phase 2 (`HIT-BACKEND`) deferred, see §4 |
| §3 S3 keepalive heartbeat | **DONE** — `head_bucket()` every 45s to pin the hyper pool warm |
| §4 Phase 2 of `X-Oxicloud-Cache`: `HIT-BACKEND` plumbing | **TODO** |
| §5 Admin banner "enable the local disk cache" | **DONE** — `storage_cache_recommended` flag on `/api/admin/dashboard` |
| §6 Pre-warm the content cache on first access | **TODO** |
| §7 Instrumentation parity on streaming paths | **TODO** |
| §8 Frontend prefetch for gallery navigation | **TODO** |
| §9 Optional disk-cache TTL for privacy-sensitive deployments | **PROPOSAL** |

The three shipped items were taken first because they're the diagnostic
foundation: without §1 you can't tell which tier is slow; without §2 the
operator can't see it from a client; without §3 the slowest cold-read case
observed (idle pool past hyper's 90 s timeout → full TCP + TLS handshake)
never improves no matter what else lands.

---

## §1 Timing instrumentation at tier boundaries — DONE

### Why

The baseline question "my image took 2.7 s to open, where did that go?" had
no answer in the logs: `oxicloud::http` showed only request end-to-end;
nothing attributed the time to moka vs. local-disk cache vs. S3 round-trip.
Any optimisation lands blind without this.

### What

Three `tracing::info!` channels, independently filterable, each one line per
event:

- `oxicloud::content_cache` — on moka miss, logs `cache_key`, `loaded`,
  `size_bytes`, `duration_ms` for the loader future. Shows the
  through-cache cost, including the single-flight follower case.
- `oxicloud::s3_backend` — on every `get_object` call, logs `hash`,
  `content_length`, `duration_ms`, `outcome`. Shows the raw S3 RTT.
- `oxicloud::download` — on every file-download response, RAII-dropped at
  handler return. Logs `file_id`, `duration_ms`. For streaming responses
  this is time-to-headers (body streams after); for `Bytes` responses it
  bounds the whole thing.

Code: `src/infrastructure/services/file_content_cache.rs`,
`src/infrastructure/services/s3_blob_backend.rs`,
`src/interfaces/api/handlers/file_handler.rs::DownloadTimer`.

Enable with:

```bash
RUST_LOG=info,oxicloud::download=info,oxicloud::content_cache=info,oxicloud::s3_backend=info
```

### Follow-ups

See §7 — the streaming path and `get_blob_range_stream` are NOT yet
instrumented. Range + TIER-2 downloads currently emit a `download`
line but no backend-tier line, so they attribute to "unknown" when
slow.

---

## §2 `X-Oxicloud-Cache` response header (phase 1) — DONE

### Why

Operators / users seeing a slow image need a one-glance signal from the
browser DevTools pane without touching server logs. Also useful to CI
tests: can assert `MISS` on first view, `HIT` on second, which pins the
cache behaviour across refactors.

### What

Three-value header on `OptimizedFileContent::Bytes` responses:

- `HIT` — moka served the assembled bytes from memory. No I/O.
- `HIT-BACKEND` — moka missed; every chunk the response needed came from
  the local `.blob-cache` tier. No remote round-trip. **Phase 2 only
  — see §4; phase 1 does not emit this value yet.**
- `MISS` — moka missed; at least one chunk required the authoritative
  backend (local filesystem on local-only; S3 / Azure on remote).

Chunked files report the worst (slowest) tier any individual chunk
required.

Omitted on paths that bypass the content cache (TIER 2 streaming, Range
requests over the cacheable threshold, 304, mount files). "No header"
is the quiet signal that this request didn't go through the cache.

Code: `src/application/ports/file_ports.rs::CacheOutcome`,
`src/infrastructure/services/file_content_cache.rs`,
`src/application/services/file_retrieval_service.rs`,
`src/interfaces/api/handlers/file_handler.rs::build_cached_response`.
Test: `tests/api/cache_header.hurl` pins the MISS → HIT transition.
Doc: `docs/architecture/caching.md` § "Observing cache behaviour".

---

## §3 S3 keepalive heartbeat — DONE

### Why

Diagnostic data showed a bimodal S3 latency distribution: a warm pool gives
~35-85 ms per `get_object`; a cold connection (hyper's 90 s idle timeout
elapsed) gives 350-1700 ms for the same byte count. The variance is pure
TCP + TLS handshake cost. Users read "every image is slow" because quiet
periods between opens routinely cross the 90 s cliff.

### What

Background `tokio::spawn` task owned by each `S3BlobBackend`:

- Ticks every `KEEPALIVE_INTERVAL = 45 s` (under typical S3 server-side
  idle timeout, so our side and theirs stay in agreement).
- Fires `head_bucket()`, logs success at `debug!`, failure at `warn!`.
- Aborted on `S3BlobBackend::drop` so test harnesses that spin backends
  per-case don't leak heartbeats.

Code: `src/infrastructure/services/s3_blob_backend.rs::new` +
`KEEPALIVE_INTERVAL` constant + `Drop` impl.

### Known limits

- Only one pooled connection is kept warm. Multi-connection parallel reads
  on a cold heartbeat gap still pay their own handshake. Mitigation would
  be `pool_max_idle_per_host` + N parallel heartbeats; deferred as
  premature.
- The heartbeat is at the SDK layer only. If `OXICLOUD_STORAGE_CACHE_ENABLED`
  is on and the hot path is served from local disk, S3's connection
  can still drop under NAT without the heartbeat ever running against
  a cold peer. Not a correctness issue; just means the first-cold-day
  read after enabling the cache may still spike.

---

## §4 Phase 2 of `X-Oxicloud-Cache`: `HIT-BACKEND` plumbing — TODO

### Why

Phase 1 collapses "moka served something from local disk" and "moka served
something from S3" to the same `MISS` value. On a deployment with a 240 MB
`.blob-cache` and remote S3, these two cases have 100× different latencies
and the operator wants to tell them apart.

### What

Three plumbing options, in increasing order of code-reuse:

1. **Trait extension.** Add `BlobStorageBackend::get_blob_stream_with_outcome`
   defaulting to `Miss`. `CachedBlobBackend` overrides and reports
   `HitBackend` when the local disk served. Threads the outcome up
   through `DedupService` so the content-cache loader can combine
   per-chunk outcomes with `CacheOutcome::worst`.
2. **`tokio::task_local!` recorder.** The content-cache loader runs under
   a scope that owns an `AtomicU8` cell initialised to `HitBackend`.
   `CachedBlobBackend` demotes to `Miss` whenever the inner backend was
   reached. The loader reads the cell after the inner future resolves.
   No trait change; one new import per site.
3. **Dedup-service return extension.** `DedupService::read_blob_bytes`
   returns `(Bytes, CacheOutcome)`. Smaller scope than option 1 — only
   the one method, not the whole trait.

Preference: option 2. Threads nothing through type signatures, works for
any depth of nested cache layers, and plays well with the existing
single-flight follower case (the follower reads the recorder's final
state after awaiting, so it sees the winner's outcome).

### Deferred because

Phase 1 answers the hot operational question ("is this cached at all?")
with one bit. Three-tier adds one more bit of information at the cost of
plumbing across four crates. Not worth it until operators actually want
to distinguish `HIT-BACKEND` from `MISS` in production.

---

## §5 Admin banner "enable the local disk cache" — DONE

### Why

`OXICLOUD_STORAGE_CACHE_ENABLED` defaults to `false` by deliberate choice:
on local-filesystem backends there is no cache tier to add; on
remote-backed deployments the operator should opt in once they've
decided the SSD budget. But a fresh install that configures S3 and
forgets the cache runs with every read going cold — a correctness-of-UX
hole that defaults can't fix without regressing local-only installs.

### What

Server-computed boolean `storage_cache_recommended: bool` on
`DashboardStatsDto` (`GET /api/admin/dashboard`), true iff
`backend != Local && !cache.enabled`.

Frontend renders a `warn-card--warn` advisory on the dashboard tab —
soft, not blocking — with the mitigation (`OXICLOUD_STORAGE_CACHE_ENABLED=true`)
spelled out in the body. Hidden on local-only deployments and once the
cache is enabled.

Code: `src/application/dtos/settings_dto.rs::DashboardStatsDto`,
`src/interfaces/api/handlers/admin_handler.rs::get_dashboard_stats`,
`frontend/src/lib/api/endpoints/admin.ts::AdminDashboard`,
`frontend/src/routes/admin/[[tab]]/+page.svelte`,
`frontend/static/locales/en.json`.

---

## §6 Pre-warm the content cache on first access — TODO

### Why

The `backend_cache_cleanup_service` sweep run on a healthy sandbox
reports 876 `live` entries after days of use — i.e., only 876 blobs
have ever been read on this instance despite thousands existing. Every
first-ever view of a blob pays full S3 RTT. For gallery-heavy
deployments this means every scroll into fresh territory feels slow.

### What

New recoverable job `content_cache_warm` modelled after `blobs_consistency`:
- Walks `storage.file_blobs` (or `storage.blobs` — whichever is the
  dedup registry on current code).
- For each unseen hash, calls `DedupService::read_blob_bytes(hash)` and
  discards the result; the side effect is a hot local-disk cache entry
  and a moka slot.
- Budget-aware: honours a configurable `warm_rate_bps` so a 50 GB
  cache doesn't drain S3 in one burst on job start.
- Paginated via the standard recoverable-job cursor pattern; pauses on
  S3 transient failure instead of thrashing.

Trigger shape:

```json
POST /api/admin/jobs/content_cache_warm/trigger
{"warm_rate_bps": 10_485_760}   // 10 MB/s
```

Interaction with §5's banner: once the operator enables
`OXICLOUD_STORAGE_CACHE_ENABLED`, offer a one-click "warm it now" in the
same admin panel.

### Open questions

- Should warm-on-upload fire automatically (write-through to the content
  cache at upload time)? Simpler than a separate job, but double-caches
  upload bytes that may never be read again. Needs a size cap to avoid
  blowing out moka on a bulk import.
- Does warming compete with real user reads for S3 bandwidth? Needs the
  budget knob; also probably needs a backoff when the real-request rate
  spikes.

---

## §7 Instrumentation parity on streaming paths — TODO

### Why

The §1 probes miss the streaming path. A 16 MB PDF opened via TIER 2
streaming emits exactly one line (`oxicloud::download duration_ms=13`),
which is handler-end-to-headers; the actual S3 fetch underneath isn't
logged because it's a `get_blob_range_stream` call, not
`get_blob_stream`. So the operator sees "download complete, 13 ms" and
has no visibility into the body-streaming cost.

### What

Add the same `tracing::info!(target: "oxicloud::s3_backend", ...)` probe to
`S3BlobBackend::get_blob_range_stream` (and `get_blob_stream_at_offset`
if that exists). Mirror the existing `get_blob_stream` shape so the
log search terms stay stable: `s3 get_object_range ok (headers)` with
`range`, `content_length`, `duration_ms`, `outcome`.

Also consider: the `DownloadTimer` RAII fires on handler return, which
for streams is before the body is sent. A second timing probe
somewhere in the stream close path would bound actual time-to-last-byte,
useful for S3 throughput diagnosis. Deferred — needs an Axum
tower-layer, not a handler change.

---

## §8 Frontend prefetch for gallery navigation — TODO

### Why

Even with every server-side cache tuned, the user-perceived latency on
"next photo" is dominated by the cold read of a never-before-opened
blob. The server has 100 ms of warm-pool S3 RTT to pay; the browser
can hide that by prefetching the next likely file while the user is
still looking at the current one.

### What

Two prefetch triggers, both pure frontend:

1. **Gallery grid open.** On folder-open in Photos / People / Places,
   prefetch the first N thumbnails via `<link rel="prefetch">` or a
   quiet `fetch(...)` with `priority: 'low'`. N starts at ~20
   (visible viewport + 1 scroll row).
2. **Photo lightbox.** On opening image X, prefetch neighbours X-1
   and X+1. On arrow-key navigation, continue extending the prefetch
   horizon by one.

Prefetched requests warm:
- The browser cache (no re-network for the actual click)
- The server-side moka content cache (next request short-circuits the backend)
- The server's `.blob-cache` (second miss still disk-fast)
- The S3 connection pool (free side effect of running a request)

### Interaction with the message bus

If a prefetched URL has already been invalidated by a server-side
collab edit, the prefetch wastes bytes. Mitigation: respect the
`If-None-Match` header the server already honours, so an invalidated
prefetch returns 304 and costs only the round-trip, not the body.

### Risk: metering / bandwidth

Mobile users on cellular don't want to prefetch 20 full-res thumbnails.
Mitigation: respect `navigator.connection.saveData` and
`navigator.connection.effectiveType`, drop prefetch to 0-3 on `3g` or
below.

---

## §9 Optional disk-cache TTL for privacy-sensitive deployments — PROPOSAL

### Why

`backend_cache_cleanup` (`docs/plan/storage-consistency.md` §6) removes
`.blob-cache` entries whose content has been deleted upstream (DB
ref-count = 0), but entries whose content is **still referenced** stay
on disk forever, in plaintext (the cache sits outside the encryption
wrapper). For deployments with regulatory requirements like "plaintext
of PII must not persist past access + N hours", there is no mechanism
to bound the plaintext horizon today.

### What

New knob `OXICLOUD_STORAGE_CACHE_TTL_SECS` (unset by default — matches
current indefinite-retention behaviour). When set, adds a `time_to_idle`
to the moka index of `CachedBlobBackend`; the eviction listener already
unlinks the file on evict.

### Trade-offs

- Shorter TTL → fewer plaintext bytes on disk at any given time → more
  S3 RTT per read on long-tail content → worse latency for the "revisit
  last week's photo" case.
- Rough sizing: TTL = 24 h matches "operator reviews the day's
  activity" use case without burning cache for stale content. TTL = 1 h
  is a plaintext-horizon knob for regulatory users.

### Open

Needs a stakeholder — the current OxiCloud deployment target
(household / small team self-host) doesn't need this. Would wait for a
real request from a regulated-tenant deployment before implementing.
Shape of the knob is captured here so if the request lands, the design
is already in flight.

---

## Non-goals in this plan

Explicitly out of scope — recorded here so they don't get picked up by
mistake:

- **Client-side mouse-move → warm request.** Considered 2026-10-03;
  rejected because the client → server pool is already warm from page
  load, and warming the server → S3 pool from the client requires a
  dedicated "warm" endpoint whose only purpose is to trigger a backend
  side-effect — architecturally ugly for marginal gain. §3's
  server-side heartbeat does the same job without any client knowledge.
- **Streaming responses carrying `X-Oxicloud-Cache`.** The streaming
  path bypasses the content cache entirely (by design — the whole
  reason TIER 2 exists); the header's values would need a parallel
  interpretation for that path. "No header on streams" stays the
  quiet signal.
- **Replacing moka with a persistent in-memory tier (Redis / memcached).**
  The 10k-user target doesn't justify a second infra component. moka
  under the configured max-capacity handles the workload; the problem
  is cold reads, not cache size.

---

## Rough priority

If the above work lands incrementally, the order with the best
cold-read-latency ROI is:

1. §5 banner (shipped) — tells operators the knob exists.
2. §3 heartbeat (shipped) — kills the 1.7 s pool-timeout cliff.
3. §6 pre-warm job — kills the "every first view is cold" cost for
   deployments willing to pay the one-time S3 fetch.
4. §8 frontend prefetch — hides remaining cold reads behind the
   user's own interaction latency.
5. §7 streaming instrumentation — unblocks diagnosing the next
   round of slow-case reports.
6. §4 phase 2 of the header — makes diagnosis sharper, doesn't
   reduce latency directly.
7. §9 TTL — only if a stakeholder requests it.
