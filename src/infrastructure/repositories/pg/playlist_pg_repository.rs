use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use std::sync::Arc;
use uuid::Uuid;

use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::playlist::{AudioFileMetadata, Playlist, PlaylistItem};
use crate::domain::repositories::playlist_repository::{
    AudioMetadataRepository, AudioMetadataRepositoryResult, PlaylistItemRepository,
    PlaylistItemRepositoryResult, PlaylistRepository, PlaylistRepositoryResult,
};

#[derive(FromRow)]
struct PlaylistRow {
    id: Uuid,
    name: String,
    description: Option<String>,
    owner_id: Uuid,
    cover_file_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct PlaylistItemRow {
    id: Uuid,
    playlist_id: Uuid,
    file_id: Uuid,
    position: i32,
    added_at: DateTime<Utc>,
}

/// A row of `audio.content_metadata`, which is keyed by CONTENT and so
/// carries no file id — several files may share these tags. Callers
/// re-attach whichever file id they were asked about.
#[derive(FromRow)]
struct AudioContentMetadataRow {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    year: Option<i32>,
    duration_secs: i32,
    bitrate: Option<i32>,
    sample_rate: Option<i32>,
    channels: Option<i16>,
    format: Option<String>,
    /// When we parsed these tags. Named for the row, not the content —
    /// `created_at` is reserved on content-keyed metadata for the
    /// content's own claimed creation date, per
    /// `docs/plan/normalize-file-dates.md`.
    extracted_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
pub struct PlaylistItemEnrichedRow {
    pub id: Uuid,
    pub playlist_id: Uuid,
    pub file_id: Uuid,
    pub position: i32,
    pub added_at: DateTime<Utc>,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
    pub mime_type: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: Option<i32>,
}

pub struct PlaylistPgRepository {
    pool: Arc<PgPool>,
}

impl PlaylistPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl PlaylistRepository for PlaylistPgRepository {
    async fn create_playlist(&self, playlist: Playlist) -> PlaylistRepositoryResult<Playlist> {
        let row = sqlx::query_as::<_, PlaylistRow>(
            r#"
            INSERT INTO audio.playlists (id, name, description, owner_id, cover_file_id, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING id, name, description, owner_id, cover_file_id, created_at, updated_at
            "#,
        )
        .bind(playlist.id())
        .bind(playlist.name())
        .bind(playlist.description())
        .bind(playlist.owner_id())
        .bind(playlist.cover_file_id())
        .bind(playlist.created_at())
        .bind(playlist.updated_at())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to create playlist: {}", e)))?;

        Playlist::with_id(
            row.id,
            row.name,
            row.description,
            row.owner_id,
            row.cover_file_id,
            row.created_at,
            row.updated_at,
        )
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "Playlist", e.to_string()))
    }

    async fn update_playlist(&self, playlist: Playlist) -> PlaylistRepositoryResult<Playlist> {
        let row = sqlx::query_as::<_, PlaylistRow>(
            r#"
            UPDATE audio.playlists
            SET name = $2, description = $3, cover_file_id = $4, updated_at = NOW()
            WHERE id = $1
            RETURNING id, name, description, owner_id, cover_file_id, created_at, updated_at
            "#,
        )
        .bind(playlist.id())
        .bind(playlist.name())
        .bind(playlist.description())
        .bind(playlist.cover_file_id())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to update playlist: {}", e)))?;

        Playlist::with_id(
            row.id,
            row.name,
            row.description,
            row.owner_id,
            row.cover_file_id,
            row.created_at,
            row.updated_at,
        )
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "Playlist", e.to_string()))
    }

    async fn delete_playlist(&self, id: &Uuid) -> PlaylistRepositoryResult<()> {
        sqlx::query("DELETE FROM audio.playlists WHERE id = $1")
            .bind(id)
            .execute(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to delete playlist: {}", e))
            })?;
        Ok(())
    }

    async fn find_playlist_by_id(&self, id: &Uuid) -> PlaylistRepositoryResult<Playlist> {
        let row = sqlx::query_as::<_, PlaylistRow>(
            "SELECT id, name, description, owner_id, cover_file_id, created_at, updated_at FROM audio.playlists WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to find playlist: {}", e)))?
        .ok_or_else(|| DomainError::new(ErrorKind::NotFound, "Playlist", "Playlist not found"))?;

        Playlist::with_id(
            row.id,
            row.name,
            row.description,
            row.owner_id,
            row.cover_file_id,
            row.created_at,
            row.updated_at,
        )
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "Playlist", e.to_string()))
    }

    async fn find_playlists_by_ids(&self, ids: &[Uuid]) -> PlaylistRepositoryResult<Vec<Playlist>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, PlaylistRow>(
            "SELECT id, name, description, owner_id, cover_file_id, created_at, updated_at FROM audio.playlists WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to find playlists: {}", e)))?;

        rows.into_iter()
            .map(|row| {
                Playlist::with_id(
                    row.id,
                    row.name,
                    row.description,
                    row.owner_id,
                    row.cover_file_id,
                    row.created_at,
                    row.updated_at,
                )
                .map_err(|e| DomainError::new(ErrorKind::InternalError, "Playlist", e.to_string()))
            })
            .collect()
    }
}

pub struct PlaylistItemPgRepository {
    pool: Arc<PgPool>,
}

impl PlaylistItemPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    pub async fn list_items_in_playlist_enriched(
        &self,
        playlist_id: &Uuid,
    ) -> PlaylistItemRepositoryResult<Vec<PlaylistItemEnrichedRow>> {
        let rows = sqlx::query_as::<_, PlaylistItemEnrichedRow>(
            r#"
            SELECT 
                pi.id, pi.playlist_id, pi.file_id, pi.position, pi.added_at,
                f.name as file_name, f.size as file_size, f.mime_type,
                m.title, m.artist, m.album, m.duration_secs
            FROM audio.playlist_items pi
            LEFT JOIN storage.files f ON pi.file_id = f.id
            LEFT JOIN audio.content_metadata m ON m.source_hash = f.blob_hash
            WHERE pi.playlist_id = $1
            ORDER BY pi.position ASC
            "#,
        )
        .bind(playlist_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to list playlist items: {}", e))
        })?;
        Ok(rows)
    }
}

impl PlaylistItemRepository for PlaylistItemPgRepository {
    async fn add_item(&self, item: PlaylistItem) -> PlaylistItemRepositoryResult<PlaylistItem> {
        let row = sqlx::query_as::<_, PlaylistItemRow>(
            r#"
            INSERT INTO audio.playlist_items (id, playlist_id, file_id, position, added_at)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (playlist_id, file_id) DO UPDATE SET position = $4
            RETURNING id, playlist_id, file_id, position, added_at
            "#,
        )
        .bind(item.id())
        .bind(item.playlist_id())
        .bind(item.file_id())
        .bind(item.position())
        .bind(item.added_at())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to add playlist item: {}", e)))?;

        PlaylistItem::with_id(
            row.id,
            row.playlist_id,
            row.file_id,
            row.position,
            row.added_at,
        )
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "PlaylistItem", e.to_string()))
    }

    async fn remove_item(&self, id: &Uuid) -> PlaylistItemRepositoryResult<()> {
        sqlx::query("DELETE FROM audio.playlist_items WHERE id = $1")
            .bind(id)
            .execute(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to remove playlist item: {}", e))
            })?;
        Ok(())
    }

    async fn remove_item_by_playlist_and_file(
        &self,
        playlist_id: &Uuid,
        file_id: &Uuid,
    ) -> PlaylistItemRepositoryResult<()> {
        sqlx::query("DELETE FROM audio.playlist_items WHERE playlist_id = $1 AND file_id = $2")
            .bind(playlist_id)
            .bind(file_id)
            .execute(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to remove playlist item: {}", e))
            })?;
        Ok(())
    }

    async fn find_item_by_id(&self, id: &Uuid) -> PlaylistItemRepositoryResult<PlaylistItem> {
        let row = sqlx::query_as::<_, PlaylistItemRow>(
            "SELECT id, playlist_id, file_id, position, added_at FROM audio.playlist_items WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to find playlist item: {}", e)))?
        .ok_or_else(|| DomainError::new(ErrorKind::NotFound, "PlaylistItem", "Item not found"))?;

        PlaylistItem::with_id(
            row.id,
            row.playlist_id,
            row.file_id,
            row.position,
            row.added_at,
        )
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "PlaylistItem", e.to_string()))
    }

    async fn list_items_in_playlist(
        &self,
        playlist_id: &Uuid,
    ) -> PlaylistItemRepositoryResult<Vec<PlaylistItem>> {
        let rows = sqlx::query_as::<_, PlaylistItemRow>(
            "SELECT id, playlist_id, file_id, position, added_at FROM audio.playlist_items WHERE playlist_id = $1 ORDER BY position ASC",
        )
        .bind(playlist_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to list playlist items: {}", e)))?;

        rows.into_iter()
            .map(|row| {
                PlaylistItem::with_id(
                    row.id,
                    row.playlist_id,
                    row.file_id,
                    row.position,
                    row.added_at,
                )
                .map_err(|e| {
                    DomainError::new(ErrorKind::InternalError, "PlaylistItem", e.to_string())
                })
            })
            .collect()
    }

    async fn update_position(
        &self,
        id: &Uuid,
        new_position: i32,
    ) -> PlaylistItemRepositoryResult<()> {
        sqlx::query("UPDATE audio.playlist_items SET position = $2 WHERE id = $1")
            .bind(id)
            .bind(new_position)
            .execute(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to update position: {}", e))
            })?;
        Ok(())
    }

    async fn reorder_items(
        &self,
        playlist_id: &Uuid,
        item_ids: &[Uuid],
    ) -> PlaylistItemRepositoryResult<()> {
        if item_ids.is_empty() {
            return Ok(());
        }
        // One UNNEST-driven UPDATE instead of one autocommit round-trip per
        // track — a full drag-reorder of an N-track playlist was N statements
        // (and non-atomic: a mid-loop failure left a half-applied order).
        // `WITH ORDINALITY` numbers the ids in array order, 1-based, so
        // `ord - 1` reproduces the historical 0-based positions.
        sqlx::query(
            r#"
            UPDATE audio.playlist_items AS pi
               SET position = (t.ord - 1)::int
              FROM unnest($1::uuid[]) WITH ORDINALITY AS t(id, ord)
             WHERE pi.id = t.id AND pi.playlist_id = $2
            "#,
        )
        .bind(item_ids)
        .bind(playlist_id)
        .execute(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to reorder: {}", e)))?;
        Ok(())
    }

    async fn get_item_count(&self, playlist_id: &Uuid) -> PlaylistItemRepositoryResult<i64> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM audio.playlist_items WHERE playlist_id = $1",
        )
        .bind(playlist_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to count items: {}", e)))?;
        Ok(count)
    }

    async fn get_max_position(&self, playlist_id: &Uuid) -> PlaylistItemRepositoryResult<i32> {
        let max_pos = sqlx::query_scalar::<_, Option<i32>>(
            "SELECT MAX(position) FROM audio.playlist_items WHERE playlist_id = $1",
        )
        .bind(playlist_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to get max position: {}", e)))?;
        Ok(max_pos.unwrap_or(-1))
    }
}

pub struct AudioMetadataPgRepository {
    pool: Arc<PgPool>,
}

impl AudioMetadataPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

impl AudioMetadataRepository for AudioMetadataPgRepository {
    async fn find_by_file_id(
        &self,
        file_id: &Uuid,
    ) -> AudioMetadataRepositoryResult<Option<AudioFileMetadata>> {
        // Content-keyed now, so resolve the file to its content first. The
        // subquery is a PK lookup on storage.files followed by a PK lookup
        // on content_metadata — same two index hits as the old direct
        // read, and it means N copies of a track share one row.
        //
        // The row carries NO file id: these columns describe content, which
        // several files may share. `file_id` is re-attached below from the
        // id the caller asked about, so the DTO still answers "metadata for
        // THIS file" and the API shape is unchanged. Selecting
        // `source_hash AS file_id` instead would compile and then fail at
        // runtime, since a 64-char hex string does not decode as a UUID.
        let row = sqlx::query_as::<_, AudioContentMetadataRow>(
            r#"
            SELECT title, artist, album, album_artist, genre,
                   track_number, disc_number, year, duration_secs, bitrate,
                   sample_rate, channels, format, extracted_at, updated_at
              FROM audio.content_metadata
             WHERE source_hash = (SELECT blob_hash FROM storage.files WHERE id = $1)
            "#,
        )
        .bind(file_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to find audio metadata: {}", e))
        })?;

        Ok(row.map(|r| {
            AudioFileMetadata::with_all_fields(
                // Re-attached from the request, not read from the row —
                // the row describes content, which may back many files.
                *file_id,
                r.title,
                r.artist,
                r.album,
                r.album_artist,
                r.genre,
                r.track_number,
                r.disc_number,
                r.year,
                r.duration_secs,
                r.bitrate,
                r.sample_rate,
                r.channels,
                r.format,
                // The entity's `created_at` carries the extraction time —
                // the same value the DTO has always exposed. Only the
                // COLUMN was renamed, to free `created_at` for the
                // content's own claimed date on the sibling table.
                r.extracted_at,
                r.updated_at,
            )
        }))
    }
}
