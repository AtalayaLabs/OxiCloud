//! Capture-metadata extraction for images **and** videos.
//!
//! Populates `storage.file_metadata.captured_at` (and, for images, GPS / camera
//! / orientation / dimensions) so the Photos timeline can group by the real
//! capture date instead of the upload time. A DB trigger
//! (`trg_sync_media_sort_date`) keeps `storage.files.media_sort_date =
//! COALESCE(captured_at, created_at)` in sync, so writing `captured_at` is all
//! that is needed — the query/DTO/frontend already consume it.
//!
//! Mirrors [`AudioMetadataService`](super::audio_metadata_service): it is a
//! [`FileLifecycleHook`] wired into the upload pipeline, runs extraction off the
//! Tokio workers (`spawn_blocking`), and is dedup/copy aware.
//!
//! - **Images** — rich EXIF via the existing [`ExifService`] (kamadak-exif:
//!   GPS, camera, orientation, dimensions, capture date), merged with `nom-exif`
//!   which (a) upgrades the capture date to a timezone-correct value
//!   (`OffsetTimeOriginal`) and (b) recovers the date + GPS from files kamadak
//!   rejects entirely with `InvalidFormat("Unexpected next IFD")` — common in
//!   phone/edited photos — where the GPS would otherwise be silently dropped.
//! - **Videos** — container creation time (`mov`/`mp4`/`mkv`) via `nom-exif`,
//!   which seeks the metadata atoms (never loads the whole file) and is
//!   timezone-aware. No EXIF exists for video.

use chrono::{DateTime, FixedOffset, Utc};
use futures::StreamExt;
use sqlx::{FromRow, PgPool};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::application::ports::file_lifecycle::FileLifecycleHook;
use crate::common::errors::DomainError;
use crate::infrastructure::repositories::pg::file_metadata_repository::FileMetadataRepository;
use crate::infrastructure::services::dedup_service::DedupService;
use crate::infrastructure::services::exif_service::{ExifMetadata, ExifService};

#[derive(Debug, FromRow)]
pub struct MediaFileRow {
    pub file_id: Uuid,
    pub blob_hash: String,
    pub mime_type: String,
}

#[derive(Debug, serde::Serialize)]
pub struct MetadataExtractionResult {
    pub total: usize,
    pub processed: usize,
    pub failed: usize,
}

pub struct MediaMetadataService {
    pool: Arc<PgPool>,
    /// CDC-aware blob reader. Same abstraction `thumbnail_service` uses —
    /// hides both the chunk-manifest concatenation and the underlying
    /// `BlobStorageBackend` wrapper stack.
    dedup: Arc<DedupService>,
    /// Tier-1 scratch directory for `stream_blob_to_tempfile`. Pulled
    /// from `AppConfig::temp_dir` (env `OXICLOUD_TEMP_DIR`) at DI time.
    temp_dir: PathBuf,
}

impl MediaMetadataService {
    pub fn new(pool: Arc<PgPool>, dedup: Arc<DedupService>, temp_dir: PathBuf) -> Self {
        Self {
            pool,
            dedup,
            temp_dir,
        }
    }

    pub fn is_image_file(mime_type: &str) -> bool {
        mime_type.starts_with("image/")
    }

    pub fn is_video_file(mime_type: &str) -> bool {
        mime_type.starts_with("video/")
    }

    /// Whether this service extracts capture metadata for the given MIME type.
    pub fn handles(mime_type: &str) -> bool {
        Self::is_image_file(mime_type) || Self::is_video_file(mime_type)
    }

    fn arc(&self) -> Arc<Self> {
        Arc::new(Self {
            pool: self.pool.clone(),
            dedup: self.dedup.clone(),
            temp_dir: self.temp_dir.clone(),
        })
    }

    /// Extract capture metadata from a media blob.
    ///
    /// All parsing is synchronous (kamadak-exif + nom-exif), so this MUST only
    /// be called inside `spawn_blocking`. Returns `None` when there is nothing
    /// worth persisting (no EXIF and no capture date) — the caller then skips
    /// the upsert and the file legitimately falls back to its upload date.
    fn extract_blocking(path: &Path, mime_type: &str) -> Option<ExifMetadata> {
        if !path.exists() {
            warn!("Media file does not exist: {:?}", path);
            return None;
        }

        if Self::is_image_file(mime_type) {
            // ONE disk read: kamadak needs the full buffer anyway, and
            // nom-exif 3.6+ parses from in-RAM bytes zero-copy
            // (`MediaSource::from_memory` over the same allocation). This
            // path used to re-open the file 1-2 more times — nom-exif's
            // `read_exif(path)` plus a `read_track(path)` fallback for
            // date-less images (2-3 opens per image, benches/ROUND12.md §M4:
            // 1.44x warm geomean, 2-3x cold-cache).
            let buf = std::fs::read(path).ok()?;
            // Rich EXIF (GPS / camera / orientation / dimensions + naive date)
            // from the proven kamadak extractor.
            let kamadak = ExifService::extract(&buf);
            // nom-exif complements kamadak: a timezone-correct capture date and,
            // crucially, the date + GPS for files kamadak rejects outright
            // ("Unexpected next IFD"), where `kamadak` is None and the GPS would
            // otherwise be lost. See `merge_image_metadata`.
            let bytes = bytes::Bytes::from(buf);
            merge_image_metadata(kamadak, read_nom_exif_from_bytes(&bytes))
        } else if Self::is_video_file(mime_type) {
            // Videos carry no EXIF — pull the container creation time only.
            // Single open + header sniff; the old shape opened twice (a
            // doomed `read_exif` sniff, then `read_track`).
            read_nom_exif_video(path)
                .captured_at
                .map(|dt| ExifMetadata {
                    captured_at: Some(dt),
                    ..Default::default()
                })
        } else {
            None
        }
    }

    /// Extract metadata for one file and persist it (no-op when nothing useful
    /// could be extracted).
    ///
    /// Streams the blob (CDC-aware — chunks concatenated on the fly for
    /// chunked files; wrapper stack handles encryption + retry + cache)
    /// to a tempfile in the configured tier-1 temp dir, then hands its
    /// `.path()` to the sync extractors (kamadak-exif via
    /// `std::fs::read(path)` for images, `nom-exif` video track reader
    /// for videos — both are path-based). Peak process-heap = one chunk
    /// (~1 MiB) regardless of media size.
    pub async fn extract_and_save(
        &self,
        file_id: &Uuid,
        blob_hash: &str,
        mime_type: &str,
    ) -> Result<(), DomainError> {
        // File-extension hint for the tempfile suffix — helps
        // `nom-exif`'s content sniffer land the right parser branch.
        let suffix = if Self::is_video_file(mime_type) {
            ".mp4"
        } else {
            ".jpg"
        };
        let named = self
            .dedup
            .stream_blob_to_tempfile(blob_hash, &self.temp_dir, suffix)
            .await?;
        let path = named.path().to_path_buf();
        let mime = mime_type.to_string();
        let meta = tokio::task::spawn_blocking(move || Self::extract_blocking(&path, &mime))
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "MediaMetadataService",
                    format!("spawn_blocking join error: {e}"),
                )
            })?;
        // Keep the tempfile alive until after extraction — the
        // spawn_blocking closure only borrowed the raw path.
        drop(named);

        let Some(meta) = meta else {
            return Ok(());
        };

        FileMetadataRepository::new(self.pool.clone())
            .upsert(&file_id.to_string(), &meta)
            .await?;
        info!(
            "Saved capture metadata for file {} (captured_at={:?})",
            file_id, meta.captured_at
        );
        Ok(())
    }

    pub async fn delete_metadata(&self, file_id: &Uuid) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM storage.file_metadata WHERE file_id = $1")
            .bind(file_id)
            .execute(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to delete file metadata: {}", e))
            })?;
        Ok(())
    }

    pub fn spawn_extraction_background(
        service: Arc<Self>,
        file_id: Uuid,
        blob_hash: String,
        mime_type: String,
    ) {
        tokio::spawn(async move {
            tracing::info!("📷 Extracting capture metadata for: {}", file_id);
            if let Err(e) = service
                .extract_and_save(&file_id, &blob_hash, &mime_type)
                .await
            {
                tracing::warn!("Failed to extract capture metadata: {}", e);
            }
        });
    }

    pub fn spawn_extraction_with_delete_background(
        service: Arc<Self>,
        file_id: Uuid,
        blob_hash: String,
        mime_type: String,
    ) {
        tokio::spawn(async move {
            tracing::info!("📷 Updating capture metadata for: {}", file_id);
            let _ = service.delete_metadata(&file_id).await;
            if let Err(e) = service
                .extract_and_save(&file_id, &blob_hash, &mime_type)
                .await
            {
                tracing::warn!("Failed to update capture metadata: {}", e);
            }
        });
    }

    /// Clone metadata from a known source file (explicit copy); falls back to a
    /// blob-hash lookup or fresh extraction if the source is not yet processed.
    pub fn clone_from_source_background(
        service: Arc<Self>,
        new_file_id: Uuid,
        source_file_id: Uuid,
        blob_hash: String,
        mime_type: String,
    ) {
        tokio::spawn(async move {
            let result = sqlx::query(
                r#"
                INSERT INTO storage.file_metadata
                    (file_id, captured_at, latitude, longitude, camera_make,
                     camera_model, orientation, width, height)
                SELECT $1, captured_at, latitude, longitude, camera_make,
                       camera_model, orientation, width, height
                FROM storage.file_metadata
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
                        "Cloned capture metadata from {} to {}",
                        source_file_id, new_file_id
                    );
                }
                Ok(_) => {
                    Self::clone_or_extract_background(service, new_file_id, blob_hash, mime_type);
                }
                Err(e) => {
                    warn!(
                        "Failed to clone capture metadata from {} to {}: {}",
                        source_file_id, new_file_id, e
                    );
                }
            }
        });
    }

    /// Clone metadata from any file sharing the same blob; falls back to fresh
    /// extraction if no processed sibling exists yet.
    pub fn clone_or_extract_background(
        service: Arc<Self>,
        new_file_id: Uuid,
        blob_hash: String,
        mime_type: String,
    ) {
        tokio::spawn(async move {
            let rows_inserted = sqlx::query(
                r#"
                INSERT INTO storage.file_metadata
                    (file_id, captured_at, latitude, longitude, camera_make,
                     camera_model, orientation, width, height)
                SELECT $1, fm.captured_at, fm.latitude, fm.longitude, fm.camera_make,
                       fm.camera_model, fm.orientation, fm.width, fm.height
                FROM storage.file_metadata fm
                JOIN storage.files sf ON sf.id = fm.file_id
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
                    info!("Cloned capture metadata for file {}", new_file_id);
                }
                Ok(_) => {
                    if let Err(e) = service
                        .extract_and_save(&new_file_id, &blob_hash, &mime_type)
                        .await
                    {
                        warn!(
                            "Failed to extract capture metadata for {}: {}",
                            new_file_id, e
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to clone capture metadata for {}: {}",
                        new_file_id, e
                    );
                }
            }
        });
    }

    /// Backfill: re-extract capture metadata for every existing image/video.
    /// Streams rows (O(1) memory); failures are logged and skipped, never abort
    /// the batch. Each upsert fires the DB trigger that recomputes
    /// `media_sort_date`, so the Photos timeline re-buckets afterwards.
    pub async fn reextract_all_image_metadata(
        &self,
    ) -> Result<MetadataExtractionResult, DomainError> {
        let mut stream = sqlx::query_as::<_, MediaFileRow>(
            r#"
            SELECT id as file_id, blob_hash, mime_type
            FROM storage.files
            WHERE mime_type LIKE 'image/%' OR mime_type LIKE 'video/%'
            "#,
        )
        .fetch(&*self.pool);

        let mut total: usize = 0;
        let mut processed: usize = 0;
        let mut failed: usize = 0;

        info!("Starting streaming capture-metadata backfill for image/video files");

        while let Some(row) = stream.next().await {
            total += 1;
            let media = row.map_err(|e| {
                DomainError::database_error(format!("Failed to fetch media file row: {}", e))
            })?;
            match self
                .extract_and_save(&media.file_id, &media.blob_hash, &media.mime_type)
                .await
            {
                Ok(()) => processed += 1,
                Err(e) => {
                    warn!(
                        "Failed to extract capture metadata for file {}: {}",
                        media.file_id, e
                    );
                    failed += 1;
                }
            }
        }

        info!(
            "Capture-metadata backfill complete: {} processed, {} failed out of {} total",
            processed, failed, total
        );

        Ok(MetadataExtractionResult {
            total,
            processed,
            failed,
        })
    }
}

/// Capture date + GPS extracted via `nom-exif`.
///
/// `nom-exif` is far more lenient than kamadak-exif, which rejects many
/// real-world EXIF blocks with `InvalidFormat("Unexpected next IFD")` (phones,
/// photo editors, anything carrying a non-standard trailing IFD) and then
/// yields nothing — silently dropping GPS. Reading the date + GPS here lets
/// [`merge_image_metadata`] recover both even when kamadak fails entirely.
#[derive(Debug, Default)]
struct NomExif {
    captured_at: Option<DateTime<Utc>>,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

/// Read a timezone-correct capture instant + GPS from an image's EXIF
/// (`DateTimeOriginal`/`CreateDate` + GPSInfo), or a video/audio container's
/// creation time (`CreateDate`, no GPS). Missing pieces stay `None`.
///
/// `nom-exif` returns an offset-aware `DateTime<FixedOffset>` when the file
/// carries `OffsetTimeOriginal` (or a tz-aware container time); otherwise the
/// naive wall-clock is interpreted as UTC. Either way it is converted to a true
/// UTC instant. GPS is returned as signed decimal degrees.
fn nom_to_utc(ev: &nom_exif::EntryValue) -> Option<DateTime<Utc>> {
    let edt = ev.as_datetime()?;
    let utc0 = FixedOffset::east_opt(0)?;
    Some(edt.or_offset(utc0).with_timezone(&Utc))
}

fn nom_fill_from_exif(exif: &nom_exif::Exif, out: &mut NomExif) {
    use nom_exif::ExifTag;
    out.captured_at = exif
        .get(ExifTag::DateTimeOriginal)
        .and_then(nom_to_utc)
        .or_else(|| exif.get(ExifTag::CreateDate).and_then(nom_to_utc));
    if let Some(gps) = exif.gps_info() {
        out.latitude = gps.latitude_decimal();
        out.longitude = gps.longitude_decimal();
    }
}

/// Image arm: nom-exif fed from the buffer the kamadak pass already read —
/// `MediaSource::from_memory` shares the `Bytes` refcount, so this re-parses
/// without touching the disk again (the old shape re-opened the file once,
/// plus a second time for date-less images). The track fallback stays (fed
/// from the same bytes): it covers MIME-mislabeled rows whose actual
/// container is a video — the only case where it ever produced a date.
fn read_nom_exif_from_bytes(bytes: &bytes::Bytes) -> NomExif {
    use nom_exif::{MediaParser, MediaSource, TrackInfoTag};

    let mut out = NomExif::default();
    let mut parser = MediaParser::new();

    // Images: EXIF DateTimeOriginal → DateTimeDigitized (CreateDate), plus GPS.
    if let Ok(ms) = MediaSource::from_memory(bytes.clone())
        && let Ok(iter) = parser.parse_exif(ms)
    {
        let exif: nom_exif::Exif = iter.into();
        nom_fill_from_exif(&exif, &mut out);
    }

    // Videos / audio containers (mov/mp4/mkv): track creation time.
    if out.captured_at.is_none()
        && let Ok(ms) = MediaSource::from_memory(bytes.clone())
        && let Ok(track) = parser.parse_track(ms)
        && let Some(dt) = track.get(TrackInfoTag::CreateDate).and_then(nom_to_utc)
    {
        out.captured_at = Some(dt);
    }

    out
}

/// Video arm: ONE open, dispatched on the sniffed container kind. Matches
/// the old `read_exif(path)`-then-`read_track(path)` observable behaviour
/// exactly — a Track container never parsed as EXIF (the old first open was
/// pure waste) and an Image container never parsed as a track, so the
/// two-open sequence always reduced to a single effective parse.
fn read_nom_exif_video(path: &Path) -> NomExif {
    use nom_exif::{MediaKind, MediaParser, MediaSource, TrackInfoTag};

    let mut out = NomExif::default();
    let Ok(file) = std::fs::File::open(path) else {
        return out;
    };
    let Ok(ms) = MediaSource::seekable(file) else {
        return out;
    };
    let mut parser = MediaParser::new();
    match ms.kind() {
        MediaKind::Image => {
            // MIME said video, bytes say image (mislabeled row): same EXIF
            // extraction the old `read_exif(path)` performed.
            if let Ok(iter) = parser.parse_exif(ms) {
                let exif: nom_exif::Exif = iter.into();
                nom_fill_from_exif(&exif, &mut out);
            }
        }
        MediaKind::Track => {
            if let Ok(track) = parser.parse_track(ms)
                && let Some(dt) = track.get(TrackInfoTag::CreateDate).and_then(nom_to_utc)
            {
                out.captured_at = Some(dt);
            }
        }
    }
    out
}

/// Combine kamadak's rich EXIF with nom-exif's date + GPS.
///
/// nom-exif's tz-correct date wins whenever present; its GPS only fills gaps
/// kamadak left (so kamadak's coordinates win when it actually parsed them).
/// When kamadak parsed nothing, a record is still produced if nom-exif found a
/// date or coordinates — otherwise `None`, so the caller skips the upsert and
/// the file legitimately falls back to its upload date.
fn merge_image_metadata(kamadak: Option<ExifMetadata>, nom: NomExif) -> Option<ExifMetadata> {
    match kamadak {
        Some(mut m) => {
            if nom.captured_at.is_some() {
                m.captured_at = nom.captured_at;
            }
            if m.latitude.is_none() {
                m.latitude = nom.latitude;
            }
            if m.longitude.is_none() {
                m.longitude = nom.longitude;
            }
            Some(m)
        }
        None => {
            if nom.captured_at.is_some() || nom.latitude.is_some() || nom.longitude.is_some() {
                Some(ExifMetadata {
                    captured_at: nom.captured_at,
                    latitude: nom.latitude,
                    longitude: nom.longitude,
                    ..Default::default()
                })
            } else {
                None
            }
        }
    }
}

// ─── FileLifecycleHook ───────────────────────────────────────────────────────

impl FileLifecycleHook for MediaMetadataService {
    fn on_file_created(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        is_new_blob: bool,
    ) {
        if !Self::handles(content_type) {
            return;
        }
        let Ok(uuid) = file_id.parse::<Uuid>() else {
            warn!("on_file_created: invalid file_id UUID: {}", file_id);
            return;
        };
        let service = self.arc();
        if is_new_blob {
            Self::spawn_extraction_background(
                service,
                uuid,
                blob_hash.to_string(),
                content_type.to_string(),
            );
        } else {
            Self::clone_or_extract_background(
                service,
                uuid,
                blob_hash.to_string(),
                content_type.to_string(),
            );
        }
    }

    fn on_file_copied(
        &self,
        file_id: &str,
        blob_hash: &str,
        content_type: &str,
        source_file_id: &str,
    ) {
        if !Self::handles(content_type) {
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
        Self::clone_from_source_background(
            self.arc(),
            uuid,
            source_uuid,
            blob_hash.to_string(),
            content_type.to_string(),
        );
    }

    fn on_file_updated(&self, file_id: &str, blob_hash: &str, content_type: &str) {
        if !Self::handles(content_type) {
            return;
        }
        let Ok(uuid) = file_id.parse::<Uuid>() else {
            warn!("on_file_updated: invalid file_id UUID: {}", file_id);
            return;
        };
        Self::spawn_extraction_with_delete_background(
            self.arc(),
            uuid,
            blob_hash.to_string(),
            content_type.to_string(),
        );
    }

    fn on_file_deleted(&self, _file_id: &str) {
        // storage.file_metadata has ON DELETE CASCADE on file_id — DB handles cleanup.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(year: i32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn recovers_date_and_gps_when_kamadak_fails() {
        let nom = NomExif {
            captured_at: Some(dt(2024)),
            latitude: Some(40.5),
            longitude: Some(-74.0),
        };
        let m = merge_image_metadata(None, nom).expect("metadata when nom found GPS/date");
        assert_eq!(m.captured_at, Some(dt(2024)));
        assert_eq!(m.latitude, Some(40.5));
        assert_eq!(m.longitude, Some(-74.0));
    }

    #[test]
    fn fills_only_missing_gps_keeping_kamadak_coords() {
        let kamadak = ExifMetadata {
            latitude: Some(1.0),
            longitude: None,
            ..Default::default()
        };
        let nom = NomExif {
            latitude: Some(40.5),
            longitude: Some(-74.0),
            ..Default::default()
        };
        let m = merge_image_metadata(Some(kamadak), nom).unwrap();
        assert_eq!(m.latitude, Some(1.0)); // kamadak wins where present
        assert_eq!(m.longitude, Some(-74.0)); // gap filled from nom-exif
    }

    #[test]
    fn nom_date_upgrades_kamadak_date_but_keeps_it_when_nom_absent() {
        let with_nom = merge_image_metadata(
            Some(ExifMetadata {
                captured_at: Some(dt(2000)),
                ..Default::default()
            }),
            NomExif {
                captured_at: Some(dt(2024)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(with_nom.captured_at, Some(dt(2024)));

        let without_nom = merge_image_metadata(
            Some(ExifMetadata {
                captured_at: Some(dt(2000)),
                ..Default::default()
            }),
            NomExif::default(),
        )
        .unwrap();
        assert_eq!(without_nom.captured_at, Some(dt(2000)));
    }

    #[test]
    fn no_metadata_at_all_yields_none() {
        assert!(merge_image_metadata(None, NomExif::default()).is_none());
    }
}
