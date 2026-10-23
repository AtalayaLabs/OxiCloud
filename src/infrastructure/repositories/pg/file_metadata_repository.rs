//! PostgreSQL repository for image/video EXIF metadata.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

use crate::common::errors::DomainError;
use crate::infrastructure::services::exif_service::ExifMetadata;

/// Row shape returned by metadata queries (avoids `clippy::type_complexity`).
type MetadataRow = (
    String,
    Option<DateTime<Utc>>,
    Option<f64>,
    Option<f64>,
    Option<String>,
    Option<String>,
    Option<i16>,
    Option<i32>,
    Option<i32>,
);

/// Metadata as stored/retrieved from the database.
#[derive(Debug, Clone, Serialize)]
pub struct StoredMetadata {
    pub file_id: String,
    pub captured_at: Option<DateTime<Utc>>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub orientation: Option<i16>,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

/// Repository for `storage.content_metadata` table operations.
///
/// The table is keyed by content hash; this repository is the layer that
/// maps it back to the file ids its callers hold.
pub struct FileMetadataRepository {
    pool: Arc<PgPool>,
}

impl FileMetadataRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Insert or update the facts parsed from one blob's content.
    ///
    /// Keyed by CONTENT: N copies of a photo share one row, and a copy
    /// writes nothing. See
    /// `migrations/20261105000000_exif_metadata_content_keyed.sql`.
    ///
    /// Note the column is `created_at` while the Rust field stays
    /// `captured_at`, and the asymmetry is deliberate.
    /// `docs/plan/normalize-file-dates.md` fixes the COLUMN name, because
    /// it stops being photo-specific the moment the extractor learns to
    /// read a document's creation date — at which point `captured_at`
    /// would be a lie. The Rust side keeps EXIF vocabulary until that
    /// widening actually happens, rather than renaming 56 call sites
    /// inside a schema migration. Mapping the two is what this repository
    /// is for.
    pub async fn upsert(&self, source_hash: &str, meta: &ExifMetadata) -> Result<(), DomainError> {
        sqlx::query(
            r#"
            INSERT INTO storage.content_metadata
                (source_hash, created_at, latitude, longitude, camera_make, camera_model, orientation, width, height)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (source_hash) DO UPDATE SET
                created_at   = EXCLUDED.created_at,
                latitude     = EXCLUDED.latitude,
                longitude    = EXCLUDED.longitude,
                camera_make  = EXCLUDED.camera_make,
                camera_model = EXCLUDED.camera_model,
                orientation  = EXCLUDED.orientation,
                width        = EXCLUDED.width,
                height       = EXCLUDED.height,
                updated_at   = CURRENT_TIMESTAMP
            "#,
        )
        .bind(source_hash)
        .bind(meta.captured_at)
        .bind(meta.latitude)
        .bind(meta.longitude)
        .bind(&meta.camera_make)
        .bind(&meta.camera_model)
        .bind(meta.orientation.map(|o| o as i16))
        .bind(meta.width.map(|w| w as i32))
        .bind(meta.height.map(|h| h as i32))
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!("Failed to upsert file metadata: {}", e);
            DomainError::internal_error("FileMetadata", format!("upsert: {e}"))
        })?;

        Ok(())
    }

    /// Get metadata for a single file.
    pub async fn get(&self, file_id: &str) -> Result<Option<StoredMetadata>, DomainError> {
        let row: Option<MetadataRow> = sqlx::query_as(
            r#"
            SELECT $1::text, m.created_at, m.latitude, m.longitude,
                   m.camera_make, m.camera_model, m.orientation, m.width, m.height
              FROM storage.content_metadata m
             WHERE m.source_hash = (SELECT blob_hash FROM storage.files WHERE id = $1::uuid)
            "#,
        )
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!("Failed to get file metadata: {}", e);
            DomainError::internal_error("FileMetadata", format!("get: {e}"))
        })?;

        Ok(row.map(
            |(
                file_id,
                captured_at,
                latitude,
                longitude,
                camera_make,
                camera_model,
                orientation,
                width,
                height,
            )| {
                StoredMetadata {
                    file_id,
                    captured_at,
                    latitude,
                    longitude,
                    camera_make,
                    camera_model,
                    orientation,
                    width,
                    height,
                }
            },
        ))
    }

    /// Get metadata for multiple files in a single query.
    /// Batch-read metadata for a set of files, keyed by file id.
    ///
    /// **Sparse by construction**: files whose content has no
    /// `storage.content_metadata` row are simply absent from the map, and a
    /// present row may still carry `captured_at = NULL`. Callers must treat both as "the
    /// content claims no date" rather than defaulting to a file clock —
    /// see `nc_creation_time` in `interfaces/nextcloud`.
    ///
    /// Takes `&[&str]` rather than `&[String]` because the callers are
    /// PROPFIND/REPORT row loops that already hold borrowed ids and are
    /// deliberately allocation-free per row (benches/ROUND19.md §M6).
    pub async fn get_batch(
        &self,
        file_ids: &[&str],
    ) -> Result<HashMap<String, StoredMetadata>, DomainError> {
        if file_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let rows: Vec<MetadataRow> = sqlx::query_as(
            r#"
            SELECT f.id::text, m.created_at, m.latitude, m.longitude,
                   m.camera_make, m.camera_model, m.orientation, m.width, m.height
              FROM storage.files f
              JOIN storage.content_metadata m ON m.source_hash = f.blob_hash
             WHERE f.id = ANY($1::uuid[])
            "#,
        )
        .bind(file_ids)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| {
            error!("Failed to batch get file metadata: {}", e);
            DomainError::internal_error("FileMetadata", format!("get_batch: {e}"))
        })?;

        let mut map = HashMap::with_capacity(rows.len());
        for (
            file_id,
            captured_at,
            latitude,
            longitude,
            camera_make,
            camera_model,
            orientation,
            width,
            height,
        ) in rows
        {
            map.insert(
                file_id.clone(),
                StoredMetadata {
                    file_id,
                    captured_at,
                    latitude,
                    longitude,
                    camera_make,
                    camera_model,
                    orientation,
                    width,
                    height,
                },
            );
        }

        Ok(map)
    }
}
