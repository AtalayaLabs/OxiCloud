use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::playlist_dto::{
    AudioMetadataDto, CreatePlaylistDto, PlaylistDto, PlaylistItemDto, UpdatePlaylistDto,
};
use crate::application::ports::music_ports::MusicStoragePort;
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::playlist::Playlist;
use crate::domain::repositories::playlist_repository::{
    AudioMetadataRepository, PlaylistItemRepository, PlaylistRepository,
};
use crate::infrastructure::repositories::pg::{
    AudioMetadataPgRepository, PlaylistItemPgRepository, PlaylistPgRepository,
};

pub struct MusicStorageAdapter {
    playlist_repository: Arc<PlaylistPgRepository>,
    item_repository: Arc<PlaylistItemPgRepository>,
    audio_metadata_repository: Arc<AudioMetadataPgRepository>,
}

impl MusicStorageAdapter {
    pub fn new(
        playlist_repository: Arc<PlaylistPgRepository>,
        item_repository: Arc<PlaylistItemPgRepository>,
        audio_metadata_repository: Arc<AudioMetadataPgRepository>,
    ) -> Self {
        Self {
            playlist_repository,
            item_repository,
            audio_metadata_repository,
        }
    }
}

impl MusicStoragePort for MusicStorageAdapter {
    async fn create_playlist(
        &self,
        dto: CreatePlaylistDto,
        user_id: Uuid,
    ) -> Result<PlaylistDto, DomainError> {
        let playlist = Playlist::new(dto.name, user_id, dto.description)?;
        let created = self.playlist_repository.create_playlist(playlist).await?;
        Ok(PlaylistDto::from(created))
    }

    async fn update_playlist(
        &self,
        playlist_id: &str,
        dto: UpdatePlaylistDto,
    ) -> Result<PlaylistDto, DomainError> {
        let uuid = Uuid::parse_str(playlist_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Playlist", "Invalid playlist ID")
        })?;

        let mut playlist = self.playlist_repository.find_playlist_by_id(&uuid).await?;

        if let Some(name) = dto.name {
            playlist.update_name(name)?;
        }
        if let Some(description) = dto.description {
            playlist.update_description(Some(description));
        }
        if let Some(cover_file_id) = dto.cover_file_id {
            let cover_uuid = Uuid::parse_str(&cover_file_id).map_err(|_| {
                DomainError::new(ErrorKind::InvalidInput, "Playlist", "Invalid cover file ID")
            })?;
            playlist.set_cover(Some(cover_uuid));
        }

        let updated = self.playlist_repository.update_playlist(playlist).await?;
        Ok(PlaylistDto::from(updated))
    }

    async fn delete_playlist(&self, playlist_id: &str) -> Result<(), DomainError> {
        let uuid = Uuid::parse_str(playlist_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Playlist", "Invalid playlist ID")
        })?;
        self.playlist_repository.delete_playlist(&uuid).await
    }

    async fn get_playlist(&self, playlist_id: &str) -> Result<Option<PlaylistDto>, DomainError> {
        let uuid = Uuid::parse_str(playlist_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Playlist", "Invalid playlist ID")
        })?;

        match self.playlist_repository.find_playlist_by_id(&uuid).await {
            Ok(playlist) => Ok(Some(PlaylistDto::from(playlist))),
            Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_playlists_by_ids(&self, ids: &[Uuid]) -> Result<Vec<PlaylistDto>, DomainError> {
        let playlists = self.playlist_repository.find_playlists_by_ids(ids).await?;
        Ok(playlists.into_iter().map(PlaylistDto::from).collect())
    }

    async fn list_playlists_by_owner(
        &self,
        owner_id: Uuid,
    ) -> Result<Vec<PlaylistDto>, DomainError> {
        let playlists = self
            .playlist_repository
            .list_playlists_by_owner(owner_id)
            .await?;
        let mut result = Vec::new();
        for playlist in playlists {
            let dto = PlaylistDto::from(playlist);
            let track_count = self
                .get_track_count(&uuid::Uuid::parse_str(&dto.id).unwrap())
                .await?;
            result.push(dto.with_track_info(track_count, 0));
        }
        Ok(result)
    }

    async fn list_shared_with_user(&self, user_id: Uuid) -> Result<Vec<PlaylistDto>, DomainError> {
        let playlists = self
            .playlist_repository
            .list_shared_with_user(user_id)
            .await?;
        let mut result = Vec::new();
        for playlist in playlists {
            let dto = PlaylistDto::from(playlist);
            let track_count = self
                .get_track_count(&uuid::Uuid::parse_str(&dto.id).unwrap())
                .await?;
            result.push(dto.with_track_info(track_count, 0));
        }
        Ok(result)
    }

    async fn user_has_access(&self, playlist_id: &str, user_id: Uuid) -> Result<bool, DomainError> {
        let uuid = Uuid::parse_str(playlist_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Playlist", "Invalid playlist ID")
        })?;
        self.playlist_repository
            .user_has_access(&uuid, user_id)
            .await
    }

    async fn user_can_write(&self, playlist_id: &str, user_id: Uuid) -> Result<bool, DomainError> {
        let uuid = Uuid::parse_str(playlist_id).map_err(|_| {
            DomainError::new(ErrorKind::InvalidInput, "Playlist", "Invalid playlist ID")
        })?;

        let playlist = self.playlist_repository.find_playlist_by_id(&uuid).await?;
        if playlist.owner_id() == &user_id {
            return Ok(true);
        }

        let shares = self.playlist_repository.get_shares(&uuid).await?;
        Ok(shares
            .iter()
            .any(|(uid, can_write)| uid == &user_id && *can_write))
    }

    async fn add_tracks(
        &self,
        playlist_id: &Uuid,
        file_ids: &[Uuid],
    ) -> Result<Vec<PlaylistItemDto>, DomainError> {
        let mut max_position = self.item_repository.get_max_position(playlist_id).await?;
        let mut items = Vec::new();

        for file_id in file_ids {
            max_position += 1;
            let item = crate::domain::entities::playlist::PlaylistItem::new(
                *playlist_id,
                *file_id,
                max_position,
            )?;
            let created = self.item_repository.add_item(item).await?;
            items.push(PlaylistItemDto::from(created));
        }

        Ok(items)
    }

    async fn remove_track(&self, playlist_id: &Uuid, file_id: &Uuid) -> Result<(), DomainError> {
        self.item_repository
            .remove_item_by_playlist_and_file(playlist_id, file_id)
            .await
    }

    async fn reorder_tracks(
        &self,
        playlist_id: &Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), DomainError> {
        self.item_repository
            .reorder_items(playlist_id, item_ids)
            .await
    }

    async fn list_playlist_tracks(
        &self,
        playlist_id: &Uuid,
    ) -> Result<Vec<PlaylistItemDto>, DomainError> {
        let enriched_items = self
            .item_repository
            .list_items_in_playlist_enriched(playlist_id)
            .await?;

        let items: Vec<PlaylistItemDto> = enriched_items
            .into_iter()
            .map(
                |row| crate::application::dtos::playlist_dto::PlaylistItemDto {
                    id: row.id.to_string(),
                    playlist_id: row.playlist_id.to_string(),
                    file_id: row.file_id.to_string(),
                    position: row.position,
                    added_at: row.added_at,
                    file_name: row.file_name,
                    file_size: row.file_size,
                    mime_type: row.mime_type,
                    title: row.title,
                    artist: row.artist,
                    album: row.album,
                    duration_secs: row.duration_secs,
                },
            )
            .collect();
        Ok(items)
    }

    async fn share_playlist(
        &self,
        playlist_id: &Uuid,
        user_id: Uuid,
        can_write: bool,
    ) -> Result<(), DomainError> {
        self.playlist_repository
            .share_playlist(playlist_id, user_id, can_write)
            .await
    }

    async fn remove_share(&self, playlist_id: &Uuid, user_id: Uuid) -> Result<(), DomainError> {
        self.playlist_repository
            .remove_share(playlist_id, user_id)
            .await
    }

    async fn get_shares(&self, playlist_id: &Uuid) -> Result<Vec<(Uuid, bool)>, DomainError> {
        self.playlist_repository.get_shares(playlist_id).await
    }

    async fn get_audio_metadata(
        &self,
        file_id: &Uuid,
    ) -> Result<Option<AudioMetadataDto>, DomainError> {
        match self
            .audio_metadata_repository
            .find_by_file_id(file_id)
            .await
        {
            Ok(Some(metadata)) => Ok(Some(AudioMetadataDto::from(metadata))),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl MusicStorageAdapter {
    async fn get_track_count(&self, playlist_id: &Uuid) -> Result<i64, DomainError> {
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM audio.playlist_items WHERE playlist_id = $1")
                .bind(playlist_id)
                .fetch_one(self.playlist_repository.pool())
                .await
                .map_err(|e| {
                    DomainError::database_error(format!("Failed to get track count: {}", e))
                })?;
        Ok(count.0)
    }
}
