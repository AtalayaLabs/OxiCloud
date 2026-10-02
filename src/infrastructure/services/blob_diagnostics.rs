//! Reverse-lookup helpers shared by the storage consistency tenants.
//!
//! `blobs_consistency` (DB-side: refcount drift) and
//! `backend_consistency` (backend-side: missing / orphaned / corrupted
//! bytes) both answer the same operator question when they emit a
//! finding — *which files does this hash break?* — so the query lives
//! here rather than in whichever tenant happened to need it first.

use sqlx::PgPool;

/// Every layer of an error, outermost first.
///
/// A finding that records only `e.to_string()` keeps the outermost message and
/// discards the cause — which is how a degraded provider produced a `data_loss`
/// finding whose entire explanation was `"stream read: streaming error"`, with no
/// HTTP status and no way to tell a stalled-stream abort from a connection reset.
///
/// Walks `std::error::Error::source`, so on S3 the chain reaches the SDK error
/// that actually knows what happened.
pub(crate) fn error_chain(e: &dyn std::error::Error) -> Vec<String> {
    /// Defensive: a cyclic or pathological chain must not produce unbounded
    /// finding detail.
    const MAX_DEPTH: usize = 8;
    let mut out = Vec::new();
    let mut cur: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = cur {
        out.push(err.to_string());
        if out.len() >= MAX_DEPTH {
            break;
        }
        cur = err.source();
    }
    out
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);
    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_ref()
                .map(|b| b.as_ref() as &(dyn std::error::Error + 'static))
        }
    }

    /// The defect this exists for: findings recorded only the outermost message,
    /// so a degraded provider produced `data_loss` explained as nothing but
    /// "stream read: streaming error" — no status, no cause.
    #[test]
    fn walks_every_layer_outermost_first() {
        let e = Layer(
            "stream read: streaming error",
            Some(Box::new(Layer(
                "dispatch failure",
                Some(Box::new(Layer("503 SlowDown", None))),
            ))),
        );
        assert_eq!(
            error_chain(&e),
            vec![
                "stream read: streaming error",
                "dispatch failure",
                "503 SlowDown"
            ]
        );
    }

    #[test]
    fn a_sourceless_error_is_one_layer() {
        assert_eq!(error_chain(&Layer("flat", None)), vec!["flat"]);
    }

    /// A cyclic or pathological chain must not produce unbounded finding detail.
    #[test]
    fn depth_is_capped() {
        let mut e = Layer("deepest", None);
        for _ in 0..50 {
            e = Layer("wrap", Some(Box::new(e)));
        }
        assert_eq!(error_chain(&e).len(), 8);
    }
}
