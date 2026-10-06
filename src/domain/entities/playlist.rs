use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::common::errors::{DomainError, ErrorKind, Result};

#[derive(Debug, Clone)]
pub struct Playlist {
    id: Uuid,
    name: String,
    description: Option<String>,
    owner_id: Uuid,
    cover_file_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl Playlist {
    pub fn new(name: String, owner_id: Uuid, description: Option<String>) -> Result<Self> {
        if name.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "Playlist",
                "Playlist name cannot be empty",
            ));
        }

        let now = Utc::now();

        Ok(Self {
            id: Uuid::new_v4(),
            name,
            description,
            owner_id,
            cover_file_id: None,
            created_at: now,
            updated_at: now,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_id(
        id: Uuid,
        name: String,
        description: Option<String>,
        owner_id: Uuid,
        cover_file_id: Option<Uuid>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self> {
        if name.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "Playlist",
                "Playlist name cannot be empty",
            ));
        }

        Ok(Self {
            id,
            name,
            description,
            owner_id,
            cover_file_id,
            created_at,
            updated_at,
        })
    }

    pub fn id(&self) -> &Uuid {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn owner_id(&self) -> &Uuid {
        &self.owner_id
    }

    pub fn cover_file_id(&self) -> Option<&Uuid> {
        self.cover_file_id.as_ref()
    }

    pub fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    pub fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    pub fn update_name(&mut self, name: String) -> Result<()> {
        if name.is_empty() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "Playlist",
                "Playlist name cannot be empty",
            ));
        }
        self.name = name;
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn update_description(&mut self, description: Option<String>) {
        self.description = description;
        self.updated_at = Utc::now();
    }

    pub fn set_cover(&mut self, cover_file_id: Option<Uuid>) {
        self.cover_file_id = cover_file_id;
        self.updated_at = Utc::now();
    }

    pub fn belongs_to(&self, user_id: &Uuid) -> bool {
        self.owner_id == *user_id
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}

#[derive(Debug, Clone)]
pub struct PlaylistItem {
    id: Uuid,
    playlist_id: Uuid,
    file_id: Uuid,
    position: i32,
    added_at: DateTime<Utc>,
}

impl PlaylistItem {
    pub fn new(playlist_id: Uuid, file_id: Uuid, position: i32) -> Result<Self> {
        if position < 0 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "PlaylistItem",
                "Position cannot be negative",
            ));
        }

        Ok(Self {
            id: Uuid::new_v4(),
            playlist_id,
            file_id,
            position,
            added_at: Utc::now(),
        })
    }

    pub fn with_id(
        id: Uuid,
        playlist_id: Uuid,
        file_id: Uuid,
        position: i32,
        added_at: DateTime<Utc>,
    ) -> Result<Self> {
        if position < 0 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "PlaylistItem",
                "Position cannot be negative",
            ));
        }

        Ok(Self {
            id,
            playlist_id,
            file_id,
            position,
            added_at,
        })
    }

    pub fn id(&self) -> &Uuid {
        &self.id
    }

    pub fn playlist_id(&self) -> &Uuid {
        &self.playlist_id
    }

    pub fn file_id(&self) -> &Uuid {
        &self.file_id
    }

    pub fn position(&self) -> i32 {
        self.position
    }

    pub fn added_at(&self) -> &DateTime<Utc> {
        &self.added_at
    }

    pub fn set_position(&mut self, position: i32) -> Result<()> {
        if position < 0 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "PlaylistItem",
                "Position cannot be negative",
            ));
        }
        self.position = position;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AudioFileMetadata {
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

impl AudioFileMetadata {
    pub fn new(file_id: Uuid, duration_secs: i32) -> Self {
        let now = Utc::now();
        Self {
            file_id,
            title: None,
            artist: None,
            album: None,
            album_artist: None,
            genre: None,
            track_number: None,
            disc_number: None,
            year: None,
            duration_secs,
            bitrate: None,
            sample_rate: None,
            channels: None,
            format: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_all_fields(
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
    ) -> Self {
        Self {
            file_id,
            title,
            artist,
            album,
            album_artist,
            genre,
            track_number,
            disc_number,
            year,
            duration_secs,
            bitrate,
            sample_rate,
            channels,
            format,
            created_at,
            updated_at,
        }
    }

    pub fn file_id(&self) -> &Uuid {
        &self.file_id
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn artist(&self) -> Option<&str> {
        self.artist.as_deref()
    }

    pub fn album(&self) -> Option<&str> {
        self.album.as_deref()
    }

    pub fn album_artist(&self) -> Option<&str> {
        self.album_artist.as_deref()
    }

    pub fn genre(&self) -> Option<&str> {
        self.genre.as_deref()
    }

    pub fn track_number(&self) -> Option<i32> {
        self.track_number
    }

    pub fn disc_number(&self) -> Option<i32> {
        self.disc_number
    }

    pub fn year(&self) -> Option<i32> {
        self.year
    }

    pub fn duration_secs(&self) -> i32 {
        self.duration_secs
    }

    pub fn bitrate(&self) -> Option<i32> {
        self.bitrate
    }

    pub fn sample_rate(&self) -> Option<i32> {
        self.sample_rate
    }

    pub fn channels(&self) -> Option<i16> {
        self.channels
    }

    pub fn format(&self) -> Option<&str> {
        self.format.as_deref()
    }

    pub fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    pub fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    pub fn set_title(&mut self, title: Option<String>) {
        self.title = title;
        self.updated_at = Utc::now();
    }

    pub fn set_artist(&mut self, artist: Option<String>) {
        self.artist = artist;
        self.updated_at = Utc::now();
    }

    pub fn set_album(&mut self, album: Option<String>) {
        self.album = album;
        self.updated_at = Utc::now();
    }

    pub fn set_album_artist(&mut self, album_artist: Option<String>) {
        self.album_artist = album_artist;
        self.updated_at = Utc::now();
    }

    pub fn set_genre(&mut self, genre: Option<String>) {
        self.genre = genre;
        self.updated_at = Utc::now();
    }

    pub fn set_track_number(&mut self, track_number: Option<i32>) {
        self.track_number = track_number;
        self.updated_at = Utc::now();
    }

    pub fn set_disc_number(&mut self, disc_number: Option<i32>) {
        self.disc_number = disc_number;
        self.updated_at = Utc::now();
    }

    pub fn set_year(&mut self, year: Option<i32>) {
        self.year = year;
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_playlist() {
        let owner_id = Uuid::new_v4();
        let playlist = Playlist::new("My Playlist".to_string(), owner_id, None);
        assert!(playlist.is_ok());
        let playlist = playlist.unwrap();
        assert_eq!(playlist.name(), "My Playlist");
    }

    #[test]
    fn test_create_playlist_empty_name() {
        let owner_id = Uuid::new_v4();
        let playlist = Playlist::new("".to_string(), owner_id, None);
        assert!(playlist.is_err());
    }

    #[test]
    fn test_playlist_update() {
        let owner_id = Uuid::new_v4();
        let mut playlist = Playlist::new("My Playlist".to_string(), owner_id, None).unwrap();
        playlist
            .update_name("Updated Playlist".to_string())
            .unwrap();
        assert_eq!(playlist.name(), "Updated Playlist");
    }

    #[test]
    fn test_create_playlist_item() {
        let playlist_id = Uuid::new_v4();
        let file_id = Uuid::new_v4();
        let item = PlaylistItem::new(playlist_id, file_id, 0);
        assert!(item.is_ok());
        let item = item.unwrap();
        assert_eq!(item.position(), 0);
    }

    #[test]
    fn test_create_playlist_item_negative_position() {
        let playlist_id = Uuid::new_v4();
        let file_id = Uuid::new_v4();
        let item = PlaylistItem::new(playlist_id, file_id, -1);
        assert!(item.is_err());
    }

    #[test]
    fn test_audio_metadata() {
        let file_id = Uuid::new_v4();
        let metadata = AudioFileMetadata::new(file_id, 180);
        assert_eq!(metadata.duration_secs(), 180);
        assert!(metadata.title().is_none());
    }
}
