//! Embedded Tantivy index over file names + extracted content.
//!
//! Performance contract (the reason Tantivy was chosen):
//! * queries are memory-mapped posting-list lookups — µs to low ms even at
//!   millions of documents, executed on the blocking pool (never stalls the
//!   Tokio reactor);
//! * the single `IndexWriter` is owned by the background worker; request
//!   paths only ever touch the lock-free `IndexReader`.
//!
//! Index layout: one document per live file.
//! * `file_id`  — raw term, stored. Identity for upsert (delete_term + add).
//! * `user_id`  — raw term. Every query is `Must`-filtered by it, and hits
//!   are re-validated through SQL hydration afterwards (defense in depth).
//! * `name`     — tokenized file name (boosted 3x at query time).
//! * `content`  — tokenized extracted text (never stored — the index stays
//!   small; snippets come from `preview`).
//! * `preview`  — stored-only head of the extracted text used to render a
//!   snippet around the first matched term.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, STORED, STRING, Schema, TEXT, Value as _};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term, doc};

use async_trait::async_trait;
use uuid::Uuid;

use crate::application::ports::content_index_ports::{ContentHitDto, ContentIndexPort};
use crate::common::errors::DomainError;

/// Bump whenever the Tantivy schema OR the text extractor output changes in a
/// way that requires re-indexing. A mismatch with the on-disk marker wipes the
/// index directory and reseeds the dirty queue with every live file.
///
/// Version history:
///   1 — initial schema (file_id, user_id, name, content, preview)
///   2 — D0 added `drive_id` field; query filter pivots from user_id
///       to a `drive_id ∈ accessible_drives` set membership clause. On
///       deploy, every operator's index is wiped and reseeded against
///       the post-D0 schema (the worker drains the dirty queue with
///       drive_id-aware records).
pub const INDEX_SCHEMA_VERSION: &str = "2";

/// Recorded in `storage.blob_extracted_text.extractor`; rows from another
/// version are dropped at worker startup (the reseed re-extracts them).
/// Keep in lockstep with [`INDEX_SCHEMA_VERSION`].
pub const EXTRACTOR_VERSION: &str = "rust-native-1";

/// Marker file inside the index directory carrying the schema version.
const META_FILE: &str = "oxicloud-index.version";

/// RAM budget for the single-threaded writer. Indexing is a trickle-feed
/// background task — one thread and a small heap keep the footprint
/// negligible next to the request-serving process.
const WRITER_HEAP_BYTES: usize = 64 * 1024 * 1024;

/// Hard cap on query tokens — a pathological query must not fan out into
/// dozens of fuzzy automata.
const MAX_QUERY_TOKENS: usize = 8;

/// Snippet length target, in characters.
const SNIPPET_MAX_CHARS: usize = 180;

/// Minimum token length for typo-tolerant (edit distance 1) matching.
/// Short tokens produce too many false positives under fuzzy matching.
const FUZZY_MIN_CHARS: usize = 5;

/// Minimum token length for prefix expansion of the LAST query token
/// (search-as-you-type behaviour).
const PREFIX_MIN_CHARS: usize = 3;

/// One file to (re-)index. `content`/`preview` are `None` for files without
/// extractable text (images, archives…) — their NAME is still indexed.
#[derive(Debug)]
pub struct IndexDocRecord {
    pub file_id: String,
    pub user_id: String,
    /// Owning drive — written verbatim into the `drive_id` STRING field
    /// for set-membership filtering at query time. The user_id field is
    /// kept during the D0 dual-write window for rollback safety; the
    /// query filter no longer reads it.
    pub drive_id: String,
    pub name: String,
    pub content: Option<String>,
    pub preview: Option<String>,
}

#[derive(Clone, Copy)]
struct IndexFields {
    file_id: Field,
    user_id: Field,
    drive_id: Field,
    name: Field,
    content: Field,
    preview: Field,
}

pub struct TantivyContentIndex {
    /// Sole writer — owned by the background worker; the Mutex is never
    /// contended on a request path. (Writer and reader each keep the
    /// underlying `Index` alive.)
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    /// Pre-cloned analyzer for query-side tokenization (matches the index
    /// side: simple split + lowercase).
    analyzer: TextAnalyzer,
    fields: IndexFields,
}

impl TantivyContentIndex {
    fn build_schema() -> (Schema, IndexFields) {
        let mut builder = Schema::builder();
        let fields = IndexFields {
            file_id: builder.add_text_field("file_id", STRING | STORED),
            user_id: builder.add_text_field("user_id", STRING),
            drive_id: builder.add_text_field("drive_id", STRING),
            name: builder.add_text_field("name", TEXT),
            content: builder.add_text_field("content", TEXT),
            preview: builder.add_text_field("preview", STORED),
        };
        (builder.build(), fields)
    }

    /// Open the index at `dir`, wiping and recreating it when the on-disk
    /// version marker is absent or stale. Returns `(index, needs_reseed)`:
    /// when `needs_reseed` is true the caller must re-enqueue every live file.
    pub fn open_or_rebuild(dir: &Path) -> Result<(Self, bool), DomainError> {
        let marker: PathBuf = dir.join(META_FILE);
        let version_ok = std::fs::read_to_string(&marker)
            .map(|v| v.trim() == INDEX_SCHEMA_VERSION)
            .unwrap_or(false);

        if !version_ok && dir.exists() {
            // remove_dir_all is racy on Linux against concurrent writes:
            // it enumerates entries, unlinks them, then rmdirs the parent.
            // Tantivy writer threads from a previously-opened Index may
            // still be flushing segment files as we walk — the fresh
            // segment lands after our listing, and the final rmdir fails
            // with ENOTEMPTY (os error 39). CI hits this on tmpfs where
            // the race window is widest; local runs on APFS/ext4 rarely
            // reproduce it. Retry a few times with short backoffs so the
            // background writer can drain. In practice one retry is enough;
            // three gives headroom for a very slow VM without stalling the
            // real "dir can't be removed" case (permissions, EBUSY, etc.)
            // which will still surface after the last attempt.
            let mut attempt = 0;
            loop {
                match std::fs::remove_dir_all(dir) {
                    Ok(()) => break,
                    Err(e)
                        if attempt < 3
                            && matches!(
                                e.kind(),
                                std::io::ErrorKind::DirectoryNotEmpty
                                    | std::io::ErrorKind::ResourceBusy
                            ) =>
                    {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
                    }
                    Err(e) => {
                        return Err(DomainError::internal_error(
                            "ContentIndex",
                            format!("wiping stale index dir {}: {e}", dir.display()),
                        ));
                    }
                }
            }
        }
        std::fs::create_dir_all(dir).map_err(|e| {
            DomainError::internal_error(
                "ContentIndex",
                format!("creating index dir {}: {e}", dir.display()),
            )
        })?;

        let (schema, fields) = Self::build_schema();
        let mmap = MmapDirectory::open(dir)
            .map_err(|e| DomainError::internal_error("ContentIndex", format!("mmap dir: {e}")))?;
        let index = Index::open_or_create(mmap, schema)
            .map_err(|e| DomainError::internal_error("ContentIndex", format!("open: {e}")))?;

        // Single writer thread: indexing is a background trickle, not a bulk
        // load — keep the CPU/RAM footprint minimal.
        let writer = index
            .writer_with_num_threads::<TantivyDocument>(1, WRITER_HEAP_BYTES)
            .map_err(|e| DomainError::internal_error("ContentIndex", format!("writer: {e}")))?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| DomainError::internal_error("ContentIndex", format!("reader: {e}")))?;

        let analyzer = index
            .tokenizer_for_field(fields.content)
            .map_err(|e| DomainError::internal_error("ContentIndex", format!("analyzer: {e}")))?;

        std::fs::write(&marker, INDEX_SCHEMA_VERSION).map_err(|e| {
            DomainError::internal_error("ContentIndex", format!("writing version marker: {e}"))
        })?;

        Ok((
            Self {
                writer: Mutex::new(writer),
                reader,
                analyzer,
                fields,
            },
            !version_ok,
        ))
    }

    /// Apply one drained queue batch: deletes, then upserts, then ONE commit.
    /// Blocking (disk I/O + segment serialization) — call from the worker via
    /// `spawn_blocking`. The caller deletes the queue rows only after this
    /// returns `Ok`, so a crash in between re-processes the batch
    /// (idempotent: upsert = delete_term + add).
    pub fn apply_batch(
        &self,
        upserts: Vec<IndexDocRecord>,
        deletes: Vec<String>,
    ) -> Result<(), DomainError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| DomainError::internal_error("ContentIndex", "writer mutex poisoned"))?;

        for file_id in &deletes {
            writer.delete_term(Term::from_field_text(self.fields.file_id, file_id));
        }

        for record in upserts {
            writer.delete_term(Term::from_field_text(self.fields.file_id, &record.file_id));
            let mut document = doc!(
                self.fields.file_id => record.file_id,
                self.fields.user_id => record.user_id,
                self.fields.drive_id => record.drive_id,
                self.fields.name => record.name,
            );
            if let Some(content) = record.content {
                document.add_text(self.fields.content, content);
            }
            if let Some(preview) = record.preview {
                document.add_text(self.fields.preview, preview);
            }
            writer
                .add_document(document)
                .map_err(|e| DomainError::internal_error("ContentIndex", format!("add: {e}")))?;
        }

        writer
            .commit()
            .map_err(|e| DomainError::internal_error("ContentIndex", format!("commit: {e}")))?;
        Ok(())
    }

    /// Number of live documents — used by tests and the startup log line.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Tokenize `raw` with the index analyzer (simple split + lowercase).
    /// Takes the analyzer by value — the caller's per-search clone is the
    /// only one needed; cloning the boxed tokenizer chain again here doubled
    /// the per-query allocation for nothing.
    fn query_tokens(mut analyzer: TextAnalyzer, raw: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut stream = analyzer.token_stream(raw);
        while stream.advance() && tokens.len() < MAX_QUERY_TOKENS {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    /// Build the scored query: every token must match (in name OR content,
    /// exact OR fuzzy OR — for the last token — prefix), and the whole thing
    /// is `Must`-scoped to the caller's accessible drives.
    ///
    /// The drive filter is expressed as a BoolQuery with `Should` arms —
    /// at least one drive_id must match — wrapped under an outer `Must`.
    /// Equivalent to a TermSetQuery; this form avoids the API churn of
    /// rebuilding the same shape across Tantivy versions.
    fn build_query(fields: IndexFields, drive_ids: &[String], tokens: &[String]) -> Box<dyn Query> {
        // Drive-membership Must clause: union of Term(drive_id = $each).
        let drive_alternatives: Vec<(Occur, Box<dyn Query>)> = drive_ids
            .iter()
            .map(|d| {
                let q: Box<dyn Query> = Box::new(TermQuery::new(
                    Term::from_field_text(fields.drive_id, d),
                    IndexRecordOption::Basic,
                ));
                (Occur::Should, q)
            })
            .collect();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> =
            vec![(Occur::Must, Box::new(BooleanQuery::new(drive_alternatives)))];

        let last = tokens.len().saturating_sub(1);
        for (i, token) in tokens.iter().enumerate() {
            let name_term = Term::from_field_text(fields.name, token);
            let content_term = Term::from_field_text(fields.content, token);

            let mut alternatives: Vec<(Occur, Box<dyn Query>)> = vec![
                (
                    Occur::Should,
                    // Name matches outrank content matches for the same term.
                    Box::new(BoostQuery::new(
                        Box::new(TermQuery::new(
                            name_term.clone(),
                            IndexRecordOption::WithFreqs,
                        )),
                        3.0,
                    )),
                ),
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        content_term.clone(),
                        IndexRecordOption::WithFreqs,
                    )),
                ),
            ];

            if token.chars().count() >= FUZZY_MIN_CHARS {
                // Edit distance 1 absorbs typos and most singular/plural
                // morphology ("patata" ↔ "patatas") without a stemmer.
                alternatives.push((
                    Occur::Should,
                    Box::new(FuzzyTermQuery::new(name_term.clone(), 1, true)),
                ));
                alternatives.push((
                    Occur::Should,
                    Box::new(FuzzyTermQuery::new(content_term.clone(), 1, true)),
                ));
            }
            if i == last && token.chars().count() >= PREFIX_MIN_CHARS {
                // Search-as-you-type: the token still being typed matches as
                // a prefix ("pata" → "patatas").
                alternatives.push((
                    Occur::Should,
                    Box::new(FuzzyTermQuery::new_prefix(name_term, 0, true)),
                ));
                alternatives.push((
                    Occur::Should,
                    Box::new(FuzzyTermQuery::new_prefix(content_term, 0, true)),
                ));
            }

            clauses.push((Occur::Must, Box::new(BooleanQuery::new(alternatives))));
        }

        Box::new(BooleanQuery::new(clauses))
    }

    /// Blocking search core — runs on the blocking pool via the port impl.
    fn search_blocking(
        searcher: tantivy::Searcher,
        analyzer: TextAnalyzer,
        fields: IndexFields,
        drive_ids: &[String],
        raw_query: &str,
        limit: usize,
    ) -> Result<Vec<ContentHitDto>, DomainError> {
        let tokens = Self::query_tokens(analyzer, raw_query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        let query = Self::build_query(fields, drive_ids, &tokens);
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit.max(1)).order_by_score())
            .map_err(|e| DomainError::internal_error("ContentIndex", format!("search: {e}")))?;

        // No hits → no documents to highlight. `SnippetGenerator::create`
        // compiles the query against the index (term lookups + weight build);
        // for a query that matched nothing that is pure waste on the search
        // request path, and the per-hit loop below never runs. Return early.
        if top_docs.is_empty() {
            return Ok(Vec::new());
        }

        // Snippets highlight CONTENT matches; an empty fragment means the hit
        // came from the name (or a fuzzy variant) — no snippet then.
        let snippet_generator = SnippetGenerator::create(&searcher, &*query, fields.content)
            .map(|mut g| {
                g.set_max_num_chars(SNIPPET_MAX_CHARS);
                g
            })
            .ok();

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, address) in top_docs {
            let document: TantivyDocument = searcher.doc(address).map_err(|e| {
                DomainError::internal_error("ContentIndex", format!("doc fetch: {e}"))
            })?;
            let Some(file_id) = document
                .get_first(fields.file_id)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
            else {
                continue;
            };

            let snippet = document
                .get_first(fields.preview)
                .and_then(|v| v.as_str())
                .and_then(|preview| {
                    let generator = snippet_generator.as_ref()?;
                    let fragment = generator.snippet(preview).fragment().trim().to_owned();
                    (!fragment.is_empty()).then_some(fragment)
                });

            hits.push(ContentHitDto {
                file_id,
                score,
                snippet,
            });
        }
        Ok(hits)
    }
}

#[async_trait]
impl ContentIndexPort for TantivyContentIndex {
    async fn search_content(
        &self,
        accessible_drive_ids: &[Uuid],
        query: &str,
        limit: usize,
    ) -> Result<Vec<ContentHitDto>, DomainError> {
        // No accessible drives → no hits, no Tantivy work. Matches the
        // anti-enumeration semantics (empty filter set returns empty
        // results without any side channel).
        if accessible_drive_ids.is_empty() {
            return Ok(Vec::new());
        }

        let searcher = self.reader.searcher();
        let analyzer = self.analyzer.clone();
        let fields = self.fields;
        let drive_ids: Vec<String> = accessible_drive_ids.iter().map(|d| d.to_string()).collect();
        let query = query.to_owned();

        tokio::task::spawn_blocking(move || {
            Self::search_blocking(searcher, analyzer, fields, &drive_ids, &query, limit)
        })
        .await
        .map_err(|e| DomainError::internal_error("ContentIndex", format!("join: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(file_id: &str, user_id: &str, name: &str, content: Option<&str>) -> IndexDocRecord {
        IndexDocRecord {
            file_id: file_id.to_owned(),
            user_id: user_id.to_owned(),
            // Tests stamp a placeholder drive_id derived from user_id so the
            // record satisfies the post-D0 schema. Query-side filtering by
            // drive_id is exercised in D0-12's integration tests, not here.
            drive_id: format!("{user_id}-drive"),
            name: name.to_owned(),
            content: content.map(str::to_owned),
            preview: content.map(str::to_owned),
        }
    }

    fn search(index: &TantivyContentIndex, user_id: &str, query: &str) -> Vec<ContentHitDto> {
        // Force a reader reload — OnCommitWithDelay is asynchronous and tests
        // must observe the commit immediately.
        index.reader.reload().unwrap();
        // Test records derive `drive_id = format!("{user_id}-drive")` —
        // the same convention used by `record()`. Filtering by that
        // single drive id exercises the same path the production
        // search uses.
        let drive_ids = vec![format!("{user_id}-drive")];
        TantivyContentIndex::search_blocking(
            index.reader.searcher(),
            index.analyzer.clone(),
            index.fields,
            &drive_ids,
            query,
            32,
        )
        .unwrap()
    }

    #[test]
    fn index_name_and_content_with_fuzzy_prefix_and_user_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let (index, needs_reseed) = TantivyContentIndex::open_or_rebuild(dir.path()).unwrap();
        assert!(needs_reseed, "fresh dir must request a reseed");

        index
            .apply_batch(
                vec![
                    record("f1", "user-a", "patatas-fritas.jpg", None),
                    record(
                        "f2",
                        "user-a",
                        "recetas.pdf",
                        Some("la mejor receta de patatas bravas del mundo"),
                    ),
                    record(
                        "f3",
                        "user-b",
                        "patatas-ajenas.txt",
                        Some("patatas de otro usuario"),
                    ),
                    record("f4", "user-a", "informe.txt", Some("nada relacionado aqui")),
                ],
                Vec::new(),
            )
            .unwrap();

        // Exact term: name hit + content hit for user-a only.
        let hits = search(&index, "user-a", "patatas");
        let ids: Vec<&str> = hits.iter().map(|h| h.file_id.as_str()).collect();
        assert!(ids.contains(&"f1"), "name match expected: {ids:?}");
        assert!(ids.contains(&"f2"), "content match expected: {ids:?}");
        assert!(!ids.contains(&"f3"), "other user's file leaked: {ids:?}");
        assert!(!ids.contains(&"f4"), "non-matching file returned: {ids:?}");

        // The content hit carries a snippet around the matched term.
        let content_hit = hits.iter().find(|h| h.file_id == "f2").unwrap();
        assert!(
            content_hit
                .snippet
                .as_deref()
                .unwrap_or("")
                .contains("patatas"),
            "snippet should surround the match: {:?}",
            content_hit.snippet
        );

        // Fuzzy (distance 1): singular finds plural.
        let ids: Vec<String> = search(&index, "user-a", "patata")
            .into_iter()
            .map(|h| h.file_id)
            .collect();
        assert!(
            ids.contains(&"f2".to_owned()),
            "fuzzy match expected: {ids:?}"
        );

        // Prefix on the last token (search-as-you-type).
        let ids: Vec<String> = search(&index, "user-a", "pata")
            .into_iter()
            .map(|h| h.file_id)
            .collect();
        assert!(
            ids.contains(&"f1".to_owned()),
            "prefix match expected: {ids:?}"
        );
    }

    #[test]
    fn upsert_replaces_and_delete_removes() {
        let dir = tempfile::tempdir().unwrap();
        let (index, _) = TantivyContentIndex::open_or_rebuild(dir.path()).unwrap();

        index
            .apply_batch(
                vec![record(
                    "f1",
                    "u",
                    "old-name.txt",
                    Some("contenido original"),
                )],
                Vec::new(),
            )
            .unwrap();
        index
            .apply_batch(
                vec![record("f1", "u", "renamed.txt", Some("contenido original"))],
                Vec::new(),
            )
            .unwrap();

        assert!(
            search(&index, "u", "old").is_empty(),
            "stale doc survived upsert"
        );
        assert_eq!(search(&index, "u", "renamed").len(), 1);

        index
            .apply_batch(Vec::new(), vec!["f1".to_owned()])
            .unwrap();
        assert!(
            search(&index, "u", "renamed").is_empty(),
            "deleted doc still found"
        );
    }

    #[test]
    fn reopen_preserves_documents_and_version_mismatch_wipes() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (index, _) = TantivyContentIndex::open_or_rebuild(dir.path()).unwrap();
            index
                .apply_batch(vec![record("f1", "u", "persistente.txt", None)], Vec::new())
                .unwrap();
        }

        // Same version: documents survive, no reseed requested.
        {
            let (index, needs_reseed) = TantivyContentIndex::open_or_rebuild(dir.path()).unwrap();
            assert!(!needs_reseed);
            assert_eq!(index.num_docs(), 1);
        }

        // Stale version marker: wipe + reseed.
        std::fs::write(dir.path().join(META_FILE), "0-stale").unwrap();
        let (index, needs_reseed) = TantivyContentIndex::open_or_rebuild(dir.path()).unwrap();
        assert!(needs_reseed);
        assert_eq!(index.num_docs(), 0);
    }
}
