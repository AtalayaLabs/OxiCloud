//! Capture-metadata extraction for images **and** videos.
//!
//! Populates `storage.content_metadata.created_at` (and, for images, GPS /
//! camera / orientation / dimensions) so the Photos timeline can group by the
//! real capture date instead of the upload time. DB triggers keep
//! `storage.files.media_sort_date` in sync — fanning out over every file
//! sharing the content on write, and seeding a new file from the existing
//! content row on INSERT — so writing the date is all that is needed; the
//! query/DTO/frontend already consume it.
//!
//! **Keyed by CONTENT, not by file.** These facts are a pure function of the
//! bytes, so N copies of a photo cost one extraction and a copy costs
//! nothing. The row holds no `ref_count` and no FK: it deliberately outlives
//! the file and dies with the BLOB, via `BlobHandler::purge_content_facts`.
//! See `migrations/20261105000000_exif_metadata_content_keyed.sql`.
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
use crate::infrastructure::services::blob_handler::BlobHandler;
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
    dedup: Arc<BlobHandler>,
    /// Tier-1 scratch directory for `stream_blob_to_tempfile`. Pulled
    /// from `AppConfig::temp_dir` (env `OXICLOUD_TEMP_DIR`) at DI time.
    temp_dir: PathBuf,
}

impl MediaMetadataService {
    pub fn new(pool: Arc<PgPool>, dedup: Arc<BlobHandler>, temp_dir: PathBuf) -> Self {
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
    /// Parse capture facts out of one blob and store them against its
    /// content hash.
    ///
    /// Keyed by CONTENT, not by file: EXIF is a pure function of the bytes,
    /// so N copies of a photo cost one extraction and a copy costs nothing.
    /// See `migrations/20261105000000_exif_metadata_content_keyed.sql`.
    pub async fn extract_and_save(
        &self,
        source_hash: &str,
        mime_type: &str,
    ) -> Result<(), DomainError> {
        let blob_hash = source_hash;
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
            .upsert(source_hash, &meta)
            .await?;
        info!(
            "Saved capture metadata for content {} (captured_at={:?})",
            source_hash, meta.captured_at
        );
        Ok(())
    }

    /// Drop the capture facts for one content hash.
    ///
    /// Content-keyed, so this affects every file sharing those bytes —
    /// which is the point. Called by the reap-time purge, and when a
    /// re-sniff moves a file's type away from media.
    pub async fn delete_metadata_for_content(&self, source_hash: &str) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM storage.content_metadata WHERE source_hash = $1")
            .bind(source_hash)
            .execute(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to delete content metadata: {}", e))
            })?;
        Ok(())
    }

    pub fn spawn_extraction_background(service: Arc<Self>, source_hash: String, mime_type: String) {
        tokio::spawn(async move {
            tracing::info!(
                "📷 Extracting capture metadata for content: {}",
                source_hash
            );
            if let Err(e) = service.extract_and_save(&source_hash, &mime_type).await {
                tracing::warn!("Failed to extract capture metadata: {}", e);
            }
        });
    }

    /// Re-extract, discarding whatever was stored for this content first.
    ///
    /// The delete is keyed on the NEW content hash, not the old one: other
    /// files may still point at the previous bytes and their facts must
    /// survive. Clearing the row we are about to rewrite just stops stale
    /// columns lingering when the new parse yields fewer fields.
    pub fn spawn_extraction_with_delete_background(
        service: Arc<Self>,
        source_hash: String,
        mime_type: String,
    ) {
        tokio::spawn(async move {
            tracing::info!("📷 Updating capture metadata for content: {}", source_hash);
            let _ = service.delete_metadata_for_content(&source_hash).await;
            if let Err(e) = service.extract_and_save(&source_hash, &mime_type).await {
                tracing::warn!("Failed to update capture metadata: {}", e);
            }
        });
    }

    // `clone_from_source_background` and `clone_or_extract_background` used
    // to live here, mirroring the pair deleted from AudioMetadataService.
    // Both existed ONLY to compensate for file-keying: they copied a
    // metadata row to each new file sharing a blob, with a fallback chain
    // for the race where the source had not been parsed yet.
    //
    // Content-keyed the row is already there, so a copy and a dedup hit are
    // both no-ops. See
    // `migrations/20261105000000_exif_metadata_content_keyed.sql`.

    /// Backfill: re-extract capture metadata for every existing image/video.
    /// Streams rows (O(1) memory); failures are logged and skipped, never abort
    /// the batch. Each upsert fires the DB trigger that recomputes
    /// `media_sort_date`, so the Photos timeline re-buckets afterwards.
    pub async fn reextract_all_image_metadata(
        &self,
    ) -> Result<MetadataExtractionResult, DomainError> {
        // DISTINCT content, not one row per file. Extraction is content-keyed
        // now, so re-parsing the same bytes once per referencing file would be
        // pure waste — on a library with any duplication that is the
        // difference between one pass and N.
        //
        // `min(mime_type)` because one blob can be referenced by files with
        // differing stored types; any of them picks the same parser branch,
        // since the grouping below only distinguishes image from video.
        let mut stream = sqlx::query_as::<_, (String, String)>(
            r#"
            SELECT blob_hash, min(mime_type) AS mime_type
            FROM storage.files
            WHERE (mime_type LIKE 'image/%' OR mime_type LIKE 'video/%')
              AND NOT is_trashed
            GROUP BY blob_hash
            "#,
        )
        .fetch(&*self.pool);

        let mut total: usize = 0;
        let mut processed: usize = 0;
        let mut failed: usize = 0;

        info!("Starting streaming capture-metadata backfill for image/video files");

        while let Some(row) = stream.next().await {
            total += 1;
            let (source_hash, mime_type) = row.map_err(|e| {
                DomainError::database_error(format!("Failed to fetch media content row: {}", e))
            })?;
            match self.extract_and_save(&source_hash, &mime_type).await {
                Ok(()) => processed += 1,
                Err(e) => {
                    warn!(
                        "Failed to extract capture metadata for blob {}: {}",
                        source_hash, e
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
        _file_id: &str,
        blob_hash: &str,
        content_type: &str,
        is_new_blob: bool,
    ) {
        if !Self::handles(content_type) {
            return;
        }
        // Content-keyed: a dedup hit means these facts already exist, so
        // there is nothing to extract and nothing to copy. The new file
        // still gets its `media_sort_date` — the BEFORE INSERT trigger
        // added by this change reads the existing content row, which is
        // the case the old clone-or-extract path was covering in Rust.
        if !is_new_blob {
            return;
        }
        Self::spawn_extraction_background(
            self.arc(),
            blob_hash.to_string(),
            content_type.to_string(),
        );
    }

    fn on_file_copied(
        &self,
        _file_id: &str,
        _blob_hash: &str,
        _content_type: &str,
        _source_file_id: &str,
    ) {
        // No-op: a copy shares the source's blob, so the content-keyed row
        // is already there. This used to clone the row per copy — pure
        // compensation for the old key.
        //
        // It also fixes the recursive folder copy, which fires no Rust
        // hooks at all (the WebDAV `COPY Depth: infinity` path is pure
        // SQL), so file-keyed EXIF was simply never created for
        // recursively-copied photos. Content-keyed, the copy resolves to
        // the existing row with no code involved.
    }

    fn on_file_updated(
        &self,
        _file_id: &str,
        blob_hash: &str,
        content_type: &str,
        _source: crate::application::ports::file_lifecycle::WriteSource,
    ) {
        if !Self::handles(content_type) {
            return;
        }
        // `blob_hash` is the NEW content. Keying the delete on it rather
        // than on the file means other files still pointing at the OLD
        // content keep their facts — their bytes did not change.
        Self::spawn_extraction_with_delete_background(
            self.arc(),
            blob_hash.to_string(),
            content_type.to_string(),
        );
    }

    fn on_file_deleted(&self, _file_id: &str) {
        // Nothing to do, and NOT for the old reason.
        // `storage.content_metadata` is keyed by content, so it has no
        // `file_id` and no cascade: the row deliberately outlives the
        // file, because other files may share those bytes. It dies with
        // the BLOB, via `BlobHandler::purge_content_facts` at reap time.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(year: i32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap()
    }

    /// The whole extraction path, against a real EXIF-bearing JPEG.
    ///
    /// Every other test in this module feeds synthesised `ExifMetadata` /
    /// `NomExif` structs into `merge_image_metadata`, which pins the merge
    /// rules and nothing else: the parsers themselves, and the dispatch in
    /// `extract_blocking`, were never exercised by a test. That gap is why
    /// no suite noticed when a column rename left the Photos query
    /// selecting a column that no longer existed — the only thing that
    /// caught it was an API test asserting a status code.
    ///
    /// It is also the gap that matters most here, because the parse result
    /// is what the whole content-keying change moves around. A fixture with
    /// no EXIF proves a copy shares a row; it cannot prove the row holds
    /// the right values.
    ///
    /// The fixture is 752 bytes, generated by
    /// `scripts/helpers/make-exif-fixture.py`, which documents each value.
    /// Orientation 6 and a southern-hemisphere-free but non-round GPS pair
    /// are deliberate: 0, 1 and NULL are all values a bug could produce by
    /// accident.
    #[test]
    fn extracts_every_exif_fact_from_a_real_jpeg() {
        let path = Path::new("tests/fixtures/exif-sample.jpg");
        let meta = MediaMetadataService::extract_blocking(path, "image/jpeg")
            .expect("the fixture carries EXIF, so extraction must yield metadata");

        assert_eq!(
            meta.captured_at,
            Some(Utc.with_ymd_and_hms(2019, 7, 14, 10, 30, 0).unwrap()),
            "DateTimeOriginal",
        );
        assert_eq!(meta.camera_make.as_deref(), Some("OxiCloud"));
        assert_eq!(meta.camera_model.as_deref(), Some("TestCam A1"));
        assert_eq!(meta.orientation, Some(6), "rotate 90° CW");

        // 48° 2' 18" N, 4° 44' 3" W. Compared with a tolerance because the
        // value round-trips through EXIF rationals.
        let lat = meta.latitude.expect("GPS latitude");
        let lon = meta.longitude.expect("GPS longitude");
        assert!((lat - 48.038_333).abs() < 1e-4, "latitude was {lat}");
        assert!(
            (lon - -4.734_166).abs() < 1e-4,
            "longitude was {lon} — a sign error here puts the photo in Asia",
        );
    }

    /// A JPEG with no EXIF must yield `None`, not an empty row.
    ///
    /// The distinction is load-bearing: `extract_and_save` skips the upsert
    /// on `None`, so a `Some(empty)` would write a content row claiming the
    /// bytes make no date claim — indistinguishable, later, from a real
    /// extraction that found nothing, and enough to make a re-derive think
    /// the work was already done.
    #[test]
    fn a_jpeg_without_exif_yields_nothing_to_store() {
        let path = Path::new("tests/fixtures/dedup-test.jpg");
        assert!(MediaMetadataService::extract_blocking(path, "image/jpeg").is_none());
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
