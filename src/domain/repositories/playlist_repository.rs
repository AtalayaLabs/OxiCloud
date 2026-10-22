use crate::common::errors::DomainError;
use crate::domain::entities::playlist::{AudioFileMetadata, Playlist, PlaylistItem};
use uuid::Uuid;

pub type PlaylistRepositoryResult<T> = Result<T, DomainError>;

pub trait PlaylistRepository: Send + Sync + 'static {
    async fn create_playlist(&self, playlist: Playlist) -> PlaylistRepositoryResult<Playlist>;

    async fn update_playlist(&self, playlist: Playlist) -> PlaylistRepositoryResult<Playlist>;

    async fn delete_playlist(&self, id: &Uuid) -> PlaylistRepositoryResult<()>;

    async fn find_playlist_by_id(&self, id: &Uuid) -> PlaylistRepositoryResult<Playlist>;

    /// Batch sibling of [`Self::find_playlist_by_id`]: one `= ANY($1)`
    /// round-trip for a page of grant-derived ids. Missing ids drop
    /// out; ordering is not guaranteed.
    async fn find_playlists_by_ids(&self, ids: &[Uuid]) -> PlaylistRepositoryResult<Vec<Playlist>>;
}

pub type PlaylistItemRepositoryResult<T> = Result<T, DomainError>;

pub trait PlaylistItemRepository: Send + Sync + 'static {
    async fn add_item(&self, item: PlaylistItem) -> PlaylistItemRepositoryResult<PlaylistItem>;

    async fn remove_item(&self, id: &Uuid) -> PlaylistItemRepositoryResult<()>;

    async fn remove_item_by_playlist_and_file(
        &self,
        playlist_id: &Uuid,
        file_id: &Uuid,
    ) -> PlaylistItemRepositoryResult<()>;

    async fn find_item_by_id(&self, id: &Uuid) -> PlaylistItemRepositoryResult<PlaylistItem>;

    async fn list_items_in_playlist(
        &self,
        playlist_id: &Uuid,
    ) -> PlaylistItemRepositoryResult<Vec<PlaylistItem>>;

    async fn update_position(
        &self,
        id: &Uuid,
        new_position: i32,
    ) -> PlaylistItemRepositoryResult<()>;

    async fn reorder_items(
        &self,
        playlist_id: &Uuid,
        item_ids: &[Uuid],
    ) -> PlaylistItemRepositoryResult<()>;

    async fn get_item_count(&self, playlist_id: &Uuid) -> PlaylistItemRepositoryResult<i64>;

    async fn get_max_position(&self, playlist_id: &Uuid) -> PlaylistItemRepositoryResult<i32>;
}

pub type AudioMetadataRepositoryResult<T> = Result<T, DomainError>;

/// Read side of `audio.content_metadata`.
///
/// One method, deliberately. `create_or_update` / `delete` /
/// `list_by_{artist,album,genre}` lived here and had no callers:
/// `AudioMetadataService` writes with its own SQL against the pool, and
/// there is no browse-by-artist feature for the list methods to serve.
/// They went with the content re-key rather than being ported to it —
/// porting dead code is how it survives another round.
pub trait AudioMetadataRepository: Send + Sync + 'static {
    /// Tags for the content behind one file. Resolves file → `blob_hash`
    /// internally; the returned entity carries the requested `file_id`,
    /// not the hash, so callers keep a file-shaped answer.
    async fn find_by_file_id(
        &self,
        file_id: &Uuid,
    ) -> AudioMetadataRepositoryResult<Option<AudioFileMetadata>>;
}
