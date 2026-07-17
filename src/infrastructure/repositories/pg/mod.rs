mod address_book_pg_repository;
mod app_password_pg_repository;
mod calendar_event_pg_repository;
mod calendar_pg_repository;
pub mod calendar_sync_change_pg_repository;
mod contact_group_pg_repository;
mod contact_persistence_dto;
mod contact_pg_repository;
mod device_code_pg_repository;
mod drive_pg_repository;
mod external_mount_repository;
mod face_pg_repository;
mod favorites_pg_repository;
pub mod file_metadata_repository;
mod magic_link_token_pg_repository;
mod nextcloud_object_id_repository;
pub mod playlist_pg_repository;
mod recent_items_pg_repository;
mod session_pg_repository;
mod settings_pg_repository;
mod share_pg_repository;
mod subject_group_pg_repository;
pub(crate) mod transaction_utils;
mod user_pg_repository;

// ── Blob-storage repositories ──
pub mod file_blob_read_repository;
pub mod file_blob_write_repository;
pub mod folder_db_repository;
pub mod folder_sync_change_pg_repository;
pub mod trash_db_repository;

pub use address_book_pg_repository::AddressBookPgRepository;
pub use app_password_pg_repository::AppPasswordPgRepository;
pub use calendar_event_pg_repository::CalendarEventPgRepository;
pub use calendar_pg_repository::CalendarPgRepository;
pub use calendar_sync_change_pg_repository::CalendarSyncChangePgRepository;
pub use contact_group_pg_repository::ContactGroupPgRepository;
pub use contact_persistence_dto::*;
pub use contact_pg_repository::ContactPgRepository;
pub use device_code_pg_repository::DeviceCodePgRepository;
pub use drive_pg_repository::DrivePgRepository;
pub use external_mount_repository::ExternalMountPgRepository;
pub use face_pg_repository::FacePgRepository;
pub use favorites_pg_repository::FavoritesPgRepository;
pub use file_blob_read_repository::FileBlobReadRepository;
pub use file_blob_write_repository::FileBlobWriteRepository;
pub use file_metadata_repository::FileMetadataRepository;
pub use folder_db_repository::FolderDbRepository;
pub use folder_sync_change_pg_repository::FolderSyncChangePgRepository;
pub use magic_link_token_pg_repository::MagicLinkTokenPgRepository;
pub use nextcloud_object_id_repository::NextcloudObjectIdRepository;
pub use playlist_pg_repository::{
    AudioMetadataPgRepository, PlaylistItemPgRepository, PlaylistPgRepository,
};
pub use recent_items_pg_repository::RecentItemsPgRepository;
pub use session_pg_repository::SessionPgRepository;
pub use settings_pg_repository::SettingsPgRepository;
pub use share_pg_repository::SharePgRepository;
pub use subject_group_pg_repository::SubjectGroupPgRepository;
pub use trash_db_repository::TrashDbRepository;
pub use user_pg_repository::UserPgRepository;

// ── SQL helpers ─────────────────────────────────────────────────────────────

/// Escape SQL `LIKE` / `ILIKE` wildcard characters (`%` and `_`) in user
/// input and wrap the result in `%…%` for a contains-match.
///
/// Without this, a user searching for `100%` would match *every* row because
/// `%` is a wildcard in LIKE patterns.
#[inline]
pub fn like_escape(raw: &str) -> String {
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}
