use crate::application::dtos::playlist_dto::{
    AddTracksDto, AudioMetadataDto, CreatePlaylistDto, PlaylistDto, PlaylistItemDto,
    PlaylistQueryDto, PlaylistShareInfoDto, ReorderTracksDto, SharePlaylistDto, UpdatePlaylistDto,
};
use crate::common::errors::DomainError;
use uuid::Uuid;

pub trait MusicUseCase: Send + Sync {
    async fn create_playlist(
        &self,
        dto: CreatePlaylistDto,
        user_id: Uuid,
    ) -> Result<PlaylistDto, DomainError>;

    async fn update_playlist(
        &self,
        playlist_id: &str,
        dto: UpdatePlaylistDto,
        user_id: Uuid,
    ) -> Result<PlaylistDto, DomainError>;

    async fn delete_playlist(&self, playlist_id: &str, user_id: Uuid) -> Result<(), DomainError>;

    async fn get_playlist(
        &self,
        playlist_id: &str,
        user_id: Uuid,
    ) -> Result<PlaylistDto, DomainError>;

    async fn list_playlists(
        &self,
        query: PlaylistQueryDto,
        user_id: Uuid,
    ) -> Result<Vec<PlaylistDto>, DomainError>;

    async fn add_tracks(
        &self,
        playlist_id: &str,
        dto: AddTracksDto,
        user_id: Uuid,
    ) -> Result<Vec<PlaylistItemDto>, DomainError>;

    async fn remove_track(
        &self,
        playlist_id: &str,
        file_id: &str,
        user_id: Uuid,
    ) -> Result<(), DomainError>;

    async fn reorder_tracks(
        &self,
        playlist_id: &str,
        dto: ReorderTracksDto,
        user_id: Uuid,
    ) -> Result<(), DomainError>;

    async fn list_playlist_tracks(
        &self,
        playlist_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<PlaylistItemDto>, DomainError>;

    async fn share_playlist(
        &self,
        playlist_id: &str,
        dto: SharePlaylistDto,
        caller_id: Uuid,
    ) -> Result<(), DomainError>;

    async fn remove_share(
        &self,
        playlist_id: &str,
        target_user_id: &str,
        caller_id: Uuid,
    ) -> Result<(), DomainError>;

    async fn get_playlist_shares(
        &self,
        playlist_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<PlaylistShareInfoDto>, DomainError>;

    async fn get_audio_metadata(
        &self,
        file_id: &str,
        user_id: Uuid,
    ) -> Result<Option<AudioMetadataDto>, DomainError>;
}

pub trait MusicStoragePort: Send + Sync {
    async fn create_playlist(
        &self,
        dto: CreatePlaylistDto,
        user_id: Uuid,
    ) -> Result<PlaylistDto, DomainError>;

    async fn update_playlist(
        &self,
        playlist_id: &str,
        dto: UpdatePlaylistDto,
    ) -> Result<PlaylistDto, DomainError>;

    async fn delete_playlist(&self, playlist_id: &str) -> Result<(), DomainError>;

    async fn get_playlist(&self, playlist_id: &str) -> Result<Option<PlaylistDto>, DomainError>;

    /// Batch sibling of [`Self::get_playlist`]: hydrate a page of
    /// grant-derived ids in ONE storage round-trip. Missing rows drop
    /// out silently; ordering is not guaranteed.
    async fn get_playlists_by_ids(&self, ids: &[Uuid]) -> Result<Vec<PlaylistDto>, DomainError>;

    async fn list_playlists_by_owner(
        &self,
        owner_id: Uuid,
    ) -> Result<Vec<PlaylistDto>, DomainError>;

    async fn list_shared_with_user(&self, user_id: Uuid) -> Result<Vec<PlaylistDto>, DomainError>;

    async fn user_has_access(&self, playlist_id: &str, user_id: Uuid) -> Result<bool, DomainError>;

    async fn user_can_write(&self, playlist_id: &str, user_id: Uuid) -> Result<bool, DomainError>;

    async fn add_tracks(
        &self,
        playlist_id: &Uuid,
        file_ids: &[Uuid],
    ) -> Result<Vec<PlaylistItemDto>, DomainError>;

    async fn remove_track(&self, playlist_id: &Uuid, file_id: &Uuid) -> Result<(), DomainError>;

    async fn reorder_tracks(
        &self,
        playlist_id: &Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), DomainError>;

    async fn list_playlist_tracks(
        &self,
        playlist_id: &Uuid,
    ) -> Result<Vec<PlaylistItemDto>, DomainError>;

    async fn share_playlist(
        &self,
        playlist_id: &Uuid,
        user_id: Uuid,
        can_write: bool,
    ) -> Result<(), DomainError>;

    async fn remove_share(&self, playlist_id: &Uuid, user_id: Uuid) -> Result<(), DomainError>;

    async fn get_shares(&self, playlist_id: &Uuid) -> Result<Vec<(Uuid, bool)>, DomainError>;

    async fn get_audio_metadata(
        &self,
        file_id: &Uuid,
    ) -> Result<Option<AudioMetadataDto>, DomainError>;
}
