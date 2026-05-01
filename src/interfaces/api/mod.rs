pub mod cookie_auth;
pub mod handlers;
pub mod routes;

pub use routes::create_api_routes;
pub use routes::create_public_api_routes;

use utoipa::OpenApi;

use crate::application::dtos::favorites_dto::{
    BatchFavoritesResult, BatchFavoritesStats, FavoriteItemDto,
};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::{
    CreateFolderDto, FolderDto, MoveFolderDto, RenameFolderDto,
};
use crate::application::dtos::folder_listing_dto::FolderListingDto;
use crate::application::dtos::pagination::{PaginationDto, PaginationRequestDto};
use crate::application::dtos::recent_dto::RecentItemDto;
use crate::application::dtos::search_dto::{
    SearchCriteriaDto, SearchFileResultDto, SearchFolderResultDto, SearchResultsDto,
    SearchSuggestionItem, SearchSuggestionsDto,
};
use crate::application::dtos::share_dto::{
    CreateShareDto, ShareDto, SharePermissionsDto, UpdateShareDto,
};
use crate::application::dtos::trash_dto::{
    DeletePermanentlyRequest, MoveToTrashRequest, RestoreFromTrashRequest, TrashedItemDto,
};
use crate::application::dtos::user_dto::{
    AuthResponseDto, ChangePasswordDto, LoginDto, RefreshTokenDto, RegisterDto, SetupAdminDto,
    UserDto,
};
use crate::application::dtos::i18n_dto::{
    LocaleDto, TranslationErrorDto, TranslationRequestDto, TranslationResponseDto,
};
use crate::application::ports::chunked_upload_ports::{
    ChunkUploadResponseDto, CreateUploadResponseDto, UploadStatusResponseDto,
};
use crate::interfaces::api::handlers::chunked_upload_handler::{
    CompleteUploadResponse, CreateUploadRequest,
};
use crate::interfaces::api::handlers::dedup_handler::{
    DedupUploadResponse, HashCheckResponse, StatsResponse,
};
use crate::interfaces::api::handlers::file_handler::MoveFilePayload;

#[derive(OpenApi)]
#[openapi(
    paths(
        // File handlers (free functions — see file_handler.rs for why)
        handlers::file_handler::list_files_query,
        handlers::file_handler::upload_file_with_thumbnails,
        handlers::file_handler::download_file,
        handlers::file_handler::get_thumbnail,
        handlers::file_handler::upload_thumbnail,
        handlers::file_handler::get_file_metadata,
        handlers::file_handler::delete_file,
        handlers::file_handler::rename_file,
        handlers::file_handler::move_file_simple,
        // Folder handlers (free functions — see folder_handler.rs for why)
        handlers::folder_handler::create_folder,
        handlers::folder_handler::get_folder,
        handlers::folder_handler::list_root_folders,
        handlers::folder_handler::list_folder_contents,
        handlers::folder_handler::list_root_folders_paginated,
        handlers::folder_handler::list_folder_contents_paginated,
        handlers::folder_handler::list_folder_listing,
        handlers::folder_handler::rename_folder,
        handlers::folder_handler::move_folder,
        handlers::folder_handler::delete_folder_with_trash,
        handlers::folder_handler::download_folder_zip,
        // Search handlers (free functions — see search_handler.rs for why)
        handlers::search_handler::search_files_get,
        handlers::search_handler::search_files_post,
        handlers::search_handler::suggest_files,
        handlers::search_handler::clear_search_cache,
        // i18n handlers (free functions — see i18n_handler.rs for why)
        handlers::i18n_handler::get_locales,
        handlers::i18n_handler::translate,
        handlers::i18n_handler::get_translations_by_locale,
        // Chunked upload handlers — all five are free functions (not impl methods) because
        // utoipa 5.4.0 cannot annotate methods on ChunkedUploadHandler; see handler file.
        handlers::chunked_upload_handler::create_upload,
        handlers::chunked_upload_handler::upload_chunk,
        handlers::chunked_upload_handler::get_upload_status,
        handlers::chunked_upload_handler::complete_upload,
        handlers::chunked_upload_handler::cancel_upload,
        // Dedup handlers — all free functions for the same utoipa reason as chunked uploads.
        handlers::dedup_handler::check_hash,
        handlers::dedup_handler::upload_with_dedup,
        handlers::dedup_handler::get_stats,
        handlers::dedup_handler::get_blob,
        handlers::dedup_handler::recalculate_stats,
        // Trash handlers (free functions)
        handlers::trash_handler::get_trash_items,
        handlers::trash_handler::move_file_to_trash,
        handlers::trash_handler::move_folder_to_trash,
        handlers::trash_handler::restore_from_trash,
        handlers::trash_handler::delete_permanently,
        handlers::trash_handler::empty_trash,
        // Share handlers (free functions)
        handlers::share_handler::create_shared_link,
        handlers::share_handler::get_shared_link,
        handlers::share_handler::get_user_shares,
        handlers::share_handler::update_shared_link,
        handlers::share_handler::delete_shared_link,
        handlers::share_handler::access_shared_item,
        handlers::share_handler::verify_shared_item_password,
        handlers::share_handler::download_shared_file,
        // Favorites handlers (free functions)
        handlers::favorites_handler::get_favorites,
        handlers::favorites_handler::add_favorite,
        handlers::favorites_handler::remove_favorite,
        handlers::favorites_handler::batch_add_favorites,
        // Recent handlers (free functions)
        handlers::recent_handler::get_recent_items,
        handlers::recent_handler::record_item_access,
        handlers::recent_handler::remove_from_recent,
        handlers::recent_handler::clear_recent_items,
        // Photos handler (free function)
        handlers::photos_handler::list_photos,
        // Batch handlers (free functions)
        handlers::batch_handler::move_files_batch,
        handlers::batch_handler::copy_files_batch,
        handlers::batch_handler::delete_files_batch,
        handlers::batch_handler::get_files_batch,
        handlers::batch_handler::delete_folders_batch,
        handlers::batch_handler::create_folders_batch,
        handlers::batch_handler::get_folders_batch,
        handlers::batch_handler::move_folders_batch,
        handlers::batch_handler::trash_batch,
        handlers::batch_handler::download_batch,
        // Music/playlist handlers (free functions)
        handlers::music_handler::create_playlist,
        handlers::music_handler::list_playlists,
        handlers::music_handler::get_playlist,
        handlers::music_handler::update_playlist,
        handlers::music_handler::delete_playlist,
        handlers::music_handler::list_playlist_tracks,
        handlers::music_handler::add_tracks,
        handlers::music_handler::remove_track,
        handlers::music_handler::reorder_tracks,
        handlers::music_handler::share_playlist,
        handlers::music_handler::remove_share,
        handlers::music_handler::get_playlist_shares,
        handlers::music_handler::get_audio_metadata,
        // Admin handlers (pub free functions)
        handlers::admin_handler::get_dashboard_stats,
        handlers::admin_handler::list_users,
        handlers::admin_handler::get_user,
        handlers::admin_handler::create_user,
        handlers::admin_handler::delete_user,
        handlers::admin_handler::update_user_role,
        handlers::admin_handler::update_user_active,
        handlers::admin_handler::update_user_quota,
        handlers::admin_handler::reset_user_password,
        handlers::admin_handler::get_registration_setting,
        handlers::admin_handler::set_registration_setting,
        handlers::admin_handler::get_general_settings,
        handlers::admin_handler::get_oidc_settings,
        handlers::admin_handler::save_oidc_settings,
        handlers::admin_handler::get_storage_settings,
        handlers::admin_handler::save_storage_settings,
        handlers::admin_handler::get_migration_status,
        handlers::admin_handler::start_migration,
        handlers::admin_handler::pause_migration,
        handlers::admin_handler::resume_migration,
        handlers::admin_handler::complete_migration,
        handlers::admin_handler::verify_migration,
        handlers::admin_handler::generate_encryption_key,
    ),
    components(
        schemas(
            // Folder schemas
            FolderDto,
            CreateFolderDto,
            RenameFolderDto,
            MoveFolderDto,
            FolderListingDto,
            // File schemas
            FileDto,
            MoveFilePayload,
            PaginationDto,
            PaginationRequestDto,
            // User / Auth schemas
            UserDto,
            LoginDto,
            RegisterDto,
            SetupAdminDto,
            AuthResponseDto,
            ChangePasswordDto,
            RefreshTokenDto,
            // Share schemas
            ShareDto,
            SharePermissionsDto,
            CreateShareDto,
            UpdateShareDto,
            // Trash schemas
            TrashedItemDto,
            MoveToTrashRequest,
            RestoreFromTrashRequest,
            DeletePermanentlyRequest,
            // Search schemas
            SearchCriteriaDto,
            SearchResultsDto,
            SearchFileResultDto,
            SearchFolderResultDto,
            SearchSuggestionsDto,
            SearchSuggestionItem,
            // Favorites schemas
            FavoriteItemDto,
            BatchFavoritesResult,
            BatchFavoritesStats,
            // Recent schemas
            RecentItemDto,
            // i18n schemas
            LocaleDto,
            TranslationRequestDto,
            TranslationResponseDto,
            TranslationErrorDto,
            // Chunked upload schemas
            CreateUploadRequest,
            CompleteUploadResponse,
            CreateUploadResponseDto,
            ChunkUploadResponseDto,
            UploadStatusResponseDto,
            // Dedup schemas
            HashCheckResponse,
            DedupUploadResponse,
            StatsResponse,
        )
    ),
    tags(
        (name = "files", description = "File management endpoints"),
        (name = "folders", description = "Folder management endpoints"),
        (name = "trash", description = "Trash / recycle bin endpoints"),
        (name = "search", description = "Search endpoints"),
        (name = "shares", description = "Shared links endpoints"),
        (name = "favorites", description = "Favorites management endpoints"),
        (name = "recent", description = "Recent items endpoints"),
        (name = "photos", description = "Photos timeline endpoints"),
        (name = "i18n", description = "Internationalisation endpoints"),
        (name = "uploads", description = "Chunked / resumable upload endpoints"),
        (name = "dedup", description = "Content deduplication endpoints"),
        (name = "batch", description = "Batch operation endpoints"),
        (name = "playlists", description = "Music playlist endpoints"),
        (name = "admin", description = "Admin management endpoints"),
    ),
    info(
        title = "OxiCloud API",
        version = env!("CARGO_PKG_VERSION"),
        description = "REST API for OxiCloud — self-hosted cloud storage, calendar & contacts",
        license(name = "MIT")
    )
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_spec_is_valid_and_has_expected_structure() {
        let spec = ApiDoc::openapi();

        assert_eq!(spec.info.title, "OxiCloud API");
        assert!(!spec.info.version.is_empty());

        let paths = &spec.paths;
        assert!(
            paths.paths.len() >= 10,
            "expected at least 10 paths, got {}",
            paths.paths.len()
        );
        assert!(paths.paths.contains_key("/api/trash"), "missing /api/trash");
        assert!(
            paths.paths.contains_key("/api/shares"),
            "missing /api/shares"
        );
        assert!(
            paths.paths.contains_key("/api/favorites"),
            "missing /api/favorites"
        );
        assert!(
            paths.paths.contains_key("/api/recent"),
            "missing /api/recent"
        );

        let schemas = &spec
            .components
            .as_ref()
            .expect("components missing")
            .schemas;
        assert!(
            schemas.len() >= 25,
            "expected at least 25 schemas, got {}",
            schemas.len()
        );
        for name in [
            "FileDto",
            "FolderDto",
            "ShareDto",
            "TrashedItemDto",
            "UserDto",
        ] {
            assert!(schemas.contains_key(name), "missing schema: {name}");
        }

        let json = serde_json::to_string(&spec).expect("spec should serialise to JSON");
        assert!(json.len() > 1000, "spec JSON suspiciously small");
    }
}
