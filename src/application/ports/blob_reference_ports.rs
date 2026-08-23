//! `BlobReferenceSource` — the extension point that teaches ref-counting
//! and the consistency jobs about a table holding blob references.
//!
//! Before this port, "who references this hash" was hardcoded SQL in two
//! places (`dedup_gc`'s reap predicate and `blobs_consistency`'s refcount
//! recompute), both naming `storage.files` and `storage.chunk_manifests`
//! directly. Any new blob-owning table therefore risked silent orphaning:
//! `dedup_gc` sees `ref_count = 0`, or a manifest with no `storage.files`
//! row behind it, and reaps live content.
//!
//! See `docs/plan/derived-blobs.md` for the design and the coverage matrix.
//!
//! # Two levels, and why a source may span both
//!
//! [`DedupService::add_reference`] bumps `chunk_manifests.ref_count` first
//! and only falls back to `storage.blobs.ref_count`. So a reference lands
//! on whichever counter its hash names, and the two must be recomputed
//! separately — mixing them double-counts, systematically:
//!
//! * A **Blob** (`chunk_manifests.file_hash`) is "the content of a file".
//! * A **Chunk** (`storage.blobs.hash`) is a physical byte payload.
//! * For a single-chunk Blob the two hashes are **equal**, because both are
//!   BLAKE3 over the same bytes. That aliasing is why today's chunk-level
//!   recompute carries a `NOT EXISTS` clause, and why every fragment here
//!   must be level-correct rather than merely plausible.
//!
//! A source is not confined to one level: [`RefLevel::Chunk`] and
//! [`RefLevel::Manifest`] fragments are requested independently, and
//! `storage.files` legitimately contributes to both — a manifest-less
//! legacy row references a chunk, a CDC row references a Blob.
//!
//! # Why SQL fragments rather than a per-hash count
//!
//! `blobs_consistency` recomputes refcounts with **one query per page**,
//! the expected count inlined as correlated subqueries. Asking each source
//! for a count per hash would turn that into `sources × rows` round-trips —
//! a catastrophic regression on a table with millions of rows. So sources
//! contribute a *fragment* that the registry sums into the existing page
//! query, and [`BlobReferenceSource::count_references`] exists only for the
//! on-demand path (`dedup_gc` checking a single reap candidate, where the
//! candidate set is already filtered to `ref_count = 0`).

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::errors::DomainError;

/// Which counter a source's references land on.
///
/// Not a property of the source — see the module docs; the same source may
/// contribute at both levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefLevel {
    /// References a physical chunk. Feeds `storage.blobs.ref_count`.
    Chunk,
    /// References a Blob via its manifest. Feeds
    /// `chunk_manifests.ref_count`.
    Manifest,
}

impl RefLevel {
    /// Both levels, for callers that sweep each in turn.
    pub const ALL: [RefLevel; 2] = [RefLevel::Chunk, RefLevel::Manifest];

    /// Stable name for logs and consistency-finding fields.
    pub fn as_str(self) -> &'static str {
        match self {
            RefLevel::Chunk => "chunk",
            RefLevel::Manifest => "manifest",
        }
    }
}

/// One table that holds references to blob hashes.
///
/// Implementors are registered on [`BlobReferenceRegistry`] during DI.
/// Adding a blob-owning table **without** registering it is the failure
/// this port exists to prevent.
#[async_trait]
pub trait BlobReferenceSource: Send + Sync {
    /// Short stable identifier for logs and consistency-finding `source`
    /// fields — `"files"`, `"chunks"`, `"content_derived"`, …
    ///
    /// Stable across releases: log aggregators key off it.
    fn source_name(&self) -> &'static str;

    /// A correlated-subquery fragment counting this source's references
    /// **at `level`** to `outer_hash_expr`, or `None` when this source
    /// holds no references at that level.
    ///
    /// `outer_hash_expr` is the SQL expression naming the hash of the row
    /// being recomputed — `"b.hash"` when sweeping `storage.blobs`,
    /// `"m.file_hash"` when sweeping `storage.chunk_manifests`. The
    /// fragment must be a parenthesised scalar subquery so the registry can
    /// join fragments with `+`.
    ///
    /// **Identifiers only.** `outer_hash_expr` is supplied by the sweep, never
    /// by a request; no fragment may interpolate caller input.
    fn ref_count_sql(&self, level: RefLevel, outer_hash_expr: &str) -> Option<String>;

    /// Count of references this source holds on `blob_hash`, across both
    /// levels.
    ///
    /// **On-demand path only** — `dedup_gc` checking a single reap
    /// candidate. The consistency sweeps must use [`Self::ref_count_sql`];
    /// calling this per row would turn one query per page into
    /// `sources × rows` round-trips.
    async fn count_references(&self, blob_hash: &str) -> Result<u64, DomainError>;

    /// Iterate the hashes this source references, paged by the
    /// implementation's natural cursor (typically a primary key).
    ///
    /// Used by `backend_consistency` to walk the backend against the union
    /// of all sources. Returns the page plus the cursor to resume from,
    /// `None` when exhausted.
    async fn list_referenced_blobs(
        &self,
        cursor: Option<Vec<u8>>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<Vec<u8>>), DomainError>;

    /// Notification that `dedup_gc` reaped this blob.
    ///
    /// Sources maintaining a denormalised refcount can clean up here. Most
    /// leave the default noop — the mapping row is normally deleted by the
    /// owning service's `on_blob_deleted` hook instead.
    fn on_blob_reaped(&self, _blob_hash: &str) {}
}

/// The set of registered [`BlobReferenceSource`]s.
///
/// Assembled once during DI and shared (`Arc`) by `dedup_gc` and the
/// consistency jobs, so all three agree on what "referenced" means.
#[derive(Default)]
pub struct BlobReferenceRegistry {
    sources: Vec<Arc<dyn BlobReferenceSource>>,
}

impl BlobReferenceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a source. Order is irrelevant — fragments are summed and
    /// counts added.
    pub fn register(&mut self, source: Arc<dyn BlobReferenceSource>) {
        self.sources.push(source);
    }

    pub fn sources(&self) -> &[Arc<dyn BlobReferenceSource>] {
        &self.sources
    }

    /// The summed SQL expression counting every source's references at
    /// `level` to `outer_hash_expr`.
    ///
    /// Returns `"0"` when no source contributes at this level, which keeps
    /// the caller's query valid without a special case.
    pub fn ref_count_expr(&self, level: RefLevel, outer_hash_expr: &str) -> String {
        let fragments: Vec<String> = self
            .sources
            .iter()
            .filter_map(|s| s.ref_count_sql(level, outer_hash_expr))
            .collect();

        if fragments.is_empty() {
            "0".to_string()
        } else {
            fragments.join("\n + ")
        }
    }

    /// Total references held on `hash` across every source.
    ///
    /// On-demand path only — see [`BlobReferenceSource::count_references`].
    pub async fn total_references(&self, hash: &str) -> Result<u64, DomainError> {
        let mut total = 0u64;
        for source in &self.sources {
            total = total.saturating_add(source.count_references(hash).await?);
        }
        Ok(total)
    }

    /// Fan out a reap notification to every source.
    pub fn notify_reaped(&self, blob_hash: &str) {
        for source in &self.sources {
            source.on_blob_reaped(blob_hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub {
        name: &'static str,
        chunk: Option<&'static str>,
        manifest: Option<&'static str>,
        count: u64,
    }

    #[async_trait]
    impl BlobReferenceSource for Stub {
        fn source_name(&self) -> &'static str {
            self.name
        }

        fn ref_count_sql(&self, level: RefLevel, outer: &str) -> Option<String> {
            let tmpl = match level {
                RefLevel::Chunk => self.chunk?,
                RefLevel::Manifest => self.manifest?,
            };
            Some(tmpl.replace("{outer}", outer))
        }

        async fn count_references(&self, _blob_hash: &str) -> Result<u64, DomainError> {
            Ok(self.count)
        }

        async fn list_referenced_blobs(
            &self,
            _cursor: Option<Vec<u8>>,
            _limit: usize,
        ) -> Result<(Vec<String>, Option<Vec<u8>>), DomainError> {
            Ok((Vec::new(), None))
        }
    }

    fn registry() -> BlobReferenceRegistry {
        let mut r = BlobReferenceRegistry::new();
        r.register(Arc::new(Stub {
            name: "a",
            chunk: Some("(SELECT 1 WHERE {outer} = 'x')"),
            manifest: None,
            count: 2,
        }));
        r.register(Arc::new(Stub {
            name: "b",
            chunk: Some("(SELECT 2 WHERE {outer} = 'y')"),
            manifest: Some("(SELECT 3 WHERE {outer} = 'z')"),
            count: 5,
        }));
        r
    }

    #[test]
    fn chunk_level_sums_every_contributing_source() {
        let expr = registry().ref_count_expr(RefLevel::Chunk, "b.hash");
        assert!(expr.contains("b.hash = 'x'"), "{expr}");
        assert!(expr.contains("b.hash = 'y'"), "{expr}");
        assert!(expr.contains('+'), "fragments must be summed: {expr}");
    }

    /// A source returning `None` for a level must contribute nothing there —
    /// this is what keeps manifest-only tables out of the chunk recompute,
    /// where they would double-count against the single-chunk hash alias.
    #[test]
    fn manifest_level_skips_non_contributing_sources() {
        let expr = registry().ref_count_expr(RefLevel::Manifest, "m.file_hash");
        assert!(expr.contains("m.file_hash = 'z'"), "{expr}");
        assert!(!expr.contains('+'), "only one source contributes: {expr}");
    }

    /// An empty level must still yield a valid scalar expression, so callers
    /// need no special case before a registry is fully populated.
    #[test]
    fn empty_level_yields_zero_literal() {
        let r = BlobReferenceRegistry::new();
        assert_eq!(r.ref_count_expr(RefLevel::Chunk, "b.hash"), "0");
    }

    #[tokio::test]
    async fn total_references_adds_across_sources() {
        assert_eq!(registry().total_references("deadbeef").await.unwrap(), 7);
    }
}
