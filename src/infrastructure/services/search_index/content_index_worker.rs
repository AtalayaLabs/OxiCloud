//! Background drainer for `storage.search_index_dirty` — the asynchronous
//! half of content indexing (see migration `20260701000000_content_search_index`).
//!
//! The statement triggers on `storage.files` only append "index me" requests
//! to the queue, taking zero locks on user write paths. This worker turns the
//! requests into Tantivy mutations: every `interval_ms` it drains a batch,
//! re-reads the CURRENT file state (the queue row is a hint, not a payload),
//! extracts text once per unique blob, applies one batched Tantivy commit and
//! only then deletes the processed queue rows.
//!
//! Correctness invariants:
//!   * At-least-once: queue rows are deleted AFTER the Tantivy commit. A
//!     crash in between re-processes the batch — harmless, upserts are
//!     idempotent (delete_term + add_document keyed by file_id).
//!   * Deletes are selected by exact id (`id = ANY(...)`), never by range —
//!     a transaction that began before our SELECT can commit a smaller id
//!     afterwards, and a range delete would discard it unprocessed.
//!   * Latest-op-wins per file within a batch; the authoritative state is
//!     re-fetched from `storage.files` at drain time anyway (a file trashed
//!     after its 'upsert' was queued simply turns into a delete).
//!   * Extraction is keyed by blob hash (content-addressed): N files sharing
//!     a blob cost ONE extraction, renames/moves cost zero re-extraction.
//!     Terminal outcomes (ok/empty/failed/too_large) are cached in
//!     `storage.blob_extracted_text`; transient blob-read errors store
//!     nothing so the next event retries.
//!
//! Resource budget: single worker, one extraction at a time inside
//! `spawn_blocking`, single-threaded Tantivy writer — the pipeline trickles
//! along on the maintenance pool and never competes with request latency.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sqlx::PgPool;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::infrastructure::services::blob_handler::BlobHandler;
use crate::infrastructure::services::search_index::tantivy_content_index::{
    EXTRACTOR_VERSION, IndexDocRecord, TantivyContentIndex,
};
use crate::infrastructure::services::search_index::text_extractor::{self, ExtractedText};

/// Queue rows drained per batch. Each row may cost a blob read + extraction,
/// so this is far smaller than the tree-etag drain batch.
const DRAIN_BATCH: i64 = 256;

/// Max batches per tick so a huge backlog (initial reseed) cannot monopolise
/// the maintenance connection within one tick.
const MAX_BATCHES_PER_TICK: u32 = 4;

/// Stored preview head per document (snippet source).
const PREVIEW_BYTES: usize = 16 * 1024;

/// Ticks between `blob_extracted_text` orphan sweeps (~1 h at the default
/// 1.5 s interval).
const ORPHAN_SWEEP_TICKS: u64 = 2400;

/// Backoff before the supervisor restarts the drain loop after an abnormal
/// exit (a panic). Long enough that a tight crash-loop can't busy-spin, short
/// enough that indexing resumes promptly.
const WORKER_RESTART_BACKOFF_SECS: u64 = 5;

pub struct ContentIndexWorker {
    maintenance_pool: Arc<PgPool>,
    dedup: Arc<BlobHandler>,
    index: Arc<TantivyContentIndex>,
    interval_ms: u64,
    max_extract_file_bytes: u64,
    max_text_bytes: usize,
}

impl ContentIndexWorker {
    pub fn new(
        maintenance_pool: Arc<PgPool>,
        dedup: Arc<BlobHandler>,
        index: Arc<TantivyContentIndex>,
        interval_ms: u64,
        max_extract_file_bytes: u64,
        max_text_bytes: usize,
    ) -> Self {
        Self {
            maintenance_pool,
            dedup,
            index,
            // Floor the cadence so a misconfiguration can't busy-loop the
            // maintenance pool.
            interval_ms: interval_ms.max(200),
            max_extract_file_bytes,
            max_text_bytes,
        }
    }

    /// Spawn the indexing loop, supervised. The drain loop logs and survives
    /// every *operational* error (a failed drain just retries next tick), but a
    /// panic in the loop body would otherwise kill the task and silently freeze
    /// the index while the dirty queue grows unbounded. The supervisor restarts
    /// the loop after a panic (with backoff) so indexing self-heals. The first
    /// drain runs immediately to absorb rows left over from a previous run or
    /// the migration backfill.
    #[instrument(skip(self))]
    pub fn start(self, needs_reseed: bool) {
        info!(
            "Starting content-index worker (every {}ms, batch {}, reseed: {})",
            self.interval_ms, DRAIN_BATCH, needs_reseed
        );
        let worker = Arc::new(self);
        tokio::spawn(async move {
            // Reseed/version cleanup runs once, not on every restart.
            if let Err(e) = worker.prepare(needs_reseed).await {
                error!("Content-index prepare failed (continuing with queue as-is): {e}");
            }

            // run_loop() never returns under normal operation, so any exit is
            // abnormal: a panic surfaces as a JoinError; a plain return would
            // be a logic bug. Either way, log loudly and restart.
            loop {
                let w = worker.clone();
                match tokio::spawn(async move { w.run_loop().await }).await {
                    Ok(()) => error!(
                        "Content-index drain loop returned unexpectedly; \
                         restarting in {WORKER_RESTART_BACKOFF_SECS}s"
                    ),
                    Err(e) if e.is_panic() => error!(
                        "Content-index drain loop panicked ({e}); \
                         restarting in {WORKER_RESTART_BACKOFF_SECS}s"
                    ),
                    Err(_) => return, // task cancelled — runtime shutting down
                }
                tokio::time::sleep(std::time::Duration::from_secs(WORKER_RESTART_BACKOFF_SECS))
                    .await;
            }
        });
    }

    /// The perpetual drain loop. Extracted from [`start`](Self::start) so the
    /// supervisor can run it in a child task and restart it after a panic.
    async fn run_loop(&self) {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(self.interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ticks: u64 = 0;
        loop {
            ticker.tick().await;
            for _ in 0..MAX_BATCHES_PER_TICK {
                match self.drain_once().await {
                    Ok(0) => break,
                    Ok(drained) => {
                        debug!("Content-index drain: processed {drained} queue row(s)");
                        if drained < DRAIN_BATCH as usize {
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Content-index drain failed (queue preserved, will retry): {e}");
                        break;
                    }
                }
            }

            ticks += 1;
            if ticks.is_multiple_of(ORPHAN_SWEEP_TICKS) {
                self.sweep_orphaned_text().await;
            }
        }
    }

    /// Spawn the discard-only janitor used when content search is DISABLED:
    /// the triggers are always installed, so something must keep the queue
    /// from growing unboundedly. Re-enabling the feature reseeds from scratch
    /// (index version marker), so discarding here loses nothing.
    pub fn start_drain_only_janitor(maintenance_pool: Arc<PgPool>) {
        info!("Content search disabled — starting queue janitor (discard-only)");
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = sqlx::query("DELETE FROM storage.search_index_dirty")
                    .execute(maintenance_pool.as_ref())
                    .await
                {
                    error!("Content-search queue janitor failed: {e}");
                }
            }
        });
    }

    /// Startup housekeeping: drop extraction rows from other extractor
    /// versions (the reseed re-extracts them) and, when the on-disk index was
    /// wiped, re-enqueue every live file.
    async fn prepare(&self, needs_reseed: bool) -> Result<(), sqlx::Error> {
        let dropped = sqlx::query("DELETE FROM storage.blob_extracted_text WHERE extractor <> $1")
            .bind(EXTRACTOR_VERSION)
            .execute(self.maintenance_pool.as_ref())
            .await?
            .rows_affected();
        if dropped > 0 {
            info!("Dropped {dropped} extraction row(s) from a previous extractor version");
        }

        if needs_reseed {
            let queued = sqlx::query(
                "INSERT INTO storage.search_index_dirty (file_id, op)
                 SELECT id, 'upsert' FROM storage.files WHERE NOT is_trashed",
            )
            .execute(self.maintenance_pool.as_ref())
            .await?
            .rows_affected();
            info!("Content-index reseed: queued {queued} file(s) for indexing");
        }
        Ok(())
    }

    /// Drain and process one queue batch. Returns the number of queue rows
    /// consumed (0 = queue empty).
    async fn drain_once(&self) -> Result<usize, sqlx::Error> {
        let rows: Vec<(i64, Uuid, String)> = sqlx::query_as(
            "SELECT id, file_id, op FROM storage.search_index_dirty ORDER BY id LIMIT $1",
        )
        .bind(DRAIN_BATCH)
        .fetch_all(self.maintenance_pool.as_ref())
        .await?;

        if rows.is_empty() {
            return Ok(0);
        }
        let drained_ids: Vec<i64> = rows.iter().map(|r| r.0).collect();

        // Latest op per file wins (rows are id-ordered).
        let mut latest_op: HashMap<Uuid, bool> = HashMap::with_capacity(rows.len());
        for (_, file_id, op) in &rows {
            latest_op.insert(*file_id, op == "upsert");
        }
        let upsert_candidates: Vec<Uuid> = latest_op
            .iter()
            .filter_map(|(id, &upsert)| upsert.then_some(*id))
            .collect();
        let mut deletes: HashSet<Uuid> = latest_op
            .iter()
            .filter_map(|(id, &upsert)| (!upsert).then_some(*id))
            .collect();

        // Authoritative state re-read: a queued 'upsert' whose row vanished
        // or got trashed in the meantime becomes a delete.
        //
        // Post-D7: `fi.user_id` is dropped — no longer projected. The
        // Tantivy `user_id` field survives as defence-in-depth but now
        // always indexes `""`. Every query is Must-scoped by `drive_id`.
        // (file_id, drive_id, name, blob_hash, mime, size).
        type FileIndexRow = (Uuid, String, String, String, String, i64);
        let files: Vec<FileIndexRow> = if upsert_candidates.is_empty() {
            Vec::new()
        } else {
            sqlx::query_as(
                "SELECT fi.id, fi.drive_id::text, fi.name,
                        fi.blob_hash, fi.mime_type, fi.size
                   FROM storage.files fi
                  WHERE fi.id = ANY($1) AND NOT fi.is_trashed",
            )
            .bind(&upsert_candidates)
            .fetch_all(self.maintenance_pool.as_ref())
            .await?
        };
        let found: HashSet<Uuid> = files.iter().map(|f| f.0).collect();
        deletes.extend(upsert_candidates.iter().filter(|id| !found.contains(id)));

        // `supports` lowercases the MIME (and, on a generic MIME, the extension)
        // — 1–2 allocations per call. Classify each file ONCE here and thread the
        // flag through both the wanted-hashes filter and the per-file records
        // loop below, where it used to be re-derived a second time per file.
        let supported: Vec<bool> = files
            .iter()
            .map(|(_, _, name, _, mime, _)| text_extractor::supports(name, mime))
            .collect();

        // Per-blob text: batch-read the extraction cache, extract misses.
        let wanted_hashes: Vec<String> = files
            .iter()
            .zip(&supported)
            .filter(|&(f, sup)| *sup && f.5 as u64 <= self.max_extract_file_bytes)
            .map(|(f, _)| f.3.clone())
            .collect();
        let mut text_by_hash: HashMap<String, Option<String>> = HashMap::new();
        if !wanted_hashes.is_empty() {
            let cached: Vec<(String, Option<String>, String)> = sqlx::query_as(
                "SELECT blob_hash, text, status FROM storage.blob_extracted_text
                  WHERE blob_hash = ANY($1)",
            )
            .bind(&wanted_hashes)
            .fetch_all(self.maintenance_pool.as_ref())
            .await?;
            for (hash, text, status) in cached {
                text_by_hash.insert(hash, (status == "ok").then_some(text.unwrap_or_default()));
            }
        }

        let mut records = Vec::with_capacity(files.len());
        for ((file_id, drive_id, name, blob_hash, mime, size), supported) in
            files.into_iter().zip(supported)
        {
            let content = if !supported {
                None
            } else if let Some(cached) = text_by_hash.get(&blob_hash) {
                cached.clone()
            } else {
                let extracted = self
                    .extract_and_cache(&blob_hash, &name, &mime, size as u64)
                    .await;
                text_by_hash.insert(blob_hash.clone(), extracted.clone());
                extracted
            };

            let preview = content
                .as_deref()
                .map(|t| truncate_on_char(t, PREVIEW_BYTES));
            records.push(IndexDocRecord {
                file_id: file_id.to_string(),
                user_id: String::new(),
                drive_id,
                name,
                content,
                preview,
            });
        }

        // One batched Tantivy commit, off the async runtime.
        let index = self.index.clone();
        let delete_ids: Vec<String> = deletes.iter().map(Uuid::to_string).collect();
        let applied: Result<(), String> =
            match tokio::task::spawn_blocking(move || index.apply_batch(records, delete_ids)).await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(format!("join: {e}")),
            };
        if let Err(e) = applied {
            // Queue rows survive — the next tick retries the whole batch.
            error!("Tantivy batch apply failed (will retry): {e}");
            return Ok(0);
        }

        // Only now is the work durable in the index — drop the queue rows.
        sqlx::query("DELETE FROM storage.search_index_dirty WHERE id = ANY($1)")
            .bind(&drained_ids)
            .execute(self.maintenance_pool.as_ref())
            .await?;

        Ok(drained_ids.len())
    }

    /// Read the blob (already size-capped), run the extractor on the blocking
    /// pool, and persist the terminal outcome keyed by blob hash. Transient
    /// read failures persist nothing — the next queue event retries.
    async fn extract_and_cache(
        &self,
        blob_hash: &str,
        name: &str,
        mime: &str,
        size: u64,
    ) -> Option<String> {
        if size > self.max_extract_file_bytes {
            self.store_extraction(blob_hash, None, "too_large").await;
            return None;
        }

        let bytes = match self.dedup.read_blob_bytes(blob_hash).await {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(
                    "Content-index blob read failed for {blob_hash} (will retry on next event): {e}"
                );
                return None;
            }
        };

        let (name, mime, max_text) = (name.to_owned(), mime.to_owned(), self.max_text_bytes);
        let outcome = tokio::task::spawn_blocking(move || {
            text_extractor::extract(&name, &mime, &bytes, max_text)
        })
        .await
        .unwrap_or_else(|e| ExtractedText::Failed(format!("join: {e}")));

        match outcome {
            ExtractedText::Text(text) => {
                self.store_extraction(blob_hash, Some(&text), "ok").await;
                Some(text)
            }
            ExtractedText::Empty => {
                self.store_extraction(blob_hash, None, "empty").await;
                None
            }
            ExtractedText::Failed(reason) => {
                warn!("Text extraction failed for blob {blob_hash}: {reason}");
                self.store_extraction(blob_hash, None, "failed").await;
                None
            }
            ExtractedText::Unsupported => None,
        }
    }

    async fn store_extraction(&self, blob_hash: &str, text: Option<&str>, status: &str) {
        if let Err(e) = sqlx::query(
            "INSERT INTO storage.blob_extracted_text (blob_hash, text, status, extractor)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (blob_hash) DO NOTHING",
        )
        .bind(blob_hash)
        .bind(text)
        .bind(status)
        .bind(EXTRACTOR_VERSION)
        .execute(self.maintenance_pool.as_ref())
        .await
        {
            warn!("Failed to cache extraction for blob {blob_hash}: {e}");
        }
    }

    /// Drop extraction rows whose blob no longer backs any live file. Uses
    /// the `idx_files_blob_hash` index; runs hourly on the maintenance pool.
    async fn sweep_orphaned_text(&self) {
        match sqlx::query(
            "DELETE FROM storage.blob_extracted_text bet
              WHERE NOT EXISTS (SELECT 1 FROM storage.files f WHERE f.blob_hash = bet.blob_hash)",
        )
        .execute(self.maintenance_pool.as_ref())
        .await
        {
            Ok(result) if result.rows_affected() > 0 => {
                debug!(
                    "Content-index sweep: dropped {} orphaned extraction row(s)",
                    result.rows_affected()
                );
            }
            Ok(_) => {}
            Err(e) => error!("Content-index orphan sweep failed: {e}"),
        }
    }
}

/// Truncate on a char boundary at most `max_bytes` into `s`.
fn truncate_on_char(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::truncate_on_char;

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate_on_char("patatas", 4), "pata");
        // 'ñ' is 2 bytes — a cut landing inside it must back off ("ñoño" is
        // ñ:0-1 o:2 ñ:3-4 o:5, so a 4-byte cut falls mid-ñ and yields "ño").
        assert_eq!(truncate_on_char("ñoño", 4), "ño");
        assert_eq!(truncate_on_char("ok", 10), "ok");
    }
}
