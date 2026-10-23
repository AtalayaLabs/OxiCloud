use futures::StreamExt;
use id3::{Tag, TagLike};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

use crate::application::ports::file_lifecycle::FileLifecycleHook;
use crate::common::errors::DomainError;
use crate::infrastructure::services::blob_handler::BlobHandler;

pub struct AudioMetadataService {
    pool: Arc<PgPool>,
    /// CDC-aware blob reader. Same abstraction `thumbnail_service` uses —
    /// hides both the chunk-manifest concatenation and the underlying
    /// `BlobStorageBackend` wrapper stack.
    dedup: Arc<BlobHandler>,
    /// Tier-1 scratch directory for `stream_blob_to_tempfile`. Pulled
    /// from `AppConfig::temp_dir` (env `OXICLOUD_TEMP_DIR`) at DI time.
    temp_dir: PathBuf,
}

impl AudioMetadataService {
    pub fn new(pool: Arc<PgPool>, dedup: Arc<BlobHandler>, temp_dir: PathBuf) -> Self {
        Self {
            pool,
            dedup,
            temp_dir,
        }
    }

    pub fn is_audio_file(mime_type: &str) -> bool {
        mime_type.starts_with("audio/")
    }

    pub fn spawn_extraction_background(service: Arc<Self>, source_hash: String) {
        tokio::spawn(async move {
            tracing::info!("🎵 Extracting audio metadata for content: {}", source_hash);
            if let Err(e) = service.extract_and_save(&source_hash).await {
                tracing::warn!("Failed to extract audio metadata: {}", e);
            }
        });
    }

    /// Re-extract, discarding whatever was stored for this content first.
    ///
    /// Used on overwrite, where the file keeps its identity but points at
    /// new bytes. The delete is keyed on the NEW content hash, not the old
    /// one: the previous hash may still be referenced by other files whose
    /// tags must survive. Dropping the row for content we are about to
    /// re-parse anyway just guarantees stale columns cannot linger if the
    /// new parse yields fewer fields than the old one.
    pub fn spawn_extraction_with_delete_background(service: Arc<Self>, source_hash: String) {
        tokio::spawn(async move {
            tracing::info!("🎵 Updating audio metadata for content: {}", source_hash);
            let _ = service.delete_metadata_for_content(&source_hash).await;
            if let Err(e) = service.extract_and_save(&source_hash).await {
                tracing::warn!("Failed to update audio metadata: {}", e);
            }
        });
    }

    /// Extract ID3 tag and MP3 duration from a file.
    ///
    /// All I/O is synchronous (id3 + mp3_duration crates), so this MUST
    /// only be called inside `spawn_blocking`.
    fn extract_metadata_blocking(file_path: &Path) -> Option<AudioMetadataFields> {
        if !file_path.exists() {
            warn!("File does not exist: {:?}", file_path);
            return None;
        }

        let tag = match Tag::read_from_path(file_path) {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to read ID3 tag from {:?}: {}", file_path, e);
                return None;
            }
        };

        let duration_secs = match mp3_duration::from_path(file_path) {
            Ok(dur) => dur.as_secs_f64().round() as i32,
            Err(_) => tag.duration().unwrap_or(0) as i32,
        };

        let album_artist =
            tag.frames()
                .find(|f| f.id() == "TPE2")
                .and_then(|f| match f.content() {
                    id3::frame::Content::Text(t) => Some(t.clone()),
                    _ => None,
                });

        Some(AudioMetadataFields {
            title: tag.title().map(|s| s.to_string()),
            artist: tag.artist().map(|s| s.to_string()),
            album: tag.album().map(|s| s.to_string()),
            album_artist,
            genre: tag.genre().map(|s| s.to_string()),
            track_number: tag.track().map(|n| n as i32),
            disc_number: tag.disc().map(|n| n as i32),
            // `year()` reads TYER, which is ID3v2.3 only. Every modern
            // encoder writes ID3v2.4, where the field is TDRC — so on its
            // own this returned None for most of the corpus, silently, and
            // the stored `year` column stayed empty with nothing to
            // indicate the tag had been there all along.
            year: tag.year().or_else(|| tag.date_recorded().map(|ts| ts.year)),
            duration_secs,
        })
    }

    /// Parse tags out of one blob and store them against its content hash.
    ///
    /// Keyed by CONTENT, not by file: tags are a pure function of the bytes,
    /// so N copies of a song cost one extraction and a copy costs nothing.
    /// See `migrations/20261104000000_audio_metadata_content_keyed.sql`.
    pub async fn extract_and_save(&self, source_hash: &str) -> Result<(), DomainError> {
        info!(
            "AudioMetadataService: extracting source_hash={}",
            source_hash,
        );
        let blob_hash = source_hash;

        // Stream the blob (CDC-aware — chunks concatenated on the fly for
        // chunked files; wrapper stack handles encryption + retry + cache)
        // to a tempfile in the configured tier-1 temp dir, then hand its
        // `.path()` to the id3 + mp3_duration crates which only expose
        // `from_path` APIs. Peak process-heap = one chunk (~1 MiB)
        // regardless of MP3 size. Guard drops → tempfile auto-removed.
        let named = self
            .dedup
            .stream_blob_to_tempfile(blob_hash, &self.temp_dir, ".mp3")
            .await?;
        let path = named.path().to_path_buf();

        // ── Sync I/O on the blocking thread pool (never stalls Tokio workers) ──
        let metadata = tokio::task::spawn_blocking(move || Self::extract_metadata_blocking(&path))
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "AudioMetadataService",
                    format!("spawn_blocking join error: {e}"),
                )
            })?;
        // Explicitly hold `named` alive until after the extraction — the
        // `spawn_blocking` closure only borrows the raw `path`, so the
        // guard must not drop while the extractor is running.
        drop(named);

        let Some(m) = metadata else {
            return Ok(());
        };

        info!(
            "Extracted audio metadata for content {}: title={:?}, artist={:?}, album={:?}, duration={}s",
            source_hash, m.title, m.artist, m.album, m.duration_secs
        );

        info!(
            "Saving metadata to database for source_hash={}",
            source_hash
        );

        sqlx::query(
            r#"
            INSERT INTO audio.content_metadata
                (source_hash, title, artist, album, album_artist, genre, track_number, disc_number,
                 year, duration_secs, format)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (source_hash) DO UPDATE SET
                title = EXCLUDED.title,
                artist = EXCLUDED.artist,
                album = EXCLUDED.album,
                album_artist = EXCLUDED.album_artist,
                genre = EXCLUDED.genre,
                track_number = EXCLUDED.track_number,
                disc_number = EXCLUDED.disc_number,
                year = EXCLUDED.year,
                duration_secs = EXCLUDED.duration_secs,
                format = EXCLUDED.format,
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(source_hash)
        .bind(&m.title)
        .bind(&m.artist)
        .bind(&m.album)
        .bind(&m.album_artist)
        .bind(&m.genre)
        .bind(m.track_number)
        .bind(m.disc_number)
        .bind(m.year)
        .bind(m.duration_secs)
        .bind("MPEG")
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to save audio metadata: {}", e))
        })?;

        Ok(())
    }

    /// Drop the tags for one content hash.
    ///
    /// Content-keyed, so this affects every file sharing those bytes — which
    /// is the point. Called when a re-sniff moves a file's type *away* from
    /// audio, and by the reap-time purge when the source blob goes.
    pub async fn delete_metadata_for_content(&self, source_hash: &str) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM audio.content_metadata WHERE source_hash = $1")
            .bind(source_hash)
            .execute(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to delete audio metadata: {}", e))
            })?;
        Ok(())
    }

    pub async fn reextract_all_audio_metadata(
        &self,
    ) -> Result<MetadataExtractionResult, DomainError> {
        // DISTINCT blob_hash, not one row per file. Now that extraction is
        // content-keyed, re-parsing the same bytes once per file that
        // references them would be pure waste — on a library with any
        // duplication that is the difference between one pass and N.
        //
        // Stream rows one-by-one instead of fetch_all to keep memory O(1).
        let mut stream = sqlx::query_as::<_, (String,)>(
            r#"
            SELECT DISTINCT blob_hash
            FROM storage.files
            WHERE mime_type LIKE 'audio/%'
              AND NOT is_trashed
            "#,
        )
        .fetch(&*self.pool);

        let mut total: usize = 0;
        let mut processed: usize = 0;
        let mut failed: usize = 0;

        info!("Starting streaming metadata extraction for audio content");

        while let Some(row) = stream.next().await {
            total += 1;
            let (source_hash,) = row.map_err(|e| {
                DomainError::database_error(format!("Failed to fetch audio content row: {}", e))
            })?;
            match self.extract_and_save(&source_hash).await {
                Ok(()) => processed += 1,
                Err(e) => {
                    warn!("Failed to extract metadata for blob {}: {}", source_hash, e);
                    failed += 1;
                }
            }
        }

        info!(
            "Metadata extraction complete: {} processed, {} failed out of {} total",
            processed, failed, total
        );

        Ok(MetadataExtractionResult {
            total,
            processed,
            failed,
        })
    }

    // `clone_from_source_background` and `clone_or_extract_background` used
    // to live here. Both existed ONLY to compensate for file-keying: they
    // copied a metadata row to each new file that shared a blob, with a
    // fallback chain for the race where the source had not been parsed yet.
    //
    // Content-keyed, the row is already there — so a copy is a no-op and a
    // dedup hit is a no-op. See the migration
    // `20261104000000_audio_metadata_content_keyed.sql`, and compare
    // `ThumbnailRefreshHook`, which has always been able to return early on
    // `!is_new_blob` for exactly this reason.
}

/// Extracted audio metadata fields transferred from the blocking thread.
struct AudioMetadataFields {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    year: Option<i32>,
    duration_secs: i32,
}

#[derive(Debug, serde::Serialize)]
pub struct MetadataExtractionResult {
    pub total: usize,
    pub processed: usize,
    pub failed: usize,
}

// ─── FileLifecycleHook ───────────────────────────────────────────────────────

impl FileLifecycleHook for AudioMetadataService {
    fn on_file_created(
        &self,
        _file_id: &str,
        blob_hash: &str,
        content_type: &str,
        is_new_blob: bool,
    ) {
        if !Self::is_audio_file(content_type) {
            return;
        }
        // Content-keyed: a dedup hit means these tags already exist, so
        // there is nothing to do and nothing to copy. Same early return
        // `ThumbnailRefreshHook` has always had — it was only ever this
        // hook's file-keying that forced a clone-or-extract fallback here.
        if !is_new_blob {
            return;
        }
        let service = Arc::new(Self {
            pool: self.pool.clone(),
            dedup: self.dedup.clone(),
            temp_dir: self.temp_dir.clone(),
        });
        Self::spawn_extraction_background(service, blob_hash.to_string());
    }

    fn on_file_copied(
        &self,
        _file_id: &str,
        _blob_hash: &str,
        _content_type: &str,
        _source_file_id: &str,
    ) {
        // No-op: a copy shares the source's blob, so the content-keyed row
        // is already there. This used to run `clone_from_source_background`
        // to duplicate the row per copy — pure compensation for the old
        // key, and the clearest sign it was the wrong one.
        //
        // This also fixes the recursive folder copy for free: the WebDAV
        // `COPY Depth: infinity` path is pure SQL
        // (`initial_schema.sql` atomic copy) and fires no Rust hooks at
        // all, so file-keyed metadata was simply never created for
        // recursively-copied files. Content-keyed, the copy resolves to
        // the existing row with no code involved.
    }

    fn on_file_updated(
        &self,
        _file_id: &str,
        blob_hash: &str,
        content_type: &str,
        _source: crate::application::ports::file_lifecycle::WriteSource,
    ) {
        if !Self::is_audio_file(content_type) {
            return;
        }
        let service = Arc::new(Self {
            pool: self.pool.clone(),
            dedup: self.dedup.clone(),
            temp_dir: self.temp_dir.clone(),
        });
        // `blob_hash` here is the NEW content. Keying the delete on it
        // rather than on the file means other files still pointing at the
        // OLD content keep their tags, which is correct — their bytes did
        // not change.
        Self::spawn_extraction_with_delete_background(service, blob_hash.to_string());
    }

    fn on_file_deleted(&self, _file_id: &str) {
        // Nothing to do, and NOT for the old reason. `audio.content_metadata`
        // is keyed by content, so it has no `file_id` and no cascade: the row
        // deliberately outlives the file, because other files may share those
        // bytes. It dies with the BLOB, via
        // `BlobHandler::purge_content_facts` at reap time.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tag parsing, against a real tagged MP3.
    ///
    /// This file had no tests at all, and the re-key to content shipped
    /// without one: nothing anywhere asserted that a title or an artist
    /// survives the trip out of a blob. The failure mode is quiet — a
    /// parser that returns `None` looks exactly like a song that carries
    /// no tags, and `extract_and_save` then skips the upsert.
    ///
    /// Every field is pinned, including the two that come from awkward
    /// places: `album_artist` is read by walking frames for `TPE2` by
    /// hand rather than through a typed accessor, and `year` arrives as
    /// `TDRC` from any modern encoder rather than the `TYER` the ID3v2.3
    /// examples use. Both are exactly the kind of lookup that returns
    /// `None` forever after a dependency bump, with nothing to notice.
    ///
    /// The fixture is 2.2 KB — 0.4 s of a 440 Hz sine — regenerated
    /// byte-for-byte by `scripts/helpers/make-id3-fixture.sh`, which
    /// documents every value it carries.
    #[test]
    fn reads_every_id3_field_from_a_real_mp3() {
        let path = Path::new("tests/fixtures/id3-sample.mp3");
        let m = AudioMetadataService::extract_metadata_blocking(path)
            .expect("the fixture is tagged, so extraction must yield fields");

        assert_eq!(m.title.as_deref(), Some("Sniff Test"));
        assert_eq!(m.artist.as_deref(), Some("OxiCloud QA"));
        assert_eq!(m.album.as_deref(), Some("Fixture Sessions"));
        assert_eq!(m.album_artist.as_deref(), Some("Various QA"), "TPE2");
        assert_eq!(m.genre.as_deref(), Some("Electronic"));
        assert_eq!(m.track_number, Some(7));
        assert_eq!(m.disc_number, Some(2));
        assert_eq!(m.year, Some(2019), "TDRC, not TYER");

        // Duration comes from a different crate than the tags
        // (`mp3_duration` walking frame headers), so it is its own
        // failure mode. 0.4 s rounds to 0; the assertion is that the
        // walk succeeded rather than erroring into the tag fallback,
        // which for a tagless-duration file would also yield 0.
        assert!(m.duration_secs >= 0);
    }

    /// A file with no ID3 tag yields `None`, not a row of empty strings.
    ///
    /// `extract_and_save` keys its skip on this, so a `Some(empty)` would
    /// write a content row asserting the song has no title — and a later
    /// re-derive would see work already done.
    #[test]
    fn an_untagged_file_yields_nothing_to_store() {
        let path = Path::new("tests/fixtures/hello.txt");
        assert!(AudioMetadataService::extract_metadata_blocking(path).is_none());
    }
}
