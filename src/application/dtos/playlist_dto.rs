use crate::domain::entities::playlist::{AudioFileMetadata, Playlist, PlaylistItem};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct PlaylistDto {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: String,
    pub is_public: bool,
    pub cover_file_id: Option<String>,
    pub track_count: Option<i64>,
    pub total_duration_secs: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Default for PlaylistDto {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            description: None,
            owner_id: String::new(),
            is_public: false,
            cover_file_id: None,
            track_count: None,
            total_duration_secs: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}

impl From<Playlist> for PlaylistDto {
    fn from(playlist: Playlist) -> Self {
        Self {
            id: playlist.id().to_string(),
            name: playlist.name().to_string(),
            description: playlist.description().map(|s| s.to_string()),
            owner_id: playlist.owner_id().to_string(),
            is_public: playlist.is_public(),
            cover_file_id: playlist.cover_file_id().map(|s| s.to_string()),
            track_count: None,
            total_duration_secs: None,
            created_at: *playlist.created_at(),
            updated_at: *playlist.updated_at(),
        }
    }
}

impl PlaylistDto {
    pub fn with_track_info(mut self, track_count: i64, total_duration_secs: i32) -> Self {
        self.track_count = Some(track_count);
        self.total_duration_secs = Some(total_duration_secs);
        self
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct PlaylistItemDto {
    pub id: String,
    pub playlist_id: String,
    pub file_id: String,
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

impl Default for PlaylistItemDto {
    fn default() -> Self {
        Self {
            id: String::new(),
            playlist_id: String::new(),
            file_id: String::new(),
            position: 0,
            added_at: Utc::now(),
            file_name: None,
            file_size: None,
            mime_type: None,
            title: None,
            artist: None,
            album: None,
            duration_secs: None,
        }
    }
}

impl From<PlaylistItem> for PlaylistItemDto {
    fn from(item: PlaylistItem) -> Self {
        Self {
            id: item.id().to_string(),
            playlist_id: item.playlist_id().to_string(),
            file_id: item.file_id().to_string(),
            position: item.position(),
            added_at: *item.added_at(),
            file_name: None,
            file_size: None,
            mime_type: None,
            title: None,
            artist: None,
            album: None,
            duration_secs: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreatePlaylistDto {
    pub name: String,
    pub description: Option<String>,
    pub is_public: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdatePlaylistDto {
    pub name: Option<String>,
    pub description: Option<String>,
    pub is_public: Option<bool>,
    pub cover_file_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AddTracksDto {
    pub file_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ReorderTracksDto {
    pub item_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SharePlaylistDto {
    pub user_id: String,
    pub can_write: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PlaylistShareInfoDto {
    pub user_id: String,
    pub can_write: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AudioMetadataDto {
    pub file_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub year: Option<i32>,
    pub duration_secs: i32,
    pub bitrate: Option<i32>,
    pub sample_rate: Option<i32>,
    pub channels: Option<i16>,
    pub format: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Default for AudioMetadataDto {
    fn default() -> Self {
        Self {
            file_id: String::new(),
            title: None,
            artist: None,
            album: None,
            album_artist: None,
            genre: None,
            track_number: None,
            disc_number: None,
            year: None,
            duration_secs: 0,
            bitrate: None,
            sample_rate: None,
            channels: None,
            format: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}

impl From<AudioFileMetadata> for AudioMetadataDto {
    fn from(metadata: AudioFileMetadata) -> Self {
        Self {
            file_id: metadata.file_id().to_string(),
            title: metadata.title().map(|s| s.to_string()),
            artist: metadata.artist().map(|s| s.to_string()),
            album: metadata.album().map(|s| s.to_string()),
            album_artist: metadata.album_artist().map(|s| s.to_string()),
            genre: metadata.genre().map(|s| s.to_string()),
            track_number: metadata.track_number(),
            disc_number: metadata.disc_number(),
            year: metadata.year(),
            duration_secs: metadata.duration_secs(),
            bitrate: metadata.bitrate(),
            sample_rate: metadata.sample_rate(),
            channels: metadata.channels(),
            format: metadata.format().map(|s| s.to_string()),
            created_at: *metadata.created_at(),
            updated_at: *metadata.updated_at(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PlaylistQueryDto {
    pub include_shared: Option<bool>,
    pub include_public: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}
