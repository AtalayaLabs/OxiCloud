# Plan — Derived content as blobs (tier-2 refactor)

**Status:** design captured 2026-08-02, revised 2026-08-12 (keying,
CDC reuse, backend-dispatch rule, client-preview boundary). Not
implemented. Follow-up to `fix/services-use-blob-abstraction` — that
PR normalised the **read-side** (services consume blobs through
`BlobStorageBackend` uniformly). This plan tackles the **write-side**:
services that today write derived artifacts (thumbnails, transcodes)
to a local sidecar directory and would benefit from writing them
through the backend abstraction instead.

## Context — the three-tier storage taxonomy

Today the codebase runs three implicit tiers with no explicit
separation:

| Tier | Purpose | Loss on reboot? | Where today |
|---|---|---|---|
| **1 — Temp** | Pure scratch, deletable at reboot | ✅ fine | `std::env::temp_dir()` (ad-hoc callers). Now unified under `OXICLOUD_TEMP_DIR` (`AppConfig::temp_dir`). |
| **2 — Persistent spool** | Caches; expensive but rebuildable | ⚠️ possible but painful | `<storage_path>/.thumbnails/`, `<storage_path>/.transcoded/`, `<storage_path>/.blob-cache/`, `<storage_path>/.search-index/`, `<storage_path>/.plugin-logs/` — all mixed into tier-3 storage today. |
| **3 — Persistent data** | Source of truth | ❌ never | `<storage_path>/.blobs/` (Local) OR S3/Azure bucket, via `BlobStorageBackend`. Already correctly configured via `OXICLOUD_STORAGE_ENTRIES`. |

**Today's misclassification**: tier-2 sidecars live under
`<storage_path>` — the same directory as tier-3 source-of-truth
data. Ops resizing / moving / backing up tier-3 accidentally moves
tier-2 caches with it. Loss of tier 2 is expensive (regenerate
thumbnails for every photo) but not data loss; conflating them
means backup policies can't distinguish "must preserve" from "can
rebuild".

## Multi-instance driver

Single-instance: tier-2-as-local-cache works fine. Rebuild after
reboot is annoying but bounded.

Multi-instance (2+ app servers behind a load balancer):

- Request for thumbnail `abc123.jpg` lands on instance A → generates
  it → stores locally at `.thumbnails/abc123.jpg`.
- Same-URL retry lands on instance B → cache miss → regenerates
  from source.
- Every derived asset gets recomputed N times (N = instance count)
  at worst.

Wasteful compute, wasteful storage, inconsistent latency. The
long-term fix is to put derived content on tier 3 (shared) with a
local read-through cache in front. Multi-instance isn't the near-
term target, but the design should leave the door open.

## Design decision — derived content IS a blob

The blob storage abstraction is already:

- Backend-agnostic (Local / S3 / Azure)
- Encrypted uniformly (`EncryptedBlobBackend` wrapper)
- Consistency-checked (`blobs_consistency`)
- Migratable (`backend_migration`)
- Rotatable (`backend_rotate`)
- Multi-instance-ready (S3/Azure natively; Local via network mount)

Reusing it for derived artifacts means no second abstraction to
build and maintain, and all the operational surface (audit,
migration, key rotation) applies to derived content by default.

### Keying — content-address only pure functions of the content

**The rule:** an artifact may be keyed by its source's content hash
**iff** it is a deterministic pure function of the source bytes.
Anything influenced by user choice must be keyed by the resource it
was attached to, never by content.

| Artifact | Function of | Content-keyable? |
|---|---|---|
| server thumbnail | `f(blob bytes, variant)` | ✅ any user uploading identical bytes derives identical output — nothing to poison |
| transcode | `f(blob bytes, target)` | ✅ |
| extracted text | `f(blob bytes)` | ✅ — `storage.blob_extracted_text` |
| face vectors | `f(blob bytes)` | ✅ — `faces.faces` |
| client-uploaded preview | `f(user's choice)` | ❌ **must be file-keyed** — see below |

This isn't a new pattern: `storage.blob_extracted_text` already
chose content-keying for the same reason, and the migration says so
(`migrations/20260701000000_content_search_index.sql:22-28`) —
"extraction is keyed by `blob_hash`, not by file: N copies of the
same PDF cost ONE extraction, and rename/move/copy never
re-extract." `faces.faces` is keyed on `blob_hash` too. Thumbnails
are the same class of artifact, and file-keying them would make
them the odd one out among three sibling features while costing:

- **the dedup fast path** — `ThumbnailRefreshHook::on_file_created`
  returns early when `!is_new_blob`, so 100 users uploading the same
  photo cost one render. File-keying means either N renders or a
  join back through `file_metadata.blob_hash` (content-keying
  through the back door, slower and with more code).
- **free copies** — `on_file_copied` is a no-op today precisely
  because the key is content.

For the derived side the hash is over the **produced** bytes, so:

- Two files with identical thumbnails (same variant of the same
  source → identical bytes → identical hash) share the physical
  blob. Dedup wins for free.
- Two variants of one source (256px vs 512px) produce different
  blobs. Also correct.

The variant spec lives in the referring DB row, not in the storage
key. Storage stays one keyspace; ownership stays per-service.

### Schema

```sql
CREATE TABLE storage.derived_blobs (
    source_hash VARCHAR(64) NOT NULL,   -- source Blob (no FK — see below)
    kind        TEXT NOT NULL,          -- 'thumbnail' | 'transcode'
    variant     TEXT NOT NULL,          -- 'icon.webp', 'large.jpg', 'av1.720p'
    blob_hash   VARCHAR(64) NOT NULL,   -- the DERIVED Blob
    size        BIGINT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (source_hash, kind, variant)
);
CREATE INDEX ON storage.derived_blobs(blob_hash);
```

One table with a `kind` discriminator rather than separate
`storage.thumbnails` / `storage.transcodes`: `count_references`,
`list_referenced_blobs`, the GC cascade and the
`backend_consistency` walk are byte-identical between the two, so
two tables means maintaining a duplicate of that SQL — which
`AGENTS.md § Code duplication` forbids. `kind` costs one column.

**No FK on `source_hash` or `blob_hash`**, for the reason the
search-index migration already documents: a file hash resolves to
either `storage.blobs` (legacy whole blob) or
`storage.chunk_manifests` (CDC file hash), so the reference can't be
expressed as a single FK. Orphans are reclaimed by GC instead.

**No `origin` column.** The 2026-08-02 draft had
`origin = 'server_derived' | 'client_provided'` so consistency-check
severity could differ. With client previews excluded from this table
(below), every row is server-derived and the severity is uniformly
"warning, regenerable" — the column carries no information. Re-add
it only if that changes.

**Deletion is app-layer, not `ON DELETE CASCADE`.** A SQL cascade
would delete the mapping row without decrementing
`storage.blobs.ref_count`, silently leaking a reference. So
`on_blob_deleted(source_hash)` does
`DELETE FROM storage.derived_blobs WHERE source_hash = $1 RETURNING blob_hash`
then `remove_reference()` per row. That is a one-for-one replacement
of today's `delete_blob_thumbnails` unlink loop — no new mechanism.

### Write path — reuse `store_from_stream`, don't special-case CDC

Derived blobs go through `DedupService::store_from_stream()`
**unchanged**. An earlier draft of this plan proposed a dedicated
single-chunk write path to avoid the manifest row; that was
optimising the wrong thing. What it costs to reuse the standard
path:

- **+1 `chunk_manifests` row per derived blob.** `CDC_MIN_CHUNK` is
  65_536, and WebP q82 thumbnails land at roughly 3–8 KB (icon),
  15–30 KB (preview), 40–90 KB (large) — so icon and preview are
  always below the minimum chunk size and emit exactly one chunk;
  `large` occasionally splits into two.
- **One manifest lookup per read**, served from RAM by
  `manifest_cached` — not a per-read query.
- **A CDC pass over ~12 KB** — below min-chunk, so a single pass
  with no boundary search. Negligible.

What it buys: zero new write path, zero new read path, and
ref-counting, GC, `add_reference` / `remove_reference` and both
consistency jobs all work on derived blobs with no changes, because
they are already manifest-aware. One `store_from_stream` call == one
reference == one `derived_blobs` row, symmetric on delete.

Two gotchas that come with the reuse:

1. `store_from_stream` fires `fire_blob_creation_hooks`. The only
   non-dispatcher `BlobLifecycleHook` implementor today is
   `ThumbnailService`, whose `on_blob_created` is a no-op — so no
   spurious work now. But creating a thumbnail now fires
   blob-creation hooks, so any future hook (search indexing, face
   detection) must not treat every new blob as user content. Add a
   `kind`/content-type guard to the hook contract before the second
   implementor lands.
2. GC of a *derived* blob fires `on_blob_deleted(derived_hash)`,
   which looks for derived-of-derived rows, finds none and stops.
   One level deep, terminates — but that's incidental rather than
   designed. Comment it at the recursion point.

### No backend-type branching in the service

`src/AGENTS.md` already forbids the shape this refactor must avoid:

> - **Never hand-craft blob paths.** No `blob_root: PathBuf` fields […]
> - **Persistent state = backend**, not `<storage_path>/*` sidecars.

`thumbnail_service` is cited there as a read-side reference impl,
and the read side is compliant. The write side is the violation:
`thumbnails_root: PathBuf` is exactly the banned field, and
`get_thumbnail_path()` is the hand-crafted path.

There must be **no `if backend is local { .thumbnails/… } else { blob }`
anywhere in the service.** `ThumbnailService` holds
`Arc<DedupService>`, reads and writes through it, and never learns
which backend it is sitting on. The local-vs-remote difference is
expressed once, as decorator composition in `common/di.rs`:

```rust
if self.config.storage.cache.enabled && active_backend_kind != StorageBackendType::Local {
    blob_backend = Arc::new(CachedBlobBackend::new(blob_backend, &cfg));
}
```

Local deployments write derived blobs into `<storage>/.blobs/` via
`LocalBlobBackend` with no cache decorator (a cache would be a
byte-identical second copy on the same disk). Remote deployments get
the cache. Same service code both ways — and it is the same branch
that already governs source blobs, not a new one.

What this deletes from `thumbnail_service.rs`:

- `thumbnails_root` field, `get_thumbnail_path()`,
  `ThumbnailSize::dir_name()` (becomes `variant()`, feeding the DB
  column)
- `initialize()`'s `create_dir_all` loop
- every `fs::read` / `fs::write` / `fs::metadata` / `remove_file`
- the three `all_exist` stat loops → one indexed query each
- `delete_blob_thumbnails`'s unlink loop and the duplicate of it
  inside `on_blob_deleted`

Net deletion, which is the main argument for this shape.

Two adjacent cleanups in the same file: `stream_blob_to_temp` uses
`self.thumbnails_root` as its temp directory and must move to
`AppConfig::temp_dir` per the `OXICLOUD_TEMP_DIR` rule; and
`store_external_thumbnail`'s `ext-{file_id}.jpg` write is the last
hand-crafted path once the rest is converted.

### Client-uploaded thumbnails — file-keyed, and NOT in `derived_blobs`

Some clients (NC desktop, mobile apps) upload their own encoded
previews alongside the file. These are **not derivable** — losing
them means asking the client to regenerate, which may not be
possible.

They are also **not a function of the content**, and that makes
content-keying a cross-user poisoning vector:

1. User A uploads file X plus a preview that does not depict X.
   There is no validation that can catch this — verifying a preview
   faithfully represents its source means re-deriving and comparing,
   at which point accepting the client's upload is pointless.
2. User B uploads the same file X. Dedup matches on `source_hash`.
3. B is served A's preview.

So client previews **stay file-keyed and stay out of
`storage.derived_blobs`.** This is a precondition for the table
having no `source_file_id` column, not an independent choice — the
two decisions must land together.

Today's code is already safe, implicitly, via its choice of
filename; the risk is losing that in the migration:

- write is file-keyed — `store_external_thumbnail` writes only
  `ext-{file_id}.jpg`, never into the `{blob_hash}.{ext}` space
- read checks the file-keyed path *before* the content-keyed one in
  `get_cached_thumbnail`, so a preview only surfaces for its own
  file
- `PUT …/thumbnail/{size}` requires `Permission::Update` on the
  target file

Two things keep the boundary after the refactor:

- **The schema is self-guarding.** With no `source_file_id` column
  there is nowhere to put a client preview, so making the mistake
  requires writing a migration — which gets reviewed. Keeping a
  nullable column would be an attractive nuisance; omitting it *is*
  the enforcement.
- **State the invariant in the migration**, in the style
  `content_search_index.sql` already uses: *keyed by `source_hash`
  because derived content is a pure function of the source bytes;
  client-uploaded previews are NOT derived, are user-chosen, and
  must never be keyed here or one user's preview would be served for
  another user's identical file.*

Note the pressure this is under: a "unify the two write paths" pass
would produce exactly the vulnerability. The two axes are separate —
client previews share the **dispatch** (they become blobs via
`store_from_stream` like everything else, satisfying the
no-backend-branching rule) while keeping **file keying** in their own
small mapping table. Storage dedup is preserved either way, because
the derived bytes are still content-addressed: two byte-identical
previews converge on one object at `ref_count = 2`. Only the mapping
is per-file, and the mapping is the part that carries the trust
problem.

Current scope note: the SvelteKit frontend has no caller of the
thumbnail `PUT` endpoint outside a test file — server-side ffmpeg
extraction (`generate_video_thumbnails_background`) replaced it. So
the client-preview table is speculative; don't build it until
there's a real feature ask, and don't pay dual-key complexity for it
in the meantime.

## `BlobReferenceSource` — reference tracking abstraction

Adding new blob-owning services without teaching the ref-count +
consistency machinery about them causes silent orphaning risk:
`dedup_gc` sees `ref_count = 0` and reaps live content.

The extension point:

```rust
#[async_trait]
pub trait BlobReferenceSource: Send + Sync {
    /// Short stable identifier for logs / consistency finding
    /// `source` fields. Suggested: `"files"`, `"chunks"`,
    /// `"derived"`.
    fn source_name(&self) -> &'static str;

    /// Count of references this source holds on `blob_hash`.
    /// Called by `blobs_consistency` when recomputing
    /// `refcount_mismatch` findings.
    async fn count_references(&self, blob_hash: &str) -> Result<u64, DomainError>;

    /// Iterate the source's referenced blobs, paged by the
    /// implementation's natural cursor (typically a DB PK). Used
    /// by `backend_consistency` to walk the backend against the
    /// union of all sources.
    async fn list_referenced_blobs(
        &self,
        cursor: Option<Vec<u8>>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<Vec<u8>>), DomainError>;

    /// Optional notify hook: `dedup_gc` reaped this blob. Sources
    /// that maintain their own denormalised refcount table can
    /// clean up here. Most sources leave this as the trait default
    /// (noop).
    fn on_blob_reaped(&self, _blob_hash: &str) {}
}
```

Wired via a `BlobReferenceRegistry`:

```rust
pub struct BlobReferenceRegistry {
    sources: Vec<Arc<dyn BlobReferenceSource>>,
}

impl BlobReferenceRegistry {
    pub fn register(&mut self, source: Arc<dyn BlobReferenceSource>);
    pub async fn total_references(&self, hash: &str) -> Result<u64, DomainError>;
    // ... etc.
}
```

Current implicit sources become the first two explicit
registrations:

- `FilesReferenceSource` — wraps `storage.files.blob_hash`
- `ChunksReferenceSource` — wraps `storage.chunk_manifests.chunk_hashes[]`

Tier-2 migration adds exactly **one** more (not one per kind, since
`derived_blobs` is one table):

- `DerivedBlobsReferenceSource` — wraps `storage.derived_blobs`;
  `count_references` is
  `SELECT count(*) FROM storage.derived_blobs WHERE blob_hash = $1`

Then:

- **`dedup_gc`** — orphan iff `registry.total_references(hash) == 0`
  (with the existing grace window). No per-service GC changes, and
  none needed for derived blobs either: they carry real `ref_count`
  in `storage.blobs`, so the existing grace-window sweep is already
  correct.
- **`blobs_consistency`** — `refcount_mismatch` recomputes via
  `registry.total_references`. New services register → automatically
  covered.
- **`backend_consistency`** — walks the backend and unions all
  `list_referenced_blobs` streams for the "did we lose bytes"
  check.

**Hard ordering dependency.** `blobs_consistency` derives expected
refcounts from `file_metadata` + manifests only. Ship
`storage.derived_blobs` before the registry and *every* derived blob
becomes a `refcount_mismatch` finding — a flood, and one an operator
might "repair". Step 1 must precede step 3 below; this is not a
sequencing preference.

## Read path and caching

Request carries `(file_id, size, format)`; the derived hash is
BLAKE3 of bytes that don't exist yet, so it is not computable from
the request. Read order:

1. **moka RAM tier** — keyed by `(source_hash, size, format)`.
   (Today it is keyed by `file_id`; rekeying to `source_hash` costs
   nothing — the handler already has the hash from the row it
   loaded — and stops N copies of one photo occupying N entries for
   identical bytes.)
2. **DB** — `derived_blobs` lookup for the variant. This is free:
   the miss path already queries `get_blob_hash(&id)`, so a
   `LEFT JOIN` on `derived_blobs` returns the source hash and the
   derived hash in one query. Net DB cost unchanged from today.
3. **`dedup.read_blob_bytes(derived_hash)`** — through the normal
   backend stack, which is where the disk cache lives.
4. Generate only if step 2 found no row.

**The disk cache is `CachedBlobBackend`, reused unchanged.** No
thumbnail-specific cache, no second root path. Routing derived
blobs through the same stack gets, for free:

- **single-flight per hash** — a gallery cold-load where 50 clients
  race one thumbnail collapses to one S3 GET
- **write-through on put** — the instance that generated the
  thumbnail already has it locally, so upload→view-gallery never
  round-trips to S3
- byte-budget eviction with unlink, and a restart-survivable index

**One thing to test rather than assume.** Thumbnails and source
blobs have opposite cache profiles: small / hot / expensive to
regenerate versus large / cold / cheap to re-fetch. Sharing one LRU
budget (`OXICLOUD_STORAGE_CACHE_MAX_SIZE`, default 50 GB) means a
sequential multi-GB video read is exactly the scan pattern that
flushes a working set — and flushing thumbnails costs a re-render,
not a re-download. moka 0.12's TinyLFU admission *should* resist
this (a one-shot large entry denied rather than evicting
frequently-hit small ones), and the eviction listener unlinks on
`RemovalCause::Size` so a denied entry shouldn't leak its file. Both
deserve a test, because the failure mode is silent: unexplained CPU
on the thumbnail path, not a cache metric.

If it does interfere, the fix that preserves the one-implementation
rule is **two instances of `CachedBlobBackend` with separate
budgets** — same type, same factory, different config — not a second
cache type. Honest cost: `DedupService` would need a second backend
handle plus a content-class selector, since derived blobs have
manifests and must still be reassembled through `DedupService`. Ship
the shared cache, measure, split only if the test says so. The knob
would be `OXICLOUD_STORAGE_DERIVED_CACHE_MAX_SIZE` alongside the
existing `OXICLOUD_STORAGE_CACHE_MAX_SIZE`.

## Cost consequences to budget for

1M photos × 3 sizes ≈ **3M additional backend objects and 3M
additional `storage.blobs` + `chunk_manifests` rows** (~100 KB of
derived content per photo, so ~100 GB total). Two costs land:

- **PUT requests at upload** — one-off, modest (~$15 per 3M on AWS
  pricing). Use `put_blob_from_bytes_unsynced` + a batched
  `sync_blobs`, not `put_blob_from_bytes`: the latter does a
  `head_object` before every PUT on S3, doubling the request count
  for an idempotency check content-addressing already guarantees.
- **`blobs_consistency` request amplification** — it does one
  `blob_exists` HEAD per blob row, so 4× the rows is 4× the S3
  requests, forever. This is the dominant recurring cost. Fix by
  diffing against `list_blob_hashes` pages in bulk (one LIST per
  1000 keys instead of 1000 HEADs) rather than per-row probes.
  Alternative: exempt derived rows from the byte-level check, since
  they are regenerable — but the bulk-LIST fix is better and helps
  source blobs too.

Note for object-store deployments: IA/Glacier tiers bill a 128 KB
minimum per object, so an 8 KB icon is billed at 128 KB. **Open
option** (config, not a code branch): persist only the `large`
variant to tier 3 and derive icon/preview from it on demand — the
render path already decodes once for all sizes, and resampling an
800px WebP is sub-millisecond. That is 1M objects instead of 3M. It
changes only *how many variants get a `derived_blobs` row*, so it
stays a single code path.

## Sidecar directories after this refactor

| Sidecar today | After |
|---|---|
| `.thumbnails/` | **Gone.** Derived blobs live in tier 3; caching is `CachedBlobBackend` in `.blob-cache/`, keyed by hash like every other blob. |
| `.transcoded/` | Gone, same shape. |
| `.blob-cache/` | Stays. Owned by `CachedBlobBackend`, path via `OXICLOUD_STORAGE_CACHE_PATH`. Now serves derived blobs too. |
| `.search-index/` | Open question — see non-goals. |
| `.plugin-logs/` | Ops-local; stays. |
| `.uploads/` | Tier 1 already; migrates to `OXICLOUD_TEMP_DIR`. |

**`OXICLOUD_SPOOL_DIR` is probably no longer worth adding.** The
2026-08-02 draft reserved it as the home for `.thumbnails/`,
`.transcoded/` and `.blob-cache/`. The first two now disappear
entirely rather than becoming local caches, and `.blob-cache/`
already has its own `OXICLOUD_STORAGE_CACHE_PATH`. That leaves
`.search-index/` (a non-goal) and `.plugin-logs/` (ops-local) — not
enough to justify a new config surface. Either drop step 2 below or
reduce it to documenting the existing `OXICLOUD_STORAGE_CACHE_PATH`
as the tier-2 relocation knob.

## Delivery order

Coarse — the trait + registry ship first (empty-impl for
`FilesReferenceSource` + `ChunksReferenceSource` mirroring today's
hardcoded SQL). New sources bolt on independently.

1. **`BlobReferenceSource` trait + registry** in
   `application/ports/`. `FilesReferenceSource` and
   `ChunksReferenceSource` implementations mirroring current SQL;
   wire into `dedup_gc` + `blobs_consistency` behind an integration
   test that proves the union equals the pre-refactor count on a
   real DB. **Blocks step 3** (see the ordering dependency above).
2. ~~`OXICLOUD_SPOOL_DIR`~~ — reduced to a docs change, or dropped;
   see the sidecar section.
3. **`ThumbnailService` writes go through `DedupService`**. New
   `storage.derived_blobs` table + `DerivedBlobsReferenceSource`.
   Deletes `thumbnails_root`, `get_thumbnail_path`, and every
   filesystem call in the service. Fold the `derived_blobs` lookup
   into the handler's existing `get_blob_hash` query.
4. **`blobs_consistency` bulk-LIST diff** — before, or immediately
   after, step 3 lands at scale; per-row HEADs do not survive a 4×
   row count.
5. **`ImageTranscodeService`** — same shape, `kind = 'transcode'`,
   no new table.
6. **Client-uploaded previews** — only if a real feature ask
   appears. Separate file-keyed table; never in `derived_blobs`.

Each slice is independently mergeable. Delivery span: rough
estimate ~2 weeks end-to-end.

## Naming clarifications to land alongside this refactor

Two consumer-facing terminology issues that surfaced during the
read-side normalisation (2026-08-02). They're not code-breakers,
but they cost every new implementor a mental round-trip, so
they belong in the tier-2 sweep:

### 1. `DedupService` name is implementation-shaped, not consumer-shaped

From a consumer's perspective the service is "the thing that
reads and writes file content by hash." Deduplication is one
internal responsibility (alongside CDC chunking, ref-counting,
GC). The name `DedupService` narrates HOW it works, not WHAT it
is — new service authors read the name and don't realise they
should be routing every blob read through it.

Suggested rename: **`BlobHandler`** (or `ContentStore` /
`BlobStore` — pick one and commit). Public surface stays
identical; consumers write `Arc<BlobHandler>` and call
`blob_handler.read_blob_bytes(hash)`. Internal doc-comments
document dedup + CDC + GC as strategies.

Scope: ~35 files (grep `DedupService|dedup_service`), mechanical.
Keep as one commit inside the tier-2 refactor so reviewers see
"rename" independently from the substantive changes.

### 2. `blob` overloaded across two scales

Current usage:

- `storage.blobs` — the physical storage table; rows are BYTES
  written to a backend. Post-CDC, most entries are chunks
  (fragments), not whole files.
- `storage.chunk_manifests` — the CDC manifest that references a
  set of `storage.blobs` rows to reconstitute a file.
- `file.blob_hash` — the hash a file row points at; either a
  whole-file blob (legacy) OR a chunk-manifest (post-CDC).

The word "blob" carries two meanings: *whole-file content* (what
a user thinks of when they say "download the blob") vs
*physical byte-payload on disk* (what the storage backend
holds — may be a whole file, may be a chunk fragment).

Proposed clarification for the tier-2 sweep:

- **Blob** = the abstraction of "content of a file", identified
  by BLAKE3 of the plaintext. Consumers work at this level. What
  `DedupService`/`BlobHandler` returns.
- **Chunk** = a physical byte-payload written to the backend,
  identified by its own BLAKE3. Storage-backend-internal.
- **Manifest** = the map from a Blob to one or more Chunks.

Schema rename (deferred, requires migration):

- `storage.blobs` → `storage.chunks` (that's what it actually holds now)
- `storage.chunk_manifests` → `storage.blob_manifests` (or keep — arguable)
- `BlobStorageBackend` trait → `ChunkStorageBackend` — reads and
  writes physical chunks, not blobs

`file.blob_hash` semantics stay — references a Blob via its
manifest OR (for pre-CDC legacy) points directly at a single-chunk
Blob whose hash equals its lone chunk's hash.

Scope for this rename: ~23 files touch the SQL, plus a migration
for the table rename. Not free. Ship AFTER the tier-2 write-side
lands so we don't stack schema changes.

## Non-goals

- **Tantivy `.search-index/`** — memory-mapped by design, doesn't
  fit the blob-storage abstraction. Separate future decision:
  keep local, snapshot-to-backend periodically, or retire Tantivy
  for PG-native full-text.
- **`.plugin-logs/`** — ops-local operational data, not user
  content. Stays local.
- **Client thumbnail negotiation protocol** — the wire-level API
  for how clients push their previews. Design piece for the
  photo/mobile team when there's a real feature ask.
- **A per-derived-content cache type.** Explicitly rejected: the
  disk cache is `CachedBlobBackend`, instantiated by the existing
  DI branch. Splitting budgets means a second *instance*, never a
  second implementation.

## References

- `docs/architecture/backend-storage.md` — the wrapper stack, header
  format, consistency check, migration semantics that derived
  content inherits.
- `docs/plan/storage-multi-entry.md` — tier-3 configuration model.
- `docs/plan/storage-key-rotation.md` — encryption/rotation applies
  to derived blobs too.
- `src/AGENTS.md` — the read-side rule enforcing backend
  abstraction, the no-hand-crafted-paths rule, and the
  persistent-state-is-backend rule this plan implements.
- `migrations/20260701000000_content_search_index.sql` — the
  content-keying precedent (`storage.blob_extracted_text`) and its
  rationale.
- Memory note `project_services_bypassing_blob_backend` — audit
  history of the pre-normalisation bypasses.
