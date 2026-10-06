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

#[derive(FromRow)]
struct AudioMetadataRow {
    file_id: Uuid,
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
    created_at: DateTime<Utc>,
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
            LEFT JOIN audio.file_metadata m ON pi.file_id = m.file_id
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
    async fn create_or_update(
        &self,
        metadata: AudioFileMetadata,
    ) -> AudioMetadataRepositoryResult<AudioFileMetadata> {
        let row = sqlx::query_as::<_, AudioMetadataRow>(
            r#"
            INSERT INTO audio.file_metadata (file_id, title, artist, album, album_artist, genre, track_number, disc_number, year, duration_secs, bitrate, sample_rate, channels, format, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
            ON CONFLICT (file_id) DO UPDATE SET
                title = COALESCE($2, audio.file_metadata.title),
                artist = COALESCE($3, audio.file_metadata.artist),
                album = COALESCE($4, audio.file_metadata.album),
                album_artist = COALESCE($5, audio.file_metadata.album_artist),
                genre = COALESCE($6, audio.file_metadata.genre),
                track_number = COALESCE($7, audio.file_metadata.track_number),
                disc_number = COALESCE($8, audio.file_metadata.disc_number),
                year = COALESCE($9, audio.file_metadata.year),
                duration_secs = $10,
                bitrate = COALESCE($11, audio.file_metadata.bitrate),
                sample_rate = COALESCE($12, audio.file_metadata.sample_rate),
                channels = COALESCE($13, audio.file_metadata.channels),
                format = COALESCE($14, audio.file_metadata.format),
                updated_at = NOW()
            RETURNING file_id, title, artist, album, album_artist, genre, track_number, disc_number, year, duration_secs, bitrate, sample_rate, channels, format, created_at, updated_at
            "#,
        )
        .bind(metadata.file_id())
        .bind(metadata.title())
        .bind(metadata.artist())
        .bind(metadata.album())
        .bind(metadata.album_artist())
        .bind(metadata.genre())
        .bind(metadata.track_number())
        .bind(metadata.disc_number())
        .bind(metadata.year())
        .bind(metadata.duration_secs())
        .bind(metadata.bitrate())
        .bind(metadata.sample_rate())
        .bind(metadata.channels())
        .bind(metadata.format())
        .bind(metadata.created_at())
        .bind(metadata.updated_at())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to create audio metadata: {}", e)))?;

        Ok(AudioFileMetadata::with_all_fields(
            row.file_id,
            row.title,
            row.artist,
            row.album,
            row.album_artist,
            row.genre,
            row.track_number,
            row.disc_number,
            row.year,
            row.duration_secs,
            row.bitrate,
            row.sample_rate,
            row.channels,
            row.format,
            row.created_at,
            row.updated_at,
        ))
    }

    async fn find_by_file_id(
        &self,
        file_id: &Uuid,
    ) -> AudioMetadataRepositoryResult<Option<AudioFileMetadata>> {
        let row = sqlx::query_as::<_, AudioMetadataRow>(
            "SELECT file_id, title, artist, album, album_artist, genre, track_number, disc_number, year, duration_secs, bitrate, sample_rate, channels, format, created_at, updated_at FROM audio.file_metadata WHERE file_id = $1",
        )
        .bind(file_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to find audio metadata: {}", e)))?;

        Ok(row.map(|r| {
            AudioFileMetadata::with_all_fields(
                r.file_id,
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
                r.created_at,
                r.updated_at,
            )
        }))
    }

    async fn delete(&self, file_id: &Uuid) -> AudioMetadataRepositoryResult<()> {
        sqlx::query("DELETE FROM audio.file_metadata WHERE file_id = $1")
            .bind(file_id)
            .execute(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to delete audio metadata: {}", e))
            })?;
        Ok(())
    }

    async fn list_by_artist(
        &self,
        artist: &str,
    ) -> AudioMetadataRepositoryResult<Vec<AudioFileMetadata>> {
        let rows = sqlx::query_as::<_, AudioMetadataRow>(
            "SELECT file_id, title, artist, album, album_artist, genre, track_number, disc_number, year, duration_secs, bitrate, sample_rate, channels, format, created_at, updated_at FROM audio.file_metadata WHERE artist ILIKE $1 ORDER BY album, track_number",
        )
        .bind(format!("%{}%", artist))
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to list by artist: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|r| {
                AudioFileMetadata::with_all_fields(
                    r.file_id,
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
                    r.created_at,
                    r.updated_at,
                )
            })
            .collect())
    }

    async fn list_by_album(
        &self,
        album: &str,
    ) -> AudioMetadataRepositoryResult<Vec<AudioFileMetadata>> {
        let rows = sqlx::query_as::<_, AudioMetadataRow>(
            "SELECT file_id, title, artist, album, album_artist, genre, track_number, disc_number, year, duration_secs, bitrate, sample_rate, channels, format, created_at, updated_at FROM audio.file_metadata WHERE album ILIKE $1 ORDER BY disc_number, track_number",
        )
        .bind(format!("%{}%", album))
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to list by album: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|r| {
                AudioFileMetadata::with_all_fields(
                    r.file_id,
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
                    r.created_at,
                    r.updated_at,
                )
            })
            .collect())
    }

    async fn list_by_genre(
        &self,
        genre: &str,
    ) -> AudioMetadataRepositoryResult<Vec<AudioFileMetadata>> {
        let rows = sqlx::query_as::<_, AudioMetadataRow>(
            "SELECT file_id, title, artist, album, album_artist, genre, track_number, disc_number, year, duration_secs, bitrate, sample_rate, channels, format, created_at, updated_at FROM audio.file_metadata WHERE genre ILIKE $1 ORDER BY artist, album, track_number",
        )
        .bind(format!("%{}%", genre))
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to list by genre: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|r| {
                AudioFileMetadata::with_all_fields(
                    r.file_id,
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
                    r.created_at,
                    r.updated_at,
                )
            })
            .collect())
    }
}
