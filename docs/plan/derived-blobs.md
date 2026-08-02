# Plan — Derived content as blobs (tier-2 refactor)

**Status:** design captured 2026-08-02, not implemented. Follow-up to
`fix/services-use-blob-abstraction` — that PR normalised the
**read-side** (services consume blobs through `BlobStorageBackend`
uniformly). This plan tackles the **write-side**: services that
today write derived artifacts (thumbnails, transcodes) to a local
sidecar directory and would benefit from writing them through the
backend abstraction instead.

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

### Keying

Content-addressable via BLAKE3, same as source blobs. For
server-derived content the hash is over the produced bytes (not
the source), so:

- Two files with **identical thumbnails** (e.g. same 256px WebP
  crop of the same underlying image → identical bytes → identical
  hash) share the physical blob. Dedup wins for free.
- Two files with **identical originals** but **different variant
  specs** (256px vs 512px thumb) produce different blobs. Also
  correct.

The variant spec (what was rendered) lives in the referring DB row
alongside the blob hash — not in the storage key. Storage stays
one keyspace; ownership stays per-service.

### Client-uploaded thumbnails

Some clients (NC desktop, mobile apps) upload their own encoded
previews alongside the file. These are **not derivable** — losing
them means asking the client to regenerate, which may not be
possible (client offline, original file no longer present on
device).

Same storage shape: BLAKE3 of the client-provided bytes → blob.
The DB row distinguishes `origin = 'server_derived' | 'client_provided'`
so consistency-check policy can differ (missing client-provided
thumbnail = data loss finding; missing server-derived = warning,
regenerable).

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
    /// `"thumbnails"`, `"transcodes"`.
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

Tier-2 migration adds:

- `ThumbnailsReferenceSource` — wraps a new
  `storage.thumbnails(hash, blob_hash, variant_spec, origin)` table
- `TranscodesReferenceSource` — wraps
  `storage.transcodes(hash, blob_hash, target_format)` table

Then:

- **`dedup_gc`** — orphan iff `registry.total_references(hash) == 0`
  (with the existing grace window). No per-service GC changes.
- **`blobs_consistency`** — `refcount_mismatch` recomputes via
  `registry.total_references`. New services register → automatically
  covered.
- **`backend_consistency`** — walks the backend and unions all
  `list_referenced_blobs` streams for the "did we lose bytes"
  check.

## Sidecar directories after this refactor

| Sidecar today | After |
|---|---|
| `.thumbnails/` | Persisted as derived blobs in tier 3. `.thumbnails/` becomes a pure read-through cache (tier 1-ish; ephemeral, per-instance). |
| `.transcoded/` | Same shape as thumbnails. |
| `.blob-cache/` | Already a cache; stays. Owned by `CachedBlobBackend`. |
| `.search-index/` | Open question — see non-goals. |
| `.plugin-logs/` | Ops-local; stays. |
| `.uploads/` | Tier 1 already; migrates to `OXICLOUD_TEMP_DIR`. |

The persistent-spool env var reserved:

- **`OXICLOUD_SPOOL_DIR`** — path for the local read-through
  caches (`.thumbnails/`, `.transcoded/`, `.blob-cache/`). Default
  `<storage_path>/spool`. Ops can point it at a different disk
  than tier-3 storage; multi-instance deployments accept per-
  instance rebuild OR mount a shared FS here.

## Delivery order

Coarse — the trait + registry ship first (empty-impl for
`FilesReferenceSource` + `ChunksReferenceSource` mirroring today's
hardcoded SQL). New sources bolt on independently.

1. **`BlobReferenceSource` trait + registry** in
   `application/ports/`. `FilesReferenceSource` and
   `ChunksReferenceSource` implementations mirroring current SQL;
   wire into `dedup_gc` + `blobs_consistency` behind an integration
   test that proves the union equals the pre-refactor count on a
   real DB.
2. **`OXICLOUD_SPOOL_DIR`** — config + `example.env` + docs +
   `AppConfig::spool_dir`. Migrate `CachedBlobBackend` cache path
   default to `<spool_dir>/blob-cache/`.
3. **`ThumbnailService` writes go through the backend**. New
   `storage.thumbnails` table + `ThumbnailsReferenceSource`. Local
   `.thumbnails/` sidecar becomes a read-through cache pattern.
4. **`ImageTranscodeService`** — same shape as thumbnails.
5. **Client-uploaded thumbnails** — new `origin` column + upload
   API path if needed.

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

## References

- `docs/architecture/backend-storage.md` — the wrapper stack, header
  format, consistency check, migration semantics that derived
  content inherits.
- `docs/plan/storage-multi-entry.md` — tier-3 configuration model.
- `docs/plan/storage-key-rotation.md` — encryption/rotation applies
  to derived blobs too.
- `src/AGENTS.md` — the read-side rule enforcing backend
  abstraction (already shipped alongside this plan doc).
- Memory note `project_services_bypassing_blob_backend` — audit
  history of the pre-normalisation bypasses.
