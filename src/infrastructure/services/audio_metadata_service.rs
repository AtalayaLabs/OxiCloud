use futures::StreamExt;
use id3::{Tag, TagLike};
use sqlx::{FromRow, PgPool};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::application::ports::file_lifecycle::FileLifecycleHook;
use crate::common::errors::DomainError;
use crate::infrastructure::services::dedup_service::DedupService;

#[derive(Debug, FromRow)]
pub struct AudioFileRow {
    pub file_id: Uuid,
    pub blob_hash: String,
}

pub struct AudioMetadataService {
    pool: Arc<PgPool>,
    /// CDC-aware blob reader. Same abstraction `thumbnail_service` uses —
    /// hides both the chunk-manifest concatenation and the underlying
    /// `BlobStorageBackend` wrapper stack.
    dedup: Arc<DedupService>,
    /// Tier-1 scratch directory for `stream_blob_to_tempfile`. Pulled
    /// from `AppConfig::temp_dir` (env `OXICLOUD_TEMP_DIR`) at DI time.
    temp_dir: PathBuf,
}

impl AudioMetadataService {
    pub fn new(pool: Arc<PgPool>, dedup: Arc<DedupService>, temp_dir: PathBuf) -> Self {
        Self {
            pool,
            dedup,
            temp_dir,
        }
    }

    pub fn is_audio_file(mime_type: &str) -> bool {
        mime_type.starts_with("audio/")
    }

    pub fn spawn_extraction_background(service: Arc<Self>, file_id: Uuid, blob_hash: String) {
        tokio::spawn(async move {
            tracing::info!("🎵 Extracting audio metadata for: {}", file_id);
            if let Err(e) = service.extract_and_save(&file_id, &blob_hash).await {
                tracing::warn!("Failed to extract audio metadata: {}", e);
            }
        });
    }

    pub fn spawn_extraction_with_delete_background(
        service: Arc<Self>,
        file_id: Uuid,
        blob_hash: String,
    ) {
        tokio::spawn(async move {
            tracing::info!("🎵 Updating audio metadata for: {}", file_id);
            let _ = service.delete_metadata(&file_id).await;
            if let Err(e) = service.extract_and_save(&file_id, &blob_hash).await {
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
            year: tag.year(),
            duration_secs,
        })
    }

    pub async fn extract_and_save(
        &self,
        file_id: &Uuid,
        blob_hash: &str,
    ) -> Result<(), DomainError> {
        info!(
            "AudioMetadataService: extracting file_id={}, blob_hash={}",
            file_id, blob_hash,
        );

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
            "Extracted audio metadata for file {}: title={:?}, artist={:?}, album={:?}, duration={}s",
            file_id, m.title, m.artist, m.album, m.duration_secs
        );

        info!("Saving metadata to database for file_id={}", file_id);

        sqlx::query(
            r#"
            INSERT INTO audio.file_metadata
                (file_id, title, artist, album, album_artist, genre, track_number, disc_number,
                 year, duration_secs, format)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (file_id) DO UPDATE SET
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
        .bind(file_id)
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

    pub async fn delete_metadata(&self, file_id: &Uuid) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM audio.file_metadata WHERE file_id = $1")
            .bind(file_id)
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
        // Stream rows one-by-one instead of fetch_all to keep memory O(1).
        let mut stream = sqlx::query_as::<_, AudioFileRow>(
            r#"
            SELECT id as file_id, blob_hash
            FROM storage.files
            WHERE mime_type LIKE 'audio/%'
            "#,
        )
        .fetch(&*self.pool);

        let mut total: usize = 0;
        let mut processed: usize = 0;
        let mut failed: usize = 0;

        info!("Starting streaming metadata extraction for audio files");

        while let Some(row) = stream.next().await {
            total += 1;
            let audio_file = row.map_err(|e| {
                DomainError::database_error(format!("Failed to fetch audio file row: {}", e))
            })?;
            match self
                .extract_and_save(&audio_file.file_id, &audio_file.blob_hash)
                .await
            {
                Ok(()) => processed += 1,
                Err(e) => {
                    warn!(
                        "Failed to extract metadata for file {}: {}",
                        audio_file.file_id, e
                    );
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

    /// Copy audio metadata from an existing file that shares the same blob.
    ///
    /// Used when `is_new_blob=false` (copy/dedup hit): instead of re-parsing
    /// the blob, clone the existing metadata row for `new_file_id`. Falls back
    /// Clones audio metadata from a known source file, falling back to
    /// [`clone_or_extract_background`] if the source has not been processed yet.
    pub fn clone_from_source_background(
        service: Arc<Self>,
        new_file_id: Uuid,
        source_file_id: Uuid,
        blob_hash: String,
    ) {
        tokio::spawn(async move {
            let result = sqlx::query(
                r#"
                INSERT INTO audio.file_metadata
                    (file_id, title, artist, album, album_artist, genre,
                     track_number, disc_number, year, duration_secs, format)
                SELECT $1, title, artist, album, album_artist, genre,
                    track_number, disc_number, year, duration_secs, format
                FROM audio.file_metadata
                WHERE file_id = $2
                ON CONFLICT (file_id) DO NOTHING
                "#,
            )
            .bind(new_file_id)
            .bind(source_file_id)
            .execute(&*service.pool)
            .await;

            match result {
                Ok(r) if r.rows_affected() > 0 => {
                    info!(
                        "Cloned audio metadata from {} to {}",
                        source_file_id, new_file_id
                    );
                }
                Ok(_) => {
                    // Source not yet processed — fall back to blob-hash lookup or extraction.
                    Self::clone_or_extract_background(service, new_file_id, blob_hash);
                }
                Err(e) => {
                    warn!(
                        "Failed to clone audio metadata from {} to {}: {}",
                        source_file_id, new_file_id, e
                    );
                }
            }
        });
    }

    /// to full extraction if no existing row is found (race: original not yet
    /// processed).
    pub fn clone_or_extract_background(service: Arc<Self>, new_file_id: Uuid, blob_hash: String) {
        tokio::spawn(async move {
            let rows_inserted = sqlx::query(
                r#"
                INSERT INTO audio.file_metadata
                    (file_id, title, artist, album, album_artist, genre,
                     track_number, disc_number, year, duration_secs, format)
                SELECT $1, title, artist, album, album_artist, genre,
                    track_number, disc_number, year, duration_secs, format
                FROM audio.file_metadata am
                JOIN storage.files sf ON sf.id = am.file_id
                WHERE sf.blob_hash = $2
                LIMIT 1
                ON CONFLICT (file_id) DO NOTHING
                "#,
            )
            .bind(new_file_id)
            .bind(&blob_hash)
            .execute(&*service.pool)
            .await;

            match rows_inserted {
                Ok(result) if result.rows_affected() > 0 => {
                    info!("Cloned audio metadata for file {}", new_file_id);
                }
                Ok(_) => {
                    // No existing metadata found — original not yet processed; fall back.
                    if let Err(e) = service.extract_and_save(&new_file_id, &blob_hash).await {
                        warn!(
                            "Failed to extract audio metadata for {}: {}",
                            new_file_id, e
                        );
                    }
                }
                Err(e) => {
                    warn!("Failed to clone audio metadata for {}: {}", new_file_id, e);
                }
            }
        });
    }
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
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        is_new_blob: bool,
    ) {
        if !Self::is_audio_file(content_type) {
            return;
        }
        let Ok(uuid) = file_id.parse::<Uuid>() else {
            warn!("on_file_created: invalid file_id UUID: {}", file_id);
            return;
        };
        let service = Arc::new(Self {
            pool: self.pool.clone(),
            dedup: self.dedup.clone(),
            temp_dir: self.temp_dir.clone(),
        });
        if is_new_blob {
            Self::spawn_extraction_background(service, uuid, blob_hash.to_string());
        } else {
            Self::clone_or_extract_background(service, uuid, blob_hash.to_string());
        }
    }

    fn on_file_copied(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        source_file_id: &str,
    ) {
        if !Self::is_audio_file(content_type) {
            return;
        }
        let Ok(uuid) = file_id.parse::<Uuid>() else {
            warn!("on_file_copied: invalid file_id UUID: {}", file_id);
            return;
        };
        let Ok(source_uuid) = source_file_id.parse::<Uuid>() else {
            warn!(
                "on_file_copied: invalid source_file_id UUID: {}",
                source_file_id
            );
            return;
        };
        let service = Arc::new(Self {
            pool: self.pool.clone(),
            dedup: self.dedup.clone(),
            temp_dir: self.temp_dir.clone(),
        });
        Self::clone_from_source_background(service, uuid, source_uuid, blob_hash.to_string());
    }

    fn on_file_updated(&self, file_id: &str, blob_hash: &str, content_type: &str) {
        if !Self::is_audio_file(content_type) {
            return;
        }
        let Ok(uuid) = file_id.parse::<Uuid>() else {
            warn!("on_file_updated: invalid file_id UUID: {}", file_id);
            return;
        };
        let service = Arc::new(Self {
            pool: self.pool.clone(),
            dedup: self.dedup.clone(),
            temp_dir: self.temp_dir.clone(),
        });
        Self::spawn_extraction_with_delete_background(service, uuid, blob_hash.to_string());
    }

    fn on_file_deleted(&self, _file_id: &str) {
        // audio.file_metadata has ON DELETE CASCADE on file_id — DB handles cleanup.
    }
}
