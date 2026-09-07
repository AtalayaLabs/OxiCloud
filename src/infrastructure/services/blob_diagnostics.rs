//! Reverse-lookup helpers shared by the storage consistency tenants.
//!
//! `blobs_consistency` (DB-side: refcount drift) and
//! `backend_consistency` (backend-side: missing / orphaned / corrupted
//! bytes) both answer the same operator question when they emit a
//! finding — *which files does this hash break?* — so the query lives
//! here rather than in whichever tenant happened to need it first.

use sqlx::PgPool;

/// Cap on reverse-lookup file names surfaced in a finding's detail.
/// Keeps detail JSON bounded when a broken blob is referenced by
/// hundreds of files.
const AFFECTED_FILES_SAMPLE: i64 = 5;

/// Sample of file names that reference this blob — either directly
/// (`files.blob_hash = $hash`, legacy pre-CDC) or transitively via a
/// manifest (`chunk_hashes @> ARRAY[$hash]`, the post-CDC dominant
/// path). Capped so a chunk shared by 10 000 files doesn't blow up the
/// finding detail JSON. Order is arbitrary — this samples for
/// diagnosis, it does not enumerate.
///
/// Returns an empty vec on query error: a finding with no sample is
/// still a finding, and failing the sweep because the diagnostic
/// garnish didn't load would trade the whole scan for a nicety.
pub(crate) async fn affected_files(pool: &PgPool, hash: &str) -> Vec<String> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT DISTINCT f.name
          FROM storage.files f
         WHERE f.blob_hash = $1
            OR EXISTS (
                 SELECT 1 FROM storage.chunk_manifests m
                  WHERE m.file_hash = f.blob_hash
                    AND $1 = ANY(m.chunk_hashes)
               )
         LIMIT $2
        "#,
    )
    .bind(hash)
    .bind(AFFECTED_FILES_SAMPLE)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    rows.into_iter().map(|(n,)| n).collect()
}
