//! The two implicit blob-reference sources, made explicit.
//!
//! Before this module, "who references this hash" lived as hardcoded SQL
//! inside `blobs_consistency`'s refcount recompute and `dedup_gc`'s reap
//! predicate. These two implementations reproduce that SQL **exactly** —
//! the fragments below sum to today's `actual_ref_count` expression — so
//! the registry can be wired in without changing any observed count.
//!
//! See `docs/plan/derived-blobs.md` and
//! [`crate::application::ports::blob_reference_ports`].

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::application::ports::blob_reference_ports::{
    BlobReferenceRegistry, BlobReferenceSource, RefLevel,
};
use crate::domain::errors::DomainError;

/// Aliases used inside the emitted fragments.
///
/// Deliberately distinct from the aliases the sweeps use for their outer
/// row (`b` for `storage.blobs`, `m` for `storage.chunk_manifests`): a
/// fragment reusing `m` would shadow the outer alias in the manifest-level
/// sweep and silently correlate against itself.
const FILES_ALIAS: &str = "cnt_f";
const MANIFEST_ALIAS: &str = "cnt_m";
const DERIVED_ALIAS: &str = "cnt_d";

/// Fragment for [`FilesReferenceSource`], as a free function so the SQL
/// shape can be tested without constructing a pool — it is a property of
/// the module, not of an instance.
fn files_ref_sql(level: RefLevel, outer_hash_expr: &str) -> Option<String> {
    let f = FILES_ALIAS;
    match level {
        // Legacy whole-file blobs only — CDC files are counted at the
        // manifest level, and counting them here too would double up on the
        // single-chunk hash alias.
        RefLevel::Chunk => Some(format!(
            "(SELECT COUNT(*) FROM storage.files {f}
               WHERE {f}.blob_hash = {outer_hash_expr}
                 AND NOT EXISTS (
                     SELECT 1 FROM storage.chunk_manifests {MANIFEST_ALIAS}
                      WHERE {MANIFEST_ALIAS}.file_hash = {f}.blob_hash
                 ))"
        )),
        RefLevel::Manifest => Some(format!(
            "(SELECT COUNT(*) FROM storage.files {f}
               WHERE {f}.blob_hash = {outer_hash_expr})"
        )),
    }
}

/// Short-circuiting existence form of [`files_ref_sql`].
///
/// `dedup_gc` evaluates this per candidate manifest, so counting every
/// referrer where existence would do is a real cost on a heavily-deduplicated
/// blob. This is also the exact shape the reap predicate used before the
/// registry existed, so wiring it in changes no plan.
fn files_exists_sql(level: RefLevel, outer_hash_expr: &str) -> Option<String> {
    let f = FILES_ALIAS;
    match level {
        RefLevel::Chunk => Some(format!(
            "EXISTS (SELECT 1 FROM storage.files {f} \
             WHERE {f}.blob_hash = {outer_hash_expr} \
             AND NOT EXISTS (SELECT 1 FROM storage.chunk_manifests {MANIFEST_ALIAS} \
             WHERE {MANIFEST_ALIAS}.file_hash = {f}.blob_hash))"
        )),
        RefLevel::Manifest => Some(format!(
            "EXISTS (SELECT 1 FROM storage.files {f} WHERE {f}.blob_hash = {outer_hash_expr})"
        )),
    }
}

/// Fragment for [`ChunksReferenceSource`]. See [`files_ref_sql`].
fn chunks_ref_sql(level: RefLevel, outer_hash_expr: &str) -> Option<String> {
    match level {
        RefLevel::Chunk => {
            let m = MANIFEST_ALIAS;
            Some(format!(
                "(SELECT COUNT(*) FROM storage.chunk_manifests {m}
                   WHERE {outer_hash_expr} = ANY({m}.chunk_hashes))"
            ))
        }
        // A manifest is never referenced by another manifest.
        RefLevel::Manifest => None,
    }
}

/// Fragment for [`ContentDerivedReferenceSource`].
///
/// **Manifest level only.** A derived artifact's `blob_hash` names a Blob
/// (its own manifest), never a chunk. Contributing at the chunk level would
/// double-count, because a thumbnail is almost always single-chunk and its
/// manifest hash therefore equals its lone chunk's hash.
fn content_derived_ref_sql(level: RefLevel, outer_hash_expr: &str) -> Option<String> {
    match level {
        RefLevel::Chunk => None,
        RefLevel::Manifest => Some(format!(
            "(SELECT COUNT(*) FROM storage.content_derived_blobs {DERIVED_ALIAS} \
             WHERE {DERIVED_ALIAS}.blob_hash = {outer_hash_expr})"
        )),
    }
}

/// Short-circuiting existence form, used by `dedup_gc`'s reap predicate.
fn content_derived_exists_sql(level: RefLevel, outer_hash_expr: &str) -> Option<String> {
    match level {
        RefLevel::Chunk => None,
        RefLevel::Manifest => Some(format!(
            "EXISTS (SELECT 1 FROM storage.content_derived_blobs {DERIVED_ALIAS} \
             WHERE {DERIVED_ALIAS}.blob_hash = {outer_hash_expr})"
        )),
    }
}

/// Every built-in blob-reference source, in one place.
///
/// THE definition of "what references a blob". `DedupService::new` uses it
/// as its construction default and hands it to the consistency jobs via
/// `reference_registry()`, so GC and the sweeps cannot disagree — and the
/// golden tests that pin the generated SQL exercise the same set production
/// runs, rather than a test-local approximation of it.
pub fn built_in_registry(pool: Arc<PgPool>) -> BlobReferenceRegistry {
    let mut registry = BlobReferenceRegistry::new();
    registry.register(Arc::new(FilesReferenceSource::new(pool.clone())));
    registry.register(Arc::new(ChunksReferenceSource::new(pool.clone())));
    // Registered before anything writes a derived blob: dedup_gc's reap
    // predicate must already know this table exists, or the first sweep
    // after the first thumbnail deletes it.
    registry.register(Arc::new(ContentDerivedReferenceSource::new(pool)));
    registry
}

// ─── storage.files ───────────────────────────────────────────────────────

/// References held by `storage.files.blob_hash`.
///
/// Contributes at **both** levels, which is why `RefLevel` is a parameter
/// rather than a property of the source:
///
/// * [`RefLevel::Manifest`] — a CDC file's `blob_hash` names a manifest.
/// * [`RefLevel::Chunk`] — a pre-CDC legacy file, whose `blob_hash` names a
///   whole-file blob with no manifest behind it. The `NOT EXISTS` guard is
///   load-bearing: for a single-chunk file the whole-file hash *equals* its
///   lone chunk's hash, so without it the row would be counted at both
///   levels.
pub struct FilesReferenceSource {
    pool: Arc<PgPool>,
}

impl FilesReferenceSource {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BlobReferenceSource for FilesReferenceSource {
    fn source_name(&self) -> &'static str {
        "files"
    }

    fn ref_count_sql(&self, level: RefLevel, outer_hash_expr: &str) -> Option<String> {
        files_ref_sql(level, outer_hash_expr)
    }

    fn ref_exists_sql(&self, level: RefLevel, outer_hash_expr: &str) -> Option<String> {
        files_exists_sql(level, outer_hash_expr)
    }

    async fn count_references(&self, blob_hash: &str) -> Result<u64, DomainError> {
        // No level split here: the question is "how many file rows name this
        // exact hash", and a hash names either a manifest or a legacy blob,
        // never both at once from the caller's point of view.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage.files WHERE blob_hash = $1")
            .bind(blob_hash)
            .fetch_one(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error("BlobRefSource", format!("files count: {e}"))
            })?;
        Ok(n.max(0) as u64)
    }

    async fn list_referenced_blobs(
        &self,
        cursor: Option<Vec<u8>>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<Vec<u8>>), DomainError> {
        // Paged by the file's own PK so the cursor is stable under concurrent
        // inserts; `blob_hash` is not unique and would skip or repeat rows.
        let after: Option<Uuid> = match cursor {
            Some(bytes) => Some(decode_uuid_cursor(&bytes)?),
            None => None,
        };

        let rows = sqlx::query(
            "SELECT id, blob_hash FROM storage.files
              WHERE ($1::uuid IS NULL OR id > $1)
              ORDER BY id
              LIMIT $2",
        )
        .bind(after)
        .bind(limit as i64)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("BlobRefSource", format!("files page: {e}")))?;

        let next = rows
            .last()
            .map(|r| r.get::<Uuid, _>("id").as_bytes().to_vec())
            .filter(|_| rows.len() == limit);
        let hashes = rows
            .iter()
            .map(|r| r.get::<String, _>("blob_hash"))
            .collect();
        Ok((hashes, next))
    }
}

// ─── storage.chunk_manifests ─────────────────────────────────────────────

/// References held by `storage.chunk_manifests.chunk_hashes[]`.
///
/// Chunk level only — a manifest never references another manifest, so
/// [`RefLevel::Manifest`] yields `None` and this source contributes nothing
/// to the manifest recompute.
pub struct ChunksReferenceSource {
    pool: Arc<PgPool>,
}

impl ChunksReferenceSource {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BlobReferenceSource for ChunksReferenceSource {
    fn source_name(&self) -> &'static str {
        "chunks"
    }

    fn ref_count_sql(&self, level: RefLevel, outer_hash_expr: &str) -> Option<String> {
        chunks_ref_sql(level, outer_hash_expr)
    }

    async fn count_references(&self, blob_hash: &str) -> Result<u64, DomainError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.chunk_manifests WHERE $1 = ANY(chunk_hashes)",
        )
        .bind(blob_hash)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("BlobRefSource", format!("chunks count: {e}")))?;
        Ok(n.max(0) as u64)
    }

    async fn list_referenced_blobs(
        &self,
        cursor: Option<Vec<u8>>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<Vec<u8>>), DomainError> {
        // Paged by the manifest PK, not by the unnested chunk hash: a single
        // manifest expands to many hashes, so the page boundary has to fall
        // between manifests or the cursor cannot be resumed unambiguously.
        let after: Option<String> = match cursor {
            Some(bytes) => Some(String::from_utf8(bytes).map_err(|e| {
                DomainError::internal_error("BlobRefSource", format!("bad chunk cursor: {e}"))
            })?),
            None => None,
        };

        let rows = sqlx::query(
            "SELECT file_hash, chunk_hashes FROM storage.chunk_manifests
              WHERE ($1::text IS NULL OR file_hash > $1)
              ORDER BY file_hash
              LIMIT $2",
        )
        .bind(after)
        .bind(limit as i64)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("BlobRefSource", format!("chunks page: {e}")))?;

        let next = rows
            .last()
            .map(|r| r.get::<String, _>("file_hash").into_bytes())
            .filter(|_| rows.len() == limit);
        let hashes = rows
            .iter()
            .flat_map(|r| r.get::<Vec<String>, _>("chunk_hashes"))
            .collect();
        Ok((hashes, next))
    }
}

fn decode_uuid_cursor(bytes: &[u8]) -> Result<Uuid, DomainError> {
    let raw: [u8; 16] = bytes.try_into().map_err(|_| {
        DomainError::internal_error(
            "BlobRefSource",
            format!("bad uuid cursor: expected 16 bytes, got {}", bytes.len()),
        )
    })?;
    Ok(Uuid::from_bytes(raw))
}

// ─── storage.content_derived_blobs ───────────────────────────────────────

/// References held by `storage.content_derived_blobs.blob_hash` — the
/// DERIVED artifact, not the source it came from.
///
/// **`source_hash` is deliberately not a reference.** It is a dependent
/// pointer: the source Blob is kept alive by the file that owns it, and when
/// that Blob is reaped these rows go with it. Counting `source_hash` here
/// would pin every source Blob for as long as a thumbnail existed.
pub struct ContentDerivedReferenceSource {
    pool: Arc<PgPool>,
}

impl ContentDerivedReferenceSource {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl BlobReferenceSource for ContentDerivedReferenceSource {
    fn source_name(&self) -> &'static str {
        "content_derived"
    }

    fn ref_count_sql(&self, level: RefLevel, outer_hash_expr: &str) -> Option<String> {
        content_derived_ref_sql(level, outer_hash_expr)
    }

    fn ref_exists_sql(&self, level: RefLevel, outer_hash_expr: &str) -> Option<String> {
        content_derived_exists_sql(level, outer_hash_expr)
    }

    async fn count_references(&self, blob_hash: &str) -> Result<u64, DomainError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.content_derived_blobs WHERE blob_hash = $1",
        )
        .bind(blob_hash)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("BlobRefSource", format!("derived count: {e}")))?;
        Ok(n.max(0) as u64)
    }

    async fn list_referenced_blobs(
        &self,
        cursor: Option<Vec<u8>>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<Vec<u8>>), DomainError> {
        // Paged by `blob_hash` itself — unlike files it IS the value we
        // return, and DISTINCT keeps a Blob shared by several variants from
        // appearing more than once per page.
        let after: Option<String> = match cursor {
            Some(bytes) => Some(String::from_utf8(bytes).map_err(|e| {
                DomainError::internal_error("BlobRefSource", format!("bad derived cursor: {e}"))
            })?),
            None => None,
        };

        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT blob_hash FROM storage.content_derived_blobs
              WHERE ($1::text IS NULL OR blob_hash > $1)
              ORDER BY blob_hash
              LIMIT $2",
        )
        .bind(after)
        .bind(limit as i64)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("BlobRefSource", format!("derived page: {e}")))?;

        let next = rows
            .last()
            .map(|(h,)| h.clone().into_bytes())
            .filter(|_| rows.len() == limit);
        Ok((rows.into_iter().map(|(h,)| h).collect(), next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::blob_reference_ports::BlobReferenceRegistry;

    /// The registry sums whatever the sources emit; these helpers exercise the
    /// same code path without needing a pool, since `ref_count_sql` is pure.
    fn summed(level: RefLevel, outer: &str) -> String {
        let frags: Vec<String> = [files_ref_sql(level, outer), chunks_ref_sql(level, outer)]
            .into_iter()
            .flatten()
            .collect();
        if frags.is_empty() {
            "0".to_string()
        } else {
            frags.join("\n + ")
        }
    }

    /// The chunk-level expression must reproduce the two terms
    /// `blobs_consistency` inlines today: legacy-only files (guarded by
    /// NOT EXISTS) plus manifests citing the chunk.
    #[test]
    fn chunk_level_reproduces_todays_two_terms() {
        let expr = summed(RefLevel::Chunk, "b.hash");
        assert!(expr.contains("storage.files"), "{expr}");
        assert!(
            expr.contains("NOT EXISTS"),
            "legacy term must keep the CDC guard: {expr}"
        );
        assert!(
            expr.contains("= ANY(cnt_m.chunk_hashes)"),
            "chunk term missing: {expr}"
        );
        assert!(expr.contains("b.hash"), "must correlate on the outer row");
        assert!(expr.contains('+'), "both terms must be summed: {expr}");
    }

    /// Only `storage.files` references a manifest, so the manifest-level
    /// expression is the single files term with no `NOT EXISTS` guard — the
    /// guard exists to keep CDC rows *out* of the chunk level, and applying
    /// it here would count nothing at all.
    #[test]
    fn manifest_level_is_files_only_and_unguarded() {
        let expr = summed(RefLevel::Manifest, "m.file_hash");
        assert!(expr.contains("storage.files"), "{expr}");
        assert!(!expr.contains("NOT EXISTS"), "{expr}");
        assert!(
            !expr.contains("chunk_hashes"),
            "chunks must not contribute at manifest level: {expr}"
        );
        assert!(!expr.contains('+'), "only one source contributes: {expr}");
        assert!(expr.contains("m.file_hash"));
    }

    /// Fragments must not use the aliases the sweeps use for their outer row
    /// (`b` for storage.blobs, `m` for chunk_manifests), or the manifest sweep
    /// would shadow its own alias and silently correlate against itself.
    #[test]
    fn fragments_avoid_outer_row_aliases() {
        for level in RefLevel::ALL {
            let expr = summed(level, "m.file_hash");
            for bad in [
                "storage.files f",
                "storage.files b",
                "chunk_manifests m ",
                "chunk_manifests b",
            ] {
                assert!(!expr.contains(bad), "alias collision at {level:?}: {expr}");
            }
        }
    }

    /// A source declining a level must drop out of the sum entirely, which is
    /// what keeps manifest-only tables out of the chunk recompute where the
    /// single-chunk hash alias would double-count them.
    #[test]
    fn chunks_source_declines_manifest_level() {
        assert!(chunks_ref_sql(RefLevel::Manifest, "m.file_hash").is_none());
        assert!(chunks_ref_sql(RefLevel::Chunk, "b.hash").is_some());
    }

    /// Guards the registry contract the sweeps rely on: an empty level still
    /// yields a valid scalar expression.
    #[test]
    fn empty_registry_yields_zero_literal() {
        let r = BlobReferenceRegistry::new();
        assert_eq!(r.ref_count_expr(RefLevel::Manifest, "m.file_hash"), "0");
    }
}
